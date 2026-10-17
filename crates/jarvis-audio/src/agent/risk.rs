//! Matriz de criterio de riesgo (HU-39 §2.3).
//!
//! La deciden **reglas locales**, no el modelo:
//!
//! | Nivel | Ejemplos | Verificación | Confirmación vocal |
//! |---|---|---|---|
//! | `Low` | play/pause, scroll, juegos, navegar, buscar | post-acción | no |
//! | `Medium` | borradores, formularios sin enviar, clics genéricos | ligera + post | no |
//! | `High` | enviar mensajes/correos, terminal, borrar, comprar, credenciales | estricta + post | **siempre** |
//!
//! El valor del planificador solo puede SUBIR el riesgo; las reglas jamás
//! rebajan un `High` del planificador.

pub use jarvis_automation::ActionRiskLevel;

use jarvis_automation::Key;

use super::plan::{ElementInfo, ElementRole, Step, StepAction};

/// Apps de mensajería y correo: Enter en su compositor envía.
const MESSAGING_APPS: &[&str] = &[
    "whatsapp",
    "telegram",
    "slack",
    "signal",
    "discord",
    "teams",
    "messenger",
    "instagram",
    "element",
    "gmail",
    "outlook",
    "thunderbird",
    "kmail",
    "evolution",
    "geary",
    "mail",
    "correo",
];

/// Terminales: cualquier texto + Enter ejecuta un comando.
const TERMINAL_APPS: &[&str] = &[
    "konsole",
    "terminal",
    "kitty",
    "alacritty",
    "wezterm",
    "xterm",
    "foot",
    "tilix",
    "terminator",
    "yakuake",
    "guake",
    "powershell",
    "cmd.exe",
    "iterm",
];

/// Nombres accesibles de botones con efecto irreversible.
const CRITICAL_LABELS: &[&str] = &[
    "enviar",
    "send",
    "publicar",
    "post",
    "comentar",
    "comment",
    "responder",
    "reply",
    "comprar",
    "buy",
    "pagar",
    "pay",
    "checkout",
    "finalizar compra",
    "transferir",
    "transfer",
    "eliminar",
    "delete",
    "borrar",
    "remove",
    "quitar",
    "vaciar",
    "confirmar",
    "confirm",
    "aceptar invitacion",
    "subir",
    "upload",
    "compartir",
    "share",
];

/// Texto peligroso al escribirlo en cualquier sitio.
const DANGEROUS_TEXT: &[&str] = &[
    "rm ", "rm\t", "sudo", "pkexec", "mkfs", "dd if=", "chmod ", "chown ", ":(){", "> /dev/",
    "del /", "format ", "shutdown", "reboot",
];

/// Controles multimedia y de juego.
const MEDIA_LABELS: &[&str] = &[
    "play",
    "pause",
    "reproducir",
    "pausar",
    "pausa",
    "siguiente",
    "next",
    "anterior",
    "previous",
    "skip",
    "saltar",
    "omitir",
    "silenciar",
    "mute",
    "volumen",
    "volume",
    "pantalla completa",
    "full screen",
    "fullscreen",
    "subtitulos",
    "subtitles",
];

/// Minúsculas y sin tildes.
fn fold(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            c => c,
        })
        .collect()
}

