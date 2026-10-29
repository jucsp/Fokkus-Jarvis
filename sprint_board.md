# Sprint Board — Fokkus-Jarvis

**Épica Actual:** Épica 16 (E-16) Voz Expresiva y STT Coloquial · v0.3.3
**Sprint:** E-16 (HU-73 y HU-74) — IMPLEMENTADO. Arrastre de E-15: release `models-v1` (69.2), IPC `jarvis.modelStatus` (69.5) y pruebas manuales 🧪
**Actualizado:** 2026-10-29 · **PM/QA:** Claude · **PO:** Antigravity · **Dev:** DeepSeek

Leyenda: `[x]` hecho y verificado con tests · `[ ]` pendiente · 🧪 requiere prueba manual

---

## 🏁 COMPLETADO: ÉPICA 16 (E-16) — Voz Expresiva y STT Coloquial · v0.3.3
> Verificado 2026-10-29 (v0.3.3): `cargo test -p jarvis-audio` 515 pasados (509 + 6), 0 fallidos, 4 ignorados; `cargo test --workspace` 957 pasados, 0 fallidos, 7 ignorados; `cargo clippy --workspace --all-targets -D warnings` y `cargo fmt --check` limpios; `cargo build --release -p jarvis-app` OK; binario instalado reporta `Fokkus-Jarvis 0.3.3`.
> v0.3.2: 512 pasados en `jarvis-audio`; primera entrega de HU-73 y HU-74.

### HU-73 — Rango emocional y prosodia perceptible en Edge-TTS ✅
- [x] 73.1 `tts/directive.rs`: nuevo ánimo **Asustado** en el protocolo de directivas [Responsable: DeepSeek / Claude]
- [x] 73.2 `tts/directive.rs` + `tts/phrase.rs`: offsets de rate/pitch/volumen ampliados hasta ser perceptibles por ánimo [Responsable: DeepSeek / Claude]
- [x] 73.3 `brain/cli.rs` + `brain/client.rs`: recordatorio del protocolo de prosodia en la sesión persistente de voz (agy entre turnos) [Responsable: DeepSeek / Claude]
- [x] 73.4 QA: aserciones de offsets actualizadas, suite `jarvis-audio` en verde [Responsable: Claude]
- [x] 73.5 (v0.3.3) `tts/directive.rs`: nuevo **`Mood::Normal`** (identidad, offsets 0/0/0; alias `neutro`, `neutral`, `plano`) como estado de partida de cada turno: ninguna emoción se arrastra al turno siguiente [Responsable: DeepSeek / Claude]
- [x] 73.6 (v0.3.3) Calibración natural sobre `es-US-AlonsoNeural`: tono máx. ±14 Hz y ritmo ±10 % (antes +40 Hz/+22 % → efecto ardilla, −18 Hz → efecto Thanos) [Responsable: DeepSeek / Claude]
- [x] 73.7 (v0.3.3) `brain/cli.rs` + `brain/client.rs`: el prompt fija tono normal por defecto y emoción solo si se pide, volviendo a `[ánimo:normal]` (antes exigía una directiva en cada respuesta y el modelo se quedaba en `serio`) [Responsable: DeepSeek / Claude]
- [x] 73.8 (v0.3.3) QA: corregido el test de orden de tono (Calmo −4 Hz queda sobre Serio −5 Hz) y un `mut` sobrante que rompía clippy [Responsable: Claude]

### HU-74 — Expansión masiva del diccionario STT ✅
- [x] 74.1 `text/lexicon.rs`: `BASE_ENTRIES` ampliado con jerga chilena, tecnicismos y deformaciones típicas de Whisper [Responsable: DeepSeek / Claude]
- [x] 74.2 QA: depuradas variantes que corregían frases correctas (falsos positivos) y `fmt --check` limpio [Responsable: Claude]
- [x] 74.3 (v0.3.3) `text/lexicon.rs`: lote adicional de transcripciones fonéticas reales (`socuen` → Soy Juan, `alvarlo` → Háblalo, `avelosidad` → a velocidad, `comardilla` → como ardilla, `su minito` → un grito, `asperate` → espérate, `tiketin plane` → ticket en Plane, `punto de mede` → archivo .md, `mieselo` → envíaselo, `gamboy advance` → Game Boy Advance) [Responsable: DeepSeek / Claude]
- [x] 74.4 (v0.3.3) QA: casos nuevos en `transcript_history_terms_are_present`; variantes `archivo punto de mede` para no escribir "archivo archivo .md" [Responsable: Claude]

---

## 🏃 EN CIERRE: ÉPICA 15 (E-15) — Excelencia Conversacional, STT Seguro e Introspección Nativa
> SDD: [`docs/specs/sdd-epica-15-excelencia-conversacional.md`](docs/specs/sdd-epica-15-excelencia-conversacional.md). Orden: **Fase A** HU-71 → HU-70 · **Fase B** HU-69 → HU-72. Leer §2 de la SDD (correcciones P1–P5) antes de codificar.
> Verificado 2026-10-28 (commits `a74c914`, `9d960dd`, `d121c37`, `dcc6fe0`): `make test` **938 pasados, 0 fallidos, 5 ignorados**; `make deny` limpio (advisories, bans, licenses, sources ok).

### HU-71 — Erradicación de `bare_search` y restricciones negativas (Fase A) ✅
- [x] 71.1 `jarvis-audio`: eliminar `bare_search` (`orchestrator.rs:1500-1517`) y su llamada (`:1572`); limpiar `LOCAL_SEARCH_WORDS` si queda sin uso; reescribir los tests de "buscar sin sitio" al nuevo contrato (va al router) [Responsable: DeepSeek]
- [x] 71.2 `jarvis-audio`: `site_request` exige verbo afirmativo sin negación en su cláusula; quitar la dependencia de `busques` como disparador [Responsable: DeepSeek]
- [x] 71.3 `jarvis-audio`: tipo `TurnConstraints { no_web, … }` extraído **antes** de `local_command`; con `no_web` se bloquea toda `LocalCommand::OpenUrl` [Responsable: DeepSeek]
- [x] 71.4 `router/`: `TurnPlan.constraints` con `#[serde(default)]`; OR con lo extraído del texto (el planificador no puede desactivarlo); registro en `PlannerTrace` y `--diag-turns` [Responsable: DeepSeek]
- [x] 71.5 `brain/client.rs`: instrucción dura "no usar búsqueda web ni abrir el navegador" cuando `no_web` (resuelto como `NO_WEB_DIRECTIVE` en el contexto efímero del turno, no en `client.rs`) [Responsable: DeepSeek]
- [x] 71.6 QA: tabla de verdad ≥ 30 casos (regresión P5 "no busques en Google, dime…", cláusulas mixtas, "no sé, búscalo en Google"), suite de comandos locales sin regresión, `make test`/`make lint`/`make fmt-check` [Responsable: Claude]

