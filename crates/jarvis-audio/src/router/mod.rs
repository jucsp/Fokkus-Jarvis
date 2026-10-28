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
//! # Direccionamiento verbal (HU-64)
//!
//! Antes del planificador, [`addressing_plan`] resuelve la **etapa 2** de §3.4:
//! reconocer que el usuario está hablándole a una sub-sesión concreta y no al
//! hilo central. Es determinista (sin LLM, sin latencia) y reconoce:
//!
//! - *"en la revisión de PRs, enfócate en el módulo de pagos"* → `route`;
//! - *"entra al hilo de PRs"*, *"vuelve al hilo principal"* → foco;
//! - *"¿cómo va lo de las PR?"*, *"léeme el detalle de los logs"* → consulta;
//! - *"cierra el hilo de los logs"*, *"cancela la de Felipe"* → control.
//!
//! Las referencias (`<ref>`) se resuelven a [`SessionRef`]: id hablado ("hilo
//! 2"), ordinal ("el primero", "el último"), alias coloquial ("felipe", "logs")
//! o la sesión enfocada. La etapa 5 (modo foco) cierra el módulo: con un foco
//! activo, una continuación anafórica (*"ahora revisa pagos"*, *"léeme lo de la
//! 214"*) se enruta a esa sub-sesión en vez de al hilo central.

pub mod planner;

#[cfg(test)]
mod tests;

use crate::sessions::{SessionId, SessionProfile};
use crate::tasks::{
    dev_task_prompt, is_dev_task, is_operational_task, read_only_task_title, task_prompt,
    task_title, Workload,
};
use crate::text::normalize::{normalize, Normalized, HEAVY_VERBS};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

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

/// Referencia verbal a una sub-sesión (HU-64).
///
/// Es lo que el usuario dice, no lo que existe: la resolución contra el registro
/// real (títulos, alias, historial) la hace el orquestador, que es el único que
/// conoce las sub-sesiones abiertas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRef {
    /// Id hablado: "hilo 2" → `Id(2)`.
    Id(SessionId),
    /// Posición entre las abiertas: "el primero" → `Ordinal(0)`.
    Ordinal(usize),
    /// Nombre o alias coloquial: "felipe", "logs", "PR".
    Alias(String),
    /// La sub-sesión más reciente ("la última").
    Latest,
    /// La sub-sesión enfocada en este momento.
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
    /// Palabras con las que el usuario puede nombrarla por voz (HU-64).
    #[serde(default)]
    pub aliases: Vec<String>,
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

    /// Palabras con las que se puede nombrar esta sub-sesión (título + alias).
    fn tokens(&self) -> Vec<String> {
        let mut tokens = normalize(&self.title).tokens().to_vec();
        for alias in &self.aliases {
            tokens.extend(normalize(alias).tokens().iter().cloned());
        }
        tokens
    }
}

/// Contexto del enrutamiento.
pub struct RouteContext<'a> {
    /// Planificador LLM; `None` deja el router en modo heurístico.
    pub planner: Option<&'a dyn TurnPlanner>,
    /// Sub-sesiones activas (roster) para el prompt del planificador.
    pub roster: &'a [RosterEntry],
    /// Sub-sesión enfocada (HU-64): a quién van las continuaciones anafóricas.
    pub focus: Option<SessionId>,
}

impl RouteContext<'_> {
    /// Contexto sin planificador: solo heurística determinista.
    pub fn heuristic_only() -> Self {
        Self {
            planner: None,
            roster: &[],
            focus: None,
        }
    }
}

/// Llamada al planificador en un turno (HU-68): alimenta las métricas de
/// enrutamiento de `VoiceStats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerTrace {
    /// Tiempo de pared de la llamada.
    pub latency: Duration,
    /// El planificador falló o devolvió un plan vacío y se usó la heurística.
    pub fallback: bool,
}

/// Enrutador de turnos (HU-63).
pub struct TurnRouter;

