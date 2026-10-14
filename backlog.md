# Product Backlog — Fokkus-Jarvis

Este documento contiene el seguimiento de funcionalidades, mejoras y arquitectura para el asistente de escritorio autónomo **Fokkus-Jarvis**.

Repositorio oficial: [jucsp/Fokkus-Jarvis](https://github.com/jucsp/Fokkus-Jarvis)  
Especificación SDD: [docs/specs/sdd-fokkus-jarvis.md](docs/specs/sdd-fokkus-jarvis.md)

---

## 🚀 Épica 1: Núcleo Nativo y Ventana Transparente Multiplataforma (Rust Nativo)
- [ ] [#1](https://github.com/jucsp/Fokkus-Jarvis/issues/1) **feat(core): setup inicial de Fokkus Jarvis en Rust nativo (layer-shell Wayland / winit / tiny-skia)**
  - _Alcance:_ Configuración de workspace en Rust nativo. Ventana frameless nativa sin decoraciones del SO, capa overlay real en Wayland (`zwlr_layer_shell_v1`) en KWin/Caelestia para soporte Always-On-Top y visible en todos los escritorios. Fallback winit para X11/Windows. Renderizado CPU ultraligero con `tiny-skia` para la esfera líquida (sin drivers GPU pesados). Consumo en reposo $\le 25-40$ MB RAM. Estructura multi-crate y Makefile.
  - _Estado QA (2026-10-14):_ ⚠️ entrega parcial, issue abierto — ver [reporte QA](docs/specs/qa-issue1-fase1.md).
    - [x] 1.1 Workspace Cargo, toolchain y Makefile
    - [x] 1.2 Máquina de estados y bus de eventos (tabla 100 % cubierta)
    - [ ] 1.3 Esfera líquida — falta animación temporal (render 1.66 ms/frame ✅)
    - [ ] 1.0 / 1.4 Ventana overlay `zwlr_layer_shell_v1` en KWin
    - [ ] 1.5 Fallback winit + detección real de layer-shell
    - [ ] 1.6 Instancia única y config TOML persistente
    - [ ] 1.7 `cargo-deny` y snapshot de pixmap (fmt + clippy ✅)
    - [ ] 1.8 Medición RSS ≤ 40 MB y CPU en reposo

---

## 🎙️ Épica 2: Motores de Audio de Alta Fidelidad y Voz Natural
- [ ] [#2](https://github.com/jucsp/Fokkus-Jarvis/issues/2) **feat(audio): pipeline de audio full-duplex con Whisper (STT) y Edge-TTS neuronal con caché de fillers**
  - _Alcance:_ Captura de audio continuo con `cpal` (16 kHz mono) y buffer deslizante de 300 ms de audio previo. Transcripción local rápida con Whisper y VAD Silero. Síntesis neuronal de alta fidelidad con `edge-tts` (15 voces neuronales, `es-CL-LorenzoNeural` por defecto) y fallback offline instantáneo a `espeak-ng`. Pre-síntesis y almacenamiento en caché de frases de relleno conversacional (*fillers*) para latencia de inicio $< 300$ ms. Reproducción con detención inmediata (*fade-out* 15 ms).

---

## 🗣️ Épica 3: Experiencia Conversacional Humana (Sin Silencios e Interrumpible)
- [ ] [#3](https://github.com/jucsp/Fokkus-Jarvis/issues/3) **feat(conversation): conversación fluida con respuestas de relleno natural (Conversational Fillers) durante el razonamiento**
  - _Alcance:_ Emisión inmediata (< 300 ms) de frases de confirmación contextuales (*"Entendido, déjame revisar..."*, *"Dame un segundo..."*) al entrar en estado `thinking`, eliminando el silencio incómodo mientras el LLM genera la respuesta completa. Transición fluida y natural al flujo de la respuesta principal.
- [ ] [#4](https://github.com/jucsp/Fokkus-Jarvis/issues/4) **feat(conversation): interrupción humana en tiempo real (Barge-In) con preservación de procesos en segundo plano**
  - _Alcance:_ Detección de voz en paralelo con cancelación de eco mientras Jarvis habla. Corte instantáneo de audio (< 50 ms) y transición inmediata a `listening` al hablar el usuario. Si hay tareas técnicas del Swarm en segundo plano (Claude/DeepSeek trabajando), la interrupción de voz NO mata el proceso a menos que el usuario lo ordene explícitamente.

---

## 🔌 Épica 4: Integración con Fokkus IDE y Supervisión Asíncrona del Swarm
- [ ] [#5](https://github.com/jucsp/Fokkus-Jarvis/issues/5) **feat(ipc): servidor Unix Socket / Named Pipe para integración bidireccional con Fokkus IDE y monitoreo asíncrono del Swarm**
  - _Alcance:_ Servidor JSON-RPC 2.0 sobre `~/.fokkus/jarvis.sock` (Linux/macOS) y `\\.\pipe\fokkus-jarvis` (Windows). Métodos bidireccionales: `jarvis.dispatchSwarm`, `jarvis.notifyTriageAlert`, `jarvis.notifySwarmCompleted`, `jarvis.getSwarmStatus`. Capacidad para consultar verbalmente el avance del equipo (*"Jarvis, ¿cómo va el desarrollo?"*).

---

## 🤖 Épica 5: Operador de Sistema y Capacidades Autónomas (Standalone)
- [ ] [#6](https://github.com/jucsp/Fokkus-Jarvis/issues/6) **feat(os-tools): orquestador de sistema y herramientas autónomas (Archivos, Terminal, Computer Use y conectores MCP)**
  - _Alcance:_ Orquestador de sistema cuando opera sin Fokkus IDE. Tool calling agnóstico (Gemini, Claude, DeepSeek, Ollama). Inspección de archivos en `~/Descargas` y documentos, ejecución de comandos CLI aprobados, visión multimodal de pantalla y conectores MCP (Google Drive, Calendar, Slack).