### HU-70 — Corrector léxico/fonético determinista (Fase A) ✅
- [x] 70.1 `jarvis-audio/src/text/lexicon.rs`: índice de frases (palabra completa, más larga primero, búsqueda insensible a tildes y mayúsculas, reemplazo que conserva el resto del texto) [Responsable: DeepSeek]
- [x] 70.2 Diccionario base embebido ≥ 60 entradas (tecnicismos + jerga chilena) [Responsable: DeepSeek · revisión de términos: PO]
- [x] 70.3 `jarvis-core`: tabla `[stt_lexicon]` con límites (≤ 500 entradas, ≤ 64 caracteres, sin control) y bloqueo de wake-word y frases de anulación [Responsable: DeepSeek]
- [x] 70.4 Inserción post-STT y antes de `local_command`/router/HUD; ampliar `DEFAULT_INITIAL_PROMPT` con sesgo técnico corto (test de longitud) [Responsable: DeepSeek]
- [x] 70.5 Log de correcciones `[lexicon] …` junto a `[stt] escuchado` (siempre activo, no solo con `--diag-turns`) [Responsable: DeepSeek]
- [x] 70.6 QA: ≥ 60 positivos + ≥ 20 negativos, tests de HU-59 (`normalize.rs`) intactos, benchmark p99 < 1 ms, léxico malicioso rechazado [Responsable: Claude]

### HU-69 — STT `small-q5_1` con orígenes soberanos sin Hugging Face (Fase B)
- [x] 69.1 Pipeline del mantenedor `scripts/build-whisper-models.sh` (+ target Makefile): `.pt` desde `openaipublic.azureedge.net` con hash de la ruta verificado → conversión en contenedor sin red con `python3-torch`/`python3-numpy` de la distro → `quantize` q5_1 del whisper.cpp vendorizado → `SHA256SUMS` + `PROVENANCE.md` [Responsable: DeepSeek · implementación: Claude]
- [ ] 69.2 Publicar el release inmutable `models-v1` en `jucsp/Fokkus-Jarvis` con tiny, base-q5_1 y small-q5_1 [Responsable: PO · requiere autorización del usuario] — ⚠️ BLOQUEANTE: hoy responde 404; una instalación nueva no puede descargar modelos
- [x] 69.3 `models.rs`: `WHISPER_SMALL_Q5_1`; migrar tiny/base a GitHub Releases; eliminar `HF_WHISPER_COMMIT`; invertir el test de catálogo (prohíbe HF y exige el prefijo de releases con tag) [Responsable: DeepSeek · implementación: Claude]
- [x] 69.4 `HttpSource`: allowlist de hosts de redirección (`github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com`), solo HTTPS; test con `ModelSource` falso [Responsable: DeepSeek · implementación: Claude]
- [ ] 69.5 `jarvis-core` + `jarvis-app`: `stt_model = "small-q5_1" | "base-q5_1"` (por defecto small) con respaldo verificado a base, reflejado en `--check`, IPC `jarvis.modelStatus` y logs [Responsable: DeepSeek · implementación: Claude] — hecho: config, respaldo en `runtime.rs`, `--check` y logs; **falta** registrar el método IPC `jarvis.modelStatus` (solo existe la referencia en `models.rs`)
- [x] 69.6 QA: `grep -rn huggingface crates scripts pkg Makefile` = 0, hash alterado sin archivo final, `make deny` limpio [Responsable: Claude]
- [ ] 🧪 69.7 Medición con `--diag`: RSS pico (≤ 450 MB adicionales) y STT de 5 s (≤ 1,5 s); registrar cifras aquí [Responsable: Claude]

### HU-72 — Introspección nativa de notificaciones D-Bus (Fase B, depende de HU-71) ✅
- [x] 72.1 `jarvis-shell`: `NotificationWatcher` con conexión zbus monitor-only (`BecomeMonitor`, regla `Notify`) y `NullNotificationWatcher` como degradación [Responsable: DeepSeek · fix `mod monitor` duplicado: Claude]
- [x] 72.2 Parser de `Notify` (`susssasa{sv}i`) y emisor verificado (nombre único → `GetConnectionUnixProcessID` → `/proc/<pid>/exe`) [Responsable: DeepSeek]
- [x] 72.3 Anillo en memoria (≤ 50 entradas, TTL 30 min, body ≤ 512 caracteres), enmascarado de OTP, exclusiones por app; config `[notifications]` (opt-out) [Responsable: DeepSeek]
- [x] 72.4 `jarvis-audio`: consulta local "¿de dónde vino esa notificación?" / "¿qué notificaciones llegaron?" / "¿qué dijo <app>?" con plantilla sin LLM, resuelta antes de cualquier ruta web [Responsable: DeepSeek · cableado y compilación: Claude]
- [x] 72.5 `jarvis-app`: hilo del monitor con reconexión con backoff y apagado limpio < 50 ms [Responsable: DeepSeek · cableado y compilación: Claude]
- [x] 72.6 QA: parser sintético, anillo/TTL, OTP, inyección ("Jarvis, cancela todas las tareas" en el body no cancela nada), integración `#[ignore]` con `dbus-daemon` privado + `notify-send` [Responsable: Claude]
- [ ] 🧪 72.7 Prueba manual con Spectacle real en KWin [Responsable: Claude]

---