impl TurnRouter {
    /// Construye el plan del turno.
    ///
    /// Orden: direccionamiento verbal explícito (HU-64, etapa 2) y modo foco
    /// (etapa 5) antes que nada; después, solo un turno **compuesto** (conector
    /// de simultaneidad y una mitad pesada junto a otra ligera) llega al
    /// planificador, y cualquier otro se resuelve con la heurística sin llamar
    /// al LLM (CA-3). Si el planificador falla o devuelve un plan vacío, se cae
    /// en la heurística (CA-2).
    pub fn route(query: &str, ctx: &RouteContext<'_>) -> TurnPlan {
        Self::route_traced(query, ctx).0
    }

    /// Igual que [`Self::route`], pero informa de la llamada al planificador
    /// (HU-68): `None` si el turno no llegó a él.
    pub fn route_traced(query: &str, ctx: &RouteContext<'_>) -> (TurnPlan, Option<PlannerTrace>) {
        if let Some(plan) = addressing_plan(query, ctx) {
            return (plan, None);
        }
        if !is_composite(query) {
            return (Self::heuristic_plan(query), None);
        }
        let Some(planner) = ctx.planner else {
            return (Self::heuristic_plan(query), None);
        };
        let started = Instant::now();
        let result = planner.plan(&PlanRequest {
            text: query,
            roster: ctx.roster,
        });
        let latency = started.elapsed();
        match result {
            Ok(plan) if !plan.is_empty() => (
                cap_profiles(plan, query),
                Some(PlannerTrace {
                    latency,
                    fallback: false,
                }),
            ),
            Ok(_) | Err(_) => (
                Self::heuristic_plan(query),
                Some(PlannerTrace {
                    latency,
                    fallback: true,
                }),
            ),
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

// ─── Etapa 2 y 5: direccionamiento verbal y modo foco (HU-64) ────────────────

/// Sustantivos que nombran una sub-sesión de forma explícita.
pub const SESSION_NOUNS: &[&str] = &[
    "hilo",
    "hilos",
    "sesion",
    "sesiones",
    "tarea",
    "tareas",
    "revision",
    "revisiones",
    "conversacion",
    "conversaciones",
    "subsesion",
    "subsesiones",
    "chat",
    "chats",
];

/// Determinantes, conectores y muletillas que no forman parte de la referencia.
const REF_STOPWORDS: &[&str] = &[
    "el", "la", "los", "las", "lo", "un", "una", "unos", "unas", "de", "del", "al", "a", "que",
    "mi", "mis", "tu", "tus", "su", "sus", "este", "esta", "estos", "estas", "ese", "esa", "esos",
    "esas", "aquel", "aquella", "ahi", "oye", "favor",
];

/// Preposiciones que enlazan el verbo con el sustantivo ("entra **al** hilo").
const LINK_WORDS: &[&str] = &["a", "al", "de", "del", "en", "hacia", "para"];

/// Verbos que abren el foco en una sub-sesión ya existente.
const FOCUS_VERBS: &[&str] = &[
    "entra",
    "entrar",
    "entremos",
    "metete",
    "vamos",
    "vete",
    "pasa",
    "pasate",
    "pasemos",
    "cambia",
    "cambiar",
    "cambiate",
    "vuelve",
    "volvamos",
    "volvemos",
    "regresa",
    "regresemos",
];

/// Verbos que cierran una sub-sesión.
const CLOSE_VERBS: &[&str] = &[
    "cierra",
    "cerrar",
    "cierrame",
    "cerremos",
    "finaliza",
    "finalizar",
    "acaba",
    "acabar",
];

/// Verbos que cancelan una sub-sesión por su nombre.
const CANCEL_VERBS: &[&str] = &[
    "cancela",
    "cancelar",
    "cancelame",
    "deten",
    "detener",
    "para",
    "parar",
    "aborta",
    "abortar",
    "mata",
    "matar",
    "termina",
    "terminar",
];

/// Verbos que piden leer el detalle de una sub-sesión.
const DETAIL_VERBS: &[&str] = &[
    "lee",
    "leeme",
    "leelo",
    "dime",
    "cuentame",
    "muestra",
    "muestrame",
    "detalla",
    "detallame",
    "explica",
    "explicame",
];

/// Frases que piden el detalle (no la instrucción) de una sub-sesión.
///
/// "Lo de la 214" queda **fuera** a propósito: es un elemento *dentro* de la
/// sub-sesión (el PR 214), no una sub-sesión, y debe enrutarse como instrucción.
const DETAIL_PHRASES: &[&str] = &[
    "el detalle",
    "lo que encontro",
    "lo que encontraste",
    "el resultado completo",
    "el informe completo",
    "las conclusiones",
];

/// Frases con las que se pregunta por el estado de una sub-sesión.
const STATUS_PHRASES: &[&str] = &[
    "como va",
    "como van",
    "que tal va",
    "que tal van",
    "como marcha",
    "como marchan",
    "en que va",
    "en que van",
    "como sigue",
    "como siguen",
];

/// Verbos que abren una instrucción dirigida a la sub-sesión enfocada.
const INSTRUCTION_MARKERS: &[&str] = &[
    "enfocate",
    "centrate",
    "concentrate",
    "revisa",
    "revisar",
    "lee",
    "leer",
    "mira",
    "mirar",
    "analiza",
    "analizar",
    "busca",
    "buscar",
    "agrega",
    "anade",
    "incluye",
    "suma",
    "quita",
    "elimina",
    "corrige",
    "arregla",
    "prioriza",
    "descarta",
    "cambia",
    "actualiza",
    "compara",
    "resume",
    "completa",
    "amplia",
    "profundiza",
    "continua",
    "sigue",
    "empieza",
    "prueba",
    "valida",
    "comprueba",
    "escribe",
    "prepara",
    "haz",
];

/// Muletillas anafóricas: el turno sigue la conversación anterior.
const ANAPHORS: &[&str] = &[
    "tambien", "ademas", "ahora", "luego", "despues", "aparte", "y", "entonces",
];

/// Verbos que, al inicio del turno, marcan una instrucción sobre el foco.
const FOCUS_INSTRUCTION_VERBS: &[&str] = &[
    "agrega",
    "anade",
    "incluye",
    "suma",
    "quita",
    "elimina",
    "sigue",
    "continua",
    "enfocate",
    "centrate",
    "concentrate",
    "prioriza",
    "descarta",
    "revisa",
    "lee",
    "mira",
    "chequea",
    "analiza",
    "busca",
    "compara",
    "resume",
    "detalla",
    "amplia",
    "profundiza",
    "corrige",
    "arregla",
    "actualiza",
    "cambia",
    "dale",
    "pon",
    "vuelve",
    "dime",
    "explica",
    "cuentame",
    "empieza",
    "termina",
    "prueba",
    "valida",
    "haz",
    "apunta",
    "anota",
];

/// Temas que siempre son del hilo central, aunque haya un foco activo.
const GENERAL_TOPICS: &[&str] = &[
    "el clima",
    "la hora",
    "que hora",
    "un chiste",
    "las noticias",
    "quien eres",
    "que dia es",
    "que significa",
    "el pronostico",
];

/// Aperturas de pregunta general (no de continuación sobre el foco).
const QUESTION_OPENERS: &[&str] = &[
    "que", "quien", "quienes", "donde", "cuando", "cuanto", "cuanta", "cuantos", "cuantas", "cual",
    "cuales", "como", "por",
];

/// Plan de direccionamiento verbal explícito (etapa 2) o de continuación sobre
/// la sub-sesión enfocada (etapa 5). `None` deja el turno al resto del router.
pub fn addressing_plan(query: &str, ctx: &RouteContext<'_>) -> Option<TurnPlan> {
    let normalized = normalize(query);
    if normalized.is_empty() {
        return None;
    }
    let spans = word_spans(query);
    if spans.is_empty() {
        return None;
    }
    let tokens = span_tokens(query, &spans);
    if is_unfocus_turn(&tokens) {
        return Some(TurnPlan {
            control: vec![SessionControl::Unfocus],
            ..TurnPlan::default()
        });
    }
    if let Some(plan) = addressed_instruction(query, &spans, &tokens, ctx) {
        return Some(plan);
    }
    if let Some(plan) = status_plan(&tokens, ctx) {
        return Some(plan);
    }
    if let Some(plan) = detail_plan(&tokens, ctx) {
        return Some(plan);
    }
    if let Some(plan) = control_plan(&tokens, ctx) {
        return Some(plan);
    }
    focus_continuation(&normalized, query, ctx.focus?)
}

/// `true` si el turno pide volver al hilo central ("vuelve al hilo principal",
/// "sal del hilo", "desenfócate").
fn is_unfocus_turn(tokens: &[String]) -> bool {
    let has = |word: &str| tokens.iter().any(|token| token == word);
    if ["desenfoca", "desenfocate", "desenfocar", "desenfocame"]
        .iter()
        .any(|word| has(word))
    {
        return true;
    }
    if tokens
        .windows(2)
        .any(|w| w[0] == "hilo" && matches!(w[1].as_str(), "principal" | "central"))
    {
        return true;
    }
    if tokens
        .windows(2)
        .any(|w| matches!(w[0].as_str(), "lo" | "hilo" | "al" | "a") && w[1] == "general")
    {
        return true;
    }
    let returns = tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "vuelve" | "volvamos" | "volvemos" | "regresa" | "regresemos"
        )
    });
    if returns && has("contigo") {
        return true;
    }
    let exits = tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "sal" | "salte" | "salir" | "salgamos" | "suelta" | "sueltame"
        )
    });
    exits && has("hilo")
}

