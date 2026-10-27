//! Segmentador incremental de frases para streaming LLM (HU-8).
//!
//! Acumula texto token a token y emite frases completas en cuanto detecta un
//! delimitador de fin de frase, de modo que la síntesis de voz puede arrancar
//! mientras el resto de la respuesta sigue generándose.
//!
//! Antes de segmentar, [`SpeechFilter`] descarta lo que nunca debe decirse en
//! voz alta: bloques de razonamiento (`<thought>…</thought>`,
//! `<thinking>…</thinking>`) y líneas de diagnóstico de la CLI (`jetski:`,
//! `[INFO]`), aunque lleguen partidos entre varios tokens. Después,
//! [`DirectiveFilter`] retira las directivas de prosodia del LLM (HU-45) y
//! cada frase sale con su [`SpeechDirective`].

use super::directive::{squeeze_spaces, DirectedPhrase, DirectiveFilter, Piece, ProsodyState, Tag};

/// Abreviaturas comunes en español que no deben partir la frase.
const ABBREVIATIONS: &[&str] = &[
    "sr", "sra", "dr", "dra", "d", "dna", "prof", "ej", "etc", "ud", "uds", "art", "num", "pag",
    "vol", "aprox",
];

/// Etiquetas de razonamiento (apertura sin `>`, cierre completo).
const THOUGHT_TAGS: &[(&str, &str)] = &[("<thinking", "</thinking>"), ("<thought", "</thought>")];

/// Prefijos de línea que delatan salida de diagnóstico, no respuesta.
const NOISE_PREFIXES: &[&str] = &["jetski:", "[info]"];

/// Filtro incremental de lo que no debe vocalizarse.
///
/// Retiene solo lo indispensable para decidir (un `<th` que puede ser el
/// inicio de `<thought>`, un `jets` al inicio de línea) y deja pasar el resto
/// en cuanto llega, para no frenar la síntesis.
#[derive(Debug, Default)]
struct SpeechFilter {
    /// Texto recibido y aún sin decidir.
    pending: String,
    /// Etiqueta de cierre esperada mientras se está dentro de un bloque.
    in_thought: Option<&'static str>,
    /// Descartando hasta el próximo salto de línea.
    skip_line: bool,
    /// `false` al inicio de una línea (ahí se evalúan los prefijos de ruido).
    mid_line: bool,
}

impl SpeechFilter {
    fn push(&mut self, text: &str, out: &mut String) {
        self.pending.push_str(text);
        self.process(out, false);
    }

    /// Vuelca lo retenido: un bloque sin cerrar o una línea de ruido se
    /// descartan; un prefijo indeciso era texto normal.
    fn finish(&mut self, out: &mut String) {
        self.process(out, true);
        *self = Self::default();
    }