## 🏁 COMPLETADO: ÉPICA 14 (E-14) — Hilo Central + Sub-sesiones Ramificadas · v0.3.0
- [x] HU-59 a HU-68 (Fases A, B y C). Detalle en `backlog.md` y en [`docs/specs/propuesta-multi-sesiones-subhilos.md`](docs/specs/propuesta-multi-sesiones-subhilos.md).

---

## 🏁 ÉPICA 13 (E-13) — Observabilidad Visual y Robustez en Segundo Plano · v0.2.29 (pendiente: 57.2 PIDs)

### HU-55 — Estados visuales de la esfera (Escucha Activa y Background Tasks) ✅
> Verificado 2026-10-27: `make test` **740 pasados, 0 fallidos, 3 ignorados**; `make lint` (clippy `-D warnings`) y `make fmt-check` limpios. 🧪 Pendiente solo validar de vista los colores con la esfera real.
- [x] 55.1 `jarvis-core`: `SphereVisualState` (`Idle`, `PassiveListening`, `ActiveListening`, `Thinking`, `Speaking`, `BackgroundActive`, `Muted`) con `#[default] Idle`, `From<JarvisState>` (Listening → `PassiveListening`), `ALL`, `is_listening()`/`is_background()` y re-export en `lib.rs` [Responsable: DeepSeek · fix de inferencia de tipos en el test `visual_state_default_is_idle`: Claude]
- [x] 55.2 `jarvis-render`: `palette_for(SphereVisualState)` (Verde `#4CAF50` pasiva, Ámbar `#FFC107` activa, Naranja `#FF7043` background), `render_into(state: impl Into<SphereVisualState>)` compatible con `JarvisState`, `ActiveListening` a ×1.5 de velocidad angular [Responsable: DeepSeek]
- [x] 55.3 `jarvis-audio`: `VoiceOrchestrator::is_active_turn()` (frase retenida tras conector o continuación esperada —publicada por `ControlLoop::reconcile` en un `AtomicBool`—, turno despachado en curso o ventana de seguimiento abierta), `active_tasks_count()` / `has_background_tasks()`; `TaskManager::active_count()` / `has_active_tasks()` sin clonar registros [Responsable: Claude]
- [x] 55.4 `jarvis-app`: `FrameSource` devuelve `(JarvisState, SphereVisualState, AudioLevel)`; la esfera se dibuja con el estado visual y silencio/presencia/HUD/cadencia siguen con `JarvisState`. Mapeo puro `runtime::sphere_visual_state(state, active_turn, background)` usado por la clausura `source` de `main.rs` [Responsable: Claude]
- [x] 55.5 Tests: tabla de mapeo completa (`sphere_visual_state_table_matches_hu55`), turno activo con frase retenida y ventana de seguimiento (`active_turn_tracks_held_phrase_and_follow_up_window`), tareas vivas (`background_tasks_are_reported_while_alive`, `without_task_manager_there_are_no_background_tasks`) y conteo en `TaskManager` [Responsable: Claude]

### HU-56 — Calibración anti-eco en Barge-In e interrupción manual inmediata ✅
> Verificado 2026-10-27 junto a HU-57/58: `cargo test --workspace` **747 pasados, 0 fallidos, 3 ignorados**; clippy `-D warnings` y `fmt --check` limpios. 🧪 Pendiente probar con parlante real la sensibilidad del nuevo umbral.
- [x] 56.1 `jarvis-audio`: `evaluate_candidate` acepta voz humana con 2 frames con voz o un tercio de la ventana (antes la mitad) [Responsable: DeepSeek]
- [x] 56.2 `jarvis-shell` / `jarvis-app`: clic simple en la esfera → `ShellCommand::Interrupt` → `VoiceOrchestrator::interrupt()` (corte acústico, cancelación del LLM, vuelta a `Listening`, `ControlMsg::Interrupt` al hilo de voz) [Responsable: Claude]
- [x] 56.3 Tests: `simple_click_on_the_sphere_interrupts_the_utterance`, `headless_delivers_the_interrupt_command`, `manual_interrupt_cuts_the_utterance_and_returns_to_listening_hu56` (idempotente, sin efecto en reposo); regresión de eco (`own_echo_pauses_briefly_then_resumes_and_is_not_transcribed`, `without_continuation_window_the_same_voice_is_discarded_as_echo`) en verde [Responsable: Claude]

### HU-57 — Subagentes ejecutores de tareas OS en segundo plano
- [x] 57.1 `jarvis-audio`: `is_operational_task()` (`OPS_VERBS` corre/ejecuta/lanza/inicia/monitorea/deja/pon + `OPS_NOUNS` monitor/ping/contador/script/proceso/daemon/segundo plano/bash/comando) dentro de `is_dev_task()` → usan `dev_task_prompt` (ejecuta en el equipo, sin `setsid`/`disown`/`nohup` para que la cancelación mate el grupo). Título propio ("el monitor"/"el proceso") y acuse "Dejo el proceso corriendo… dime «detén el proceso»" [Responsable: Claude]
- [ ] 57.2 `jarvis-audio`: persistencia y seguimiento de PIDs en `running_processes.json` para matar procesos hijos al cancelar [Responsable: DeepSeek]
- [x] 57.3 `jarvis-audio`: `CancelTarget { All, Latest, Named }`; verbos cancela/detén/para/termina/aborta/mata (los no-"cancela" solo al inicio, tras muletillas) + sustantivos tarea/proceso/trabajo/monitor/ping/contador (plurales = todas). "detén el proceso" → la más reciente; "… de X" → por título. La cancelación se evalúa antes que listar tareas [Responsable: Claude]
- [x] 57.4 Tests: `operational_tasks_use_the_executing_prompt`, `flexible_cancel_phrases_are_recognized_hu57` (incluye negativos "compra un cable para el monitor", "para qué sirve el ping"), e2e `operational_task_runs_with_the_executing_prompt_and_stops_by_voice_hu57` [Responsable: Claude]

### HU-58 — Aislamiento headless de herramientas MCP
- [x] 58.1 `jarvis-audio` (`brain/cli.rs`): `spawn` inyecta `BROWSER=/bin/true` y `NO_BROWSER=1` al CLI hijo (y por herencia a MCPs y scripts) [Responsable: Claude]
- [x] 58.2 Test `child_runs_headless_without_browser_hu58` (el hijo ve `/bin/true|1`). 🧪 Pendiente validar con workspace-mcp real que no abre el navegador [Responsable: Claude]

