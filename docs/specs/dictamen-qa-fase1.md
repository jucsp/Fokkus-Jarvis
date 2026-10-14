# Dictamen Técnico PM/QA — Fokkus-Jarvis, Fase 1

**Emisor:** Claude Code (PM/QA del Swarm)
**Destinatarios:** Antigravity (PO/Arquitecto), DeepSeek V4 Pro (Dev Senior), Juan Carlos (usuario)
**Fecha:** 2026-10-14
**Documentos revisados:** `docs/specs/sdd-fokkus-jarvis.md` v1.0.0, `backlog.md`, issues #1–#6 de `jucsp/Fokkus-Jarvis`
**Veredicto:** ✅ **APROBADO CON ENMIENDAS OBLIGATORIAS** (stack de UI, motor STT, contrato IPC y presupuestos de latencia)

---

## 0. Contexto de Negocio

El usuario quiere un asistente de voz (Jarvis) que viva **siempre visible en el escritorio**, independiente de Fokkus IDE, que no consuma cientos de MB y que converse como una persona: que no se quede callado mientras piensa y que se calle cuando el usuario lo interrumpe. Hoy Jarvis vive dentro de Theia/Electron: muere al cerrar el IDE, consume >600 MB y en Wayland/KDE aparece con marcos y fondo blanco. La Fase 1 debe entregar el "cuerpo" del producto: la ventana flotante con la esfera y el pipeline de audio.

---

## 1. Entorno verificado (evidencia)

| Elemento | Resultado |
|---|---|
| Toolchain | ✅ `rustc 1.99.0`, `cargo 1.99.0` en `~/.cargo/bin` |
| Sesión | ✅ Wayland, KDE Plasma 6.7.5 (Fedora 44) |
| `zwlr_layer_shell_v1` en KWin | ✅ Expuesto (v5) — clave para overlay real |
| `org_kde_plasma_virtual_desktop_management` | ✅ Expuesto |
| `webkit2gtk4.1-devel` / `gtk3-devel` (Tauri) | ❌ No instalados |
| `alsa-lib-devel` (cpal) | ❌ No instalado — disponible en dnf |
| `pipewire-devel`, `wayland-devel`, `libxkbcommon-devel`, `cmake` | ✅ Instalados |
| `whisper-cpp` / `whisper-cpp-devel` 1.8.1 | ⚪ Disponible en dnf (repos oficiales Fedora) |
| `webrtc-audio-processing-devel` 2.1 (AEC) | ⚪ Disponible en dnf |
| `espeak-ng` | ✅ Instalado |
| `python3-faster-whisper` / `python3-ctranslate2` | ❌ **No existe en repos** → bloqueado por regla "solo paquetes del sistema, nunca pip" |
| `piper` en dnf | ⚠️ Es el configurador de ratones (libratbag), **no** el TTS Piper |
| `~/.fokkus/` | ❌ No existe (el SDD asume `~/.fokkus/team_config.json`) |
| `gh` CLI | ❌ No instalado (issues leídos vía API pública de GitHub) |

---

## 2. Dictamen: Tauri vs. eframe/egui

### 2.1. Tauri — ❌ Descartado
1. **Incumple el objetivo de RAM.** Tauri en Linux = GTK3 + WebKitGTK; `WebKitWebProcess` + `WebKitNetworkProcess` suman típicamente 80–150 MB RSS. El objetivo ≤ 40 MB no es alcanzable.
2. **Dependencias C pesadas** (webkit2gtk, gtk3, libsoup) no instaladas y fuente recurrente de bugs de transparencia en Wayland (fondo negro con renderer DMA-BUF, requiere variables `WEBKIT_DISABLE_*`).
3. **No resuelve el problema de fondo:** GTK3 sobre Wayland **no puede** hacer always-on-top ni sticky con `xdg_toplevel`. Repetiríamos el fracaso de Theia.

