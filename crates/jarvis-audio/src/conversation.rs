//! Gestor conversacional de Fokkus-Jarvis (Issues #3, #4 y HU-9).
//!
//! `ConversationCoordinator` integra la máquina de estados ([`JarvisCore`]), la
//! señal de reproducción ([`AudioPlayback`]), el motor de síntesis
//! ([`EdgeTtsEngine`]) y el cliente LLM ([`ClaudeClient`]) para dar forma al
//! diálogo de punta a punta:
//!
//! * **Fillers (< 300 ms):** al terminar el turno del usuario se entra en
//!   `Thinking` y se dispara de inmediato una frase de relleno contextual
//!   pre-sintetizada en caché, eliminando el silencio incómodo.
//! * **Barge-in (< 50 ms):** si el usuario habla mientras Jarvis está `Speaking`,
//!   se corta la reproducción al instante, se cancela la inferencia activa y se
//!   vuelve a `Listening`.
//! * **Streaming de respuesta:** los *tokens* del LLM se segmentan en frases con
//!   [`PhraseSplitter`] y cada frase se sintetiza y entrega en cuanto está
//!   completa, de modo que la voz arranca antes de que termine la generación.
//! * **Preservación del Swarm:** la interrupción de voz NUNCA cancela las tareas
//!   técnicas en segundo plano; solo el usuario puede hacerlo de forma explícita
//!   (dictamen §4.2, CA-9.4).

use std::fmt;
use std::path::PathBuf;
use std::time::Instant;

use jarvis_core::{JarvisConfig, JarvisCore, JarvisState, SharedCore, TransitionError};

use crate::brain::{BrainError, ClaudeClient, CliBrain, Completion};
use crate::playback::AudioPlayback;
use crate::tts::{
    apply_directive_pcm, DirectedPhrase, EdgeTtsEngine, FillerCategory, PcmAudio, PhraseGrouper,
    PhraseSplitter, TtsError,
};

/// Palabras clave que identifican órdenes de desarrollo (dictamen §4.1).
pub const DEV_KEYWORDS: &[&str] = &[
    "repo",
    "código",
    "codigo",
    "tarea",
    "issue",
    "bug",
    "implementa",
    "swarm",
    "desarrollo",
    "git",
    "commit",
    "test",
    "pull",
];

/// Palabras clave que identifican triage / bandeja de entrada (dictamen §4.1).
pub const INBOX_KEYWORDS: &[&str] = &[
    "bandeja",
    "inbox",
    "ticket",
    "triage",
    "notificación",
    "notificacion",
    "slack",
    "correo",
];

/// Clasifica un texto del usuario en la categoría de filler adecuada.
///
/// El orden de precedencia es *desarrollo* → *bandeja* → *general*: si el texto
/// mezcla términos de ambas categorías específicas, gana la de desarrollo.
pub fn classify_intent(text: &str) -> FillerCategory {
    let lower = text.to_lowercase();
    if contains_any(&lower, DEV_KEYWORDS) {
        FillerCategory::Dev
    } else if contains_any(&lower, INBOX_KEYWORDS) {
        FillerCategory::Inbox
    } else {
        FillerCategory::General
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(*needle))
}

/// Error del gestor conversacional.
#[derive(Debug)]
pub enum ConversationError {
    /// La transición de estado solicitada no es válida.
    Transition(TransitionError),
    /// El motor TTS falló.
    Tts(TtsError),
    /// El cliente LLM falló (red, API, cancelación inesperada…).
    Brain(BrainError),
}

impl fmt::Display for ConversationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConversationError::Transition(e) => write!(f, "error de transición: {e}"),
            ConversationError::Tts(e) => write!(f, "error de TTS: {e}"),
            ConversationError::Brain(e) => write!(f, "error del cerebro: {e}"),
        }
    }
}

impl std::error::Error for ConversationError {}

impl From<TransitionError> for ConversationError {
    fn from(value: TransitionError) -> Self {
        ConversationError::Transition(value)
    }
}

impl From<TtsError> for ConversationError {
    fn from(value: TtsError) -> Self {
        ConversationError::Tts(value)
    }
}

impl From<BrainError> for ConversationError {
    fn from(value: BrainError) -> Self {
        ConversationError::Brain(value)
    }
}

/// Registro de tareas técnicas del Swarm en segundo plano.
///
/// Es deliberadamente pasivo: el barge-in *lee* este registro pero nunca lo
/// modifica. Solo una orden explícita del usuario (`complete`) retira una tarea.
#[derive(Debug, Default, Clone)]
pub struct BackgroundTasks {
    active: Vec<String>,
}

impl BackgroundTasks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registra una tarea activa (idempotente por identificador).
    pub fn register(&mut self, task_id: impl Into<String>) {
        let id = task_id.into();
        if !self.active.contains(&id) {
            self.active.push(id);
        }
    }

    /// Marca una tarea como completada. Devuelve `true` si existía.
    pub fn complete(&mut self, task_id: &str) -> bool {
        if let Some(pos) = self.active.iter().position(|id| id == task_id) {
            self.active.remove(pos);
            true
        } else {
            false
        }
    }

    /// `true` si la tarea sigue en segundo plano.
    pub fn is_active(&self, task_id: &str) -> bool {
        self.active.iter().any(|id| id == task_id)
    }

    /// Número de tareas activas.
    pub fn len(&self) -> usize {
        self.active.len()
    }

    /// `true` si no hay tareas en segundo plano.
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Identificadores de las tareas activas, en orden de registro.
    pub fn active_ids(&self) -> &[String] {
        &self.active
    }
}

