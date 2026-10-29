//! Directivas de prosodia emitidas por el LLM (HU-45, contrato §3.2).
//!
//! El LLM puede intercalar etiquetas compactas entre corchetes para guiar la
//! entonación:
//!
//! ```text
//! [ánimo:normal|alegre|serio|empático|entusiasta|calmo|curioso|asustado]
//! [ritmo:lento|normal|rápido]
//! [pausa:300] [respira] [énfasis]palabra[/énfasis] [susurro]…[/susurro]
//! ```
//!
//! [`DirectiveFilter`] las extrae del stream de forma incremental (una
//! etiqueta puede llegar partida entre varios tokens) y nunca las deja pasar
//! al texto hablado ni al HUD. Una etiqueta con forma de directiva pero
//! desconocida se descarta en silencio. [`ProsodyState`] acumula su efecto
//! y produce la [`SpeechDirective`] de cada frase.

/// Largo máximo (en caracteres) del contenido de una etiqueta: más allá, el
/// `[` se considera texto y no bloquea el streaming.
const MAX_TAG_CHARS: usize = 32;

/// Nombres de etiqueta conocidos, ya normalizados (minúsculas, sin tildes).
const TAG_NAMES: &[&str] = &[
    "animo", "ritmo", "pausa", "respira", "enfasis", "/enfasis", "susurro", "/susurro",
];

/// Pausa por defecto de `[pausa]` sin argumento.
const DEFAULT_PAUSE_MS: u32 = 300;
/// Techo de una pausa explícita: un número absurdo no congela la voz.
const MAX_PAUSE_MS: u32 = 2_000;

/// Pausa heurística tras una frase que termina en puntos suspensivos.
pub const ELLIPSIS_PAUSE_MS: u32 = 400;

/// Techo de cada desplazamiento de prosodia, aplicado **después** de sumar
/// ánimo, ritmo, susurro, énfasis y la subida de las preguntas.
///
/// Recorrido: primero ±30 % de ritmo y ±10 Hz de tono, que recortaba el ánimo
/// y sonaba plano; después ±50 Hz, que dejó de recortar y llevó la voz al
/// efecto ardilla. La tabla de [`Mood::offsets`] ya está **calibrada a oído**
/// (máximo +14 Hz) y queda muy por debajo de estos topes, que solo existen
/// como red de seguridad ante combinaciones absurdas (ánimo + ritmo + susurro
/// + pregunta + énfasis a la vez).
const MAX_RATE_PCT: i8 = 50;
const MAX_PITCH_HZ: i8 = 50;
/// El susurro resta 25 % de volumen, ningún ánimo suma más de 4 y el énfasis
/// suma 5: el techo real (9 %) no llega a este límite.
const MAX_VOLUME_MUTE: i8 = -40;
const MAX_VOLUME_BOOST: i8 = 20;
/// Pausa heurística tras un cierre de frase (`.`, `!`, `?`).
pub const SENTENCE_PAUSE_MS: u32 = 250;
/// Pausa heurística tras `;` o `:`.
pub const CLAUSE_PAUSE_MS: u32 = 120;

/// Ánimo de una frase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    /// Tono base, sin desplazamiento: la voz tal como suena de fábrica.
    ///
    /// Existe porque no había forma de pedir "habla normal": el modelo, al no
    /// tener un ánimo neutro, elegía `serio` para bajar el tono, y con una voz
    /// masculina grave (−18 Hz) eso sonaba a ultratumba.
    Normal,
    Alegre,
    Serio,
    Empatico,
    Entusiasta,
    Calmo,
    Curioso,
    /// HU-73: el usuario pedía hablar "asustado" y la voz no cambiaba, porque
    /// el ánimo ni siquiera existía en la tabla.
    Asustado,
}

impl Mood {
    fn parse(arg: &str) -> Option<Self> {
        Some(match arg {
            // Cuatro formas de pedir lo mismo: el LLM escribe "neutro" o
            // "plano" según el idioma con que venga la petición del usuario.
            "normal" | "neutro" | "neutral" | "plano" => Mood::Normal,
            "alegre" => Mood::Alegre,
            "serio" => Mood::Serio,
            "empatico" => Mood::Empatico,
            "entusiasta" => Mood::Entusiasta,
            "calmo" => Mood::Calmo,
            "curioso" => Mood::Curioso,
            "asustado" => Mood::Asustado,
            _ => return None,
        })
    }

