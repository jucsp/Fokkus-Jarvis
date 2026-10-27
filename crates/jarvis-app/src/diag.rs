//! `fokkus-jarvis --diag-turns [segundos]`: diagnóstico del *endpointing* de
//! turno (HU-41).
//!
//! Abre el micrófono con la tabla `[voice_endpoint]` de `config.toml`, escucha
//! durante una ventana fija sin transcribir ni responder, y vuelca por qué se
//! cerró cada turno (`CloseReason`) y la distribución de pausas intra-turno
//! (P50/P90). Con eso se calibra `silence_ms`: si el P90 de las pausas se
//! acerca al silencio de cierre, Jarvis está cortando al usuario a media frase.
//! El audio nunca sale de memoria (contrato HU-10).

use std::fmt::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use jarvis_audio::{
    AudioEngine, AudioPlayback, CloseReason, EndpointConfig, EngineConfig, TurnPauseStats,
    TurnPauseSummary, VoiceEvent,
};
use jarvis_core::{JarvisConfig, VoiceEndpointConfig};

/// Ventana de escucha por defecto.
pub const DEFAULT_SECS: u64 = 60;
/// Rango aceptado para la ventana de escucha.
pub const SECS_RANGE: std::ops::RangeInclusive<u64> = 5..=3_600;

/// Fracción de `silence_ms` a partir de la cual el P90 de pausas indica que
/// el cierre es demasiado agresivo.
const TIGHT_RATIO: f64 = 0.8;

/// Interpreta el argumento opcional de `--diag-turns`.
pub fn parse_secs(args: &[String]) -> Result<Duration, String> {
    let secs = match args {
        [] => DEFAULT_SECS,
        [value] => value
            .parse::<u64>()
            .ok()
            .filter(|s| SECS_RANGE.contains(s))
            .ok_or_else(|| {
                format!(
                    "segundos inválidos: {value} (rango {}–{})",
                    SECS_RANGE.start(),
                    SECS_RANGE.end()
                )
            })?,
        _ => return Err("uso: --diag-turns [segundos]".into()),
    };
    Ok(Duration::from_secs(secs))
}

/// Conteo de cierres de turno por razón.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CloseCounts {
    pub silence: u64,
    pub max_timeout: u64,
    pub connector: u64,
}

impl CloseCounts {
    pub fn count(&mut self, reason: CloseReason) {
        match reason {
            CloseReason::Silence => self.silence += 1,
            CloseReason::MaxTimeout => self.max_timeout += 1,
            CloseReason::Connector => self.connector += 1,
        }
    }

