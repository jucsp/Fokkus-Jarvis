//! Configuración de usuario del asistente, persistida en TOML.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fullscreen::FullscreenBehavior;
use crate::paths;

/// Comando CLI por defecto del cerebro (Antigravity, como en Fokkus IDE).
///
/// `--effort low` prioriza la latencia sobre la profundidad de razonamiento:
/// es el modo conversacional de baja latencia para el flujo de voz.
pub const DEFAULT_CLI_COMMAND: &str = "agy --effort low --dangerously-skip-permissions -p";

/// Tamaño por defecto de la esfera, en píxeles lógicos.
///
/// Constante canónica compartida por `jarvis-render` y `jarvis-shell` para
/// evitar una dependencia circular entre crates.
pub const DEFAULT_SPHERE_SIZE_PX: u32 = 220;

/// Rango válido de escala de la esfera, en porcentaje del tamaño base.
pub const SPHERE_SCALE_RANGE: std::ops::RangeInclusive<u16> = 50..=200;

/// Rango válido de opacidad de la esfera, en porcentaje.
pub const SPHERE_OPACITY_RANGE: std::ops::RangeInclusive<u8> = 20..=100;

/// Rango válido de opacidad de la esfera bajo detección de pantalla completa
/// (HU-35), en porcentaje. Compartido por `fullscreen_dim_pct` y
/// `fullscreen_active_pct`.
pub const FULLSCREEN_PCT_RANGE: std::ops::RangeInclusive<u8> = 5..=100;

/// Lado de la esfera en píxeles lógicos para un porcentaje de escala,
/// saturado a [`SPHERE_SCALE_RANGE`] (lo usa también la vista previa en vivo).
pub fn sphere_size_for_pct(pct: u16) -> u32 {
    let pct = pct.clamp(*SPHERE_SCALE_RANGE.start(), *SPHERE_SCALE_RANGE.end());
    DEFAULT_SPHERE_SIZE_PX * u32::from(pct) / 100
}

/// Opacidad de la esfera, en escala 0–255 (0 transparente, 255 opaco).
///
/// Se compone de forma multiplicativa para combinar la opacidad del usuario
/// (HU-34) con la atenuación automática por pantalla completa (HU-35).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Opacity(u8);

impl Opacity {
    /// Opacidad completa (100 %): la ruta de render no aplica ningún factor.
    pub const OPAQUE: Opacity = Opacity(255);

    /// Construye una opacidad desde su valor crudo 0–255, sin saturar al
    /// piso de usuario (lo usan los fundidos y la atenuación automática).
    pub const fn from_raw(val: u8) -> Self {
        Self(val)
    }

    /// Convierte un porcentaje (20–100) a opacidad 0–255, saturando al rango.
    ///
    /// `from_pct(0)` → 20 % (51/255) y `from_pct(200)` → 100 % (255/255).
    pub fn from_pct(pct: u8) -> Self {
        let pct = pct.clamp(*SPHERE_OPACITY_RANGE.start(), *SPHERE_OPACITY_RANGE.end());
        // `(pct * 255 + 50) / 100` redondea 50 % → 128 en vez de truncar a 127.
        let value = (u32::from(pct) * 255 + 50) / 100;
        Self(value as u8)
    }

    /// Valor crudo 0–255.
    pub fn as_u8(self) -> u8 {
        self.0
    }

    /// Composición multiplicativa (usuario × atenuación automática).
    #[allow(clippy::should_implement_trait)] // nombre fijado por el contrato SDD HU-34
    pub fn mul(self, other: Opacity) -> Opacity {
        // `(a * b + 127) / 255` mantiene `c ≤ a` en premultiplicado para todo `a`.
        let value = (u32::from(self.0) * u32::from(other.0) + 127) / 255;
        Self(value as u8)
    }

    /// `true` si la opacidad es completa (se salta la multiplicación).
    pub fn is_opaque(self) -> bool {
        self.0 == Self::OPAQUE.0
    }
}