fn any_in(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// La etiqueta coincide con una de la lista como palabra o frase completa
/// ("Enviar", "Send message") y no como fragmento ("Postre", "Sendero").
fn label_matches(label: &str, list: &[&str]) -> bool {
    let words: Vec<String> = fold(label)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let joined = format!(" {} ", words.join(" "));
    list.iter()
        .any(|item| joined.contains(&format!(" {item} ")))
}

fn is_search(role: &str, label: &str) -> bool {
    let (role, label) = (fold(role), fold(label));
    role.contains("search")
        || (role.contains("combobox") && (label.contains("buscar") || label.contains("search")))
}

/// Riesgo según las reglas locales, sin mirar la pista del planificador.
pub fn rules_risk(step: &Step, target: &ElementInfo) -> ActionRiskLevel {
    let app = fold(step.app().unwrap_or_default());
    let spec = step.target();
    let messaging = any_in(&app, MESSAGING_APPS);
    let terminal = any_in(&app, TERMINAL_APPS);
    let label = spec
        .and_then(|t| t.expected_label.as_deref())
        .map(|expected| format!("{expected} {}", target.label))
        .unwrap_or_else(|| target.label.clone());
    let password = fold(&target.role).contains("password")
        || spec.is_some_and(|t| t.role == ElementRole::PasswordField);
    let search = is_search(&target.role, &target.label)
        || spec.is_some_and(|t| t.role == ElementRole::SearchBox);
    let media = fold(&target.role).contains("media")
        || spec.is_some_and(|t| t.role == ElementRole::MediaControl)
        || label_matches(&label, MEDIA_LABELS);

    match &step.action {
        StepAction::Scroll { .. } | StepAction::ReadOut(_) | StepAction::Navigate(_) => {
            ActionRiskLevel::Low
        }
        StepAction::Click(_) => {
            if password || label_matches(&label, CRITICAL_LABELS) {
                ActionRiskLevel::High
            } else if media {
                ActionRiskLevel::Low
            } else {
                ActionRiskLevel::Medium
            }
        }
        StepAction::Fill { text, .. } => {
            if password || terminal || any_in(&fold(text), DANGEROUS_TEXT) {
                ActionRiskLevel::High
            } else if search {
                ActionRiskLevel::Low
            } else {
                ActionRiskLevel::Medium
            }
        }
        StepAction::Press(keys) => {
            let enter = keys.contains(&Key::Enter);
            let deletes = keys.contains(&Key::Delete);
            if password || deletes || (enter && terminal) || (enter && messaging && !search) {
                ActionRiskLevel::High
            } else if (enter && search) || keys.iter().all(is_navigation_or_media_key) {
                ActionRiskLevel::Low
            } else {
                ActionRiskLevel::Medium
            }
        }
    }
}

/// Teclas sin efecto destructivo: flechas, espacio, multimedia, páginas.
fn is_navigation_or_media_key(key: &Key) -> bool {
    matches!(
        key,
        Key::Up
            | Key::Down
            | Key::Left
            | Key::Right
            | Key::Space
            | Key::PageUp
            | Key::PageDown
            | Key::Home
            | Key::End
            | Key::MediaPlayPause
            | Key::MediaNext
            | Key::MediaPrevious
            | Key::VolumeUp
            | Key::VolumeDown
            | Key::Mute
            | Key::Escape
    ) || matches!(key, Key::Char(c) if "kjlmf".contains(c.to_ascii_lowercase()))
}

/// Riesgo final: el mayor entre las reglas locales y la pista del
/// planificador. Las reglas nunca rebajan lo que el planificador marcó.
pub fn classify_risk(
    step: &Step,
    target: &ElementInfo,
    planner_hint: ActionRiskLevel,
) -> ActionRiskLevel {
    rules_risk(step, target).max(planner_hint)
}

/// Nivel de autoverificación previa a la acción.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreVerification {
    /// Sin precondiciones pesadas (flujo autónomo y fluido).
    None,
    /// Elemento correcto.
    Light,
    /// Elemento exacto, destinatario/contexto correcto y foco en la caja de
    /// texto, con doble llave (DOM/a11y + visión) y re-verificación TOCTOU.
    Strict,
}

/// Qué exige cada nivel de riesgo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskPolicy {
    pub pre_verification: PreVerification,
    /// Comprobar que el efecto esperado ocurrió.
    pub post_verification: bool,
    /// Confirmación hablada obligatoria antes de actuar.
    pub voice_confirmation: bool,
}

