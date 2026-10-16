# Brief técnico para el Dev Senior (DeepSeek V4 Pro): orquestador de voz de punta a punta

**Contrato:** [`sdd-conversacion-nativa-v0.2.0.md`](sdd-conversacion-nativa-v0.2.0.md) (aprobado por el PO). Este brief cubre el cableado de **HU-1, HU-4 (mute), HU-5, HU-6 y HU-9** dentro del binario.
**Redactado por:** Claude Code (PM/QA) · **Fecha:** 2026-10-15
**Iteración:** local, sobre `main` (v0.2.1). **Prohibido** crear tags, releases, commits o push: el PO integra y decide.
**Invocación:** `python3 ~/.vibe/deepseek_task.py "<este brief>" --cwd /home/juancarlos/Proyectos/Personales/Fokkus-Jarvis --model pro`

---

## Contexto de negocio

Juan Carlos instaló la v0.2.1 y la probó en su equipo: la esfera aparece y se puede arrastrar, pero **Jarvis no escucha ni habla**. Le habla al micrófono y no pasa nada. El problema no es de una pieza rota: todas las piezas de audio existen en `jarvis-audio` y tienen tests (captura, DSP con cancelación de eco, VAD, Whisper, cliente de Claude, TTS, barge-in), pero **nadie las conecta**. El binario `jarvis-app` nunca abre el micrófono ni los altavoces; `start_listening()` solo cambia el color de una máquina de estados.

Lo que el usuario quiere: decir "Jarvis, ¿qué hora es?", oír un "déjame ver…" casi inmediato, luego la respuesta de Claude con la voz Lorenzo, y poder cortarlo hablando encima. Y si lo silencia desde el menú de la esfera (clic derecho → *Silenciar*, ya implementado por PM/QA en esta iteración), el micrófono debe soltarse de verdad.

El PO exige **funcionalidad completa, no MVP** (contrato §0): sin *stubs*, `todo!()`, `unimplemented!()`, `// TODO` en rutas de producción, ni logs que anuncien algo que el programa no verificó.

---

## Inventario verificado por PM/QA (2026-10-15, `main` @ 5cfe613 + cambios locales del menú)

| Pieza | Dónde | Estado |
|---|---|---|
| Selección y negociación de dispositivos | `jarvis-audio/src/io/device.rs` (`select_*_device`, `negotiate_*`) | ✅ existe, usada solo por `--check` |
| Lógica de callbacks sin alloc | `io/callbacks.rs` (`process_input_frame`, `process_output_frame`, `Fader`, `compute_rms`) | ✅ testeada (`tests/no_alloc.rs`) — **ningún `build_input_stream`/`build_output_stream` en el repo** |
| Métricas atómicas | `io/metrics.rs` (`AudioMetrics`) | ✅ |
| Hilo DSP + VAD + endpointing | `dsp/thread.rs` (`spawn_dsp_thread` → `VoiceEvent` por `crossbeam-channel`) | ✅ existe, **nunca se invoca** |
| Latencias para AEC | `dsp/delay.rs` (`LatencyProbe`) | ✅ |
| Activación "Jarvis" + ventana de seguimiento + mute | `activation.rs` (`ActivationDetector::process_segment`) | ✅ |
| STT | `stt.rs` (`WhisperEngine::from_verified`) + `models.rs` (`ModelManager::ensure`) | ✅ modelo `ggml-base-q5_1` verificado en disco en el equipo del usuario |
| Cerebro | `brain/` (`ClaudeClient`, `CancellationToken`, `resolve_api_key`) | ✅ — **el equipo del usuario NO tiene API key aún** (log: `[cerebro] sin API key`) |
| Turno completo + barge-in | `conversation.rs` (`handle_user_utterance`, `run_turn_streaming`, `handle_barge_in`, `finish_speech`) | ✅ testeado con dobles |
| TTS nativo | `tts/` (`synthesize_phrase_native` → `PcmAudio`, fillers en caché) | ✅ |
| Estado que dibuja la esfera | `jarvis-ipc` `ServerState::state()` | ⚠️ **núcleo distinto** al de `ConversationCoordinator` (deuda "Unificar `JarvisCore`" del backlog) |
| Mute desde el menú de la esfera | `jarvis-app/src/main.rs` (`on_command` → `ServerState::set_muted`) | ✅ nuevo — hoy solo cambia el estado; **no toca el micrófono** |

---

## División de trabajo del Swarm

