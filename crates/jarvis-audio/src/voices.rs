//! Catálogo de voces neuronales de Edge-TTS (español e inglés).
//!
//! Incluye las 19 voces soportadas en Fase 2 (15 en español y 4 en inglés).
//! La voz por defecto es `es-CL-LorenzoNeural` (Chile, masculina), alineada
//! con el SDD.

/// Identificador de la voz por defecto.
pub const DEFAULT_VOICE_ID: &str = "es-CL-LorenzoNeural";

/// Idioma/código de la voz por defecto.
pub const DEFAULT_VOICE_LANG: &str = "es-CL";

/// Género de una voz (para el selector de voz).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gender {
    Male,
    Female,
}

/// Descriptor inmutable de una voz neuronal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub gender: Gender,
    pub lang: &'static str,
    pub region: &'static str,
    pub preview_text: &'static str,
}

/// Texto de preescucha usado por todas las voces.
const PREVIEW_TEXT: &str = "Hola, soy Jarvis, tu asistente de escritorio.";

/// Catálogo de las 19 voces neuronales soportadas.
pub static VOICES: [VoiceInfo; 19] = [
    VoiceInfo {
        id: "es-CL-LorenzoNeural",
        name: "Lorenzo",
        gender: Gender::Male,
        lang: "es-CL",
        region: "Chile",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-CL-CatalinaNeural",
        name: "Catalina",
        gender: Gender::Female,
        lang: "es-CL",
        region: "Chile",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-AR-ElenaNeural",
        name: "Elena",
        gender: Gender::Female,
        lang: "es-AR",
        region: "Argentina",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-AR-TomasNeural",
        name: "Tomás",
        gender: Gender::Male,
        lang: "es-AR",
        region: "Argentina",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-CO-GonzaloNeural",
        name: "Gonzalo",
        gender: Gender::Male,
        lang: "es-CO",
        region: "Colombia",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-CO-SalomeNeural",
        name: "Salomé",
        gender: Gender::Female,
        lang: "es-CO",
        region: "Colombia",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-MX-DaliaNeural",
        name: "Dalia",
        gender: Gender::Female,
        lang: "es-MX",
        region: "México",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-MX-JorgeNeural",
        name: "Jorge",
        gender: Gender::Male,
        lang: "es-MX",
        region: "México",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-ES-AlvaroNeural",
        name: "Álvaro",
        gender: Gender::Male,
        lang: "es-ES",
        region: "España",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-ES-ElviraNeural",
        name: "Elvira",
        gender: Gender::Female,
        lang: "es-ES",
        region: "España",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-PE-CamilaNeural",
        name: "Camila",
        gender: Gender::Female,
        lang: "es-PE",
        region: "Perú",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-PE-AlexNeural",
        name: "Alex",
        gender: Gender::Male,
        lang: "es-PE",
        region: "Perú",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-US-AlonsoNeural",
        name: "Alonso",
        gender: Gender::Male,
        lang: "es-US",
        region: "Estados Unidos",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-US-PalomaNeural",
        name: "Paloma",
        gender: Gender::Female,
        lang: "es-US",
        region: "Estados Unidos",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "es-VE-PaolaNeural",
        name: "Paola",
        gender: Gender::Female,
        lang: "es-VE",
        region: "Venezuela",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "en-US-AriaNeural",
        name: "Aria",
        gender: Gender::Female,
        lang: "en-US",
        region: "Estados Unidos",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "en-US-GuyNeural",
        name: "Guy",
        gender: Gender::Male,
        lang: "en-US",
        region: "Estados Unidos",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "en-GB-SoniaNeural",
        name: "Sonia",
        gender: Gender::Female,
        lang: "en-GB",
        region: "Reino Unido",
        preview_text: PREVIEW_TEXT,
    },
    VoiceInfo {
        id: "en-GB-RyanNeural",
        name: "Ryan",
        gender: Gender::Male,
        lang: "en-GB",
        region: "Reino Unido",
        preview_text: PREVIEW_TEXT,
    },
];

/// Voz por defecto (primera del catálogo, `es-CL-LorenzoNeural`).
pub fn default_voice() -> &'static VoiceInfo {
    &VOICES[0]
}

/// Resuelve una voz por identificador (`es-CL-LorenzoNeural`) o por idioma
/// (`es-CL`, `es-MX`, `es`…).
///
/// 1. Coincidencia exacta de `id` (sin distinguir mayúsculas).
/// 2. Coincidencia exacta de `lang` (devuelve la voz principal de esa región).
/// 3. Prefijo de `lang` (p. ej. `es` → primera voz en español del catálogo).
/// 4. Si nada coincide, devuelve [`default_voice`].
pub fn resolve_voice(lang_or_id: &str) -> &'static VoiceInfo {
    let needle = lang_or_id.trim();
    if needle.is_empty() {
        return default_voice();
    }

    for voice in VOICES.iter() {
        if voice.id.eq_ignore_ascii_case(needle) {
            return voice;
        }
    }
    for voice in VOICES.iter() {
        if voice.lang.eq_ignore_ascii_case(needle) {
            return voice;
        }
    }
    let lowered = needle.to_ascii_lowercase();
    for voice in VOICES.iter() {
        if voice
            .lang
            .to_ascii_lowercase()
            .starts_with(lowered.as_str())
        {
            return voice;
        }
    }
    default_voice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_voice_matches_constants() {
        assert_eq!(default_voice().id, DEFAULT_VOICE_ID);
        assert_eq!(default_voice().lang, DEFAULT_VOICE_LANG);
    }

    #[test]
    fn resolves_by_exact_id_case_insensitive() {
        assert_eq!(
            resolve_voice("es-CL-LorenzoNeural").id,
            "es-CL-LorenzoNeural"
        );
        assert_eq!(
            resolve_voice("es-cl-lorenzoneural").id,
            "es-CL-LorenzoNeural"
        );
        assert_eq!(resolve_voice("es-MX-DaliaNeural").id, "es-MX-DaliaNeural");
    }

    #[test]
    fn resolves_by_lang_exact_and_prefix() {
        assert_eq!(resolve_voice("es-CL").lang, "es-CL");
        assert_eq!(resolve_voice("es-MX").lang, "es-MX");
        // Primera voz en español del catálogo (es-CL-LorenzoNeural).
        assert_eq!(resolve_voice("es").lang, "es-CL");
    }

    #[test]
    fn falls_back_to_default_for_unknown() {
        assert_eq!(resolve_voice("xx-XX-Desconocida").id, DEFAULT_VOICE_ID);
        assert_eq!(resolve_voice("").id, DEFAULT_VOICE_ID);
        assert_eq!(resolve_voice("   ").id, DEFAULT_VOICE_ID);
    }
}
