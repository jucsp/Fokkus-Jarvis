//! *Endpointer*: convierte los puntajes de [`crate::vad::VadDetector`] en
//! eventos de inicio de voz y fin de turno (HU-3, HU-41).
//!
//! Funciona sobre frames de 256 muestras a 16 kHz (16 ms). Mantiene:
//! * **Hangover de apertura** (`open_frames`): cuántos frames de voz seguidos se
//!   necesitan para declarar inicio de voz.
//! * **Histéresis VAD**: `voice_open` (0,45) abre el turno y `voice_hold`
//!   (0,25) lo mantiene abierto; así las consonantes sordas o las caídas de
//!   volumen a mitad de palabra no cortan el turno.
//! * **Silencio de cierre** (`silence_ms`, 1,2 s por defecto): cuánto silencio
//!   continuo cierra el turno. Si el STT informa un conector ("y…", "pero…"),
//!   [`signal_connector`](Endpointer::signal_connector) extiende la espera a
//!   `extended_silence_ms` (2 s).
//! * **Pre-roll de 300 ms**: el segmento entregado incluye el audio previo al
//!   inicio detectado, desde un buffer circular preasignado.
//! * **Fragmentos de turno largo** (`max_segment_ms`, 30 s; HU-44): al
//!   alcanzarla se entrega un [`VoiceEvent::Partial`] y el turno **sigue
//!   abierto**. Solo se cierra por silencio real o al llegar al techo total
//!   `max_turn_ms` (120 s). La memoria queda acotada a un fragmento: cada
//!   parcial se entrega y el búfer vuelve a empezar.
//!
//! HU-41 añade dos cosas: cada [`VoiceEvent::EndOfTurn`] viaja con su
//! [`CloseReason`] (silencio, conector o duración máxima) y el detector acumula
//! las pausas de silencio **intra-turno** en [`TurnPauseStats`] para calibrar
//! los umbrales con los percentiles P50/P90 reales del usuario.

use std::collections::VecDeque;
use std::time::Instant;

use jarvis_core::VoiceEndpointConfig;

use crate::events::{CloseReason, Segment, VoiceEvent};

/// Muestras por frame (256 = 16 ms a 16 kHz).
pub const FRAME_SAMPLES: usize = 256;

/// Tasa de muestreo de trabajo del VAD.
pub const SAMPLE_RATE: u32 = 16_000;

/// Configuración del *endpointer*.
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointConfig {
    /// Puntaje mínimo para ABRIR el turno (histéresis alta).
    pub voice_open: f32,
    /// Puntaje mínimo para MANTENER el turno abierto (histéresis baja).
    pub voice_hold: f32,
    /// Frames de voz consecutivos para abrir el turno.
    pub open_frames: usize,
    /// Silencio de cierre por defecto en milisegundos.
    pub silence_ms: u32,
    /// Silencio de cierre extendido (conector) en milisegundos.
    pub extended_silence_ms: u32,
    /// Duración del *pre-roll* en milisegundos.
    pub pre_roll_ms: u32,
    /// Duración máxima de un fragmento en milisegundos (HU-44: corta un
    /// parcial, no el turno).
    pub max_segment_ms: u32,
    /// Techo total del turno en milisegundos (HU-44). Si no supera
    /// `max_segment_ms`, el primer corte cierra el turno (comportamiento
    /// previo a HU-44).
    pub max_turn_ms: u32,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            voice_open: 0.45,
            voice_hold: 0.25,
            open_frames: 2,
            silence_ms: 1_200,
            extended_silence_ms: 2_000,
            pre_roll_ms: 300,
            max_segment_ms: 30_000,
            max_turn_ms: 120_000,
        }
    }
}

