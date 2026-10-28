//! Consulta local de las notificaciones del escritorio (HU-72, CA-72.5).
//!
//! El orquestador no conoce D-Bus: recibe un [`NotificationIntrospection`] que
//! cablea `jarvis-app` sobre el monitor de `jarvis-shell`. Aquí vive solo el
//! reconocimiento de la pregunta ("¿de dónde vino esa notificación?", "¿qué
//! notificaciones llegaron?", "¿qué dijo Spectacle?") y las frases de respaldo.
//!
//! La respuesta es siempre una plantilla local: el contenido de una
//! notificación nunca llega al LLM ni se reinterpreta como orden (CA-72.4).

use std::sync::Arc;

/// Qué se pregunta sobre las notificaciones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationQuery {
    /// "¿De dónde vino la última notificación?"
    Latest,
    /// "¿Qué notificaciones llegaron?"
    All,
    /// "¿Qué dijo Spectacle?" / "notificaciones de Discord".
    ByApp(String),
}

/// Fuente de notificaciones para el orquestador.
pub trait NotificationIntrospection: Send + Sync {
    /// `false` si la función está desactivada o el bus no permite observar.
    fn is_available(&self) -> bool;

    /// Respuesta hablada a `query`, o `None` si no hay notificaciones que la
    /// respondan (anillo vacío o sin coincidencias).
    fn answer(&self, query: &NotificationQuery) -> Option<String>;
}

/// Introspección compartida entre el orquestador y el apagado de la app.
pub type NotificationSource = Arc<dyn NotificationIntrospection>;

/// Respuesta cuando la función está desactivada o el bus no la permite.
pub const NOTIFICATIONS_UNAVAILABLE_PHRASE: &str =
    "No puedo ver las notificaciones: la introspección no está disponible.";

/// Respuesta cuando el anillo está vacío.
pub const NO_NOTIFICATIONS_PHRASE: &str = "No hay notificaciones recientes.";

/// Frase cuando no hay nada que responda a `query`.
pub fn empty_answer(query: &NotificationQuery) -> String {
    match query {
        NotificationQuery::ByApp(app) => format!("No hay notificaciones recientes de {app}."),
        _ => NO_NOTIFICATIONS_PHRASE.to_string(),
    }
}

const SINGULAR: &[&str] = &["notificacion", "noti"];
const PLURAL: &[&str] = &["notificaciones", "notis"];

/// Verbos que piden **actuar** sobre las notificaciones, no consultarlas: esos
/// turnos siguen al router.
const ACTION_VERBS: &[&str] = &[
    "activa",
    "activar",
    "borra",
    "borrar",
    "cierra",
    "cerrar",
    "configura",
    "configurar",
    "desactiva",
    "desactivar",
    "elimina",
    "eliminar",
    "limpia",
    "limpiar",
    "oculta",
    "ocultar",
    "quita",
    "quitar",
    "silencia",
    "silenciar",
];

/// Palabras que nunca son nombre de aplicación tras "de" ("de hoy", "de recién").
const NOT_AN_APP: &[&str] = &[
    "ahora",
    "antes",
    "ayer",
    "donde",
    "el",
    "en",
    "esa",
    "ese",
    "esta",
    "este",
    "hace",
    "hoy",
    "la",
    "las",
    "lo",
    "los",
    "manana",
    "mi",
    "mis",
    "que",
    "quien",
    "recien",
    "recientes",
    "sistema",
    "tarde",
    "un",
    "una",
];

/// Muletillas que cierran el nombre de la app ("qué dijo Spectacle po").
const TAIL_FILLERS: &[&str] = &[
    "ahora", "al", "cachai", "con", "en", "hace", "hoy", "nomas", "para", "po", "por", "porfa",
    "pues", "recien", "sobre", "tiro", "y",
];

/// Lo que puede preceder a "qué dijo X" sin cambiar la pregunta.
const LEADING_FILLERS: &[&str] = &["a", "oye", "jarvis", "ver", "y", "ya", "dime", "sabes"];

/// Verbos de "¿qué dijo/mandó/notificó X?".
const SAID_VERBS: &[&str] = &["dijo", "mando", "notifico", "envio", "aviso"];