    fn process(&mut self, out: &mut String, eof: bool) {
        while !self.pending.is_empty() {
            if let Some(close) = self.in_thought {
                match find_ignore_case(&self.pending, close) {
                    Some(i) => {
                        self.pending.drain(..i + close.len());
                        self.in_thought = None;
                        continue;
                    }
                    None => {
                        // Solo se conserva una posible etiqueta de cierre partida.
                        let keep = self
                            .pending
                            .rfind('<')
                            .filter(|&i| starts_with_ignore_case(close, &self.pending[i..]))
                            .unwrap_or(self.pending.len());
                        self.pending.drain(..keep);
                        return;
                    }
                }
            }

            if self.skip_line {
                match self.pending.find('\n') {
                    Some(i) => {
                        self.pending.drain(..=i);
                        self.skip_line = false;
                        self.mid_line = false;
                        continue;
                    }
                    None => {
                        self.pending.clear();
                        return;
                    }
                }
            }

            if !self.mid_line {
                let line = self
                    .pending
                    .trim_start_matches(|c: char| c.is_whitespace() && c != '\n');
                if line.starts_with('\n') {
                    let lead = self.pending.len() - line.len() + 1;
                    out.push_str(&self.pending[..lead]);
                    self.pending.drain(..lead);
                    continue;
                }
                if line.is_empty() {
                    if eof {
                        out.push_str(&self.pending);
                        self.pending.clear();
                    }
                    return;
                }
                if NOISE_PREFIXES
                    .iter()
                    .any(|p| starts_with_ignore_case(line, p))
                {
                    self.skip_line = true;
                    continue;
                }
                if !eof
                    && NOISE_PREFIXES
                        .iter()
                        .any(|p| starts_with_ignore_case(p, line))
                {
                    return;
                }
                self.mid_line = true;
            }

            let Some(i) = self.pending.find(['<', '\n']) else {
                out.push_str(&self.pending);
                self.pending.clear();
                return;
            };
            out.push_str(&self.pending[..i]);
            self.pending.drain(..i);
            if self.pending.starts_with('\n') {
                out.push('\n');
                self.pending.drain(..1);
                self.mid_line = false;
                continue;
            }

            // `pending` empieza con '<'.
            if let Some(&(open, close)) = THOUGHT_TAGS.iter().find(|(open, _)| {
                starts_with_ignore_case(&self.pending, open)
                    && self.pending[open.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == '>' || c.is_whitespace())
            }) {
                match self.pending[open.len()..].find('>') {
                    Some(end) => {
                        self.pending.drain(..open.len() + end + 1);
                        self.in_thought = Some(close);
                        continue;
                    }
                    None if eof => {
                        self.pending.clear();
                        return;
                    }
                    None => return,
                }
            }
            // Cierre huérfano: se descarta sin más.
            if let Some(&(_, close)) = THOUGHT_TAGS
                .iter()
                .find(|(_, close)| starts_with_ignore_case(&self.pending, close))
            {
                self.pending.drain(..close.len());
                continue;
            }
            // Etiqueta aún incompleta: esperar al siguiente token. Al cerrar el
            // stream, un fragmento ya inequívoco (`<thin`, `</thou`) se
            // descarta; uno ambiguo (`<th`) era texto.
            let partial = THOUGHT_TAGS.iter().any(|(open, close)| {
                starts_with_ignore_case(open, &self.pending)
                    || starts_with_ignore_case(close, &self.pending)
                    || self.pending.len() == open.len()
                        && starts_with_ignore_case(&self.pending, open)
            });
            if partial && !eof {
                return;
            }
            if partial && self.pending.trim_start_matches(['<', '/']).len() >= 4 {
                self.pending.clear();
                return;
            }
            out.push('<');
            self.pending.drain(..1);
        }
    }
}

