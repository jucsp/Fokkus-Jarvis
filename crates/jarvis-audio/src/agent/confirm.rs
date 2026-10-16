//! Confirmación vocal obligatoria de las acciones `High` (HU-39 §2.3).
//!
//! Protocolo: Jarvis lee destinatario + contenido exacto, abre la ventana de
//! seguimiento (8 s, sin wake-word) e interpreta la respuesta:
//! afirmativa → actuar (tras re-verificar); negativa → no se envía y el
//! borrador queda; edición → se re-redacta y se vuelve a preguntar; ambigua o
//! silencio → se re-pregunta UNA vez y luego se aborta sin actuar.

/// Respuesta del usuario a una confirmación.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmationReply {
    Yes,
    No,
    /// "cámbialo a 'llego a las 9'" → nuevo contenido.
    Edit(String),
    Unclear,
    Timeout,
}

/// Lo que va a hacer la acción crítica, para leerlo en voz alta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CriticalAction {
    SendMessage {
        app: String,
        recipient: String,
        content: String,
    },
    DeleteFiles {
        what: String,
    },
    RunCommand {
        command: String,
    },
    Purchase {
        what: String,
        amount: Option<String>,
    },
    Other {
        summary: String,
    },
}

/// Más palabras que esto se resumen al leerlas (§2.3, paso 1).
const MAX_SPOKEN_WORDS: usize = 25;

fn spoken_content(content: &str) -> String {
    let words: Vec<&str> = content.split_whitespace().collect();
    if words.len() <= MAX_SPOKEN_WORDS {
        words.join(" ")
    } else {
        format!("{}…", words[..MAX_SPOKEN_WORDS].join(" "))
    }
}

/// Pregunta específica que Jarvis lee antes de una acción crítica.
pub fn confirmation_question(action: &CriticalAction) -> String {
    match action {
        CriticalAction::SendMessage {
            app,
            recipient,
            content,
        } => format!(
            "Voy a enviar este mensaje a {recipient} por {app} que dice: \"{}\". ¿Procedo?",
            spoken_content(content)
        ),
        CriticalAction::DeleteFiles { what } => {
            format!("Voy a borrar {what}. Esto no se puede deshacer. ¿Procedo?")
        }
        CriticalAction::RunCommand { command } => format!(
            "Voy a ejecutar en la terminal: {}. ¿Procedo?",
            spoken_content(command)
        ),
        CriticalAction::Purchase { what, amount } => match amount {
            Some(amount) => format!("Voy a comprar {what} por {amount}. ¿Procedo?"),
            None => format!("Voy a comprar {what}. ¿Procedo?"),
        },
        CriticalAction::Other { summary } => format!("Voy a {summary}. ¿Procedo?"),
    }
}

/// Minúscula sin tilde, carácter a carácter (conserva los índices).
fn fold_char(c: char) -> char {
    match c.to_lowercase().next().unwrap_or(c) {
        'á' | 'à' | 'ä' | 'â' => 'a',
        'é' | 'è' | 'ë' | 'ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' => 'o',
        'ú' | 'ù' | 'ü' | 'û' => 'u',
        c => c,
    }
}

const YES_WORDS: &[&str] = &[
    "si",
    "sip",
    "dale",
    "ok",
    "okay",
    "okey",
    "vale",
    "confirmo",
    "confirmado",
    "hazlo",
    "procede",
    "adelante",
    "claro",
    "correcto",
    "afirmativo",
    "envialo",
    "mandalo",
    "enviar",
    "envia",
    "manda",
    "perfecto",
    "listo",
    "exacto",
    "yes",
];

const NO_WORDS: &[&str] = &[
    "no",
    "nop",
    "espera",
    "esperate",
    "cancela",
    "cancelalo",
    "cancelar",
    "detente",
    "para",
    "negativo",
    "alto",
    "nunca",
    "tampoco",
];

/// Marcadores de edición: lo que sigue es el contenido nuevo.
const EDIT_MARKERS: &[&str] = &[
    "cambialo para que diga ",
    "cambia el mensaje por ",
    "cambia el mensaje a ",
    "reemplazalo por ",
    "cambialo por ",
    "cambialo a ",
    "corrigelo a ",
    "mejor escribe ",
    "escribe mejor ",
    "mejor pon ",
    "mejor di ",
    "que diga ",
    "cambia a ",
    "ponle ",
];

