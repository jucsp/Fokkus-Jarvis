//! Núcleo de Fokkus-Jarvis.
//!
//! Este crate agrupa la máquina de estados del asistente, la configuración de
//! usuario (persistida en TOML), el bus de eventos (nivel de audio y
//! transiciones de estado) y el lock de instancia única. No realiza I/O de UI
//! ni de audio: es puramente testeable.

mod bus;
mod config;
mod fullscreen;
mod paths;
mod shared;
pub mod single_instance;
mod state;

pub use bus::{AudioLevel, AudioLevelStore, EventBus};
pub use config::{
    sphere_size_for_pct, AutomationConfig, BrainMode, ConfigError, JarvisConfig, Opacity,
    VoiceEndpointConfig, CONNECTOR_GRACE_MS_RANGE, DEFAULT_CLI_COMMAND, DEFAULT_SPHERE_SIZE_PX,
    EXTENDED_SILENCE_MS_RANGE, FULLSCREEN_PCT_RANGE, HUD_MAX_BUBBLES_RANGE, MAX_SEGMENT_MS_RANGE,
    MAX_TURN_MS_RANGE, SILENCE_MS_RANGE, SPHERE_OPACITY_RANGE, SPHERE_SCALE_RANGE,
    VOICE_HOLD_RANGE, VOICE_OPEN_RANGE,
};
pub use fullscreen::{
    is_effectively_fullscreen, FullscreenBehavior, OutputGeometry, Rect, WindowSnapshot,
};
pub use paths::{state_dir, tasks_dir};
pub use shared::SharedCore;
pub use state::{JarvisState, StateMachine, StateTransition, TransitionError};

/// Orquestador mínimo del núcleo: estado + configuración + bus de eventos.
pub struct JarvisCore {
    config: JarvisConfig,
    state_machine: StateMachine,
    bus: EventBus,
}

impl JarvisCore {
    /// Crea un núcleo en estado `Idle` con la configuración dada.
    pub fn new(config: JarvisConfig) -> Self {
        Self {
            config,
            state_machine: StateMachine::new(),
            bus: EventBus::new(),
        }
    }

    /// Estado actual del asistente.
    pub fn state(&self) -> JarvisState {
        self.state_machine.state()
    }

    /// Configuración activa (referencia inmutable).
    pub fn config(&self) -> &JarvisConfig {
        &self.config
    }

    /// Bus de eventos para suscribirse a nivel de audio y transiciones.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// `true` cuando el asistente está silenciado.
    pub fn is_muted(&self) -> bool {
        self.state_machine.state().is_muted()
    }

    /// Reemplaza la configuración en caliente.
    pub fn set_config(&mut self, config: JarvisConfig) {
        self.config = config;
    }

    /// Intenta una transición de estado y la publica en el bus.
    ///
    /// Devuelve la transición realizada o [`TransitionError`] si no es válida.
    pub fn transition(&mut self, to: JarvisState) -> Result<StateTransition, TransitionError> {
        let transition = self.state_machine.transition(to)?;
        self.bus
            .publish_state_transition(transition.from, transition.to);
        Ok(transition)
    }

    /// Alterna el silencio: `Idle ↔ Muted`.
    pub fn toggle_mute(&mut self) -> Result<StateTransition, TransitionError> {
        let target = if self.is_muted() {
            JarvisState::Idle
        } else {
            JarvisState::Muted
        };
        self.transition(target)
    }

    /// Publica el nivel de audio (0.0..=1.0); se satura fuera de rango.
    ///
    /// Pensado para ser llamado desde el callback de audio: no bloquea.
    pub fn set_audio_level(&self, level: AudioLevel) {
        self.bus.publish_audio_level(level);
    }

    /// Último nivel de audio publicado.
    pub fn audio_level(&self) -> AudioLevel {
        self.bus.audio_level()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_spec() {
        let c = JarvisConfig::default();
        assert_eq!(c.voice_id, "es-CL-LorenzoNeural");
        assert_eq!(c.voice_lang, "es-CL");
        assert!(!c.gaming_mode);
        assert!(!c.continuous_listening);
    }

    #[test]
    fn initial_state_is_idle() {
        let core = JarvisCore::new(JarvisConfig::default());
        assert_eq!(core.state(), JarvisState::Idle);
        assert!(!core.is_muted());
    }

    #[test]
    fn valid_transitions_are_accepted() {
        let mut core = JarvisCore::new(JarvisConfig::default());
        for target in [
            JarvisState::Listening,
            JarvisState::Thinking,
            JarvisState::Muted,
        ] {
            let t = core.transition(target).expect("transición válida");
            assert_eq!(t.to, target);
        }
    }

    #[test]
    fn invalid_transition_is_rejected() {
        let mut core = JarvisCore::new(JarvisConfig::default());
        assert_eq!(
            core.transition(JarvisState::Speaking),
            Err(TransitionError::InvalidTransition {
                from: JarvisState::Idle,
                to: JarvisState::Speaking,
            })
        );
        assert_eq!(core.state(), JarvisState::Idle);
    }

    #[test]
    fn self_transition_is_rejected() {
        let mut core = JarvisCore::new(JarvisConfig::default());
        assert_eq!(
            core.transition(JarvisState::Idle),
            Err(TransitionError::SelfTransition(JarvisState::Idle))
        );
    }

    #[test]
    fn audio_level_is_clamped_and_last_value_wins() {
        let core = JarvisCore::new(JarvisConfig::default());
        core.set_audio_level(1.7);
        assert_eq!(core.audio_level(), 1.0);
        core.set_audio_level(-0.4);
        assert_eq!(core.audio_level(), 0.0);
        core.set_audio_level(0.42);
        core.set_audio_level(0.77);
        assert_eq!(core.audio_level(), 0.77);
    }

    #[test]
    fn state_transition_is_published_on_bus() {
        let mut core = JarvisCore::new(JarvisConfig::default());
        let t = core.transition(JarvisState::Listening).unwrap();
        let received = core.bus().state_receiver().recv().unwrap();
        assert_eq!(received, t);
        assert_eq!(received.from, JarvisState::Idle);
        assert_eq!(received.to, JarvisState::Listening);
    }

    #[test]
    fn mute_toggle_roundtrip() {
        let mut core = JarvisCore::new(JarvisConfig::default());
        let t = core.toggle_mute().unwrap();
        assert_eq!(t.to, JarvisState::Muted);
        assert!(core.is_muted());

        let t = core.toggle_mute().unwrap();
        assert_eq!(t.to, JarvisState::Idle);
        assert!(!core.is_muted());
    }

    #[test]
    fn full_conversation_flow() {
        let mut core = JarvisCore::new(JarvisConfig::default());
        core.transition(JarvisState::Listening).unwrap();
        core.transition(JarvisState::Thinking).unwrap();
        core.transition(JarvisState::Speaking).unwrap();
        core.transition(JarvisState::Idle).unwrap();
        assert_eq!(core.state(), JarvisState::Idle);
    }
}