/// Origen de las respuestas de Jarvis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrainMode {
    /// CLI local (`cli_command`), sin API key.
    #[default]
    Cli,
    /// API de Mensajes de Anthropic (key en el llavero del sistema).
    Api,
}

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
    /// Escucha continua global. Apagada por defecto: en reposo Jarvis solo
    /// reacciona a la palabra de activación "Jarvis" y no transcribe ni
    /// responde a la conversación de oficina, llamadas o charla familiar.
    pub continuous_listening: bool,
    /// Interrupción por voz mientras Jarvis habla. Apagada por defecto: sin
    /// audífonos el eco de los altavoces lo cortaría a sí mismo.
    pub barge_in_enabled: bool,
    /// Cerebro: CLI local o API.
    pub brain_mode: BrainMode,
    /// Comando del cerebro CLI; el prompt va como último argumento.
    pub cli_command: String,
    /// Base URL de la API (vacía = oficial de Anthropic). La API key nunca
    /// se guarda aquí: vive en el llavero del sistema.
    pub api_base_url: String,
    /// Modelo de la API (vacío = el por defecto del cliente).
    pub api_model: String,
    /// Tamaño de la esfera en % del tamaño base (50–200). Entero para conservar `Eq`.
    #[serde(rename = "sphere_scale_pct", default = "default_sphere_scale_pct")]
    sphere_scale_pct: u16,
    /// Opacidad de la esfera en % (20–100). Entero para conservar `Eq`.
    #[serde(rename = "sphere_opacity_pct", default = "default_sphere_opacity_pct")]
    sphere_opacity_pct: u8,
    /// Comportamiento al detectar una ventana a pantalla completa (HU-35).
    fullscreen_behavior: FullscreenBehavior,
    /// Opacidad de la esfera atenuada por pantalla completa (5–100, default 25).
    #[serde(default = "default_fullscreen_dim_pct")]
    fullscreen_dim_pct: u8,
    /// Opacidad de la esfera durante un turno activo estando atenuada
    /// (5–100, default 60).
    #[serde(default = "default_fullscreen_active_pct")]
    fullscreen_active_pct: u8,
}

fn default_sphere_scale_pct() -> u16 {
    100
}

fn default_sphere_opacity_pct() -> u8 {
    100
}

fn default_fullscreen_dim_pct() -> u8 {
    25
}

fn default_fullscreen_active_pct() -> u8 {
    60
}