/// `en (el|la) (hilo|sesión|tarea|revisión) (de)? <ref>, <instrucción>`.
///
/// El corte entre la referencia y la instrucción se hace por el verbo de la
/// instrucción, no por la coma: Whisper no la escribe de forma fiable.
fn addressed_instruction(
    query: &str,
    spans: &[(usize, usize)],
    tokens: &[String],
    ctx: &RouteContext<'_>,
) -> Option<TurnPlan> {
    let mut i = tokens
        .iter()
        .position(|token| !LEADING_FILLERS.contains(&token.as_str()))?;
    if tokens.get(i).map(String::as_str) != Some("en") {
        return None;
    }
    i += 1;
    if i < tokens.len() && DETERMINERS.contains(&tokens[i].as_str()) {
        i += 1;
    }
    if i >= tokens.len() || !SESSION_NOUNS.contains(&tokens[i].as_str()) {
        return None;
    }
    i += 1;
    if i < tokens.len() && LINK_WORDS.contains(&tokens[i].as_str()) {
        i += 1;
    }
    let start = i;
    let marker =
        (start..tokens.len()).find(|k| INSTRUCTION_MARKERS.contains(&tokens[*k].as_str()))?;
    let reference = session_ref(&tokens[start..marker]).unwrap_or_else(|| default_reference(ctx));
    let id = resolve_reference(&reference, ctx)?;
    let instruction = query[spans[marker].0..].trim();
    if instruction.is_empty() {
        return None;
    }
    Some(TurnPlan {
        route: vec![(id, instruction.to_string())],
        ..TurnPlan::default()
    })
}