---

## 🏁 COMPLETADO RECIENTEMENTE: E-12 FASE 1 (Quick Wins Conversacionales y Prosodia)

### HU-41 — Endpointing configurable y observable
- [x] 41.1 Campos en `jarvis-core`: tabla `[voice_endpoint]` (`voice_open`, `voice_hold`, `silence_ms`, `extended_silence_ms`, `max_segment_ms`, `connector_grace_ms`) con serde y saturación de rangos [Responsable: DeepSeek · fix serialización `f32` corta: Claude]
- [x] 41.2 Exposición en `jarvis-audio`: `EndpointConfig: From<&VoiceEndpointConfig>`, enum `CloseReason` (`Silence`, `MaxTimeout`, `Connector`) en `VoiceEvent::EndOfTurn`, `TurnPauseStats` (P50/P90, anillo de 256) y conteo en `VoiceStats` [Responsable: DeepSeek]
- [x] 41.3 Cableado en `jarvis-app`: `EngineConfig.endpoint`, `connector_grace` y `vad_threshold` desde config en `main.rs`; histograma DSP→`DspMetrics::pause_stats()`; flag `--diag-turns [segundos]` (`diag.rs`) [Responsable: Claude]
- [x] 41.4 Validación y QA: tests en core/audio/app, `make test`, clippy `-D warnings` y `fmt-check` limpios [Responsable: Claude]
- [ ] 41.5 🧪 Prueba manual: `fokkus-jarvis --diag-turns 60` hablando con pausas naturales y calibrar `silence_ms` según P90 [Responsable: PO]

**QA 2026-10-27 (Claude):** `make test` → 652 passed / 0 failed / 3 ignored · clippy `-D warnings` OK · `make fmt-check` OK · `cargo deny` ❌ advisories preexistentes (RUSTSEC-2026-0385 `ctutils`, -0382 `rustls-webpki`, -0381 `webpki-roots`; Cargo.lock sin cambios) → requiere `cargo update -p` autorizado por PO.

---

### HU-42 — Ventana de continuación sin eco falso
- [x] 42.1 Chime diferido (`chime_delay` 450 ms) emitido desde el hilo de voz y cancelado si el usuario retoma; ventana de continuación (`continuation_window` 1,5 s); fusión en `Speaking` temprano (`speaking_merge_window` 800 ms); contador `stt_in_flight` contra la carrera STT/`VoiceStart` (H-T6) [Responsable: DeepSeek]
- [x] 42.2 Fixes QA [Responsable: Claude]: (a) la ventana ya no anula la defensa anti-eco cuando Jarvis suena con la respuesta del **turno en curso** (`sounded_since_dispatch`; rompía 2 tests de HU-29 con `barge_in=false`); (b) `in_early_speech()` se evaluaba **después** de `barge_in()` (estado ya en `Listening`) → la fusión tras barge-in temprano nunca ocurría; (c) `speech_started` solo lo fijaba `reconcile()` → un `VoiceStart` previo al tick perdía la ventana temprana
- [x] 42.3 Tests de orquestador: chime diferido tras el retardo, chime cancelado + turno fusionado, voz en ventana no es eco (+ control negativo sin ventana), fusión por segmento largo en `Speaking` temprano, barge-in temprano fusiona / tardío abre pregunta nueva, STT lento no parte la frase (H-T6) [Responsable: Claude]

### HU-43 — Detector léxico de incompletitud
- [x] 43.1 Módulo `jarvis-audio/src/incompleteness.rs`: `incompleteness()` → `IncompleteCue` (`Connector`, `Filler`, `Preposition`, `Determiner`, `Auxiliary`, `TrailingOff`), `is_incomplete()`, `incompleteness_score()` (peso `p_text` para HU-47). `ends_with_connector` queda como alias; `SttOutcome.connector` → `incomplete` [Responsable: Claude]
- [x] 43.2 `stt.rs`: `DEFAULT_INITIAL_PROMPT` con disfluencias ("Jarvis, eh... este, mmm, o sea, como que, igual...") por defecto en `WhisperEngine`; `without_initial_prompt()` / `initial_prompt()`; `runtime.rs` deja de pisarlo con "Jarvis." [Responsable: Claude]
- [x] 43.3 Tabla de verdad de 65 casos + tests de vacilaciones y etiquetas; integración: muletilla final y preposición final retienen el turno y fusionan la continuación [Responsable: Claude]
- [ ] 43.4 🧪 Prueba manual con whisper real: verificar que conserva "eh"/"o sea" y que el prompt no se alucina en silencio [Responsable: PO]

### HU-44 — Turnos largos sin corte duro
- [x] 44.1 `VoiceEvent::Partial { segment, index }` y `EndOfTurn.partials`; el endpointer entrega un fragmento cada `max_segment_ms` sin cerrar el turno y cierra por silencio real o al techo `max_turn_ms` (`CloseReason::MaxTimeout`). Memoria acotada a un fragmento [Responsable: Claude]
- [x] 44.2 `max_turn_ms` en `[voice_endpoint]` (default 120 000, rango 5 000–120 000, nunca < `max_segment_ms`); `--diag-turns` muestra parciales y recomienda `max_turn_ms` [Responsable: Claude]
- [x] 44.3 Orquestador/STT: los parciales se transcriben y acumulan como texto (`LongTurnText`), activación e incompletitud se evalúan sobre el turno completo; un parcial mantiene vivo un turno retenido por conector; un cierre sin parciales nunca hereda texto huérfano [Responsable: Claude]
- [x] 44.4 Tests: parcial a 30 s + cierre por silencio sin pérdida de audio, techo de 120 s con 3 parciales, memoria ≤ 1 fragmento, pausa que cruza el corte, turno siguiente limpio, acumulación de punta a punta en el orquestador [Responsable: Claude]
- [ ] 44.5 🧪 Prueba manual: dictar > 60 s y verificar una sola respuesta [Responsable: PO]

