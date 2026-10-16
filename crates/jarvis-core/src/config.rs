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

/// Número máximo de burbujas simultáneas del HUD (HU-36).
pub const HUD_MAX_BUBBLES_RANGE: std::ops::RangeInclusive<u8> = 1..=3;

/// Tareas pesadas simultáneas permitidas (HU-37).
pub const TASKS_PARALLEL_RANGE: std::ops::RangeInclusive<u8> = 1..=4;

/// Timeout por tarea pesada, en segundos (HU-37).
pub const TASKS_TIMEOUT_RANGE: std::ops::RangeInclusive<u32> = 60..=3_600;

/// Hueco de silencio para entregar resultados de tareas, en milisegundos (HU-37).
pub const TASKS_QUIET_GAP_RANGE: std::ops::RangeInclusive<u32> = 500..=5_000;

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
    /// Tareas pesadas simultáneas (1–4, default 2).
    #[serde(default = "default_tasks_max_parallel")]
    tasks_max_parallel: u8,
    /// Timeout por tarea pesada en segundos (60–3600, default 900).
    #[serde(default = "default_tasks_timeout_secs")]
    tasks_timeout_secs: u32,
    /// Hueco de silencio en ms antes de entregar un resultado (500–5000,
    /// default 1500).
    #[serde(default = "default_tasks_quiet_gap_ms")]
    tasks_quiet_gap_ms: u32,
    /// Mostrar burbujas de conversación efímeras (HUD) sobre la esfera (HU-36).
    #[serde(default = "default_hud_enabled")]
    hud_enabled: bool,
    /// Máximo de burbujas simultáneas del HUD (1–3, default 3).
    #[serde(default = "default_hud_max_bubbles")]
    hud_max_bubbles: u8,
    /// Automatización de escritorio (HU-38, tabla `[automation]`). Va al final:
    /// en TOML las tablas se emiten después de los valores simples.
    pub automation: AutomationConfig,
}

/// Configuración de la capa "ojos y manos" (HU-38). Apagada por defecto:
/// ninguna captura ni acción de entrada ocurre sin `enabled = true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomationConfig {
    /// Interruptor general de la automatización.
    pub enabled: bool,
    /// Permite el respaldo `ydotool` (requiere `ydotoold` y `/dev/uinput`).
    pub allow_uinput: bool,
    /// Tope del *token bucket* de acciones de entrada por segundo.
    pub max_actions_per_sec: u32,
    /// Duración de un *lease* de automatización en segundos.
    pub lease_secs: u64,
    /// Aplicaciones (id o fragmento del nombre) que nunca se capturan ni
    /// controlan, además de la lista fija (gestores de contraseñas, polkit…).
    pub excluded_apps: Vec<String>,
    /// Registrar el texto tecleado en la auditoría. Con `false` solo se guarda
    /// su longitud y su hash SHA-256.
    pub audit_log_text: bool,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_uinput: false,
            max_actions_per_sec: 10,
            lease_secs: 120,
            excluded_apps: Vec::new(),
            audit_log_text: false,
        }
    }
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

fn default_tasks_max_parallel() -> u8 {
    2
}

fn default_tasks_timeout_secs() -> u32 {
    900
}

fn default_tasks_quiet_gap_ms() -> u32 {
    1_500
}

fn default_hud_enabled() -> bool {
    true
}

