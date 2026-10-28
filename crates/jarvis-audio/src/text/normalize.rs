//! Normalización léxica unificada de las órdenes de voz (HU-59).
//!
//! Antes de HU-59 convivían dos criterios de comparación distintos en el mismo
//! módulo: `classify_workload` comparaba por subcadena sobre `to_lowercase()`
//! (de ahí "reporte" ⊃ "repo" y "blog" ⊃ "log"), mientras que las tareas de
//! desarrollo y las operacionales ya comparaban palabra completa. Este módulo
//! deja **un** criterio para todo:
//!
//! - minúsculas y tildes plegadas (`á` → `a`, `ñ` → `n`, `ü` → `u`);
//! - tokens alfanuméricos completos: nunca `contains` por subcadena;
//! - pronombres enclíticos de las órdenes habladas lematizados
//!   ("revísame" → "revisa", "analízalo" → "analiza");
//! - variantes fonéticas de Whisper (STT) unificadas en un token canónico
//!   ("pe erre", "pull requests", "PRs" → `pr`).
//!
//! El resultado es un [`Normalized`] con el texto acolchado por espacios (para
//! poder buscar frases de varias palabras) y la lista de tokens.

/// Token canónico al que se reducen todas las transcripciones de "PR".
pub const PR_TOKEN: &str = "pr";

/// Verbos que, junto a un objeto de [`HEAVY_OBJECTS`], marcan una consulta
/// como tarea larga. Todos sin tildes: el texto llega ya normalizado.
pub const HEAVY_VERBS: &[&str] = &[
    // Los seis clásicos (HU-37).
    "revisa",
    "revisar",
    "analiza",
    "analizar",
    "resume",
    "resumir",
    "busca",
    "buscar",
    "compara",
    "comparar",
    "recopila",
    "recopilar",
    "investiga",
    "investigar",
    "procesa",
    "procesar",
    // Ampliación de HU-59: verbos de revisión e inspección.
    "lee",
    "leer",
    "chequea",
    "chequear",
    "mira",
    "mirar",
    "echa",
    "echar",
    "inspecciona",
    "inspeccionar",
    "audita",
    "auditar",
    "monitorea",
    "monitorear",
    "escanea",
    "escanear",
    "evalua",
    "evaluar",
    // Locuciones con verbo ligero que piden un informe.
    "genera un informe",
    "arma un informe",
    "prepara un informe",
];

/// Modismos que equivalen a un verbo pesado ("échale un ojo a las PR").
pub const HEAVY_IDIOMS: &[&str] = &[
    "echa un ojo",
    "echa un vistazo",
    "dale una revision",
    "dale un ojo",
    "dale una mirada",
    "dale un vistazo",
    "pega una mirada",
    "pegale una mirada",
    "pegale un ojo",
];

/// Objetos que, junto a un verbo pesado, marcan la consulta como `Heavy`.
pub const HEAVY_OBJECTS: &[&str] = &[
    PR_TOKEN,
    "repo",
    "repos",
    "repositorio",
    "repositorios",
    "correo",
    "correos",
    "bandeja",
    "inbox",
    "gmail",
    "ticket",
    "tickets",
    "triage",
    "triaje",
    "slack",
    "drive",
    "log",
    "logs",
    "dato",
    "datos",
    "documento",
    "documentos",
    "documentacion",
    "codigo",
];

/// Pasa `s` a minúsculas y convierte cada carácter acentuado en su forma base
/// (`á`→`a`, `é`→`e`, `í`→`i`, `ó`→`o`, `ú`/`ü`→`u`, `ñ`→`n`).
///
/// No toca la puntuación: para palabras completas usa [`tokenize`].
pub fn fold_accents(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        for lower in c.to_lowercase() {
            out.push(fold_char(lower));
        }
    }
    out
}

fn fold_char(c: char) -> char {
    match c {
        'á' | 'à' | 'ä' | 'â' | 'ã' | 'å' => 'a',
        'é' | 'è' | 'ë' | 'ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o',
        'ú' | 'ù' | 'ü' | 'û' => 'u',
        'ñ' => 'n',
        'ç' => 'c',
        other => other,
    }
}