impl Default for JarvisConfig {
    fn default() -> Self {
        Self {
            voice_id: "es-CL-LorenzoNeural".to_string(),
            voice_lang: "es-CL".to_string(),
            gaming_mode: false,
            continuous_listening: false,
            barge_in_enabled: false,
            brain_mode: BrainMode::Cli,
            cli_command: DEFAULT_CLI_COMMAND.to_string(),
            api_base_url: String::new(),
            api_model: String::new(),
            sphere_scale_pct: 100,
            sphere_opacity_pct: 100,
            fullscreen_behavior: FullscreenBehavior::Dim,
            fullscreen_dim_pct: 25,
            fullscreen_active_pct: 60,
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
            ..Self::default()
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

    pub fn with_barge_in(mut self, enabled: bool) -> Self {
        self.barge_in_enabled = enabled;
        self
    }

    pub fn with_brain_mode(mut self, mode: BrainMode) -> Self {
        self.brain_mode = mode;
        self
    }

    pub fn with_cli_command(mut self, command: impl Into<String>) -> Self {
        self.cli_command = command.into();
        self
    }

    pub fn with_fullscreen_behavior(mut self, behavior: FullscreenBehavior) -> Self {
        self.fullscreen_behavior = behavior;
        self
    }

    /// Comando CLI efectivo (el por defecto si quedó vacío).
    pub fn effective_cli_command(&self) -> &str {
        let command = self.cli_command.trim();
        if command.is_empty() {
            DEFAULT_CLI_COMMAND
        } else {
            command
        }
    }

    /// Porcentaje de escala de la esfera, saturado al rango válido (50–200).
    pub fn sphere_scale_pct(&self) -> u16 {
        self.sphere_scale_pct
            .clamp(*SPHERE_SCALE_RANGE.start(), *SPHERE_SCALE_RANGE.end())
    }

    /// Lado de la superficie de la esfera en píxeles lógicos.
    pub fn sphere_size_px(&self) -> u32 {
        sphere_size_for_pct(self.sphere_scale_pct)
    }

    /// Porcentaje de opacidad de la esfera, saturado al rango válido (20–100).
    pub fn sphere_opacity_pct(&self) -> u8 {
        self.sphere_opacity_pct
            .clamp(*SPHERE_OPACITY_RANGE.start(), *SPHERE_OPACITY_RANGE.end())
    }

    /// Opacidad efectiva de la esfera (20–100 % → 51–255).
    pub fn sphere_opacity(&self) -> Opacity {
        Opacity::from_pct(self.sphere_opacity_pct)
    }

    /// Comportamiento al detectar pantalla completa (HU-35).
    pub fn fullscreen_behavior(&self) -> FullscreenBehavior {
        self.fullscreen_behavior
    }

    /// Opacidad de la esfera atenuada por pantalla completa, saturada a 5–100.
    pub fn fullscreen_dim_pct(&self) -> u8 {
        self.fullscreen_dim_pct
            .clamp(*FULLSCREEN_PCT_RANGE.start(), *FULLSCREEN_PCT_RANGE.end())
    }

    /// Opacidad de la esfera durante un turno activo estando atenuada,
    /// saturada a 5–100.
    pub fn fullscreen_active_pct(&self) -> u8 {
        self.fullscreen_active_pct
            .clamp(*FULLSCREEN_PCT_RANGE.start(), *FULLSCREEN_PCT_RANGE.end())
    }

    /// Satura los valores fuera de rango al cargar (no es error de parseo).
    fn saturate(&mut self) {
        let scale = self.sphere_scale_pct();
        if scale != self.sphere_scale_pct {
            eprintln!(
                "[config] sphere_scale_pct fuera de rango ({}): se satura a {scale}",
                self.sphere_scale_pct
            );
            self.sphere_scale_pct = scale;
        }
        let opacity = self
            .sphere_opacity_pct
            .clamp(*SPHERE_OPACITY_RANGE.start(), *SPHERE_OPACITY_RANGE.end());
        if opacity != self.sphere_opacity_pct {
            eprintln!(
                "[config] sphere_opacity_pct fuera de rango ({}): se satura a {opacity}",
                self.sphere_opacity_pct
            );
            self.sphere_opacity_pct = opacity;
        }

        let dim = self.fullscreen_dim_pct();
        if dim != self.fullscreen_dim_pct {
            eprintln!(
                "[config] fullscreen_dim_pct fuera de rango ({}): se satura a {dim}",
                self.fullscreen_dim_pct
            );
            self.fullscreen_dim_pct = dim;
        }
        let active = self.fullscreen_active_pct();
        if active != self.fullscreen_active_pct {
            eprintln!(
                "[config] fullscreen_active_pct fuera de rango ({}): se satura a {active}",
                self.fullscreen_active_pct
            );
            self.fullscreen_active_pct = active;
        }
    }

    /// Carga la configuración desde la ruta por defecto.
    ///
    /// Si el archivo no existe, devuelve [`JarvisConfig::default`].
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&paths::config_file())
    }

    /// Ruta por defecto del archivo (`~/.config/fokkus-jarvis/config.toml`).
    pub fn path() -> PathBuf {
        paths::config_file()
    }

