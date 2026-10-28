//! Pruebas de HU-62: `SessionManager` y sub-sesiones persistentes.
//!
//! Se usan cerebros falsos (nunca el CLI ni la red) para poder comprobar
//! concurrencia, multiturno, cancelación, timeout y rehidratación de forma
//! determinista.

use super::*;
use crate::brain::Completion;
use crate::speech_queue::DeliveryGate;

/// Respuesta del cerebro falso cuando no hay guion.
const DEFAULT_REPLY: &str = "RESUMEN_HABLADO: Todo en orden.\nDETALLE:\n- sin novedades";

/// Respuesta con la que el cerebro falso simula el timeout del CLI.
const TIMEOUT_SENTINEL: &str = "TIMEOUT";

/// Contadores compartidos entre la fábrica y los cerebros que crea.
#[derive(Clone, Default)]
struct Probe {
    prompts: Arc<Mutex<Vec<String>>>,
    profiles: Arc<Mutex<Vec<SessionProfile>>>,
    in_flight: Arc<AtomicU64>,
    max_in_flight: Arc<AtomicU64>,
}

impl Probe {
    fn prompts(&self) -> Vec<String> {
        lock(&self.prompts).clone()
    }

    fn profiles(&self) -> Vec<SessionProfile> {
        lock(&self.profiles).clone()
    }

    fn max_in_flight(&self) -> u64 {
        self.max_in_flight.load(Ordering::SeqCst)
    }
}

/// Cerebro falso: responde el guion tras `delay` y se deja cancelar.
struct FakeBrain {
    reply: String,
    delay: Duration,
    tools: bool,
    cancelled: AtomicBool,
    probe: Probe,
}

