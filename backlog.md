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
- [x] [#7](https://github.com/jucsp/Fokkus-Jarvis/issues/7) **fix(cli): soporte para tareas asíncronas en segundo plano y desacoplamiento del timeout síncrono de 120s** (Contrato: [sdd-issue7-tareas-asincronas-cli-timeout.md](docs/specs/sdd-contracts/sdd-issue7-tareas-asincronas-cli-timeout.md)) ✅ Cerrada en v0.2.26
  - _Contexto de negocio:_ Evitar que órdenes de desarrollo ("lánselo no más", "inicia el pipeline", orquestaciones con `agy`) bloqueen la conversación durante 120s y fallen por timeout rígido.
  - 📁 _Repositorio:_ `Fokkus-Jarvis`
  - 🎯 _Capa a trabajar:_ `crates/jarvis-core` (`cli_timeout_secs`), `crates/jarvis-audio` (`classify_workload`, `LIST_TASK_PHRASES`), `crates/jarvis-app` (`settings.rs`).
  - 📌 _Objetivo técnico:_ Clasificación asíncrona inmediata de tareas de desarrollo/swarm, despacho desacoplado a `TaskManager`, timeout CLI configurable en `config.toml` y consulta verbal de estado de tareas en curso.
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