/// `¿cómo va(n) …?` → consulta de estado (o listado si no hay a quién preguntar).
fn status_plan(tokens: &[String], ctx: &RouteContext<'_>) -> Option<TurnPlan> {
    let end = phrase_end(tokens, STATUS_PHRASES)?;
    let query = match session_ref(&tokens[end..]) {
        Some(reference) => SessionQuery::Status(reference),
        // Sin referencia explícita: el foco manda; si no, la única abierta; si
        // hay varias, se listan (CA-3).
        None if ctx.focus.is_some() => SessionQuery::Status(SessionRef::Focused),
        None if ctx.roster.len() == 1 => SessionQuery::Status(SessionRef::Latest),
        None => SessionQuery::ListAll,
    };
    Some(TurnPlan {
        query: vec![query],
        ..TurnPlan::default()
    })
}

/// `léeme (el detalle|lo que encontró) (de)? <ref>` → consulta de detalle.
fn detail_plan(tokens: &[String], ctx: &RouteContext<'_>) -> Option<TurnPlan> {
    if !tokens
        .iter()
        .any(|token| DETAIL_VERBS.contains(&token.as_str()))
    {
        return None;
    }
    let end = phrase_end(tokens, DETAIL_PHRASES)?;
    let reference = session_ref(&tokens[end..]).unwrap_or_else(|| default_reference(ctx));
    Some(TurnPlan {
        query: vec![SessionQuery::ReadDetail(reference)],
        ..TurnPlan::default()
    })
}