    /// Escribe los valores por defecto en `path` si el archivo no existe, para
    /// que el usuario (o el formulario) parta de una configuración explícita.
    /// Devuelve `true` si lo creó.
    pub fn ensure_file(path: &Path) -> Result<bool, ConfigError> {
        if path.exists() {
            return Ok(false);
        }
        Self::default().save_to(path)?;
        Ok(true)
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
        let mut config: JarvisConfig =
            toml::from_str(&content).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        config.saturate();
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
            .with_continuous_listening(true)
            .with_barge_in(true)
            .with_brain_mode(BrainMode::Api)
            .with_cli_command("agy -p --model opus");
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
        assert!(!loaded.barge_in_enabled);
        assert_eq!(loaded.brain_mode, BrainMode::Cli);
        assert_eq!(loaded.cli_command, DEFAULT_CLI_COMMAND);
        cleanup(&path);
    }

    #[test]
    fn ensure_file_writes_defaults_once() {
        let path = temp_config_path("ensure");
        cleanup(&path);
        assert!(JarvisConfig::ensure_file(&path).expect("crea"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written
                .contains("cli_command = \"agy --effort low --dangerously-skip-permissions -p\""),
            "{written}"
        );
        assert!(written.contains("barge_in_enabled = false"), "{written}");
        std::fs::write(&path, "gaming_mode = true\n").unwrap();
        assert!(!JarvisConfig::ensure_file(&path).expect("existe"));
        assert!(JarvisConfig::load_from(&path).unwrap().gaming_mode);
        cleanup(&path);
    }

    #[test]
    fn defaults_are_cli_agy_without_barge_in() {
        let config = JarvisConfig::default();
        assert_eq!(config.brain_mode, BrainMode::Cli);
        assert_eq!(
            config.cli_command,
            "agy --effort low --dangerously-skip-permissions -p"
        );
        assert!(!config.barge_in_enabled);
        assert!(!config.continuous_listening);
    }

    #[test]
    fn brain_mode_is_lowercase_in_toml_and_empty_command_falls_back() {
        let toml = toml::to_string(&JarvisConfig::default().with_brain_mode(BrainMode::Api))
            .expect("serializa");
        assert!(toml.contains("brain_mode = \"api\""), "{toml}");
        let config: JarvisConfig =
            toml::from_str("brain_mode = \"cli\"\ncli_command = \"  \"\n").expect("parsea");
        assert_eq!(config.effective_cli_command(), DEFAULT_CLI_COMMAND);
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

    #[test]
    fn opacity_from_pct_saturates_and_rounds() {
        assert_eq!(Opacity::from_pct(0), Opacity(51)); // mínimo 20 %
        assert_eq!(Opacity::from_pct(20), Opacity(51));
        assert_eq!(Opacity::from_pct(50), Opacity(128));
        assert_eq!(Opacity::from_pct(100), Opacity(255));
        assert_eq!(Opacity::from_pct(200), Opacity(255)); // satura a 100 %
        assert!(Opacity::OPAQUE.is_opaque());
        assert!(!Opacity::from_pct(50).is_opaque());
        assert_eq!(Opacity::OPAQUE.as_u8(), 255);
    }

    #[test]
    fn opacity_mul_is_commutative_and_bounded() {
        let a = Opacity::from_pct(70);
        let b = Opacity::from_pct(30);
        assert_eq!(a.mul(b), b.mul(a));
        assert_eq!(Opacity::OPAQUE.mul(a), a);
        assert_eq!(a.mul(Opacity::OPAQUE), a);
        // El resultado nunca supera el factor menor.
        assert!(a.mul(b).as_u8() <= a.as_u8());
        assert!(a.mul(b).as_u8() <= b.as_u8());
    }

    #[test]
    fn sphere_size_px_scales_from_base() {
        let mut config = JarvisConfig {
            sphere_scale_pct: 50,
            ..JarvisConfig::default()
        };
        assert_eq!(config.sphere_scale_pct(), 50);
        assert_eq!(config.sphere_size_px(), 110);
        config.sphere_scale_pct = 100;
        assert_eq!(config.sphere_size_px(), 220);
        config.sphere_scale_pct = 200;
        assert_eq!(config.sphere_size_px(), 440);
    }

    #[test]
    fn out_of_range_scale_saturates_on_load() {
        let path = temp_config_path("scale-sat");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "sphere_scale_pct = 201\n").unwrap();
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.sphere_scale_pct(), 200);
        cleanup(&path);

        let path2 = temp_config_path("scale-sat2");
        cleanup(&path2);
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(&path2, "sphere_scale_pct = 49\n").unwrap();
        let loaded2 = JarvisConfig::load_from(&path2).unwrap();
        assert_eq!(loaded2.sphere_scale_pct(), 50);
        cleanup(&path2);
    }