    /// Desplazamiento `(ritmo %, tono Hz, volumen %)` sobre la voz base.
    ///
    /// Calibración natural para una voz masculina grave (es-US-AlonsoNeural /
    /// es-CL-LorenzoNeural). La tabla anterior llevaba el ánimo agudo a
    /// +40 Hz y +22 % —sonaba a ardilla— y el grave a −18 Hz —sonaba a
    /// ultratumba—. El recorrido actual mueve el tono como mucho ±14 Hz y el
    /// ritmo ±10 %, que se oyen con claridad sin cambiar el timbre de la voz.
    ///
    /// `pitch` va en Hz y `rate`/`volume` en %, que es lo que espera el
    /// `<prosody>` de `msedge-tts`. `Normal` es la identidad: deja la voz
    /// exactamente como viene de fábrica, y es el estado de partida de cada
    /// turno.
    pub fn offsets(self) -> (i8, i8, i8) {
        match self {
            Mood::Normal => (0, 0, 0),
            Mood::Asustado => (10, 14, 4),
            Mood::Entusiasta => (8, 10, 3),
            Mood::Alegre => (5, 7, 2),
            Mood::Curioso => (3, 6, 0),
            Mood::Serio => (-4, -5, 0),
            Mood::Calmo => (-6, -4, -3),
            Mood::Empatico => (-6, -4, -3),
        }
    }
}

/// Etiqueta reconocida en el stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Mood(Mood),
    /// Desplazamiento de ritmo en %, derivado de `[ritmo:…]`.
    Rhythm(i8),
    Pause(u32),
    Breath,
    EmphasisOpen,
    EmphasisClose,
    WhisperOpen,
    WhisperClose,
}

impl Tag {
    /// Las pausas, respiraciones y susurros solo pueden aplicarse a una
    /// frase entera: fuerzan un corte donde aparecen.
    pub fn is_boundary(self) -> bool {
        matches!(
            self,
            Tag::Pause(_) | Tag::Breath | Tag::WhisperOpen | Tag::WhisperClose
        )
    }

    /// Interpreta el contenido de una etiqueta (sin corchetes).
    fn parse(content: &str) -> Option<Self> {
        let norm = normalize(content);
        let (name, arg) = match norm.split_once(':') {
            Some((name, arg)) => (name, Some(arg)),
            None => (norm.as_str(), None),
        };
        match (name, arg) {
            ("animo", Some(arg)) => Mood::parse(arg).map(Tag::Mood),
            ("ritmo", Some("lento")) => Some(Tag::Rhythm(-12)),
            ("ritmo", Some("normal")) => Some(Tag::Rhythm(0)),
            ("ritmo", Some("rapido")) => Some(Tag::Rhythm(12)),
            ("pausa", None) => Some(Tag::Pause(DEFAULT_PAUSE_MS)),
            ("pausa", Some(arg)) => {
                let digits = arg.strip_suffix("ms").unwrap_or(arg);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let ms = digits.parse::<u32>().unwrap_or(MAX_PAUSE_MS);
                Some(Tag::Pause(ms.min(MAX_PAUSE_MS)))
            }
            ("respira", None) => Some(Tag::Breath),
            ("enfasis", None) => Some(Tag::EmphasisOpen),
            ("/enfasis", None) => Some(Tag::EmphasisClose),
            ("susurro", None) => Some(Tag::WhisperOpen),
            ("/susurro", None) => Some(Tag::WhisperClose),
            _ => None,
        }
    }
}

/// Minúsculas y sin tildes (el LLM puede escribir `ánimo` o `animo`).
fn normalize(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'ä' => 'a',
            'é' | 'è' | 'ë' => 'e',
            'í' | 'ì' | 'ï' => 'i',
            'ó' | 'ò' | 'ö' => 'o',
            'ú' | 'ù' | 'ü' => 'u',
            c => c,
        })
        .collect()
}

/// Carácter admisible dentro de una etiqueta. El primero debe ser una letra
/// o `/`: así `[1]` o `[ nota ]` siguen siendo texto.
fn is_tag_char(c: char, first: bool) -> bool {
    if first {
        c.is_alphabetic() || c == '/'
    } else {
        c.is_alphanumeric() || matches!(c, ':' | '/' | '_' | '-')
    }
}

/// `true` si un fragmento sin cerrar (al fin del stream) ya delata una
/// directiva rota y debe descartarse en lugar de pronunciarse.
fn looks_like_broken_directive(content: &str) -> bool {
    let norm = normalize(content);
    let name = norm.split(':').next().unwrap_or("");
    if norm.contains(':') {
        return TAG_NAMES.contains(&name);
    }
    name.trim_start_matches('/').chars().count() >= 2
        && TAG_NAMES.iter().any(|known| known.starts_with(name))
}

/// Trozo de salida del filtro: texto hablable o una directiva.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    Tag(Tag),
}

fn push_text(out: &mut Vec<Piece>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Piece::Text(last)) = out.last_mut() {
        last.push_str(text);
    } else {
        out.push(Piece::Text(text.to_string()));
    }
}

/// Extractor incremental de directivas.
///
/// Solo retiene texto mientras un `[` puede ser el inicio de una etiqueta
/// (como mucho [`MAX_TAG_CHARS`] caracteres); el resto pasa de inmediato.
#[derive(Debug, Default)]
pub struct DirectiveFilter {
    pending: String,
}

