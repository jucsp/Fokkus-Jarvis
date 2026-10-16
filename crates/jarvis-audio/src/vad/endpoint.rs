//! *Endpointer*: convierte los puntajes de [`crate::vad::VadDetector`] en
//! eventos de inicio de voz y fin de turno (HU-3).
//!
//! Funciona sobre frames de 256 muestras a 16 kHz (16 ms). Mantiene:
//! * **Hangover de apertura** (`open_frames`): cuántos frames de voz seguidos se
//!   necesitan para declarar inicio de voz.
//! * **Silencio de cierre** (`silence_ms`, 750 ms por defecto): cuánto silencio
//!   continuo cierra el turno. Si el STT informa un conector ("y…", "pero…"),
//!   [`signal_connector`](Endpointer::signal_connector) extiende la espera a
//!   `extended_silence_ms` (1,2 s).
//! * **Pre-roll de 300 ms**: el segmento entregado incluye el audio previo al
//!   inicio detectado, desde un buffer circular preasignado.
//! * **Duración máxima** (`max_segment_ms`, 30 s): corta el turno limpiamente.

use std::time::Instant;

use crate::events::{Segment, VoiceEvent};

/// Muestras por frame (256 = 16 ms a 16 kHz).
pub const FRAME_SAMPLES: usize = 256;

/// Tasa de muestreo de trabajo del VAD.
pub const SAMPLE_RATE: u32 = 16_000;

/// Configuración del *endpointer*.
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointConfig {
    /// Puntaje mínimo para considerar un frame como voz.
    pub threshold: f32,
    /// Frames de voz consecutivos para abrir el turno.
    pub open_frames: usize,
    /// Silencio de cierre por defecto en milisegundos.
    pub silence_ms: u32,
    /// Silencio de cierre extendido (conector) en milisegundos.
    pub extended_silence_ms: u32,
    /// Duración del *pre-roll* en milisegundos.
    pub pre_roll_ms: u32,
    /// Duración máxima del segmento en milisegundos.
    pub max_segment_ms: u32,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            open_frames: 2,
            silence_ms: 750,
            extended_silence_ms: 1_200,
            pre_roll_ms: 300,
            max_segment_ms: 30_000,
        }
    }
}

impl EndpointConfig {
    /// Duración de un frame en milisegundos (entero, sin decimales).
    pub fn frame_ms(&self) -> u32 {
        (FRAME_SAMPLES as u32 * 1000) / SAMPLE_RATE
    }

    fn frames_for(ms: u32, frame_ms: u32) -> usize {
        // Redondeo hacia arriba: un silencio de 750 ms son ceil(750/16) = 47 frames.
        (ms as usize).div_ceil(frame_ms as usize)
    }

    pub fn close_frames(&self) -> usize {
        Self::frames_for(self.silence_ms, self.frame_ms())
    }

    pub fn extended_close_frames(&self) -> usize {
        Self::frames_for(self.extended_silence_ms, self.frame_ms())
    }

    pub fn pre_roll_frames(&self) -> usize {
        Self::frames_for(self.pre_roll_ms, self.frame_ms())
    }

    /// Frames máximos del segmento **completo** (pre-roll incluido), con
    /// redondeo hacia abajo para no superar `max_segment_ms` (CA-5.3).
    pub fn max_speech_frames(&self) -> usize {
        (self.max_segment_ms / self.frame_ms()).max(1) as usize
    }
}

/// Estado interno del *endpointer*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Sin voz detectada (acumulando *pre-roll*).
    Silence,
    /// Turno abierto, acumulando el segmento.
    Speech,
}

/// Buffer circular de *pre-roll* que retiene los últimos `frames` frames.
struct PreRollBuffer {
    frames: usize,
    samples: Vec<f32>,
    /// Índice del próximo frame a escribir (anillo).
    write_frame: usize,
    /// Total de frames escritos desde el arranque.
    filled: usize,
}

impl PreRollBuffer {
    fn new(frames: usize) -> Self {
        Self {
            frames,
            samples: vec![0.0; frames * FRAME_SAMPLES],
            write_frame: 0,
            filled: 0,
        }
    }

    fn push(&mut self, frame: &[f32]) {
        let base = self.write_frame * FRAME_SAMPLES;
        self.samples[base..base + FRAME_SAMPLES].copy_from_slice(&frame[..FRAME_SAMPLES]);
        self.write_frame = (self.write_frame + 1) % self.frames;
        self.filled = self.filled.saturating_add(1);
    }

