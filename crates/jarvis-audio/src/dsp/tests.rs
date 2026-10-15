//! Tests de la cadena DSP: AEC sintético (HU-2) y robustez SEC-08.

use super::*;
use crate::vad::{VadDetector, FRAME_SAMPLES};

/// Ruido blanco uniforme determinista (LCG) en `[-peak, peak]`.
struct Noise(u32);

impl Noise {
    fn next(&mut self, peak: f32) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.0 >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0) * peak
    }
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|s| f64::from(*s) * f64::from(*s)).sum()
}

fn db(ratio: f64) -> f64 {
    10.0 * ratio.max(1e-20).log10()
}

/// Resultado de una corrida del AEC sintético.
struct AecRun {
    /// Salida limpia completa.
    out: Vec<f32>,
    /// Eco que entró al micrófono.
    echo: Vec<f32>,
    /// "Voz" que entró al micrófono.
    voice: Vec<f32>,
    erle: Option<f64>,
}

/// Referencia = ruido blanco; captura = referencia con retardo y atenuación
/// (eco) + "voz" opcional (tono de 440 Hz con envolvente silábica).
fn run_aec(config: ProcessorConfig, with_voice: bool, seconds: usize) -> AecRun {
    const DELAY: usize = 640; // 40 ms a 16 kHz
    const ECHO_GAIN: f32 = 0.5;
    let total = seconds * DSP_RATE as usize;
    let mut noise = Noise(0x0bad_cafe);
    let reference: Vec<f32> = (0..total).map(|_| noise.next(0.4)).collect();
    let echo: Vec<f32> = (0..total)
        .map(|i| {
            if i >= DELAY {
                reference[i - DELAY] * ECHO_GAIN
            } else {
                0.0
            }
        })
        .collect();
    let voice: Vec<f32> = (0..total)
        .map(|i| {
            if !with_voice {
                return 0.0;
            }
            let t = i as f32 / DSP_RATE as f32;
            // Ráfagas de 400 ms cada 800 ms, como sílabas.
            let envelope = if (t % 0.8) < 0.4 { 1.0 } else { 0.0 };
            0.2 * envelope * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
        })
        .collect();

    let mut apm = SonoraProcessor::new(config);
    apm.set_stream_delay_ms((DELAY * 1000 / DSP_RATE as usize) as i32);
    let mut out = vec![0.0f32; total];
    let mut mic = [0.0f32; DSP_FRAME];
    for start in (0..total).step_by(DSP_FRAME) {
        let end = start + DSP_FRAME;
        apm.process_render(&reference[start..end])
            .expect("render 160");
        for (k, m) in mic.iter_mut().enumerate() {
            *m = echo[start + k] + voice[start + k];
        }
        apm.process_capture(&mic, &mut out[start..end])
            .expect("capture 160");
    }
    AecRun {
        out,
        echo,
        voice,
        erle: apm.erle_db(),
    }
}

/// Atenuación del eco (dB) en los últimos `tail_s` segundos de una corrida
/// sin voz: energía del eco de entrada vs. energía residual de salida.
fn echo_attenuation_db(run: &AecRun, tail_s: usize) -> f64 {
    let start = run.out.len() - tail_s * DSP_RATE as usize;
    db(energy(&run.echo[start..]) / energy(&run.out[start..]))
}

#[test]
fn aec3_attenuates_reference_by_at_least_20db() {
    let run = run_aec(
        ProcessorConfig {
            echo_cancellation: true,
            noise_suppression: false,
            gain_control: false,
        },
        false,
        10,
    );
    let attenuation = echo_attenuation_db(&run, 4);
    eprintln!(
        "AEC sintético (solo AEC3): atenuación del eco {attenuation:.1} dB, ERLE sonora {:?}",
        run.erle
    );
    assert!(
        attenuation >= 20.0,
        "el eco residual solo cayó {attenuation:.1} dB (CA-2.3: evaluar plan B)"
    );
}

#[test]
fn aec3_preserves_voice_during_double_talk() {
    let config = ProcessorConfig {
        echo_cancellation: true,
        noise_suppression: false,
        gain_control: false,
    };
    let talk = run_aec(config, true, 10);
    let start = talk.out.len() - 4 * DSP_RATE as usize;

    // Tramos con voz (doble habla) y huecos sin voz (solo eco), separados por
    // la envolvente conocida; se descartan 20 ms en cada borde de ráfaga.
    let (mut voice_in, mut voice_out) = (0.0f64, 0.0f64);
    let (mut gap_echo, mut gap_out) = (0.0f64, 0.0f64);
    for i in start..talk.out.len() {
        let phase = (i as f32 / DSP_RATE as f32) % 0.8;
        let sq = |x: f32| f64::from(x) * f64::from(x);
        if (0.02..0.38).contains(&phase) {
            voice_in += sq(talk.voice[i]);
            voice_out += sq(talk.out[i]);
        } else if (0.42..0.78).contains(&phase) {
            gap_echo += sq(talk.echo[i]);
            gap_out += sq(talk.out[i]);
        }
    }
    let voice_kept = db(voice_out / voice_in);
    let echo_drop = db(gap_echo / gap_out);
    eprintln!(
        "AEC sintético con doble habla: voz conservada {voice_kept:.1} dB, \
         eco residual entre ráfagas −{echo_drop:.1} dB"
    );
    assert!(voice_kept > -6.0, "la voz se atenuó {voice_kept:.1} dB");
    assert!(echo_drop >= 20.0, "eco residual solo −{echo_drop:.1} dB");
}