/// Fuente de tokens de streaming ([`ClaudeClient`] en producción). Permite
/// probar el coordinador y el orquestador sin red inyectando un cliente falso.
pub trait TokenStream {
    /// Envía el turno y llama `on_token` con cada fragmento de texto.
    fn stream_tokens(
        &self,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError>;

    /// Como [`stream_tokens`](Self::stream_tokens) con un bloque de contexto
    /// efímero (p. ej. `[Sub-sesiones activas]`, HU-65) antepuesto al turno.
    ///
    /// El contexto viaja solo en esta petición: las implementaciones con
    /// historial guardan únicamente `user_text`. La implementación por
    /// defecto concatena ambos textos (dobles de test sin historial).
    fn stream_tokens_with_context(
        &self,
        context: Option<&str>,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        match context {
            Some(ctx) => self.stream_tokens(&with_context(ctx, user_text), on_token),
            None => self.stream_tokens(user_text, on_token),
        }
    }

    /// Cancela la petición en curso (barge-in). Idempotente.
    fn cancel(&self);

    /// `true` si el cerebro puede usar herramientas de inspección de
    /// repositorios y código (`gh`, `git`, `rg`…) en el equipo del usuario.
    ///
    /// El modo API no expone herramientas, así que el valor por defecto es
    /// `false`; una tarea que las necesite avisa "Necesito el modo CLI para
    /// revisar repositorios" en vez de inventar el resultado (HU-60 CA-3).
    fn has_repo_tools(&self) -> bool {
        false
    }
}

/// Turno del usuario precedido por un bloque de contexto efímero (HU-65).
pub fn with_context(context: &str, user_text: &str) -> String {
    format!("{context}\n\nUsuario: {user_text}")
}

/// Síntesis de una frase a PCM en memoria, inyectable (tests y orquestador).
pub type SynthesizeFn = dyn Fn(&str) -> Result<PcmAudio, TtsError> + Send;

impl TokenStream for CliBrain {
    fn stream_tokens(
        &self,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        self.stream(user_text, on_token)
    }

    fn stream_tokens_with_context(
        &self,
        context: Option<&str>,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        self.stream_with_context(context, user_text, on_token)
    }

    fn cancel(&self) {
        CliBrain::cancel(self);
    }

    /// El CLI corre en el equipo del usuario: tiene `gh`, `git`, `rg` (HU-60).
    fn has_repo_tools(&self) -> bool {
        true
    }
}

impl TokenStream for ClaudeClient {
    fn stream_tokens(
        &self,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        self.stream(user_text, on_token)
    }

    fn stream_tokens_with_context(
        &self,
        context: Option<&str>,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        self.stream_with_context(context, user_text, on_token)
    }

    fn cancel(&self) {
        ClaudeClient::cancel(self);
    }

    /// El modo API no expone herramientas: la tarea avisa y no inventa (HU-60).
    fn has_repo_tools(&self) -> bool {
        false
    }
}

/// Barge-in sin acceso exclusivo al coordinador.
///
/// Lo usa el orquestador desde el hilo de eventos de voz mientras el hilo del
/// cerebro está dentro de [`ConversationCoordinator::run_turn_streaming_with`]:
/// basta con la señal de reproducción (compartida), el cliente (cancelación
/// atómica) y el núcleo compartido. Comprobar `Speaking` y pasar a `Listening`
/// ocurre bajo el mismo lock, así que no compite con el fin natural de la
/// locución. Las tareas del Swarm no se tocan (CA-9.4).
///
/// Devuelve `true` solo si hubo una interrupción efectiva.
pub fn barge_in(core: &SharedCore, playback: &AudioPlayback, client: &dyn TokenStream) -> bool {
    let mut core = core.lock();
    if core.state() != JarvisState::Speaking {
        // No hay locución que interrumpir: operación nula.
        return false;
    }
    // Corte acústico instantáneo: señal de parada + invalidación de generación.
    playback.stop();
    // Cancela la inferencia en curso (idempotente si ya terminó).
    client.cancel();
    // La transición es válida por tabla, pero ante cualquier fallo no debe
    // abortar la interrupción: el audio ya está detenido.
    let _ = core.transition(JarvisState::Listening);
    true
}

/// Coordinador del flujo conversacional (fillers + barge-in + streaming HU-9).
pub struct ConversationCoordinator {
    core: SharedCore,
    playback: AudioPlayback,
    tts: EdgeTtsEngine,
    background_tasks: BackgroundTasks,
    last_category: Option<FillerCategory>,
    last_filler: Option<PathBuf>,
    utterance_generation: u64,
    /// `true` si el turno en curso fue interrumpido por barge-in.
    interrupted: bool,
    /// Si es `true`, el turno termina en `Speaking` y quien reproduce el
    /// audio llama a [`finish_speech`](Self::finish_speech) cuando la cola de
    /// reproducción se vacía (el stream del LLM acaba antes que la voz).
    defer_finish: bool,
    /// Motor de síntesis alternativo (dobles de prueba).
    synth_override: Option<Box<SynthesizeFn>>,
    /// Agrupa frases con la misma directiva en un request (HU-50).
    group_phrases: bool,
}

impl ConversationCoordinator {
    /// Crea el coordinador en estado `Idle` con el motor TTS dado.
    pub fn new(tts: EdgeTtsEngine) -> Self {
        Self::with_config(tts, JarvisConfig::default())
    }

    /// Crea el coordinador con una configuración de núcleo explícita.
    pub fn with_config(tts: EdgeTtsEngine, config: JarvisConfig) -> Self {
        Self::with_shared_core(
            tts,
            SharedCore::new(JarvisCore::new(config)),
            AudioPlayback::new(),
        )
    }

    /// Crea el coordinador sobre el núcleo compartido con la esfera y el IPC
    /// y sobre la señal de reproducción del motor de audio real.
    pub fn with_shared_core(tts: EdgeTtsEngine, core: SharedCore, playback: AudioPlayback) -> Self {
        Self {
            core,
            playback,
            tts,
            background_tasks: BackgroundTasks::new(),
            last_category: None,
            last_filler: None,
            utterance_generation: 0,
            interrupted: false,
            defer_finish: false,
            synth_override: None,
            group_phrases: true,
        }
    }

