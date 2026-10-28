//! `TurnRouter` y `TurnPlan`: enrutamiento de turnos de voz (HU-63).
//!
//! Antes de HU-63 `on_turn` tomaba una decisión binaria: si
//! [`classify_workload`](crate::tasks::classify_workload) decía `Heavy`, **todo**
//! el texto se iba a segundo plano; si decía `Light`, todo se respondía en
//! primer plano. Una orden compuesta como *"revísame las PR de Felipe en
//! segundo plano mientras me sintetizas un mensaje para Oscar"* perdía una de
//! las dos mitades: o el mensaje para Oscar no se respondía nunca, o la
//! revisión de PRs bloqueaba la conversación.
//!
//! El router reemplaza esa bifurcación por un [`TurnPlan`] con cinco campos
//! (`foreground`, `spawn`, `route`, `query`, `control`), que el orquestador
//! ejecuta en orden. El plan se construye en dos etapas:
//!
//! 1. **Heurística determinista** ([`TurnRouter::heuristic_plan`]): si el turno
//!    lleva un conector de simultaneidad ([`SIMULTANEITY_CONNECTORS`]) y un lado
//!    es pesado y el otro ligero, el turno es **compuesto**; si no, se resuelve
//!    como pesado (una sub-sesión) o ligero (primer plano) sin coste extra.
//! 2. **Planificador LLM** (solo para compuestos): [`planner::TurnPlanner`]
//!    recibe el turno y el roster de sub-sesiones activas y devuelve el plan en
//!    JSON estricto. Si falla (timeout, error o JSON inválido) se usa la
//!    heurística del punto 1: el turno nunca se pierde.
//!
//! Los turnos simples **no** invocan al planificador (CA-3): la latencia no
//! crece en el caso fácil.
//!
//! # Seguridad
//!
//! La salida del planificador se deserializa en los tipos de este módulo y
//! **nunca** se evalúa como comando: los textos quedan como datos en
//! `SpawnSpec::instruction`, `TurnPlan::foreground` o `route`. No hay
//! `std::process` en este módulo (CA-4).
//!
//! El direccionamiento verbal completo (referencias por id hablado, ordinales,
//! desambiguación y caducidad de foco) llega con HU-64; aquí el plan ya tiene
//! los campos y el orquestador ya los ejecuta.

pub mod planner;

#[cfg(test)]
mod tests;

use crate::sessions::{SessionId, SessionProfile};
use crate::tasks::{
    dev_task_prompt, is_dev_task, is_operational_task, read_only_task_title, task_prompt,
    task_title, Workload,
};
use crate::text::normalize::{normalize, HEAVY_VERBS};
use serde::{Deserialize, Serialize};

pub use planner::{BrainTurnPlanner, PlanError, PlanRequest, TurnPlanner, PLANNER_TIMEOUT};

/// Conectores que anuncian que el usuario pide **dos cosas a la vez**.
///
/// El orden importa: las locuciones de varias palabras van antes que la de una
/// sola para que "mientras tanto …" no se parta por "mientras".
pub const SIMULTANEITY_CONNECTORS: &[&str] = &[
    "mientras tanto",
    "al mismo tiempo",
    "en paralelo",
    "a la vez",
    "y aparte",
    "mientras",
];

/// Frases de fondo que se recortan del final del título de una sub-sesión.
const BACKGROUND_PHRASES: &[&str] = &[
    "en segundo plano",
    "de fondo",
    "en background",
    "en paralelo",
    "al mismo tiempo",
    "a la vez",
];

/// Muletillas y pronombres que abren una orden hablada.
const LEADING_FILLERS: &[&str] = &[
    "jarvis", "oye", "hola", "puedes", "podrias", "por", "favor", "me", "nos", "le", "te",
];

/// Artículos, posesivos y preposiciones que no aportan al título.
const DETERMINERS: &[&str] = &[
    "un", "una", "unos", "unas", "el", "la", "los", "las", "lo", "mi", "mis", "tu", "tus", "su",
    "sus", "de", "del", "al",
];

/// Palabras vacías que no sirven como alias.
const ALIAS_STOPWORDS: &[&str] = &[
    "que", "con", "para", "por", "los", "las", "del", "una", "unos", "unas", "mis", "tus", "sus",
    "este", "esta", "sobre", "desde", "cuando", "algo", "todo", "todos", "mas", "muy", "sin",
];

/// Sub-sesión que hay que abrir para atender la parte de fondo del turno.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// Título corto y hablable ("la revisión de PR de Felipe").
    pub title: String,
    /// Palabras con las que el usuario podrá referirse a ella por voz.
    #[serde(default)]
    pub aliases: Vec<String>,
    pub profile: SessionProfile,
    /// Instrucción completa para el agente de la sub-sesión.
    pub instruction: String,
}