| Rol | Entrega en esta iteración |
|---|---|
| **Claude Code (PM/QA)** | ✅ Menú contextual (clic derecho) — hecho. Plan de QA y validación de cada HU de este brief (`make test`, `make lint`, `make fmt-check`, smoke en KWin, protocolo §9 del SDD). |
| **DeepSeek V4 Pro (Dev Senior)** | HU-ORQ-1 a HU-ORQ-5 de este brief: streams `cpal`, hilos y orquestación. |
| **Ollama (Dev Junior)** | Actualizar `backlog.md` y README (sección "Uso": menú contextual y flujo de voz) cuando PM/QA apruebe. |
| **Antigravity (PO)** | Integración, decisión de commit (sin tag) y prueba en el equipo real. |

---

## Reglas rígidas (aplican a todas las HU)

1. **Rust 2021 del workspace, sin dependencias nuevas.** Todo lo necesario ya está aprobado en `Cargo.toml` (`cpal`, `ringbuf`, `crossbeam-channel`, `rubato`, `whisper-rs`, `ureq`, …). Si crees que falta una, **detente y repórtalo**; no la agregues (`make lint` corre `cargo deny`).
2. **Callbacks de `cpal`:** sin locks, sin allocations, sin I/O, sin `println!`. Solo las funciones de `io/callbacks.rs` + atómicos + `ringbuf`. Extiende `tests/no_alloc.rs` si agregas lógica nueva al callback (CA-1.3).
3. **El bucle de render (`runtime::run_loop`) no puede bloquearse nunca.** Nada de STT, HTTP ni síntesis en el hilo principal. El render lee estado y nivel con operaciones O(1).
4. **Un único `JarvisCore`** como fuente de verdad del estado (SDD §5, último párrafo). La esfera, el IPC y el coordinador deben ver el mismo estado.
5. **Hilos con nombre** (`jarvis-audio-*`, `jarvis-stt`, `jarvis-brain`, `jarvis-tts`) y apagado ordenado: SIGTERM o "Salir" en el menú cierran todo en < 500 ms (el test `loop_stops_on_shutdown_flag_within_500ms` ya fija ese criterio para el bucle).
6. **Degradación honesta:** sin micrófono, sin modelo o sin API key, el proceso sigue vivo, lo informa una vez en el log (y por voz cuando haya salida de audio, HU-6) y la esfera no miente sobre su estado.
7. **Privacidad (HU-10):** prohibido escribir audio a disco o loguear transcripciones/respuestas en el nivel por defecto.
8. **Build:** antes de `make` ejecutar `. ~/.cache/fokkus-src/env.sh` (no hay `alsa-lib-devel` ni `clang` del sistema). Usar siempre los targets del `Makefile`: `make test`, `make lint`, `make fmt-check`.
9. **Git:** sin commits, tags ni push.

---

## Historias de usuario

### HU-ORQ-1 — Motor de streams de audio (captura y reproducción reales)
- 📁 **Repositorio:** Fokkus-Jarvis — `/home/juancarlos/Proyectos/Personales/Fokkus-Jarvis`
- 🎯 **Capa a trabajar:** Backend — `crates/jarvis-audio/src/io/` (nuevo componente de motor) y su export en `lib.rs`.
- 📌 **Objetivo técnico:** Construir el componente que abre los streams `cpal` de entrada y salida con los dispositivos configurados, los conecta a los ring buffers que consume `spawn_dsp_thread` (captura + referencia de AEC) y a un ring buffer de reproducción que alimentará el TTS, publica latencias en `LatencyProbe`, el RMS real (para la esfera) y las métricas atómicas. Debe poder **cerrar y reabrir la captura** (mute real, CA-4.3) y **recuperarse solo** si el dispositivo desaparece (CA-1.2). Decide tú el dimensionado de los ring buffers y cómo se detecta la desconexión; justifícalo en el doc del módulo.
- ✅ **CA:** CA-1.1, CA-1.2, CA-1.3, CA-4.3 del SDD. Test de que abrir/cerrar captura no filtra hilos ni streams.

### HU-ORQ-2 — Pipeline DSP vivo
- 📁 **Repositorio:** Fokkus-Jarvis
- 🎯 **Capa a trabajar:** Backend — `jarvis-audio` (motor) + orquestador.
- 📌 **Objetivo técnico:** Lanzar `spawn_dsp_thread` con las tasas negociadas reales y `EndpointConfig` desde la configuración, recibir `VoiceEvent` por `crossbeam-channel` y garantizar que el hilo DSP sobrevive a una reconexión del dispositivo (o se relanza de forma ordenada). Resolver cómo el STT avisa conectores (`DspHandle::signal_connector`, HU-3 adaptativo).
- ✅ **CA:** CA-2.1 (sin auto-interrupciones con altavoces), CA-3.2. Métricas de `DspMetrics` visibles en `--check` o en el log de diagnóstico.