    /// Sustituye la síntesis de frases (p. ej. por un doble de prueba).
    pub fn with_synthesizer(
        mut self,
        synth: impl Fn(&str) -> Result<PcmAudio, TtsError> + Send + 'static,
    ) -> Self {
        self.synth_override = Some(Box::new(synth));
        self
    }

    /// Ver el campo `defer_finish`: con reproducción asíncrona real, el turno
    /// se cierra cuando el audio termina de sonar, no cuando acaba el stream.
    pub fn set_deferred_finish(&mut self, deferred: bool) {
        self.defer_finish = deferred;
    }

    /// Activa o desactiva la agrupación de frases (HU-50). Activa por
    /// defecto; desactivada, cada frase va en su propio request.
    pub fn set_phrase_grouping(&mut self, enabled: bool) {
        self.group_phrases = enabled;
    }

    /// Estado actual de la máquina de estados.
    pub fn state(&self) -> JarvisState {
        self.core.state()
    }

    /// Núcleo compartido (máquina de estados + bus de eventos).
    pub fn core(&self) -> &SharedCore {
        &self.core
    }

    /// Referencia a la señal de reproducción (comparte estado con las copias).
    pub fn playback(&self) -> &AudioPlayback {
        &self.playback
    }

    /// Referencia al motor TTS.
    pub fn tts(&self) -> &EdgeTtsEngine {
        &self.tts
    }

    /// Cambia la voz activa en caliente (misma caché y binarios TTS).
    ///
    /// La caché de fillers es por voz, así que los de la voz nueva se
    /// pre-sintetizan en segundo plano desde el orquestador.
    pub fn set_voice(&mut self, voice_id: &str) {
        self.tts = self.tts.for_voice(voice_id);
    }

    /// Categoría del último turno clasificado.
    pub fn last_category(&self) -> Option<FillerCategory> {
        self.last_category
    }

    /// Ruta del último filler seleccionado (si hubo alguno en caché).
    pub fn last_filler(&self) -> Option<&PathBuf> {
        self.last_filler.as_ref()
    }

    /// `true` si el asistente está emitiendo audio.
    pub fn is_speaking(&self) -> bool {
        self.core.state().is_speaking()
    }

    /// Registro de tareas del Swarm en segundo plano.
    pub fn background_tasks(&self) -> &BackgroundTasks {
        &self.background_tasks
    }

    /// Registra una tarea técnica en segundo plano (no se toca en barge-in).
    pub fn register_background_task(&mut self, task_id: impl Into<String>) {
        self.background_tasks.register(task_id);
    }

    /// Retira explícitamente una tarea de segundo plano (solo por orden del usuario).
    pub fn complete_background_task(&mut self, task_id: &str) -> bool {
        self.background_tasks.complete(task_id)
    }

    /// Pasa a `Listening` (arranque de captura tras wake-word / atajo).
    pub fn start_listening(&mut self) -> Result<(), ConversationError> {
        let mut core = self.core.lock();
        match core.state() {
            JarvisState::Listening => Ok(()),
            JarvisState::Idle => {
                core.transition(JarvisState::Listening)?;
                Ok(())
            }
            other => Err(TransitionError::InvalidTransition {
                from: other,
                to: JarvisState::Listening,
            }
            .into()),
        }
    }

    /// Procesa la elocución completa del usuario.
    ///
    /// 1. Transiciona `Listening → Thinking` (arranca en `Listening` si hacía
    ///    falta desde `Idle`).
    /// 2. Clasifica la intención para elegir la categoría de filler.
    /// 3. Obtiene de forma inmediata (< 300 ms) el filler pre-sintetizado en
    ///    caché, sin red ni síntesis on-line.
    /// 4. Declara el inicio de la locución en [`AudioPlayback`].
    ///
    /// Devuelve la ruta del filler cuando existe en caché.
    pub fn handle_user_utterance(
        &mut self,
        text: &str,
    ) -> Result<Option<PathBuf>, ConversationError> {
        // Nuevo turno: se limpia cualquier marca de interrupción anterior.
        self.interrupted = false;

        // Se garantiza el paso por Listening antes de razonar; ambas
        // transiciones bajo el mismo lock (otro hilo puede mutear entre medio).
        {
            let mut core = self.core.lock();
            if core.state() == JarvisState::Idle {
                core.transition(JarvisState::Listening)?;
            }
            // Listening -> Thinking.
            core.transition(JarvisState::Thinking)?;
        }

        let category = classify_intent(text);
        self.last_category = Some(category);

        // Selección inmediata desde la caché local (solo I/O de archivos).
        let filler = self.tts.get_cached_filler_for(category);

        // Declara la locución: limpia paradas previas y abre nueva generación.
        self.utterance_generation = self.playback.begin_utterance();

        self.last_filler = filler.clone();
        Ok(filler)
    }

    /// Comienza la locución de la respuesta real (`Thinking → Speaking`).
    pub fn begin_speaking(&mut self) -> Result<(), ConversationError> {
        self.core.transition(JarvisState::Speaking)?;
        self.utterance_generation = self.playback.begin_utterance();
        Ok(())
    }

    /// Interrupción humana en tiempo real (*barge-in*).
    ///
    /// Si el estado actual es `Speaking`, corta la reproducción de inmediato
    /// (marca de parada + nuevo `generation`), cancela la inferencia activa del
    /// cliente y vuelve a `Listening` en el mismo hilo lógico, en microsegundos
    /// (< 50 ms).
    ///
    /// Las tareas del Swarm en segundo plano NO se tocan ni se cancelan
    /// (CA-9.4).
    ///
    /// Devuelve `true` solo si hubo una interrupción efectiva.
    pub fn handle_barge_in(&mut self, client: &ClaudeClient) -> bool {
        let interrupted = barge_in(&self.core, &self.playback, client);
        if interrupted {
            self.interrupted = true;
        }
        interrupted
    }

