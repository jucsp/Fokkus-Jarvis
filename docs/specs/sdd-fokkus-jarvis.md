# Especificación Técnica Formal (SDD): Fokkus-Jarvis

**Proyecto:** Fokkus-Jarvis  
**Autor:** Antigravity (Product Owner) & Juan Carlos (Architect)  
**Versión del Documento:** 1.0.0  
**Fecha:** 2026-10-14  
**Estado:** Aprobado para Implementación  

---

## 1. Resumen Ejecutivo y Visión del Producto

### 1.1. Contexto y Justificación (ADR - Architectural Decision Record)
En versiones anteriores de **Fokkus IDE** (v1.8.0 a v1.8.5), el asistente de voz *Jarvis* fue implementado como una extensión embebida (`fokkus-orchestrator`) dentro de la arquitectura de Eclipse Theia / Electron. Aunque la síntesis neuronal (`edge-tts`), la transcripción local (`faster-whisper`) y la esfera líquida en Canvas 2D alcanzaron alta calidad, la arquitectura acoplada al IDE demostró limitaciones estructurales insalvables:

1. **Incompatibilidad de Ventana Overlay en Linux/Wayland:** Theia intercepta cada llamada a `window.open`, forzando marcos de ventana nativos (`frame: true`), barras de menú (`File Edit View Window`) y fondos blancos opacos no deseados en gestores como KDE Plasma, Wayland y shells modernos en mosaico como Caelestia.
2. **Ciclo de Vida Restringido:** Al vivir dentro del IDE, minimizar o cerrar Fokkus IDE destruía al asistente, impidiendo que Jarvis opere como un compañero permanente de escritorio en segundo plano mientras el usuario navega, diseña o juega.
3. **Consumo Ocioso de Recursos:** Levantar todo el stack de Theia/Electron para un asistente de voz consume > 600 MB de RAM.
4. **Desdibujamiento de Dominio:** Un IDE debe estar enfocado en ingeniería de software; dotarlo de capacidades de sistema operativo (inspección de descargas, calendario, computer use, control del navegador) sobrecarga la base de código.

### 1.2. La Decisión de Producto
Se decide desacoplar a **Jarvis** de Fokkus IDE y convertirlo en un producto independiente: **`Fokkus-Jarvis`**, una aplicación de escritorio nativa de ultra-baja latencia construida en **Rust + Tauri**:
* **Consumo objetivo:** $\le$ 40 MB de memoria RAM en reposo.
* **Ventana flotante nativa:** Frameless, 100 % transparente, Always-on-Top y persistente entre todos los escritorios virtuales (`visible_on_all_workspaces`).
* **Operador de Sistema Autónomo:** Con cerebro frontal desacoplado y conectores de sistema (archivos, terminal, visión, MCP).
* **Conector de Alta Fidelidad con Fokkus IDE:** Servidor IPC local (`jarvis.sock`) para orquestar al Swarm (PO, PM, Dev) y recibir alertas de la bandeja de solicitudes de forma proactiva.

---

## 2. Arquitectura General del Sistema

