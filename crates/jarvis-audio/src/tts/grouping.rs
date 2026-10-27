//! Continuidad de entonación (HU-50, H-V3).
//!
//! Edge-TTS entona cada request como una unidad: si cada frase va por
//! separado, la curva melódica se reinicia y la respuesta suena a lista
//! cortada. [`PhraseGrouper`] deja salir sola la **primera** frase (para no
//! subir el tiempo hasta el primer audio) y junta las siguientes en un solo
//! request mientras compartan la misma voz (ánimo, ritmo, tono y volumen).
//!
//! Una pausa explícita (`[pausa:N]`), una respiración o unos puntos
//! suspensivos siempre abren un grupo nuevo: ese silencio es local y Edge no
//! lo reproduciría dentro del párrafo. La pausa heurística de un `.` sí se
//! absorbe, porque Edge ya respira en la puntuación.

use std::time::{Duration, Instant};

use super::directive::{DirectedPhrase, SpeechDirective, SENTENCE_PAUSE_MS};

/// Máximo de frases por request.
pub const MAX_GROUP_PHRASES: usize = 3;
/// Máximo de caracteres por request (unas 2-3 frases normales).
pub const MAX_GROUP_CHARS: usize = 320;
/// Espera máxima de un grupo abierto a que llegue la frase siguiente: con el
/// LLM lento, la frase sale sola antes de que se agote el audio anterior.
pub const MAX_GROUP_WAIT: Duration = Duration::from_millis(700);

/// `true` si `next` puede sintetizarse en el mismo request que `group`.
pub fn can_join(group: &SpeechDirective, next: &SpeechDirective) -> bool {
    group.mood == next.mood
        && group.rate == next.rate
        && group.pitch == next.pitch
        && group.volume == next.volume
        && group.pause_after_ms == 0
        && !next.breath
        && !next.explicit_pause
        && next.pause_before_ms <= SENTENCE_PAUSE_MS
}

#[derive(Debug)]
struct OpenGroup {
    phrase: DirectedPhrase,
    count: usize,
    opened: Instant,
}

/// Agrupador incremental de frases dirigidas.
#[derive(Debug)]
pub struct PhraseGrouper {
    first_sent: bool,
    open: Option<OpenGroup>,
    max_phrases: usize,
    max_chars: usize,
    max_wait: Duration,
}

impl PhraseGrouper {
    pub fn new() -> Self {
        Self {
            first_sent: false,
            open: None,
            max_phrases: MAX_GROUP_PHRASES,
            max_chars: MAX_GROUP_CHARS,
            max_wait: MAX_GROUP_WAIT,
        }
    }

    /// Sin agrupación: cada frase sale tal cual (comportamiento previo).
    pub fn disabled() -> Self {
        Self {
            max_phrases: 1,
            ..Self::new()
        }
    }

    pub fn with_max_wait(mut self, wait: Duration) -> Self {
        self.max_wait = wait;
        self
    }

    /// Recibe una frase del segmentador y devuelve las que ya pueden
    /// sintetizarse, en orden.
    pub fn push(&mut self, phrase: DirectedPhrase, now: Instant) -> Vec<DirectedPhrase> {
        if !self.first_sent || self.max_phrases <= 1 {
            self.first_sent = true;
            let mut out: Vec<DirectedPhrase> = self.flush().into_iter().collect();
            out.push(phrase);
            return out;
        }

        let mut out = Vec::new();
        match self.open.take() {
            Some(mut group) if self.fits(&group, &phrase) => {
                group.phrase.text.push(' ');
                group.phrase.text.push_str(&phrase.text);
                group.phrase.directive.pause_after_ms = phrase.directive.pause_after_ms;
                group.count += 1;
                if group.count >= self.max_phrases {
                    out.push(group.phrase);
                } else {
                    self.open = Some(group);
                }
            }
            previous => {
                out.extend(previous.map(|group| group.phrase));
                self.open = Some(OpenGroup {
                    phrase,
                    count: 1,
                    opened: now,
                });
            }
        }
        out
    }

    /// Libera el grupo abierto si ya esperó demasiado a la frase siguiente.
    pub fn poll(&mut self, now: Instant) -> Option<DirectedPhrase> {
        let expired = self
            .open
            .as_ref()
            .is_some_and(|group| now.saturating_duration_since(group.opened) >= self.max_wait);
        if expired {
            self.flush()
        } else {
            None
        }
    }