    /// Cierra el turno de locución.
    ///
    /// * Si `completed_naturally` es `true` y no hubo barge-in previo, pasa de
    ///   `Speaking` a `Idle`.
    /// * Si hubo barge-in, permanece en `Listening` (la captura del nuevo
    ///   comando ya está en curso).
    pub fn finish_speech(&mut self, completed_naturally: bool) -> Result<(), ConversationError> {
        let was_interrupted = self.interrupted;
        if completed_naturally && !was_interrupted {
            if let Some(result) = self
                .core
                .transition_from(JarvisState::Speaking, JarvisState::Idle)
            {
                result?;
            }
        }
        self.interrupted = false;
        Ok(())
    }

    /// Generación de la locución activa (para descartar audio obsoleto).
    pub fn utterance_generation(&self) -> u64 {
        self.utterance_generation
    }

    /// `true` si la generación dada quedó obsoleta por un barge-in posterior.
    pub fn is_stale(&self, generation: u64) -> bool {
        self.playback.is_stale(generation)
    }

    /// Consume el turno completo del LLM y emite el audio de cada frase.
    ///
    /// Conecta el *streaming* de tokens de `client` con un [`PhraseSplitter`]:
    /// cada frase completa se sintetiza con
    /// [`EdgeTtsEngine::synthesize_phrase_native`] y se entrega a
    /// `on_phrase_audio`. Si el usuario interrumpe, se detiene el stream y se
    /// descarta el audio restante.
    pub fn run_turn_streaming<F>(
        &mut self,
        user_text: &str,
        client: &ClaudeClient,
        mut on_phrase_audio: F,
    ) -> Result<Completion, ConversationError>
    where
        F: FnMut(&str, &PcmAudio),
    {
        self.run_turn_streaming_with(client, user_text, &mut on_phrase_audio)
    }

    /// Igual que [`run_turn_streaming`](Self::run_turn_streaming) con
    /// cualquier [`TokenStream`] (el orquestador y los tests inyectan dobles).
    pub fn run_turn_streaming_with<C, F>(
        &mut self,
        client: &C,
        user_text: &str,
        on_phrase_audio: &mut F,
    ) -> Result<Completion, ConversationError>
    where
        C: TokenStream + ?Sized,
        F: FnMut(&str, &PcmAudio),
    {
        self.run_turn_streaming_with_context(client, None, user_text, on_phrase_audio)
    }

    /// Igual que [`run_turn_streaming_with`](Self::run_turn_streaming_with)
    /// con un bloque de contexto efímero para esta petición (HU-65): el
    /// roster de sub-sesiones llega al cerebro, pero no al historial.
    pub fn run_turn_streaming_with_context<C, F>(
        &mut self,
        client: &C,
        context: Option<&str>,
        user_text: &str,
        on_phrase_audio: &mut F,
    ) -> Result<Completion, ConversationError>
    where
        C: TokenStream + ?Sized,
        F: FnMut(&str, &PcmAudio),
    {
        let generation = self.utterance_generation;
        let mut splitter = PhraseSplitter::new();
        // HU-50: la primera frase sale sola; las siguientes con la misma voz
        // van en un request para que Edge entone el párrafo como unidad.
        let mut grouper = if self.group_phrases {
            PhraseGrouper::new()
        } else {
            PhraseGrouper::disabled()
        };
        let mut saw_phrase = false;
        let mut aborted = false;

        let stream_result =
            client.stream_tokens_with_context(context, user_text, &mut |token: &str| {
                // Barge-in en curso: detiene el stream y no sintetiza más.
                if self.interrupted || self.playback.is_stop_requested() {
                    client.cancel();
                    aborted = true;
                    let _ = self
                        .core
                        .transition_from(JarvisState::Speaking, JarvisState::Listening);
                    return;
                }

                splitter.push(token);
                let now = Instant::now();
                let mut ready = Vec::new();
                for phrase in splitter.drain_directed() {
                    ready.extend(grouper.push(phrase, now));
                }
                ready.extend(grouper.poll(now));
                for phrase in ready {
                    if self.interrupted || self.playback.is_stop_requested() {
                        client.cancel();
                        aborted = true;
                        let _ = self
                            .core
                            .transition_from(JarvisState::Speaking, JarvisState::Listening);
                        return;
                    }

                    // Primera frase de respuesta real: `Thinking → Speaking`.
                    if !saw_phrase {
                        match self
                            .core
                            .transition_from(JarvisState::Thinking, JarvisState::Speaking)
                        {
                            Some(Ok(_)) => saw_phrase = true,
                            // Otro hilo cambió el estado (barge-in, silencio):
                            // la respuesta ya no debe sonar.
                            Some(Err(_)) | None => {
                                client.cancel();
                                aborted = true;
                                return;
                            }
                        }
                    }

                    if let Ok(pcm) = self.synthesize_directed(&phrase) {
                        // Locución obsoleta por barge-in: descarta y aborta.
                        if self.is_stale(generation) {
                            client.cancel();
                            aborted = true;
                            let _ = self
                                .core
                                .transition_from(JarvisState::Speaking, JarvisState::Listening);
                            return;
                        }
                        on_phrase_audio(&phrase.text, &pcm);
                    }
                }
            });

        // Remanente pendiente del segmentador al terminar el stream.
        if !aborted {
            let now = Instant::now();
            let mut ready = Vec::new();
            for phrase in splitter.finish_directed() {
                ready.extend(grouper.push(phrase, now));
            }
            ready.extend(grouper.flush());
            for remaining in ready {
                if self.interrupted || self.playback.is_stop_requested() {
                    break;
                }
                if !saw_phrase {
                    match self
                        .core
                        .transition_from(JarvisState::Thinking, JarvisState::Speaking)
                    {
                        Some(result) => {
                            result?;
                            saw_phrase = true;
                        }
                        None => {
                            aborted = true;
                            break;
                        }
                    }
                }
                if let Ok(pcm) = self.synthesize_directed(&remaining) {
                    if !self.is_stale(generation) {
                        on_phrase_audio(&remaining.text, &pcm);
                    }
                }
            }
        }

        match stream_result {
            Ok(completion) => {
                if !aborted
                    && !self.defer_finish
                    && !self.interrupted
                    && !self.playback.is_stop_requested()
                {
                    self.finish_speech(true)?;
                }
                Ok(completion)
            }
            // Cualquier cancelación del cerebro es limpia (barge-in, hablar
            // antes del primer token…): jamás debe escalarse como fallo de red
            // (HU-32).
            Err(BrainError::Cancelled) => Ok(Completion::default()),
            Err(e) => Err(ConversationError::Brain(e)),
        }
    }

