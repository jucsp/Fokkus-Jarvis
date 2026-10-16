//! Núcleo compartido entre hilos: la única fuente de verdad del estado.
//!
//! La esfera (bucle de render), el servidor IPC y el orquestador de voz leen y
//! cambian el estado sobre el mismo [`JarvisCore`] (SDD v0.2.0 §5). El lock
//! solo se toma para operaciones O(1) y nunca durante I/O, así que el bucle de
//! render puede consultarlo en cada frame sin riesgo de bloquearse.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::state::{JarvisState, StateTransition, TransitionError};
use crate::JarvisCore;

/// `JarvisCore` compartido y clonable (`Arc<Mutex<…>>`).
///
/// Nunca entra en pánico por envenenamiento: un pánico previo con el lock
/// tomado no deja la máquina de estados inconsistente (cada transición es
/// atómica), así que se recupera el valor con
/// [`std::sync::PoisonError::into_inner`].
#[derive(Clone)]
pub struct SharedCore(Arc<Mutex<JarvisCore>>);

impl SharedCore {
    pub fn new(core: JarvisCore) -> Self {
        Self(Arc::new(Mutex::new(core)))
    }

    /// Acceso exclusivo al núcleo. No retener el guard durante I/O.
    pub fn lock(&self) -> MutexGuard<'_, JarvisCore> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn state(&self) -> JarvisState {
        self.lock().state()
    }

    pub fn is_muted(&self) -> bool {
        self.lock().is_muted()
    }

    pub fn transition(&self, to: JarvisState) -> Result<StateTransition, TransitionError> {
        self.lock().transition(to)
    }

    /// Transición condicionada: solo se aplica si el estado actual es `from`.
    ///
    /// Comprobar y transicionar ocurre bajo el mismo lock, así que dos hilos
    /// que compiten (p. ej. el barge-in y el fin de una locución) no pueden
    /// pisarse. Devuelve `None` si el estado ya no era `from`.
    pub fn transition_from(
        &self,
        from: JarvisState,
        to: JarvisState,
    ) -> Option<Result<StateTransition, TransitionError>> {
        let mut core = self.lock();
        (core.state() == from).then(|| core.transition(to))
    }

    /// `true` si ambos apuntan al mismo núcleo.
    pub fn ptr_eq(&self, other: &SharedCore) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for SharedCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SharedCore").field(&self.state()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JarvisConfig;

    #[test]
    fn clones_share_the_same_state() {
        let a = SharedCore::new(JarvisCore::new(JarvisConfig::default()));
        let b = a.clone();
        a.transition(JarvisState::Listening).unwrap();
        assert_eq!(b.state(), JarvisState::Listening);
        assert!(a.ptr_eq(&b));
    }

    #[test]
    fn transition_from_only_applies_when_state_matches() {
        let core = SharedCore::new(JarvisCore::new(JarvisConfig::default()));
        assert!(core
            .transition_from(JarvisState::Listening, JarvisState::Thinking)
            .is_none());
        assert_eq!(core.state(), JarvisState::Idle);
        let t = core
            .transition_from(JarvisState::Idle, JarvisState::Listening)
            .expect("estado coincide")
            .expect("transición válida");
        assert_eq!(t.to, JarvisState::Listening);
    }

    #[test]
    fn survives_a_poisoned_lock() {
        let core = SharedCore::new(JarvisCore::new(JarvisConfig::default()));
        let clone = core.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.lock();
            panic!("pánico con el lock tomado");
        })
        .join();
        assert_eq!(core.state(), JarvisState::Idle);
        core.transition(JarvisState::Muted).unwrap();
        assert!(core.is_muted());
    }
}