    /// Libera el grupo abierto (fin del stream).
    pub fn flush(&mut self) -> Option<DirectedPhrase> {
        self.open.take().map(|group| group.phrase)
    }

    /// `true` si hay frases retenidas esperando grupo.
    pub fn has_pending(&self) -> bool {
        self.open.is_some()
    }

    fn fits(&self, group: &OpenGroup, next: &DirectedPhrase) -> bool {
        group.count < self.max_phrases
            && group.phrase.text.chars().count() + 1 + next.text.chars().count() <= self.max_chars
            && can_join(&group.phrase.directive, &next.directive)
    }
}

impl Default for PhraseGrouper {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tts::directive::Mood;
    use crate::tts::PhraseSplitter;

    fn phrase(text: &str, directive: SpeechDirective) -> DirectedPhrase {
        DirectedPhrase {
            text: text.to_string(),
            directive,
        }
    }

    fn sentence() -> SpeechDirective {
        SpeechDirective {
            pause_before_ms: SENTENCE_PAUSE_MS,
            ..Default::default()
        }
    }

    /// Segmenta y agrupa `text` de una vez, sin esperas.
    fn grouped(text: &str) -> Vec<DirectedPhrase> {
        let mut splitter = PhraseSplitter::new();
        let mut grouper = PhraseGrouper::new();
        let now = Instant::now();
        splitter.push(text);
        let mut out = Vec::new();
        for p in splitter.drain_directed() {
            out.extend(grouper.push(p, now));
        }
        for p in splitter.finish_directed() {
            out.extend(grouper.push(p, now));
        }
        out.extend(grouper.flush());
        out
    }