impl DirectiveFilter {
    pub fn push(&mut self, text: &str, out: &mut Vec<Piece>) {
        self.pending.push_str(text);
        self.process(out, false);
    }

    /// Cierra el stream: una etiqueta rota se descarta; un `[` suelto que no
    /// se parece a una directiva era texto.
    pub fn finish(&mut self, out: &mut Vec<Piece>) {
        self.process(out, true);
        self.pending.clear();
    }

    fn process(&mut self, out: &mut Vec<Piece>, eof: bool) {
        while !self.pending.is_empty() {
            let Some(open) = self.pending.find('[') else {
                push_text(out, &self.pending);
                self.pending.clear();
                return;
            };
            push_text(out, &self.pending[..open]);
            self.pending.drain(..open);

            // `pending` empieza con '['.
            let mut close = None;
            let mut aborted_at = None;
            for (n, (i, c)) in self.pending[1..].char_indices().enumerate() {
                if c == ']' {
                    close = Some(1 + i);
                    break;
                }
                if n >= MAX_TAG_CHARS || !is_tag_char(c, n == 0) {
                    aborted_at = Some(1 + i);
                    break;
                }
            }
            // Etiqueta interrumpida por un espacio u otro carácter: si ya
            // delataba una directiva (`[ánimo: `) se descarta lo acumulado.
            if let Some(end) = aborted_at {
                if looks_like_broken_directive(&self.pending[1..end]) {
                    self.pending.drain(..end);
                    continue;
                }
            }
            let literal = aborted_at.is_some();
            match close {
                Some(end) if end > 1 => {
                    if let Some(tag) = Tag::parse(&self.pending[1..end]) {
                        out.push(Piece::Tag(tag));
                    }
                    // Desconocida: se descarta sin pronunciarla.
                    self.pending.drain(..=end);
                }
                Some(_) => {
                    // `[]`: texto.
                    push_text(out, "[");
                    self.pending.drain(..1);
                }
                None if literal => {
                    push_text(out, "[");
                    self.pending.drain(..1);
                }
                None if !eof => return,
                None => {
                    if looks_like_broken_directive(&self.pending[1..]) {
                        self.pending.clear();
                        return;
                    }
                    push_text(out, "[");
                    self.pending.drain(..1);
                }
            }
        }
    }
}

/// Elimina todas las directivas de un texto completo (historial, tareas de
/// fondo, respuestas guardadas): lo que queda es lo que se diría en voz alta.
pub fn strip_directives(text: &str) -> String {
    let mut filter = DirectiveFilter::default();
    let mut pieces = Vec::new();
    filter.push(text, &mut pieces);
    filter.finish(&mut pieces);
    let mut out = String::with_capacity(text.len());
    for piece in pieces {
        if let Piece::Text(t) = piece {
            out.push_str(&t);
        }
    }
    squeeze_spaces(&out)
}

/// Colapsa los espacios dobles que deja una etiqueta retirada entre palabras.
pub fn squeeze_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for c in text.chars() {
        if c == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(c);
    }
    out
}

/// Prosodia resuelta de una frase.
///
/// `rate` y `volume` son desplazamientos en %, `pitch` en Hz, sobre la voz
/// base; las pausas son silencio local que se añade al PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpeechDirective {
    pub mood: Option<Mood>,
    pub rate: i8,
    pub pitch: i8,
    pub volume: i8,
    pub pause_before_ms: u32,
    pub pause_after_ms: u32,
    pub breath: bool,
    /// `pause_before_ms` viene de un `[pausa:N]` del LLM, no de la heurística
    /// de puntuación: la agrupación de frases (HU-50) nunca lo absorbe.
    pub explicit_pause: bool,
}

impl SpeechDirective {
    /// `true` si la frase se sintetiza con la voz base y sin silencios.
    pub fn is_neutral(&self) -> bool {
        self.rate == 0
            && self.pitch == 0
            && self.volume == 0
            && self.pause_before_ms == 0
            && self.pause_after_ms == 0
            && !self.breath
    }
}

/// Frase lista para sintetizar con su prosodia.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectedPhrase {
    pub text: String,
    pub directive: SpeechDirective,
}

impl DirectedPhrase {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            directive: SpeechDirective::default(),
        }
    }
}

/// Estado de prosodia de un turno: ánimo y ritmo persisten hasta que otra
/// directiva los cambia; pausas y respiraciones se aplican a la frase
/// siguiente.
///
/// Nace en [`Mood::Normal`], es decir, en la voz base. El segmentador se crea
/// por turno ([`crate::tts::PhraseSplitter::new`] en cada respuesta) y se
/// reinicia al cerrarlo, así que **ninguna emoción se arrastra al turno
/// siguiente**: ese arrastre era el otro síntoma del usuario, que tras pedir
/// "asustado" seguía oyendo la voz gravísima turnos después.
#[derive(Debug, Default)]
pub struct ProsodyState {
    mood: Option<Mood>,
    rhythm: i8,
    whisper: bool,
    pending_pause: Option<u32>,
    pending_breath: bool,
    /// Pausa heurística que dejó la frase anterior (por su puntuación).
    trailing_pause: u32,
    phrases: usize,
}

