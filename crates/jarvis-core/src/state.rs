//! Máquina de estados pura del asistente.

use std::fmt;

/// Estados del ciclo de vida de Jarvis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum JarvisState {
    /// Sin actividad; esperando wake-word / atajo.
    #[default]
    Idle,
    /// Capturando voz del usuario.
    Listening,
    /// Procesando el comando (LLM / herramientas).
    Thinking,
    /// Reproduciendo la respuesta por voz.
    Speaking,
    /// Silenciado globalmente.
    Muted,
}

impl JarvisState {
    /// Todos los estados, útil para recorridos exhaustivos en tests y render.
    pub const ALL: [JarvisState; 5] = [
        JarvisState::Idle,
        JarvisState::Listening,
        JarvisState::Thinking,
        JarvisState::Speaking,
        JarvisState::Muted,
    ];

    /// Tabla de transiciones permitidas.
    pub fn can_transition_to(self, to: JarvisState) -> bool {
        use JarvisState::*;
        match self {
            Idle => matches!(to, Listening | Thinking | Muted),
            Listening => matches!(to, Thinking | Idle | Muted),
            Thinking => matches!(to, Speaking | Listening | Idle | Muted),
            Speaking => matches!(to, Listening | Idle | Muted),
            Muted => matches!(to, Idle),
        }
    }

    /// `true` si el asistente está emitiendo audio.
    pub fn is_speaking(self) -> bool {
        matches!(self, JarvisState::Speaking)
    }

    /// `true` si el asistente está silenciado.
    pub fn is_muted(self) -> bool {
        matches!(self, JarvisState::Muted)
    }
}

/// Estados visuales de la esfera líquida (HU-55).
///
/// [`JarvisState`] describe el ciclo de vida del asistente; este enum describe
/// lo que el usuario debe *ver*. Son dos ejes distintos a propósito: el ciclo
/// de vida tiene cinco estados, pero visualmente hay que distinguir, por un
/// lado, la espera pasiva de la captura de un turno (`PassiveListening` frente
/// a `ActiveListening`) y, por otro, la presencia de tareas en segundo plano
/// (`BackgroundActive`), que no cambia el estado de la máquina pero sí lo que
/// conviene mostrar.
///
/// La conversión [`From<JarvisState>`] cubre el mapeo directo (identidad en
/// `Idle`, `Thinking`, `Speaking` y `Muted`; `Listening` → `PassiveListening`).
/// Los dos estados que *no* se deducen del ciclo de vida los decide quien
/// conoce el contexto: el orquestador sabe si hay un turno abierto
/// (`ActiveListening`) y si quedan tareas activas (`BackgroundActive`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SphereVisualState {
    /// Sin actividad; esperando wake-word / atajo. Cian.
    #[default]
    Idle,
    /// Escuchando en reposo, sin turno abierto (verde esmeralda).
    PassiveListening,
    /// Capturando un turno en vivo tras el wake-word o en ventana de
    /// seguimiento (amarillo ámbar).
    ActiveListening,
    /// Procesando el comando con el LLM o herramientas (púrpura).
    Thinking,
    /// Reproduciendo la respuesta por voz (azul).
    Speaking,
    /// Hay subagentes o tareas activas en segundo plano (naranja cálido).
    BackgroundActive,
    /// Silenciado globalmente (gris).
    Muted,
}

impl SphereVisualState {
    /// Todos los estados, útil para recorridos exhaustivos en tests y render.
    pub const ALL: [SphereVisualState; 7] = [
        SphereVisualState::Idle,
        SphereVisualState::PassiveListening,
        SphereVisualState::ActiveListening,
        SphereVisualState::Thinking,
        SphereVisualState::Speaking,
        SphereVisualState::BackgroundActive,
        SphereVisualState::Muted,
    ];

    /// `true` si el estado representa escucha, pasiva o activa.
    pub fn is_listening(self) -> bool {
        matches!(
            self,
            SphereVisualState::PassiveListening | SphereVisualState::ActiveListening
        )
    }