/// Foco, cierre o cancelación de una sub-sesión.
fn control_plan(tokens: &[String], ctx: &RouteContext<'_>) -> Option<TurnPlan> {
    // El orden importa: "termina"/"para" valen para cerrar y para cancelar;
    // cancelar es la lectura que espera el usuario.
    [
        (CANCEL_VERBS, ControlKind::Cancel),
        (CLOSE_VERBS, ControlKind::Close),
        (FOCUS_VERBS, ControlKind::Focus),
    ]
    .into_iter()
    .find_map(|(verbs, kind)| control_with(tokens, verbs, kind, ctx))
}

/// Qué control pide el verbo encontrado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Focus,
    Close,
    Cancel,
}

fn control_with(
    tokens: &[String],
    verbs: &[&str],
    kind: ControlKind,
    ctx: &RouteContext<'_>,
) -> Option<TurnPlan> {
    let at = tokens
        .iter()
        .position(|token| verbs.contains(&token.as_str()))?;
    let reference = reference_after_verb(&tokens[at + 1..], kind, ctx)?;
    let control = match kind {
        ControlKind::Focus => SessionControl::Focus(reference),
        ControlKind::Close => SessionControl::Close(reference),
        ControlKind::Cancel => SessionControl::Cancel(reference),
    };
    Some(TurnPlan {
        control: vec![control],
        ..TurnPlan::default()
    })
}

/// Referencia que sigue a un verbo de control.
///
/// Exige un sustantivo de sub-sesión o la forma "la de <nombre>": así "para el
/// monitor" o "compra un cable para el hilo" no se leen como órdenes de control.
fn reference_after_verb(
    rest: &[String],
    kind: ControlKind,
    ctx: &RouteContext<'_>,
) -> Option<SessionRef> {
    let skip = |from: usize| -> usize {
        let mut i = from;
        while i < rest.len()
            && (DETERMINERS.contains(&rest[i].as_str()) || LINK_WORDS.contains(&rest[i].as_str()))
        {
            i += 1;
        }
        i
    };
    // "cancela la de Felipe": artículo + "de" + nombre. (`DETERMINERS` incluye
    // "de", así que aquí solo se saltan artículos.)
    let named_at = rest
        .iter()
        .position(|token| !matches!(token.as_str(), "el" | "la" | "los" | "las" | "lo"))
        .unwrap_or(rest.len());
    let named =
        named_at > 0 && matches!(rest.get(named_at).map(String::as_str), Some("de" | "del"));
    let mut i = skip(0);
    let saw_noun = i < rest.len() && SESSION_NOUNS.contains(&rest[i].as_str());
    if saw_noun {
        i = skip(i + 1);
    }
    // "cierra el primero", "entra al último": el ordinal ya nombra un hilo.
    let ordinal = rest[i..]
        .first()
        .is_some_and(|token| spoken_ordinal(token).is_some());
    let has_target = saw_noun || named || ordinal;
    if !has_target {
        return None;
    }
    match session_ref(&rest[i..]) {
        Some(reference) => Some(reference),
        None if kind == ControlKind::Focus && !saw_noun => None,
        None => Some(default_reference(ctx)),
    }
}

