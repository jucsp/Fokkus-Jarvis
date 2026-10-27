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

/// Timeout del cerebro CLI síncrono, en segundos (Issue #7).
pub const CLI_TIMEOUT_RANGE: std::ops::RangeInclusive<u32> = 30..=1_800;

/// Hueco de silencio para entregar resultados de tareas, en milisegundos (HU-37).
pub const TASKS_QUIET_GAP_RANGE: std::ops::RangeInclusive<u32> = 500..=5_000;

/// Rango válido del umbral VAD que ABRE un turno (HU-41).
///
/// Por debajo de 0,1 el ruido estacionario abriría turnos; por encima de 0,95
/// casi ninguna voz real lo superaría.
pub const VOICE_OPEN_RANGE: std::ops::RangeInclusive<f32> = 0.1..=0.95;

/// Rango válido del umbral VAD que MANTIENE el turno abierto (HU-41).
pub const VOICE_HOLD_RANGE: std::ops::RangeInclusive<f32> = 0.05..=0.8;

/// Rango válido del silencio de cierre normal, en milisegundos (HU-41).
pub const SILENCE_MS_RANGE: std::ops::RangeInclusive<u32> = 300..=5_000;

/// Rango válido del silencio de cierre extendido por conector (HU-41).
pub const EXTENDED_SILENCE_MS_RANGE: std::ops::RangeInclusive<u32> = 500..=8_000;

/// Rango válido de la duración máxima de un segmento (HU-41, techo de HU-44).
pub const MAX_SEGMENT_MS_RANGE: std::ops::RangeInclusive<u32> = 5_000..=120_000;

/// Rango válido del techo total de un turno largo, en milisegundos (HU-44).
/// Nunca queda por debajo de `max_segment_ms` (ver
/// [`VoiceEndpointConfig::max_turn_ms`]).
pub const MAX_TURN_MS_RANGE: std::ops::RangeInclusive<u32> = 5_000..=120_000;

/// Rango válido de la espera extra tras un turno que termina en conector (HU-41).
pub const CONNECTOR_GRACE_MS_RANGE: std::ops::RangeInclusive<u32> = 100..=3_000;

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

/// Configuración del *endpointing* de turno (HU-41), tabla `[voice_endpoint]`.
///
/// Los seis valores son los que el DSP usa para decidir cuándo cierra el turno
/// y cuánto espera cuando la transcripción termina en conector. Cada getter
/// satura al rango documentado en sus constantes, así un TOML editado a mano
/// nunca deja el detector en un estado absurdo (p. ej. `silence_ms = 0`).
///
/// Se conserva `Copy` y `PartialEq` (sin `Eq`) porque los umbrales VAD son
/// `f32`; la comparación se hace por valor exacto, sin tolerancia.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceEndpointConfig {
    /// Puntaje VAD mínimo para ABRIR el turno (histéresis alta).
    #[serde(serialize_with = "serialize_f32_short")]
    pub voice_open: f32,
    /// Puntaje VAD mínimo para MANTENER el turno abierto (histéresis baja).
    #[serde(serialize_with = "serialize_f32_short")]
    pub voice_hold: f32,
    /// Silencio continuo que cierra el turno, en milisegundos.
    pub silence_ms: u32,
    /// Silencio de cierre cuando hay un conector pendiente, en milisegundos.
    pub extended_silence_ms: u32,
    /// Duración máxima de un segmento, en milisegundos. Desde HU-44 no cierra
    /// el turno: al alcanzarla se transcribe un fragmento parcial y se sigue.
    pub max_segment_ms: u32,
    /// Techo total de un turno largo (suma de fragmentos), en milisegundos
    /// (HU-44). Al alcanzarlo el turno sí se cierra.
    pub max_turn_ms: u32,
    /// Espera extra tras un turno que termina en conector, en milisegundos.
    pub connector_grace_ms: u32,
}

impl Default for VoiceEndpointConfig {
    fn default() -> Self {
        Self {
            voice_open: 0.45,
            voice_hold: 0.25,
            silence_ms: 1_200,
            extended_silence_ms: 2_000,
            max_segment_ms: 30_000,
            max_turn_ms: 120_000,
            connector_grace_ms: 500,
        }
    }
}