fn default_hud_max_bubbles() -> u8 {
    3
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
            tasks_max_parallel: 2,
            tasks_timeout_secs: 900,
            tasks_quiet_gap_ms: 1_500,
            hud_enabled: true,
            hud_max_bubbles: 3,
            automation: AutomationConfig::default(),
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

    pub fn with_tasks_max_parallel(mut self, value: u8) -> Self {
        self.tasks_max_parallel = value;
        self
    }

    pub fn with_tasks_timeout_secs(mut self, value: u32) -> Self {
        self.tasks_timeout_secs = value;
        self
    }

    pub fn with_tasks_quiet_gap_ms(mut self, value: u32) -> Self {
        self.tasks_quiet_gap_ms = value;
        self
    }

    pub fn with_hud_enabled(mut self, enabled: bool) -> Self {
        self.hud_enabled = enabled;
        self
    }

    pub fn with_hud_max_bubbles(mut self, value: u8) -> Self {
        self.hud_max_bubbles = value;
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

    /// Tareas pesadas simultáneas, saturado a 1–4.
    pub fn tasks_max_parallel(&self) -> u8 {
        self.tasks_max_parallel
            .clamp(*TASKS_PARALLEL_RANGE.start(), *TASKS_PARALLEL_RANGE.end())
    }

    /// Timeout por tarea pesada en segundos, saturado a 60–3600.
    pub fn tasks_timeout_secs(&self) -> u32 {
        self.tasks_timeout_secs
            .clamp(*TASKS_TIMEOUT_RANGE.start(), *TASKS_TIMEOUT_RANGE.end())
    }

    /// Hueco de silencio en ms para entregar resultados, saturado a 500–5000.
    pub fn tasks_quiet_gap_ms(&self) -> u32 {
        self.tasks_quiet_gap_ms
            .clamp(*TASKS_QUIET_GAP_RANGE.start(), *TASKS_QUIET_GAP_RANGE.end())
    }

    /// `true` si el HUD de burbujas de conversación está habilitado (HU-36).
    pub fn hud_enabled(&self) -> bool {
        self.hud_enabled
    }

    /// Máximo de burbujas simultáneas del HUD, saturado a 1–3.
    pub fn hud_max_bubbles(&self) -> u8 {
        self.hud_max_bubbles
            .clamp(*HUD_MAX_BUBBLES_RANGE.start(), *HUD_MAX_BUBBLES_RANGE.end())
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

        let parallel = self.tasks_max_parallel();
        if parallel != self.tasks_max_parallel {
            eprintln!(
                "[config] tasks_max_parallel fuera de rango ({}): se satura a {parallel}",
                self.tasks_max_parallel
            );
            self.tasks_max_parallel = parallel;
        }
        let timeout = self.tasks_timeout_secs();
        if timeout != self.tasks_timeout_secs {
            eprintln!(
                "[config] tasks_timeout_secs fuera de rango ({}): se satura a {timeout}",
                self.tasks_timeout_secs
            );
            self.tasks_timeout_secs = timeout;
        }
        let quiet_gap = self.tasks_quiet_gap_ms();
        if quiet_gap != self.tasks_quiet_gap_ms {
            eprintln!(
                "[config] tasks_quiet_gap_ms fuera de rango ({}): se satura a {quiet_gap}",
                self.tasks_quiet_gap_ms
            );
            self.tasks_quiet_gap_ms = quiet_gap;
        }

        let hud_bubbles = self.hud_max_bubbles();
        if hud_bubbles != self.hud_max_bubbles {
            eprintln!(
                "[config] hud_max_bubbles fuera de rango ({}): se satura a {hud_bubbles}",
                self.hud_max_bubbles
            );
            self.hud_max_bubbles = hud_bubbles;
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
    fn automation_is_disabled_by_default() {
        let a = JarvisConfig::default().automation;
        assert!(!a.enabled);
        assert!(!a.allow_uinput);
        assert_eq!(a.max_actions_per_sec, 10);
        assert_eq!(a.lease_secs, 120);
        assert!(a.excluded_apps.is_empty());
        assert!(!a.audit_log_text);
    }

    #[test]
    fn automation_table_roundtrips_and_partial_table_fills_defaults() {
        let path = temp_config_path("automation-roundtrip");
        cleanup(&path);
        let mut config = JarvisConfig::default();
        config.automation.enabled = true;
        config.automation.excluded_apps = vec!["org.kde.kwalletmanager".into()];
        config.save_to(&path).unwrap();
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(toml.contains("[automation]"), "{toml}");
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.automation, config.automation);
        cleanup(&path);

        let path2 = temp_config_path("automation-partial");
        cleanup(&path2);
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(&path2, "voice_id = \"x\"\n[automation]\nenabled = true\n").unwrap();
        let loaded = JarvisConfig::load_from(&path2).unwrap();
        assert!(loaded.automation.enabled);
        assert_eq!(loaded.automation.max_actions_per_sec, 10);
        assert_eq!(loaded.automation.lease_secs, 120);
        cleanup(&path2);
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

    #[test]
    fn tasks_defaults_and_builders() {
        let config = JarvisConfig::default();
        assert_eq!(config.tasks_max_parallel(), 2);
        assert_eq!(config.tasks_timeout_secs(), 900);
        assert_eq!(config.tasks_quiet_gap_ms(), 1_500);

        let config = config
            .with_tasks_max_parallel(4)
            .with_tasks_timeout_secs(1_200)
            .with_tasks_quiet_gap_ms(2_000);
        assert_eq!(config.tasks_max_parallel(), 4);
        assert_eq!(config.tasks_timeout_secs(), 1_200);
        assert_eq!(config.tasks_quiet_gap_ms(), 2_000);
    }

    #[test]
    fn tasks_fields_roundtrip_through_toml() {
        let path = temp_config_path("tasks-roundtrip");
        cleanup(&path);
        let config = JarvisConfig {
            tasks_max_parallel: 3,
            tasks_timeout_secs: 600,
            tasks_quiet_gap_ms: 800,
            ..JarvisConfig::default()
        };
        config.save_to(&path).unwrap();
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(toml.contains("tasks_max_parallel = 3"), "{toml}");
        assert!(toml.contains("tasks_timeout_secs = 600"), "{toml}");
        assert!(toml.contains("tasks_quiet_gap_ms = 800"), "{toml}");

        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(loaded.tasks_max_parallel(), 3);
        assert_eq!(loaded.tasks_timeout_secs(), 600);
        assert_eq!(loaded.tasks_quiet_gap_ms(), 800);
        cleanup(&path);
    }

    #[test]
    fn out_of_range_tasks_values_saturate_on_load() {
        let path = temp_config_path("tasks-sat");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "tasks_max_parallel = 9\ntasks_timeout_secs = 1\ntasks_quiet_gap_ms = 9_999\n",
        )
        .unwrap();
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.tasks_max_parallel(), 4);
        assert_eq!(loaded.tasks_timeout_secs(), 60);
        assert_eq!(loaded.tasks_quiet_gap_ms(), 5_000);
        cleanup(&path);
    }

    #[test]
    fn hud_defaults_and_builders() {
        let config = JarvisConfig::default();
        assert!(config.hud_enabled());
        assert_eq!(config.hud_max_bubbles(), 3);

        let config = config.with_hud_enabled(false).with_hud_max_bubbles(1);
        assert!(!config.hud_enabled());
        assert_eq!(config.hud_max_bubbles(), 1);
    }

    #[test]
    fn hud_fields_roundtrip_through_toml() {
        let path = temp_config_path("hud-roundtrip");
        cleanup(&path);
        let config = JarvisConfig {
            hud_enabled: false,
            hud_max_bubbles: 2,
            ..JarvisConfig::default()
        };
        config.save_to(&path).unwrap();
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(toml.contains("hud_enabled = false"), "{toml}");
        assert!(toml.contains("hud_max_bubbles = 2"), "{toml}");

        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded, config);
        assert!(!loaded.hud_enabled());
        assert_eq!(loaded.hud_max_bubbles(), 2);
        cleanup(&path);
    }

    #[test]
    fn out_of_range_hud_max_bubbles_saturates_on_load() {
        let path = temp_config_path("hud-sat");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "hud_max_bubbles = 9\n").unwrap();
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.hud_max_bubbles(), 3);
        cleanup(&path);

        let path2 = temp_config_path("hud-sat2");
        cleanup(&path2);
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(&path2, "hud_max_bubbles = 0\n").unwrap();
        let loaded2 = JarvisConfig::load_from(&path2).unwrap();
        assert_eq!(loaded2.hud_max_bubbles(), 1);
        cleanup(&path2);
    }
}
