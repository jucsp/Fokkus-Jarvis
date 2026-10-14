//! Configuración de usuario del asistente, persistida en TOML.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

/// Configuración persistente de Jarvis.
///
/// `#[serde(default)]` hace que un archivo TOML parcial (p. ej. que solo
/// defina `voice_id`) se complete con los valores por defecto del resto.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JarvisConfig {
    /// Voz neuronal por defecto.
    pub voice_id: String,
    /// Idioma/código de la voz.
    pub voice_lang: String,
    /// Modo juego: silencia notificaciones verbales y reduce interrupciones.
    pub gaming_mode: bool,
    /// Escucha continua (micrófono siempre abierto) vs. wake-word/atajo.
    pub continuous_listening: bool,
}

impl Default for JarvisConfig {
    fn default() -> Self {
        Self {
            voice_id: "es-CL-LorenzoNeural".to_string(),
            voice_lang: "es-CL".to_string(),
            gaming_mode: false,
            continuous_listening: false,
        }
    }
}

/// Error al cargar o guardar la configuración.
#[derive(Debug)]
pub enum ConfigError {
    /// No se pudo leer el archivo.
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// El contenido no es TOML válido.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// No se pudo serializar la configuración.
    Serialize(toml::ser::Error),
    /// No se pudo crear el directorio o escribir el archivo.
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "no se pudo leer {}: {source}", path.display())
            }
            ConfigError::Parse { path, source } => {
                write!(f, "TOML inválido en {}: {source}", path.display())
            }
            ConfigError::Serialize(e) => write!(f, "no se pudo serializar la configuración: {e}"),
            ConfigError::Write { path, source } => {
                write!(f, "no se pudo escribir {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl JarvisConfig {
    /// Construye una configuración con una voz explícita y el resto en defaults.
    pub fn new(voice_id: impl Into<String>, voice_lang: impl Into<String>) -> Self {
        Self {
            voice_id: voice_id.into(),
            voice_lang: voice_lang.into(),
            gaming_mode: false,
            continuous_listening: false,
        }
    }

    pub fn with_gaming_mode(mut self, enabled: bool) -> Self {
        self.gaming_mode = enabled;
        self
    }

    pub fn with_continuous_listening(mut self, enabled: bool) -> Self {
        self.continuous_listening = enabled;
        self
    }

    /// Carga la configuración desde la ruta por defecto.
    ///
    /// Si el archivo no existe, devuelve [`JarvisConfig::default`].
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&paths::config_file())
    }

    /// Guarda la configuración en la ruta por defecto, creando directorios.
    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&paths::config_file())
    }

    /// Carga la configuración desde una ruta explícita (útil en tests).
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: JarvisConfig =
            toml::from_str(&content).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(config)
    }

    /// Guarda la configuración en una ruta explícita, creando directorios.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let content = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        std::fs::write(path, content).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("jarvis-config-test-{name}-{}", std::process::id()))
            .join("config.toml")
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn load_missing_file_returns_default() {
        let path = temp_config_path("missing");
        let config = JarvisConfig::load_from(&path).expect("missing => default");
        assert_eq!(config, JarvisConfig::default());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let path = temp_config_path("roundtrip");
        cleanup(&path);
        let config = JarvisConfig::new("es-ES-ElviraNeural", "es-ES")
            .with_gaming_mode(true)
            .with_continuous_listening(true);
        config.save_to(&path).expect("guardar");
        assert!(path.exists());
        let loaded = JarvisConfig::load_from(&path).expect("cargar");
        assert_eq!(loaded, config);
        cleanup(&path);
    }

    #[test]
    fn save_creates_missing_directories() {
        let path = temp_config_path("nested")
            .join("a")
            .join("b")
            .join("config.toml");
        JarvisConfig::default()
            .save_to(&path)
            .expect("crea directorios");
        assert!(path.exists());
        cleanup(&path);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let path = temp_config_path("partial");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "voice_id = \"es-MX-DaliaNeural\"\n").unwrap();
        let loaded = JarvisConfig::load_from(&path).expect("TOML parcial");
        assert_eq!(loaded.voice_id, "es-MX-DaliaNeural");
        assert_eq!(loaded.voice_lang, JarvisConfig::default().voice_lang);
        assert!(!loaded.gaming_mode);
        assert!(!loaded.continuous_listening);
        cleanup(&path);
    }

    #[test]
    fn invalid_toml_returns_parse_error() {
        let path = temp_config_path("invalid");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "esto no es [TOML\n").unwrap();
        let result = JarvisConfig::load_from(&path);
        assert!(matches!(result, Err(ConfigError::Parse { .. })));
        cleanup(&path);
    }
}