/// Referencia por defecto cuando el usuario no nombra la sub-sesión.
fn default_reference(ctx: &RouteContext<'_>) -> SessionRef {
    if ctx.focus.is_some() {
        SessionRef::Focused
    } else {
        SessionRef::Latest
    }
}

/// Resuelve una referencia contra el roster (`None` si no hay coincidencia).
fn resolve_reference(reference: &SessionRef, ctx: &RouteContext<'_>) -> Option<SessionId> {
    match reference {
        // El id hablado se pasa tal cual: quien conoce el registro es el
        // orquestador, y responder "no existe" es su trabajo.
        SessionRef::Id(id) => Some(*id),
        other => resolve_in_roster(other, ctx.roster, ctx.focus)
            .first()
            .copied(),
    }
}

/// Candidatas del roster para una referencia hablada (HU-64).
///
/// Vacío = nada coincide; más de una = ambigüedad que el orquestador debe
/// preguntar ("¿La de Felipe o la de Oscar?"). Los ordinales cuentan por id
/// creciente (el orden de apertura); "la última" es el id mayor.
pub fn resolve_in_roster(
    reference: &SessionRef,
    roster: &[RosterEntry],
    focus: Option<SessionId>,
) -> Vec<SessionId> {
    match reference {
        SessionRef::Id(id) => roster
            .iter()
            .filter(|entry| entry.id == *id)
            .map(|entry| entry.id)
            .collect(),
        SessionRef::Ordinal(index) => {
            let mut ids: Vec<SessionId> = roster.iter().map(|entry| entry.id).collect();
            ids.sort_unstable();
            ids.get(*index).copied().into_iter().collect()
        }
        SessionRef::Latest => roster
            .iter()
            .map(|entry| entry.id)
            .max()
            .into_iter()
            .collect(),
        SessionRef::Focused => focus.into_iter().collect(),
        SessionRef::Alias(alias) => roster_candidates(alias, roster),
    }
}

/// `true` si dos palabras nombran lo mismo, tolerando el plural ("prs"/"pr").
fn same_word(a: &str, b: &str) -> bool {
    let singular = |word: &str| -> String {
        match word.strip_suffix("es").or_else(|| word.strip_suffix('s')) {
            Some(stem) if stem.len() >= 2 => stem.to_string(),
            _ => word.to_string(),
        }
    };
    a == b || singular(a) == singular(b)
}