const QUOTES_AND_PUNCT: &[char] = &[
    '"', '\'', '“', '”', '‘', '’', '«', '»', '.', ',', ';', ':', '¡', '!', '¿', '?', ' ',
];

/// Contenido nuevo de una petición de edición, con mayúsculas y tildes del
/// original.
fn edit_request(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    // Puntuación → espacio (1:1): "ponle: nos vemos" también cuenta.
    let folded: String = chars
        .iter()
        .map(|c| {
            let c = fold_char(*c);
            if c.is_alphanumeric() {
                c
            } else {
                ' '
            }
        })
        .collect();
    let (pos, marker) = EDIT_MARKERS
        .iter()
        .filter_map(|m| folded.find(m).map(|p| (p, *m)))
        .min_by_key(|(p, m)| (*p, usize::MAX - m.len()))?;
    // `find` da un índice en bytes del texto plegado; se pasa a caracteres.
    let start_chars = folded[..pos].chars().count() + marker.chars().count();
    let rest: String = chars[start_chars..].iter().collect();
    let rest = rest.trim_matches(QUOTES_AND_PUNCT).to_string();
    (!rest.is_empty()).then_some(rest)
}

/// Interpreta la respuesta hablada. Ante la duda NO confirma: un "sí"
/// mezclado con un "no" es `Unclear` (se re-pregunta), nunca `Yes`.
pub fn parse_confirmation(text: &str) -> ConfirmationReply {
    if let Some(new_content) = edit_request(text) {
        return ConfirmationReply::Edit(new_content);
    }
    let folded: String = text
        .chars()
        .map(|c| {
            let c = fold_char(c);
            if c.is_alphanumeric() {
                c
            } else {
                ' '
            }
        })
        .collect();
    let words: Vec<&str> = folded
        .split_whitespace()
        .skip_while(|w| *w == "jarvis")
        .collect();
    let joined = format!(" {} ", words.join(" "));
    let yes = words.iter().any(|w| YES_WORDS.contains(w))
        || [" de acuerdo ", " por supuesto ", " por favor envia"]
            .iter()
            .any(|p| joined.contains(p));
    let no = words.iter().any(|w| NO_WORDS.contains(w));
    match (yes, no) {
        (true, false) => ConfirmationReply::Yes,
        (false, true) => ConfirmationReply::No,
        _ => ConfirmationReply::Unclear,
    }
}

/// Qué hacer tras una respuesta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmationDecision {
    /// Re-verificar (TOCTOU) y actuar.
    Proceed,
    /// No actuar; el borrador queda escrito.
    Abort { reason: String },
    /// Volver a preguntar (ambigua o silencio, solo una vez).
    AskAgain,
    /// Re-redactar con el contenido nuevo y volver a confirmar.
    Redraft(String),
}

/// Estado de UNA confirmación: vale para una sola acción crítica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationFlow {
    reasks_left: u8,
}

impl Default for ConfirmationFlow {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfirmationFlow {
    pub fn new() -> Self {
        Self { reasks_left: 1 }
    }