impl ProsodyState {
    /// Aplica una directiva; `emphasis` marca la frase en curso.
    pub fn apply(&mut self, tag: Tag, emphasis: &mut bool) {
        match tag {
            Tag::Mood(mood) => self.mood = Some(mood),
            Tag::Rhythm(rate) => self.rhythm = rate,
            Tag::Pause(ms) => {
                self.pending_pause = Some(self.pending_pause.unwrap_or(0).saturating_add(ms));
            }
            Tag::Breath => self.pending_breath = true,
            Tag::EmphasisOpen => *emphasis = true,
            Tag::EmphasisClose => {}
            Tag::WhisperOpen => self.whisper = true,
            Tag::WhisperClose => self.whisper = false,
        }
    }

    /// Un segmento sin palabras (`.`, `..`, `!`) no se sintetiza, pero su
    /// puntuación alarga la pausa tras la frase anterior.
    pub fn absorb_punctuation(&mut self, text: &str) {
        if self.phrases > 0 && text.chars().all(|c| c == '.' || c == '…') {
            self.trailing_pause = ELLIPSIS_PAUSE_MS;
        } else {
            self.trailing_pause = self.trailing_pause.max(punctuation_pause_ms(text));
        }
    }

    /// Pausa explícita aún sin frase a la que aplicarse (fin del turno).
    pub fn take_pending_pause(&mut self) -> Option<u32> {
        self.pending_pause.take()
    }

    /// Construye la frase y consume las pausas y respiraciones pendientes.
    pub fn directed(&mut self, text: &str, emphasis: bool) -> DirectedPhrase {
        // Sin directiva, el turno habla con la voz base: `Normal` es la
        // identidad, así que "no hay ánimo" y "[ánimo:normal]" dan lo mismo.
        let (mut rate, mut pitch, mut volume) = self.mood.unwrap_or(Mood::Normal).offsets();
        rate += self.rhythm;
        if self.whisper {
            rate -= 5;
            volume -= 25;
        }
        if emphasis {
            volume += 5;
        }
        // Las preguntas suben levemente el tono.
        if ends_with_any(text, &['?']) {
            pitch += 2;
        }

        // Primera frase sin pausa (latencia hasta el primer audio); después,
        // la pausa explícita manda sobre la heurística de puntuación.
        let explicit = self.pending_pause.take();
        let pause_before = match explicit {
            Some(ms) => ms,
            None if self.phrases == 0 => 0,
            None => self.trailing_pause,
        };
        let directive = SpeechDirective {
            mood: self.mood,
            rate: rate.clamp(-MAX_RATE_PCT, MAX_RATE_PCT),
            pitch: pitch.clamp(-MAX_PITCH_HZ, MAX_PITCH_HZ),
            volume: volume.clamp(MAX_VOLUME_MUTE, MAX_VOLUME_BOOST),
            pause_before_ms: pause_before,
            pause_after_ms: 0,
            breath: std::mem::take(&mut self.pending_breath),
            explicit_pause: explicit.is_some(),
        };
        self.trailing_pause = punctuation_pause_ms(text);
        self.phrases += 1;
        DirectedPhrase {
            text: text.to_string(),
            directive,
        }
    }
}

/// Último carácter significativo (ignora comillas y paréntesis de cierre).
fn ends_with_any(text: &str, set: &[char]) -> bool {
    text.trim_end_matches(['"', '\'', '»', '”', '’', ')'])
        .chars()
        .next_back()
        .is_some_and(|c| set.contains(&c))
}