/// Deriva el `EndpointConfig` del DSP desde la tabla `[voice_endpoint]` de la
/// configuración de usuario (HU-41).
///
/// `open_frames` y `pre_roll_ms` no se exponen al usuario: son detalles del
/// detector y se quedan en su valor por defecto. Los seis valores expuestos ya
/// llegan saturados a su rango desde [`VoiceEndpointConfig`].
impl From<&VoiceEndpointConfig> for EndpointConfig {
    fn from(voice: &VoiceEndpointConfig) -> Self {
        let defaults = Self::default();
        Self {
            voice_open: voice.voice_open(),
            voice_hold: voice.voice_hold(),
            open_frames: defaults.open_frames,
            silence_ms: voice.silence_ms(),
            extended_silence_ms: voice.extended_silence_ms(),
            pre_roll_ms: defaults.pre_roll_ms,
            max_segment_ms: voice.max_segment_ms(),
            max_turn_ms: voice.max_turn_ms(),
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

    /// Frames máximos del turno completo (suma de fragmentos, HU-44). Nunca
    /// menos que un fragmento.
    pub fn max_turn_frames(&self) -> usize {
        ((self.max_turn_ms / self.frame_ms()).max(1) as usize).max(self.max_speech_frames())
    }
}

/// Registro acotado de las pausas de silencio **intra-turno** (HU-41).
///
/// Una pausa intra-turno es un tramo de silencio que se reanuda con voz dentro
/// del mismo turno: no cierra el turno, así que no aparece en el segmento. Su
/// distribución es la que dice cuánto hay que esperar de verdad: si el P90 ronda
/// `silence_ms`, el detector está cortando al usuario a mitad de frase.
///
/// El histograma es un anillo de [`Self::MAX_SAMPLES`] muestras (las más
/// recientes) y la memoria es fija: nunca crece durante una conversación larga.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPauseStats {
    samples: VecDeque<u32>,
    /// Total de pausas registradas desde el arranque (no solo las retenidas).
    recorded: u64,
}

impl Default for TurnPauseStats {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnPauseStats {
    /// Muestras retenidas para el histograma (las más recientes).
    pub const MAX_SAMPLES: usize = 256;

    /// Registro vacío con el anillo ya reservado (no asigna al registrar).
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(Self::MAX_SAMPLES),
            recorded: 0,
        }
    }

    /// Registra una pausa; las de duración cero (frame único irrelevante) se
    /// ignoran para no sesgar los percentiles hacia abajo.
    pub fn record(&mut self, pause_ms: u32) {
        if pause_ms == 0 {
            return;
        }
        if self.samples.len() == Self::MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(pause_ms);
        self.recorded = self.recorded.saturating_add(1);
    }

    /// Pausas retenidas, en orden de llegada.
    pub fn samples(&self) -> impl Iterator<Item = u32> + '_ {
        self.samples.iter().copied()
    }

    /// Cuántas pausas se retienen.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// `true` si aún no hay ninguna pausa registrada.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Total de pausas registradas (incluidas las que ya salieron del anillo).
    pub fn recorded(&self) -> u64 {
        self.recorded
    }

    /// Incorpora las pausas de otro registro (p. ej. las de un turno recién
    /// cerrado) conservando el límite del anillo y el total histórico.
    pub fn absorb(&mut self, other: &TurnPauseStats) {
        for pause_ms in other.samples() {
            if self.samples.len() == Self::MAX_SAMPLES {
                self.samples.pop_front();
            }
            self.samples.push_back(pause_ms);
        }
        self.recorded = self.recorded.saturating_add(other.recorded);
    }

    /// Vacía el histograma y el contador.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.recorded = 0;
    }

    /// Percentil `p` (0–100) por rango más cercano, en milisegundos.
    ///
    /// `None` si no hay muestras. Un `p` fuera de rango se satura a 0–100.
    pub fn percentile(&self, p: f64) -> Option<u32> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<u32> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let p = p.clamp(0.0, 100.0);
        // Rango más cercano: ceil(p/100 · n), con índice base 0.
        let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
        let index = rank.saturating_sub(1).min(sorted.len() - 1);
        Some(sorted[index])
    }

    /// Percentil 50 (mediana) de las pausas intra-turno.
    pub fn p50(&self) -> Option<u32> {
        self.percentile(50.0)
    }

    /// Percentil 90 de las pausas intra-turno.
    pub fn p90(&self) -> Option<u32> {
        self.percentile(90.0)
    }
}

/// Resumen del histograma para el volcado de diagnóstico (`--diag-turns`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnPauseSummary {
    /// Pausas retenidas en el histograma.
    pub samples: usize,
    /// Percentil 50 en milisegundos (`None` sin muestras).
    pub p50_ms: Option<u32>,
    /// Percentil 90 en milisegundos (`None` sin muestras).
    pub p90_ms: Option<u32>,
}