    #[test]
    fn first_phrase_goes_alone_and_immediately() {
        let mut grouper = PhraseGrouper::new();
        let now = Instant::now();
        let out = grouper.push(phrase("Hola.", SpeechDirective::default()), now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Hola.");
        assert!(!grouper.has_pending());
    }

    #[test]
    fn following_phrases_with_same_voice_share_a_request() {
        let out = grouped("Listo. Revisé el repositorio. Todo compila. Los tests pasan. Fin.");
        let texts: Vec<&str> = out.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Listo.",
                "Revisé el repositorio. Todo compila. Los tests pasan.",
                "Fin."
            ]
        );
    }

    #[test]
    fn group_keeps_the_directive_of_its_first_phrase() {
        let mut grouper = PhraseGrouper::new();
        let now = Instant::now();
        grouper.push(phrase("Uno.", SpeechDirective::default()), now);
        let calm = SpeechDirective {
            mood: Some(Mood::Calmo),
            rate: -6,
            pause_before_ms: SENTENCE_PAUSE_MS,
            ..Default::default()
        };
        assert!(grouper.push(phrase("Dos.", calm), now).is_empty());
        assert!(grouper.push(phrase("Tres.", calm), now).is_empty());
        let group = grouper.flush().expect("grupo abierto");
        assert_eq!(group.text, "Dos. Tres.");
        assert_eq!(group.directive, calm);
    }

    #[test]
    fn mood_change_closes_the_group() {
        let out = grouped(
            "[ánimo:entusiasta]¡Listo! Ya quedó. Funciona. [ánimo:calmo]Te aviso. Cuando termine.",
        );
        let texts: Vec<&str> = out.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "¡Listo!",
                "Ya quedó. Funciona.",
                "Te aviso. Cuando termine."
            ]
        );
        assert_eq!(out[1].directive.mood, Some(Mood::Entusiasta));
        assert_eq!(out[2].directive.mood, Some(Mood::Calmo));
    }

    #[test]
    fn explicit_pause_breath_and_ellipsis_open_a_new_group() {
        let out = grouped("Hola. Uno. [pausa:200]Dos. Tres. [respira]Cuatro. Cinco... Seis.");
        let texts: Vec<&str> = out.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            ["Hola.", "Uno.", "Dos. Tres.", "Cuatro. Cinco...", "Seis."]
        );
        assert!(out[2].directive.explicit_pause);
        assert_eq!(out[2].directive.pause_before_ms, 200);
        assert!(out[3].directive.breath);
        // Los puntos suspensivos conservan su silencio local.
        assert_eq!(out[4].directive.pause_before_ms, 400);
    }

    #[test]
    fn question_pitch_is_not_flattened_by_grouping() {
        let out = grouped("Hola. Revisé todo. ¿Quieres que siga? Bien.");
        let texts: Vec<&str> = out.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            ["Hola.", "Revisé todo.", "¿Quieres que siga?", "Bien."]
        );
        assert_eq!(out[2].directive.pitch, 2);
    }

    #[test]
    fn group_is_capped_by_phrases_and_chars() {
        let mut grouper = PhraseGrouper::new();
        let now = Instant::now();
        grouper.push(phrase("Inicio.", SpeechDirective::default()), now);
        let mut out = Vec::new();
        for i in 0..7 {
            out.extend(grouper.push(phrase(&format!("Frase {i}."), sentence()), now));
        }
        out.extend(grouper.flush());
        let counts: Vec<usize> = out
            .iter()
            .map(|p| p.text.matches("Frase").count())
            .collect();
        assert_eq!(counts, [3, 3, 1]);

        let long = "x".repeat(200);
        let mut grouper = PhraseGrouper::new();
        grouper.push(phrase("Inicio.", SpeechDirective::default()), now);
        assert!(grouper.push(phrase(&long, sentence()), now).is_empty());
        let out = grouper.push(phrase(&long, sentence()), now);
        assert_eq!(out.len(), 1, "200 + 200 supera el techo de caracteres");
        assert!(grouper.has_pending());
    }

    #[test]
    fn open_group_is_released_after_max_wait() {
        let mut grouper = PhraseGrouper::new();
        let t0 = Instant::now();
        grouper.push(phrase("Uno.", SpeechDirective::default()), t0);
        assert!(grouper.push(phrase("Dos.", sentence()), t0).is_empty());
        assert!(grouper.poll(t0 + MAX_GROUP_WAIT / 2).is_none());
        let released = grouper
            .poll(t0 + MAX_GROUP_WAIT)
            .expect("liberado por tiempo");
        assert_eq!(released.text, "Dos.");
        assert!(!grouper.has_pending());
        assert!(grouper.poll(t0 + MAX_GROUP_WAIT * 2).is_none());
    }

    #[test]
    fn trailing_pause_after_is_kept_on_the_group() {
        let mut grouper = PhraseGrouper::new();
        let now = Instant::now();
        grouper.push(phrase("Hola.", SpeechDirective::default()), now);
        assert!(grouper.push(phrase("Ya está.", sentence()), now).is_empty());
        let tail = SpeechDirective {
            pause_after_ms: 500,
            ..sentence()
        };
        assert!(grouper.push(phrase("Listo", tail), now).is_empty());
        let group = grouper.flush().unwrap();
        assert_eq!(group.text, "Ya está. Listo");
        assert_eq!(group.directive.pause_after_ms, 500);
        // Con silencio final, el grupo ya no admite más frases.
        let mut grouper = PhraseGrouper::new();
        grouper.push(phrase("Hola.", SpeechDirective::default()), now);
        grouper.push(phrase("Listo", tail), now);
        let out = grouper.push(phrase("Otra.", sentence()), now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Listo");
    }

    #[test]
    fn disabled_grouper_passes_every_phrase_through() {
        let mut grouper = PhraseGrouper::disabled();
        let now = Instant::now();
        for text in ["Uno.", "Dos.", "Tres."] {
            let out = grouper.push(phrase(text, sentence()), now);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].text, text);
        }
        assert!(grouper.flush().is_none());
    }

    #[test]
    fn grouping_never_loses_or_reorders_text() {
        let text = "[ánimo:alegre]¡Hola! Te cuento. [pausa:300]Revisé tres cosas: el build, \
                    los tests y el lint. [ánimo:serio]Hay un warning… ¿Lo arreglo? \
                    [respira]Dime nomás. Quedo atento.";
        let mut splitter = PhraseSplitter::new();
        splitter.push(text);
        let mut plain: Vec<String> = splitter
            .drain_directed()
            .into_iter()
            .map(|p| p.text)
            .collect();
        plain.extend(splitter.finish_directed().into_iter().map(|p| p.text));
        let joined: Vec<String> = grouped(text).into_iter().map(|p| p.text).collect();
        assert_eq!(joined.join(" "), plain.join(" "));
        assert!(joined.len() < plain.len(), "debe agrupar algo");
    }
}