/// Pausa natural que deja la puntuación final de una frase.
pub fn punctuation_pause_ms(text: &str) -> u32 {
    let core = text.trim_end_matches(['"', '\'', '»', '”', '’', ')']);
    if core.ends_with('…') || core.ends_with("...") {
        ELLIPSIS_PAUSE_MS
    } else if ends_with_any(core, &['.', '!', '?']) {
        SENTENCE_PAUSE_MS
    } else if ends_with_any(core, &[';', ':']) {
        CLAUSE_PAUSE_MS
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> Vec<Piece> {
        let mut filter = DirectiveFilter::default();
        let mut out = Vec::new();
        for chunk in chunks {
            filter.push(chunk, &mut out);
        }
        filter.finish(&mut out);
        out
    }

    fn text_of(pieces: &[Piece]) -> String {
        pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Text(t) => Some(t.as_str()),
                Piece::Tag(_) => None,
            })
            .collect()
    }

    fn tags_of(pieces: &[Piece]) -> Vec<Tag> {
        pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Tag(t) => Some(*t),
                Piece::Text(_) => None,
            })
            .collect()
    }

    /// Todas las particiones de a un carácter.
    fn char_chunks(text: &str) -> Vec<String> {
        text.chars().map(|c| c.to_string()).collect()
    }

    #[test]
    fn parses_every_valid_tag() {
        let cases = [
            ("[ánimo:alegre]", Tag::Mood(Mood::Alegre)),
            ("[animo:SERIO]", Tag::Mood(Mood::Serio)),
            ("[Ánimo:empático]", Tag::Mood(Mood::Empatico)),
            ("[ánimo:empatico]", Tag::Mood(Mood::Empatico)),
            ("[ánimo:entusiasta]", Tag::Mood(Mood::Entusiasta)),
            ("[ánimo:calmo]", Tag::Mood(Mood::Calmo)),
            ("[ánimo:curioso]", Tag::Mood(Mood::Curioso)),
            ("[ánimo:asustado]", Tag::Mood(Mood::Asustado)),
            ("[Ánimo:ASUSTADO]", Tag::Mood(Mood::Asustado)),
            ("[ánimo:normal]", Tag::Mood(Mood::Normal)),
            ("[ánimo:neutro]", Tag::Mood(Mood::Normal)),
            ("[Ánimo:NEUTRAL]", Tag::Mood(Mood::Normal)),
            ("[animo:plano]", Tag::Mood(Mood::Normal)),
            ("[ritmo:lento]", Tag::Rhythm(-12)),
            ("[ritmo:normal]", Tag::Rhythm(0)),
            ("[ritmo:rápido]", Tag::Rhythm(12)),
            ("[pausa:300]", Tag::Pause(300)),
            ("[pausa:450ms]", Tag::Pause(450)),
            ("[pausa]", Tag::Pause(DEFAULT_PAUSE_MS)),
            ("[pausa:999999999999]", Tag::Pause(MAX_PAUSE_MS)),
            ("[respira]", Tag::Breath),
            ("[énfasis]", Tag::EmphasisOpen),
            ("[/énfasis]", Tag::EmphasisClose),
            ("[susurro]", Tag::WhisperOpen),
            ("[/susurro]", Tag::WhisperClose),
        ];
        for (input, tag) in cases {
            let pieces = run(&[input]);
            assert_eq!(pieces, vec![Piece::Tag(tag)], "{input}");
        }
    }

    #[test]
    fn unknown_or_malformed_tags_are_dropped_silently() {
        for input in [
            "[ánimo:furioso]",
            "[ritmo:turbo]",
            "[pausa:abc]",
            "[pausa:-3]",
            "[respira:fuerte]",
            "[gritar]",
            "[foo:bar]",
            "[/xyz]",
        ] {
            let pieces = run(&["Hola ", input, "chao."]);
            assert!(tags_of(&pieces).is_empty(), "{input}");
            assert_eq!(text_of(&pieces), "Hola chao.", "{input}");
        }
    }

    #[test]
    fn brackets_that_are_not_tags_stay_as_text() {
        for text in [
            "El índice [1] es el primero.",
            "Una lista [ vacía ] queda.",
            "Corchetes [] vacíos.",
            "array[0] vale 3.",
            "Abro [ y no cierro.",
            "Texto con [una nota larga sin cerrar",
            "Cierre suelto ] aquí.",
        ] {
            let whole = run(&[text]);
            assert_eq!(text_of(&whole), text, "{text}");
            let chunks = char_chunks(text);
            let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
            assert_eq!(text_of(&run(&refs)), text, "streamed: {text}");
        }
    }

    #[test]
    fn tags_split_across_tokens_are_reassembled() {
        let pieces = run(&["Listo", " [ánim", "o:entu", "siasta", "]¡Ya", " quedó!"]);
        assert_eq!(tags_of(&pieces), vec![Tag::Mood(Mood::Entusiasta)]);
        assert_eq!(text_of(&pieces), "Listo ¡Ya quedó!");

        let text = "[ánimo:entusiasta]¡Listo, ya quedó! [pausa:300][ánimo:calmo]Dejé el \
                    desarrollo corriendo… [respira]te aviso.";
        let chunks = char_chunks(text);
        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let pieces = run(&refs);
        assert_eq!(
            tags_of(&pieces),
            vec![
                Tag::Mood(Mood::Entusiasta),
                Tag::Pause(300),
                Tag::Mood(Mood::Calmo),
                Tag::Breath,
            ]
        );
        assert_eq!(
            text_of(&pieces),
            "¡Listo, ya quedó! Dejé el desarrollo corriendo… te aviso."
        );
    }

    #[test]
    fn broken_tag_at_end_of_stream_is_dropped() {
        for tail in ["[ánimo:ale", "[pau", "[resp", "[/susu", "[ritmo:"] {
            let pieces = run(&["Hola ", tail]);
            assert_eq!(text_of(&pieces), "Hola ", "{tail}");
            assert!(tags_of(&pieces).is_empty(), "{tail}");
        }
        // Un `[` seguido de algo que no es una directiva era texto.
        assert_eq!(text_of(&run(&["Hola [xy"])), "Hola [xy");
        assert_eq!(text_of(&run(&["Hola ["])), "Hola [");
    }

    #[test]
    fn strip_directives_cleans_full_text() {
        assert_eq!(
            strip_directives("[ánimo:alegre]Hola [pausa:200] mundo. [énfasis]Ya[/énfasis] está."),
            "Hola mundo. Ya está."
        );
        assert_eq!(strip_directives("Sin directivas."), "Sin directivas.");
    }

    /// Generador pseudoaleatorio determinista para el fuzzing.
    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const FUZZ_ATOMS: &[&str] = &[
        "[ánimo:alegre]",
        "[ánimo:serio]",
        "[ánimo:asustado]",
        "[ritmo:rápido]",
        "[pausa:300]",
        "[respira]",
        "[énfasis]",
        "[/énfasis]",
        "[susurro]",
        "[/susurro]",
        "[desconocida]",
        "[ánimo:raro]",
        "Hola",
        " mundo",
        ". ",
        "¿qué tal? ",
        "ñandú",
        "… ",
        "[",
        "]",
        "[ánim",
        "o:",
        "calmo]",
        "[1]",
        " ",
    ];

    /// Fuzzing: cualquier mezcla de etiquetas válidas, rotas y texto, partida
    /// en trozos aleatorios, produce lo mismo que procesarla de una vez, y
    /// jamás deja un nombre de directiva en el texto hablado.
    #[test]
    fn fuzz_random_chunking_matches_whole_input_and_never_leaks_tags() {
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for _ in 0..2_000 {
            let mut text = String::new();
            for _ in 0..rng.below(12) + 1 {
                text.push_str(FUZZ_ATOMS[rng.below(FUZZ_ATOMS.len())]);
            }
            let whole = run(&[&text]);

            let chars: Vec<char> = text.chars().collect();
            let mut chunks = Vec::new();
            let mut i = 0;
            while i < chars.len() {
                let len = (rng.below(5) + 1).min(chars.len() - i);
                chunks.push(chars[i..i + len].iter().collect::<String>());
                i += len;
            }
            let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
            let streamed = run(&refs);

            assert_eq!(text_of(&streamed), text_of(&whole), "input: {text:?}");
            assert_eq!(tags_of(&streamed), tags_of(&whole), "input: {text:?}");

            let spoken = normalize(&text_of(&streamed));
            for name in ["animo:", "ritmo:", "pausa", "respira", "enfasis", "susurro"] {
                assert!(
                    !spoken.contains(name),
                    "fuga de directiva {name:?} en {spoken:?} (input {text:?})"
                );
            }
        }
    }

    #[test]
    fn prosody_state_resolves_mood_rhythm_and_pauses() {
        let mut state = ProsodyState::default();
        let mut emphasis = false;
        state.apply(Tag::Mood(Mood::Entusiasta), &mut emphasis);
        let first = state.directed("¡Listo, ya quedó!", emphasis);
        assert_eq!(first.directive.rate, 8);
        assert_eq!(first.directive.pitch, 10);
        assert_eq!(first.directive.volume, 3);
        assert_eq!(
            first.directive.pause_before_ms, 0,
            "primera frase sin pausa"
        );

        // Heurística de puntuación sin directiva de pausa.
        let second = state.directed("Ahora sigo…", false);
        assert_eq!(second.directive.pause_before_ms, SENTENCE_PAUSE_MS);
        assert_eq!(
            second.directive.mood,
            Some(Mood::Entusiasta),
            "ánimo persiste"
        );
        let third = state.directed("Y termino.", false);
        assert_eq!(third.directive.pause_before_ms, ELLIPSIS_PAUSE_MS);

        // Pausa explícita y respiración mandan sobre la heurística.
        state.apply(Tag::Pause(600), &mut emphasis);
        state.apply(Tag::Breath, &mut emphasis);
        state.apply(Tag::Mood(Mood::Empatico), &mut emphasis);
        state.apply(Tag::Rhythm(-12), &mut emphasis);
        state.apply(Tag::WhisperOpen, &mut emphasis);
        let fourth = state.directed("¿Estás bien?", false);
        assert_eq!(fourth.directive.pause_before_ms, 600);
        assert!(fourth.directive.breath);
        assert_eq!(fourth.directive.rate, -6 - 12 - 5);
        assert_eq!(fourth.directive.pitch, -4 + 2, "la pregunta sube el tono");
        assert_eq!(fourth.directive.volume, -3 - 25);
        let fifth = state.directed("Sigo.", false);
        assert!(!fifth.directive.breath, "la respiración se consume");
    }

    // ------------------------------------------------------------- HU-73

    /// La tabla está **calibrada a oído** para una voz masculina grave: los
    /// ánimos se distinguen entre sí y del neutro, pero sin el efecto ardilla
    /// del ánimo agudo anterior (+40 Hz, +22 %) ni el efecto ultratumba del
    /// grave (−18 Hz), que dejaba la voz "tipo Thanos".
    #[test]
    fn mood_offsets_are_natural_and_ordered_hu73() {
        let expected = [
            (Mood::Normal, (0, 0, 0)),
            (Mood::Asustado, (10, 14, 4)),
            (Mood::Entusiasta, (8, 10, 3)),
            (Mood::Alegre, (5, 7, 2)),
            (Mood::Curioso, (3, 6, 0)),
            (Mood::Serio, (-4, -5, 0)),
            (Mood::Calmo, (-6, -4, -3)),
            (Mood::Empatico, (-6, -4, -3)),
        ];
        for (mood, offsets) in expected {
            assert_eq!(mood.offsets(), offsets, "{mood:?}");
        }

        // El neutro es la identidad exacta: la voz de fábrica, sin tocar.
        assert_eq!(
            Mood::Normal.offsets(),
            (0, 0, 0),
            "el ánimo normal no puede mover nada"
        );

        // Del más agudo al más grave sin saltos: si el orden se rompe, dos
        // ánimos distintos vuelven a sonar igual.
        let by_pitch = [
            Mood::Asustado,
            Mood::Entusiasta,
            Mood::Alegre,
            Mood::Curioso,
            Mood::Normal,
            Mood::Calmo,
            Mood::Serio,
        ]
        .map(|mood| mood.offsets().1);
        assert!(
            by_pitch.windows(2).all(|pair| pair[0] > pair[1]),
            "el orden del tono se rompió: {by_pitch:?}"
        );

        // Banda humana: ±15 Hz de tono y ±10 % de ritmo es lo máximo que
        // admite la voz sin que el timbre deje de reconocerse.
        for mood in [
            Mood::Normal,
            Mood::Alegre,
            Mood::Serio,
            Mood::Empatico,
            Mood::Entusiasta,
            Mood::Calmo,
            Mood::Curioso,
            Mood::Asustado,
        ] {
            let (rate, pitch, volume) = mood.offsets();
            assert!(pitch.abs() <= 15, "{mood:?} mueve el tono {pitch} Hz");
            assert!(rate.abs() <= 10, "{mood:?} mueve el ritmo {rate} %");
            assert!(volume.abs() <= 5, "{mood:?} mueve el volumen {volume} %");
        }

        // Y ningún ánimo salvo el neutro se queda sin efecto.
        for mood in [
            Mood::Alegre,
            Mood::Serio,
            Mood::Empatico,
            Mood::Entusiasta,
            Mood::Calmo,
            Mood::Curioso,
            Mood::Asustado,
        ] {
            assert_ne!(mood.offsets(), (0, 0, 0), "{mood:?} no se oye");
        }
    }

    /// El techo de [`ProsodyState::directed`] no puede recortar la tabla: con
    /// el límite antiguo (±10 Hz) asustado y entusiasta salían los dos a
    /// +10 Hz y seguían sonando iguales.
    #[test]
    fn mood_offsets_survive_the_clamp_hu71() {
        for mood in [
            Mood::Alegre,
            Mood::Serio,
            Mood::Empatico,
            Mood::Entusiasta,
            Mood::Calmo,
            Mood::Curioso,
            Mood::Asustado,
        ] {
            let mut state = ProsodyState::default();
            let mut emphasis = false;
            state.apply(Tag::Mood(mood), &mut emphasis);
            // Una pregunta suma 2 Hz más y tampoco debe recortarse.
            let phrase = state.directed("¿En serio?", emphasis);
            let (rate, pitch, volume) = mood.offsets();
            assert_eq!(phrase.directive.rate, rate, "{mood:?}");
            assert_eq!(phrase.directive.pitch, pitch + 2, "{mood:?}");
            assert_eq!(phrase.directive.volume, volume, "{mood:?}");
            assert_ne!(phrase.directive.pitch, 0, "{mood:?} se quedó neutro");
        }
        // El neutro tampoco se recorta: solo lo mueve la entonación de la
        // pregunta, que es puntuación, no ánimo.
        let mut state = ProsodyState::default();
        let mut emphasis = false;
        state.apply(Tag::Mood(Mood::Normal), &mut emphasis);
        let phrase = state.directed("¿En serio?", emphasis);
        assert_eq!((phrase.directive.rate, phrase.directive.pitch), (0, 2));
        let plain = state.directed("Vale.", emphasis);
        assert_eq!(plain.directive.pitch, 0, "sin pregunta vuelve al tono base");
    }

    /// HU-73: existe el ánimo neutro, se pide con cuatro nombres y devuelve la
    /// voz a su tono base después de una emoción.
    #[test]
    fn normal_mood_returns_to_the_base_voice_hu73() {
        for alias in [
            "[ánimo:normal]",
            "[ánimo:neutro]",
            "[ánimo:neutral]",
            "[ánimo:plano]",
            "[Ánimo:NORMAL]",
        ] {
            assert_eq!(
                run(&[alias]),
                vec![Piece::Tag(Tag::Mood(Mood::Normal))],
                "{alias}"
            );
            // Y nunca se pronuncia.
            assert_eq!(strip_directives(&format!("{alias}Hola.")), "Hola.");
        }

        // Un turno que empieza en normal suena con la voz de fábrica.
        let mut fresh = ProsodyState::default();
        let mut emphasis = false;
        fresh.apply(Tag::Mood(Mood::Normal), &mut emphasis);
        let first = fresh.directed("Hola.", emphasis);
        assert_eq!(
            (
                first.directive.rate,
                first.directive.pitch,
                first.directive.volume
            ),
            (0, 0, 0)
        );
        assert!(first.directive.is_neutral(), "el neutro no debe sonar raro");

        // Y deshace la emoción anterior dentro del mismo turno: era el fallo
        // que dejaba la voz cavernosa cuando el usuario pedía "habla normal".
        let mut state = ProsodyState::default();
        let mut emphasis = false;
        state.apply(Tag::Mood(Mood::Asustado), &mut emphasis);
        let afraid = state.directed("¡Cuidado!", emphasis);
        assert_eq!(afraid.directive.pitch, 14);
        state.apply(Tag::Mood(Mood::Normal), &mut emphasis);
        let back = state.directed("No era nada.", emphasis);
        assert_eq!(back.directive.mood, Some(Mood::Normal));
        assert_eq!(
            (
                back.directive.rate,
                back.directive.pitch,
                back.directive.volume
            ),
            (0, 0, 0),
            "el ánimo normal tiene que devolver la voz base"
        );
    }

    /// Cada turno nace en neutro: el segmentador se crea por turno, así que la
    /// emoción de la respuesta anterior no se arrastra a la siguiente.
    #[test]
    fn every_turn_starts_from_a_neutral_state_hu73() {
        let mut previous = ProsodyState::default();
        let mut emphasis = false;
        previous.apply(Tag::Mood(Mood::Serio), &mut emphasis);
        let grim = previous.directed("Esto es grave.", emphasis);
        assert_eq!(grim.directive.pitch, -5);

        let mut next_turn = ProsodyState::default();
        let plain = next_turn.directed("Listo, ya está.", false);
        assert_eq!(plain.directive.mood, None, "el turno nuevo no hereda ánimo");
        assert_eq!(
            (
                plain.directive.rate,
                plain.directive.pitch,
                plain.directive.volume
            ),
            (0, 0, 0)
        );
    }

    /// Lo que el usuario pidió por voz se traduce en una directiva real, no en
    /// texto: "[ánimo:asustado]" nunca se pronuncia.
    #[test]
    fn fear_reaches_the_directive_and_never_the_text_hu71() {
        let pieces = run(&["[ánimo:asustado]¡Cuidado, se cayó el servidor!"]);
        assert_eq!(tags_of(&pieces), vec![Tag::Mood(Mood::Asustado)]);
        assert_eq!(text_of(&pieces), "¡Cuidado, se cayó el servidor!");
        let mut state = ProsodyState::default();
        let mut emphasis = false;
        state.apply(Tag::Mood(Mood::Asustado), &mut emphasis);
        let phrase = state.directed("¡Cuidado!", emphasis);
        assert_eq!(phrase.directive.mood, Some(Mood::Asustado));
        assert_eq!(phrase.directive.pitch, 14, "agitado, no ardilla");
        assert_eq!(phrase.directive.rate, 10);
    }

    #[test]
    fn punctuation_pause_table() {
        assert_eq!(punctuation_pause_ms("Hola."), SENTENCE_PAUSE_MS);
        assert_eq!(punctuation_pause_ms("¿Sí?"), SENTENCE_PAUSE_MS);
        assert_eq!(punctuation_pause_ms("dijo \"ya.\""), SENTENCE_PAUSE_MS);
        assert_eq!(punctuation_pause_ms("Bueno…"), ELLIPSIS_PAUSE_MS);
        assert_eq!(punctuation_pause_ms("Bueno..."), ELLIPSIS_PAUSE_MS);
        assert_eq!(punctuation_pause_ms("Uno;"), CLAUSE_PAUSE_MS);
        assert_eq!(punctuation_pause_ms("sin cierre"), 0);
    }
}