**QA HU-42/43/44 2026-10-27 (Claude):** `make test` → 679 passed / 0 failed / 3 ignored · suite del orquestador 10/10 corridas estables · clippy `-D warnings` OK · `make fmt-check` OK. `cargo deny` sin re-ejecutar (advisories preexistentes, Cargo.lock sin cambios).

### HU-45 — Protocolo de directivas de prosodia
- [x] 45.1 `tts/directive.rs`: `SpeechDirective { mood, rate, pitch, volume, pause_before_ms, pause_after_ms, breath }`, `Mood` (alegre, serio, empático, entusiasta, calmo, curioso) y `DirectiveFilter` incremental para `[ánimo:…]`, `[ritmo:…]`, `[pausa:N]`, `[respira]`, `[énfasis]…[/énfasis]`, `[susurro]…[/susurro]` (con o sin tildes, sin distinguir mayúsculas) [Responsable: Claude]
- [x] 45.2 Robustez: los tags partidos entre tokens se reensamblan; los desconocidos o malformados se descartan en silencio; un tag roto al final del stream o cortado por un espacio no se lee en voz alta; `[1]`, `array[0]`, `[ nota ]` siguen siendo texto [Responsable: Claude]
- [x] 45.3 `PhraseSplitter::drain_directed()` / `finish_directed()` → `DirectedPhrase`; ánimo y ritmo persisten durante el turno; pausa, respiración y susurro fuerzan un corte de frase. HUD, TTS, `last_assistant_response` y las tareas de fondo (`strip_directives`) nunca reciben tags [Responsable: Claude]
- [x] 45.4 `SYSTEM_PROMPT`: bloque con el protocolo, puntuación expresiva y un máximo de 1 o 2 directivas por respuesta, más un ejemplo [Responsable: Claude]
- [x] 45.5 Tests: los 19 tags válidos, tags desconocidos y malformados, troceo de a un carácter, fuzzing (2 000 casos en el filtro y 1 000 en el splitter) que compara el stream troceado contra el texto completo y busca fugas [Responsable: Claude]

### HU-46 — Render expresivo sobre Edge-TTS
- [x] 46.1 `edge.rs`: `edge_speech_config()` / `synthesize_edge_with()` traducen la directiva al `<prosody>` de Edge (`rate` en %, `pitch` en Hz, `volume` en %). Tabla de ánimos (por ejemplo, entusiasta +8 %/+2 Hz/+5 % y empático −8 %/−2 Hz/−3 %), ritmo ±12 %, susurro −25 % de volumen, énfasis +5 % y pregunta +2 Hz, con límites [Responsable: Claude]
- [x] 46.2 `tts/prosody.rs`: silencio PCM local antes y después de la frase, sin importar la tasa ni los canales; heurística de puntuación cuando no hay directiva (`…`/`...` → 400 ms, `.!?` → 250 ms, `;:` → 120 ms) y la primera frase sin pausa para no subir el tiempo hasta el primer audio [Responsable: Claude]
- [x] 46.3 Respiración: inhalación sintética (ruido filtrado en banda de 300 a 2 500 Hz, 320 ms, pico 0,035, envolvente suave y determinista) más 70 ms de hueco antes de la voz [Responsable: Claude]
- [x] 46.4 `EdgeTtsEngine::synthesize_directed()` y `ConversationCoordinator::synthesize_directed()`: las pausas y la respiración también se aplican sobre el fallback de espeak y el doble de prueba [Responsable: Claude]
- [x] 46.5 Fix colateral: un `...` que llegaba partido se sintetizaba como frases sueltas `"."`; ahora un segmento sin palabras no se pronuncia y solo alarga la pausa [Responsable: Claude]
- [ ] 46.6 Agrupar 2 o 3 frases por request si comparten directiva (H-V3, continuidad de entonación) — no implementado [Responsable: DeepSeek]
- [ ] 46.7 🧪 Medir la varianza de F0 antes y después con Edge real, y hacer una prueba A/B de oído (que no suene caricaturesco) [Responsable: PO]

**QA HU-45/46 2026-10-27 (Claude):** `make test` → 705 passed / 0 failed / 3 ignored · clippy `--workspace --all-targets -D warnings` OK · `make fmt-check` OK. Clips de respiración pregrabados reemplazados por síntesis procedural, porque el repo no tiene grabaciones.

---

## ✅ DONE / COMPLETADAS

| HU | Versión | Tests | Autoría | Pendiente |
|----|---------|-------|---------|-----------|
| Fallback Desacoplado Tool `dev run` | v0.2.27 | 632 passed / 0 failed · clippy OK · fmt OK · deny OK | DeepSeek (7.7.1–7.7.3) · Claude (QA, correcciones main.rs, fmt) · Antigravity (PO / SDD) | 🧪 Prueba con usuario por voz |
| Issue #7 Tareas Asíncronas CLI | v0.2.26 | 628 passed / 0 failed · clippy OK · fmt OK · deny OK | DeepSeek (7.1–7.5: core, tasks, orchestrator, settings) · Claude (QA, C-1 a C-7, tests UTF-8) · Antigravity (PO / SDD) | 🧪 Prueba con usuario por voz |
| Fix Thinking Barge-In | v0.2.21 | 516 passed / 0 failed · clippy OK · fmt OK | DeepSeek Dev · Antigravity PO | — |
| HU-AUDIO-CONTINUITY | v0.2.20 | 515 passed / 0 failed · clippy OK · fmt OK | DeepSeek (histéresis VAD, concatenación Thinking, merge HUD) · Antigravity PO | 🧪 Prueba con usuario en vivo |
| HU-36 Speech Popups HUD | v0.2.18 | `make test` 512 passed / 0 failed · `make lint` OK · `make fmt-check` OK | DeepSeek (36.1–36.6: config, `hud` render, `TextRasterizer`) · Claude (36.7–36.14: `HudEvent`, `HudLayer`, `HudPipeline`, toggle, QA) | 🧪 36.15 prueba manual en KWin (PO) |
| HU-37 Tareas pesadas en segundo plano | v0.2.17 | 474 passed / 0 failed | DeepSeek · Claude | 37.10b, 🧪 37.16, 37.17 |
| HU-35 Pantalla completa Auto-Dim / Auto-Hide | v0.2.16 | 440 passed | DeepSeek · Claude | — |