    #[test]
    fn out_of_range_opacity_saturates_on_load() {
        let path = temp_config_path("opacity-sat");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "sphere_opacity_pct = 0\n").unwrap();
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.sphere_opacity(), Opacity::from_pct(20));
        cleanup(&path);
    }

    #[test]
    fn sphere_fields_roundtrip_through_toml() {
        let path = temp_config_path("sphere-roundtrip");
        cleanup(&path);
        let config = JarvisConfig {
            sphere_scale_pct: 135,
            sphere_opacity_pct: 60,
            ..JarvisConfig::default()
        };
        config.save_to(&path).unwrap();
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(toml.contains("sphere_scale_pct = 135"), "{toml}");
        assert!(toml.contains("sphere_opacity_pct = 60"), "{toml}");
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.sphere_scale_pct(), 135);
        assert_eq!(loaded.sphere_opacity(), Opacity::from_pct(60));
        cleanup(&path);
    }

    #[test]
    fn partial_toml_defaults_sphere_to_100() {
        let path = temp_config_path("sphere-partial");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "voice_id = \"es-MX-DaliaNeural\"\n").unwrap();
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.sphere_scale_pct(), 100);
        assert_eq!(loaded.sphere_size_px(), 220);
        assert_eq!(loaded.sphere_opacity(), Opacity::OPAQUE);
        cleanup(&path);
    }

    #[test]
    fn fullscreen_defaults_and_builder() {
        let config = JarvisConfig::default();
        assert_eq!(config.fullscreen_behavior(), FullscreenBehavior::Dim);
        assert_eq!(config.fullscreen_dim_pct(), 25);
        assert_eq!(config.fullscreen_active_pct(), 60);

        let config = config.with_fullscreen_behavior(FullscreenBehavior::Hide);
        assert_eq!(config.fullscreen_behavior(), FullscreenBehavior::Hide);
    }

    #[test]
    fn fullscreen_fields_roundtrip_through_toml() {
        let path = temp_config_path("fullscreen-roundtrip");
        cleanup(&path);
        let config = JarvisConfig {
            fullscreen_behavior: FullscreenBehavior::Hide,
            fullscreen_dim_pct: 40,
            fullscreen_active_pct: 80,
            ..JarvisConfig::default()
        };
        config.save_to(&path).unwrap();
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(toml.contains("fullscreen_behavior = \"hide\""), "{toml}");
        assert!(toml.contains("fullscreen_dim_pct = 40"), "{toml}");
        assert!(toml.contains("fullscreen_active_pct = 80"), "{toml}");

        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(loaded.fullscreen_behavior(), FullscreenBehavior::Hide);
        assert_eq!(loaded.fullscreen_dim_pct(), 40);
        assert_eq!(loaded.fullscreen_active_pct(), 80);
        cleanup(&path);
    }

    #[test]
    fn out_of_range_fullscreen_pct_saturates_on_load() {
        let path = temp_config_path("fullscreen-sat");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "fullscreen_dim_pct = 0\nfullscreen_active_pct = 250\n",
        )
        .unwrap();
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.fullscreen_dim_pct(), 5);
        assert_eq!(loaded.fullscreen_active_pct(), 100);
        cleanup(&path);
    }

    #[test]
    fn fullscreen_behavior_serializes_lowercase_by_default() {
        let toml = toml::to_string(&JarvisConfig::default()).expect("serializa");
        assert!(toml.contains("fullscreen_behavior = \"dim\""), "{toml}");
    }
}