/// Serializa un `f32` con su representación decimal más corta: TOML solo
/// conoce `f64`, y el ensanchamiento directo escribiría `0.6000000238418579`
/// en el `config.toml` que edita el usuario.
fn serialize_f32_short<S: serde::Serializer>(
    value: &f32,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let short = value
        .to_string()
        .parse::<f64>()
        .unwrap_or(f64::from(*value));
    serializer.serialize_f64(short)
}

fn saturate_f32(value: f32, range: std::ops::RangeInclusive<f32>) -> f32 {
    // Un `f32::NAN` no es comparable: `clamp` lo propagaría y el detector
    // nunca abriría turno. Se cae al mínimo del rango.
    if value.is_nan() {
        return *range.start();
    }
    value.clamp(*range.start(), *range.end())
}

impl VoiceEndpointConfig {
    /// Umbral de apertura, saturado a [`VOICE_OPEN_RANGE`].
    pub fn voice_open(&self) -> f32 {
        saturate_f32(self.voice_open, VOICE_OPEN_RANGE)
    }

    /// Umbral de mantenimiento, saturado a [`VOICE_HOLD_RANGE`].
    pub fn voice_hold(&self) -> f32 {
        saturate_f32(self.voice_hold, VOICE_HOLD_RANGE)
    }

    /// Silencio de cierre normal en ms, saturado a [`SILENCE_MS_RANGE`].
    pub fn silence_ms(&self) -> u32 {
        self.silence_ms
            .clamp(*SILENCE_MS_RANGE.start(), *SILENCE_MS_RANGE.end())
    }

    /// Silencio de cierre extendido en ms, saturado a
    /// [`EXTENDED_SILENCE_MS_RANGE`].
    pub fn extended_silence_ms(&self) -> u32 {
        self.extended_silence_ms.clamp(
            *EXTENDED_SILENCE_MS_RANGE.start(),
            *EXTENDED_SILENCE_MS_RANGE.end(),
        )
    }

    /// Duración máxima del segmento en ms, saturada a [`MAX_SEGMENT_MS_RANGE`].
    pub fn max_segment_ms(&self) -> u32 {
        self.max_segment_ms
            .clamp(*MAX_SEGMENT_MS_RANGE.start(), *MAX_SEGMENT_MS_RANGE.end())
    }

    /// Techo total del turno en ms, saturado a [`MAX_TURN_MS_RANGE`] y nunca
    /// menor que [`max_segment_ms`](Self::max_segment_ms) (HU-44).
    pub fn max_turn_ms(&self) -> u32 {
        self.max_turn_ms
            .clamp(*MAX_TURN_MS_RANGE.start(), *MAX_TURN_MS_RANGE.end())
            .max(self.max_segment_ms())
    }

    /// Espera extra por conector en ms, saturada a
    /// [`CONNECTOR_GRACE_MS_RANGE`].
    pub fn connector_grace_ms(&self) -> u32 {
        self.connector_grace_ms.clamp(
            *CONNECTOR_GRACE_MS_RANGE.start(),
            *CONNECTOR_GRACE_MS_RANGE.end(),
        )
    }

    /// Copia con todos los valores saturados a su rango válido.
    pub fn saturated(&self) -> Self {
        Self {
            voice_open: self.voice_open(),
            voice_hold: self.voice_hold(),
            silence_ms: self.silence_ms(),
            extended_silence_ms: self.extended_silence_ms(),
            max_segment_ms: self.max_segment_ms(),
            max_turn_ms: self.max_turn_ms(),
            connector_grace_ms: self.connector_grace_ms(),
        }
    }
}

/// Configuración persistente de Jarvis.
///
/// `#[serde(default)]` hace que un archivo TOML parcial (p. ej. que solo
/// defina `voice_id`) se complete con los valores por defecto del resto.
///
/// No deriva `Eq`: la tabla `[voice_endpoint]` lleva umbrales `f32`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Timeout del cerebro CLI síncrono en segundos (30–1800, default 120).
    #[serde(default = "default_cli_timeout_secs")]
    cli_timeout_secs: u32,
    /// Mostrar burbujas de conversación efímeras (HUD) sobre la esfera (HU-36).
    #[serde(default = "default_hud_enabled")]
    hud_enabled: bool,
    /// Máximo de burbujas simultáneas del HUD (1–3, default 3).
    #[serde(default = "default_hud_max_bubbles")]
    hud_max_bubbles: u8,
    /// Automatización de escritorio (HU-38, tabla `[automation]`). Va al final:
    /// en TOML las tablas se emiten después de los valores simples.
    pub automation: AutomationConfig,
    /// *Endpointing* de turno (HU-41, tabla `[voice_endpoint]`). También al
    /// final, por la misma razón que `automation`.
    pub voice_endpoint: VoiceEndpointConfig,
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