---

## ✅ Fix Thinking Barge-In — Protección de inferencias largas y eliminación de falsos barge-ins · v0.2.21

- `is_sounding()` restringido exclusivamente a `Speaking`: durante `Thinking` Jarvis no emite audio, eliminando candidatos de barge-in por ruido ambiental en silencios de razonamiento.
- Preservación íntegra de contexto: el LLM/CLI puede pensar sin que micro-sonidos o respiraciones aborten la consulta ni disparen alucinaciones de seguimiento.
- Test unitario `ambient_noise_during_thinking_does_not_abort_reasoning` en `orchestrator/tests.rs`.

**QA Fix Thinking Barge-In 2026-10-16:** `make test` → 516 passed / 0 failed · clippy OK · fmt OK.

---

## ✅ HU-AUDIO-CONTINUITY — Continuidad de audio y conversación · v0.2.20

- [x] Histéresis VAD de dos niveles (open 0.45, hold 0.25), silencio extendido 1200 ms / conector 2000 ms.
- [x] `PendingUserTurn` y concatenación de continuaciones durante estado `Thinking`.
- [x] Fusión y renovación de TTL de globos de usuario en HUD sin duplicación (`BubbleStack::push_user`).
- [x] Tests unitarios en `jarvis-audio` y `jarvis-render` (515 passing).

**QA HU-AUDIO-CONTINUITY 2026-10-16 (Claude):** `make test` → 515 passed / 0 failed · clippy OK · fmt OK. Pendiente 🧪 prueba con usuario en vivo.

---

## ✅ HU-36 — Mensajes efímeros flotantes en burbujas transparentes (Speech Popups HUD) · v0.2.18

### jarvis-core
- [x] 36.1 Campos en `JarvisConfig`: `hud_enabled` (bool, default `true`), `hud_max_bubbles` (1..=3, default 3) con getters y builders [Responsable: DeepSeek]
- [x] 36.2 Tests unitarios de configuración y roundtrip TOML para HUD [Responsable: DeepSeek]

### jarvis-render
- [x] 36.3 `fontdue` en `Cargo.toml` y fuente Noto Sans embebida en `crates/jarvis-render/assets/` [Responsable: DeepSeek]
- [x] 36.4 Módulo `hud`: `Speaker`, `Bubble`, `BubbleStack` puro con word-wrap, `bubble_ttl(chars) -> Duration` y animación [Responsable: DeepSeek]
- [x] 36.5 `TextRasterizer` con caché de glifos y `HudRenderer::render_into` con rectángulos redondeados y elipsis [Responsable: DeepSeek]
- [x] 36.6 Tests unitarios de `BubbleStack`, word wrap y TTL [Responsable: DeepSeek]

### jarvis-audio
- [x] 36.7 Enum `HudEvent` (`UserSaid`, `JarvisPhrase`, `JarvisDone`, `JarvisInterrupted`, `Notice`, `Clear`) en `events.rs` y `hud: Option<Sender<HudEvent>>` en `VoiceDeps` (`try_send`, nunca bloquea) [Responsable: Claude]
- [x] 36.8 Emisión en `BrainLoop`: `UserSaid` en `on_turn` (sin vocativo), `JarvisPhrase` desde el hilo TTS al entrar la frase a sonar (descarta generaciones obsoletas), `JarvisDone`/`JarvisInterrupted` al cerrar la locución, `Notice` para IPC y cola de tareas [Responsable: Claude]
- [x] 36.9 Tests de emisión en `orchestrator/tests.rs` (orden del turno, comando local, barge-in, aviso IPC, canal lleno) [Responsable: Claude]

### jarvis-shell
- [x] 36.10 Segunda capa `HudLayer` en `WaylandLayerSurface` con región de entrada vacía (click-through total), creada/destruida bajo demanda, colocada encima/debajo de la esfera según anclaje y arrastre (`hud_layout`, con tests) [Responsable: Claude]
- [x] 36.11 Trait `HudSurface`, `OverlaySurface::{set_hud_enabled, hud}` y soporte en `HeadlessSurface` (`HeadlessHud`) [Responsable: Claude]

### jarvis-app
- [x] 36.12 `HudPipeline` (`src/hud.rs`) en `run_loop`: drena `HudEvent`, mide la altura, presenta solo con cambios o fundidos y oculta la capa sin globos; sin globos nuevos en silencio o con pantalla completa fuera de turno (CA-36.7) [Responsable: Claude]
- [x] 36.13 Toggle "Mostrar burbujas de conversación (HUD)" en Ajustes → General (`unified_settings.py`), persiste `hud_enabled` [Responsable: Claude]
- [x] 36.14 Verificación completa de QA `make test`, `make lint` y `make fmt-check` [Responsable: Claude]
- [ ] 36.15 🧪 Prueba manual en KWin: conversación de 5 turnos, clic sobre un globo atraviesa, legibilidad sobre fondo blanco y negro, toggle en Ajustes [Responsable: PO]

**QA HU-36 2026-10-16 (Claude):** `make test` → 512 passed / 0 failed (audio 246, ipc 67, core 64, app 51, shell 48, render 30, no_alloc 6) · `make lint` OK · `make fmt-check` OK. Fix en `BubbleStack::visible`: incluía solo globos con alfa > 0 y descartaba los recién nacidos.


---

## HU-37 — Tareas pesadas en segundo plano con notificación vocal secuencial vía IPC

### jarvis-core
- [x] 37.1 `paths::state_dir()` (`$XDG_STATE_HOME/fokkus-jarvis`, default `~/.local/state/fokkus-jarvis`) y subdirectorio `tasks/` [Responsable: DeepSeek]
- [x] 37.2 Campos en `JarvisConfig`: `tasks_max_parallel` (1..=4, default 2), `tasks_timeout_secs` (60..=3600, default 900), `tasks_quiet_gap_ms` (500..=5000, default 1500) [Responsable: DeepSeek]
- [x] 37.3 Tests de config y paths para state_dir [Responsable: DeepSeek]