### 2.2. eframe/egui (winit) — ⚠️ Mejor, pero insuficiente por sí solo
- ✅ Frameless y transparente funcionan en Wayland vía winit.
- ✅ Sin WebView, binario único, RSS bajo.
- ❌ **winit no soporta en Wayland** `WindowLevel::AlwaysOnTop`, `set_visible_on_all_workspaces` ni `set_outer_position`: el protocolo `xdg_shell` no lo permite. Es una limitación del protocolo, no de la librería.
- ⚠️ El "15 MB" es optimista: un contexto GL/wgpu carga Mesa y los drivers (RSS típico 40–90 MB). Solo se cumple con renderizado por CPU.

### 2.3. Decisión recomendada — ✅ Rust nativo con backend de superficie por plataforma
```
crates/
  jarvis-core      → máquina de estados, bus de eventos, config (sin I/O de UI)
  jarvis-render    → esfera líquida con tiny-skia (CPU, Pixmap RGBA premultiplicado)
  jarvis-shell     → trait OverlaySurface + backends:
      wayland_layer  → smithay-client-toolkit + zwlr_layer_shell_v1 (layer=Overlay,
                       anchor configurable, exclusive_zone=0, keyboard=OnDemand) + wl_shm ARGB8888
      winit_window   → X11 / Windows / macOS: decorations(false), transparent(true),
                       WindowLevel::AlwaysOnTop, visible_on_all_workspaces donde exista
  jarvis-audio     → (Issue #2) cpal + whisper-rs + TTS
  jarvis-app       → binario: une todo, tray/atajos
```
- **Layer-shell** da gratis: siempre encima, visible en todos los escritorios virtuales, sin decoraciones, posición anclada. Soportado por KWin (verificado), Hyprland (Caelestia), Sway, niri, COSMIC. **No** por GNOME/Mutter → fallback a `winit` vía XWayland (`WINIT_UNIX_BACKEND=x11`) donde `_NET_WM_STATE_ABOVE` + `_NET_WM_STATE_STICKY` sí funcionan.
- **Renderizado CPU (tiny-skia)** para una esfera de ~220×220 px a 60 fps es trivial (<1 ms/frame) y evita cargar drivers GPU → RSS objetivo alcanzable.
- **egui** se reserva para ventanas secundarias bajo demanda (selector de voz, ajustes) como `xdg_toplevel` normal; se crean al abrir y se destruyen al cerrar para no penalizar la RAM en reposo.
- **Windows:** "visible en todos los escritorios virtuales" no tiene API pública (`IVirtualDesktopPinnedApps` es no documentada). Se marca como **riesgo R3**, fuera de Fase 1.

---

## 3. Validación del contrato IPC (`~/.fokkus/jarvis.sock`, JSON-RPC 2.0)

