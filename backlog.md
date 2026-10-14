# Product Backlog — Fokkus-Jarvis

Este documento contiene el seguimiento de funcionalidades, mejoras y arquitectura para el asistente de escritorio autónomo **Fokkus-Jarvis**.

Repositorio oficial: [jucsp/Fokkus-Jarvis](https://github.com/jucsp/Fokkus-Jarvis)  
Especificación SDD: [docs/specs/sdd-fokkus-jarvis.md](docs/specs/sdd-fokkus-jarvis.md)

---

## 🚀 Épica 1: Núcleo Nativo y Ventana Transparente Multiplataforma (Rust + Tauri)
- [ ] [#1](https://github.com/jucsp/Fokkus-Jarvis/issues/1) **feat(core): setup inicial de Fokkus Jarvis en Rust/Tauri con ventana flotante frameless y transparente**
  - _Alcance:_ Configuración de proyecto en Rust + Tauri. Ventana frameless nativa sin decoraciones del SO, fondo 100% transparente en Wayland (KDE / Caelestia), X11, Windows 10/11 y macOS. Always-On-Top y persistente en todos los escritorios virtuales (`visible_on_all_workspaces: true`). Canvas 2D con esfera líquida orgánica reactiva. Consumo en reposo $\le 40$ MB RAM.

---

## 🎙️ Épica 2: Motores de Audio de Alta Fidelidad y Voz Natural
- [ ] [#2](https://github.com/jucsp/Fokkus-Jarvis/issues/2) **feat(audio): pipeline de audio full-duplex con Faster-Whisper base (STT) y Edge-TTS neuronal**
  - _Alcance:_ Integración de `faster-whisper` (base, int8 en CPU, beam_size=5, Silero VAD activo) con buffer WebM continuo (sin latencia de hardware). Síntesis neuronal de alta fidelidad con `edge-tts` (15 voces neuronales, `es-CL-LorenzoNeural` por defecto) y fallback offline instantáneo a `espeak-ng`. Selector interactivo de voces con preescucha en vivo (▶️ / ⏹️).

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
