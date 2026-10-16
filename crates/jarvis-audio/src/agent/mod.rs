//! Agente visual con autoverificación pre-acción (HU-39).
//!
//! Esta entrega cubre la parte que decide **cuándo** el agente puede actuar
//! solo y cuándo debe frenar y preguntar:
//!
//! - [`plan`]: vocabulario del plan (`Step`, `Precondition`, `TargetSpec`…).
//! - [`risk`]: matriz de criterio de riesgo con reglas locales (§2.3). El
//!   planificador solo puede SUBIR el riesgo, nunca bajarlo.
//! - [`confirm`]: confirmación vocal obligatoria de las acciones `High`
//!   ("Voy a enviar este mensaje a X por WhatsApp que dice Y. ¿Procedo?").
//!
//! El bucle Observar → Localizar → Verificar → Actuar, la delegación de
//! visión y el sidecar web quedan para las siguientes iteraciones (D-39.1).

pub mod confirm;
pub mod plan;
pub mod risk;

pub use confirm::{
    confirmation_question, parse_confirmation, ConfirmationDecision, ConfirmationFlow,
    ConfirmationReply, CriticalAction,
};
pub use plan::{
    ActionPlan, AgentMode, ContextKind, ElementInfo, ElementRole, ElementSource, MatchPolicy,
    Postcondition, Precondition, SpokenIntent, Step, StepAction, TargetSpec,
};
pub use risk::{
    classify_risk, policy_for, rules_risk, ActionRiskLevel, PreVerification, RiskPolicy,
};