    /// Frames disponibles en orden cronológico.
    fn available(&self) -> usize {
        self.filled.min(self.frames)
    }

    /// Copia los frames retenidos, en orden cronológico, al final de `out`.
    fn collect_into(&self, out: &mut Vec<f32>) {
        let avail = self.available();
        if self.filled < self.frames {
            for i in 0..avail {
                out.extend_from_slice(&self.samples[i * FRAME_SAMPLES..(i + 1) * FRAME_SAMPLES]);
            }
        } else {
            for i in 0..avail {
                let idx = (self.write_frame + i) % self.frames;
                out.extend_from_slice(
                    &self.samples[idx * FRAME_SAMPLES..(idx + 1) * FRAME_SAMPLES],
                );
            }
        }
    }
}

/// Detector de voz y fin de turno.
pub struct Endpointer {
    config: EndpointConfig,
    state: State,
    voice_streak: usize,
    silence_streak: usize,
    speech_frames: usize,
    connector_pending: bool,
    pre_roll: PreRollBuffer,
    segment: Vec<f32>,
}

impl Endpointer {
    pub fn new(config: EndpointConfig) -> Self {
        Self {
            pre_roll: PreRollBuffer::new(config.pre_roll_frames().max(1)),
            config,
            state: State::Silence,
            voice_streak: 0,
            silence_streak: 0,
            speech_frames: 0,
            connector_pending: false,
            segment: Vec::new(),
        }
    }

    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    /// Estado actual (`true` si hay un turno abierto).
    pub fn is_speaking(&self) -> bool {
        self.state == State::Speech
    }

    /// Informa que la transcripción parcial termina en conector: extiende el
    /// silencio de cierre a `extended_silence_ms` para el turno en curso.
    pub fn signal_connector(&mut self) {
        if self.state == State::Speech {
            self.connector_pending = true;
        }
    }

    /// Procesa un frame (256 muestras, ya saneado) y su puntaje de voz.
    ///
    /// Devuelve a lo sumo un evento por frame. `now` es la marca monotónica del
    /// frame, que viaja dentro del evento para medir latencias.
    /// Un frame de largo distinto de 256 se ignora (no puede venir del
    /// re-encuadre, pero el endpointer nunca debe entrar en pánico).
    pub fn process(&mut self, frame: &[f32], score: f32, now: Instant) -> Option<VoiceEvent> {
        if frame.len() != FRAME_SAMPLES {
            return None;
        }
        let is_voice = score >= self.config.threshold;

        let event = match self.state {
            State::Silence => {
                if is_voice {
                    self.voice_streak += 1;
                    if self.voice_streak >= self.config.open_frames {
                        self.state = State::Speech;
                        self.voice_streak = 0;
                        self.silence_streak = 0;
                        self.speech_frames = 0;
                        self.connector_pending = false;
                        // El segmento arranca con el pre-roll (los 300 ms previos)
                        // y el frame disparador.
                        // Una sola reserva por turno: el segmento nunca crece
                        // más allá de la duración máxima.
                        let mut segment =
                            Vec::with_capacity(self.config.max_speech_frames() * FRAME_SAMPLES);
                        self.pre_roll.collect_into(&mut segment);
                        segment.extend_from_slice(frame);
                        self.speech_frames = segment.len() / FRAME_SAMPLES;
                        self.segment = segment;
                        Some(VoiceEvent::VoiceStart { at: now })
                    } else {
                        None
                    }
                } else {
                    self.voice_streak = 0;
                    None
                }
            }
            State::Speech => {
                if is_voice {
                    self.silence_streak = 0;
                } else {
                    self.silence_streak += 1;
                }
                self.segment.extend_from_slice(&frame[..FRAME_SAMPLES]);
                self.speech_frames += 1;

                let close = if self.connector_pending {
                    self.config.extended_close_frames()
                } else {
                    self.config.close_frames()
                };
                let over_max = self.speech_frames >= self.config.max_speech_frames();

                if self.silence_streak >= close || over_max {
                    let pcm = std::mem::take(&mut self.segment);
                    let duration_ms =
                        u32::try_from(pcm.len() as u64 * 1000 / u64::from(SAMPLE_RATE))
                            .unwrap_or(u32::MAX);
                    self.state = State::Silence;
                    self.silence_streak = 0;
                    self.voice_streak = 0;
                    self.speech_frames = 0;
                    self.connector_pending = false;
                    Some(VoiceEvent::EndOfTurn {
                        at: now,
                        segment: Segment { pcm, duration_ms },
                    })
                } else {
                    None
                }
            }
        };

        self.pre_roll.push(&frame[..FRAME_SAMPLES]);
        event
    }