/// Política de la matriz para un nivel.
pub fn policy_for(level: ActionRiskLevel) -> RiskPolicy {
    match level {
        ActionRiskLevel::Low => RiskPolicy {
            pre_verification: PreVerification::None,
            post_verification: true,
            voice_confirmation: false,
        },
        ActionRiskLevel::Medium => RiskPolicy {
            pre_verification: PreVerification::Light,
            post_verification: true,
            voice_confirmation: false,
        },
        ActionRiskLevel::High => RiskPolicy {
            pre_verification: PreVerification::Strict,
            post_verification: true,
            voice_confirmation: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::plan::{ElementSource, MatchPolicy, Precondition, TargetSpec};
    use jarvis_automation::LRect;

    fn spec(app: &str, role: ElementRole, label: Option<&str>) -> TargetSpec {
        TargetSpec {
            app: app.into(),
            role,
            expected_label: label.map(str::to_string),
            match_policy: MatchPolicy::Fuzzy(0.9),
        }
    }

    fn element(role: &str, label: &str) -> ElementInfo {
        ElementInfo {
            mark: Some(1),
            role: role.into(),
            label: label.into(),
            bbox: LRect::new(0.0, 0.0, 10.0, 10.0),
            focused: true,
            source: ElementSource::Dom,
        }
    }

    fn step(action: StepAction, app: &str) -> Step {
        Step {
            action,
            pre: vec![Precondition::AppFocused { app: app.into() }],
            post: Vec::new(),
            risk_hint: ActionRiskLevel::Low,
            summary_for_user: String::new(),
        }
    }

    fn risk(
        action: StepAction,
        app: &str,
        el: ElementInfo,
        hint: ActionRiskLevel,
    ) -> ActionRiskLevel {
        classify_risk(&step(action, app), &el, hint)
    }

    #[test]
    fn enter_in_messaging_composer_is_high_even_with_low_hint() {
        for app in ["WhatsApp Web", "org.telegram.desktop", "Slack", "Gmail"] {
            for hint in [ActionRiskLevel::Low, ActionRiskLevel::Medium] {
                assert_eq!(
                    risk(
                        StepAction::Press(vec![Key::Enter]),
                        app,
                        element("textbox", "Escribe un mensaje"),
                        hint
                    ),
                    ActionRiskLevel::High,
                    "{app}"
                );
            }
        }
    }

    #[test]
    fn critical_buttons_are_high() {
        for label in [
            "Enviar",
            "Send message",
            "Eliminar",
            "Delete",
            "Comprar ahora",
            "Pagar",
            "Borrar chat",
        ] {
            let s = spec("cualquiera", ElementRole::Button, None);
            assert_eq!(
                risk(
                    StepAction::Click(s),
                    "cualquiera",
                    element("button", label),
                    ActionRiskLevel::Low
                ),
                ActionRiskLevel::High,
                "{label}"
            );
        }
        // Coincidencia por palabra, no por fragmento.
        let s = spec("recetas", ElementRole::Button, None);
        assert_eq!(
            risk(
                StepAction::Click(s),
                "recetas",
                element("button", "Postres"),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::Medium
        );
    }

    #[test]
    fn terminal_typing_and_dangerous_text_are_high() {
        let konsole = spec("konsole", ElementRole::Textbox, None);
        assert_eq!(
            risk(
                StepAction::Fill {
                    target: konsole,
                    text: "ls".into()
                },
                "konsole",
                element("terminal", ""),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::High
        );
        assert_eq!(
            risk(
                StepAction::Press(vec![Key::Enter]),
                "konsole",
                element("terminal", ""),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::High
        );
        let editor = spec("kwrite", ElementRole::Textbox, None);
        assert_eq!(
            risk(
                StepAction::Fill {
                    target: editor,
                    text: "sudo rm -rf /".into()
                },
                "kwrite",
                element("textbox", ""),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::High
        );
    }

    #[test]
    fn credentials_are_high() {
        let s = spec("banco", ElementRole::PasswordField, None);
        assert_eq!(
            risk(
                StepAction::Fill {
                    target: s,
                    text: "x".into()
                },
                "banco",
                element("password text", "Clave"),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::High
        );
    }

    #[test]
    fn search_is_low_and_needs_no_confirmation() {
        let s = spec("youtube.com", ElementRole::SearchBox, Some("Buscar"));
        assert_eq!(
            risk(
                StepAction::Fill {
                    target: s,
                    text: "Fireship".into()
                },
                "youtube.com",
                element("combobox", "Buscar"),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::Low
        );
        assert_eq!(
            risk(
                StepAction::Press(vec![Key::Enter]),
                "youtube.com",
                element("searchbox", "Buscar"),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::Low
        );
        // Enter en la búsqueda de WhatsApp tampoco envía nada.
        assert_eq!(
            risk(
                StepAction::Press(vec![Key::Enter]),
                "WhatsApp Web",
                element("searchbox", "Buscar"),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::Low
        );
        assert!(!policy_for(ActionRiskLevel::Low).voice_confirmation);
    }

    #[test]
    fn media_scroll_and_gaming_are_low() {
        let play = spec("youtube.com", ElementRole::Button, None);
        assert_eq!(
            risk(
                StepAction::Click(play),
                "youtube.com",
                element("button", "Reproducir (k)"),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::Low
        );
        let pause = spec("spotify", ElementRole::MediaControl, None);
        assert_eq!(
            risk(
                StepAction::Click(pause),
                "spotify",
                element("button", "Pausa"),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::Low
        );
        assert_eq!(
            risk(
                StepAction::Scroll { dy: 3.0 },
                "firefox",
                element("document", ""),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::Low
        );
        assert_eq!(
            risk(
                StepAction::Press(vec![Key::MediaPlayPause]),
                "spotify",
                element("", ""),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::Low
        );
        assert_eq!(
            risk(
                StepAction::Press(vec![Key::Left, Key::Up]),
                "steam_app_570",
                element("", ""),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::Low
        );
    }

    #[test]
    fn rules_never_lower_a_high_planner_hint() {
        assert_eq!(
            risk(
                StepAction::Scroll { dy: 1.0 },
                "firefox",
                element("document", ""),
                ActionRiskLevel::High
            ),
            ActionRiskLevel::High
        );
    }

    #[test]
    fn drafting_is_medium() {
        let s = spec("WhatsApp Web", ElementRole::MessageComposer, None);
        assert_eq!(
            risk(
                StepAction::Fill {
                    target: s,
                    text: "Llego a las 8".into()
                },
                "WhatsApp Web",
                element("textbox", "Escribe un mensaje"),
                ActionRiskLevel::Low
            ),
            ActionRiskLevel::Medium
        );
        assert!(!policy_for(ActionRiskLevel::Medium).voice_confirmation);
    }

    #[test]
    fn high_policy_demands_strict_preverification_and_voice() {
        let p = policy_for(ActionRiskLevel::High);
        assert_eq!(p.pre_verification, PreVerification::Strict);
        assert!(p.voice_confirmation);
        assert!(p.post_verification);
        assert_eq!(
            policy_for(ActionRiskLevel::Low).pre_verification,
            PreVerification::None
        );
    }
}
