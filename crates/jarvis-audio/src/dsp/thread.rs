//! Hilo `jarvis-audio-dsp`: DSP + VAD + *endpointing* (hilo 2 del contrato §5).
//!
//! Consume los ring buffers que llenan los callbacks de cpal (captura y
//! referencia de salida), ejecuta [`DspPipeline`], puntúa cada frame limpio con
//! `earshot` y publica [`VoiceEvent`] por `crossbeam-channel`. Los hilos STT,
//! LLM y TTS se conectan desde fuera: el STT avisa conectores con
//! [`DspHandle::signal_connector`] y el coordinador consume los eventos.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ringbuf::traits::Consumer;
use ringbuf::HeapCons;

use super::{
    stream_delay_ms, DspError, DspMetrics, DspPipeline, LatencyProbe, ProcessorConfig,
    SonoraProcessor,
};
use crate::events::VoiceEvent;
use crate::vad::{EndpointConfig, Endpointer, VadDetector, FRAME_SAMPLES};

/// Espera cuando ambas colas están vacías (menos de un frame de 10 ms).
const IDLE_SLEEP: Duration = Duration::from_millis(4);
/// Muestras que se extraen de cada cola por vuelta.
const POP_CHUNK: usize = 4_096;
/// Frames de 10 ms entre lecturas del ERLE en modo diagnóstico (1 s).
const DIAGNOSTIC_PERIOD: u64 = 100;

/// Parámetros del hilo DSP.
#[derive(Debug, Clone)]
pub struct DspThreadConfig {
    /// Tasa negociada del dispositivo de entrada.
    pub capture_rate: u32,
    /// Tasa negociada del dispositivo de salida (referencia de AEC).
    pub render_rate: u32,
    pub processor: ProcessorConfig,
    pub endpoint: EndpointConfig,
}

/// Señal de conector compartible: equivale a [`DspHandle::signal_connector`]
/// sin tener que poseer el `DspHandle` (vive en el supervisor de audio).
#[derive(Debug, Clone)]
pub struct ConnectorSignal(Arc<AtomicBool>);

impl ConnectorSignal {
    /// Señal suelta (sin hilo DSP detrás): para dobles de prueba.
    pub fn detached() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn signal(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// `true` si hay una señal pendiente que el DSP aún no consumió.
    pub fn is_pending(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Control del hilo DSP. Al soltarlo, el hilo se detiene y se espera.
pub struct DspHandle {
    shutdown: Arc<AtomicBool>,
    connector: Arc<AtomicBool>,
    metrics: Arc<DspMetrics>,
    join: Option<JoinHandle<()>>,
}

impl DspHandle {
    /// Métricas compartidas (ERLE, retardo, errores, puntaje del VAD).
    pub fn metrics(&self) -> &Arc<DspMetrics> {
        &self.metrics
    }

    /// El STT detectó que la transcripción parcial termina en conector: el
    /// turno en curso espera hasta `extended_silence_ms` antes de cerrarse.
    pub fn signal_connector(&self) {
        self.connector.store(true, Ordering::Release);
    }

    /// Copia clonable de la señal de conector para otros hilos (STT).
    pub fn connector_signal(&self) -> ConnectorSignal {
        ConnectorSignal(Arc::clone(&self.connector))
    }

    /// `true` mientras el hilo sigue vivo.
    pub fn is_running(&self) -> bool {
        self.join.as_ref().is_some_and(|j| !j.is_finished())
    }

    /// Detiene el hilo y espera a que termine.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for DspHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Arranca el hilo DSP.
///
/// `capture` y `render` son los consumidores de los ring buffers que llenan
/// los callbacks de entrada y salida; `latency` recibe las latencias que esos
/// callbacks leen de los timestamps de cpal. El hilo termina al soltar el
/// [`DspHandle`] o cuando el receptor de `events` desaparece.
pub fn spawn_dsp_thread(
    config: DspThreadConfig,
    mut capture: HeapCons<f32>,
    mut render: HeapCons<f32>,
    latency: Arc<LatencyProbe>,
    events: Sender<VoiceEvent>,
) -> Result<DspHandle, DspError> {
    let metrics = Arc::new(DspMetrics::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let connector = Arc::new(AtomicBool::new(false));

    // La cadena se construye aquí para devolver el error de configuración al
    // llamador en vez de perderlo dentro del hilo.
    let mut pipeline = DspPipeline::new(
        config.capture_rate,
        config.render_rate,
        FRAME_SAMPLES,
        Box::new(SonoraProcessor::new(config.processor)),
        Arc::clone(&metrics),
    )?;
    let mut vad = VadDetector::new();
    let mut endpointer = Endpointer::new(config.endpoint);

    let thread_metrics = Arc::clone(&metrics);
    let thread_shutdown = Arc::clone(&shutdown);
    let thread_connector = Arc::clone(&connector);
    let join = std::thread::Builder::new()
        .name("jarvis-audio-dsp".into())
        .spawn(move || {
            let mut buf = vec![0.0f32; POP_CHUNK];
            let mut receiver_gone = false;
            let mut last_diagnostic = 0u64;
            while !thread_shutdown.load(Ordering::Acquire) && !receiver_gone {
                let mut idle = true;

                // Referencia primero: AEC3 necesita ver lo que sonó antes de
                // procesar la captura que lo contiene.
                loop {
                    let n = render.pop_slice(&mut buf);
                    if n == 0 {
                        break;
                    }
                    idle = false;
                    pipeline.push_render(&buf[..n]);
                }

                let mut terms = pipeline.delay_terms();
                terms.output_latency = latency.output_latency();
                terms.input_latency = latency.input_latency();
                pipeline.set_stream_delay_ms(stream_delay_ms(&terms));

                loop {
                    let n = capture.pop_slice(&mut buf);
                    if n == 0 {
                        break;
                    }
                    idle = false;
                    pipeline.push_capture(&buf[..n], |frame| {
                        let score = vad.score(frame);
                        thread_metrics.set_vad_score(score);
                        if thread_connector.swap(false, Ordering::AcqRel) {
                            endpointer.signal_connector();
                        }
                        if let Some(event) = endpointer.process(frame, score, Instant::now()) {
                            // Al cerrar el turno, sus pausas pasan a las
                            // métricas compartidas (HU-41, `--diag-turns`).
                            if matches!(event, VoiceEvent::EndOfTurn { .. }) {
                                thread_metrics.absorb_pauses(&endpointer.take_pause_stats());
                            }
                            receiver_gone |= events.send(event).is_err();
                        }
                    });
                }

                if thread_metrics.diagnostics_enabled() {
                    let frames = thread_metrics.snapshot().capture_frames;
                    if frames.saturating_sub(last_diagnostic) >= DIAGNOSTIC_PERIOD {
                        last_diagnostic = frames;
                        thread_metrics.set_erle(pipeline.erle_db());
                    }
                }

                if idle {
                    std::thread::sleep(IDLE_SLEEP);
                }
            }
        })
        .map_err(DspError::Spawn)?;

    Ok(DspHandle {
        shutdown,
        connector,
        metrics,
        join: Some(join),
    })
}