    /// Sintetiza una frase con el motor configurado (o el doble inyectado).
    pub fn synthesize_phrase(&self, text: &str) -> Result<PcmAudio, TtsError> {
        self.synthesize_directed(&DirectedPhrase::plain(text))
    }

    /// Sintetiza una frase con su prosodia (HU-46). El doble inyectado solo
    /// recibe el texto; las pausas y la respiración se aplican igual.
    pub fn synthesize_directed(&self, phrase: &DirectedPhrase) -> Result<PcmAudio, TtsError> {
        if let Some(synth) = &self.synth_override {
            return synth(&phrase.text).map(|pcm| apply_directive_pcm(pcm, &phrase.directive));
        }
        self.tts.synthesize_directed(phrase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::Path;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("jarvis-conv-{name}-{}", std::process::id()))
    }

    /// Motor TTS con el primer filler de la categoría ya presente en caché.
    fn engine_with_cached(dir: &Path, category: FillerCategory) -> EdgeTtsEngine {
        let engine = EdgeTtsEngine::new()
            .with_cache_dir(dir.to_path_buf())
            .with_edge_tts_binary(Some(PathBuf::from("/bin/edge-tts")));
        let first = engine.filler_cache_path(category.phrases()[0]);
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::write(&first, b"audio").unwrap();
        engine
    }

    fn empty_engine(dir: &Path) -> EdgeTtsEngine {
        EdgeTtsEngine::new()
            .with_cache_dir(dir.to_path_buf())
            .with_edge_tts_binary(Some(PathBuf::from("/bin/edge-tts")))
    }