### jarvis-audio
- [x] 37.4 `classify_workload(query: &str) -> Workload` (heurística pesada vs conversacional) [Responsable: DeepSeek]
- [x] 37.5 `SpeechPriority`, `Delivery`, `DeliveryGate` y `SpeechQueue` con `BinaryHeap` (prioridad > FIFO, tope 32) [Responsable: DeepSeek]
- [x] 37.6 `TaskRecord`, `TaskStatus`, `TaskOrigin`, `BrainFactory` trait y `TaskManager` con pool desacoplado [Responsable: DeepSeek]
- [x] 37.7 Integración en `BrainLoop`: acuse rápido (< 1s) para tareas pesadas, despacho a worker independiente sin bloqueo, tick de 250ms para drenar `SpeechQueue` con `DeliveryGate` [Responsable: DeepSeek · cierre: Claude]
- [x] 37.8 Comandos de voz locales: cancelación ("cancela la tarea de los correos", "cancela todas las tareas"), estado y pendientes [Responsable: DeepSeek · cierre: Claude]
- [x] 37.9 Tests unitarios de `SpeechQueue`, `classify_workload` y `TaskManager` [Responsable: DeepSeek]

### jarvis-ipc
- [x] 37.10 Métodos JSON-RPC: `jarvis.dispatchTask`, `jarvis.getTasks`, `jarvis.getTask`, `jarvis.cancelTask` [Responsable: Claude]
- [ ] 37.10b Notificación servidor→cliente `jarvis.taskUpdated` (requiere canal de push por conexión en `IpcServer`) [Responsable: pendiente]
- [x] 37.11 Soporte de cola en `jarvis.speak`: params `queue`, `priority` y respuesta `queued`, `position` [Responsable: Claude]
- [x] 37.12 Implementación del trait `TaskPort` sobre `ServerState` [Responsable: Claude]

### jarvis-app
- [x] 37.13 Implementación de `BrainFactory` en `jarvis-app` para generar instancias limpias e independientes de CLI/Claude [Responsable: Claude]
- [x] 37.14 Cableado de `TaskManager` en `main.rs` y pasarela hacia `BrainLoop` y servidor IPC [Responsable: Claude]
- [x] 37.15 Tests de integración completos y suite de QA `cargo test --workspace` [Responsable: Claude]
- [ ] 37.16 🧪 Prueba manual: "Jarvis, revisa mis correos de hoy y dime si hay algo urgente" con `agy`/`claude` real, conversar mientras corre y verificar entrega + archivo en `~/.local/state/fokkus-jarvis/tasks/` [Responsable: PO]
- [ ] 37.17 Timeout por tarea en modo API (`ClaudeClient` no tiene timeout total; hoy solo lo aplica `CliBrain`) y política "máx. 60 s de espera del gate → siguiente Idle" (CA-37.5) [Responsable: pendiente]

**QA 2026-10-16 (Claude):** `make test` → 474 passed / 0 failed (audio 241, ipc 67, core 61, app 44, shell 42, render 13, no_alloc 6) · `make lint` (cargo-deny + clippy `-D warnings`) OK · `make fmt-check` OK.


---

## HU-35 — Detección universal de pantalla completa y modo ventana sin bordes (Auto-Dim / Auto-Hide)

### jarvis-core
- [x] 35.1 `FullscreenBehavior` enum (`Ignore`, `Dim`, `Hide`) con serde lowercase y default `Dim` [Responsable: DeepSeek]
- [x] 35.2 `Rect` struct (`x`, `y`, `w`, `h`) y tipos `WindowSnapshot`, `OutputGeometry` [Responsable: DeepSeek]
- [x] 35.3 Función pura `is_effectively_fullscreen(&WindowSnapshot, &OutputGeometry) -> bool` (fullscreen o borderless >= 98% con +/-2px) [Responsable: DeepSeek]
- [x] 35.4 Campos en `JarvisConfig`: `fullscreen_behavior`, `fullscreen_dim_pct` (5..=100, default 25), `fullscreen_active_pct` (5..=100, default 60) [Responsable: DeepSeek]
- [x] 35.5 Tests unitarios exhaustivos para `is_effectively_fullscreen` y config (55 tests pasando ✅) [Responsable: DeepSeek]

### jarvis-shell
- [x] 35.6 Módulo `window_watch` con `WindowEvent`, `WatcherKind` y trait `WindowWatcher` [Responsable: Claude]
- [x] 35.7 Script de KWin `assets/kwin_fullscreen_watch.js` con escucha de eventos de ventana y reporte DBus [Responsable: Claude]
- [x] 35.8 `KwinScriptWatcher`: servicio DBus `io.fokkus.Jarvis.WindowWatch`, carga/descarga con `org.kde.kwin.Scripting` y emisión a crossbeam channel [Responsable: Claude]
- [x] 35.9 `NullWatcher` y fábrica `detect_watcher(jarvis_pid: u32, behavior: FullscreenBehavior)` [Responsable: Claude]
- [x] 35.10 Tests unitarios e integración para `window_watch` [Responsable: Claude]

### jarvis-app
- [x] 35.11 `PresenceDirective` (`Normal`, `Dimmed(Opacity)`, `Hidden`) y máquina de estados `FullscreenPolicy` con histéresis 300 ms y override vocal [Responsable: Claude]
- [x] 35.12 Integración en `run_loop.rs`: consumo de eventos de ventana, modulación de `auto_factor` de HU-34 / visibilidad de superficie [Responsable: Claude]
- [x] 35.13 Selector "Pantalla completa" en Ajustes Generales (`unified_settings.py`) [Responsable: Claude]
- [x] 35.14 Verificación integral `cargo test --workspace`, clippy y formato [Responsable: Claude]


---

## HU-34 — Opacidad y transparencia variable de la esfera

### jarvis-core
- [x] 34.1 `Opacity` (newtype `u8`): `OPAQUE`, `from_pct` (piso 20 %), `as_u8`, `mul`, `is_opaque` [Responsable: DeepSeek]
- [x] 34.2 `sphere_opacity_pct` en `JarvisConfig` (default 100, saturado al cargar) + `sphere_opacity()` [Responsable: DeepSeek]
- [x] 34.3 Fix de visibilidad: `Opacity::from_raw(u8)` (`const fn`) para fundidos y atenuación automática [Responsable: Claude]
- [x] 34.4 Getter `sphere_opacity_pct()` saturado (log de `apply_config`) [Responsable: Claude]