    /// Reinicia el detector (p. ej. al reconectar el dispositivo).
    pub fn reset(&mut self) {
        self.state = State::Silence;
        self.voice_streak = 0;
        self.silence_streak = 0;
        self.speech_frames = 0;
        self.connector_pending = false;
        self.segment.clear();
        self.pre_roll = PreRollBuffer::new(self.config.pre_roll_frames().max(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: f32) -> Vec<f32> {
        vec![value; FRAME_SAMPLES]
    }

    fn voice_frame() -> Vec<f32> {
        frame(0.2)
    }

    /// Ejecuta `n` frames silenciosos.
    fn feed_silence(ep: &mut Endpointer, n: usize, now: &mut Instant) {
        for _ in 0..n {
            *now += std::time::Duration::from_millis(16);
            assert!(ep.process(&frame(0.0), 0.0, *now).is_none());
        }
    }

    #[test]
    fn frame_ms_and_close_frames_are_consistent() {
        let cfg = EndpointConfig::default();
        assert_eq!(cfg.frame_ms(), 16);
        assert_eq!(cfg.close_frames(), 47); // 750 / 16 = 46.875 → 47
        assert_eq!(cfg.extended_close_frames(), 75); // 1200 / 16
        assert_eq!(cfg.pre_roll_frames(), 19); // 300 / 16 = 18.75 → 19
        assert_eq!(cfg.max_speech_frames(), 1875); // 30000 / 16 (pre-roll incluido)
    }

    #[test]
    fn voice_start_requires_open_hangover() {
        let cfg = EndpointConfig {
            open_frames: 2,
            ..EndpointConfig::default()
        };
        let mut ep = Endpointer::new(cfg);
        let mut now = Instant::now();

        // Un solo frame de voz no abre.
        assert!(ep.process(&voice_frame(), 0.9, now).is_none());
        assert!(!ep.is_speaking());
        now += std::time::Duration::from_millis(16);
        // El segundo frame abre.
        assert!(matches!(
            ep.process(&voice_frame(), 0.9, now),
            Some(VoiceEvent::VoiceStart { .. })
        ));
        assert!(ep.is_speaking());
    }

    #[test]
    fn pause_shorter_than_endpoint_does_not_close() {
        let cfg = EndpointConfig::default(); // 750 ms = 47 frames
        let mut ep = Endpointer::new(cfg);
        let mut now = Instant::now();

        // Abre el turno con dos frames de voz.
        ep.process(&voice_frame(), 0.9, now);
        now += std::time::Duration::from_millis(16);
        assert!(matches!(
            ep.process(&voice_frame(), 0.9, now),
            Some(VoiceEvent::VoiceStart { .. })
        ));

        // Pausa de 320 ms (20 frames) dentro de la frase: no cierra.
        feed_silence(&mut ep, 20, &mut now);
        assert!(ep.is_speaking());

        // Continúa hablando: reinicia el contador de silencio.
        now += std::time::Duration::from_millis(16);
        assert!(ep.process(&voice_frame(), 0.9, now).is_none());

        // 46 frames de silencio no cierran; el frame 47 sí.
        feed_silence(&mut ep, 46, &mut now);
        assert!(ep.is_speaking());
        now += std::time::Duration::from_millis(16);
        assert!(matches!(
            ep.process(&frame(0.0), 0.0, now),
            Some(VoiceEvent::EndOfTurn { .. })
        ));
        assert!(!ep.is_speaking());
    }

    #[test]
    fn endpoint_silence_of_750ms_closes() {
        let cfg = EndpointConfig::default();
        let mut ep = Endpointer::new(cfg);
        let mut now = Instant::now();

        ep.process(&voice_frame(), 0.9, now);
        now += std::time::Duration::from_millis(16);
        ep.process(&voice_frame(), 0.9, now);

        // 46 frames de silencio (736 ms): aún abierto.
        feed_silence(&mut ep, 46, &mut now);
        assert!(ep.is_speaking());
        // El frame 47 cierra (752 ms).
        now += std::time::Duration::from_millis(16);
        assert!(matches!(
            ep.process(&frame(0.0), 0.0, now),
            Some(VoiceEvent::EndOfTurn { .. })
        ));
        assert!(!ep.is_speaking());
    }

    #[test]
    fn connector_extends_timeout_to_1200ms() {
        let cfg = EndpointConfig::default();
        let mut ep = Endpointer::new(cfg);
        let mut now = Instant::now();

        ep.process(&voice_frame(), 0.9, now);
        now += std::time::Duration::from_millis(16);
        ep.process(&voice_frame(), 0.9, now);

        ep.signal_connector();

        // 1000 ms de silencio (62 frames): con conector, no cierra.
        feed_silence(&mut ep, 62, &mut now);
        assert!(ep.is_speaking());
        // 1200 ms (75 frames) sí cierra: 12 frames más siguen abiertos y el
        // 75.º emite el fin de turno.
        feed_silence(&mut ep, 12, &mut now);
        assert!(ep.is_speaking());
        now += std::time::Duration::from_millis(16);
        assert!(matches!(
            ep.process(&frame(0.0), 0.0, now),
            Some(VoiceEvent::EndOfTurn { .. })
        ));
        assert!(!ep.is_speaking());
    }

    #[test]
    fn pre_roll_delivers_about_300ms() {
        // open_frames = 1: el primer frame de voz abre el turno, así el pre-roll
        // contiene exactamente los 19 frames previos (todos silencio).
        let cfg = EndpointConfig {
            open_frames: 1,
            ..EndpointConfig::default()
        };
        let mut ep = Endpointer::new(cfg);
        let mut now = Instant::now();

        // Llena el pre-roll con 19 frames de silencio (304 ms ≈ 300 ms).
        feed_silence(&mut ep, 19, &mut now);

        // Abre el turno con el primer frame de voz.
        now += std::time::Duration::from_millis(16);
        assert!(matches!(
            ep.process(&voice_frame(), 0.9, now),
            Some(VoiceEvent::VoiceStart { .. })
        ));

        // 46 frames de silencio no cierran; el 47.º cierra.
        feed_silence(&mut ep, 46, &mut now);
        assert!(ep.is_speaking());
        now += std::time::Duration::from_millis(16);
        match ep.process(&frame(0.0), 0.0, now) {
            Some(VoiceEvent::EndOfTurn { segment, .. }) => {
                // pre-roll (19) + 1 voz + 47 silencio = 67 frames.
                assert_eq!(segment.pcm.len(), 67 * FRAME_SAMPLES);
                // Los primeros 19 frames (304 ms) son el pre-roll de silencio.
                assert!(segment.pcm[..19 * FRAME_SAMPLES].iter().all(|s| *s == 0.0));
                assert_eq!(segment.duration_ms, 67 * 16);
            }
            other => panic!("esperaba EndOfTurn, obtuve {other:?}"),
        }
    }

    #[test]
    fn segment_is_cut_at_30s_including_pre_roll() {
        let mut ep = Endpointer::new(EndpointConfig {
            open_frames: 1,
            ..EndpointConfig::default()
        });
        let mut now = Instant::now();
        feed_silence(&mut ep, 40, &mut now);

        // Habla continua: el corte debe llegar con el segmento en 30 s exactos.
        let mut ended = None;
        for _ in 0..3_000 {
            now += std::time::Duration::from_millis(16);
            if let Some(VoiceEvent::EndOfTurn { segment, .. }) =
                ep.process(&voice_frame(), 0.9, now)
            {
                ended = Some(segment);
                break;
            }
        }
        let segment = ended.expect("debe cortar por duración máxima");
        assert_eq!(segment.pcm.len(), 1875 * FRAME_SAMPLES);
        assert_eq!(segment.duration_ms, 30_000);
        assert!(!ep.is_speaking());
    }

    #[test]
    fn wrong_frame_length_is_ignored() {
        let mut ep = Endpointer::new(EndpointConfig::default());
        assert!(ep.process(&[0.1; 10], 1.0, Instant::now()).is_none());
        assert!(ep.process(&[], 1.0, Instant::now()).is_none());
    }

    #[test]
    fn connector_does_not_leak_into_next_turn() {
        let mut ep = Endpointer::new(EndpointConfig {
            open_frames: 1,
            ..EndpointConfig::default()
        });
        let mut now = Instant::now();
        ep.process(&voice_frame(), 0.9, now);
        ep.signal_connector();
        feed_silence(&mut ep, 74, &mut now);
        now += std::time::Duration::from_millis(16);
        assert!(ep.process(&frame(0.0), 0.0, now).is_some());

        // Nuevo turno sin conector: vuelve a cerrar a los 750 ms.
        now += std::time::Duration::from_millis(16);
        ep.process(&voice_frame(), 0.9, now);
        feed_silence(&mut ep, 46, &mut now);
        now += std::time::Duration::from_millis(16);
        assert!(ep.process(&frame(0.0), 0.0, now).is_some());
    }
}