impl TurnPauseStats {
    /// Resumen P50/P90 listo para imprimir.
    pub fn summary(&self) -> TurnPauseSummary {
        TurnPauseSummary {
            samples: self.len(),
            p50_ms: self.p50(),
            p90_ms: self.p90(),
        }
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
    /// Frames del turno completo, sumando fragmentos parciales (HU-44).
    turn_frames: usize,
    /// Fragmentos parciales ya entregados en el turno en curso (HU-44).
    partials: u32,
    connector_pending: bool,
    pre_roll: PreRollBuffer,
    segment: Vec<f32>,
    pause_stats: TurnPauseStats,
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
            turn_frames: 0,
            partials: 0,
            connector_pending: false,
            segment: Vec::new(),
            pause_stats: TurnPauseStats::new(),
        }
    }

    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    /// Histograma de pausas intra-turno acumulado desde el arranque (HU-41).
    pub fn pause_stats(&self) -> &TurnPauseStats {
        &self.pause_stats
    }

    /// Se lleva el histograma acumulado (p. ej. al volcar `--diag-turns`).
    pub fn take_pause_stats(&mut self) -> TurnPauseStats {
        std::mem::take(&mut self.pause_stats)
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
        // Histéresis: abrir exige más energía que mantener (HU-AUDIO-CONTINUITY).
        let threshold = match self.state {
            State::Silence => self.config.voice_open,
            State::Speech => self.config.voice_hold,
        };
        let is_voice = score >= threshold;

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
                        // Una sola reserva por fragmento: el segmento nunca
                        // crece más allá de `max_segment_ms` (HU-44).
                        let mut segment =
                            Vec::with_capacity(self.config.max_speech_frames() * FRAME_SAMPLES);
                        self.pre_roll.collect_into(&mut segment);
                        segment.extend_from_slice(frame);
                        self.speech_frames = segment.len() / FRAME_SAMPLES;
                        self.turn_frames = self.speech_frames;
                        self.partials = 0;
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
                    // La pausa terminó con voz: es intra-turno y se registra en
                    // el histograma de HU-41 antes de reiniciar el contador.
                    self.record_pause();
                    self.silence_streak = 0;
                } else {
                    self.silence_streak += 1;
                }
                self.segment.extend_from_slice(&frame[..FRAME_SAMPLES]);
                self.speech_frames += 1;
                self.turn_frames += 1;

                let close = if self.connector_pending {
                    self.config.extended_close_frames()
                } else {
                    self.config.close_frames()
                };
                let over_turn = self.turn_frames >= self.config.max_turn_frames();
                let over_chunk = self.speech_frames >= self.config.max_speech_frames();

                if self.silence_streak >= close || over_turn {
                    // Razón del cierre (HU-41): el techo del turno gana sobre
                    // el silencio, y un conector pendiente explica la espera
                    // larga.
                    let close_reason = if over_turn {
                        CloseReason::MaxTimeout
                    } else if self.connector_pending {
                        CloseReason::Connector
                    } else {
                        CloseReason::Silence
                    };
                    let pcm = std::mem::take(&mut self.segment);
                    let duration_ms = duration_ms(&pcm);
                    let partials = self.partials;
                    self.state = State::Silence;
                    self.silence_streak = 0;
                    self.voice_streak = 0;
                    self.speech_frames = 0;
                    self.turn_frames = 0;
                    self.partials = 0;
                    self.connector_pending = false;
                    Some(VoiceEvent::EndOfTurn {
                        at: now,
                        segment: Segment { pcm, duration_ms },
                        partials,
                        close_reason,
                    })
                } else if over_chunk {
                    // Turno largo (HU-44): se entrega el fragmento para
                    // transcribirlo y el turno sigue abierto. El silencio en
                    // curso se conserva: una pausa que cruza el corte cuenta
                    // entera para el cierre.
                    let capacity = self.config.max_speech_frames() * FRAME_SAMPLES;
                    let pcm = std::mem::replace(&mut self.segment, Vec::with_capacity(capacity));
                    let duration_ms = duration_ms(&pcm);
                    let index = self.partials;
                    self.partials = self.partials.saturating_add(1);
                    self.speech_frames = 0;
                    Some(VoiceEvent::Partial {
                        at: now,
                        segment: Segment { pcm, duration_ms },
                        index,
                    })
                } else {
                    None
                }
            }
        };

        self.pre_roll.push(&frame[..FRAME_SAMPLES]);
        event
    }

    /// Registra el silencio acumulado como pausa intra-turno (ms) y lo reinicia.
    ///
    /// Solo cuenta el silencio que se reanuda con voz: el que cierra el turno
    /// no es una pausa, es el fin del turno.
    fn record_pause(&mut self) {
        if self.silence_streak > 0 {
            let frame_ms = self.config.frame_ms();
            let pause_ms = (self.silence_streak as u32).saturating_mul(frame_ms);
            self.pause_stats.record(pause_ms);
        }
    }

    /// Reinicia el detector (p. ej. al reconectar el dispositivo).
    pub fn reset(&mut self) {
        self.state = State::Silence;
        self.voice_streak = 0;
        self.silence_streak = 0;
        self.speech_frames = 0;
        self.turn_frames = 0;
        self.partials = 0;
        self.connector_pending = false;
        self.segment.clear();
        self.pre_roll = PreRollBuffer::new(self.config.pre_roll_frames().max(1));
    }
}

