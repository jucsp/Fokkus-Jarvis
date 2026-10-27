//! Detector léxico de incompletitud en español (HU-43).
//!
//! Decide, solo con el texto transcrito, si el usuario dejó la frase a medias:
//! terminó en un conector ("y", "pero"), una muletilla ("eh", "o sea"), una
//! preposición o un artículo colgado ("para", "el"), un verbo auxiliar sin
//! complemento ("quiero", "necesito") o con puntos suspensivos. El orquestador
//! usa la señal para esperar el silencio extendido en vez de despachar media
//! consulta. Es barato (sin LLM ni audio) y se combinará con el clasificador
//! acústico de la Fase 2 a través de [`IncompleteCue::weight`].

/// Conectores: la frase sigue con otra cláusula.
const CONNECTORS: &[&str] = &[
    "y", "e", "o", "u", "pero", "que", "porque", "entonces", "aunque", "ni", "sino", "pues",
    "como", "cuando", "donde", "si", "mientras", "luego",
];

/// Muletillas de una palabra (las de dos, como "o sea", van aparte).
const FILLERS: &[&str] = &["este", "esta", "igual", "osea", "tipo", "bueno", "onda"];

/// Muletillas de dos palabras: `(penúltima, última)`.
const FILLER_PAIRS: &[(&str, &str)] = &[("o", "sea"), ("como", "que"), ("es", "que")];

/// Preposiciones (y contracciones) que exigen un complemento.
const PREPOSITIONS: &[&str] = &[
    "a", "al", "ante", "con", "contra", "de", "del", "desde", "en", "entre", "hacia", "hasta",
    "para", "por", "según", "sin", "sobre", "tras",
];

/// Artículos, posesivos y demostrativos que exigen un sustantivo.
const DETERMINERS: &[&str] = &[
    "el", "la", "los", "las", "un", "una", "unos", "unas", "mi", "mis", "tu", "tus", "su", "sus",
    "nuestro", "nuestra", "nuestros", "nuestras", "ese", "esa", "esos", "esas", "estos", "estas",
    "aquel", "aquella", "cada", "otro", "otra",
];

/// Verbos que piden un infinitivo o un objeto ("quiero…", "necesito…").
const AUXILIARIES: &[&str] = &[
    "quiero",
    "quisiera",
    "necesito",
    "necesitaría",
    "puedes",
    "podrías",
    "podrias",
    "puedo",
    "debo",
    "debería",
    "deberia",
    "gustaría",
    "gustaria",
    "intenta",
];

/// Palabras que, antes de un auxiliar, cierran la frase: "lo quiero", "no
/// puedo", "eso necesito" son completas.
const AUXILIARY_CLOSERS: &[&str] = &[
    "lo", "la", "los", "las", "le", "les", "me", "te", "se", "nos", "eso", "esto", "no", "yo",
    "tú", "que", "qué", "si", "cuanto", "cuánto", "como", "cómo",
];

/// Por qué la frase parece inacabada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompleteCue {
    /// Termina en un conector ("y", "pero", "porque").
    Connector,
    /// Termina en una muletilla o vacilación ("eh", "mmm", "o sea").
    Filler,
    /// Termina en una preposición ("para", "de", "con").
    Preposition,
    /// Termina en un artículo o determinante ("el", "mi", "esa").
    Determiner,
    /// Termina en un verbo auxiliar sin complemento ("quiero", "necesito").
    Auxiliary,
    /// Termina en puntos suspensivos o coma: whisper marcó que se apagó.
    TrailingOff,
}

impl IncompleteCue {
    /// Probabilidad aproximada de que el usuario siga hablando (0–1). Es la
    /// señal `p_text` que se combinará con el clasificador acústico (HU-47).
    pub fn weight(self) -> f32 {
        match self {
            Self::Connector | Self::Preposition | Self::Determiner => 0.9,
            Self::Filler => 0.8,
            Self::Auxiliary => 0.7,
            Self::TrailingOff => 0.6,
        }
    }

