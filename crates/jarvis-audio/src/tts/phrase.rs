//! Segmentador incremental de frases para streaming LLM (HU-8).
//!
//! Acumula texto token a token y emite frases completas en cuanto detecta un
//! delimitador de fin de frase, de modo que la síntesis de voz puede arrancar
//! mientras el resto de la respuesta sigue generándose.

/// Abreviaturas comunes en español que no deben partir la frase.
const ABBREVIATIONS: &[&str] = &[
    "sr", "sra", "dr", "dra", "d", "dna", "prof", "ej", "etc", "ud", "uds", "art", "num", "pag",
    "vol", "aprox",
];

/// Segmentador incremental de frases para streaming LLM.
#[derive(Debug)]
pub struct PhraseSplitter {
    buffer: String,
    max_chars: usize,
}

impl PhraseSplitter {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            max_chars: 160,
        }
    }

    pub fn with_max_chars(mut self, max: usize) -> Self {
        self.max_chars = max;
        self
    }

    pub fn push(&mut self, text: &str) {
        self.buffer.push_str(text);
    }

    /// Devuelve las frases completas disponibles y las retira del buffer.
    ///
    /// Si no hay puntuación pero el buffer ya supera `max_chars`, fuerza un
    /// corte por espacios para no bloquear la síntesis indefinidamente.
    pub fn drain_available(&mut self) -> Vec<String> {
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

        if cuts.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(cuts.len());
        let mut start = 0usize;
        for cut in cuts {
            let slice = self.buffer[start..cut].trim();
            if !slice.is_empty() {
                out.push(slice.to_string());
            }

            start = cut;
            while start < self.buffer.len() {
                let ch = self.buffer[start..]
                    .chars()
                    .next()
                    .expect("byte en límite char");
                if ch.is_whitespace() {
                    start += ch.len_utf8();
                } else {
                    break;
                }
            }
        }

        self.buffer.replace_range(..start, "");
        out
    }

    /// Vacía y retorna el remanente pendiente, si lo hay.
    pub fn finish(&mut self) -> Option<String> {
        let trimmed = self.buffer.trim();
        let out = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        self.buffer.clear();
        out
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
}