fn default_cli_timeout_secs() -> u32 {
    120
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
            cli_timeout_secs: 120,
            hud_enabled: true,
            hud_max_bubbles: 3,
            automation: AutomationConfig::default(),
            voice_endpoint: VoiceEndpointConfig::default(),
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

    pub fn with_cli_timeout_secs(mut self, value: u32) -> Self {
        self.cli_timeout_secs = value;
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

    /// Reemplaza la tabla `[voice_endpoint]` completa (HU-41).
    pub fn with_voice_endpoint(mut self, value: VoiceEndpointConfig) -> Self {
        self.voice_endpoint = value;
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

    /// Timeout del cerebro CLI síncrono en segundos, saturado a 30–1800.
    pub fn cli_timeout_secs(&self) -> u32 {
        self.cli_timeout_secs
            .clamp(*CLI_TIMEOUT_RANGE.start(), *CLI_TIMEOUT_RANGE.end())
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

    /// Configuración de *endpointing* con todos los valores ya saturados
    /// (HU-41). Es lo que consume `jarvis-audio` para construir su
    /// `EndpointConfig`.
    pub fn voice_endpoint(&self) -> VoiceEndpointConfig {
        self.voice_endpoint.saturated()
    }

    /// Umbral VAD de apertura de turno, saturado a [`VOICE_OPEN_RANGE`].
    pub fn voice_open(&self) -> f32 {
        self.voice_endpoint.voice_open()
    }

    /// Umbral VAD de mantenimiento, saturado a [`VOICE_HOLD_RANGE`].
    pub fn voice_hold(&self) -> f32 {
        self.voice_endpoint.voice_hold()
    }

    /// Silencio de cierre normal en ms, saturado a [`SILENCE_MS_RANGE`].
    pub fn silence_ms(&self) -> u32 {
        self.voice_endpoint.silence_ms()
    }

    /// Silencio de cierre extendido (conector) en ms, saturado a
    /// [`EXTENDED_SILENCE_MS_RANGE`].
    pub fn extended_silence_ms(&self) -> u32 {
        self.voice_endpoint.extended_silence_ms()
    }

    /// Duración máxima de segmento en ms, saturada a [`MAX_SEGMENT_MS_RANGE`].
    pub fn max_segment_ms(&self) -> u32 {
        self.voice_endpoint.max_segment_ms()
    }

    /// Techo total de un turno largo en ms (HU-44), saturado a
    /// [`MAX_TURN_MS_RANGE`].
    pub fn max_turn_ms(&self) -> u32 {
        self.voice_endpoint.max_turn_ms()
    }

    /// Espera extra tras un turno que termina en conector, en ms, saturada a
    /// [`CONNECTOR_GRACE_MS_RANGE`].
    pub fn connector_grace_ms(&self) -> u32 {
        self.voice_endpoint.connector_grace_ms()
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

        let cli_timeout = self.cli_timeout_secs();
        if cli_timeout != self.cli_timeout_secs {
            eprintln!(
                "[config] cli_timeout_secs fuera de rango ({}): se satura a {cli_timeout}",
                self.cli_timeout_secs
            );
            self.cli_timeout_secs = cli_timeout;
        }

        let hud_bubbles = self.hud_max_bubbles();
        if hud_bubbles != self.hud_max_bubbles {
            eprintln!(
                "[config] hud_max_bubbles fuera de rango ({}): se satura a {hud_bubbles}",
                self.hud_max_bubbles
            );
            self.hud_max_bubbles = hud_bubbles;
        }

        let endpoint = self.voice_endpoint.saturated();
        if endpoint != self.voice_endpoint {
            eprintln!(
                "[config] [voice_endpoint] con valores fuera de rango ({:?}): se saturan a {:?}",
                self.voice_endpoint, endpoint
            );
            self.voice_endpoint = endpoint;
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
    fn cli_timeout_defaults_and_builder() {
        let config = JarvisConfig::default();
        assert_eq!(config.cli_timeout_secs(), 120);

        let config = config.with_cli_timeout_secs(300);
        assert_eq!(config.cli_timeout_secs(), 300);
    }

    #[test]
    fn cli_timeout_roundtrip_through_toml() {
        let path = temp_config_path("cli-timeout-roundtrip");
        cleanup(&path);
        let config = JarvisConfig {
            cli_timeout_secs: 240,
            ..JarvisConfig::default()
        };
        config.save_to(&path).unwrap();
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(toml.contains("cli_timeout_secs = 240"), "{toml}");

        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(loaded.cli_timeout_secs(), 240);
        cleanup(&path);
    }

    #[test]
    fn out_of_range_cli_timeout_saturates_on_load() {
        let path = temp_config_path("cli-timeout-sat");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "cli_timeout_secs = 5\n").unwrap();
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.cli_timeout_secs(), 30);
        cleanup(&path);

        let path2 = temp_config_path("cli-timeout-sat2");
        cleanup(&path2);
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(&path2, "cli_timeout_secs = 99999\n").unwrap();
        let loaded2 = JarvisConfig::load_from(&path2).unwrap();
        assert_eq!(loaded2.cli_timeout_secs(), 1_800);
        cleanup(&path2);
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

    #[test]
    fn voice_endpoint_defaults_match_spec() {
        let ep = VoiceEndpointConfig::default();
        assert_eq!(ep.voice_open, 0.45);
        assert_eq!(ep.voice_hold, 0.25);
        assert_eq!(ep.silence_ms, 1_200);
        assert_eq!(ep.extended_silence_ms, 2_000);
        assert_eq!(ep.max_segment_ms, 30_000);
        assert_eq!(ep.max_turn_ms, 120_000);
        assert_eq!(ep.connector_grace_ms, 500);
        // Los defaults ya están dentro de rango: saturar no cambia nada.
        assert_eq!(ep.saturated(), ep);

        let config = JarvisConfig::default();
        assert_eq!(config.voice_endpoint(), ep);
        assert_eq!(config.voice_open(), 0.45);
        assert_eq!(config.voice_hold(), 0.25);
        assert_eq!(config.silence_ms(), 1_200);
        assert_eq!(config.extended_silence_ms(), 2_000);
        assert_eq!(config.max_segment_ms(), 30_000);
        assert_eq!(config.max_turn_ms(), 120_000);
        assert_eq!(config.connector_grace_ms(), 500);
    }

    #[test]
    fn voice_endpoint_roundtrips_through_toml() {
        let ep = VoiceEndpointConfig {
            voice_open: 0.6,
            voice_hold: 0.35,
            silence_ms: 900,
            extended_silence_ms: 2_500,
            max_segment_ms: 45_000,
            max_turn_ms: 90_000,
            connector_grace_ms: 750,
        };
        // Serialización/deserialización directa del struct.
        let toml = toml::to_string(&ep).expect("serializa");
        assert!(toml.contains("voice_open = 0.6"), "{toml}");
        assert!(toml.contains("voice_hold = 0.35"), "{toml}");
        assert!(toml.contains("silence_ms = 900"), "{toml}");
        assert!(toml.contains("extended_silence_ms = 2500"), "{toml}");
        assert!(toml.contains("max_segment_ms = 45000"), "{toml}");
        assert!(toml.contains("max_turn_ms = 90000"), "{toml}");
        assert!(toml.contains("connector_grace_ms = 750"), "{toml}");
        let parsed: VoiceEndpointConfig = toml::from_str(&toml).expect("parsea");
        assert_eq!(parsed, ep);

        // Y como subtabla `[voice_endpoint]` de la configuración completa.
        let path = temp_config_path("voice-endpoint-roundtrip");
        cleanup(&path);
        let config = JarvisConfig::default().with_voice_endpoint(ep);
        config.save_to(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("[voice_endpoint]"), "{written}");
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.voice_endpoint(), ep);
        assert_eq!(loaded.voice_open(), 0.6);
        assert_eq!(loaded.voice_hold(), 0.35);
        assert_eq!(loaded.silence_ms(), 900);
        assert_eq!(loaded.extended_silence_ms(), 2_500);
        assert_eq!(loaded.max_segment_ms(), 45_000);
        assert_eq!(loaded.max_turn_ms(), 90_000);
        assert_eq!(loaded.connector_grace_ms(), 750);
        cleanup(&path);
    }

    #[test]
    fn partial_voice_endpoint_table_fills_defaults() {
        let path = temp_config_path("voice-endpoint-partial");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "voice_id = \"x\"\n[voice_endpoint]\nsilence_ms = 800\n",
        )
        .unwrap();
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.silence_ms(), 800);
        // El resto de la tabla cae a los defaults.
        assert_eq!(loaded.voice_open(), 0.45);
        assert_eq!(loaded.voice_hold(), 0.25);
        assert_eq!(loaded.extended_silence_ms(), 2_000);
        assert_eq!(loaded.max_segment_ms(), 30_000);
        assert_eq!(loaded.connector_grace_ms(), 500);
        cleanup(&path);
    }

    #[test]
    fn out_of_range_voice_endpoint_saturates_on_load() {
        let path = temp_config_path("voice-endpoint-sat-low");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[voice_endpoint]\nvoice_open = 0.0\nvoice_hold = -1.0\n\
             silence_ms = 0\nextended_silence_ms = 1\nmax_segment_ms = 10\n\
             connector_grace_ms = 0\n",
        )
        .unwrap();
        let loaded = JarvisConfig::load_from(&path).unwrap();
        assert_eq!(loaded.voice_open(), 0.1);
        assert_eq!(loaded.voice_hold(), 0.05);
        assert_eq!(loaded.silence_ms(), 300);
        assert_eq!(loaded.extended_silence_ms(), 500);
        assert_eq!(loaded.max_segment_ms(), 5_000);
        assert_eq!(loaded.connector_grace_ms(), 100);
        // La carga deja la estructura ya saturada (no solo el getter).
        assert_eq!(loaded.voice_endpoint, loaded.voice_endpoint());
        cleanup(&path);

        let path2 = temp_config_path("voice-endpoint-sat-high");
        cleanup(&path2);
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(
            &path2,
            "[voice_endpoint]\nvoice_open = 9.0\nvoice_hold = 9.0\n\
             silence_ms = 99_999\nextended_silence_ms = 99_999\n\
             max_segment_ms = 999_999\nmax_turn_ms = 999_999\n\
             connector_grace_ms = 99_999\n",
        )
        .unwrap();
        let loaded2 = JarvisConfig::load_from(&path2).unwrap();
        assert_eq!(loaded2.voice_open(), 0.95);
        assert_eq!(loaded2.voice_hold(), 0.8);
        assert_eq!(loaded2.silence_ms(), 5_000);
        assert_eq!(loaded2.extended_silence_ms(), 8_000);
        assert_eq!(loaded2.max_segment_ms(), 120_000);
        assert_eq!(loaded2.max_turn_ms(), 120_000);
        assert_eq!(loaded2.connector_grace_ms(), 3_000);
        cleanup(&path2);
    }

    #[test]
    fn max_turn_never_drops_below_max_segment() {
        // HU-44: un techo menor que el fragmento no tiene sentido; se eleva.
        let ep = VoiceEndpointConfig {
            max_segment_ms: 60_000,
            max_turn_ms: 10_000,
            ..VoiceEndpointConfig::default()
        };
        assert_eq!(ep.max_turn_ms(), 60_000);
        let low = VoiceEndpointConfig {
            max_turn_ms: 1,
            max_segment_ms: 5_000,
            ..VoiceEndpointConfig::default()
        };
        assert_eq!(low.max_turn_ms(), 5_000);
        assert_eq!(low.saturated().max_turn_ms, 5_000);
    }

    #[test]
    fn voice_endpoint_getters_saturate_without_write() {
        // Un valor inválido construido en memoria también se satura al leerlo.
        let ep = VoiceEndpointConfig {
            voice_open: f32::NAN,
            ..VoiceEndpointConfig::default()
        };
        assert_eq!(ep.voice_open(), *VOICE_OPEN_RANGE.start());
        let config = JarvisConfig::default().with_voice_endpoint(VoiceEndpointConfig {
            voice_open: 0.0,
            silence_ms: 1,
            ..VoiceEndpointConfig::default()
        });
        assert_eq!(config.voice_open(), 0.1);
        assert_eq!(config.silence_ms(), 300);
    }
}