```
┌────────────────────────────────────────────────────────────────────────┐
│                        FOKKUS-JARVIS (Rust + Tauri)                    │
│                                                                        │
│  ┌───────────────────────┐   ┌──────────────────────────────────────┐  │
│  │   UI Canvas 2D        │   │   Audio Engine (Full-Duplex)         │  │
│  │   Esfera líquida      │◄─►│   - faster-whisper base (Silero VAD) │  │
│  │   Temas reactivos     │   │   - edge-tts (15 voces neuronales)   │  │
│  │   Modal Selector Voz  │   │   - Voice Barge-In (< 50ms)          │  │
│  └───────────────────────┘   └──────────────────────────────────────┘  │
│                                                   ▲                    │
│                                                   │                    │
│  ┌────────────────────────────────────────────────┴─────────────────┐  │
│  │          Core Conversacional & Orquestador Frontal (Rust)        │  │
│  │   - Conversational Fillers (< 300ms) durante razonamiento        │  │
│  │   - Intent Router & Tool Calling (System, Files, Browser)        │  │
│  │   - Dynamic Swarm Knowledge (~/.fokkus/team_config.json)        │  │
│  └────────────────────────┬─────────────────────────────────────────┘  │
│                           │                                            │
│  ┌────────────────────────┴─────────────────────────────────────────┐  │
│  │          Servidor IPC Local (Unix Domain Socket / Named Pipe)     │  │
│  │          ~/.fokkus/jarvis.sock                                   │  │
│  └────────────────────────┬─────────────────────────────────────────┘  │
└───────────────────────────┼────────────────────────────────────────────┘
                            │ JSON-RPC 2.0 (Bidireccional)
┌───────────────────────────▼────────────────────────────────────────────┐
│                        FOKKUS IDE (Cliente)                            │
│                                                                        │
│  ┌────────────────────────┐  ┌───────────────────┐  ┌───────────────┐  │
│  │ Status Bar & Launcher  │  │ Triage Inbox      │  │ Swarm Engine  │  │
│  │ Detección/Descarga     │  │ Alertas proactivas│  │ PO, PM, Dev   │  │
│  └────────────────────────┘  └───────────────────┘  └───────────────┘  │
└────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Protocolo de Comunicación IPC (Fokkus-Jarvis $\longleftrightarrow$ Fokkus-IDE)

La comunicación se establece a través de un Unix Domain Socket local en Linux/macOS (`~/.fokkus/jarvis.sock`) y Named Pipe en Windows (`\\.\pipe\fokkus-jarvis`) bajo el estándar **JSON-RPC 2.0**.

### 3.1. Métodos Expuestos por Fokkus-Jarvis (Llamados desde Fokkus IDE)

#### `jarvis.ping`
Verifica conectividad y presencia del asistente de voz.
* **Request:** `{ "jsonrpc": "2.0", "method": "jarvis.ping", "id": 1 }`
* **Response:** `{ "jsonrpc": "2.0", "result": { "status": "ok", "version": "1.0.0", "voice": "es-CL-LorenzoNeural" }, "id": 1 }`

#### `jarvis.notifyTriageAlert`
Fokkus IDE notifica a Jarvis la llegada de un nuevo ticket desde Slack o Gmail.
* **Request:**
  ```json
  {
    "jsonrpc": "2.0",
    "method": "jarvis.notifyTriageAlert",
    "params": {
      "ticketId": "triage-4819",
      "source": "slack",
      "author": "María González (Melón)",
      "summary": "Error en generación de reporte PDF",
      "priority": "high"
    },
    "id": 2
  }
  ```
* **Comportamiento en Jarvis:** Si el usuario no está en modo silencio (`muted`), Jarvis sintetiza verbalmente: *"Juan Carlos, acaba de entrar una solicitud urgente de Melón en Slack. ¿Quieres que te la lea?"*.

#### `jarvis.notifySwarmProgress` / `jarvis.notifySwarmCompleted`
Notificación del estado de ejecución del equipo de desarrollo.
* **Request:**
  ```json
  {
    "jsonrpc": "2.0",
    "method": "jarvis.notifySwarmCompleted",
    "params": {
      "taskId": "task-102",
      "summary": "Refactor de login completado",
      "testsPassed": 43,
      "testsFailed": 0
    },
    "id": 3
  }
  ```

### 3.2. Métodos Invocados por Fokkus-Jarvis hacia Fokkus IDE

#### `ide.dispatchPrompt`
Jarvis delega una orden técnica de desarrollo hacia el IDE si este se encuentra en ejecución.
* **Request:**
  ```json
  {
    "jsonrpc": "2.0",
    "method": "ide.dispatchPrompt",
    "params": {
      "prompt": "Implementa autenticación OAuth2 con Google",
      "projectPath": "/home/juancarlos/Proyectos/Personales/Fokkus-IDE",
      "mode": "auto"
    },
    "id": 10
  }
  ```

#### `ide.getWorkspaceContext`
Consulta el proyecto o workspace activo actualmente en el IDE.

---

## 4. Especificación de Conversación Fluida e Interrupción Humana

### 4.1. Conversación Fluida con Frases de Relleno (*Conversational Fillers*)
* **Problema resuelto:** En la interacción anterior, tras terminar de hablar el usuario, la burbuja entraba en estado `thinking` y se producía un silencio absoluto de 3 a 10 segundos mientras el modelo procesaba el prompt y ejecutaba herramientas, creando una experiencia rígida y robotizada.
* **Arquitectura de Relleno:**
  1. En cuanto el motor VAD detecta el fin de la elocución y Whisper emite el texto final, el orquestador dispara **en paralelo**:
     - A) La llamada al modelo de razonamiento / tools (LLM).
     - B) Un generador de frases de confirmación inmediata (< 300 ms).
  2. El generador selecciona una frase contextual con variabilidad determinista:
     * *Consultas generales:* "Entendido, déjame revisar...", "Dame un segundo..."
     * *Órdenes de desarrollo:* "Comprendido, analizando el repositorio...", "Preparando la tarea..."
     * *Bandeja/Tickets:* "Revisando la bandeja de entrada..."
  3. La frase corta se sintetiza de inmediato por Edge-TTS mientras el LLM continúa generando la respuesta en segundo plano.
  4. En cuanto el primer token de la respuesta real llega por streaming, el reproductor transiciona de forma continua sin pausas artificiales.

### 4.2. Interrupción Humana en Tiempo Real (*Voice Barge-In*)
* **Problema resuelto:** Si Jarvis comienza una locución larga, el usuario debe poder interrumpirlo hablando por encima, tal como ocurre en una conversación humana real.
* **Comportamiento acústico:**
  - El micrófono permanece activo durante la reproducción del audio (`speaking`), con cancelación de eco acústico (AEC).
  - Al detectar energía de voz del usuario con confianza superior al umbral RMS (0.018) durante $\ge 120$ ms:
    1. Se ejecuta inmediatamente `stopSpeaking()` en el motor de audio (< 50 ms).
    2. La reproducción se corta sin chasquidos (*smooth fade-out* de 15 ms).
    3. El estado de la burbuja transiciona a `listening` y comienza la captura del nuevo comando.
* **Preservación de Tareas en Segundo Plano:**
  - Si el usuario interrumpe a Jarvis mientras un subproceso del Swarm (Claude/DeepSeek) está corriendo en segundo plano:
    - La interrupción **NO cancela la tarea técnica** salvo que el usuario lo ordene explícitamente (*"Jarvis, cancela lo que estabas haciendo"* vs *"Oye Jarvis, otra pregunta antes..."*).

### 4.3. Supervisión Asíncrona de Tareas del Swarm
* Jarvis mantiene un registro de procesos activos en segundo plano (`background_tasks`).
* El usuario puede continuar conversando con Jarvis sobre cualquier otro tema mientras el Swarm trabaja.
* Si el usuario pregunta: *"Jarvis, ¿cómo va el equipo?"*:
  - Jarvis consulta el estado del proceso en disco o vía socket.
  - Responde verbalmente: *"Claude PM sigue ejecutando la suite de tests, van 38 de 43 pruebas completadas"*.

---

## 5. Estrategia de Funcionamiento Autónomo y Desacoplado

### 5.1. Operación Headless (Sin Fokkus IDE Abierto)
Cuando el usuario solicita trabajo de desarrollo con el IDE cerrado:
1. Jarvis detecta la existencia de `~/.fokkus/team_config.json`.
2. Extrae las definiciones de los roles:
   * **Product Owner (PO):** `agy --model ... -p`
   * **Project Manager (PM/QA):** `claude --model sonnet -p`
   * **Senior Dev:** `python3 ~/.vibe/deepseek_task.py --model pro`
3. Instancia la orquestación en segundo plano lanzando los subprocesos de CLI directamente en la ruta del repositorio detectado, leyendo y actualizando `technical_memory.md` y `backlog.md`.
4. Emite reportes verbales periódicos y al finalizar la tarea.

### 5.2. Detección Inteligente de Proyecto
Jarvis determina el repositorio de trabajo mediante:
1. Nombre explícito en la voz (*"en el proyecto Melón"* $\rightarrow$ busca en lista de proyectos conocidos).
2. Ventana enfocada en el sistema operativo (mediante llamadas nativas a KWin / Wayland / Windows API).
3. Último workspace registrado en `~/.fokkus/sessions/`.
4. Pregunta breve de confirmación en caso de ambigüedad (*"¿Te refieres al proyecto Fokkus-IDE o al proyecto Melón?"*).

---

## 6. Plan de Desacoplamiento en Fokkus IDE

Para mantener a Fokkus IDE limpio y profesional:
1. **Retiro de Jarvis Embebido:** Se eliminan los hooks de `jarvis-controller.ts` y la ventana secundaria overlay de Theia.
2. **Nuevo Módulo Integrador en Fokkus IDE (`fokkus-jarvis-connector`):**
   - Icono y estado de conexión en la barra de estado (*Status Bar*):
     * 🟢 **Jarvis Conectado:** Muestra tooltip "Jarvis Companion activo en segundo plano".
     * ⚪ **Jarvis No detectado:** Muestra botón "Conectar con Jarvis / Instalar Asistente de Voz".
   - Al pulsar el botón cuando no está instalado: abre un diálogo modal con la opción de **descargar e instalar automáticamente el RPM / AppImage / instalador nativo** desde los Releases de `github.com/jucsp/Fokkus-Jarvis`.
3. **Liberación de Release Fokkus IDE v1.9.0:** Fokkus IDE se publica como una versión enfocada puramente en desarrollo, con integración desacoplada hacia Jarvis.

---

## 7. Plan de Ejecución por Fases (Milestones)

| Fase | Hito | Descripción | Entregable |
|---|---|---|---|
| **Fase 1** | **Setup Core Rust/Tauri** | Creación de la estructura del proyecto en Rust + Tauri, ventana frameless, 100% transparente en Wayland/X11/Windows, Always-on-top y sticky en todos los escritorios. Montaje del Canvas 2D líquido. | Issue #1 |
| **Fase 2** | **Audio Full-Duplex & Fillers** | Integración nativa de `faster-whisper` (base) y `edge-tts` (15 voces). Implementación de frases de relleno conversacional (< 300 ms) durante el estado `thinking`. | Issue #2 & #3 |
| **Fase 3** | **Barge-In & Servidor IPC** | Implementación de interrupción acústica en tiempo real (< 50 ms) y servidor de socket local `~/.fokkus/jarvis.sock` (JSON-RPC 2.0). | Issue #4 & #5 |
| **Fase 4** | **Herramientas de SO & Desacoplamiento IDE** | Orquestador de sistema (archivos, terminal, MCPs) y actualización en Fokkus IDE para conectarse al nuevo daemon independiente. | Issue #6 & Fokkus IDE v1.9.0 |
