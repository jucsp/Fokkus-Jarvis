# Product Backlog — Fokkus-Jarvis

Este documento contiene el seguimiento de funcionalidades, mejoras y arquitectura para el asistente de escritorio autónomo **Fokkus-Jarvis**.

Repositorio oficial: [jucsp/Fokkus-Jarvis](https://github.com/jucsp/Fokkus-Jarvis)  
Especificación SDD: [docs/specs/sdd-fokkus-jarvis.md](docs/specs/sdd-fokkus-jarvis.md)

---

## 🚀 Épica 1: Núcleo Nativo y Ventana Transparente Multiplataforma (Rust Nativo)
- [x] [#1](https://github.com/jucsp/Fokkus-Jarvis/issues/1) **feat(core): setup inicial de Fokkus Jarvis en Rust nativo (layer-shell Wayland / winit / tiny-skia)**
  - _Alcance:_ Configuración de workspace en Rust nativo. Ventana frameless nativa sin decoraciones del SO, capa overlay real en Wayland (`zwlr_layer_shell_v1`) en KWin/Caelestia para soporte Always-On-Top y visible en todos los escritorios. Fallback winit para X11/Windows. Renderizado CPU ultraligero con `tiny-skia` para la esfera líquida (sin drivers GPU pesados). Consumo en reposo $\le 25-40$ MB RAM. Estructura multi-crate y Makefile.
  - _Estado QA (2026-10-14):_ ⚠️ entrega parcial, issue abierto — ver [reporte QA](docs/specs/qa-issue1-fase1.md).
    - [x] 1.1 Workspace Cargo, toolchain y Makefile
    - [x] 1.2 Máquina de estados y bus de eventos (tabla 100 % cubierta)
    - [x] 1.3 Esfera líquida — animación temporal completa (render 1.66 ms/frame ✅)
    - [ ] 1.0 / 1.4 Ventana overlay `zwlr_layer_shell_v1` en KWin
    - [ ] 1.5 Fallback winit + detección real de layer-shell
    - [x] 1.6 Instancia única (lock TOML) y config TOML persistente
    - [x] 1.7 `cargo-deny` y snapshot de pixmap (`fmt` + `clippy` limpios ✅ — 27 tests core + 9 tests render)
    - [ ] 1.8 Medición RSS ≤ 40 MB y CPU en reposo

---

## 🎙️ Épica 2: Motores de Audio de Alta Fidelidad y Voz Natural
- [x] [#2](https://github.com/jucsp/Fokkus-Jarvis/issues/2) **feat(audio): pipeline de audio, catálogo de 15 voces neuronales (default es-CL-LorenzoNeural), Edge-TTS con fallback espeak-ng, caché de fillers en ~/.cache/fokkus-jarvis/fillers/, playback con stop atómico y Whisper STT** (47 tests pasando ✅)
  - _Alcance:_ Captura de audio continuo con `cpal` (16 kHz mono) y buffer deslizante de 300 ms de audio previo. Transcripción local rápida con Whisper y VAD Silero. Síntesis neuronal de alta fidelidad con `edge-tts` (15 voces neuronales, `es-CL-LorenzoNeural` por defecto) y fallback offline instantáneo a `espeak-ng`. Pre-síntesis y almacenamiento en caché de frases de relleno conversacional (*fillers*) para latencia de inicio $< 300$ ms. Reproducción con detención inmediata (*fade-out* 15 ms).

---

## 🗣️ Épica 3: Experiencia Conversacional Humana (Sin Silencios e Interrumpible)
- [x] [#3](https://github.com/jucsp/Fokkus-Jarvis/issues/3) **feat(conversation): conversación fluida con respuestas de relleno natural (Conversational Fillers < 300 ms) vía ConversationCoordinator y classify_intent (Dev, Inbox, General)**
  - _Alcance:_ Emisión inmediata (< 300 ms) de frases de confirmación contextuales (*"Entendido, déjame revisar..."*, *"Dame un segundo..."*) al entrar en estado `thinking`, eliminando el silencio incómodo mientras el LLM genera la respuesta completa. Transición fluida y natural al flujo de la respuesta principal.
- [x] [#4](https://github.com/jucsp/Fokkus-Jarvis/issues/4) **feat(conversation): interrupción humana en tiempo real (Barge-In < 50 ms) vía AudioPlayback con preservación incondicional de procesos BackgroundTasks del Swarm**
  - _Alcance:_ Detección de voz en paralelo con cancelación de eco mientras Jarvis habla. Corte instantáneo de audio (< 50 ms) y transición inmediata a `listening` al hablar el usuario. Si hay tareas técnicas del Swarm en segundo plano (Claude/DeepSeek trabajando), la interrupción de voz NO mata el proceso a menos que el usuario lo ordene explícitamente.
- [x] **feat(audio): continuidad de audio y conversación (histéresis VAD de dos niveles, concatenación de turnos en `Thinking` y fusión de globos HUD)** (HU-AUDIO-CONTINUITY) ✅ Cerrada en v0.2.20
  - _Alcance:_ Histéresis VAD de dos niveles (open 0.45, hold 0.25) y silencio extendido a 1200 ms / conector a 2000 ms para no cortar frases largas ni pausas naturales. `PendingUserTurn` y concatenación de continuaciones durante estado `Thinking`, de modo que una pausa de 1–2 s no pierda el contexto de la pregunta. Fusión y renovación de TTL de los globos de usuario en el HUD sin duplicación (✅ 515 tests pasando).
- [x] **fix(audio): protección de inferencias largas y eliminación de falsos barge-ins en `Thinking`** (v0.2.21)
  - _Alcance:_ `is_sounding()` restringido exclusivamente a `Speaking`, de modo que el ruido ambiental durante el razonamiento silencioso del LLM/CLI no dispara candidatos de barge-in ni aborta la consulta en vuelo. Test `ambient_noise_during_thinking_does_not_abort_reasoning` (✅ 516 tests pasando).

---

## 🔌 Épica 4: Integración con Fokkus IDE y Supervisión Asíncrona del Swarm
- [x] [#5](https://github.com/jucsp/Fokkus-Jarvis/issues/5) **feat(ipc): servidor Unix Socket JSON-RPC 2.0 sobre ~/.fokkus/jarvis.sock y cliente Rust nativo con soporte de ciclo de vida Swarm y alertas de Triage** (63 tests pasando ✅)
  - _Alcance:_ Servidor JSON-RPC 2.0 sobre `~/.fokkus/jarvis.sock` (Linux/macOS) y `\\.\pipe\fokkus-jarvis` (Windows). Métodos bidireccionales: `jarvis.dispatchSwarm`, `jarvis.notifyTriageAlert`, `jarvis.notifySwarmCompleted`, `jarvis.getSwarmStatus`. Capacidad para consultar verbalmente el avance del equipo (*"Jarvis, ¿cómo va el desarrollo?"*).

---

## 🤖 Épica 5: Operador de Sistema y Capacidades Autónomas (Standalone)
- [ ] [#6](https://github.com/jucsp/Fokkus-Jarvis/issues/6) **feat(os-tools): orquestador de sistema y herramientas autónomas (Archivos, Terminal, Computer Use y conectores MCP)**
  - _Alcance:_ Orquestador de sistema cuando opera sin Fokkus IDE. Tool calling agnóstico (Gemini, Claude, DeepSeek, Ollama). Inspección de archivos en `~/Descargas` y documentos, ejecución de comandos CLI aprobados, visión multimodal de pantalla y conectores MCP (Google Drive, Calendar, Slack).

---

## 🎨 Épica 6: Experiencia Visual Adaptativa y Personalización de Interfaz
- [x] **feat(ui): ajuste dinámico de tamaño de la burbuja en tiempo real** (HU-33)
  - _Contexto de negocio:_ El usuario necesita redimensionar la presencia visual de Jarvis en su pantalla según su monitor o preferencia de trabajo, viendo el cambio en vivo sin reiniciar.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa a trabajar:_ `crates/jarvis-app` (Ajustes Generales PyQt6) y `crates/jarvis-shell` (Wayland layer-shell / winit resize).
  - 📌 _Objetivo técnico:_ Proveer un control deslizante (50% a 200%) en Ajustes Generales que emita eventos reactivos inmediatos para escalar dinámicamente el canvas de renderizado y el ancho/alto de la superficie flotante. Implementado con canal stdout piped a 30 Hz y redimensionado O(1) de capa y región de entrada (✅ 398 tests pasando).
- [x] **feat(ui): control de opacidad y transparencia variable de la esfera líquida** (HU-34)
  - _Contexto de negocio:_ Permitir que Jarvis sea más o menos translúcido para trabajar cómodamente sobre código o ventanas sin tapar información crítica.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa a trabajar:_ `crates/jarvis-render` y `crates/jarvis-app`.
  - 📌 _Objetivo técnico:_ Añadir parámetro de opacidad global en la configuración y modular el canal alfa durante la composición en `tiny-skia` con interpolador temporal lineal `OpacityFade` de 150 ms (✅ 398 tests pasando).
- [x] **feat(ui): detección universal de pantalla completa y modo ventana sin bordes (Auto-Dim / Auto-Hide)** (HU-35)
  - _Contexto de negocio:_ Si el usuario entra a pantalla completa (video en YouTube, juego en modo frameless o Steam), Jarvis debe detectarlo genéricamente para no entorpecer la vista.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa a trabajar:_ `crates/jarvis-shell` / integración de ventanas de SO.
  - 📌 _Objetivo técnico:_ Implementar un monitor agnóstico de estado de ventanas (KWin DBus / XDG toplevel / APIs nativas de OS) que active automáticamente atenuación de opacidad o minimizado temporal. Implementado con watcher D-Bus de KWin Scripting, máquina `FullscreenPolicy` con histéresis de 300 ms, override vocal temporal durante conversación y selector en Ajustes Generales (✅ 440 tests pasando).
- [x] **feat(ui): mensajes efímeros flotantes en burbujas transparentes (Speech Popups HUD)** (HU-36, v0.2.18)
  - _Contexto de negocio:_ Confirmación visual rápida de lo que el usuario dijo y lo que Jarvis responde, sin recurrir a una ventana pesada de chat permanente.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa a trabajar:_ `crates/jarvis-render` / `crates/jarvis-shell`.
  - 📌 _Objetivo técnico:_ Renderizar globos de texto translúcidos flotantes efímeros sobre la esfera (máximo 1-3 mensajes tipo HUD) que se desvanezcan tras unos segundos. Configurable (toggle On/Off) desde Ajustes Generales. Implementado con segunda capa Wayland click-through (`HudLayer`), rasterizado `fontdue` + Noto Sans embebida, `BubbleStack` con TTL dinámico y `HudEvent` sincronizado con los turnos de voz (✅ 512 tests pasando).

---

## 👁️ Épica 7: Agente Visual Multiplataforma y Automatización Generalista (Computer Use)
- [x] [#7](https://github.com/jucsp/Fokkus-Jarvis/issues/7) **fix(cli): soporte para tareas asíncronas en segundo plano y desacoplamiento del timeout síncrono de 120s** (Contrato: [sdd-issue7-tareas-asincronas-cli-timeout.md](docs/specs/sdd-contracts/sdd-issue7-tareas-asincronas-cli-timeout.md)) ✅ Cerrada en v0.2.26 / Fallback v0.2.27
  - _Contexto de negocio:_ Evitar que órdenes de desarrollo ("lánselo no más", "inicia el pipeline", orquestaciones con `agy`) bloqueen la conversación durante 120s y fallen por timeout rígido. Fallback desacoplado nativo con tool `fokkus-jarvis dev run <tarea>`.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa a trabajar:_ `crates/jarvis-core` (`cli_timeout_secs`), `crates/jarvis-audio` (`classify_workload`, `LIST_TASK_PHRASES`, `SYSTEM_PROMPT`), `crates/jarvis-app` (`fokkus-jarvis dev run`).
  - 📌 _Objetivo técnico:_ Clasificación asíncrona inmediata de tareas de desarrollo/swarm, despacho desacoplado a `TaskManager`, timeout CLI configurable en `config.toml`, tool nativa de fallback `dev run` y consulta verbal de estado de tareas en curso.
- [x] **feat(automation): tareas pesadas en segundo plano con notificación vocal secuencial vía IPC** (HU-37)
  - _Contexto de negocio:_ Las tareas de larga duración (revisión de correos, Slack, análisis de datos) no deben bloquear la voz ni ser canceladas accidentalmente si el usuario sigue conversando.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa a trabajar:_ `crates/jarvis-audio` (orquestador de tareas) y `crates/jarvis-ipc`.
  - 📌 _Objetivo técnico:_ Acuse de recibo vocal inmediato (< 1s), despacho a worker desacoplado que no se aborte por turnos posteriores, y encolamiento FIFO garantizado en `SpeechQueue` con `DeliveryGate` para entregar resultados completos sin colisiones de audio (✅ 474 tests pasando).
- [ ] **feat(automation): ojos y manos multiplataforma (Screen Inspector & Input Synthesizer)** (HU-38)
  - _Contexto de negocio:_ Dotar a Jarvis de capacidad genérica para ver la pantalla y enviar entradas de mouse/teclado de forma consistente en Linux (Wayland/X11) y Windows.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa a trabajar:_ `crates/jarvis-core` y nuevo módulo de automatización.
  - 📌 _Objetivo técnico:_ Abstraer captura visual (XDG Portal / `grim` / WinAPI) e inyección de eventos de teclado/mouse (`enigo` / `wtype` / `ydotool`), con salvaguardas de seguridad.
- [ ] **feat(automation): agente visual y de navegación web generalista (Computer Use / Visual Action Loop)** (HU-39)
  - _Contexto de negocio:_ Permitir que la IA interactúe con cualquier aplicación web o de escritorio (WhatsApp Web, Instagram, portales internos) reconociendo elementos visuales y el DOM, en vez de crear integraciones rígidas una a una.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa a trabajar:_ `crates/jarvis-audio` / herramientas del cerebro CLI.
  - 📌 _Objetivo técnico:_ Implementar un bucle de acción visual asistido (Playwright para navegación web desacoplada o Set-of-Marks para escritorio), con solicitud de confirmación vocal antes de acciones críticas o de envío.

---

## 🎙️ Épica 8: Personalización de Identidad y Activador Vocal Adaptativo
- [ ] **feat(audio): nombre personalizable y entrenador fonético local de wake-word** (HU-40)
  - _Contexto de negocio:_ El usuario puede renombrar al asistente y calibrar localmente las variantes acústicas con su propia voz para maximizar la tasa de acierto de activación.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa a trabajar:_ `crates/jarvis-audio` (detección de activación) y `crates/jarvis-app` (UI de entrenamiento).
  - 📌 _Objetivo técnico:_ Permitir configurar el nombre en `config.toml`, con una herramienta de grabación guiada (3 muestras de voz) que transcriba y almacene las variantes fonéticas del usuario en el detector de wake-word. Prioridad baja.

---

## 🗣️ Épica 9 (E-12): Conversación Natural: Turn-Taking Humano y Voz Expresiva con Guión
- _SDD / Propuesta Técnica:_ [`docs/specs/sdd-contracts/investigacion-turn-taking-voz-expresiva.md`](docs/specs/sdd-contracts/investigacion-turn-taking-voz-expresiva.md)
- _Objetivo:_ Eliminar cortes prematuros al hablar y pausas reflexivas/tomas de aire, introduciendo además prosodia, entonación, respiraciones y pausas dirigidas por el LLM.

### ⚡ Fase 1: Quick Wins Conversacionales y Prosodia (EN CURSO - PRIORIDAD MÁXIMA)
- [x] **HU-41: Endpointing configurable y observable** (✅ 652 tests pasando)
  - _Contexto de negocio:_ El usuario necesita que el tiempo de espera no sea fijo ni arbitrario, y poder auditar cómo corta el fin de turno.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa:_ `crates/jarvis-audio/src/vad/`, `io/engine.rs`, `crates/jarvis-app/src/config.rs`, `main.rs`.
  - 📌 _Objetivo técnico:_ Exponer `EndpointConfig` y `connector_grace` en `config.toml` (`[voice_endpoint]`) con validación de rangos. Registrar histograma de pausas intra-turno (P50/P90), razón de cierre y flag `--diag-turns`.
- [x] **HU-42: Ventana de continuación sin eco falso** (✅ 679 tests pasando)
  - _Contexto de negocio:_ Al hablar con pausas o respirar, Jarvis no debe interpretar la continuación como eco del chime ni responder en dos fragmentos separados. Chime con retraso controlado.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa:_ `crates/jarvis-audio/src/orchestrator.rs`, `conversation.rs`.
  - 📌 _Objetivo técnico:_ Ventana de continuación post-despacho (~1.5s) que nunca se descarte como eco, chime con leve retraso configurable, y fusión de turnos en `Speaking` temprano.
- [x] **HU-43: Detector léxico de incompletitud** (✅ tabla de verdad 65 casos)
  - _Contexto de negocio:_ Cuando el usuario deja frases a medias ("y...", "o sea...", "para..."), Jarvis debe esperar y no precipitarse.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa:_ `crates/jarvis-audio/src/orchestrator.rs`, `stt.rs`.
  - 📌 _Objetivo técnico:_ Reemplazar `ends_with_connector` por evaluador de incompletitud en español chileno (muletillas, preposiciones, artículos finales) y prompt de Whisper con disfluencias.
- [x] **HU-44: Turnos largos sin corte duro** (✅ parciales + techo `max_turn_ms` 120 s)
  - _Contexto de negocio:_ Si el usuario habla o dicta más de 30 segundos, Jarvis no debe cortar ni despachar la mitad.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa:_ `crates/jarvis-audio/src/vad/endpoint.rs`, `events.rs`, `orchestrator.rs`.
  - 📌 _Objetivo técnico:_ Fragmentación parcial acumulada al alcanzar `max_segment_ms` sin despachar, con techo total configurable hasta 120 s.
- [x] **HU-45: Protocolo de directivas de prosodia ("Director de Escena")** (✅ parser incremental + fuzzing, 705 tests)
  - _Contexto de negocio:_ El cerebro (LLM) debe poder indicar cómo decir cada frase para no sonar plano (`[ánimo:...]`, `[pausa:...]`, `[respira]`).
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa:_ `crates/jarvis-audio/src/tts/phrase.rs`, `brain/client.rs`.
  - 📌 _Objetivo técnico:_ Gramática de directivas, parser incremental inmune a tags cortados, extensión de `SYSTEM_PROMPT` y remoción limpia de directivas del HUD y audio.
- [x] **HU-46: Render expresivo sobre Edge-TTS** (✅ prosodia Edge + pausas PCM + respiración; 🧪 medición F0 pendiente)
  - _Contexto de negocio:_ Jarvis debe entonar con variaciones de tono, ritmo y pausas/respiraciones audibles naturales.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa:_ `crates/jarvis-audio/src/tts/edge.rs`, `tts/mod.rs`, `speech_queue.rs`, `playback.rs`.
  - 📌 _Objetivo técnico:_ Mapeo de directivas a `rate/pitch/volume` en Edge-TTS, silencios PCM y clips cacheados de respiración/inhalación.

### 🧠 Fase 2: Turn-Taking Inteligente con ML Acústico
- [ ] **HU-47: Fin de turno en dos etapas con Smart Turn v3 (Tract Rust)**
  - 📌 Integrar Smart Turn v3 (ONNX 8 MB, BSD-2) ejecutado con `tract` en Rust puro para arbitraje inteligente de silencios.
- [ ] **HU-48: Perfil de pausas adaptativo**
  - 📌 Auto-ajuste de umbrales según el ritmo habitual del usuario.
- [ ] **HU-49: Indicador "sigo escuchando" en esfera y modo dictado**
  - 📌 Feedback visual continuo y modo "toma nota" con cierre explícito.
- [ ] **HU-50: Continuidad de entonación**
  - 📌 Agrupación inteligente de frases por directiva para entonación armónica completa.

### 🔬 Fase 3 & QA Transversal
- [ ] **HU-51: Backend Azure Speech con SSML completo (Opcional/Key)**
- [ ] **HU-52: TTS local neural Kokoro-82M (Reemplazo offline)**
- [ ] **HU-53: STT especulativo y pre-generación**
- [ ] **HU-54: Corpus y suite QA de turn-taking y expresividad**

---

## 🔮 Épica 13 (E-13): Observabilidad Visual del Asistente y Robustez Operativa en Segundo Plano

### HU-55: Estados visuales de la esfera para Escucha Activa y Procesos en Background
- _Contexto de negocio:_ La esfera se mantenía en verde plano tanto en espera como al capturar un turno, generando incertidumbre sobre si Jarvis estaba ignorando la frase, grabando activamente o ejecutando procesos de fondo.
- 📁 _Repositorio:_ `Fokkus-Jarvis`
- 🎯 _Capa:_ `crates/jarvis-core`, `crates/jarvis-render`, `crates/jarvis-audio`, `crates/jarvis-app`.
- 📌 _Objetivo técnico:_
  - Escucha Pasiva (Standby): Verde Esmeralda suave (`#4CAF50`).
  - Escucha Activa (Turno en vivo/Follow-up): Amarillo eléctrico / Ámbar brillante (`#FFC107`).
  - Procesos en Segundo Plano (`active_tasks > 0`): Naranja cálido (`#FF7043`).

### HU-56: Calibración anti-eco en Barge-In e interrupción manual inmediata
- _Contexto de negocio:_ Durante locuciones largas, el habla del usuario era confundida por el VAD/AEC con eco de los parlantes y descartada, impidiendo interrumpir a Jarvis con facilidad. Tampoco existía mecanismo manual para callarlo de inmediato.
- 📁 _Repositorio:_ `Fokkus-Jarvis`
- 🎯 _Capa:_ `crates/jarvis-audio/src/orchestrator.rs`, `crates/jarvis-shell`, `crates/jarvis-app`.
- 📌 _Objetivo técnico:_
  - Ponderación de energía vocal humana sobre retroalimentación de altavoces en `barge_in`.
  - Soporte de corte manual instantáneo: clic izquierdo sobre la esfera durante `Speaking` (o tecla de interrupción) detiene `playback` en $\le 10$ ms.

### HU-57: Subagentes ejecutores reales de procesos OS en segundo plano con control de ciclo de vida
- _Contexto de negocio:_ Al pedirle a Jarvis monitores, contadores o pings en segundo plano, el subagente solo redactaba explicaciones teóricas en vez de ejecutar el proceso, debido a un prompt restrictivo. Además no se podían detener con la voz.
- 📁 _Repositorio:_ `Fokkus-Jarvis`
- 🎯 _Capa:_ `crates/jarvis-audio/src/tasks.rs`, `crates/jarvis-app`.
- 📌 _Objetivo técnico:_
  - Reconocimiento de tareas OS / scripts / monitores para no inyectar prompt descriptivo pasivo.
  - Registro y persistencia de PIDs en `running_processes.json`.
  - Comando verbal de terminación (*"detén el proceso"*, *"cancela la tarea"*).

### HU-58: Aislamiento headless de herramientas MCP en subagentes de fondo
- _Contexto de negocio:_ La invocación de `agy` en segundo plano cargaba MCPs sin autenticar que abrían el navegador por sorpresa en la pantalla del usuario.
- 📁 _Repositorio:_ `Fokkus-Jarvis`
- 🎯 _Capa:_ `crates/jarvis-audio/src/tasks.rs`.
- 📌 _Objetivo técnico:_
  - Aislamiento o flags en subagentes headless para silenciar la apertura espontánea de navegadores o interactividad gráfica no solicitada.

---

## 🧵 Épica 14 (E-14): Hilo Central + Sub-sesiones Ramificadas — ✅ v0.3.0
- _Propuesta técnica:_ [`docs/specs/propuesta-multi-sesiones-subhilos.md`](docs/specs/propuesta-multi-sesiones-subhilos.md)
- [x] **HU-59** Normalización léxica STT para clasificación (`text/normalize.rs`) · **HU-60** subagentes desacoplados · **HU-61** subcomando `task run` [Fase A]
- [x] **HU-62** Gestor de sub-sesiones persistentes · **HU-63** `TurnRouter` con planificador · **HU-64** direccionamiento verbal y foco · **HU-65** roster en contexto efímero [Fase B]
- [x] **HU-66** Protocolo IPC y CLI de sub-sesiones · **HU-67/68** chips HUD, menú de bandeja y métricas de enrutamiento [Fase C]

---

## 🛡️ Épica 15 (E-15): Excelencia Conversacional, STT Seguro e Introspección Nativa del Sistema (Grado Comercial)
- _SDD:_ [`docs/specs/sdd-epica-15-excelencia-conversacional.md`](docs/specs/sdd-epica-15-excelencia-conversacional.md)
- _Contexto de negocio:_ Jarvis transcribe mal tecnicismos y jerga chilena, abre Google sin que se lo pidan (incluso cuando el usuario dice "no busques en Google") y no sabe de qué aplicación vienen las notificaciones del escritorio. Restricciones: 100 % local y gratuito, **Hugging Face vetado**, sin root y sin `pip`.
- _Versión objetivo:_ v0.4.0

### HU-69: STT `small-q5_1` con orígenes de descarga soberanos (sin Hugging Face)
- 📁 _Repositorio:_ `Fokkus-Jarvis`
- 🎯 _Capa:_ `crates/jarvis-audio/src/models.rs`, `stt.rs`, `crates/jarvis-core` (config), `crates/jarvis-app` (`runtime.rs`, `main.rs`), `scripts/`, GitHub Releases.
- 📌 _Objetivo técnico:_ Usar `small-q5_1` por defecto y `base-q5_1` como respaldo. Migrar **los tres** modelos (tiny, base y small; hoy los dos actuales salen de HF) a GitHub Releases propios con SHA-256 y tamaño fijados y redirecciones limitadas a hosts de GitHub. El CDN de OpenAI solo publica `.pt`, así que el mantenedor convierte y cuantiza offline con un pipeline reproducible y sin `pip`. RAM y latencia se miden, no se suponen.

### HU-70: Corrector léxico/fonético determinista (tecnicismos y jerga chilena)
- 📁 _Repositorio:_ `Fokkus-Jarvis`
- 🎯 _Capa:_ `crates/jarvis-audio/src/text/` (módulo nuevo, separado de `normalize.rs` de HU-59), `stt.rs` (`initial_prompt`), config `[stt_lexicon]`.
- 📌 _Objetivo técnico:_ Corrección post-STT por palabra completa y frase más larga que conserve tildes y mayúsculas, con < 1 ms por turno, diccionario base de ≥ 60 entradas con tabla de negativos y extensión acotada del usuario que no pueda inyectar wake-word ni comandos de anulación.

### HU-71: Erradicación de `bare_search` y restricciones negativas del turno
- 📁 _Repositorio:_ `Fokkus-Jarvis`
- 🎯 _Capa:_ `crates/jarvis-audio/src/orchestrator.rs` (`local_command`, `site_request`, `bare_search`), `router/` (`TurnPlan`, planificador), `brain/client.rs`.
- 📌 _Objetivo técnico:_ "Busca X" sin sitio deja de ir a Google. Una negación explícita ("no busques en Google") pasa a ser una restricción dura `no_web` que se extrae **antes** de `local_command` y que ni las heurísticas, ni el planificador, ni el cerebro pueden saltarse. Bug actual: `site_request` abre Google con "no busques en Google, dime X".

### HU-72: Introspección nativa de notificaciones de escritorio vía D-Bus
- 📁 _Repositorio:_ `Fokkus-Jarvis`
- 🎯 _Capa:_ `crates/jarvis-shell` (zbus), `crates/jarvis-audio/src/orchestrator.rs`, `crates/jarvis-core`, `crates/jarvis-app`.
- 📌 _Objetivo técnico:_ Monitor monitor-only de `org.freedesktop.Notifications.Notify` en el bus de sesión (sin root), con emisor verificado por PID → `/proc/<pid>/exe`. Anillo solo en memoria, con TTL, exclusiones y enmascarado de OTP. El contenido se trata como dato no confiable (anti inyección de prompt) y las preguntas sobre el origen se responden en local, sin web.

---

## 🧾 Deuda técnica

- [ ] **Cablear el pipeline de voz en el binario** (hallazgo QA v0.2.1, bloqueante para "escucha y habla"): `jarvis-app` nunca abre streams `cpal` ni lanza `spawn_dsp_thread`/VAD/`WhisperEngine`/`ClaudeClient`/salida TTS. `start_listening()` solo cambia la máquina de estados. Los componentes existen y están testeados en `jarvis-audio`; falta el orquestador captura → DSP → VAD/endpointing → STT → activación → `run_turn_streaming` → playback. Brief listo para DeepSeek V4 Pro: [`docs/specs/brief-hu9-orquestador-voz.md`](docs/specs/brief-hu9-orquestador-voz.md) (HU-ORQ-1 a HU-ORQ-5).
- [x] **Clic derecho cerraba la app sin aviso** (feedback v0.2.1): ahora abre un menú contextual `xdg_popup` con *grab* (Silenciar/Activar micrófono, Posición inicial, Salir); clic fuera lo cierra. "Silenciar" hoy solo cambia el estado: liberar el micrófono queda en HU-ORQ-4.
- [ ] **Unificar `JarvisCore`** (cubierto por HU-ORQ-4; hallazgo QA, HU-4 de `docs/specs/brief-hu-overlay-event-loop.md`): `ConversationCoordinator` y `ServerState` mantienen núcleos independientes. La esfera ya lee el estado de `ServerState::state()` (fuente de verdad del IPC) y el nivel de audio del coordinador, pero un `start_listening()` del coordinador no se refleja en el color de la esfera. Requiere compartir un único núcleo (`Arc<Mutex<JarvisCore>>` o eventos) cambiando APIs de `jarvis-ipc`/`jarvis-audio`.
- [ ] **CPU en reposo de la esfera** (~6 % de un núcleo en release a 30 FPS): `SphereRenderer::render_into` cuesta ~1,6 ms por frame a 220×220. Objetivo M5 < 5 %: cachear el halo estático o bajar a 20 FPS en `Idle`.
- [ ] **Clippy en Windows** (`--target x86_64-pc-windows-gnu`): `result_large_err` en `jarvis-core/src/config.rs` (`ConfigError::Parse`). `cargo check` compila; solo falla el lint.

---

## 📊 Métricas Globales (2026-10-15)

- ✅ **163 pruebas unitarias pasando al 100%** en todo el workspace (**0 fallos**).
- ✅ Bucle principal con apagado limpio (SIGINT/SIGTERM < 50 ms) y overlay `zwlr_layer_shell_v1` en KWin Wayland, con fallback headless.
- ✅ `make check` / `make lint` / `make fmt-check` limpios (sin warnings).
- ✅ Binario `jarvis-app` con CLI operativo: `--version` (`-v`), `--check` (`-c`), `--help` (`-h`).
- ✅ Socket IPC activo y listo para conexiones de Fokkus IDE.