impl SpawnSpec {
    /// Acuse hablado al abrir la sub-sesión.
    ///
    /// Un proceso operacional recuerda cómo pararlo (HU-57).
    pub fn ack_phrase(&self) -> String {
        match self.profile {
            SessionProfile::Operator => format!(
                "Dejé {} corriendo en segundo plano. Dime «detén el proceso» para pararlo.",
                self.title
            ),
            _ => format!("Abrí {} en segundo plano.", self.title),
        }
    }

    /// Título con el que se registra la tarea en el gestor.
    ///
    /// El título elige el *prompt* del subagente en el puerto IPC (HU-61), así
    /// que una sub-sesión de solo lectura no puede acabar con el *prompt* de
    /// ingeniería ([`crate::tasks::DEV_TASK_TITLE`]).
    pub fn task_title(&self) -> String {
        match self.profile {
            SessionProfile::ReadOnly => read_only_task_title(&self.instruction),
            _ => task_title(&self.instruction),
        }
    }

    /// *Prompt* del subagente según el perfil de la sub-sesión.
    pub fn task_prompt(&self, proposal: Option<&str>) -> String {
        match self.profile {
            SessionProfile::ReadOnly => task_prompt(&self.instruction),
            SessionProfile::Operator | SessionProfile::Developer => {
                dev_task_prompt(&self.instruction, proposal)
            }
        }
    }
}

/// Referencia verbal a una sub-sesión.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRef {
    Id(SessionId),
    Alias(String),
    Latest,
    Focused,
}

/// Consulta sobre una sub-sesión ("¿cómo va X?", "léeme el detalle").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionQuery {
    Status(SessionRef),
    ReadDetail(SessionRef),
    ListAll,
}

/// Control del ciclo de vida de una sub-sesión.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionControl {
    Focus(SessionRef),
    Unfocus,
    Close(SessionRef),
    Cancel(SessionRef),
}

/// Plan de un turno: qué se responde ahora y qué se despacha.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPlan {
    /// Parte que responde el hilo central en este turno.
    #[serde(default)]
    pub foreground: Option<String>,
    /// Sub-sesiones nuevas que se abren.
    #[serde(default)]
    pub spawn: Vec<SpawnSpec>,
    /// Instrucciones para sub-sesiones que ya existen.
    #[serde(default)]
    pub route: Vec<(SessionId, String)>,
    /// Consultas sobre sub-sesiones existentes.
    #[serde(default)]
    pub query: Vec<SessionQuery>,
    /// Enfocar, cerrar o cancelar sub-sesiones.
    #[serde(default)]
    pub control: Vec<SessionControl>,
}

impl TurnPlan {
    /// `true` si el plan no pide nada.
    pub fn is_empty(&self) -> bool {
        self.foreground.is_none()
            && self.spawn.is_empty()
            && self.route.is_empty()
            && self.query.is_empty()
            && self.control.is_empty()
    }

    /// Acuse hablado de todas las sub-sesiones que abre el plan.
    pub fn spawn_ack(&self) -> Option<String> {
        if self.spawn.is_empty() {
            return None;
        }
        let phrases: Vec<String> = self.spawn.iter().map(SpawnSpec::ack_phrase).collect();
        Some(phrases.join(" "))
    }

    /// Plan de un único turno conversacional.
    pub fn foreground(text: &str) -> Self {
        Self {
            foreground: Some(text.trim().to_string()),
            ..Self::default()
        }
    }

    /// Plan de una única sub-sesión.
    pub fn spawn(instruction: &str) -> Self {
        Self {
            spawn: vec![describe(instruction)],
            ..Self::default()
        }
    }
}

/// Sub-sesión activa tal como la ve el planificador y el hilo central.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterEntry {
    pub id: SessionId,
    pub title: String,
    /// Estado en palabras ("trabajando", "lista").
    pub status: String,
    #[serde(default)]
    pub progress: Option<String>,
}

impl RosterEntry {
    /// Línea compacta del roster (§3.5).
    pub fn line(&self) -> String {
        match self.progress.as_deref() {
            Some(progress) if !progress.trim().is_empty() => {
                format!(
                    "#{} {} — {} — progreso: \"{}\"",
                    self.id,
                    self.title,
                    self.status,
                    progress.trim()
                )
            }
            _ => format!("#{} {} — {}", self.id, self.title, self.status),
        }
    }
}

/// Contexto del enrutamiento.
pub struct RouteContext<'a> {
    /// Planificador LLM; `None` deja el router en modo heurístico.
    pub planner: Option<&'a dyn TurnPlanner>,
    /// Sub-sesiones activas (roster) para el prompt del planificador.
    pub roster: &'a [RosterEntry],
}

impl RouteContext<'_> {
    /// Contexto sin planificador: solo heurística determinista.
    pub fn heuristic_only() -> Self {
        Self {
            planner: None,
            roster: &[],
        }
    }
}