### jarvis-render
- [x] 34.5 `apply_opacity(&mut Pixmap, Opacity)` con `(c*a+127)/255` y atajo para opaco [Responsable: DeepSeek]
- [x] 34.6 `OpacityFade` (`new`, `retarget`, `value_at`, `is_settled`) [Responsable: DeepSeek]
- [x] 34.7 Migración a `Opacity::from_raw` (el campo ya no se construye desde fuera de `jarvis-core`) [Responsable: Claude]

### jarvis-shell
- [x] 34.8 `rgba_premul_to_argb8888_with_alpha(src, dst, opacity)`; `rgba_premul_to_argb8888` delega con `Opacity::OPAQUE` [Responsable: Claude]
- [x] 34.9 `OverlaySurface::set_opacity` (default no-op) [Responsable: Claude]
- [x] 34.10 `WaylandLayerSurface::set_opacity`: la opacidad se aplica en el pase de conversión a `wl_shm` (un solo recorrido) [Responsable: Claude]
- [x] 34.11 `HeadlessSurface::set_opacity` + getter `opacity()` para tests [Responsable: Claude]
- [x] 34.12 Tests: equivalencia con la ruta opaca, blanco 50 % → `(128,128,128,128)`, invariante premultiplicado en 10⁴ píxeles [Responsable: Claude]

### jarvis-app
- [x] 34.13 Slider "Opacidad" 20–100 % (paso 5) en General → Esfera [Responsable: Claude]
- [x] 34.14 Vista previa `{"preview":{"sphere_opacity_pct":Y}}` ≤ 30 Hz, aplicada con fundido de 150 ms en `run_loop` [Responsable: Claude]
- [x] 34.15 Composición con la atenuación automática de HU-35 (`user × auto`) — depende de HU-35 [Responsable: DeepSeek]
- [ ] 🧪 34.16 Prueba manual: esfera al 30 % sobre un editor, colores de estado distinguibles [Responsable: Claude]
- [ ] 34.17 Benchmark de sobrecoste (≤ 0.3 ms a 220 px, ≤ 1.2 ms a 440 px) [Responsable: DeepSeek]

---

## HU-33 — Tamaño dinámico de la burbuja en tiempo real

### jarvis-core
- [x] 33.1 `DEFAULT_SPHERE_SIZE_PX`, `SPHERE_SCALE_RANGE`, `sphere_scale_pct` (default 100, saturado al cargar) y `sphere_size_px()` [Responsable: DeepSeek]
- [x] 33.2 `sphere_size_for_pct(pct)` compartido por config y vista previa [Responsable: Claude]

### jarvis-render
- [x] 33.3 `SphereRenderer::resize(size)` en O(1) [Responsable: DeepSeek]

### jarvis-shell
- [x] 33.4 `OverlaySurface::set_size` (default `Err(BackendUnavailable("resize"))`) [Responsable: Claude]
- [x] 33.5 `WaylandLayerSurface::set_size`: `layer.set_size` + commit, espera del `configure` (timeout 250 ms con log único), reutiliza/crece el `SlotPool`, recalcula región de entrada circular y re-satura los márgenes de drag; si el compositor no responde, restaura el tamaño anterior [Responsable: Claude]
- [x] 33.6 `HeadlessSurface::set_size` (revalida los frames contra el tamaño nuevo) [Responsable: Claude]
- [x] 33.7 Tests: `circle_strips` a 110/440 px y márgenes re-saturados tras crecer [Responsable: Claude]

### jarvis-app
- [x] 33.8 Slider "Tamaño de la esfera" 50–200 % (paso 5) en General → Esfera, valor cargado del TOML y botón "Guardar" [Responsable: Claude]
- [x] 33.9 Emisión `{"preview":{"sphere_scale_pct":X}}` limitada a 30 Hz (`PreviewThrottle` con `QTimer`, coalescencia y flush al soltar) [Responsable: Claude]
- [x] 33.10 `UnifiedSettings` con `stdout` piped + hilo `jarvis-settings-preview` → `bounded(16)` descartando el más viejo; `parse_preview_line` y `poll_preview` [Responsable: Claude]
- [x] 33.11 `SphereView` (renderer + pixmap, reasigna solo al cambiar) y `SphereLook` en `run_loop`: como máximo un cambio de tamaño por frame [Responsable: Claude]
- [x] 33.12 Arranque con el tamaño/opacidad persistidos; `Saved` relee el TOML, `Cancelled`/proceso muerto revierte al persistido [Responsable: Claude]
- [x] 33.13 Tests: parseo (válido, fuera de rango, basura), lector con canal lleno, `SphereView::resize`, bucle con último valor por frame y reversión [Responsable: Claude]
- [ ] 33.14 Cerrar el `xdg_popup` del menú antes del resize y re-posicionar el HUD (HU-36) [Responsable: DeepSeek]
- [ ] 33.15 HiDPI: buffer en píxeles físicos con `buffer_scale` (CA-33.7) [Responsable: DeepSeek]
- [ ] 33.16 Test de no-asignación del bucle (1 000 frames sin resize → 0 asignaciones) [Responsable: DeepSeek]
- [ ] 🧪 33.17 Prueba manual en KWin: slider 50→200→50 sin parpadeo, clic en esquinas *click-through*, `--diag` a 200 % [Responsable: Claude]

---

## Verificación (2026-10-16)
- `cargo test --workspace`: ✅ 440 passed, 0 failed (2 ignored preexistentes)
- `cargo clippy --workspace --all-targets -- -D warnings`: ✅ limpio
- `cargo fmt --all -- --check`: ✅ limpio
- `cargo deny check`: ✅ limpio (advisories ok, bans ok, licenses ok, sources ok)
- `unified_settings.py` en Qt offscreen: selector "Pantalla completa" (`dim`, `hide`, `ignore`); ráfaga de 151 cambios → 2 líneas JSON; valores saturados y alineados al paso; TOML guardado con `sphere_scale_pct`/`sphere_opacity_pct`/`fullscreen_behavior`