    pub fn total(&self) -> u64 {
        self.silence + self.max_timeout + self.connector
    }
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

fn ms_or_dash(value: Option<u32>) -> String {
    value.map_or_else(|| "—".to_string(), |ms| format!("{ms} ms"))
}

/// Resumen legible del diagnóstico. Función pura para poder probarla.
pub fn format_report(
    endpoint: &VoiceEndpointConfig,
    window: Duration,
    closes: &CloseCounts,
    pauses: &TurnPauseSummary,
    recorded_pauses: u64,
) -> String {
    let mut out = String::new();
    let total = closes.total();
    let _ = writeln!(out, "── Diagnóstico de turnos (HU-41) ──");
    let _ = writeln!(
        out,
        "Config: voice_open={:.2} voice_hold={:.2} silence_ms={} extended_silence_ms={} \
         max_segment_ms={} max_turn_ms={} connector_grace_ms={}",
        endpoint.voice_open,
        endpoint.voice_hold,
        endpoint.silence_ms,
        endpoint.extended_silence_ms,
        endpoint.max_segment_ms,
        endpoint.max_turn_ms,
        endpoint.connector_grace_ms,
    );
    let _ = writeln!(out, "Ventana: {} s", window.as_secs());
    let _ = writeln!(out, "Turnos cerrados: {total}");
    for (label, n) in [
        (CloseReason::Silence.as_str(), closes.silence),
        (CloseReason::Connector.as_str(), closes.connector),
        (CloseReason::MaxTimeout.as_str(), closes.max_timeout),
    ] {
        let _ = writeln!(out, "  {label:<12} {n:>4} ({:.0} %)", pct(n, total));
    }
    let _ = writeln!(
        out,
        "Pausas intra-turno: {recorded_pauses} (retenidas {}) · P50 {} · P90 {}",
        pauses.samples,
        ms_or_dash(pauses.p50_ms),
        ms_or_dash(pauses.p90_ms),
    );
    for hint in hints(endpoint, closes, pauses) {
        let _ = writeln!(out, "→ {hint}");
    }
    out
}

/// Sugerencias de calibración a partir de las métricas.
pub fn hints(
    endpoint: &VoiceEndpointConfig,
    closes: &CloseCounts,
    pauses: &TurnPauseSummary,
) -> Vec<String> {
    let mut out = Vec::new();
    if closes.total() == 0 {
        out.push("Sin turnos: habla durante la ventana para obtener métricas.".into());
        return out;
    }
    if let Some(p90) = pauses.p90_ms {
        if f64::from(p90) >= f64::from(endpoint.silence_ms) * TIGHT_RATIO {
            out.push(format!(
                "El P90 de pausas ({p90} ms) roza silence_ms ({} ms): considera subirlo.",
                endpoint.silence_ms
            ));
        }
    }
    if closes.max_timeout > 0 {
        out.push(format!(
            "{} turno(s) cortados por el techo total del turno: considera subir max_turn_ms.",
            closes.max_timeout
        ));
    }
    out
}

/// Ejecuta el diagnóstico con el micrófono real y sale.
pub fn run(window: Duration) -> ExitCode {
    let config = match JarvisConfig::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("[config] ilegible ({e}); usando valores por defecto.");
            JarvisConfig::default()
        }
    };
    let endpoint = config.voice_endpoint();
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let engine_config = EngineConfig {
        endpoint: EndpointConfig::from(&endpoint),
        ..EngineConfig::default()
    };
    let mut engine =
        match AudioEngine::start_cpal(engine_config, None, None, AudioPlayback::new(), events_tx) {
            Ok(engine) => engine,
            Err(e) => {
                eprintln!("[diag] no se pudo arrancar el motor de audio: {e}");
                return ExitCode::FAILURE;
            }
        };
    engine.set_enabled(true);
    println!(
        "[diag] escuchando {} s; habla con pausas naturales…",
        window.as_secs()
    );

    let deadline = Instant::now() + window;
    let mut closes = CloseCounts::default();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events_rx.recv_timeout(left) {
            Ok(VoiceEvent::EndOfTurn {
                segment,
                close_reason,
                ..
            }) => {
                closes.count(close_reason);
                println!(
                    "[diag] turno {}: cierre={} duración={} ms",
                    closes.total(),
                    close_reason.as_str(),
                    segment.duration_ms
                );
            }
            Ok(VoiceEvent::Partial { segment, index, .. }) => {
                // Turno largo (HU-44): el turno sigue abierto.
                println!(
                    "[diag] fragmento parcial {} ({} ms), el turno sigue abierto",
                    index + 1,
                    segment.duration_ms
                );
            }
            Ok(VoiceEvent::VoiceStart { .. }) => {}
            Err(_) => break,
        }
    }

    // Las pausas viven en las métricas del hilo DSP (se suman al cerrar
    // cada turno). Sin DSP (dispositivo caído) el histograma queda vacío.
    let pauses = engine
        .shared()
        .dsp_metrics()
        .map(|m| m.pause_stats())
        .unwrap_or_else(TurnPauseStats::new);
    engine.shutdown();

    print!(
        "{}",
        format_report(
            &endpoint,
            window,
            &closes,
            &pauses.summary(),
            pauses.recorded()
        )
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_secs_defaults_and_validates_range() {
        assert_eq!(parse_secs(&[]), Ok(Duration::from_secs(DEFAULT_SECS)));
        assert_eq!(parse_secs(&args(&["30"])), Ok(Duration::from_secs(30)));
        assert!(parse_secs(&args(&["0"])).is_err());
        assert!(parse_secs(&args(&["4"])).is_err());
        assert!(parse_secs(&args(&["3601"])).is_err());
        assert!(parse_secs(&args(&["abc"])).is_err());
        assert!(parse_secs(&args(&["10", "20"])).is_err());
    }

    #[test]
    fn close_counts_tally_by_reason() {
        let mut c = CloseCounts::default();
        c.count(CloseReason::Silence);
        c.count(CloseReason::Silence);
        c.count(CloseReason::Connector);
        c.count(CloseReason::MaxTimeout);
        assert_eq!(
            c,
            CloseCounts {
                silence: 2,
                max_timeout: 1,
                connector: 1
            }
        );
        assert_eq!(c.total(), 4);
    }

    #[test]
    fn report_includes_reasons_percentiles_and_hints() {
        let endpoint = VoiceEndpointConfig::default();
        let closes = CloseCounts {
            silence: 3,
            max_timeout: 1,
            connector: 0,
        };
        let pauses = TurnPauseSummary {
            samples: 12,
            p50_ms: Some(400),
            p90_ms: Some(1_000),
        };
        let report = format_report(&endpoint, Duration::from_secs(60), &closes, &pauses, 15);
        assert!(report.contains("silence_ms=1200"), "{report}");
        assert!(report.contains("Turnos cerrados: 4"), "{report}");
        assert!(report.contains("silence         3 (75 %)"), "{report}");
        assert!(report.contains("max_timeout     1 (25 %)"), "{report}");
        assert!(report.contains("P50 400 ms · P90 1000 ms"), "{report}");
        assert!(
            report.contains("Pausas intra-turno: 15 (retenidas 12)"),
            "{report}"
        );
        // P90 1000 ≥ 0,8 · 1200 → sugiere subir silence_ms.
        assert!(report.contains("considera subirlo"), "{report}");
        assert!(report.contains("max_turn_ms."), "{report}");
    }

    #[test]
    fn report_without_turns_has_no_percentiles() {
        let endpoint = VoiceEndpointConfig::default();
        let pauses = TurnPauseStats::new().summary();
        let report = format_report(
            &endpoint,
            Duration::from_secs(5),
            &CloseCounts::default(),
            &pauses,
            0,
        );
        assert!(report.contains("P50 — · P90 —"), "{report}");
        assert!(report.contains("Sin turnos"), "{report}");
    }

    #[test]
    fn hints_stay_quiet_with_healthy_metrics() {
        let endpoint = VoiceEndpointConfig::default();
        let closes = CloseCounts {
            silence: 5,
            ..CloseCounts::default()
        };
        let pauses = TurnPauseSummary {
            samples: 5,
            p50_ms: Some(200),
            p90_ms: Some(500),
        };
        assert!(hints(&endpoint, &closes, &pauses).is_empty());
    }
}
