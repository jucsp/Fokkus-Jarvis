//! Motor de transcripción local con Whisper.
//!
//! Invoca el binario `whisper-cli` (o `whisper-cpp`) de whisper.cpp contra un
//! archivo WAV PCM 16 kHz y devuelve el texto. La carga perezosa del modelo y
//! la descarga por inactividad se abordan en la tarea 2.3; aquí se define el
//! contrato de invocación y el parseo de salida.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::tts::find_executable_in_path;

/// Nombres de binario de Whisper a sondear en `PATH`.
pub const WHISPER_BINARY_NAMES: &[&str] = &["whisper-cli", "whisper-cpp"];

/// Nombre del modelo por defecto (cuantizado, ~60 MB).
pub const DEFAULT_MODEL_FILE: &str = "ggml-base-q5_1.bin";

/// Error de transcripción.
#[derive(Debug)]
pub enum SttError {
    NoBinary,
    NoModel,
    MissingWav(PathBuf),
    Command { source: std::io::Error },
    CommandFailed { status: std::process::ExitStatus },
}

impl fmt::Display for SttError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SttError::NoBinary => {
                write!(
                    f,
                    "no se encontró el binario de Whisper (whisper-cli/whisper-cpp)"
                )
            }
            SttError::NoModel => write!(f, "no hay modelo de Whisper configurado"),
            SttError::MissingWav(path) => {
                write!(f, "el archivo WAV no existe: {}", path.display())
            }
            SttError::Command { source } => write!(f, "no se pudo ejecutar Whisper: {source}"),
            SttError::CommandFailed { status } => write!(f, "Whisper terminó con {status}"),
        }
    }
}

impl std::error::Error for SttError {}

/// Motor de transcripción Whisper.
pub struct WhisperEngine {
    binary: Option<PathBuf>,
    model: Option<PathBuf>,
    language: String,
}

impl WhisperEngine {
    pub fn new() -> Self {
        Self {
            binary: resolve_whisper_binary(),
            model: default_model_path(),
            language: "es".to_string(),
        }
    }

    pub fn with_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.binary = Some(path.into());
        self
    }

    pub fn with_model(mut self, path: impl Into<PathBuf>) -> Self {
        self.model = Some(path.into());
        self
    }

    pub fn with_language(mut self, lang: impl Into<String>) -> Self {
        self.language = lang.into();
        self
    }

    pub fn binary(&self) -> Option<&Path> {
        self.binary.as_deref()
    }

    pub fn model(&self) -> Option<&Path> {
        self.model.as_deref()
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    /// Transcribe un WAV PCM 16 kHz a texto.
    pub fn transcribe(&self, wav: &Path) -> Result<String, SttError> {
        let binary = self.binary.as_ref().ok_or(SttError::NoBinary)?;
        let model = self.model.as_ref().ok_or(SttError::NoModel)?;
        if !wav.is_file() {
            return Err(SttError::MissingWav(wav.to_path_buf()));
        }

        let output = Command::new(binary)
            .arg("-m")
            .arg(model)
            .arg("-f")
            .arg(wav)
            .arg("-l")
            .arg(&self.language)
            .arg("-nt")
            .output()
            .map_err(|source| SttError::Command { source })?;

        if !output.status.success() {
            return Err(SttError::CommandFailed {
                status: output.status,
            });
        }

        Ok(parse_whisper_output(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
}

impl Default for WhisperEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Resuelve el binario de Whisper en `PATH`.
pub fn resolve_whisper_binary() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?.into_string().ok()?;
    WHISPER_BINARY_NAMES
        .iter()
        .find_map(|name| find_executable_in_path(name, &path_var))
}

/// Ruta del modelo por defecto
/// (`~/.cache/fokkus-jarvis/models/ggml-base-q5_1.bin`).
pub fn default_model_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WHISPER_MODEL") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return Some(
                PathBuf::from(xdg)
                    .join("fokkus-jarvis")
                    .join("models")
                    .join(DEFAULT_MODEL_FILE),
            );
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return Some(
                PathBuf::from(home)
                    .join(".cache")
                    .join("fokkus-jarvis")
                    .join("models")
                    .join(DEFAULT_MODEL_FILE),
            );
        }
    }
    None
}

/// Parsea la salida de `whisper-cli` a una sola línea de texto.
pub fn parse_whisper_output(raw: &str) -> String {
    let mut words: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("whisper_") || line.contains("-->") {
            continue;
        }
        words.extend(line.split_whitespace());
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_whisper_output_joins_and_trims() {
        let raw = "  hola   mundo \n\nsegunda línea\n";
        assert_eq!(parse_whisper_output(raw), "hola mundo segunda línea");
    }

    #[test]
    fn parse_whisper_output_skips_timestamps_and_logs() {
        let raw =
            "[00:00:00.000 --> 00:00:02.000] hola mundo\nwhisper_model_load: ok\nsegunda línea\n";
        assert_eq!(parse_whisper_output(raw), "segunda línea");
    }

    #[test]
    fn transcribe_errors_without_binary() {
        let engine = WhisperEngine {
            binary: None,
            model: Some(PathBuf::from("/tmp/modelo.bin")),
            language: "es".to_string(),
        };
        assert!(matches!(
            engine.transcribe(Path::new("/tmp/x.wav")),
            Err(SttError::NoBinary)
        ));
    }

    #[test]
    fn transcribe_errors_without_model() {
        let engine = WhisperEngine {
            binary: Some(PathBuf::from("/tmp/whisper-cli")),
            model: None,
            language: "es".to_string(),
        };
        assert!(matches!(
            engine.transcribe(Path::new("/tmp/x.wav")),
            Err(SttError::NoModel)
        ));
    }

    #[test]
    fn transcribe_errors_when_wav_missing() {
        let engine = WhisperEngine {
            binary: Some(PathBuf::from("/tmp/whisper-cli")),
            model: Some(PathBuf::from("/tmp/modelo.bin")),
            language: "es".to_string(),
        };
        assert!(matches!(
            engine.transcribe(Path::new("/tmp/no-existe.wav")),
            Err(SttError::MissingWav(_))
        ));
    }

    #[test]
    #[cfg(unix)]
    fn transcribe_invokes_fake_whisper_binary() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("jarvis-stt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let bin = dir.join("whisper-cli");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho \"hola mundo\"\necho \"segunda línea\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let wav = dir.join("input.wav");
        std::fs::write(&wav, b"RIFF....").unwrap();

        let engine = WhisperEngine::new()
            .with_binary(bin.clone())
            .with_model("/tmp/modelo-falso.bin");
        let text = engine.transcribe(&wav).expect("transcripción con fake");
        assert_eq!(text, "hola mundo segunda línea");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