| # | Hallazgo | Severidad | Enmienda |
|---|---|---|---|
| I1 | **Framing no especificado.** JSON-RPC no define cómo delimitar mensajes sobre un stream. | 🔴 Bloqueante | NDJSON: un objeto JSON por línea, UTF-8, `\n` terminador; tamaño máx. 1 MiB por mensaje; si se excede → cerrar conexión. |
| I2 | `notify*` se definen con `id` (requests). Semánticamente son eventos. | 🟠 Alta | `notifyTriageAlert`, `notifySwarmProgress`, `notifySwarmCompleted` como **Notifications** (sin `id`, sin respuesta). |
| I3 | **SDD y Issue #5 no coinciden.** SDD: `ping`, `notifyTriageAlert`, `notifySwarmProgress/Completed`, `ide.dispatchPrompt`, `ide.getWorkspaceContext`. Issue #5 añade `jarvis.dispatchSwarm`, `jarvis.getSwarmStatus` y cambia firmas (`testResults` vs `testsPassed/testsFailed`). | 🟠 Alta | Catálogo único en §3.1 abajo; el SDD es la fuente de verdad. |
| I4 | **Bidireccionalidad ambigua.** El IDE es cliente del socket; ¿cómo llama Jarvis a `ide.*`? | 🟠 Alta | Misma conexión, ambos extremos son peer JSON-RPC. `ide.*` solo disponible si hay sesión IDE registrada (`jarvis.hello`); si no → modo headless (§5 SDD). |
| I5 | **Sin handshake ni versionado.** | 🟡 Media | `jarvis.hello { client, clientVersion, protocolVersion: 1 }` → `{ protocolVersion, capabilities[] }`. Versión mayor distinta → error `-32001`. |
| I6 | **Seguridad del socket no especificada.** Cualquier proceso del sistema podría dictar `ide.dispatchPrompt` (ejecución de agentes = ejecución de código). | 🔴 Bloqueante | `~/.fokkus/` 0700, socket 0600, verificación `SO_PEERCRED` (uid igual al del proceso). Windows: DACL del pipe restringida al SID del usuario. Socket huérfano: intentar `connect`; si falla → `unlink` y re-`bind`; si responde → otra instancia viva → salir. |
| I7 | Ruta `~/.fokkus/jarvis.sock`. | 🟡 Media | Preferir `$XDG_RUNTIME_DIR/fokkus/jarvis.sock` (tmpfs, limpieza automática) con symlink/compatibilidad en `~/.fokkus/jarvis.sock`. |
| I8 | Códigos de error no definidos. | 🟡 Media | Estándar (-32700, -32600, -32601, -32602, -32603) + `-32001 ProtocolVersionMismatch`, `-32002 IdeNotConnected`, `-32003 Busy`. |
| I9 | Frase de alerta con nombre "Juan Carlos" hardcodeado. | 🟢 Baja | `config.user.display_name`. |
| I10 | `~/.fokkus/team_config.json` no existe en el equipo. | 🟢 Baja | Fase 4: definir esquema y ruta; no bloquea Fase 1. |

### 3.1. Catálogo IPC unificado (propuesto, protocolVersion 1)
| Dirección | Método | Tipo |
|---|---|---|
| IDE → Jarvis | `jarvis.hello` | request |
| IDE → Jarvis | `jarvis.ping` | request |
| IDE → Jarvis | `jarvis.getSwarmStatus` | request |
| IDE → Jarvis | `jarvis.notifyTriageAlert` | notification |
| IDE → Jarvis | `jarvis.notifySwarmProgress { taskId, role, percent, message }` | notification |
| IDE → Jarvis | `jarvis.notifySwarmCompleted { taskId, summary, testsPassed, testsFailed }` | notification |
| Jarvis → IDE | `ide.dispatchPrompt { prompt, projectPath, mode }` | request |
| Jarvis → IDE | `ide.getWorkspaceContext` | request |

> `jarvis.dispatchSwarm` del Issue #5 se elimina: el despacho lo inicia Jarvis hacia el IDE (`ide.dispatchPrompt`) o en headless. Si el PO lo quiere conservar, debe documentar quién lo invoca y para qué.