    fn fake_client() -> ClaudeClient {
        ClaudeClient::with_api_key("test-key")
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Cliente LLM falso que emite tokens en memoria, sin red.
    struct FakeClient {
        tokens: Vec<String>,
        cancelled: Cell<bool>,
        /// Si es `Some(n)`, detiene el `playback` justo antes del token `n`.
        interrupt_after: Option<usize>,
        playback: Option<AudioPlayback>,
    }

    impl TokenStream for FakeClient {
        fn stream_tokens(
            &self,
            _user_text: &str,
            on_token: &mut dyn FnMut(&str),
        ) -> Result<Completion, BrainError> {
            let mut count = 0usize;
            for token in &self.tokens {
                if self.cancelled.get() {
                    return Err(BrainError::Cancelled);
                }
                count += 1;
                if self.interrupt_after == Some(count) {
                    if let Some(playback) = &self.playback {
                        playback.stop();
                    }
                }
                on_token(token);
            }
            if self.cancelled.get() {
                return Err(BrainError::Cancelled);
            }
            Ok(Completion {
                text: self.tokens.concat(),
                stop_reason: Some("end_turn".to_string()),
                input_tokens: 1,
                output_tokens: 1,
                cache_read_input_tokens: 0,
            })
        }

        fn cancel(&self) {
            self.cancelled.set(true);
        }
    }

    #[test]
    fn classify_intent_detects_dev_keywords() {
        for text in [
            "revisa el repositorio del proyecto",
            "hay un bug en el código",
            "implementa esta tarea",
            "abre el issue 42",
            "lanza el swarm de desarrollo",
            "haz un commit y sube el pull request",
            "corre los test del crate",
        ] {
            assert_eq!(classify_intent(text), FillerCategory::Dev, "texto: {text}");
        }
        // Acento y ausencia de acento son equivalentes.
        assert_eq!(classify_intent("arregla el codigo"), FillerCategory::Dev);
        assert_eq!(classify_intent("revisa el código"), FillerCategory::Dev);
    }

    #[test]
    fn classify_intent_detects_inbox_keywords() {
        for text in [
            "revisa la bandeja de entrada",
            "mira mi inbox",
            "abre el ticket nuevo",
            "haz el triage de mensajes",
            "mándame una notificación",
            "envíalo por slack",
            "lee el correo de esta mañana",
        ] {
            assert_eq!(
                classify_intent(text),
                FillerCategory::Inbox,
                "texto: {text}"
            );
        }
        // Variante sin acento.
        assert_eq!(
            classify_intent("genera una notificacion"),
            FillerCategory::Inbox
        );
    }

    #[test]
    fn classify_intent_falls_back_to_general() {
        for text in [
            "hola, ¿qué hora es?",
            "cuéntame un chiste",
            "pon música",
            "",
            "sube el volumen",
        ] {
            assert_eq!(
                classify_intent(text),
                FillerCategory::General,
                "texto: {text}"
            );
        }
    }

    #[test]
    fn classify_intent_is_case_insensitive() {
        assert_eq!(classify_intent("REVISA EL REPO"), FillerCategory::Dev);
        assert_eq!(classify_intent("El Ticket Nuevo"), FillerCategory::Inbox);
    }

    #[test]
    fn dev_takes_precedence_over_inbox() {
        let text = "abre el ticket y revisa el código";
        assert_eq!(classify_intent(text), FillerCategory::Dev);
    }

    #[test]
    fn handle_user_utterance_transitions_listening_to_thinking() {
        let dir = temp_dir("utterance");
        cleanup(&dir);
        let engine = engine_with_cached(&dir, FillerCategory::Dev);
        let mut coord = ConversationCoordinator::new(engine);

        coord.start_listening().unwrap();
        assert_eq!(coord.state(), JarvisState::Listening);

        let filler = coord
            .handle_user_utterance("implementa la nueva feature del repo")
            .expect("turno válido");

        assert_eq!(coord.state(), JarvisState::Thinking);
        assert_eq!(coord.last_category(), Some(FillerCategory::Dev));
        assert!(filler.is_some(), "debe seleccionar el filler cacheado");
        assert_eq!(coord.last_filler(), filler.as_ref());
        // El filler proviene de la caché de la categoría dev.
        assert!(filler.unwrap().is_file());

        cleanup(&dir);
    }

    #[test]
    fn handle_user_utterance_autostarts_from_idle() {
        let dir = temp_dir("idle-start");
        cleanup(&dir);
        let engine = engine_with_cached(&dir, FillerCategory::Inbox);
        let mut coord = ConversationCoordinator::new(engine);

        assert_eq!(coord.state(), JarvisState::Idle);
        let filler = coord
            .handle_user_utterance("revisa la bandeja")
            .expect("turno válido desde Idle");

        assert_eq!(coord.state(), JarvisState::Thinking);
        assert_eq!(coord.last_category(), Some(FillerCategory::Inbox));
        assert!(filler.is_some());

        cleanup(&dir);
    }

    #[test]
    fn handle_user_utterance_without_cached_filler_returns_none() {
        let dir = temp_dir("no-filler");
        cleanup(&dir);
        let engine = empty_engine(&dir);
        let mut coord = ConversationCoordinator::new(engine);

        coord.start_listening().unwrap();
        let filler = coord
            .handle_user_utterance("hola, cuéntame algo")
            .expect("turno válido");

        assert_eq!(coord.state(), JarvisState::Thinking);
        assert_eq!(coord.last_category(), Some(FillerCategory::General));
        assert_eq!(filler, None);

        cleanup(&dir);
    }

    #[test]
    fn set_voice_swaps_engine_without_losing_backend() {
        let dir = temp_dir("voice-swap");
        cleanup(&dir);
        let engine = empty_engine(&dir);
        let mut coord = ConversationCoordinator::new(engine);

        assert_eq!(coord.tts().voice_id(), "es-CL-LorenzoNeural");
        coord.set_voice("es-ES-ElviraNeural");
        assert_eq!(coord.tts().voice_id(), "es-ES-ElviraNeural");
        assert_eq!(coord.tts().voice_lang(), "es-ES");
        // El backend y la caché se conservan (solo cambia la voz).
        assert_eq!(coord.tts().cache_dir(), &dir);

        cleanup(&dir);
    }

    #[test]
    fn barge_in_during_speaking_stops_playback_and_listens() {
        let dir = temp_dir("bargein-speaking");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        let client = fake_client();

        coord.start_listening().unwrap();
        coord.handle_user_utterance("hola").unwrap();
        coord.begin_speaking().unwrap();
        assert_eq!(coord.state(), JarvisState::Speaking);

        let generation_before = coord.playback().generation();
        assert!(!coord.playback().is_stop_requested());

        assert!(coord.handle_barge_in(&client), "debe interrumpir");

        assert_eq!(coord.state(), JarvisState::Listening);
        assert!(coord.playback().is_stop_requested(), "playback detenido");
        assert_eq!(
            coord.playback().generation(),
            generation_before + 1,
            "la generación de parada se incrementa"
        );
        assert!(client.is_cancelled(), "la inferencia se cancela");
        // La generación de la locución interrumpida queda obsoleta.
        let interrupted_gen = coord.utterance_generation();
        assert!(coord.is_stale(interrupted_gen));

        cleanup(&dir);
    }

    #[test]
    fn barge_in_is_noop_when_not_speaking() {
        let dir = temp_dir("bargein-noop");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        let client = fake_client();

        // En Idle.
        assert_eq!(coord.state(), JarvisState::Idle);
        assert!(!coord.handle_barge_in(&client));
        assert_eq!(coord.state(), JarvisState::Idle);
        assert!(!client.is_cancelled());

        // En Thinking (filler en curso, respuesta aún no locutada).
        coord.start_listening().unwrap();
        coord.handle_user_utterance("hola").unwrap();
        assert_eq!(coord.state(), JarvisState::Thinking);
        assert!(!coord.handle_barge_in(&client));
        assert_eq!(coord.state(), JarvisState::Thinking);
        assert!(!client.is_cancelled());

        cleanup(&dir);
    }

    #[test]
    fn finish_speech_completed_transitions_to_idle() {
        let dir = temp_dir("finish-idle");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));

        coord.start_listening().unwrap();
        coord.handle_user_utterance("hola").unwrap();
        coord.begin_speaking().unwrap();
        assert_eq!(coord.state(), JarvisState::Speaking);

        coord.finish_speech(true).unwrap();
        assert_eq!(coord.state(), JarvisState::Idle);

        cleanup(&dir);
    }

    #[test]
    fn finish_speech_after_barge_in_stays_listening() {
        let dir = temp_dir("finish-listening");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        let client = fake_client();

        coord.start_listening().unwrap();
        coord.handle_user_utterance("hola").unwrap();
        coord.begin_speaking().unwrap();
        assert!(coord.handle_barge_in(&client));
        assert_eq!(coord.state(), JarvisState::Listening);

        // Interrumpido: aunque se informe "natural", permanece en Listening.
        coord.finish_speech(false).unwrap();
        assert_eq!(coord.state(), JarvisState::Listening);

        cleanup(&dir);
    }

    #[test]
    fn barge_in_preserves_background_swarm_tasks() {
        let dir = temp_dir("background");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        let client = fake_client();

        coord.register_background_task("task-102");
        coord.register_background_task("task-103");
        assert_eq!(coord.background_tasks().len(), 2);

        coord.start_listening().unwrap();
        coord.handle_user_utterance("desarrollo algo").unwrap();
        coord.begin_speaking().unwrap();

        assert!(coord.handle_barge_in(&client));

        // La interrupción de voz NO toca las tareas de segundo plano.
        assert!(coord.background_tasks().is_active("task-102"));
        assert!(coord.background_tasks().is_active("task-103"));
        assert_eq!(coord.background_tasks().len(), 2);
        let expected: Vec<String> = vec!["task-102".to_string(), "task-103".to_string()];
        assert_eq!(coord.background_tasks().active_ids(), expected.as_slice());

        // Solo una orden explícita las retira.
        assert!(coord.complete_background_task("task-102"));
        assert!(!coord.background_tasks().is_active("task-102"));
        assert_eq!(coord.background_tasks().len(), 1);

        cleanup(&dir);
    }