/// Sub-sesiones del roster con el mejor *score* por tokens para `alias`.
///
/// Empate = ambigüedad; se devuelven de la más reciente a la más antigua.
fn roster_candidates(alias: &str, roster: &[RosterEntry]) -> Vec<SessionId> {
    let needle = normalize(alias);
    let wanted = needle.tokens();
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut best = 0;
    let mut ids: Vec<SessionId> = Vec::new();
    for entry in roster {
        let tokens = entry.tokens();
        let score = wanted
            .iter()
            .filter(|word| tokens.iter().any(|token| same_word(word, token)))
            .count();
        if score == 0 || score < best {
            continue;
        }
        if score > best {
            best = score;
            ids.clear();
        }
        ids.push(entry.id);
    }
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// Primera aparición de alguna de `phrases`; devuelve el índice siguiente.
fn phrase_end(tokens: &[String], phrases: &[&str]) -> Option<usize> {
    for phrase in phrases {
        let needle: Vec<String> = normalize(phrase).tokens().to_vec();
        if needle.is_empty() || needle.len() > tokens.len() {
            continue;
        }
        for start in 0..=(tokens.len() - needle.len()) {
            if tokens[start..start + needle.len()] == needle[..] {
                return Some(start + needle.len());
            }
        }
    }
    None
}

/// Referencia hablada a partir de sus palabras ("hilo 2", "el primero", "felipe").
fn session_ref(tokens: &[String]) -> Option<SessionRef> {
    let kept: Vec<&str> = tokens
        .iter()
        .map(String::as_str)
        .filter(|token| !REF_STOPWORDS.contains(token) && !SESSION_NOUNS.contains(token))
        .collect();
    if kept.is_empty() {
        return None;
    }
    if kept.len() == 1 {
        if let Some(id) = spoken_number(kept[0]) {
            return Some(SessionRef::Id(id));
        }
    }
    for token in &kept {
        if let Some(ordinal) = spoken_ordinal(token) {
            return Some(ordinal);
        }
    }
    Some(SessionRef::Alias(kept.join(" ")))
}

/// Número dicho en voz alta ("2", "dos").
fn spoken_number(token: &str) -> Option<SessionId> {
    if !token.is_empty() && token.chars().all(|c| c.is_ascii_digit()) {
        return token.parse().ok().filter(|n: &SessionId| *n > 0);
    }
    match token {
        "uno" => Some(1),
        "dos" => Some(2),
        "tres" => Some(3),
        "cuatro" => Some(4),
        "cinco" => Some(5),
        "seis" => Some(6),
        _ => None,
    }
}

/// Ordinal hablado ("el primero", "el último").
fn spoken_ordinal(token: &str) -> Option<SessionRef> {
    let index = match token {
        "primero" | "primera" | "primer" => 0,
        "segundo" | "segunda" => 1,
        "tercero" | "tercera" | "tercer" => 2,
        "cuarto" | "cuarta" => 3,
        "quinto" | "quinta" => 4,
        "sexto" | "sexta" => 5,
        "ultimo" | "ultima" => return Some(SessionRef::Latest),
        _ => return None,
    };
    Some(SessionRef::Ordinal(index))
}

/// `true` si el turno es una pregunta general y no una instrucción al foco.
fn is_general_question(normalized: &Normalized) -> bool {
    if normalized.has_any(GENERAL_TOPICS) {
        return true;
    }
    let first = normalized
        .tokens()
        .first()
        .map(String::as_str)
        .unwrap_or_default();
    QUESTION_OPENERS.contains(&first)
}

/// `true` si el turno continúa la conversación de la sub-sesión enfocada.
fn is_focused_continuation(normalized: &Normalized) -> bool {
    let first = normalized
        .tokens()
        .first()
        .map(String::as_str)
        .unwrap_or_default();
    if FOCUS_INSTRUCTION_VERBS.contains(&first) || ANAPHORS.contains(&first) {
        return true;
    }
    normalized.has_any(&[
        "lo de",
        "otra cosa",
        "sigue con",
        "vuelve a",
        "el detalle de",
    ])
}

/// Etapa 5: con un foco activo, una instrucción o continuación va a esa
/// sub-sesión; una pregunta general sigue en el hilo central.
fn focus_continuation(normalized: &Normalized, query: &str, focus: SessionId) -> Option<TurnPlan> {
    if is_general_question(normalized) || !is_focused_continuation(normalized) {
        return None;
    }
    Some(TurnPlan {
        route: vec![(focus, query.trim().to_string())],
        ..TurnPlan::default()
    })
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
    voice_aliases(title, instruction, 3)
}

/// Palabras útiles con las que el usuario puede nombrar una sub-sesión por voz.
///
/// Las usa el router al abrir una sub-sesión (HU-63) y el orquestador al
/// construir el roster que resuelve las referencias habladas (HU-64).
pub fn voice_aliases(title: &str, instruction: &str, max: usize) -> Vec<String> {
    let mut aliases: Vec<String> = Vec::new();
    for normalized in [normalize(title), normalize(instruction)] {
        for token in normalized.tokens() {
            let usable = token.len() >= 3
                && !ALIAS_STOPWORDS.contains(&token.as_str())
                && !aliases.iter().any(|known| known == token);
            if usable {
                aliases.push(token.clone());
            }
            if aliases.len() >= max {
                return aliases;
            }
        }
    }
    if aliases.is_empty() {
        // Sin palabras útiles ("revisa eso"): el título tal cual.
        let whole = normalize(title).text().trim().to_string();
        if !whole.is_empty() {
            aliases.push(whole);
        }
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