### HU-ORQ-3 — Orquestador conversacional (VoiceEvent → STT → Claude → TTS)
- 📁 **Repositorio:** Fokkus-Jarvis
- 🎯 **Capa a trabajar:** Backend — nuevo módulo orquestador (ubicación a tu criterio: `jarvis-audio` si es reutilizable, `jarvis-app/src/` si es solo cableado; justifícalo).
- 📌 **Objetivo técnico:** Consumir los `VoiceEvent`:
  - `VoiceStart` durante `Speaking` → `ConversationCoordinator::handle_barge_in` (corte ≤ 50 ms p95, CA-9.1) y cancelación del LLM/TTS en curso.
  - `EndOfTurn` → `ActivationDetector::process_segment` (wake word o ventana de seguimiento) → transcripción con un `WhisperEngine` cargado **una sola vez** → `handle_user_utterance` (filler de caché sonando ≤ 300 ms, CA-9.2) → `run_turn_streaming` con el `ClaudeClient` → cada frase PCM remuestreada a la tasa de salida y encolada en el ring de reproducción → `finish_speech` y apertura de la ventana de seguimiento.
  - Diseña el modelo de hilos (SDD §5: STT, LLM, TTS separados del DSP) y cómo se cancela todo lo pendiente ante un barge-in sin carreras sobre `utterance_generation`.
- ✅ **CA:** CA-4.2, CA-6.4, CA-8.3, CA-9.1, CA-9.2, CA-9.3. Test de integración con dobles (`Transcriber`, `TokenStream`, síntesis) que recorra: wake word → respuesta → barge-in → nueva pregunta en ventana de seguimiento.

### HU-ORQ-4 — Un solo `JarvisCore` y mute de punta a punta
- 📁 **Repositorio:** Fokkus-Jarvis
- 🎯 **Capa a trabajar:** Backend — `jarvis-core`, `jarvis-ipc` (`ServerState`), `jarvis-audio` (`ConversationCoordinator`), `jarvis-app`.
- 📌 **Objetivo técnico:** Eliminar la duplicación de núcleos para que la esfera, el IPC (`jarvis.getState`, `jarvis.setMuted`) y el coordinador compartan estado. El mute pedido por **cualquier vía** (menú contextual de la esfera — ver `ShellCommand::ToggleMute` en `jarvis-app/src/main.rs` —, IPC o voz "Jarvis, deja de escuchar") debe: cambiar el estado a `Muted`, mutear el `ActivationDetector` y **liberar el dispositivo de captura**; al desmutear, reabrirlo y volver a `Listening`. El nivel de audio de la esfera debe ser el RMS real del micrófono o de la salida.
- ✅ **CA:** CA-4.3, CA-9.3, CA-9.4 (una interrupción no cancela tareas del swarm). Los tests existentes de `jarvis-ipc` (`mute_blocks_speak_and_unmute_restores_it`, etc.) siguen en verde.

### HU-ORQ-5 — Integración en `jarvis-app` y degradaciones
- 📁 **Repositorio:** Fokkus-Jarvis
- 🎯 **Capa a trabajar:** Backend — `crates/jarvis-app/src/main.rs` y `runtime.rs`.
- 📌 **Objetivo técnico:** Arrancar el orquestador después de la superficie sin retrasar el primer frame; reemplazar el `start_listening_if_ready` actual (que solo cambia estado) por arranque real de escucha; conectar la descarga del modelo en segundo plano (`poll_model_ready`) con la carga del `WhisperEngine`; apagado ordenado (orquestador → streams → DSP → coordinador → IPC → lock). Sin API key: Jarvis lo dice en voz alta una vez (espeak/Piper si no hay red) y sigue respondiendo comandos locales (mute, hora). `--check` debe probar abrir los streams de verdad (HU-11) sin dejar el micrófono tomado.
- ✅ **CA:** CA-6.4, CA-11.2; `make test`, `make lint` y `make fmt-check` en verde; log de arranque honesto (cada subsistema reporta lo que **verificó**, no lo que intentó).

---

## Entregables esperados del Dev Senior

1. Código aplicado en disco (no solo en la respuesta), con tests unitarios e integración de cada HU.
2. Resumen por HU: archivos tocados, decisiones de diseño (ring buffers, hilos, cancelación) y CA que **no** se pudieron verificar sin hardware.
3. Salida de `make test`, `make lint` y `make fmt-check`.

PM/QA validará luego en el equipo real (Fedora 44, KWin, PipeWire, EasyEffects) con el protocolo §9 del SDD.