/// Enrutador de turnos (HU-63).
pub struct TurnRouter;

impl TurnRouter {
    /// Construye el plan del turno.
    ///
    /// Solo un turno **compuesto** (conector de simultaneidad y una mitad
    /// pesada junto a otra ligera) llega al planificador; cualquier otro se
    /// resuelve con la heurística, sin llamar al LLM (CA-3). Si el planificador
    /// falla o devuelve un plan vacío, se cae en la heurística (CA-2).
    pub fn route(query: &str, ctx: &RouteContext<'_>) -> TurnPlan {
        if !is_composite(query) {
            return Self::heuristic_plan(query);
        }
        let Some(planner) = ctx.planner else {
            return Self::heuristic_plan(query);
        };
        match planner.plan(&PlanRequest {
            text: query,
            roster: ctx.roster,
        }) {
            Ok(plan) if !plan.is_empty() => cap_profiles(plan, query),
            Ok(_) | Err(_) => Self::heuristic_plan(query),
        }
    }

    /// Plan determinista (HU-59) sin LLM.
    ///
    /// Es el *fallback* del planificador y el camino de los turnos simples:
    /// pesado → una sub-sesión; ligero → primer plano. Un compuesto sin
    /// planificador se reparte por el conector (mitad pesada a `spawn`, mitad
    /// ligera a `foreground`).
    pub fn heuristic_plan(query: &str) -> TurnPlan {
        if let Some((first, second)) = split_at_connector(query) {
            let first_heavy = is_heavy(first);
            let second_heavy = is_heavy(second);
            if first_heavy != second_heavy {
                let (heavy, light) = if first_heavy {
                    (first, second)
                } else {
                    (second, first)
                };
                return TurnPlan {
                    foreground: Some(light.trim().to_string()),
                    spawn: vec![describe(heavy)],
                    ..TurnPlan::default()
                };
            }
        }
        if is_heavy(query) {
            TurnPlan::spawn(query)
        } else {
            TurnPlan::foreground(query)
        }
    }
}

/// `true` si el turno pide dos cosas a la vez (conector + una mitad pesada y
/// otra ligera).
pub fn is_composite(query: &str) -> bool {
    if !normalize(query).has_any(SIMULTANEITY_CONNECTORS) {
        return false;
    }
    match split_at_connector(query) {
        Some((first, second)) => is_heavy(first) != is_heavy(second),
        None => false,
    }
}

/// Impide que el planificador suba el perfil por su cuenta (CA-4).
///
/// El perfil `Operator`/`Developer` da al subagente el *prompt* que ejecuta
/// procesos y toca código. Solo se respeta si el **turno del usuario** ya lo
/// pedía según la heurística determinista; si no, la sub-sesión queda en
/// `ReadOnly` aunque el JSON diga otra cosa.
fn cap_profiles(mut plan: TurnPlan, query: &str) -> TurnPlan {
    if !is_dev_task(query) {
        for spec in &mut plan.spawn {
            spec.profile = SessionProfile::ReadOnly;
        }
    }
    plan
}

/// `true` si la mitad se atiende en una sub-sesión.
fn is_heavy(text: &str) -> bool {
    crate::tasks::classify_workload(text) == Workload::Heavy
}

/// Construye la sub-sesión que atiende `instruction`.
fn describe(instruction: &str) -> SpawnSpec {
    let profile = if is_operational_task(instruction) {
        SessionProfile::Operator
    } else if is_dev_task(instruction) {
        SessionProfile::Developer
    } else {
        SessionProfile::ReadOnly
    };
    let title = match profile {
        SessionProfile::ReadOnly => read_only_title(instruction),
        _ => task_title(instruction),
    };
    SpawnSpec {
        aliases: spawn_aliases(&title, instruction),
        title,
        profile,
        instruction: instruction.trim().to_string(),
    }
}

/// Título hablable de una sub-sesión de solo lectura.
///
/// Prefiere la forma "la revisión de PR de Felipe" (corta y con el objeto a la
/// vista) y cae en [`read_only_task_title`] cuando el objeto es largo: un
/// acuse con siete palabras no se entiende de oído.
fn read_only_title(instruction: &str) -> String {
    let cleaned = strip_background(instruction);
    match object_phrase(cleaned) {
        Some(object) => {
            let words = object.split_whitespace().count();
            if words == 0 {
                read_only_task_title(instruction)
            } else if words <= 5 {
                format!("la revisión de {object}")
            } else {
                read_only_task_title(instruction)
            }
        }
        None => read_only_task_title(instruction),
    }
}