#[test]
fn full_chain_aec_ns_agc_reports_numbers_and_stays_bounded() {
    let run = run_aec(ProcessorConfig::default(), false, 10);
    let attenuation = echo_attenuation_db(&run, 4);
    eprintln!(
        "AEC3+NS+AGC2: atenuación del eco {attenuation:.1} dB, ERLE sonora {:?}",
        run.erle
    );
    assert!(run.out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
    assert!(
        attenuation >= 20.0,
        "con NS+AGC2 el eco cayó {attenuation:.1} dB"
    );
}

/// Ejecuta la cadena completa a `rate` con la misma señal en captura y
/// referencia, y devuelve los frames de 256 que llegan al VAD.
fn run_chain(rate: u32, capture: &[f32], chunk: usize) -> (Vec<f32>, DspMetricsSnapshot) {
    let metrics = Arc::new(DspMetrics::new());
    let mut pipeline = DspPipeline::new(
        rate,
        rate,
        FRAME_SAMPLES,
        Box::new(SonoraProcessor::new(ProcessorConfig::default())),
        Arc::clone(&metrics),
    )
    .expect("tasa soportada");
    let mut vad = VadDetector::new();
    let mut frames = Vec::new();
    let chunk = chunk.max(1);
    for piece in capture.chunks(chunk) {
        pipeline.push_render(piece);
        pipeline.push_capture(piece, |frame| {
            assert_eq!(frame.len(), FRAME_SAMPLES);
            let score = vad.score(frame);
            assert!((0.0..=1.0).contains(&score));
            frames.extend_from_slice(frame);
        });
    }
    // Llamadas vacías: no deben producir nada ni fallar.
    pipeline.push_render(&[]);
    pipeline.push_capture(&[], |_| {});
    (frames, metrics.snapshot())
}

#[test]
fn sec08_robustness_across_rates_and_pathological_input() {
    let rates = [8_000u32, 22_050, 44_100, 48_000, 96_000];
    for rate in rates {
        let n = rate as usize / 2; // 500 ms
        let silence = vec![0.0f32; n];
        let saturated: Vec<f32> = (0..n).map(|i| if i % 7 < 3 { 8.0 } else { -8.0 }).collect();
        let non_finite: Vec<f32> = (0..n)
            .map(|i| match i % 4 {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => f32::NEG_INFINITY,
                _ => 0.3,
            })
            .collect();
        for (name, signal) in [
            ("silencio", &silence),
            ("saturación", &saturated),
            ("NaN/inf", &non_finite),
        ] {
            // Tramos de 1 muestra, irregulares y grandes.
            for chunk in [1usize, 37, 4_096] {
                let (frames, snapshot) = run_chain(rate, signal, chunk);
                assert!(
                    frames.iter().all(|s| s.is_finite() && s.abs() <= 1.0),
                    "{rate} Hz / {name} / tramo {chunk}: salida fuera de rango"
                );
                // 500 ms a 16 kHz = 8 000 muestras, menos el retardo del filtro
                // y lo que queda pendiente en los acumuladores.
                assert!(
                    frames.len() >= 6_000,
                    "{rate} Hz / {name}: solo {} muestras",
                    frames.len()
                );
                assert_eq!(snapshot.processor_errors, 0);
                assert_eq!(snapshot.resampler_errors, 0);
            }
        }
    }
}

#[test]
fn sec08_zero_and_one_sample_frames_do_not_panic() {
    let metrics = Arc::new(DspMetrics::new());
    let mut pipeline = DspPipeline::new(
        48_000,
        44_100,
        FRAME_SAMPLES,
        Box::new(SonoraProcessor::new(ProcessorConfig::default())),
        metrics,
    )
    .expect("tasas soportadas");
    let mut produced = 0usize;
    for _ in 0..2_000 {
        pipeline.push_render(&[]);
        pipeline.push_render(&[f32::NAN]);
        pipeline.push_capture(&[], |_| produced += 1);
        pipeline.push_capture(&[f32::INFINITY], |_| produced += 1);
    }
    // 2 000 muestras a 48 kHz ≈ 666 a 16 kHz → al menos 1 frame de 256.
    assert!(produced >= 1);
}

#[test]
fn processor_rejects_wrong_frame_length_without_panicking() {
    let mut apm = SonoraProcessor::new(ProcessorConfig::default());
    let mut out = [0.0f32; 160];
    assert!(matches!(
        apm.process_render(&[0.0; 10]),
        Err(DspError::FrameLength(10))
    ));
    assert!(apm.process_capture(&[0.0; 0], &mut out).is_err());
}

/// Procesador que siempre falla: la señal debe pasar sin procesar.
struct FailingProcessor;

impl EchoProcessor for FailingProcessor {
    fn process_render(&mut self, _frame: &[f32]) -> Result<(), DspError> {
        Err(DspError::FrameLength(0))
    }
    fn process_capture(&mut self, _input: &[f32], _output: &mut [f32]) -> Result<(), DspError> {
        Err(DspError::FrameLength(0))
    }
    fn set_stream_delay_ms(&mut self, _delay_ms: i32) {}
    fn erle_db(&self) -> Option<f64> {
        None
    }
}

#[test]
fn processor_errors_are_counted_and_signal_passes_through() {
    let metrics = Arc::new(DspMetrics::new());
    let mut pipeline = DspPipeline::new(
        DSP_RATE,
        DSP_RATE,
        FRAME_SAMPLES,
        Box::new(FailingProcessor),
        Arc::clone(&metrics),
    )
    .expect("16 kHz");
    // 1 280 muestras = 8 frames de 160 = 5 frames de 256: nada queda pendiente.
    let input: Vec<f32> = (0..1_280).map(|i| (i as f32 / 1_280.0) - 0.5).collect();
    let mut out = Vec::new();
    pipeline.push_render(&input);
    pipeline.push_capture(&input, |f| out.extend_from_slice(f));
    assert_eq!(
        out, input,
        "a 16 kHz sin procesador la señal debe pasar intacta"
    );
    let s = metrics.snapshot();
    assert_eq!(s.processor_errors, 16);
    assert_eq!(s.capture_frames, 8);
}

#[test]
fn white_noise_after_ns_does_not_open_a_turn() {
    // Calibración: el mismo ruido que en `vad::tests` (−20 dBFS RMS), ahora
    // pasando por NS+AGC2 antes del VAD, durante 5 s.
    let rate = 48_000u32;
    let mut noise = Noise(0x1234_5678);
    let signal: Vec<f32> = (0..rate as usize * 5)
        .map(|_| noise.next(0.1 * 3f32.sqrt()))
        .collect();
    let metrics = Arc::new(DspMetrics::new());
    let mut pipeline = DspPipeline::new(
        rate,
        rate,
        FRAME_SAMPLES,
        Box::new(SonoraProcessor::new(ProcessorConfig::default())),
        metrics,
    )
    .expect("48 kHz");
    let silence = vec![0.0f32; 480];
    let mut vad = VadDetector::new();
    let mut endpointer = crate::vad::Endpointer::new(crate::vad::EndpointConfig::default());
    let (mut opened, mut max_score) = (false, 0.0f32);
    let now = std::time::Instant::now();
    for piece in signal.chunks(480) {
        pipeline.push_render(&silence);
        pipeline.push_capture(piece, |frame| {
            let score = vad.score(frame);
            max_score = max_score.max(score);
            opened |= endpointer.process(frame, score, now).is_some();
        });
    }
    eprintln!("ruido −20 dBFS tras NS+AGC2: puntaje VAD máx {max_score:.3}, abrió turno: {opened}");
    assert!(!opened, "el ruido estacionario no debe abrir turno tras NS");
}

#[test]
fn dsp_thread_emits_events_and_stops_cleanly() {
    use ringbuf::traits::{Producer, Split};
    use ringbuf::HeapRb;

    let (mut cap_prod, cap_cons) = HeapRb::<f32>::new(48_000).split();
    let (mut ren_prod, ren_cons) = HeapRb::<f32>::new(48_000).split();
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut handle = spawn_dsp_thread(
        DspThreadConfig {
            capture_rate: 48_000,
            render_rate: 48_000,
            processor: ProcessorConfig::default(),
            endpoint: crate::vad::EndpointConfig::default(),
        },
        cap_cons,
        ren_cons,
        Arc::new(LatencyProbe::new()),
        tx,
    )
    .expect("hilo DSP");
    handle.metrics().set_diagnostics(true);

    // 1 s de silencio en ambos sentidos, en tramos de 10 ms.
    let chunk = [0.0f32; 480];
    for _ in 0..100 {
        while cap_prod.push_slice(&chunk) == 0 || ren_prod.push_slice(&chunk) == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while handle.metrics().snapshot().capture_frames < 90 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let snapshot = handle.metrics().snapshot();
    assert!(snapshot.capture_frames >= 90, "procesó {snapshot:?}");
    assert!(snapshot.render_frames >= 90);
    assert!(handle.is_running());
    // El silencio no produce eventos de voz.
    assert!(rx.try_recv().is_err());

    handle.stop();
    assert!(!handle.is_running());
}

use std::time::Duration;
