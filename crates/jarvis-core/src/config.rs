//! Configuración de usuario del asistente.

/// Configuración persistente de Jarvis.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}
