# Fokkus-Jarvis 🎙️✨

> **Asistente de voz autónomo y operador de escritorio de ultra-baja latencia para Linux (Wayland/X11), Windows y macOS. Complemento nativo de Fokkus IDE.**

[![GitHub Issues](https://img.shields.io/github/issues/jucsp/Fokkus-Jarvis)](https://github.com/jucsp/Fokkus-Jarvis/issues)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

---

## 🌟 Características Principales

* ⚡ **Ultra-ligero en Rust + Tauri:** Consumo inferior a **40 MB de memoria RAM** en reposo (frente a los más de 400 MB de aplicaciones tradicionales en Electron).
* 🪟 **Ventana Flotante Frameless y 100% Transparente:**
  * Soporte nativo para **Wayland** (KDE Plasma, Caelestia, GNOME y gestores en mosaico / *tiling WMs*), X11, Windows 10/11 y macOS.
  * Sin barras de título del SO, sin marcos y sin menús.
  * **Always-On-Top y persistente:** Se mantiene visible en todos los escritorios virtuales (`visible_on_all_workspaces`).
* 🗣️ **Conversación Fluida Humana:**
  * **Frases de relleno en tiempo real (*Conversational Fillers*):** Elimina el silencio incómodo emitiendo confirmaciones contextuales (*"Dame un segundo, lo estoy revisando..."*) en menos de 300 ms mientras el modelo razona.
  * **Interrupción instantánea (*Voice Barge-In*):** Si el usuario empieza a hablar mientras Jarvis emite una respuesta, Jarvis se calla de inmediato (< 50 ms) y vuelve a escuchar con atención.
* 🎙️ **Motores de Audio de Alta Definición:**
  * **STT Local:** Transcripción precisa con `faster-whisper` (modelo `base`, beam_size=5, Silero VAD activo en CPU).
  * **TTS Neuronal:** 15 voces humanas de alta fidelidad con `edge-tts` (Español de Chile por defecto, México, Argentina, Colombia, Perú, España, EE.UU.) y fallback offline inmediato a `espeak-ng`.
* 🤖 **Modo Dual (Autónomo o Conectado a Fokkus IDE):**
  * **Standalone:** Operador de sistema operativo autónomo (archivos, terminal, visión de pantalla y conectores MCP).
  * **Conectado a Fokkus IDE:** Servidor IPC local (`~/.fokkus/jarvis.sock`) para supervisar el Swarm de desarrollo (PO, PM, Dev) y recibir alertas de la bandeja de solicitudes (Slack/Gmail) por voz.

---

## 📚 Documentación

* 📋 [Contrato y Especificación Técnica SDD](docs/specs/sdd-fokkus-jarvis.md)
* 🗺️ [Product Backlog y Seguimiento de Tareas](backlog.md)

---

## 👥 Equipo y Créditos

Desarrollado con arquitectura agnóstica por **Juan Carlos (Kasper)** y el ecosistema **Fokkus**.