    /// Etiqueta estable para logs y diagnóstico.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connector => "connector",
            Self::Filler => "filler",
            Self::Preposition => "preposition",
            Self::Determiner => "determiner",
            Self::Auxiliary => "auxiliary",
            Self::TrailingOff => "trailing_off",
        }
    }
}

/// Pista de incompletitud del texto, o `None` si parece una frase cerrada.
pub fn incompleteness(text: &str) -> Option<IncompleteCue> {
    let words: Vec<String> = text.split_whitespace().filter_map(normalize).collect();
    let last = words.last()?;
    let previous = words.len().checked_sub(2).map(|i| words[i].as_str());

    // Muletillas de dos palabras antes que los conectores: "o sea" no es "o".
    if previous.is_some_and(|p| FILLER_PAIRS.contains(&(p, last.as_str()))) {
        return Some(IncompleteCue::Filler);
    }
    if CONNECTORS.contains(&last.as_str()) {
        return Some(IncompleteCue::Connector);
    }
    if is_hesitation(last) || FILLERS.contains(&last.as_str()) {
        return Some(IncompleteCue::Filler);
    }
    if PREPOSITIONS.contains(&last.as_str()) {
        return Some(IncompleteCue::Preposition);
    }
    if DETERMINERS.contains(&last.as_str()) {
        return Some(IncompleteCue::Determiner);
    }
    // "me gustaría" sigue abierta aunque lleve clítico: el pronombre es parte
    // de la perífrasis, no el objeto.
    let closed =
        previous.is_some_and(|p| AUXILIARY_CLOSERS.contains(&p)) && !last.starts_with("gustar");
    if AUXILIARIES.contains(&last.as_str()) && !closed {
        return Some(IncompleteCue::Auxiliary);
    }
    if trails_off(text) {
        return Some(IncompleteCue::TrailingOff);
    }
    None
}

/// `true` si el texto parece inacabado (el usuario no terminó).
pub fn is_incomplete(text: &str) -> bool {
    incompleteness(text).is_some()
}

/// Puntaje 0–1 de que el usuario siga hablando; 0 si la frase está cerrada.
pub fn incompleteness_score(text: &str) -> f32 {
    incompleteness(text).map_or(0.0, IncompleteCue::weight)
}

/// Minúsculas y sin signos; `None` si la palabra era solo puntuación.
fn normalize(word: &str) -> Option<String> {
    let w: String = word
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    (!w.is_empty()).then_some(w)
}

/// Vacilaciones sin léxico: "eh", "ehh", "em", "mmm", "hmm", "ehm".
fn is_hesitation(word: &str) -> bool {
    word.chars().count() >= 2
        && word.chars().all(|c| matches!(c, 'e' | 'h' | 'm'))
        && !matches!(word, "me" | "he")
}

/// Puntos suspensivos o coma final: whisper deja así una frase que se apaga.
fn trails_off(text: &str) -> bool {
    let t = text.trim_end();
    t.ends_with("...") || t.ends_with('…') || t.ends_with(',')
}

#[cfg(test)]
mod tests {
    use super::*;
    use IncompleteCue::*;

