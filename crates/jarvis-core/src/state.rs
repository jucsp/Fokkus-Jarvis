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
}
