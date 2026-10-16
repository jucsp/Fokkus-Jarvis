//! Vocabulario del plan de acción (HU-39 §5).

use jarvis_automation::{Key, LRect};
use serde::{Deserialize, Serialize};

use super::risk::ActionRiskLevel;

/// Modo de operación del agente.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Sitio web en el perfil de navegador dedicado (DOM + visión).
    Web,
    /// App nativa (captura + accesibilidad + visión).
    Desktop,
}

/// Rol esperado del elemento objetivo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementRole {
    Textbox,
    SearchBox,
    /// Compositor de mensajes de una app de mensajería.
    MessageComposer,
    Button,
    Link,
    ChatTitle,
    MediaControl,
    PasswordField,
    Other(String),
}

/// Cómo se compara la etiqueta esperada.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MatchPolicy {
    /// Personas y destinatarios: igualdad exacta (normalizada).
    Exact,
    /// Elementos de UI: similitud mínima.
    Fuzzy(f32),
}

/// Elemento que el paso necesita encontrar.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetSpec {
    /// "WhatsApp Web", "youtube.com", "org.telegram.desktop".
    pub app: String,
    pub role: ElementRole,
    /// "Buscar", "Mamá".
    pub expected_label: Option<String>,
    pub match_policy: MatchPolicy,
}

/// Qué contexto se verifica (p. ej. el título del chat abierto).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextKind {
    ChatTitle,
    PageTitle,
    WindowTitle,
}

/// Precondición declarativa, verificada sobre una observación fresca.
#[derive(Debug, Clone, PartialEq)]
pub enum Precondition {
    AppFocused { app: String },
    UrlMatches { pattern: String },
    ElementVisible(TargetSpec),
    ContextIs { what: ContextKind, expected: String },
    FocusInside(TargetSpec),
    FieldValueIs { target: TargetSpec, value: String },
    FieldEmpty(TargetSpec),
}

pub type Postcondition = Precondition;

/// Acción de un paso.
#[derive(Debug, Clone, PartialEq)]
pub enum StepAction {
    Navigate(String),
    Click(TargetSpec),
    Fill { target: TargetSpec, text: String },
    Press(Vec<Key>),
    Scroll { dy: f64 },
    ReadOut(TargetSpec),
}

/// Un paso del plan con sus pre y postcondiciones.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub action: StepAction,
    pub pre: Vec<Precondition>,
    pub post: Vec<Postcondition>,
    /// Riesgo según el planificador: las reglas locales solo pueden subirlo.
    pub risk_hint: ActionRiskLevel,
    /// "enviar a Mamá: 'Llego a las 8'".
    pub summary_for_user: String,
}

impl Step {
    /// App sobre la que actúa el paso: la del objetivo o, si la acción no
    /// tiene objetivo (teclas, scroll), la de la precondición `AppFocused`.
    pub fn app(&self) -> Option<&str> {
        match &self.action {
            StepAction::Click(t) | StepAction::Fill { target: t, .. } | StepAction::ReadOut(t) => {
                Some(t.app.as_str())
            }
            _ => self.pre.iter().find_map(|p| match p {
                Precondition::AppFocused { app } => Some(app.as_str()),
                Precondition::ElementVisible(t) | Precondition::FocusInside(t) => {
                    Some(t.app.as_str())
                }
                _ => None,
            }),
        }
    }

    /// Objetivo declarado del paso (o el de su precondición de foco).
    pub fn target(&self) -> Option<&TargetSpec> {
        match &self.action {
            StepAction::Click(t) | StepAction::Fill { target: t, .. } | StepAction::ReadOut(t) => {
                Some(t)
            }
            _ => self.pre.iter().find_map(|p| match p {
                Precondition::FocusInside(t) => Some(t),
                _ => None,
            }),
        }
    }
}

/// Lo que el usuario dijo, inmutable: destinatarios y contenido salen de aquí,
/// nunca de la pantalla (§2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpokenIntent {
    pub raw: String,
    pub recipients: Vec<String>,
    pub content: Option<String>,
}

/// Plan completo.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionPlan {
    pub mode: AgentMode,
    pub steps: Vec<Step>,
    pub intent: SpokenIntent,
}

/// Origen de un elemento observado.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementSource {
    Dom,
    Atspi,
    Uia,
    Vision,
}

/// Elemento observado en pantalla. `label` es dato NO confiable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElementInfo {
    /// Número Set-of-Marks.
    pub mark: Option<u16>,
    pub role: String,
    pub label: String,
    pub bbox: LRect,
    pub focused: bool,
    pub source: ElementSource,
}