    #[test]
    fn background_tasks_register_is_idempotent() {
        let mut tasks = BackgroundTasks::new();
        tasks.register("t1");
        tasks.register("t1");
        assert_eq!(tasks.len(), 1);
        assert!(!tasks.is_empty());
        assert!(!tasks.complete("inexistente"));
        assert!(tasks.complete("t1"));
        assert!(tasks.is_empty());

        // Clonar el registro no afecta al original.
        let clone = tasks.clone();
        assert_eq!(clone.len(), tasks.len());
    }

    #[test]
    fn start_listening_rejects_from_speaking() {
        let dir = temp_dir("start-invalid");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));

        coord.start_listening().unwrap();
        coord.handle_user_utterance("hola").unwrap();
        coord.begin_speaking().unwrap();

        assert!(matches!(
            coord.start_listening(),
            Err(ConversationError::Transition(_))
        ));
        assert_eq!(coord.state(), JarvisState::Speaking);

        cleanup(&dir);
    }

    #[test]
    fn barge_in_cancels_client_and_stops_playback_instantly() {
        let dir = temp_dir("bargein-cancel");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        let client = fake_client();

        coord.start_listening().unwrap();
        coord.handle_user_utterance("hola").unwrap();
        coord.begin_speaking().unwrap();

        let generation_before = coord.playback().generation();

        assert!(coord.handle_barge_in(&client));

        assert!(coord.playback().is_stop_requested());
        assert_eq!(coord.playback().generation(), generation_before + 1);
        assert!(client.is_cancelled());
        assert_eq!(coord.state(), JarvisState::Listening);

        cleanup(&dir);
    }

    #[test]
    fn turn_streaming_transitions_thinking_to_speaking_and_emits_phrases() {
        let dir = temp_dir("streaming-emits");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        coord.start_listening().unwrap();
        coord.handle_user_utterance("hola").unwrap();
        assert_eq!(coord.state(), JarvisState::Thinking);

        coord.synth_override = Some(Box::new(|_text| {
            Ok(PcmAudio::new(vec![0.0, 0.5], 24_000, 1))
        }));

        let client = FakeClient {
            tokens: vec!["Hola".to_string(), " mundo".to_string(), ".".to_string()],
            cancelled: Cell::new(false),
            interrupt_after: None,
            playback: None,
        };

        let mut emitted = Vec::new();
        let completion = coord
            .run_turn_streaming_with(&client, "hola", &mut |phrase, pcm| {
                emitted.push((phrase.to_string(), pcm.clone()));
            })
            .expect("turno de streaming");

        assert_eq!(coord.state(), JarvisState::Idle);
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].0, "Hola mundo.");
        assert_eq!(emitted[0].1.sample_rate, 24_000);
        assert_eq!(completion.text, "Hola mundo.");

        cleanup(&dir);
    }

    /// Cliente que anota cada prompt recibido (implementación por defecto de
    /// `stream_tokens_with_context`).
    #[derive(Default)]
    struct RecordingClient {
        prompts: std::cell::RefCell<Vec<String>>,
    }

    impl TokenStream for RecordingClient {
        fn stream_tokens(
            &self,
            user_text: &str,
            on_token: &mut dyn FnMut(&str),
        ) -> Result<Completion, BrainError> {
            self.prompts.borrow_mut().push(user_text.to_string());
            on_token("Va bien.");
            Ok(Completion {
                text: "Va bien.".to_string(),
                ..Completion::default()
            })
        }

        fn cancel(&self) {}
    }

    #[test]
    fn turn_streaming_with_context_prefixes_the_prompt_only_when_given_hu65() {
        let dir = temp_dir("streaming-context");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        coord.synth_override = Some(Box::new(|_text| {
            Ok(PcmAudio::new(vec![0.0, 0.5], 24_000, 1))
        }));
        let client = RecordingClient::default();
        let roster = "[Sub-sesiones activas]\n#1 los logs de Melón — trabajando (2 min)";
        for context in [Some(roster), None] {
            coord.start_listening().unwrap();
            coord.handle_user_utterance("¿cómo va?").unwrap();
            coord
                .run_turn_streaming_with_context(&client, context, "¿cómo va?", &mut |_, _| {})
                .expect("turno con contexto");
        }
        coord.start_listening().unwrap();
        coord.handle_user_utterance("¿y ahora?").unwrap();
        coord
            .run_turn_streaming_with(&client, "¿y ahora?", &mut |_, _| {})
            .expect("turno sin contexto");
        assert_eq!(
            *client.prompts.borrow(),
            vec![
                format!("{roster}\n\nUsuario: ¿cómo va?"),
                "¿cómo va?".to_string(),
                "¿y ahora?".to_string(),
            ]
        );
        cleanup(&dir);
    }

    /// Corre un turno con `tokens` y devuelve el texto de cada request TTS.
    fn streamed_requests(name: &str, tokens: &[&str], grouping: bool) -> Vec<String> {
        let dir = temp_dir(name);
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        coord.set_phrase_grouping(grouping);
        coord.start_listening().unwrap();
        coord.handle_user_utterance("hola").unwrap();
        coord.synth_override = Some(Box::new(|_text| {
            Ok(PcmAudio::new(vec![0.0, 0.5], 24_000, 1))
        }));
        let client = FakeClient {
            tokens: tokens.iter().map(|t| t.to_string()).collect(),
            cancelled: Cell::new(false),
            interrupt_after: None,
            playback: None,
        };
        let mut emitted = Vec::new();
        coord
            .run_turn_streaming_with(&client, "hola", &mut |phrase, _pcm| {
                emitted.push(phrase.to_string());
            })
            .expect("turno de streaming");
        assert_eq!(coord.state(), JarvisState::Idle);
        cleanup(&dir);
        emitted
    }

    #[test]
    fn turn_streaming_groups_phrases_after_the_first_one() {
        let tokens = [
            "[ánimo:alegre]Listo",
            ".",
            " Revisé",
            " el build",
            ".",
            " Todo",
            " pasa",
            ".",
            " [pausa:300]Te",
            " aviso",
            ".",
        ];
        assert_eq!(
            streamed_requests("streaming-grouped", &tokens, true),
            ["Listo.", "Revisé el build. Todo pasa.", "Te aviso."]
        );
        assert_eq!(
            streamed_requests("streaming-ungrouped", &tokens, false),
            ["Listo.", "Revisé el build.", "Todo pasa.", "Te aviso."]
        );
    }

    #[test]
    fn barge_in_during_streaming_stops_phrases_and_preserves_background_tasks() {
        let dir = temp_dir("bargein-streaming");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        coord.register_background_task("task-201");
        coord.register_background_task("task-202");

        coord.start_listening().unwrap();
        coord.handle_user_utterance("desarrollo algo").unwrap();
        assert_eq!(coord.state(), JarvisState::Thinking);

        coord.synth_override = Some(Box::new(|_text| {
            Ok(PcmAudio::new(vec![0.0, 0.5, -0.5], 16_000, 1))
        }));

        let playback = coord.playback().clone();
        let client = FakeClient {
            tokens: vec![
                "Hola".to_string(),
                " mundo".to_string(),
                ".".to_string(),
                " ¿Sigues".to_string(),
                "?".to_string(),
            ],
            cancelled: Cell::new(false),
            // Barge-in justo antes del cuarto token: la primera frase ya se
            // emitió, la segunda debe descartarse.
            interrupt_after: Some(4),
            playback: Some(playback),
        };

        let mut emitted = Vec::new();
        let completion = coord
            .run_turn_streaming_with(&client, "desarrollo algo", &mut |phrase, pcm| {
                emitted.push((phrase.to_string(), pcm.clone()));
            })
            .expect("turno interrumpido por barge-in no es error");

        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].0, "Hola mundo.");
        // El barge-in vuelve a Listening.
        assert_eq!(coord.state(), JarvisState::Listening);
        // Las tareas de fondo siguen intactas.
        assert!(coord.background_tasks().is_active("task-201"));
        assert!(coord.background_tasks().is_active("task-202"));
        assert_eq!(coord.background_tasks().len(), 2);
        assert_eq!(completion.text, String::new());

        cleanup(&dir);
    }

    #[test]
    fn multi_turn_stress_test_20_turns_without_inconsistent_state() {
        let dir = temp_dir("stress-20");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        coord.synth_override = Some(Box::new(|text| {
            Ok(PcmAudio::new(vec![0.0; text.len()], 24_000, 1))
        }));

        for i in 0..20 {
            coord.start_listening().unwrap();
            coord
                .handle_user_utterance(&format!("turno {i}"))
                .expect("turno de usuario");
            assert_eq!(coord.state(), JarvisState::Thinking, "turno {i}");

            let client = FakeClient {
                tokens: vec![format!("Respuesta {i}.")],
                cancelled: Cell::new(false),
                interrupt_after: None,
                playback: None,
            };

            let mut emitted = 0usize;
            coord
                .run_turn_streaming_with(&client, &format!("turno {i}"), &mut |_phrase, _pcm| {
                    emitted += 1;
                })
                .expect("turno completo");
            assert_eq!(emitted, 1, "turno {i}");
            assert_eq!(coord.state(), JarvisState::Idle, "turno {i}");
            assert!(!coord.playback().is_stop_requested(), "turno {i}");
        }

        cleanup(&dir);
    }

    /// HU-45/46: las directivas del LLM no llegan al HUD ni al TTS, y su
    /// prosodia se aplica al audio de cada frase.
    #[test]
    fn streaming_turn_strips_directives_and_renders_pauses() {
        let dir = temp_dir("streaming-directives");
        cleanup(&dir);
        let mut coord = ConversationCoordinator::new(empty_engine(&dir));
        coord.start_listening().unwrap();
        coord.handle_user_utterance("hola").unwrap();

        let synthesized = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = std::sync::Arc::clone(&synthesized);
        coord.synth_override = Some(Box::new(move |text| {
            seen.lock().unwrap().push(text.to_string());
            Ok(PcmAudio::new(vec![0.5; 2_400], 24_000, 1))
        }));

        let client = FakeClient {
            tokens: vec![
                "[ánimo:entu".to_string(),
                "siasta]¡Listo!".to_string(),
                " [pau".to_string(),
                "sa:300][respira]Te".to_string(),
                " aviso.".to_string(),
            ],
            cancelled: Cell::new(false),
            interrupt_after: None,
            playback: None,
        };

        let mut emitted = Vec::new();
        coord
            .run_turn_streaming_with(&client, "hola", &mut |phrase, pcm| {
                emitted.push((phrase.to_string(), pcm.samples.len()));
            })
            .expect("turno con directivas");

        assert_eq!(
            *synthesized.lock().unwrap(),
            vec!["¡Listo!".to_string(), "Te aviso.".to_string()]
        );
        let captions: Vec<&str> = emitted.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(captions, vec!["¡Listo!", "Te aviso."]);
        // Primera frase sin silencio; la segunda con 300 ms de pausa,
        // inhalación (320 ms) y su hueco (70 ms) antes de la voz.
        assert_eq!(emitted[0].1, 2_400);
        assert_eq!(emitted[1].1, 2_400 + 7_200 + 7_680 + 1_680);
        cleanup(&dir);
    }
}