/// Alias por los que el usuario podrá nombrar la sub-sesión.
fn spawn_aliases(title: &str, instruction: &str) -> Vec<String> {
    let mut aliases: Vec<String> = Vec::new();
    for normalized in [normalize(title), normalize(instruction)] {
        for token in normalized.tokens() {
            let usable = token.len() >= 3
                && !ALIAS_STOPWORDS.contains(&token.as_str())
                && !aliases.iter().any(|known| known == token);
            if usable {
                aliases.push(token.clone());
            }
            if aliases.len() >= 3 {
                return aliases;
            }
        }
    }
    if aliases.is_empty() {
        // Sin palabras útiles ("revisa eso"): el título tal cual.
        aliases.push(normalize(title).text().trim().to_string());
    }
    aliases
}

/// Recorta del final las frases de fondo ("en segundo plano").
fn strip_background(text: &str) -> &str {
    let mut current = text.trim();
    loop {
        let mut changed = false;
        for phrase in BACKGROUND_PHRASES {
            let stripped = strip_phrase_at_end(current, phrase);
            if stripped.len() != current.len() {
                current = stripped;
                changed = true;
            }
        }
        if !changed {
            return current;
        }
    }
}

/// Quita `phrase` del final de `text` si aparece como palabras completas.
fn strip_phrase_at_end<'a>(text: &'a str, phrase: &str) -> &'a str {
    let spans = word_spans(text);
    let needle: Vec<String> = normalize(phrase).tokens().to_vec();
    if needle.is_empty() || spans.len() < needle.len() {
        return text;
    }
    let tokens = span_tokens(text, &spans);
    if tokens[spans.len() - needle.len()..] == needle[..] {
        return text[..spans[spans.len() - needle.len()].0]
            .trim_end_matches(['.', ',', ';', ':'])
            .trim();
    }
    text
}

/// Parte de la instrucción que queda tras muletillas, verbo y artículos.
///
/// "revísame las PR de Felipe" → "PR de Felipe".
fn object_phrase(text: &str) -> Option<&str> {
    let spans = word_spans(text);
    if spans.is_empty() {
        return None;
    }
    let tokens = span_tokens(text, &spans);
    let mut i = 0;
    while i < tokens.len() && LEADING_FILLERS.contains(&tokens[i].as_str()) {
        i += 1;
    }
    if i < tokens.len() && HEAVY_VERBS.contains(&tokens[i].as_str()) {
        i += 1;
    }
    while i < tokens.len() && DETERMINERS.contains(&tokens[i].as_str()) {
        i += 1;
    }
    if i >= tokens.len() {
        return None;
    }
    Some(text[spans[i].0..].trim())
}

/// Límites de las palabras alfanuméricas de `text`.
fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(from) = start.take() {
            spans.push((from, i));
        }
    }
    if let Some(from) = start {
        spans.push((from, text.len()));
    }
    spans
}

/// Token normalizado de cada palabra de `text`.
fn span_tokens(text: &str, spans: &[(usize, usize)]) -> Vec<String> {
    spans
        .iter()
        .map(|(from, to)| {
            normalize(&text[*from..*to])
                .tokens()
                .first()
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

/// Parte el turno por el primer conector de simultaneidad.
fn split_at_connector(query: &str) -> Option<(&str, &str)> {
    let spans = word_spans(query);
    if spans.is_empty() {
        return None;
    }
    let tokens = span_tokens(query, &spans);
    for connector in SIMULTANEITY_CONNECTORS {
        let needle: Vec<String> = normalize(connector).tokens().to_vec();
        if needle.is_empty() || needle.len() > tokens.len() {
            continue;
        }
        for start in 0..=(tokens.len() - needle.len()) {
            if tokens[start..start + needle.len()] != needle[..] {
                continue;
            }
            let left = query[..spans[start].0].trim();
            let right = query[spans[start + needle.len() - 1].1..].trim();
            let left = left.trim_end_matches(['.', ',', ';', ':']).trim();
            let right = right.trim_start_matches(['.', ',', ';', ':']).trim();
            if !left.is_empty() && !right.is_empty() {
                return Some((left, right));
            }
        }
    }
    None
}

/// Funde el acuse del `spawn` con la respuesta en primer plano.
///
/// El hilo central debe decir las dos cosas en **una sola** locución (§3.4), así
/// que el acuse viaja como instrucción de apertura del turno: la respuesta del
/// LLM empieza por la frase exacta y sigue con lo que el usuario preguntó.
pub fn fuse_ack_and_foreground(ack: &str, foreground: &str) -> String {
    let ack = ack.trim();
    let foreground = foreground.trim();
    if ack.is_empty() {
        return foreground.to_string();
    }
    if foreground.is_empty() {
        return ack.to_string();
    }
    format!(
        "Abre tu respuesta con esta frase exacta, sin comillas y sin nada antes: \
         {ack} Después, en la misma locución, atiende lo siguiente: {foreground}"
    )
}