/// Extrae las palabras alfanuméricas completas, en minúsculas y sin acentos.
///
/// "Revísame las PR, ¡por favor!" → `["revisame", "las", "pr", "por", "favor"]`.
pub fn tokenize(s: &str) -> Vec<String> {
    let folded = fold_accents(s);
    let mut tokens = Vec::new();
    let mut current = String::new();
    for c in folded.chars() {
        if c.is_alphanumeric() {
            current.push(c);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Lematiza los pronombres enclíticos frecuentes en órdenes de voz.
///
/// "revísame" / "revísalo" / "revisalas" → "revisa"; si la palabra no es un
/// enclítico conocido se devuelve tal cual.
pub fn lemmatize_enclitic(word: &str) -> &str {
    match word {
        "revisame" | "revisalo" | "revisala" | "revisalos" | "revisalas" | "revisanos"
        | "revisale" | "revisate" | "revisemelo" => "revisa",
        "analizame" | "analizalo" | "analizala" | "analizalos" | "analizalas" | "analizanos"
        | "analizale" | "analizate" => "analiza",
        "echale" | "echame" | "echate" | "echalos" | "echalas" => "echa",
        "chequealo" | "chequeame" | "chequeala" | "chequealos" | "chequealas" => "chequea",
        "resumeme" | "resumelo" | "resumela" | "resumelos" | "resumelas" => "resume",
        "miralo" | "mirame" | "mirala" | "miralos" | "miralas" => "mira",
        "buscame" | "buscalo" | "buscala" | "buscalos" | "buscalas" => "busca",
        "leeme" | "leelo" | "leela" | "leelos" | "leelas" => "lee",
        "damelo" | "dame" => "da",
        "hazlo" | "hazme" | "hazla" => "haz",
        "explicame" | "explicalo" | "explicala" => "explica",
        other => other,
    }
}

/// Une las variantes fonéticas de STT en tokens canónicos.
///
/// - `["pe", "erre"]`, `["pe", "erres"]` → `["pr"]`
/// - `["pull", "request"]`, `["pull", "requests"]` → `["pr"]`
/// - `["prs"]`, `["pr"]` → `["pr"]`
pub fn merge_phonetic_aliases(tokens: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        let next = tokens.get(i + 1).map(String::as_str);
        if (token == "pe" && matches!(next, Some("erre" | "erres")))
            || (matches!(token, "pull" | "pul") && matches!(next, Some("request" | "requests")))
        {
            out.push(PR_TOKEN.to_string());
            i += 2;
        } else if matches!(token, "pr" | "prs") {
            out.push(PR_TOKEN.to_string());
            i += 1;
        } else {
            out.push(token.to_string());
            i += 1;
        }
    }
    out
}

/// Texto normalizado listo para comparar por palabras completas.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Normalized {
    /// Tokens separados por un espacio y acolchados (" revisa mis correos ").
    text: String,
    tokens: Vec<String>,
}

impl Normalized {
    /// Texto acolchado por espacios; buscar aquí siempre es palabra completa.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Tokens canónicos en orden.
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    /// `true` si no hay ninguna palabra.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// `true` si `phrase` (palabra o frase de varias palabras) aparece como
    /// palabras completas. "repo" **no** coincide con "reporte".
    pub fn has_phrase(&self, phrase: &str) -> bool {
        let needle = normalize(phrase);
        !needle.is_empty() && self.text.contains(&needle.text)
    }

    /// `true` si aparece alguna de las frases.
    pub fn has_any(&self, phrases: &[&str]) -> bool {
        phrases.iter().any(|phrase| self.has_phrase(phrase))
    }
}

/// Normaliza `input`: minúsculas, sin tildes, sin puntuación, enclíticos
/// lematizados y variantes fonéticas de "PR" unificadas.
pub fn normalize(input: &str) -> Normalized {
    let folded = tokenize(input);
    let lemmas: Vec<String> = folded
        .iter()
        .map(|word| lemmatize_enclitic(word).to_string())
        .collect();
    let tokens = merge_phonetic_aliases(&lemmas);
    let mut text = String::with_capacity(tokens.iter().map(|t| t.len() + 1).sum::<usize>() + 1);
    text.push(' ');
    for token in &tokens {
        text.push_str(token);
        text.push(' ');
    }
    Normalized { text, tokens }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_accents_lowers_and_strips_diacritics() {
        assert_eq!(
            fold_accents("Revísame las PR de Melón"),
            "revisame las pr de melon"
        );
        assert_eq!(fold_accents("Canción ÜBER"), "cancion uber");
        assert_eq!(fold_accents("pingüino"), "pinguino");
        assert_eq!(fold_accents("sin acentos"), "sin acentos");
    }

    #[test]
    fn tokenize_keeps_only_whole_alphanumeric_words() {
        assert_eq!(
            tokenize("Revísame las PR, ¡por favor!"),
            ["revisame", "las", "pr", "por", "favor"]
        );
        assert_eq!(tokenize("   "), Vec::<String>::new());
        assert_eq!(tokenize("uno-dos_tres"), ["uno", "dos", "tres"]);
    }

    #[test]
    fn enclitics_are_lemmatized() {
        for (word, lemma) in [
            ("revisame", "revisa"),
            ("revisalo", "revisa"),
            ("revisalas", "revisa"),
            ("analizame", "analiza"),
            ("analizalo", "analiza"),
            ("echale", "echa"),
            ("chequealo", "chequea"),
            ("resumelo", "resume"),
            ("miralo", "mira"),
            ("buscala", "busca"),
            ("leelo", "lee"),
            ("damelo", "da"),
            ("hazlo", "haz"),
            ("explicame", "explica"),
            ("revisa", "revisa"),
            ("correos", "correos"),
        ] {
            assert_eq!(lemmatize_enclitic(word), lemma, "{word}");
        }
    }

    #[test]
    fn phonetic_pr_variants_collapse_to_one_token() {
        let merge = |words: &[&str]| {
            let owned: Vec<String> = words.iter().map(|w| (*w).to_string()).collect();
            merge_phonetic_aliases(&owned)
        };
        assert_eq!(merge(&["las", "pe", "erre", "de"]), ["las", "pr", "de"]);
        assert_eq!(merge(&["las", "pe", "erres"]), ["las", "pr"]);
        assert_eq!(merge(&["los", "pull", "request"]), ["los", "pr"]);
        assert_eq!(merge(&["los", "pull", "requests"]), ["los", "pr"]);
        assert_eq!(merge(&["las", "prs"]), ["las", "pr"]);
        assert_eq!(merge(&["la", "pr"]), ["la", "pr"]);
        // "pull" suelto no es una PR.
        assert_eq!(merge(&["pull", "over"]), ["pull", "over"]);
    }

    #[test]
    fn normalizing_a_sentence_applies_every_step() {
        let n = normalize("Échale un ojo a las pe erre del repo de Melón");
        let tokens: Vec<&str> = n.tokens().iter().map(String::as_str).collect();
        assert_eq!(
            tokens,
            ["echa", "un", "ojo", "a", "las", "pr", "del", "repo", "de", "melon"]
        );
        assert_eq!(n.text(), " echa un ojo a las pr del repo de melon ");
        assert!(n.has_phrase("echa un ojo"));
        assert!(n.has_phrase("pr"));
        assert!(n.has_phrase("repo"));
        assert!(!n.has_phrase("repositorio"));
        assert!(normalize("").is_empty());
    }

    #[test]
    fn matching_is_by_whole_word_never_by_substring() {
        // Falsos positivos que tenía el `contains` de HU-37.
        assert!(!normalize("revisa el reporte de ventas").has_phrase("repo"));
        assert!(!normalize("resume el blog de Rust").has_phrase("log"));
        assert!(!normalize("busca el catálogo de productos").has_phrase("log"));
        assert!(!normalize("cuál es la iniciativa").has_phrase("inicia"));
        assert!(!normalize("la procedencia del vino").has_phrase("procede"));
        assert!(!normalize("construyendo una casa").has_phrase("construye"));
        // Y los positivos legítimos siguen funcionando.
        assert!(normalize("revisa el repo de melón").has_phrase("repo"));
        assert!(normalize("los logs del servidor").has_phrase("logs"));
    }

    #[test]
    fn phrase_matching_needs_every_word() {
        let n = normalize("deja un monitor corriendo en segundo plano");
        assert!(n.has_any(&["segundo plano"]));
        assert!(!n.has_any(&["primer plano"]));
        assert!(n.has_any(&["monitor", "ping", "contador"]));
    }
}