impl TokenStream for FakeBrain {
    fn stream_tokens(
        &self,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        lock(&self.probe.prompts).push(user_text.to_string());
        let in_flight = self.probe.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.probe
            .max_in_flight
            .fetch_max(in_flight, Ordering::SeqCst);

        let deadline = Instant::now() + self.delay;
        while Instant::now() < deadline {
            if self.cancelled.load(Ordering::SeqCst) {
                self.probe.in_flight.fetch_sub(1, Ordering::SeqCst);
                return Err(BrainError::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.probe.in_flight.fetch_sub(1, Ordering::SeqCst);

        if self.reply == TIMEOUT_SENTINEL {
            return Err(BrainError::Cli("el CLI no respondió a tiempo".into()));
        }
        // Se emite en trozos para que el escáner de `PROGRESO:` se pruebe con
        // marcadores partidos entre tokens.
        for chunk in chunks(&self.reply) {
            if self.cancelled.load(Ordering::SeqCst) {
                return Err(BrainError::Cancelled);
            }
            on_token(&chunk);
        }
        Ok(Completion {
            text: self.reply.clone(),
            ..Completion::default()
        })
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn has_repo_tools(&self) -> bool {
        self.tools
    }
}

/// Trocea un texto en bloques de 8 caracteres respetando UTF-8.
fn chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(8).map(|w| w.iter().collect()).collect()
}

/// Fábrica de cerebros falsos con guion y contadores.
struct FakeFactory {
    probe: Probe,
    script: Mutex<VecDeque<String>>,
    default_reply: String,
    delay: Duration,
    tools: bool,
    spawned: AtomicU64,
}

impl Default for FakeFactory {
    fn default() -> Self {
        Self {
            probe: Probe::default(),
            script: Mutex::new(VecDeque::new()),
            default_reply: DEFAULT_REPLY.to_string(),
            delay: Duration::ZERO,
            tools: true,
            spawned: AtomicU64::new(0),
        }
    }
}

impl FakeFactory {
    /// Encola la respuesta del siguiente cerebro que se cree.
    fn queue_reply(&self, reply: &str) {
        lock(&self.script).push_back(reply.to_string());
    }

    fn next_reply(&self) -> String {
        lock(&self.script)
            .pop_front()
            .unwrap_or_else(|| self.default_reply.clone())
    }

    fn spawned(&self) -> u64 {
        self.spawned.load(Ordering::SeqCst)
    }
}

impl BrainFactory for FakeFactory {
    fn spawn(&self, _timeout: Duration) -> Result<Arc<dyn TokenStream + Send + Sync>, BrainError> {
        self.spawned.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeBrain {
            reply: self.next_reply(),
            delay: self.delay,
            tools: self.tools,
            cancelled: AtomicBool::new(false),
            probe: self.probe.clone(),
        }))
    }

    fn spawn_worker(
        &self,
        profile: SessionProfile,
        timeout: Duration,
    ) -> Result<Arc<dyn TokenStream + Send + Sync>, BrainError> {
        lock(&self.probe.profiles).push(profile);
        self.spawn(timeout)
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jarvis-sessions-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn config(dir: PathBuf) -> SessionConfig {
    SessionConfig {
        dir,
        timeout: Duration::from_secs(30),
        ..SessionConfig::default()
    }
}

fn manager(
    name: &str,
    factory: Arc<dyn BrainFactory>,
) -> (SessionManager, SpeechQueueHandle, PathBuf) {
    let dir = temp_dir(name);
    let queue = SpeechQueueHandle::new();
    let manager = SessionManager::start(factory, config(dir.clone()), queue.clone())
        .expect("el gestor debe arrancar");
    (manager, queue, dir)
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timeout esperando: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn status(manager: &SessionManager, id: SessionId) -> SessionStatus {
    manager.get(id).expect("la sub-sesión debe existir").status
}

fn open_gate() -> DeliveryGate {
    DeliveryGate {
        state: jarvis_core::JarvisState::Idle,
        speaking: false,
        last_voice_activity: None,
        quiet_gap: Duration::ZERO,
        muted: false,
        gaming_mode: false,
    }
}

/// Drena la cola de voz y devuelve lo que se diría.
fn spoken(queue: &SpeechQueueHandle) -> Vec<String> {
    let gate = open_gate();
    let mut out = Vec::new();
    while let Some(delivery) = queue.lock().pop_ready(&gate, Instant::now()) {
        out.push(delivery.text);
    }
    out
}

// ─── Ciclo de vida básico ────────────────────────────────────────────────────

#[test]
fn spawn_runs_the_first_turn_and_leaves_the_session_idle_hu62() {
    let factory = Arc::new(FakeFactory::default());
    let (manager, queue, dir) = manager("first-turn", factory.clone());

    let id = manager.spawn(
        "revisión de PRs de Felipe".into(),
        vec!["prs".into()],
        SessionProfile::ReadOnly,
        TaskOrigin::Voice,
        "revisa las PR de Felipe".into(),
    );
    wait_for("primer turno", || {
        status(&manager, id) == SessionStatus::Idle
    });

    let session = manager.get(id).unwrap();
    assert_eq!(
        session.status,
        SessionStatus::Idle,
        "al terminar un turno queda Idle, no Done"
    );
    assert_eq!(session.transcript.len(), 2);
    assert_eq!(session.transcript[0].role, TurnRole::User);
    assert_eq!(session.transcript[1].role, TurnRole::Agent);
    assert_eq!(session.last_summary.as_deref(), Some("Todo en orden."));
    assert!(session.unread, "el resultado aún no se ha escuchado");
    assert_eq!(session.profile, SessionProfile::ReadOnly);

    let detail = session.detail_path.expect("detalle en disco");
    assert!(std::fs::read_to_string(&detail)
        .unwrap()
        .contains("- sin novedades"));

    assert_eq!(factory.probe.profiles(), vec![SessionProfile::ReadOnly]);

    let events = manager.drain_events();
    assert!(events.contains(&SessionEvent::Created {
        id,
        title: "revisión de PRs de Felipe".into(),
    }));
    assert!(events.contains(&SessionEvent::Started { id }));
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::Replied { id: other, .. } if *other == id)));

    let news = spoken(&queue);
    assert!(
        news.iter()
            .any(|text| text.contains("Sobre revisión de PRs de Felipe")),
        "{news:?}"
    );

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_session_can_be_opened_idle_without_instruction_hu62() {
    let factory = Arc::new(FakeFactory::default());
    let (manager, _queue, dir) = manager("idle-born", factory.clone());
    let id = manager.spawn(
        "informe de Óscar".into(),
        Vec::new(),
        SessionProfile::ReadOnly,
        TaskOrigin::Ipc,
        String::new(),
    );
    assert_eq!(status(&manager, id), SessionStatus::Idle);
    assert_eq!(factory.spawned(), 0, "sin instrucción no hay cerebro");

    manager
        .send(id, "revisa el informe de Óscar".into())
        .unwrap();
    wait_for("turno", || status(&manager, id) == SessionStatus::Idle);
    assert_eq!(manager.get(id).unwrap().transcript.len(), 2);

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

// ─── CA-1: multiturno con historial visible ─────────────────────────────────

#[test]
fn send_on_idle_injects_the_previous_history_hu62() {
    let factory = Arc::new(FakeFactory::default());
    let (manager, _queue, dir) = manager("history", factory.clone());
    let id = manager.spawn(
        "revisión de PRs de Felipe".into(),
        Vec::new(),
        SessionProfile::ReadOnly,
        TaskOrigin::Voice,
        "revisa las PR de Felipe".into(),
    );
    wait_for("primer turno", || {
        status(&manager, id) == SessionStatus::Idle
    });

    factory.queue_reply("RESUMEN_HABLADO: Pagos revisados.\nDETALLE:\n- cobro duplicado");
    manager.send(id, "ahora enfócate en pagos".into()).unwrap();
    wait_for("segundo turno", || {
        manager
            .get(id)
            .is_some_and(|s| s.status == SessionStatus::Idle && s.transcript.len() == 4)
    });

    let prompts = factory.probe.prompts();
    assert_eq!(prompts.len(), 2, "un turno, un prompt");
    let second = &prompts[1];
    assert!(
        second.contains("Usuario: revisa las PR de Felipe"),
        "el historial previo debe ser visible: {second}"
    );
    assert!(
        second.contains("Agente: RESUMEN_HABLADO: Todo en orden."),
        "la respuesta anterior también: {second}"
    );
    assert!(second.contains("Nueva instrucción del usuario: ahora enfócate en pagos"));
    assert_eq!(
        manager.get(id).unwrap().last_summary.as_deref(),
        Some("Pagos revisados.")
    );
    assert_eq!(factory.spawned(), 2, "un cerebro por turno");

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

// ─── CA-2: lo que llega mientras trabaja no se pierde ───────────────────────

#[test]
fn send_on_working_is_queued_and_processed_in_order_hu62() {
    let factory = Arc::new(FakeFactory {
        delay: Duration::from_millis(150),
        ..FakeFactory::default()
    });
    let (manager, _queue, dir) = manager("queue", factory.clone());
    let id = manager.spawn(
        "revisión de PRs".into(),
        Vec::new(),
        SessionProfile::ReadOnly,
        TaskOrigin::Voice,
        "primera instrucción".into(),
    );
    wait_for("trabajando", || {
        status(&manager, id) == SessionStatus::Working
    });

    manager.send(id, "segunda instrucción".into()).unwrap();
    assert_eq!(
        status(&manager, id),
        SessionStatus::Working,
        "sigue en su turno; la nueva queda en su cola"
    );
    manager.send(id, "tercera instrucción".into()).unwrap();

    wait_for("tres turnos", || {
        manager
            .get(id)
            .is_some_and(|s| s.status == SessionStatus::Idle && s.transcript.len() == 6)
    });

    let session = manager.get(id).unwrap();
    let instructions: Vec<&str> = session
        .transcript
        .iter()
        .filter(|turn| turn.role == TurnRole::User)
        .map(|turn| turn.text.as_str())
        .collect();
    assert_eq!(
        instructions,
        vec![
            "primera instrucción",
            "segunda instrucción",
            "tercera instrucción"
        ],
        "el orden se respeta y no se pierde ninguna"
    );
    assert_eq!(factory.probe.prompts().len(), 3);
    assert_eq!(manager.get(id).unwrap().status, SessionStatus::Idle);

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

// ─── CA-3: pool de 6 y anuncio de `Queued` ──────────────────────────────────

#[test]
fn the_pool_queues_the_seventh_session_and_announces_it_once_hu62() {
    let factory = Arc::new(FakeFactory {
        delay: Duration::from_secs(30),
        ..FakeFactory::default()
    });
    let dir = temp_dir("pool");
    let queue = SpeechQueueHandle::new();
    let manager = SessionManager::start(
        factory.clone(),
        SessionConfig {
            max_parallel: SESSION_POOL_LIMIT,
            ..config(dir.clone())
        },
        queue.clone(),
    )
    .unwrap();

    let ids: Vec<SessionId> = (0..7)
        .map(|i| {
            manager.spawn(
                format!("hilo {i}"),
                Vec::new(),
                SessionProfile::ReadOnly,
                TaskOrigin::Voice,
                "trabaja".into(),
            )
        })
        .collect();

    wait_for("seis trabajando a la vez", || {
        let working = ids
            .iter()
            .filter(|id| status(&manager, **id) == SessionStatus::Working)
            .count();
        working == SESSION_POOL_LIMIT && factory.probe.max_in_flight() == SESSION_POOL_LIMIT as u64
    });

    let queued: Vec<SessionId> = ids
        .iter()
        .copied()
        .filter(|id| status(&manager, *id) == SessionStatus::Queued)
        .collect();
    assert_eq!(queued.len(), 1, "solo una espera hueco");
    assert_eq!(manager.active_count(), 7);

    // La séptima nace `Queued` antes de que su hilo intente tomar hueco: el
    // aviso llega un poco después, así que se espera en vez de leerlo ya.
    let mut announced: Vec<String> = Vec::new();
    let collect = |announced: &mut Vec<String>| {
        announced.extend(
            spoken(&queue)
                .into_iter()
                .filter(|text| text.contains("queda en espera")),
        );
    };
    wait_for("aviso de espera", || {
        collect(&mut announced);
        !announced.is_empty()
    });
    std::thread::sleep(Duration::from_millis(50));
    collect(&mut announced);
    assert_eq!(announced.len(), 1, "el estado Queued se anuncia una vez");

    manager.shutdown(Duration::from_secs(3));
    let _ = std::fs::remove_dir_all(dir);
}

// ─── CA-4: persistencia y rehidratación ─────────────────────────────────────

#[test]
fn sessions_are_restored_after_a_restart_hu62() {
    let factory = Arc::new(FakeFactory::default());
    let dir = temp_dir("restart");
    let queue = SpeechQueueHandle::new();
    let cfg = config(dir.clone());

    let id = {
        let manager = SessionManager::start(factory.clone(), cfg.clone(), queue.clone()).unwrap();
        let id = manager.spawn(
            "revisión de PRs de Felipe".into(),
            vec!["prs".into(), "felipe".into()],
            SessionProfile::ReadOnly,
            TaskOrigin::Voice,
            "revisa las PR".into(),
        );
        wait_for("turno", || status(&manager, id) == SessionStatus::Idle);
        manager.shutdown(Duration::from_secs(2));
        id
    };

    let manager = SessionManager::start(factory, cfg, queue).unwrap();
    let restored = manager.get(id).expect("rehidratada del disco");
    assert_eq!(restored.title, "revisión de PRs de Felipe");
    assert_eq!(
        restored.aliases,
        vec!["prs".to_string(), "felipe".to_string()]
    );
    assert_eq!(restored.profile, SessionProfile::ReadOnly);
    assert_eq!(restored.status, SessionStatus::Idle);
    assert_eq!(restored.origin, TaskOrigin::Voice);
    assert_eq!(restored.transcript.len(), 2);
    assert_eq!(restored.transcript[0].text, "revisa las PR");
    assert_eq!(restored.transcript[1].role, TurnRole::Agent);
    assert_eq!(restored.last_summary.as_deref(), Some("Todo en orden."));
    assert!(restored
        .detail_path
        .as_ref()
        .is_some_and(|path| path.exists()));

    // Los ids no se reutilizan tras el reinicio.
    let next = manager.spawn(
        "otro hilo".into(),
        Vec::new(),
        SessionProfile::Operator,
        TaskOrigin::Ipc,
        String::new(),
    );
    assert!(next > id, "{next} debe ser mayor que {id}");

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_session_left_working_becomes_idle_with_a_restart_note_hu62() {
    let dir = temp_dir("interrupted");
    let store = SessionStore::new(dir.clone());
    let orphan = SubSession {
        id: 5,
        title: "revisión de PRs".into(),
        aliases: Vec::new(),
        profile: SessionProfile::ReadOnly,
        status: SessionStatus::Working,
        origin: TaskOrigin::Voice,
        created_at_ms: 1,
        updated_at_ms: 1,
        transcript: vec![SessionTurn {
            role: TurnRole::User,
            text: "revisa".into(),
            at_ms: 1,
        }],
        last_summary: None,
        progress: Some("a medias".into()),
        pending_question: None,
        detail_path: None,
        unread: false,
    };
    store.write_meta(&orphan).unwrap();
    store.append_turn(5, &orphan.transcript[0]).unwrap();

    let manager = SessionManager::start(
        Arc::new(FakeFactory::default()),
        config(dir.clone()),
        SpeechQueueHandle::new(),
    )
    .unwrap();

    let restored = manager.get(5).expect("rehidratada");
    assert_eq!(
        restored.status,
        SessionStatus::Idle,
        "un cierre abrupto no deja la sesión Working"
    );
    assert!(restored.progress.is_none());
    assert_eq!(restored.transcript.len(), 2);
    let note = restored.transcript.last().unwrap();
    assert_eq!(note.role, TurnRole::System);
    assert_eq!(note.text, INTERRUPTED_NOTE);

    // Y sigue aceptando turnos.
    assert!(manager.send(5, "continúa".into()).is_ok());
    wait_for("turno tras el reinicio", || {
        manager
            .get(5)
            .is_some_and(|s| s.status == SessionStatus::Idle && s.transcript.len() == 4)
    });

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

// ─── CA-5: cancelación, timeout y preguntas ─────────────────────────────────

#[test]
fn cancel_stops_the_running_turn_and_marks_the_session_hu62() {
    let factory = Arc::new(FakeFactory {
        delay: Duration::from_secs(30),
        ..FakeFactory::default()
    });
    let (manager, queue, dir) = manager("cancel", factory);
    let id = manager.spawn(
        "monitor de logs".into(),
        Vec::new(),
        SessionProfile::Operator,
        TaskOrigin::Voice,
        "monitorea los logs".into(),
    );
    wait_for("trabajando", || {
        status(&manager, id) == SessionStatus::Working
    });

    assert!(manager.cancel(id));
    wait_for("cancelada", || {
        status(&manager, id) == SessionStatus::Cancelled
    });
    assert!(manager.drain_events().contains(&SessionEvent::Finished {
        id,
        status: SessionStatus::Cancelled
    }));
    assert!(
        spoken(&queue).is_empty(),
        "una cancelación no se locuta como resultado"
    );
    assert!(!manager.cancel(999), "id inexistente");

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_timed_out_turn_marks_the_session_and_speaks_hu62() {
    let factory = Arc::new(FakeFactory {
        default_reply: TIMEOUT_SENTINEL.to_string(),
        ..FakeFactory::default()
    });
    let (manager, queue, dir) = manager("timeout", factory);
    let id = manager.spawn(
        "revisión larga".into(),
        Vec::new(),
        SessionProfile::ReadOnly,
        TaskOrigin::Voice,
        "revisa los logs".into(),
    );
    wait_for("timeout", || {
        status(&manager, id) == SessionStatus::TimedOut
    });
    let news = spoken(&queue);
    assert!(
        news.iter().any(|text| text.contains("tardó demasiado")),
        "{news:?}"
    );
    assert!(manager
        .get(id)
        .unwrap()
        .transcript
        .iter()
        .any(|turn| { turn.role == TurnRole::System && turn.text.contains("no respondió") }));

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_question_leaves_the_session_awaiting_user_hu62() {
    let factory = Arc::new(FakeFactory {
        default_reply: "PREGUNTA: ¿Reviso también los borradores?\n\
                        RESUMEN_HABLADO: Encontré dos PR abiertos.\n\
                        DETALLE:\n- PR 12"
            .to_string(),
        ..FakeFactory::default()
    });
    let (manager, queue, dir) = manager("question", factory.clone());
    let id = manager.spawn(
        "revisión de PRs".into(),
        Vec::new(),
        SessionProfile::ReadOnly,
        TaskOrigin::Voice,
        "revisa las PR".into(),
    );
    wait_for("pregunta", || {
        status(&manager, id) == SessionStatus::AwaitingUser
    });

    let session = manager.get(id).unwrap();
    assert_eq!(
        session.pending_question.as_deref(),
        Some("¿Reviso también los borradores?")
    );
    assert_eq!(
        session.last_summary.as_deref(),
        Some("Encontré dos PR abiertos.")
    );
    assert!(manager.drain_events().iter().any(|event| matches!(
        event,
        SessionEvent::Question { id: other, text } if *other == id && text.contains("borradores")
    )));
    assert!(
        spoken(&queue).iter().any(|text| text.contains("decisión")),
        "la pregunta se habla"
    );

    // La respuesta del usuario arranca el turno siguiente y limpia la pregunta.
    factory.queue_reply(DEFAULT_REPLY);
    manager
        .send(id, "sí, revisa los borradores".into())
        .unwrap();
    wait_for("respuesta", || {
        manager
            .get(id)
            .is_some_and(|s| s.status == SessionStatus::Idle && s.transcript.len() == 4)
    });
    assert!(manager.get(id).unwrap().pending_question.is_none());

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn progress_lines_update_the_session_without_being_spoken_hu62() {
    let factory = Arc::new(FakeFactory {
        default_reply: "PROGRESO: leyendo la PR 12\n\
                        - PROGRESO: comparando los diffs\n\
                        RESUMEN_HABLADO: Listo.\n\
                        DETALLE:\n- ok"
            .to_string(),
        ..FakeFactory::default()
    });
    let (manager, queue, dir) = manager("progress", factory);
    let id = manager.spawn(
        "revisión de PRs".into(),
        Vec::new(),
        SessionProfile::ReadOnly,
        TaskOrigin::Voice,
        "revisa las PR".into(),
    );
    wait_for("turno", || status(&manager, id) == SessionStatus::Idle);

    assert_eq!(
        manager.get(id).unwrap().progress.as_deref(),
        Some("comparando los diffs"),
        "el estado guarda el último avance"
    );
    let progress: Vec<String> = manager
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            SessionEvent::Progress { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        progress,
        vec![
            "leyendo la PR 12".to_string(),
            "comparando los diffs".to_string()
        ],
        "los avances se detectan aunque lleguen troceados"
    );
    let news = spoken(&queue);
    assert!(
        !news.iter().any(|text| text.contains("comparando")),
        "un avance no se lee en voz alta: {news:?}"
    );
    assert!(news
        .iter()
        .any(|text| text.contains("Sobre revisión de PRs")));

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

// ─── Guardas: herramientas, cierre y errores de la API ──────────────────────

#[test]
fn a_session_without_repo_tools_warns_instead_of_inventing_hu62() {
    let factory = Arc::new(FakeFactory {
        tools: false,
        ..FakeFactory::default()
    });
    let (manager, queue, dir) = manager("no-tools", factory);
    let id = manager.spawn(
        "revisión de PRs".into(),
        Vec::new(),
        SessionProfile::ReadOnly,
        TaskOrigin::Voice,
        "revisa las PR de Felipe".into(),
    );
    wait_for("fallida", || status(&manager, id) == SessionStatus::Failed);

    assert!(spoken(&queue).iter().any(|text| text == REPO_TOOLS_NOTICE));
    let last = manager.get(id).unwrap().transcript.last().cloned().unwrap();
    assert_eq!(last.role, TurnRole::System);
    assert_eq!(last.text, REPO_TOOLS_NOTICE.trim_end_matches('.'));

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn close_ends_the_session_and_rejects_further_instructions_hu62() {
    let factory = Arc::new(FakeFactory::default());
    let (manager, _queue, dir) = manager("close", factory);
    let id = manager.spawn(
        "revisión de PRs".into(),
        Vec::new(),
        SessionProfile::ReadOnly,
        TaskOrigin::Voice,
        String::new(),
    );
    assert!(manager.close(id));
    assert_eq!(status(&manager, id), SessionStatus::Done);
    assert_eq!(
        manager.send(id, "otra cosa".into()),
        Err(SessionError::Closed(id))
    );
    assert_eq!(
        manager.send(999, "otra cosa".into()),
        Err(SessionError::NotFound(999))
    );
    assert_eq!(
        manager.send(id, "   ".into()),
        Err(SessionError::EmptyInstruction)
    );
    assert!(!manager.close(999));
    assert!(!SessionStatus::Done.accepts_instructions());
    assert!(SessionStatus::Cancelled.accepts_instructions());

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn list_and_active_count_follow_the_registry_hu62() {
    let factory = Arc::new(FakeFactory {
        delay: Duration::from_secs(30),
        ..FakeFactory::default()
    });
    let (manager, _queue, dir) = manager("list", factory);
    let one = manager.spawn(
        "hilo uno".into(),
        Vec::new(),
        SessionProfile::ReadOnly,
        TaskOrigin::Voice,
        "trabaja".into(),
    );
    let two = manager.spawn(
        "hilo dos".into(),
        Vec::new(),
        SessionProfile::Operator,
        TaskOrigin::Ipc,
        String::new(),
    );
    assert_eq!(
        manager.list().iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![one, two]
    );
    wait_for("uno activo", || manager.active_count() == 1);
    assert_eq!(status(&manager, two), SessionStatus::Idle);

    manager.shutdown(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(dir);
}

// ─── Unidades: protocolo de salida ──────────────────────────────────────────

#[test]
fn parse_output_reads_summary_detail_and_question() {
    let parsed = parse_output(
        "PROGRESO: mirando\nPREGUNTA: ¿Sigo con los borradores?\n\
         RESUMEN_HABLADO: Dos PR abiertos.\nDETALLE:\n- PR 12",
    );
    assert_eq!(parsed.summary, "Dos PR abiertos.");
    assert_eq!(parsed.detail, "- PR 12");
    assert_eq!(
        parsed.question.as_deref(),
        Some("¿Sigo con los borradores?")
    );

    let without = parse_output("RESUMEN_HABLADO: Nada urgente.\nDETALLE:\n- ok");
    assert!(without.question.is_none());
}

#[test]
fn parse_output_accepts_a_question_on_the_following_lines() {
    let parsed =
        parse_output("**PREGUNTA:**\n¿Reviso el repo de Melón?\n\nRESUMEN_HABLADO: Hecho.");
    assert_eq!(
        parsed.question.as_deref(),
        Some("¿Reviso el repo de Melón?")
    );
    assert_eq!(parsed.summary, "Hecho.");
}

#[test]
fn parse_output_falls_back_when_the_model_ignores_the_protocol() {
    let parsed = parse_output("No usé el formato pedido.");
    assert_eq!(parsed.summary, "No usé el formato pedido.");
    assert_eq!(parsed.detail, "No usé el formato pedido.");
    assert!(parsed.question.is_none());
}

#[test]
fn progress_scanner_survives_tokens_split_mid_marker() {
    let mut scanner = ProgressScanner::default();
    let mut out = Vec::new();
    for chunk in chunks("PROGRESO: uno\nPROGRE") {
        scanner.feed(&chunk, &mut out);
    }
    assert_eq!(out, vec!["uno".to_string()]);
    scanner.feed("SO: dos\n", &mut out);
    assert_eq!(out, vec!["uno".to_string(), "dos".to_string()]);

    scanner.feed("PROGRESO: tres", &mut out);
    scanner.flush(&mut out);
    assert_eq!(out.last().map(String::as_str), Some("tres"));
}

#[test]
fn truncate_chars_never_breaks_utf8() {
    let text = "á".repeat(10);
    let clipped = truncate_chars(&text, 3);
    assert!(clipped.starts_with("ááá"));
    assert!(clipped.ends_with("[truncado]"));
    assert_eq!(truncate_chars("corto", 10), "corto");
}