    /// `true` si el estado anuncia trabajo en segundo plano.
    pub fn is_background(self) -> bool {
        matches!(self, SphereVisualState::BackgroundActive)
    }
}

impl From<JarvisState> for SphereVisualState {
    fn from(state: JarvisState) -> Self {
        match state {
            JarvisState::Idle => SphereVisualState::Idle,
            // El ciclo de vida solo sabe que se está capturando voz; que el
            // turno esté abierto (y por tanto que la escucha sea activa) es
            // contexto del orquestador.
            JarvisState::Listening => SphereVisualState::PassiveListening,
            JarvisState::Thinking => SphereVisualState::Thinking,
            JarvisState::Speaking => SphereVisualState::Speaking,
            JarvisState::Muted => SphereVisualState::Muted,
        }
    }
}

/// Transición de estado observada por el bus de eventos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateTransition {
    pub from: JarvisState,
    pub to: JarvisState,
}

/// Error al intentar una transición no permitida.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionError {
    /// La transición no está en la tabla permitida.
    InvalidTransition { from: JarvisState, to: JarvisState },
    /// Transición a sí mismo (no-op prohibido para no emitir eventos espurios).
    SelfTransition(JarvisState),
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransitionError::InvalidTransition { from, to } => {
                write!(f, "transición inválida: {from:?} -> {to:?}")
            }
            TransitionError::SelfTransition(s) => {
                write!(f, "transición a sí mismo no permitida: {s:?}")
            }
        }
    }
}

impl std::error::Error for TransitionError {}