/// `haystack` empieza con `prefix` (ASCII sin distinguir mayúsculas).
fn starts_with_ignore_case(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len()
        && haystack.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Posición de `needle` (ASCII, empieza con `<`) sin distinguir mayúsculas.
fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Segmentador incremental de frases para streaming LLM.
#[derive(Debug)]
pub struct PhraseSplitter {
    buffer: String,
    max_chars: usize,
    filter: SpeechFilter,
    directives: DirectiveFilter,
    /// Directivas pendientes, con su posición (byte) en `buffer`.
    markers: Vec<(usize, Tag)>,
    prosody: ProsodyState,
}

impl PhraseSplitter {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            max_chars: 160,
            filter: SpeechFilter::default(),
            directives: DirectiveFilter::default(),
            markers: Vec::new(),
            prosody: ProsodyState::default(),
        }
    }

    pub fn with_max_chars(mut self, max: usize) -> Self {
        self.max_chars = max;
        self
    }

    pub fn push(&mut self, text: &str) {
        let mut filtered = String::new();
        self.filter.push(text, &mut filtered);
        let mut pieces = Vec::new();
        self.directives.push(&filtered, &mut pieces);
        self.absorb(pieces);
    }

    fn absorb(&mut self, pieces: Vec<Piece>) {
        for piece in pieces {
            match piece {
                Piece::Text(text) => self.buffer.push_str(&text),
                Piece::Tag(tag) => self.markers.push((self.buffer.len(), tag)),
            }
        }
    }

    /// Como [`drain_directed`](Self::drain_directed), solo el texto.
    pub fn drain_available(&mut self) -> Vec<String> {
        self.drain_directed()
            .into_iter()
            .map(|phrase| phrase.text)
            .collect()
    }

    /// Como [`finish_directed`](Self::finish_directed), unido en un texto.
    pub fn finish(&mut self) -> Option<String> {
        let phrases = self.finish_directed();
        if phrases.is_empty() {
            return None;
        }
        let texts: Vec<String> = phrases.into_iter().map(|p| p.text).collect();
        Some(texts.join(" "))
    }

    /// Vacía el remanente al cerrar el stream. Solo corta donde una
    /// directiva de pausa, respiración o susurro lo exige.
    pub fn finish_directed(&mut self) -> Vec<DirectedPhrase> {
        let mut filtered = String::new();
        self.filter.finish(&mut filtered);
        let mut pieces = Vec::new();
        self.directives.push(&filtered, &mut pieces);
        self.directives.finish(&mut pieces);
        self.absorb(pieces);

        let mut cuts = self.boundary_cuts();
        cuts.push(self.buffer.len());
        let mut out = self.take_segments(&cuts);

        // Directivas al final del texto: una pausa alarga la última frase.
        let mut emphasis = false;
        for (_, tag) in std::mem::take(&mut self.markers) {
            self.prosody.apply(tag, &mut emphasis);
        }
        if let (Some(ms), Some(last)) = (self.prosody.take_pending_pause(), out.last_mut()) {
            last.directive.pause_after_ms = ms;
        }

        let max_chars = self.max_chars;
        *self = Self::new().with_max_chars(max_chars);
        out
    }

    /// Posiciones donde una directiva obliga a cortar la frase.
    fn boundary_cuts(&self) -> Vec<usize> {
        self.markers
            .iter()
            .filter(|(_, tag)| tag.is_boundary())
            .map(|(pos, _)| *pos)
            .collect()
    }

    /// Emite los segmentos `[corte anterior, corte)` como frases, aplicando
    /// las directivas de cada uno, y retira lo consumido del buffer.
    fn take_segments(&mut self, cuts: &[usize]) -> Vec<DirectedPhrase> {
        let mut out = Vec::with_capacity(cuts.len());
        let mut markers = std::mem::take(&mut self.markers).into_iter().peekable();
        let mut start = 0usize;
        for &cut in cuts {
            let mut emphasis = false;
            let mut had_tags = false;
            while let Some((_, tag)) = markers.next_if(|(pos, _)| *pos < cut) {
                self.prosody.apply(tag, &mut emphasis);
                had_tags = true;
            }
            let slice = self.buffer[start..cut].trim();
            if !slice.is_empty() && !slice.chars().any(char::is_alphanumeric) {
                // Solo puntuación (los puntos de un `...` que llegó partido):
                // no se pronuncia, pero alarga la pausa siguiente.
                self.prosody.absorb_punctuation(slice);
            } else if !slice.is_empty() {
                let text = if had_tags {
                    squeeze_spaces(slice)
                } else {
                    slice.to_string()
                };
                out.push(self.prosody.directed(&text, emphasis));
            }
            start = cut;
        }

        let rest = &self.buffer[start..];
        start += rest.len() - rest.trim_start().len();
        self.buffer.replace_range(..start, "");
        self.markers = markers
            .map(|(pos, tag)| (pos.saturating_sub(start), tag))
            .collect();
        out
    }

    /// Devuelve las frases completas disponibles y las retira del buffer.
    ///
    /// Si no hay puntuación pero el buffer ya supera `max_chars`, fuerza un
    /// corte por espacios para no bloquear la síntesis indefinidamente. Una
    /// pausa, respiración o susurro también corta donde aparece.
    pub fn drain_directed(&mut self) -> Vec<DirectedPhrase> {
        if self.buffer.is_empty() {
            return Vec::new();
        }

        let chars: Vec<(usize, char)> = self.buffer.char_indices().collect();
        let mut cuts: Vec<usize> = Vec::new();

        let mut i = 0usize;
        while i < chars.len() {
            let (_, c) = chars[i];
            if is_delimiter(c) && self.valid_end(&chars, i) {
                // El corte incluye el delimitador y las comillas de cierre que
                // vengan justo después (ej. `dijo "hola."`). No aplica a los
                // saltos de línea, donde la comilla suele abrir la línea nueva.
                let mut end = i + 1;
                if c != '\n' {
                    while end < chars.len() && is_quote(chars[end].1) {
                        end += 1;
                    }
                }
                let cut_byte = if end < chars.len() {
                    chars[end].0
                } else {
                    self.buffer.len()
                };
                cuts.push(cut_byte);

                i = end;
                while i < chars.len() && chars[i].1.is_whitespace() {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }

        // Sin puntuación y con el buffer ya largo, fuerza el corte por espacios.
        if cuts.is_empty() && self.buffer.len() >= self.max_chars {
            if let Some(byte) = self.space_cut(&chars) {
                cuts.push(byte);
            }
        }

        cuts.extend(self.boundary_cuts());
        if cuts.is_empty() {
            return Vec::new();
        }
        cuts.sort_unstable();
        cuts.dedup();
        self.take_segments(&cuts)
    }

    fn valid_end(&self, chars: &[(usize, char)], i: usize) -> bool {
        let c = chars[i].1;

        // El salto de línea es un separador fuerte por sí mismo.
        if c == '\n' {
            return true;
        }

        // Excepción numérica: no partir decimales ni separadores de miles
        // (ej. 3.14, 1.000).
        if c == '.' && i > 0 && i + 1 < chars.len() {
            let prev = chars[i - 1].1;
            let next = chars[i + 1].1;
            if prev.is_ascii_digit() && next.is_ascii_digit() {
                return false;
            }
        }

        // Excepción de abreviaturas: no partir en "Dr.", "Sra.", etc.
        if c == '.' && self.preceding_word_is_abbreviation(chars, i) {
            return false;
        }

        // Fin de frase si el delimitador va seguido de espacio, salto de línea,
        // comillas o fin del buffer.
        if i + 1 >= chars.len() {
            return true;
        }
        let next = chars[i + 1].1;
        next.is_whitespace() || is_quote(next)
    }

    fn preceding_word_is_abbreviation(&self, chars: &[(usize, char)], dot_idx: usize) -> bool {
        let mut start = dot_idx;
        while start > 0 && chars[start - 1].1.is_alphabetic() {
            start -= 1;
        }
        if start == dot_idx {
            return false;
        }

        let word: String = chars[start..dot_idx].iter().map(|(_, c)| *c).collect();
        let lower = word.to_lowercase();
        ABBREVIATIONS.contains(&lower.as_str())
    }

    fn space_cut(&self, chars: &[(usize, char)]) -> Option<usize> {
        // Ignora espacios iniciales para no producir frases vacías.
        let first_word = chars
            .iter()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(idx, _)| *idx)
            .unwrap_or(self.buffer.len());

        let mut last_before: Option<usize> = None;
        let mut first_after: Option<usize> = None;

        for (idx, c) in chars {
            if !c.is_whitespace() || *idx < first_word {
                continue;
            }
            if *idx < self.max_chars {
                last_before = Some(*idx);
            } else if first_after.is_none() {
                first_after = Some(*idx);
            }
        }

        last_before.or(first_after)
    }
}