/// Reconoce una consulta de notificaciones en palabras ya plegadas
/// (minúsculas, sin tildes ni puntuación).
///
/// Devuelve la consulta y si fue **explícita** (nombra la palabra
/// "notificación"). Una consulta implícita ("¿qué dijo Spectacle?") puede ser
/// una pregunta general ("¿qué dijo Einstein?"): el orquestador la devuelve al
/// router si no hay notificaciones que la respondan.
pub fn notification_query(words: &[&str]) -> Option<(NotificationQuery, bool)> {
    if words.iter().any(|w| ACTION_VERBS.contains(w)) {
        return None;
    }
    if let Some(pos) = words.iter().position(|w| PLURAL.contains(w)) {
        let query = app_after(&words[pos + 1..])
            .map(NotificationQuery::ByApp)
            .unwrap_or(NotificationQuery::All);
        return Some((query, true));
    }
    if let Some(pos) = words.iter().position(|w| SINGULAR.contains(w)) {
        let query = app_after(&words[pos + 1..])
            .map(NotificationQuery::ByApp)
            .unwrap_or(NotificationQuery::Latest);
        return Some((query, true));
    }
    said_by_app(words).map(|app| (NotificationQuery::ByApp(app), false))
}

/// "… de Spectacle" / "… del Discord" justo después de la palabra clave.
fn app_after(rest: &[&str]) -> Option<String> {
    let rest = match rest {
        [prep, rest @ ..] if *prep == "de" || *prep == "del" => rest,
        _ => return None,
    };
    let name: Vec<&str> = rest
        .iter()
        .take_while(|w| !TAIL_FILLERS.contains(w) && **w != "que")
        .take(3)
        .copied()
        .collect();
    match name.first() {
        Some(first) if !NOT_AN_APP.contains(first) => Some(name.join(" ")),
        _ => None,
    }
}

/// "qué dijo Spectacle", "qué me mandó Discord po": el nombre es todo lo que
/// sigue al verbo (1 o 2 palabras) y antes solo hay muletillas.
fn said_by_app(words: &[&str]) -> Option<String> {
    let que = words.iter().position(|w| *w == "que")?;
    if !words[..que].iter().all(|w| LEADING_FILLERS.contains(w)) {
        return None;
    }
    let mut rest = &words[que + 1..];
    if rest.first() == Some(&"me") {
        rest = &rest[1..];
    }
    let (verb, rest) = rest.split_first()?;
    if !SAID_VERBS.contains(verb) {
        return None;
    }
    let mut name = rest;
    while let [head @ .., last] = name {
        if TAIL_FILLERS.contains(last) {
            name = head;
        } else {
            break;
        }
    }
    match name {
        [first, ..] if (1..=2).contains(&name.len()) && !NOT_AN_APP.contains(first) => {
            Some(name.join(" "))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::normalize::fold_accents;

    fn parse(text: &str) -> Option<(NotificationQuery, bool)> {
        let clean: String = fold_accents(text)
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect();
        let words: Vec<&str> = clean.split_whitespace().collect();
        notification_query(&words)
    }

    fn by_app(app: &str, explicit: bool) -> Option<(NotificationQuery, bool)> {
        Some((NotificationQuery::ByApp(app.into()), explicit))
    }

    #[test]
    fn recognizes_the_three_spec_questions() {
        let latest = Some((NotificationQuery::Latest, true));
        let all = Some((NotificationQuery::All, true));
        assert_eq!(parse("¿de dónde vino esa notificación?"), latest);
        assert_eq!(parse("de dónde vino la última notificación"), latest);
        assert_eq!(parse("¿quién mandó la noti de recién?"), latest);
        assert_eq!(
            parse("no busques en Google, dime de dónde salió la notificación"),
            latest
        );
        assert_eq!(parse("¿qué notificaciones llegaron?"), all);
        assert_eq!(parse("qué notificaciones me llegaron hoy"), all);
        assert_eq!(parse("léeme las notificaciones recientes"), all);
        assert_eq!(parse("¿qué dijo Spectacle?"), by_app("spectacle", false));
        assert_eq!(
            parse("oye, ¿qué me mandó Discord po?"),
            by_app("discord", false)
        );
        assert_eq!(
            parse("¿qué notificó Google Chrome?"),
            by_app("google chrome", false)
        );
        assert_eq!(
            parse("las notificaciones de Spectacle"),
            by_app("spectacle", true)
        );
        assert_eq!(
            parse("la última notificación del Discord"),
            by_app("discord", true)
        );
    }

    #[test]
    fn leaves_other_turns_alone() {
        for text in [
            "borra las notificaciones",
            "desactiva las notificaciones de Discord",
            "silencia las notificaciones",
            "¿qué hora es?",
            "¿qué dijo Einstein sobre la relatividad general en 1915?",
            "dime qué dijo el profe en la reunión de ayer",
            "¿qué es Kubernetes?",
            "¿qué dijo?",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn empty_answer_names_the_app() {
        assert_eq!(
            empty_answer(&NotificationQuery::Latest),
            NO_NOTIFICATIONS_PHRASE
        );
        assert_eq!(
            empty_answer(&NotificationQuery::All),
            NO_NOTIFICATIONS_PHRASE
        );
        assert_eq!(
            empty_answer(&NotificationQuery::ByApp("spectacle".into())),
            "No hay notificaciones recientes de spectacle."
        );
    }
}