/// Duración en ms de un búfer de PCM a [`SAMPLE_RATE`].
fn duration_ms(pcm: &[f32]) -> u32 {
    u32::try_from(pcm.len() as u64 * 1000 / u64::from(SAMPLE_RATE)).unwrap_or(u32::MAX)
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

    /// Cierra el turno en curso y devuelve la razón del cierre.
    fn close_reason_and_segment(ep: &mut Endpointer, now: &mut Instant) -> (CloseReason, Segment) {
        *now += std::time::Duration::from_millis(16);
        match ep.process(&frame(0.0), 0.0, *now) {
            Some(VoiceEvent::EndOfTurn {
                segment,
                close_reason,
                ..
            }) => (close_reason, segment),
            other => panic!("esperaba EndOfTurn, obtuve {other:?}"),
        }
    }

    #[test]
    fn end_of_turn_counts_the_final_frame() {
        // Un frame de cierre que llega justo al umbral cuenta: 75 frames.
        let cfg = EndpointConfig {
            open_frames: 1,
            ..EndpointConfig::default()
        };
        let mut ep = Endpointer::new(cfg);
        let mut now = Instant::now();
        ep.process(&voice_frame(), 0.9, now);
        feed_silence(&mut ep, 74, &mut now);
        let (reason, _) = close_reason_and_segment(&mut ep, &mut now);
        assert_eq!(reason, CloseReason::Silence);
        assert!(!ep.is_speaking());
    }

    #[test]
    fn frame_ms_and_close_frames_are_consistent() {
        let cfg = EndpointConfig::default();
        assert_eq!(cfg.frame_ms(), 16);
        assert_eq!(cfg.close_frames(), 75); // 1200 / 16
        assert_eq!(cfg.extended_close_frames(), 125); // 2000 / 16
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
    fn hysteresis_does_not_open_with_scores_below_open_threshold() {
        let mut ep = Endpointer::new(EndpointConfig::default());
        let mut now = Instant::now();

        // Entre voice_hold (0.25) y voice_open (0.45) no se abre el turno.
        for _ in 0..10 {
            now += std::time::Duration::from_millis(16);
            assert!(ep.process(&frame(0.0), 0.4, now).is_none());
        }
        assert!(!ep.is_speaking());
    }

    #[test]
    fn hysteresis_holds_speech_with_scores_below_open_threshold() {
        let mut ep = Endpointer::new(EndpointConfig::default());
        let mut now = Instant::now();

        // Abre con voz clara.
        ep.process(&voice_frame(), 0.9, now);
        now += std::time::Duration::from_millis(16);
        ep.process(&voice_frame(), 0.9, now);
        assert!(ep.is_speaking());

        // Consonantes sordas / caídas de volumen (0.3 ≥ voice_hold y
        // < voice_open) mantienen el turno abierto.
        for _ in 0..10 {
            now += std::time::Duration::from_millis(16);
            assert!(ep.process(&frame(0.0), 0.3, now).is_none());
            assert!(ep.is_speaking());
        }
    }

    #[test]
    fn pause_shorter_than_endpoint_does_not_close() {
        let cfg = EndpointConfig::default(); // 1200 ms = 75 frames
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

        // 74 frames de silencio no cierran; el frame 75 sí.
        feed_silence(&mut ep, 74, &mut now);
        assert!(ep.is_speaking());
        let (reason, _) = close_reason_and_segment(&mut ep, &mut now);
        assert_eq!(reason, CloseReason::Silence);
        assert!(!ep.is_speaking());
    }

    #[test]
    fn endpoint_silence_of_1200ms_closes() {
        let cfg = EndpointConfig::default();
        let mut ep = Endpointer::new(cfg);
        let mut now = Instant::now();

        ep.process(&voice_frame(), 0.9, now);
        now += std::time::Duration::from_millis(16);
        ep.process(&voice_frame(), 0.9, now);

        // 74 frames de silencio (1184 ms): aún abierto.
        feed_silence(&mut ep, 74, &mut now);
        assert!(ep.is_speaking());
        // El frame 75 cierra (1200 ms) por silencio normal.
        let (reason, _) = close_reason_and_segment(&mut ep, &mut now);
        assert_eq!(reason, CloseReason::Silence);
        assert!(!ep.is_speaking());
    }

    #[test]
    fn connector_extends_timeout_to_2000ms() {
        let cfg = EndpointConfig::default();
        let mut ep = Endpointer::new(cfg);
        let mut now = Instant::now();

        ep.process(&voice_frame(), 0.9, now);
        now += std::time::Duration::from_millis(16);
        ep.process(&voice_frame(), 0.9, now);

        ep.signal_connector();

        // 1600 ms de silencio (100 frames): con conector, no cierra.
        feed_silence(&mut ep, 100, &mut now);
        assert!(ep.is_speaking());
        // 2000 ms (125 frames) sí cierra: 24 frames más siguen abiertos y el
        // 125.º emite el fin de turno.
        feed_silence(&mut ep, 24, &mut now);
        assert!(ep.is_speaking());
        let (reason, _) = close_reason_and_segment(&mut ep, &mut now);
        assert_eq!(reason, CloseReason::Connector);
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

        // 74 frames de silencio no cierran; el 75.º cierra.
        feed_silence(&mut ep, 74, &mut now);
        assert!(ep.is_speaking());
        now += std::time::Duration::from_millis(16);
        match ep.process(&frame(0.0), 0.0, now) {
            Some(VoiceEvent::EndOfTurn { segment, .. }) => {
                // pre-roll (19) + 1 voz + 75 silencio = 95 frames.
                assert_eq!(segment.pcm.len(), 95 * FRAME_SAMPLES);
                // Los primeros 19 frames (304 ms) son el pre-roll de silencio.
                assert!(segment.pcm[..19 * FRAME_SAMPLES].iter().all(|s| *s == 0.0));
                assert_eq!(segment.duration_ms, 95 * 16);
            }
            other => panic!("esperaba EndOfTurn, obtuve {other:?}"),
        }
    }

    #[test]
    fn segment_is_cut_at_30s_including_pre_roll() {
        // Techo del turno = fragmento: el corte de 30 s cierra el turno
        // (comportamiento previo a HU-44, sigue disponible por config).
        let mut ep = Endpointer::new(EndpointConfig {
            open_frames: 1,
            max_turn_ms: 30_000,
            ..EndpointConfig::default()
        });
        let mut now = Instant::now();
        feed_silence(&mut ep, 40, &mut now);

        // Habla continua: el corte debe llegar con el segmento en 30 s exactos.
        let mut ended = None;
        for _ in 0..3_000 {
            now += std::time::Duration::from_millis(16);
            if let Some(VoiceEvent::EndOfTurn {
                segment,
                close_reason,
                ..
            }) = ep.process(&voice_frame(), 0.9, now)
            {
                assert_eq!(close_reason, CloseReason::MaxTimeout);
                ended = Some(segment);
                break;
            }
        }
        let segment = ended.expect("debe cortar por duración máxima");
        assert_eq!(segment.pcm.len(), 1875 * FRAME_SAMPLES);
        assert_eq!(segment.duration_ms, 30_000);
        assert!(!ep.is_speaking());
    }

    /// Eventos de un turno largo (HU-44): parciales y cierre.
    #[derive(Debug, Default)]
    struct LongTurn {
        partials: Vec<(u32, Segment)>,
        end: Option<(CloseReason, u32, Segment)>,
        max_capacity: usize,
    }

    /// Alimenta `voice` frames de voz y luego `silence` de silencio,
    /// recogiendo los eventos y la mayor capacidad del búfer de segmento.
    fn feed_long_turn(ep: &mut Endpointer, voice: usize, silence: usize) -> LongTurn {
        let mut now = Instant::now();
        feed_silence(ep, 40, &mut now);
        let mut turn = LongTurn::default();
        let frames = std::iter::repeat_n(true, voice).chain(std::iter::repeat_n(false, silence));
        for is_voice in frames {
            now += std::time::Duration::from_millis(16);
            let (f, score) = if is_voice {
                (voice_frame(), 0.9)
            } else {
                (frame(0.0), 0.0)
            };
            match ep.process(&f, score, now) {
                Some(VoiceEvent::Partial { segment, index, .. }) => {
                    assert!(ep.is_speaking(), "el parcial no cierra el turno");
                    turn.partials.push((index, segment));
                }
                Some(VoiceEvent::EndOfTurn {
                    segment,
                    partials,
                    close_reason,
                    ..
                }) => {
                    turn.end = Some((close_reason, partials, segment));
                    break;
                }
                _ => {}
            }
            turn.max_capacity = turn.max_capacity.max(ep.segment.capacity());
        }
        turn
    }

    fn long_turn_endpointer() -> Endpointer {
        Endpointer::new(EndpointConfig {
            open_frames: 1,
            ..EndpointConfig::default()
        })
    }

    #[test]
    fn long_turn_emits_a_partial_at_30s_and_closes_on_real_silence() {
        let mut ep = long_turn_endpointer();
        // 32 s de voz continua y luego el silencio de cierre (75 frames).
        let turn = feed_long_turn(&mut ep, 2_000, 200);
        assert_eq!(turn.partials.len(), 1, "un parcial a los 30 s");
        let (index, first) = &turn.partials[0];
        assert_eq!(*index, 0);
        assert_eq!(first.pcm.len(), 1_875 * FRAME_SAMPLES);
        assert_eq!(first.duration_ms, 30_000);

        let (reason, partials, last) = turn.end.expect("cierra por silencio");
        assert_eq!(reason, CloseReason::Silence);
        assert_eq!(partials, 1);
        // Nada se pierde: pre-roll (19) + voz (2000) + silencio de cierre (75).
        let total = first.pcm.len() + last.pcm.len();
        assert_eq!(total, (19 + 2_000 + 75) * FRAME_SAMPLES);
        assert!(!ep.is_speaking());
    }

    #[test]
    fn long_turn_accumulates_up_to_the_120s_ceiling() {
        let mut ep = long_turn_endpointer();
        // Monólogo sin pausas de más de 2 minutos.
        let turn = feed_long_turn(&mut ep, 8_000, 0);
        let indices: Vec<u32> = turn.partials.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![0, 1, 2], "parciales a los 30, 60 y 90 s");
        for (_, segment) in &turn.partials {
            assert_eq!(segment.duration_ms, 30_000);
        }
        let (reason, partials, last) = turn.end.expect("techo de 120 s");
        assert_eq!(reason, CloseReason::MaxTimeout);
        assert_eq!(partials, 3);
        let total: usize = turn
            .partials
            .iter()
            .map(|(_, s)| s.pcm.len())
            .sum::<usize>()
            + last.pcm.len();
        assert_eq!(
            total,
            7_500 * FRAME_SAMPLES,
            "120 s exactos, pre-roll incluido"
        );
        assert!(!ep.is_speaking());
    }

    #[test]
    fn long_turn_memory_is_bounded_to_one_fragment() {
        let mut ep = long_turn_endpointer();
        let turn = feed_long_turn(&mut ep, 8_000, 0);
        assert!(
            turn.max_capacity <= ep.config().max_speech_frames() * FRAME_SAMPLES,
            "el búfer nunca supera un fragmento: {} muestras",
            turn.max_capacity
        );
    }

    #[test]
    fn pause_crossing_the_fragment_boundary_still_closes_the_turn() {
        let mut ep = long_turn_endpointer();
        // La voz termina 5 frames antes del corte: el fragmento sale durante
        // el silencio y el cierre llega a los 75 frames de silencio continuo.
        let turn = feed_long_turn(&mut ep, 1_875 - 19 - 5, 200);
        assert_eq!(turn.partials.len(), 1);
        let (reason, partials, last) = turn.end.expect("cierra por silencio");
        assert_eq!(reason, CloseReason::Silence);
        assert_eq!(partials, 1);
        assert_eq!(last.pcm.len(), (75 - 5) * FRAME_SAMPLES);
    }

    #[test]
    fn next_turn_after_a_long_one_starts_without_partials() {
        let mut ep = long_turn_endpointer();
        let first = feed_long_turn(&mut ep, 2_000, 200);
        assert_eq!(first.end.expect("cierre").1, 1);
        let second = feed_long_turn(&mut ep, 100, 200);
        assert!(second.partials.is_empty());
        let (reason, partials, _) = second.end.expect("cierre");
        assert_eq!(reason, CloseReason::Silence);
        assert_eq!(partials, 0);
    }

    #[test]
    fn max_turn_frames_never_below_one_fragment() {
        let cfg = EndpointConfig {
            max_segment_ms: 60_000,
            max_turn_ms: 10_000,
            ..EndpointConfig::default()
        };
        assert_eq!(cfg.max_turn_frames(), cfg.max_speech_frames());
        assert_eq!(EndpointConfig::default().max_turn_frames(), 7_500);
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
        feed_silence(&mut ep, 124, &mut now);
        now += std::time::Duration::from_millis(16);
        assert!(ep.process(&frame(0.0), 0.0, now).is_some());

        // Nuevo turno sin conector: vuelve a cerrar a los 1200 ms.
        now += std::time::Duration::from_millis(16);
        ep.process(&voice_frame(), 0.9, now);
        feed_silence(&mut ep, 74, &mut now);
        now += std::time::Duration::from_millis(16);
        assert!(ep.process(&frame(0.0), 0.0, now).is_some());
    }

    #[test]
    fn endpoint_config_from_voice_endpoint_maps_user_values() {
        let voice = VoiceEndpointConfig {
            voice_open: 0.6,
            voice_hold: 0.3,
            silence_ms: 900,
            extended_silence_ms: 2_400,
            max_segment_ms: 45_000,
            max_turn_ms: 90_000,
            connector_grace_ms: 700,
        };
        let cfg = EndpointConfig::from(&voice);
        assert_eq!(cfg.max_turn_ms, 90_000);
        assert_eq!(cfg.voice_open, 0.6);
        assert_eq!(cfg.voice_hold, 0.3);
        assert_eq!(cfg.silence_ms, 900);
        assert_eq!(cfg.extended_silence_ms, 2_400);
        assert_eq!(cfg.max_segment_ms, 45_000);
        // Los detalles internos del detector se quedan en su valor por defecto.
        let defaults = EndpointConfig::default();
        assert_eq!(cfg.open_frames, defaults.open_frames);
        assert_eq!(cfg.pre_roll_ms, defaults.pre_roll_ms);
        // `connector_grace_ms` es del orquestador, no del endpointer.
        assert_eq!(voice.connector_grace_ms(), 700);
    }

    #[test]
    fn endpoint_config_from_voice_endpoint_saturates_ranges() {
        // Un TOML editado a mano con valores imposibles no rompe el detector.
        let voice = VoiceEndpointConfig {
            voice_open: 0.0,
            voice_hold: 99.0,
            silence_ms: 0,
            extended_silence_ms: 1,
            max_segment_ms: 1,
            max_turn_ms: 1,
            connector_grace_ms: 0,
        };
        let cfg = EndpointConfig::from(&voice);
        assert_eq!(cfg.voice_open, 0.1);
        assert_eq!(cfg.voice_hold, 0.8);
        assert_eq!(cfg.silence_ms, 300);
        assert_eq!(cfg.extended_silence_ms, 500);
        assert_eq!(cfg.max_segment_ms, 5_000);
        assert_eq!(cfg.max_turn_ms, 5_000);
        // Con `max_segment_ms` mínimo, el corte por duración máxima sigue siendo
        // coherente (al menos un frame).
        assert!(cfg.max_speech_frames() >= 1);
    }

    #[test]
    fn pause_stats_record_intra_turn_pauses() {
        let mut ep = Endpointer::new(EndpointConfig {
            open_frames: 1,
            ..EndpointConfig::default()
        });
        let mut now = Instant::now();
        assert!(ep.pause_stats().is_empty());

        ep.process(&voice_frame(), 0.9, now);
        // Pausa de 20 frames (320 ms) que se reanuda con voz: intra-turno.
        feed_silence(&mut ep, 20, &mut now);
        now += std::time::Duration::from_millis(16);
        assert!(ep.process(&voice_frame(), 0.9, now).is_none());
        // Pausa corta de 5 frames (80 ms).
        feed_silence(&mut ep, 5, &mut now);
        now += std::time::Duration::from_millis(16);
        assert!(ep.process(&voice_frame(), 0.9, now).is_none());

        let stats = ep.pause_stats();
        assert_eq!(stats.recorded(), 2);
        assert_eq!(stats.samples().collect::<Vec<_>>(), vec![320, 80]);
        assert_eq!(stats.p50(), Some(80));
        assert_eq!(stats.p90(), Some(320));
        assert_eq!(
            stats.summary(),
            TurnPauseSummary {
                samples: 2,
                p50_ms: Some(80),
                p90_ms: Some(320),
            }
        );

        // El silencio que cierra el turno no es una pausa intra-turno.
        feed_silence(&mut ep, 74, &mut now);
        let (reason, _) = close_reason_and_segment(&mut ep, &mut now);
        assert_eq!(reason, CloseReason::Silence);
        assert_eq!(ep.pause_stats().recorded(), 2);
    }

    #[test]
    fn pause_stats_survive_reset_but_can_be_taken() {
        let mut ep = Endpointer::new(EndpointConfig {
            open_frames: 1,
            ..EndpointConfig::default()
        });
        let mut now = Instant::now();
        ep.process(&voice_frame(), 0.9, now);
        feed_silence(&mut ep, 10, &mut now);
        now += std::time::Duration::from_millis(16);
        ep.process(&voice_frame(), 0.9, now);
        assert_eq!(ep.pause_stats().recorded(), 1);

        // `reset` reinicia el estado del detector, no el histograma acumulado:
        // el volcado de `--diag-turns` es de toda la sesión.
        ep.reset();
        assert_eq!(ep.pause_stats().recorded(), 1);

        let taken = ep.take_pause_stats();
        assert_eq!(taken.recorded(), 1);
        assert!(ep.pause_stats().is_empty());
    }

    #[test]
    fn turn_pause_stats_percentiles_use_nearest_rank() {
        let mut stats = TurnPauseStats::new();
        assert_eq!(stats.p50(), None);
        assert_eq!(stats.p90(), None);
        assert!(stats.is_empty());

        for value in (10..=100).step_by(10) {
            stats.record(value);
        }
        assert_eq!(stats.len(), 10);
        assert_eq!(stats.p50(), Some(50));
        assert_eq!(stats.p90(), Some(90));
        assert_eq!(stats.percentile(0.0), Some(10));
        assert_eq!(stats.percentile(100.0), Some(100));
        assert_eq!(stats.percentile(95.0), Some(100));
        // Fuera de rango se satura a 0–100.
        assert_eq!(stats.percentile(-5.0), Some(10));
        assert_eq!(stats.percentile(1_000.0), Some(100));
        // Las pausas nulas no sesgan los percentiles.
        stats.record(0);
        assert_eq!(stats.len(), 10);
    }

    #[test]
    fn turn_pause_stats_absorb_keeps_bound_and_total() {
        let mut total = TurnPauseStats::new();
        let mut turn = TurnPauseStats::new();
        turn.record(100);
        turn.record(300);
        total.absorb(&turn);
        assert_eq!(total.samples().collect::<Vec<_>>(), vec![100, 300]);
        assert_eq!(total.recorded(), 2);

        let mut big = TurnPauseStats::new();
        for value in 1..=TurnPauseStats::MAX_SAMPLES as u32 {
            big.record(value);
        }
        total.absorb(&big);
        assert_eq!(total.len(), TurnPauseStats::MAX_SAMPLES);
        assert_eq!(total.recorded(), 2 + TurnPauseStats::MAX_SAMPLES as u64);
        // Las del primer turno salieron del anillo: quedan las más recientes.
        assert_eq!(total.samples().next(), Some(1));
    }

    #[test]
    fn turn_pause_stats_ring_is_bounded() {
        let mut stats = TurnPauseStats::new();
        for value in 1..=300u32 {
            stats.record(value);
        }
        assert_eq!(stats.len(), TurnPauseStats::MAX_SAMPLES);
        assert_eq!(stats.recorded(), 300);
        // Se retienen las más recientes: 300 − 256 + 1 = 45.
        assert_eq!(stats.samples().min(), Some(45));
        assert_eq!(stats.samples().max(), Some(300));
        stats.clear();
        assert!(stats.is_empty());
        assert_eq!(stats.recorded(), 0);
        assert_eq!(stats.p50(), None);
    }
}