/// Máquina de estados con arranque en `Idle`.
#[derive(Debug, Clone, Default)]
pub struct StateMachine {
    state: JarvisState,
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            state: JarvisState::Idle,
        }
    }

    pub fn state(&self) -> JarvisState {
        self.state
    }

    /// Aplica una transición si es válida.
    pub fn transition(&mut self, to: JarvisState) -> Result<StateTransition, TransitionError> {
        if to == self.state {
            return Err(TransitionError::SelfTransition(self.state));
        }
        if !self.state.can_transition_to(to) {
            return Err(TransitionError::InvalidTransition {
                from: self.state,
                to,
            });
        }
        let from = self.state;
        self.state = to;
        Ok(StateTransition { from, to })
    }

    /// Vuelve al estado inicial.
    pub fn reset(&mut self) {
        self.state = JarvisState::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use JarvisState::*;

    /// Tabla de transiciones esperada (fila = origen, columna = destino), en el
    /// orden de [`JarvisState::ALL`]: Idle, Listening, Thinking, Speaking, Muted.
    const EXPECTED: [[bool; 5]; 5] = [
        [false, true, true, false, true],   // Idle
        [true, false, true, false, true],   // Listening
        [true, true, false, true, true],    // Thinking
        [true, true, false, false, true],   // Speaking
        [true, false, false, false, false], // Muted
    ];

    #[test]
    fn transition_table_is_fully_covered() {
        for (i, from) in JarvisState::ALL.into_iter().enumerate() {
            for (j, to) in JarvisState::ALL.into_iter().enumerate() {
                let mut sm = StateMachine { state: from };
                let result = sm.transition(to);
                if from == to {
                    assert_eq!(result, Err(TransitionError::SelfTransition(from)));
                    assert_eq!(sm.state(), from);
                } else if EXPECTED[i][j] {
                    assert_eq!(
                        result,
                        Ok(StateTransition { from, to }),
                        "{from:?} -> {to:?}"
                    );
                    assert_eq!(sm.state(), to);
                } else {
                    assert_eq!(
                        result,
                        Err(TransitionError::InvalidTransition { from, to }),
                        "{from:?} -> {to:?}"
                    );
                    assert_eq!(sm.state(), from, "estado alterado tras rechazo");
                }
            }
        }
    }

    #[test]
    fn reset_returns_to_idle() {
        let mut sm = StateMachine::new();
        sm.transition(Listening).unwrap();
        sm.reset();
        assert_eq!(sm.state(), Idle);
        assert_eq!(StateMachine::default().state(), Idle);
    }

    #[test]
    fn predicates_match_state() {
        for s in JarvisState::ALL {
            assert_eq!(s.is_speaking(), s == Speaking);
            assert_eq!(s.is_muted(), s == Muted);
        }
    }

    /// Tabla de conversión esperada, en el orden de [`JarvisState::ALL`]:
    /// Idle, Listening, Thinking, Speaking, Muted.
    const EXPECTED_VISUAL: [SphereVisualState; 5] = [
        SphereVisualState::Idle,
        SphereVisualState::PassiveListening,
        SphereVisualState::Thinking,
        SphereVisualState::Speaking,
        SphereVisualState::Muted,
    ];

    #[test]
    fn visual_state_default_is_idle() {
        assert_eq!(SphereVisualState::default(), SphereVisualState::Idle);
        let s: SphereVisualState = JarvisState::default().into();
        assert_eq!(s, SphereVisualState::Idle);
    }

    #[test]
    fn from_jarvis_state_table_is_fully_covered() {
        for (state, expected) in JarvisState::ALL.into_iter().zip(EXPECTED_VISUAL) {
            let got = SphereVisualState::from(state);
            assert_eq!(got, expected, "conversión de {state:?}");
            // La conversión debe funcionar también por `Into` (lo que consume
            // `SphereRenderer::render_into`).
            let via_into: SphereVisualState = state.into();
            assert_eq!(via_into, expected);
        }
    }

    #[test]
    fn from_jarvis_state_never_invents_turn_or_background() {
        // `ActiveListening` y `BackgroundActive` dependen de contexto que la
        // máquina de estados no tiene: la conversión no debe fabricarlos.
        for state in JarvisState::ALL {
            let visual = SphereVisualState::from(state);
            assert_ne!(visual, SphereVisualState::ActiveListening, "{state:?}");
            assert!(!visual.is_background(), "{state:?} -> {visual:?}");
        }
    }

    #[test]
    fn visual_states_are_unique_and_complete() {
        let mut seen = std::collections::HashSet::new();
        for state in SphereVisualState::ALL {
            assert!(seen.insert(state), "estado duplicado en ALL: {state:?}");
        }
        assert_eq!(seen.len(), SphereVisualState::ALL.len());
        // Un representante por variante: si se añade una variante y se olvida
        // en `ALL`, este conteo deja de cuadrar.
        let all_variants = [
            SphereVisualState::Idle,
            SphereVisualState::PassiveListening,
            SphereVisualState::ActiveListening,
            SphereVisualState::Thinking,
            SphereVisualState::Speaking,
            SphereVisualState::BackgroundActive,
            SphereVisualState::Muted,
        ];
        assert_eq!(all_variants.len(), SphereVisualState::ALL.len());
        for state in all_variants {
            assert!(seen.contains(&state), "variante fuera de ALL: {state:?}");
        }
    }

    #[test]
    fn visual_state_predicates_match_state() {
        // (estado, es escucha, es background)
        let cases = [
            (SphereVisualState::Idle, false, false),
            (SphereVisualState::PassiveListening, true, false),
            (SphereVisualState::ActiveListening, true, false),
            (SphereVisualState::Thinking, false, false),
            (SphereVisualState::Speaking, false, false),
            (SphereVisualState::BackgroundActive, false, true),
            (SphereVisualState::Muted, false, false),
        ];
        for (state, listening, background) in cases {
            assert_eq!(state.is_listening(), listening, "{state:?}");
            assert_eq!(state.is_background(), background, "{state:?}");
        }
        // Un caso por variante: si se añade una y se olvida aquí, no cuadra.
        assert_eq!(cases.len(), SphereVisualState::ALL.len());
    }

    #[test]
    fn visual_states_are_hashable_and_copyable() {
        let mut counts = std::collections::HashMap::new();
        for state in SphereVisualState::ALL {
            *counts.entry(state).or_insert(0u32) += 1;
        }
        assert_eq!(counts.len(), SphereVisualState::ALL.len());
        assert!(counts.values().all(|&c| c == 1));
    }
}