**Nota:** el IPC es Fase 3 (Issue #5). En Fase 1 solo se exige el *esqueleto* de instancia única (lock del socket), para no lanzar dos esferas.

---

## 4. Validación de algoritmos de latencia

### 4.1. Conversational Fillers (< 300 ms) — ⚠️ Viable solo con caché
- **Problema:** `edge-tts` es un servicio remoto no oficial (endpoint de Microsoft Edge con token `Sec-MS-GEC`). TTFB típico 300–900 ms y puede romperse sin aviso. **Sintetizar el relleno en vivo NO cumple < 300 ms.**
- **Algoritmo enmendado:**
  1. **Pre-síntesis:** al iniciar y al cambiar de voz, sintetizar el banco de fillers (≈ 8 por categoría × 3 categorías) y cachear PCM en `~/.cache/fokkus-jarvis/fillers/<voz>/<sha1(texto)>.pcm`. Carga en memoria bajo demanda (~ 30–60 KB c/u a 24 kHz mono i16).
  2. **Disparo:** `t0` = transcripción final emitida por STT. Clasificador por palabras clave (O(n), < 1 ms) → categoría `general | dev | inbox`.
  3. **Selección determinista sin repetición:** índice = `(hash(texto) + contador) mod N`, excluyendo los últimos 2 usados por categoría.
  4. **Reproducción** desde memoria → primera muestra audible a `t0 + < 50 ms` (presupuesto: 300 ms con margen amplio).
  5. **Supresión:** si la respuesta del LLM ya tiene su primera **oración** sintetizada antes de `t0 + 150 ms`, no se emite filler.
  6. **Transición:** la respuesta se sintetiza por oraciones (segmentar en `. ? ! \n`); la primera se encola **al terminar** el filler (no se corta a media palabra), con crossfade de 20 ms.
- **Corrección al SDD §4.1.4:** "en cuanto llega el primer token" no es implementable con TTS de frase completa; la unidad mínima es la **primera oración**.
- **Fallback:** si no hay red → fillers sintetizados con `espeak-ng` (también cacheados).

### 4.2. Voice Barge-In (< 50 ms) — ⚠️ Viable con condiciones
- **Aclaración de presupuesto:** 120 ms de ventana de detección + < 50 ms de corte = **~170 ms desde el inicio del habla del usuario**. Los < 50 ms miden *decisión de VAD → silencio en el altavoz*.
- **Condiciones obligatorias:**
  1. El corte se ejecuta **dentro del callback de salida de cpal**: un `AtomicBool stop_requested` + rampa de ganancia lineal de 15 ms (720 muestras a 48 kHz). Nada de `drop(stream)` ni locks en el hilo de audio.
  2. Buffer de salida ≤ 10 ms (`BufferSize::Fixed(480)` a 48 kHz) y vaciado de la cola del reproductor al cortar; si no, las muestras ya encoladas siguen sonando.
  3. **RMS 0.018 sobre micrófono crudo es inviable con altavoces**: el propio Jarvis supera el umbral. Requiere AEC: `module-echo-cancel` de PipeWire (sistema) o `webrtc-audio-processing` 2.1 (dnf) en proceso con la señal de referencia del altavoz. Con AEC activo, decisión = **RMS ≥ 0.018 Y probabilidad VAD ≥ 0.6** sostenida 120 ms (6 frames de 20 ms).
  4. **Pre-roll:** ring buffer de 300 ms siempre activo, para que la primera sílaba del usuario entre en la transcripción del nuevo comando.
  5. Con auriculares (sin eco) se puede bajar la ventana a 80 ms (configurable).
- **Preservación de tareas:** correcta en el SDD. Barge-in solo afecta al `Player`; la cancelación de tareas es un *intent* explícito del router, nunca un efecto colateral.

### 4.3. Pipeline STT — 🔴 Enmienda obligatoria
- `faster-whisper` es Python + CTranslate2, **no disponible en repos del sistema** → prohibido por la regla de supply chain. Además obligaría a un sidecar Python que rompe el objetivo de RAM y de binario único.
- **Sustituto:** `whisper-rs` (bindings de whisper.cpp) enlazando contra `whisper-cpp-devel` 1.8.1 del sistema, modelo `ggml-base` (cuantizado q5_1 ≈ 60 MB). whisper.cpp ≥ 1.7.6 incluye **VAD Silero nativo** → no requiere ONNX Runtime.
- "Buffer deslizante **WebM**" es un artefacto de `MediaRecorder` del navegador. En nativo: **PCM f32 mono 16 kHz** desde cpal, sin codificar.
- **RAM:** el modelo cargado ocupa ~150–200 MB. El objetivo "≤ 40 MB en reposo" debe redefinirse como **"≤ 40 MB con el modelo STT descargado"**, cargando el modelo de forma perezosa (wake-word / atajo) y liberándolo tras N minutos de inactividad (config). Alternativa: STT en proceso hijo.
- TTS: `msedge-tts` (crate Rust) para Edge; fallback `espeak-ng` del sistema. El TTS neuronal offline "Piper" no está empaquetado en Fedora (el `piper` de dnf es otra herramienta).

---

## 5. Riesgos

| ID | Riesgo | Prob. | Impacto | Mitigación |
|---|---|---|---|---|
| R1 | GNOME no soporta layer-shell | Alta | Medio | Fallback XWayland; documentar |
| R2 | Endpoint `edge-tts` cambia/bloquea | Media | Alto | Caché de fillers + fallback espeak-ng; aislar en trait `TtsEngine` |
| R3 | Sticky en escritorios virtuales de Windows sin API pública | Alta | Bajo | Fuera de Fase 1 |
| R4 | Supply chain de crates.io (misma clase de riesgo que PyPI) | Media | Alto | `Cargo.lock` versionado, `cargo-deny` (licencias/advisories/fuentes), crates mínimos y auditados, preferir enlazar libs del sistema (`whisper-cpp-devel`) |
| R5 | AEC insuficiente → falsos barge-in | Media | Alto | Métrica de falsos positivos en QA; modo auriculares |

---

## 6. Desglose de tareas — Fase 1

Leyenda: **[DS]** DeepSeek V4 Pro (`--model pro`) · **[CL]** Claude PM/QA · **[US]** requiere acción del usuario

### Prerrequisitos
| ID | Tarea | Resp. |
|---|---|---|
| P0 | Aprobar enmiendas de este dictamen (stack nativo sin Tauri, whisper.cpp en vez de faster-whisper, redefinición de RAM) | [US]/PO |
| P1 | `sudo dnf install alsa-lib-devel whisper-cpp-devel webrtc-audio-processing-devel clang-devel` | [US] |
| P2 | Retitular Issue #1 ("Rust/Tauri" → "Rust nativo") y corregir alcance de #2 (whisper.cpp, PCM) | PO |

### Issue #1 — Ventana flotante y núcleo
| ID | Tarea | Resp. | Criterio de aceptación |
|---|---|---|---|
| 1.0 | **Spike (1 día):** layer-shell + wl_shm ARGB en KWin con un círculo tiny-skia; medir RSS | [DS] → [CL] valida | Esfera visible sobre ventanas maximizadas, en los 4 escritorios virtuales, sin marco; RSS < 25 MB |
| 1.1 | Workspace Cargo (`jarvis-core`, `-render`, `-shell`, `-app`), `rust-toolchain.toml` (1.99), `Makefile` (`build`, `test`, `lint`, `run`, `audit`) | [DS] | `make build` y `make test` verdes |
| 1.2 | `jarvis-core`: máquina de estados `Idle → Listening → Thinking → Speaking` (+ `Muted`), transiciones puras y testeables; bus `crossbeam-channel` | [DS] | Tabla de transiciones cubierta 100 % por tests |
| 1.3 | `jarvis-render`: esfera líquida (deformación senoidal multi-armónica, parámetro `level: f32` 0–1 y paleta por estado) | [DS] | Render 220×220 < 2 ms/frame en release (benchmark) |
| 1.4 | `jarvis-shell::wayland_layer` (sctk): Overlay, anchor/margen configurables, `input region` limitada a la esfera (click-through fuera), arrastre para re-anclar | [DS] | Prueba manual KDE + Hyprland |
| 1.5 | `jarvis-shell::winit_window`: X11/Windows/macOS + selección automática de backend (`WAYLAND_DISPLAY` + global `zwlr_layer_shell_v1` presente → layer; si no → winit) | [DS] | Arranca en sesión X11 y en GNOME vía XWayland |
| 1.6 | Instancia única (lock en `$XDG_RUNTIME_DIR/fokkus/jarvis.lock`) y config TOML en `~/.config/fokkus-jarvis/config.toml` | [DS] | Segunda instancia termina con código 0 y aviso |
| 1.7 | QA: `cargo fmt --check`, `clippy -D warnings`, `cargo-deny`, tests de estados y render (snapshot de pixmap con tolerancia) | [CL] | Pipeline `make lint test audit` verde |
| 1.8 | QA: script de medición de RSS en reposo (`/proc/<pid>/status` VmRSS, promedio 60 s) y FPS/CPU | [CL] | Reporte: RSS ≤ 40 MB, CPU reposo < 1 % (esfera en `Idle` a 30 fps o pausada) |

### Issue #2 — Pipeline de audio full-duplex
| ID | Tarea | Resp. | Criterio de aceptación |
|---|---|---|---|
| 2.1 | `jarvis-audio::capture`: cpal input keep-warm, resample a 16 kHz mono f32, ring buffer 30 s + pre-roll 300 ms, medidor RMS → `level` de la esfera | [DS] | Latencia de apertura 0 (stream persistente); test de ring buffer |
| 2.2 | `jarvis-audio::playback`: cpal output, cola de segmentos, `stop()` vía `AtomicBool` + rampa 15 ms en el callback | [DS] | Test: `stop()` → silencio en ≤ 50 ms medido en muestras |
| 2.3 | `jarvis-audio::stt`: `whisper-rs` contra libwhisper del sistema, modelo `ggml-base` q5_1, VAD Silero de whisper.cpp, beam 5, idioma `es`; carga perezosa + descarga por inactividad | [DS] | WER razonable en corpus de 20 frases; RTF < 0.3 en CPU |
| 2.4 | `jarvis-audio::tts`: trait `TtsEngine` con `EdgeTts` (`msedge-tts`, 15 voces, default `es-CL-LorenzoNeural`) y `EspeakTts` (subprocess `espeak-ng --stdout`); fallback automático por timeout 1.5 s | [DS] | Sin red → espeak-ng sin intervención |
| 2.5 | Integración: estados reales (`Listening` por VAD, `Speaking` por playback) conectados al core y a la esfera | [DS] | Demo extremo a extremo con eco ("repite lo que digo") |
| 2.6 | Selector de voz (ventana egui bajo demanda) con preescucha ▶️/⏹️ | [DS] | Ventana se destruye al cerrar; RSS vuelve a la línea base |
| 2.7 | QA: harness con WAV sintéticos (silencio, voz, ruido) inyectados al pipeline (fuente `cpal` mockeada por trait `AudioSource`) | [CL] | Tests deterministas sin hardware en CI |
| 2.8 | QA: medición de latencias reales (VAD fin → texto; `stop()` → silencio) con loopback de PipeWire | [CL] | Reporte con p50/p95 |

> Fillers (#3), Barge-In (#4) e IPC (#5) quedan para Fases 2–3, pero 2.2 (stop en callback) y 2.1 (pre-roll) **se diseñan ya** con esos requisitos para no rehacer el pipeline.

### Reglas para los prompts a DeepSeek
- Un prompt por tarea (1.0, 1.1, …), con: contexto de negocio, crate destino, firmas públicas esperadas, criterios de aceptación de esta tabla y prohibiciones (sin `unwrap()` en código de producción, sin locks ni asignaciones en callbacks de audio, sin crates fuera de la lista aprobada sin consultar a QA).
- Cada entrega pasa por [CL] con `make lint test audit` antes de integrarse.

---

## 7. Conclusión

| Punto | Estado |
|---|---|
| Toolchain Rust 1.99 | ✅ |
| Tauri como stack de UI | ❌ Descartado (RAM, WebKit, no resuelve Wayland) |
| eframe/egui como ventana principal | ⚠️ Insuficiente en Wayland (sin always-on-top/sticky) |
| Rust nativo + layer-shell + tiny-skia (+ egui para modales) | ✅ Recomendado; KWin verificado |
| Contrato IPC JSON-RPC 2.0 | ⚠️ Viable con enmiendas I1–I10 (2 bloqueantes: framing y seguridad) |
| Fillers < 300 ms | ✅ Viable con pre-síntesis en caché (en vivo con edge-tts: ❌) |
| Barge-In < 50 ms | ✅ Viable con corte en callback + AEC + VAD (RMS crudo: ❌) |
| STT `faster-whisper` | ❌ Bloqueado por política de paquetes → `whisper.cpp` (dnf) |
| Objetivo ≤ 40 MB | ⚠️ Solo con modelo STT descargado (redefinir) |
| Tests | No aplica aún: no hay código. Se ejecutarán desde la tarea 1.1 |

**Pendiente para iniciar implementación:** P0 (aprobación de enmiendas) y P1 (instalar dependencias del sistema).