impl Default for PhraseSplitter {
    fn default() -> Self {
        Self::new()
    }
}

fn is_delimiter(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | ';' | '\n')
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '«' | '»' | '“' | '”' | '‘' | '’')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_by_punctuation() {
        let mut splitter = PhraseSplitter::new();
        splitter.push("Hola. ¡Qué tal! ¿Bien? Adiós; hasta luego.");
        assert_eq!(
            splitter.drain_available(),
            vec!["Hola.", "¡Qué tal!", "¿Bien?", "Adiós;", "hasta luego."]
        );
        assert_eq!(splitter.finish(), None);
    }

    #[test]
    fn protects_decimals() {
        let mut splitter = PhraseSplitter::new();
        splitter.push("El valor es 3.14 y sube.");
        assert_eq!(splitter.drain_available(), vec!["El valor es 3.14 y sube."]);
    }

    #[test]
    fn protects_abbreviations() {
        let mut splitter = PhraseSplitter::new();
        splitter.push("El Dr. Gómez llegó.");
        assert_eq!(splitter.drain_available(), vec!["El Dr. Gómez llegó."]);
    }

    #[test]
    fn cuts_by_max_chars_without_punctuation() {
        let mut splitter = PhraseSplitter::new().with_max_chars(10);
        splitter.push("uno dos tres cuatro");
        assert_eq!(splitter.drain_available(), vec!["uno dos"]);
        assert_eq!(splitter.finish(), Some("tres cuatro".to_string()));
    }

    #[test]
    fn consumes_token_by_token() {
        let mut splitter = PhraseSplitter::new();

        splitter.push("H");
        splitter.push("ola");
        assert!(splitter.drain_available().is_empty());

        splitter.push(".");
        assert_eq!(splitter.drain_available(), vec!["Hola."]);

        splitter.push(" ");
        splitter.push("¿C");
        splitter.push("ómo");
        splitter.push(" estás");
        assert!(splitter.drain_available().is_empty());

        splitter.push("?");
        assert_eq!(splitter.drain_available(), vec!["¿Cómo estás?"]);
        assert_eq!(splitter.finish(), None);
    }

    #[test]
    fn finish_returns_remaining_and_clears() {
        let mut splitter = PhraseSplitter::new();
        assert_eq!(splitter.finish(), None);

        splitter.push("hola mundo");
        assert_eq!(splitter.finish(), Some("hola mundo".to_string()));
        assert_eq!(splitter.finish(), None);
    }

    /// Empuja `text` de a un carácter (peor caso de streaming) y devuelve
    /// todas las frases, incluido el remanente de `finish()`.
    fn speak_streamed(text: &str) -> Vec<String> {
        let mut splitter = PhraseSplitter::new();
        let mut out = Vec::new();
        for c in text.chars() {
            splitter.push(&c.to_string());
            out.extend(splitter.drain_available());
        }
        out.extend(splitter.finish());
        out
    }

    #[test]
    fn strips_thought_blocks_at_once() {
        let mut splitter = PhraseSplitter::new();
        splitter
            .push("<thought>The user wants Spotify. I should answer.</thought>Abriendo Spotify. ");
        splitter.push("<THINKING>\nmore reasoning.\n</THINKING>Listo.");
        assert_eq!(
            splitter.drain_available(),
            vec!["Abriendo Spotify.", "Listo."]
        );
        assert_eq!(splitter.finish(), None);
    }

    #[test]
    fn strips_thought_blocks_token_by_token() {
        let text = "Hola. <thinking>Let me think. The user asked.</thinking>Son las tres. \
                    <thought type=\"x\">Hidden.</thought>Chao.";
        assert_eq!(
            speak_streamed(text),
            vec!["Hola.", "Son las tres.", "Chao."]
        );
    }

    #[test]
    fn unclosed_thought_is_dropped_on_finish() {
        let mut splitter = PhraseSplitter::new();
        splitter.push("Claro. <thought>I need to");
        assert_eq!(splitter.drain_available(), vec!["Claro."]);
        splitter.push(" open the app");
        assert!(splitter.drain_available().is_empty());
        assert_eq!(splitter.finish(), None);

        assert_eq!(speak_streamed("Bueno <thinki"), vec!["Bueno"]);
    }

    #[test]
    fn keeps_angle_brackets_that_are_not_tags() {
        assert_eq!(
            speak_streamed("Si a <b y b> c, listo."),
            vec!["Si a <b y b> c, listo."]
        );
        assert_eq!(speak_streamed("tres <th"), vec!["tres <th"]);
        assert_eq!(
            speak_streamed("Cierre </thought>suelto."),
            vec!["Cierre suelto."]
        );
    }

    #[test]
    fn drops_cli_noise_lines() {
        let text = "[INFO] Loading model. Done.\njetski: no output produced\nHola, aquí estoy.";
        assert_eq!(speak_streamed(text), vec!["Hola, aquí estoy."]);

        let mut splitter = PhraseSplitter::new();
        splitter.push("jetski: no output produced");
        assert!(splitter.drain_available().is_empty());
        assert_eq!(splitter.finish(), None);

        // Solo al inicio de línea: en medio de una frase es texto normal.
        assert_eq!(
            speak_streamed("El log dice jetski: ok."),
            vec!["El log dice jetski: ok."]
        );
        assert_eq!(speak_streamed("jets"), vec!["jets"]);
    }

    // ── HU-45: directivas de prosodia ──────────────────────────────────

    use crate::tts::directive::{Mood, SENTENCE_PAUSE_MS};

    /// Como `speak_streamed`, con la directiva de cada frase.
    fn directed_streamed(text: &str) -> Vec<DirectedPhrase> {
        let mut splitter = PhraseSplitter::new();
        let mut out = Vec::new();
        for c in text.chars() {
            splitter.push(&c.to_string());
            out.extend(splitter.drain_directed());
        }
        out.extend(splitter.finish_directed());
        out
    }

    fn directed_whole(text: &str) -> Vec<DirectedPhrase> {
        let mut splitter = PhraseSplitter::new();
        splitter.push(text);
        let mut out = splitter.drain_directed();
        out.extend(splitter.finish_directed());
        out
    }

    const CONTRACT_EXAMPLE: &str = "[ánimo:entusiasta]¡Listo, ya quedó! [pausa:300][ánimo:calmo]\
        Dejé el desarrollo corriendo en segundo plano… te aviso cuando termine.";

    #[test]
    fn contract_example_strips_tags_and_attaches_directives() {
        for phrases in [
            directed_streamed(CONTRACT_EXAMPLE),
            directed_whole(CONTRACT_EXAMPLE),
        ] {
            let texts: Vec<&str> = phrases.iter().map(|p| p.text.as_str()).collect();
            assert_eq!(
                texts,
                vec![
                    "¡Listo, ya quedó!",
                    "Dejé el desarrollo corriendo en segundo plano… te aviso cuando termine."
                ]
            );
            let first = phrases[0].directive;
            assert_eq!(first.mood, Some(Mood::Entusiasta));
            assert_eq!((first.rate, first.pitch, first.volume), (8, 2, 5));
            assert_eq!(first.pause_before_ms, 0);

            let second = phrases[1].directive;
            assert_eq!(second.mood, Some(Mood::Calmo));
            assert_eq!(second.pause_before_ms, 300, "la pausa explícita manda");
            assert!(second.rate < 0);
        }
    }

    #[test]
    fn pause_and_breath_force_a_phrase_boundary() {
        let phrases = directed_streamed("Bueno [pausa:500] te cuento [respira]que ya está listo.");
        let texts: Vec<&str> = phrases.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, vec!["Bueno", "te cuento", "que ya está listo."]);
        assert_eq!(phrases[1].directive.pause_before_ms, 500);
        assert!(phrases[2].directive.breath);
        assert!(!phrases[1].directive.breath);
    }

    #[test]
    fn whisper_applies_only_inside_its_span() {
        let phrases = directed_streamed("Te digo algo: [susurro]es un secreto[/susurro] ¿vale?");
        let texts: Vec<&str> = phrases.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, vec!["Te digo algo:", "es un secreto", "¿vale?"]);
        assert_eq!(phrases[0].directive.volume, 0);
        assert!(phrases[1].directive.volume <= -20);
        assert_eq!(phrases[2].directive.volume, 0);
    }

    #[test]
    fn emphasis_is_stripped_without_splitting() {
        let phrases = directed_streamed("Eso es [énfasis]muy[/énfasis] importante.");
        assert_eq!(phrases.len(), 1);
        assert_eq!(phrases[0].text, "Eso es muy importante.");
        assert!(phrases[0].directive.volume > 0);
    }

    #[test]
    fn punctuation_heuristic_without_directives() {
        let phrases = directed_streamed("Hola. ¿Cómo estás? Bien... gracias.");
        let pauses: Vec<u32> = phrases
            .iter()
            .map(|p| p.directive.pause_before_ms)
            .collect();
        assert_eq!(pauses, vec![0, SENTENCE_PAUSE_MS, SENTENCE_PAUSE_MS, 400]);
        assert_eq!(phrases[1].directive.pitch, 2, "la pregunta sube el tono");
        // Los puntos sueltos de un `...` partido nunca se sintetizan solos.
        let texts: Vec<&str> = phrases.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, vec!["Hola.", "¿Cómo estás?", "Bien.", "gracias."]);
        assert_eq!(phrases[0].directive.rate, 0);
        assert!(phrases.iter().all(|p| p.directive.mood.is_none()));
    }

    #[test]
    fn unknown_and_broken_tags_never_reach_the_text() {
        assert_eq!(
            speak_streamed("[gritar]Hola [ánimo:raro]mundo. Fin [pausa:x]ya [respi"),
            vec!["Hola mundo.", "Fin ya"]
        );
        // Corchetes que no son directivas siguen siendo texto.
        assert_eq!(
            speak_streamed("El índice [1] vale."),
            vec!["El índice [1] vale."]
        );
    }

    #[test]
    fn trailing_directives_produce_no_phrase() {
        assert_eq!(
            speak_streamed("Listo. [pausa:400][respira]"),
            vec!["Listo."]
        );

        // Si el remanente sale en `finish`, la pausa final lo alarga.
        let mut splitter = PhraseSplitter::new();
        splitter.push("Listo");
        let mut tail = splitter.finish_directed();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].directive.pause_after_ms, 0);

        let mut splitter = PhraseSplitter::new();
        splitter.push("Listo, ya");
        splitter.push(" [pausa:400]");
        assert_eq!(
            splitter.drain_directed().len(),
            1,
            "la pausa corta la frase"
        );
        tail = splitter.finish_directed();
        assert!(tail.is_empty());
    }

    #[test]
    fn directives_coexist_with_thought_blocks_and_noise() {
        let text = "[INFO] cargando modelo\n<thought>pienso [ánimo:serio]</thought>\
                    [ánimo:alegre]¡Hola! [respira]Ya estoy.";
        let phrases = directed_streamed(text);
        let texts: Vec<&str> = phrases.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, vec!["¡Hola!", "Ya estoy."]);
        assert_eq!(phrases[0].directive.mood, Some(Mood::Alegre));
        assert!(phrases[1].directive.breath);
    }

    /// Fuzzing de integración: troceo aleatorio de respuestas con directivas
    /// válidas, rotas y desconocidas produce las mismas frases que el texto
    /// completo, y ninguna frase contiene corchetes de directiva.
    #[test]
    fn fuzz_chunked_streams_match_whole_and_never_leak() {
        const ATOMS: &[&str] = &[
            "[ánimo:alegre]",
            "[ritmo:lento]",
            "[pausa:250]",
            "[respira]",
            "[susurro]",
            "[/susurro]",
            "[énfasis]",
            "[/énfasis]",
            "[zzz]",
            "[ánim",
            "o:serio]",
            "Hola",
            " mundo",
            ". ",
            "¿sí? ",
            "… ",
            ", ",
            "Dr. Pérez",
            "\n",
        ];
        let mut seed: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..1_000 {
            let mut text = String::new();
            for _ in 0..next(14) + 1 {
                text.push_str(ATOMS[next(ATOMS.len())]);
            }
            let whole = directed_whole(&text);

            let chars: Vec<char> = text.chars().collect();
            let mut splitter = PhraseSplitter::new();
            let mut streamed = Vec::new();
            let mut i = 0;
            while i < chars.len() {
                let len = (next(6) + 1).min(chars.len() - i);
                splitter.push(&chars[i..i + len].iter().collect::<String>());
                streamed.extend(splitter.drain_directed());
                i += len;
            }
            streamed.extend(splitter.finish_directed());

            let join = |v: &[DirectedPhrase]| {
                v.iter()
                    .map(|p| p.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            assert_eq!(
                join(&streamed).split_whitespace().collect::<Vec<_>>(),
                join(&whole).split_whitespace().collect::<Vec<_>>(),
                "input: {text:?}"
            );
            for phrase in &streamed {
                let lower = phrase.text.to_lowercase();
                for leak in [
                    "ánimo:", "ritmo:", "pausa", "respira", "susurro", "énfasis", "[zzz]",
                ] {
                    assert!(
                        !lower.contains(leak),
                        "fuga de directiva {leak:?} en {:?} (input {text:?})",
                        phrase.text
                    );
                }
            }
        }
    }
}