    pub fn decide(&mut self, reply: ConfirmationReply) -> ConfirmationDecision {
        match reply {
            ConfirmationReply::Yes => ConfirmationDecision::Proceed,
            ConfirmationReply::No => ConfirmationDecision::Abort {
                reason: "el usuario dijo que no".into(),
            },
            ConfirmationReply::Edit(text) => {
                // Una edición es una respuesta clara: renueva el re-pregunteo.
                self.reasks_left = 1;
                ConfirmationDecision::Redraft(text)
            }
            ConfirmationReply::Unclear | ConfirmationReply::Timeout if self.reasks_left > 0 => {
                self.reasks_left -= 1;
                ConfirmationDecision::AskAgain
            }
            ConfirmationReply::Unclear => ConfirmationDecision::Abort {
                reason: "respuesta ambigua dos veces".into(),
            },
            ConfirmationReply::Timeout => ConfirmationDecision::Abort {
                reason: "sin respuesta dos veces".into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConfirmationReply::*;

    #[test]
    fn parses_forty_phrases() {
        let cases: &[(&str, ConfirmationReply)] = &[
            ("sí", Yes),
            ("Sí.", Yes),
            ("si", Yes),
            ("sí, envíalo", Yes),
            ("envíalo", Yes),
            ("dale", Yes),
            ("ok", Yes),
            ("okey", Yes),
            ("confirmo", Yes),
            ("hazlo", Yes),
            ("procede", Yes),
            ("adelante", Yes),
            ("claro", Yes),
            ("de acuerdo", Yes),
            ("por supuesto", Yes),
            ("Jarvis, sí", Yes),
            ("sí, mándalo", Yes),
            ("perfecto, envía", Yes),
            ("vale", Yes),
            ("afirmativo", Yes),
            ("no", No),
            ("No.", No),
            ("no lo envíes", No),
            ("espera", No),
            ("cancela", No),
            ("mejor no", No),
            ("todavía no", No),
            ("Jarvis, no", No),
            ("negativo", No),
            ("detente", No),
            ("sí, no, espera", Unclear),
            ("no sé", No),
            ("mmm", Unclear),
            ("", Unclear),
            ("¿qué?", Unclear),
            ("repite", Unclear),
            ("cámbialo a 'llego a las 9'", Edit("llego a las 9".into())),
            (
                "sí pero cámbialo a llego a las 9",
                Edit("llego a las 9".into()),
            ),
            ("mejor di que Llego Tarde", Edit("que Llego Tarde".into())),
            (
                "cámbialo por \"Voy en camino\"",
                Edit("Voy en camino".into()),
            ),
            ("ponle: nos vemos mañana", Edit("nos vemos mañana".into())),
            ("cámbialo a", Unclear),
        ];
        assert!(cases.len() >= 40);
        for (text, expected) in cases {
            assert_eq!(&parse_confirmation(text), expected, "{text:?}");
        }
    }

    #[test]
    fn whatsapp_question_names_recipient_app_and_content() {
        let q = confirmation_question(&CriticalAction::SendMessage {
            app: "WhatsApp".into(),
            recipient: "Mamá".into(),
            content: "Llego a las 8".into(),
        });
        assert_eq!(
            q,
            "Voy a enviar este mensaje a Mamá por WhatsApp que dice: \"Llego a las 8\". ¿Procedo?"
        );
    }

    #[test]
    fn long_content_is_summarized() {
        let content = (1..=40)
            .map(|i| format!("p{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let q = confirmation_question(&CriticalAction::SendMessage {
            app: "Slack".into(),
            recipient: "Felipe".into(),
            content,
        });
        assert!(q.contains("p25…"), "{q}");
        assert!(!q.contains("p26"), "{q}");
    }

    #[test]
    fn other_critical_actions_have_specific_questions() {
        assert_eq!(
            confirmation_question(&CriticalAction::DeleteFiles {
                what: "la carpeta Descargas/viejos".into()
            }),
            "Voy a borrar la carpeta Descargas/viejos. Esto no se puede deshacer. ¿Procedo?"
        );
        assert_eq!(
            confirmation_question(&CriticalAction::RunCommand {
                command: "dnf upgrade".into()
            }),
            "Voy a ejecutar en la terminal: dnf upgrade. ¿Procedo?"
        );
        assert_eq!(
            confirmation_question(&CriticalAction::Purchase {
                what: "el libro".into(),
                amount: Some("15 dólares".into())
            }),
            "Voy a comprar el libro por 15 dólares. ¿Procedo?"
        );
    }

    #[test]
    fn flow_reasks_once_then_aborts() {
        let mut flow = ConfirmationFlow::new();
        assert_eq!(flow.decide(Timeout), ConfirmationDecision::AskAgain);
        assert!(matches!(
            flow.decide(Timeout),
            ConfirmationDecision::Abort { .. }
        ));

        let mut flow = ConfirmationFlow::new();
        assert_eq!(flow.decide(Unclear), ConfirmationDecision::AskAgain);
        assert_eq!(flow.decide(Yes), ConfirmationDecision::Proceed);

        let mut flow = ConfirmationFlow::new();
        assert!(matches!(
            flow.decide(No),
            ConfirmationDecision::Abort { .. }
        ));

        let mut flow = ConfirmationFlow::new();
        assert_eq!(flow.decide(Unclear), ConfirmationDecision::AskAgain);
        assert_eq!(
            flow.decide(Edit("llego a las 9".into())),
            ConfirmationDecision::Redraft("llego a las 9".into())
        );
        assert_eq!(flow.decide(Timeout), ConfirmationDecision::AskAgain);
    }
}