    /// Tabla de verdad: `(texto, pista esperada)`.
    const TRUTH_TABLE: &[(&str, Option<IncompleteCue>)] = &[
        // Conectores.
        ("busca el archivo y", Some(Connector)),
        ("ábrelo pero", Some(Connector)),
        ("avísame porque", Some(Connector)),
        ("hazlo entonces", Some(Connector)),
        ("dime si", Some(Connector)),
        ("recuérdame que", Some(Connector)),
        ("Busca el archivo, y...", Some(Connector)),
        ("Jarvis, abre el menú Y.", Some(Connector)),
        // Muletillas y vacilaciones.
        ("quiero ver, eh", Some(Filler)),
        ("busca el, ehh", Some(Filler)),
        ("pon música, este", Some(Filler)),
        ("abre el correo o sea", Some(Filler)),
        ("necesito algo, o sea,", Some(Filler)),
        ("mmm", Some(Filler)),
        ("abre Spotify, mmm...", Some(Filler)),
        ("hmm", Some(Filler)),
        ("es como que", Some(Filler)),
        ("lo hago igual", Some(Filler)),
        ("ehm", Some(Filler)),
        ("lo que pasa es que", Some(Filler)),
        // Preposiciones.
        ("busca algo para", Some(Preposition)),
        ("abre el archivo de", Some(Preposition)),
        ("voy a", Some(Preposition)),
        ("llama con", Some(Preposition)),
        ("mándalo al", Some(Preposition)),
        ("ordénalo por", Some(Preposition)),
        ("¿Cuánto falta hasta?", Some(Preposition)),
        // Artículos y determinantes.
        ("abre el", Some(Determiner)),
        ("busca la", Some(Determiner)),
        ("pon un", Some(Determiner)),
        ("revisa mi", Some(Determiner)),
        ("cierra esa", Some(Determiner)),
        ("mándale a mis", Some(Determiner)),
        // Auxiliares colgados.
        ("Jarvis, quiero", Some(Auxiliary)),
        ("necesito", Some(Auxiliary)),
        ("me gustaría", Some(Auxiliary)),
        ("¿puedes?", Some(Auxiliary)),
        ("Jarvis, ¿podrías", Some(Auxiliary)),
        // Frase que se apaga.
        ("abre Spotify...", Some(TrailingOff)),
        ("busca el clima…", Some(TrailingOff)),
        ("abre el navegador,", Some(TrailingOff)),
        // Frases completas.
        ("", None),
        ("   ", None),
        ("busca el archivo", None),
        ("¿qué tal el clima?", None),
        ("Jarvis, qué hora es", None),
        ("qué tareas tengo", None),
        ("abre Spotify.", None),
        ("cierra Spotify", None),
        ("lo quiero", None),
        ("no puedo", None),
        ("eso necesito", None),
        ("dime", None),
        ("dame eso", None),
        ("tráeme", None),
        ("guárdalo", None),
        ("pausa el video", None),
        ("escribe en pantalla Hola mundo.", None),
        ("busca Marvel Rivals", None),
        ("silencio", None),
        ("¿y mañana?", None),
        ("Hola, ¿cómo estás?", None),
        ("ya voy", None),
        ("vamos", None),
        ("revisa mis correos y dime si hay algo urgente", None),
    ];

    #[test]
    fn truth_table_covers_spanish_incompleteness() {
        assert!(TRUTH_TABLE.len() >= 60, "tabla de verdad con > 60 casos");
        let failures: Vec<String> = TRUTH_TABLE
            .iter()
            .filter(|(text, expected)| incompleteness(text) != *expected)
            .map(|(text, expected)| {
                format!(
                    "{text:?}: esperado {expected:?}, obtenido {:?}",
                    incompleteness(text)
                )
            })
            .collect();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn is_incomplete_matches_the_cue() {
        for (text, expected) in TRUTH_TABLE {
            assert_eq!(is_incomplete(text), expected.is_some(), "{text:?}");
        }
    }

    #[test]
    fn score_is_zero_for_complete_and_bounded_otherwise() {
        assert_eq!(incompleteness_score("busca el archivo"), 0.0);
        for (text, expected) in TRUTH_TABLE.iter().filter(|(_, e)| e.is_some()) {
            let score = incompleteness_score(text);
            assert!(score > 0.0 && score <= 1.0, "{text:?} → {score}");
            assert_eq!(score, expected.unwrap().weight());
        }
    }

    #[test]
    fn hesitations_do_not_swallow_real_words() {
        for word in ["eh", "ehh", "em", "mm", "mmmm", "hm", "ehm"] {
            assert!(is_hesitation(word), "{word}");
        }
        for word in ["me", "he", "e", "m", "mesa", "hola"] {
            assert!(!is_hesitation(word), "{word}");
        }
    }

    #[test]
    fn cue_labels_are_stable() {
        let all = [
            Connector,
            Filler,
            Preposition,
            Determiner,
            Auxiliary,
            TrailingOff,
        ];
        let labels: Vec<_> = all.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            labels,
            [
                "connector",
                "filler",
                "preposition",
                "determiner",
                "auxiliary",
                "trailing_off"
            ]
        );
    }
}
