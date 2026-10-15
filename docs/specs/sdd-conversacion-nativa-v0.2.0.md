# Contrato SDD: Conversación Natural Nativa (Fokkus-Jarvis v0.2.0)

**Proyecto:** Fokkus-Jarvis
**Solicitante:** Juan Carlos (Architect / dueño del producto)
**Redactado por:** Claude Code (PM/QA), con análisis de ciberseguridad previo
**Destinatario:** Swarm de desarrollo (PO · PM/QA · Dev Senior)
**Versión del documento:** 1.0.0
**Fecha:** 2026-10-15
**Estado:** Aprobado por el PO (2026-10-15)
**Tag objetivo:** `v0.2.0`
**Documento padre:** [`sdd-fokkus-jarvis.md`](sdd-fokkus-jarvis.md) (este contrato lo amplía; donde haya conflicto, prevalece este contrato para el alcance de v0.2.0)

---

## 0. Cláusula principal: entrega completa, no MVP

Este contrato pide una **funcionalidad terminada y usable a diario**, no un prototipo ni un MVP. Las reglas de aceptación son estrictas:

1. **El tag `v0.2.0` solo se crea cuando TODAS las historias de usuario (HU-0 a HU-12) cumplen TODOS sus criterios de aceptación.** No se aceptan entregas parciales bajo este tag. Si algo no se puede terminar, se detiene el release y se informa al PO. No se recorta alcance en silencio.
2. **Prohibido simular éxito.** No se aceptan *stubs*, superficies *headless* que aparentan funcionar, `todo!()`, `unimplemented!()`, `// TODO` en rutas de producción, datos de ejemplo cableados, ni mensajes que anuncien un estado que el programa no verificó. La versión 0.1.2 imprimía "Socket IPC activo" justo antes de terminar el proceso; ese tipo de defecto descalifica la entrega.
3. **"Funciona" significa en el equipo real del usuario:** Fedora 44, KDE Plasma con KWin Wayland, shell Caelestia, PipeWire 1.6, RX 7900 GRE. Instalado desde el RPM del release y lanzado desde el menú de Caelestia. Los tests unitarios en verde son necesarios, pero no bastan.
4. **Prueba de aceptación de punta a punta** (sección 9) ejecutada y documentada, con evidencia (logs, métricas, grabación o capturas), antes de proponer el tag.
5. Toda dependencia nueva fuera de la sección 4 requiere un análisis de seguridad con el mismo formato de la sección 3 y aprobación explícita del PO **antes** de agregarse a `Cargo.toml`.

---

## 1. Contexto y diagnóstico del estado actual (verificado el 2026-10-15 sobre v0.1.3)

v0.1.3 corrigió el bucle principal y el overlay Wayland: la esfera aparece y el socket IPC queda activo. Pero **la conversación por voz todavía no existe de punta a punta**. Esto es lo que se verificó leyendo el código y ejecutando el binario instalado:

| Capacidad | Estado real en v0.1.3 | Evidencia |
|---|---|---|
| Captura de micrófono | ❌ No existe. `jarvis-audio` no tiene ninguna dependencia de audio | `crates/jarvis-audio/Cargo.toml` solo depende de `jarvis-core` |
| Reproducción de audio | ❌ No existe. `AudioPlayback` solo expone una señal de parada; el "callback cpal (tarea 2.2)" nunca se implementó | `crates/jarvis-audio/src/playback.rs:1-7` |
| STT | ⚠️ Invoca un binario externo `whisper-cli`/`whisper-cpp` que el RPM no instala ni declara. El paquete `whisper-cpp` de Fedora **no trae** ese binario (solo `libwhisper`/`libggml`) | `stt.rs:15`, `dnf repoquery` |
| Modelo STT | ⚠️ Espera `~/.cache/fokkus-jarvis/models/ggml-base-q5_1.bin`; nadie lo descarga ni verifica | `stt.rs:139-153` |
| TTS | ⚠️ Invoca el CLI de Python `edge-tts` (dependencia externa no declarada); el fallback `espeak-ng` es robótico | `tts.rs:237-272` |
| VAD / fin de turno | ❌ No existe (el SDD exige Silero VAD) | — |
| Cancelación de eco (AEC) | ❌ No existe (el SDD exige AEC para barge-in) | — |
| Cerebro conversacional (LLM) | ❌ No existe ningún cliente LLM; el estado `Thinking` no tiene a quién consultar | `grep` sin resultados en `crates/*/src` |
| `cargo-deny` | ❌ El backlog marca la tarea 1.7 como hecha, pero **no hay `deny.toml`** en el repo y la herramienta no está instalada | `ls deny.toml` |
| Firma del RPM | ❌ `Signature: (none)`; dnf omite la verificación OpenPGP | `rpm -qip` |
| Licencia | ⚠️ El RPM declara `License: Proprietary` y el README declara MIT | `rpm -qip`, `README.md` |
| README | ⚠️ Sigue anunciando Tauri y `faster-whisper`, ambos reemplazados por el dictamen QA | `README.md` |

**Conclusión:** para que la conversación sea natural y fluida hay que incorporar **de forma nativa** (enlazada al binario Rust, sin depender de CLIs externos) la cadena completa: audio de entrada y salida → limpieza de señal → detección de voz → activación → transcripción → razonamiento con streaming → síntesis con streaming → reproducción interrumpible.

---

## 2. Objetivo del producto

Al terminar v0.2.0, el usuario debe poder:

1. Decir **"Jarvis, …"** con el PC funcionando normalmente (música, videollamada o juego de fondo) y que Jarvis lo escuche sin tocar nada.
2. Recibir una **confirmación audible en menos de 300 ms** (filler) y la **respuesta real empezando a sonar en menos de 1,5 s** (p50) desde que termina de hablar.
3. **Interrumpir a Jarvis hablando encima**: Jarvis se calla en menos de 50 ms y escucha la nueva instrucción.
4. **Seguir la conversación sin repetir "Jarvis"** durante una ventana de seguimiento, con memoria de lo dicho en la sesión.
5. Seguir usando Jarvis **sin internet** con voz neuronal local (degradado, pero natural y no robótico) y recibir un aviso claro de que el razonamiento en la nube no está disponible.
6. Confiar en que **el audio nunca sale del equipo** y en que todo modelo descargado está verificado criptográficamente.

---

## 3. Análisis de ciberseguridad de las librerías (Fase 0, bloqueante)

### 3.1. Metodología aplicada

El análisis se hizo el 2026-10-15 sin compilar ni ejecutar código de terceros:

1. **Resolución del árbol completo** con `cargo generate-lockfile` sobre un proyecto de prueba con el stack actual más las candidatas: **374 paquetes de crates.io**.
2. **Cruce de cada paquete y versión con OSV.dev** (agrupa RustSec Advisory DB y GitHub Security Advisories): **0 vulnerabilidades conocidas**. El `Cargo.lock` actual del repo (71 paquetes) también dio 0.
3. **Licencias** de los 374 paquetes vía `cargo metadata`.
4. **Componentes nativos** (`links =`): revisión de `build.rs` en busca de descargas en tiempo de compilación.
5. **Revisión de código fuente** de las candidatas: conteo de `unsafe`, TLS sin verificar, ejecución de procesos externos, endpoints de red cableados.
6. **Investigación de CVEs del código C/C++ vendorizado**, que OSV **no** cubre porque va dentro de crates `-sys`.
7. **Mantención**: fecha de la última versión, descargas recientes y repositorio.

### 3.2. Hallazgos críticos (obligatorio resolverlos)

| ID | Severidad | Hallazgo | Mitigación exigida |
|---|---|---|---|
| **SEC-01** | **Alta** | `whisper-rs-sys 0.15.0` (la última publicada) **vendoriza whisper.cpp 1.8.3**. Esa versión está afectada por fallas corregidas en versiones posteriores: lectura fuera de límites en `log_mel_spectrogram` con audio muy corto (corregida en 1.9.3, #3956); *stack buffer overflow* con `n_dims` inválido en modelos malformados (1.9.3, #3957); endurecimiento del cargador GGUF (1.9.3); overflow de enteros en ggml/gguf (1.9.5); y **CVE-2026-110810**, lectura fuera de límites en `whisper_model_load` vía `ttype`, con exploit público (corregida en **1.9.5**, commit `60c0be6`). El paquete `whisper-cpp` de Fedora (1.8.1) también está desactualizado. | Compilar contra **whisper.cpp ≥ 1.9.5**: `[patch.crates-io]` hacia un fork de `whisper-rs-sys` con el submódulo fijado al commit del tag `v1.9.5` (o posterior), o una versión upstream de `whisper-rs` que ya lo incluya. Agregar en CI una verificación que falle si la versión vendorizada es < 1.9.5. |
| **SEC-02** | **Alta** | **Issue #4059 de whisper.cpp, sin parche publicado al 2026-10-15:** un modelo con `n_vocab` entre 50258 y 50363 provoca escritura fuera de límites en el heap (`whisper_process_logits`, CVSS 7.1). Cualquier archivo de modelo no confiable es un vector de ejecución de código. | (a) Cargar **solo** modelos cuyo SHA-256 esté fijado en el código (HU-7). (b) Validar en Rust la cabecera del modelo **antes** de pasarlo a whisper.cpp y rechazar `n_vocab` < 51864. (c) Prohibido cargar modelos desde rutas arbitrarias del usuario o desde variables de entorno sin hash; la variable actual `WHISPER_MODEL` (`stt.rs:142`) se elimina o queda sujeta a la misma verificación. (d) Seguir #4059 / PR #4097 y actualizar cuando se publique. |
| **SEC-03** | Media | El árbol incluye **dos proveedores criptográficos TLS** a la vez: `ring` (vía `ureq`) y `aws-lc-rs`/`aws-lc-sys` (vía las features por defecto de `rustls` que activa `msedge-tts`). Duplica superficie de ataque y código C/ensamblador. | Un único proveedor: **`ring`**. Desactivar las features por defecto que traen `aws-lc-rs` (si `msedge-tts` no lo permite, PR upstream o `[patch]`). Gate de CI: `cargo tree -i aws-lc-sys` debe salir vacío. |
| **SEC-04** | Media | **Edge TTS es una API no oficial obtenida por ingeniería inversa** (`speech.platform.bing.com`, *TrustedClientToken* cableado y token `Sec-MS-GEC` rotativo). Microsoft ya la bloqueó temporalmente (reportes de diciembre de 2025) y cambia el token según el ciclo de versiones de Edge. Riesgo de disponibilidad y de cumplimiento de términos, y el **texto de las respuestas** se envía a Microsoft. | Edge se mantiene como voz principal por calidad (es-CL Lorenzo), pero es **obligatorio** un TTS neuronal **offline** (Piper, HU-8) con conmutación automática en < 1 s ante error 401/403/timeout. Documentar en el README que el texto hablado se envía a Microsoft y ofrecer el modo `tts = "local"` en la configuración. |
| **SEC-05** | Media | La configuración de `cargo-deny` que el backlog daba por hecha **no existe**. No hay ningún gate automático de advisories, licencias ni fuentes. | Crear `deny.toml` (sección 8) y ejecutarlo en CI y en `make lint`. |
| **SEC-06** | Media | El RPM se distribuye **sin firma** y sin `SHA256SUMS`. Cualquiera que reemplace el archivo en el canal de distribución puede ejecutar código como root al instalarlo. | Firmar el RPM con GPG (`rpm --addsign`), publicar la clave pública y un `SHA256SUMS` firmado en el release. |
| **SEC-07** | Baja | `ort` (para Piper) solo existe como **release candidate** (`2.0.0-rc.14`). Su feature por defecto `download-binaries` **descarga onnxruntime precompilado desde un CDN de terceros durante la compilación** (riesgo de cadena de suministro). | Usar `ort` con `default-features = false` y feature `load-dynamic`, enlazando el **onnxruntime 1.22.2 del repositorio oficial de Fedora** (`Requires: onnxruntime` en el RPM). Fijar la versión exacta de `ort` compatible con la API 1.22 y documentarla. Prohibido `download-binaries`. |
| **SEC-08** | Baja | Hay mucho `unsafe` en crates de audio: `cpal` (203 líneas), `ringbuf` (196), `sonora-simd` (136), `whisper-rs` (128), `rubato` (52). Es normal (FFI e intrínsecos SIMD) y no tiene advisories, pero es superficie de memoria. | El código propio sigue con **cero `unsafe`**. Tests de robustez de la cadena de audio con entradas extremas (silencio, saturación, NaN, frames de largo 0 y 1, tasas de muestreo raras), 10 minutos sin pánicos ni lecturas inválidas, con un build de ASan/Valgrind sobre la integración con whisper.cpp. |
| **SEC-09** | Baja | `symphonia` (decodificador MP3) es **MPL-2.0**, copyleft débil a nivel de archivo, y las raíces `webpki-roots` son CDLA-Permissive-2.0. Hay que tenerlas presentes al definir la licencia del producto. | Las dos se permiten en `deny.toml`. Si se modifican archivos de `symphonia`, publicar esos cambios. Resolver la incoherencia Proprietary vs MIT (decisión D-1). |

### 3.3. Veredicto por librería

| Capacidad | Librería (versión) | Licencia | Mantención | Advisories (OSV) | Riesgos / notas | Veredicto |
|---|---|---|---|---|---|---|
| Audio E/S | `cpal` 0.18.2 | Apache-2.0 | Activa (2026-08), 8,3 M descargas recientes | 0 | Backend ALSA (vía `pipewire-alsa`); necesita `alsa-lib-devel`. Sin red ni procesos | ✅ Aprobada |
| Buffer lock-free | `ringbuf` 0.5.3 | MIT/Apache-2.0 | Activa (2026-10) | 0 | `unsafe` interno, muy usada | ✅ Aprobada |
| Remuestreo | `rubato` 5.0.1 | MIT/Apache-2.0 | Activa (2026-10) | 0 | Rust puro | ✅ Aprobada |
| VAD | `earshot` 1.2.2 | MIT/Apache-2.0 | Activa (2026-08), de pykeio (autores de `ort`) | 0 | **Cero dependencias**, Rust puro, red neuronal embebida de ~40 KiB, ~8 KiB por detector. En un benchmark de terceros tiene menos *recall* que Silero (82 % vs 90 %), así que hay que calibrarla (HU-3) | ✅ Aprobada |
| AEC + NS + AGC | `sonora` 0.2.0 | BSD-3-Clause | Activa (2026-07), 725 k descargas recientes | 0 | Port **Rust puro** de WebRTC APM (AEC3, NS, AGC2). Joven (0.x) | ✅ Aprobada, con plan B |
| STT | `whisper-rs` 0.16.0 + whisper.cpp **≥ 1.9.5** | Unlicense + MIT | Activa | 0 en crates.io; **varios CVE en el C++ vendorizado** | Ver SEC-01 y SEC-02. Sin descargas en `build.rs` (verificado). Necesita `cmake` y `clang` para compilar | ✅ Aprobada **condicionada** a SEC-01 y SEC-02 |
| TTS en línea | `msedge-tts` 0.4.1 (feature `blocking`) | MIT/Apache-2.0 | Muy activa (2026-10-14, sigue la rotación de tokens) | 0 | API no oficial (SEC-04); TLS verificado (no hay `danger`/`NoCertificateVerification`); no ejecuta procesos | ✅ Aprobada **condicionada** a SEC-03 y SEC-04 |
| Decodificación MP3 | `symphonia` 0.6.1 (solo feature `mp3`) | MPL-2.0 | Activa | 0 | `#![forbid(unsafe_code)]` en el decodificador MP3 | ✅ Aprobada |
| TTS offline | Piper vía `ort` (`load-dynamic`) + onnxruntime 1.22.2 de Fedora | MIT/Apache-2.0 | `ort` activa, pero en RC | 0 | Ver SEC-07. El modelo de voz se fija por SHA-256 y su licencia se verifica en la *model card* | ✅ Aprobada **condicionada** a SEC-07 |
| HTTP (LLM y modelos) | `ureq` 3.4.2 (`rustls` + `ring`, `json`) | MIT/Apache-2.0 | Muy activa | 0 | Síncrono, coherente con la política actual de "sin tokio" | ✅ Aprobada |
| Secretos | `keyring` 4.2.0 (Secret Service vía `zbus`, Rust puro) | MIT/Apache-2.0 | Activa | 0 | Guarda la API key en KWallet/Secret Service, no en texto plano | ✅ Aprobada |
| Integridad | `sha2` 0.11.1 | MIT/Apache-2.0 | RustCrypto | 0 | — | ✅ Aprobada |

### 3.4. Alternativas evaluadas y descartadas

| Librería | Motivo del descarte |
|---|---|
| `voice_activity_detector` 0.2.1 | Fija `ort =2.0.0-rc.10` (versión antigua) y arrastra onnxruntime solo para el VAD; licencia no estándar |
| `webrtc-vad` 0.4.0 | Sin versiones desde 2019 |
| `silero-vad-rs` 0.1.2 | Uso marginal (383 descargas recientes) y sin mantención desde 2025-04 |
| `rustpotter` 3.0.2 (*wake word*) | Sin versiones desde 2023-10 y con muy poco uso |
| `edge-tts-rust` 0.1.6 | Arrastra `tokio`, `reqwest` 0.12 y `clap` en la librería; 3,9 k descargas recientes |
| `webrtc-audio-processing` 2.1.0 | Envuelve C++ por FFI. Queda como **plan B de AEC** si `sonora` no cumple HU-2, enlazando la biblioteca del sistema (`webrtc-audio-processing` 2.1 de Fedora, la misma que usa PipeWire) y nunca la variante `bundled` |
| `speexdsp` 0.1.2 | Sin mantención desde 2022 |
| `sherpa-rs` / `sherpa-onnx` | Duplican la funcionalidad de whisper-rs + Piper con un árbol nativo mucho mayor |
| CLIs externos (`whisper-cli`, `edge-tts` de Python) | Se **eliminan**: no se declaran en el RPM, quedan fuera de la verificación de dependencias y agregan dos runtimes (Python y binarios de terceros) a la superficie de ataque |

---

## 4. Stack técnico aprobado para v0.2.0

```toml
# Workspace — dependencias nuevas aprobadas por este contrato (versiones mínimas fijadas).
cpal       = "0.18.2"
ringbuf    = "0.5.3"
rubato     = "5.0.1"
earshot    = "1.2.2"
sonora     = "0.2.0"
whisper-rs = "0.16.0"   # + [patch.crates-io] whisper-rs-sys → whisper.cpp ≥ 1.9.5 (SEC-01)
msedge-tts = { version = "0.4.1", default-features = false, features = ["blocking"] }  # proveedor TLS: ring (SEC-03)
symphonia  = { version = "0.6.1", default-features = false, features = ["mp3"] }
ort        = { version = "=<versión compatible con onnxruntime 1.22>", default-features = false, features = ["load-dynamic"] }  # SEC-07
ureq       = { version = "3.4.2", features = ["json"] }  # rustls + ring
keyring    = "4.2.0"
sha2       = "0.11.1"
```

**Reglas del stack:**
- Aceleración GPU de whisper (`hipblas`/`vulkan`) **opcional** y **apagada por defecto** en el RPM. Solo se activa si se demuestra que hace falta para cumplir las latencias de la sección 7; en ese caso se documenta y se declara como dependencia del RPM.
- Nada de `tokio`, `reqwest`, `openssl`, `native-tls` ni `aws-lc` en el árbol (gate en `deny.toml`).
- Se mantiene **cero `unsafe`** en el código propio y sin `unwrap()`/`expect()` fuera de los tests (reglas del brief previo).

---

## 5. Arquitectura del pipeline conversacional

```
 Micrófono (PipeWire→ALSA)                                   Altavoces
      │ cpal input 48 kHz                                        ▲ cpal output 48 kHz
      ▼                                                          │
 ┌──────────┐  ringbuf   ┌──────────┐   referencia de salida   ┌┴─────────────┐
 │ Captura  │──────────►│ rubato   │◄─────────────────────────│ Mezclador     │◄── fade-out 15 ms
 └──────────┘ (lock-free)│ →16 kHz  │                           │ + stop atómico│    (barge-in)
                         └────┬─────┘                           └──────▲───────┘
                              ▼                                        │ PCM
                 ┌────────────────────────┐                   ┌────────┴─────────┐
                 │ sonora: AEC3+NS+AGC2   │                   │ symphonia (MP3)  │
                 └───────────┬────────────┘                   │ / PCM Piper      │
                             ▼                                └────────▲─────────┘
                 ┌────────────────────────┐   voz durante              │
                 │ earshot VAD (16 ms)    │── speaking ──► BARGE-IN ───┤ cancela LLM+TTS
                 └───────────┬────────────┘                            │
                             ▼ segmento                       ┌────────┴─────────┐
                 ┌────────────────────────┐                   │ TTS streaming    │
                 │ Activación: "Jarvis"   │                   │ Edge (msedge-tts)│
                 │ o ventana de seguim.   │                   │ ↘ Piper offline  │
                 └───────────┬────────────┘                   └────────▲─────────┘
                             ▼ fin de turno                            │ frases
                 ┌────────────────────────┐   texto   ┌───────────────┴──────────┐
                 │ whisper-rs (≥1.9.5)    │──────────►│ Orquestador conversación  │
                 │ modelo verificado SHA  │  filler ◄─│ + cliente LLM SSE (ureq)  │
                 └────────────────────────┘ (<300 ms) │ + memoria de sesión       │
                                                      └───────────────────────────┘
```

**Hilos:** (1) callback de entrada cpal, solo copia al ringbuf, sin locks ni allocations; (2) DSP + VAD; (3) STT; (4) LLM + segmentador de frases; (5) TTS + decodificación; (6) callback de salida cpal. La comunicación usa `crossbeam-channel` (ya aprobado) y el estado vive en **un único `JarvisCore` compartido**, lo que cierra la deuda técnica "Unificar `JarvisCore`" del backlog.

---

## 6. Historias de usuario

> Formato por HU: objetivo, alcance técnico, criterios de aceptación (CA). **Todos los CA son obligatorios.**

### HU-0 — Gates de seguridad y cadena de suministro (se hace primero)
- **Alcance:** `deny.toml` (sección 8); `make audit` real (instalar `cargo-deny` y `cargo-audit` en CI); verificación de la versión de whisper.cpp vendorizada; SBOM CycloneDX (`cargo-cyclonedx`) por release; firma GPG del RPM y `SHA256SUMS` firmado.
- **CA-0.1:** `cargo deny check` pasa en CI y en `make lint`, con las bans de la sección 8.
- **CA-0.2:** `cargo tree -i aws-lc-sys`, `-i openssl-sys`, `-i tokio` y `-i reqwest` salen vacíos.
- **CA-0.3:** CI falla si whisper.cpp vendorizado es < 1.9.5 (prueba con una versión mala incluida en el PR).
- **CA-0.4:** el release publica RPM firmado (`rpm -K` muestra la firma OK), `SHA256SUMS(.asc)` y `sbom.cdx.json`.
- **CA-0.5:** se resuelven SEC-01 a SEC-09 (sección 3.2), cada uno con su evidencia en el informe de seguridad del release.

### HU-1 — Captura y reproducción de audio nativas
- **Alcance:** `cpal` entrada y salida, selección de dispositivo (por defecto el del sistema, configurable por nombre en TOML), reconexión en caliente si el dispositivo desaparece (por ejemplo, audífonos USB), mezclador de salida con `AudioPlayback` real (el `AtomicBool` y la generación existentes pasan a usarse dentro del callback).
- **CA-1.1:** captura continua 8 h sin *xruns* acumulados > 0,1 % ni crecimiento de memoria.
- **CA-1.2:** desconectar y reconectar el dispositivo de entrada o salida se recupera solo en < 3 s, sin reiniciar el proceso.
- **CA-1.3:** los callbacks de audio no bloquean, no hacen allocations y no toman locks (revisión de código + test con `assert_no_alloc` o equivalente).
- **CA-1.4:** funciona junto a EasyEffects (perfil `Voz_Locutor` activo) usando la fuente procesada como micrófono por defecto.

### HU-2 — Limpieza de señal: AEC, supresión de ruido y AGC
- **Alcance:** `sonora` (AEC3 + NS + AGC2) a 16 kHz, en frames de 10 ms. La señal de referencia es la salida real del mezclador, alineada en el tiempo.
- **CA-2.1:** con Jarvis hablando por **altavoces** a volumen normal, su propia voz **no** dispara el VAD (0 auto-interrupciones en 50 locuciones de prueba).
- **CA-2.2:** con altavoces, el usuario sí puede interrumpir (ver HU-9).
- **CA-2.3:** si `sonora` no cumple CA-2.1, se activa el plan B (`webrtc-audio-processing` del sistema) **dentro de este mismo release**, sin bajar el criterio.

### HU-3 — Detección de voz y fin de turno
- **Alcance:** `earshot` sobre frames de 16 ms; *endpointing* configurable (silencio de cierre por defecto 700 ms, con pre-roll de 300 ms que el SDD ya pedía); fin de turno adaptativo: si la frase termina en conector ("y…", "pero…", "entonces…") según la transcripción parcial, se extiende la espera hasta 1,2 s.
- **CA-3.1:** el corpus de prueba de la sección 9 (es-CL, con ruido de fondo de ventilador y música baja) da *recall* de voz ≥ 95 % y falsos positivos ≤ 5 % por frame. Si `earshot` no llega, se ajusta el umbral o se suma un suavizado *hangover*; el criterio no se rebaja.
- **CA-3.2:** no se corta al usuario en pausas naturales de < 600 ms dentro de una frase (0 cortes en 30 frases largas del corpus).

### HU-4 — Activación por voz, ventana de seguimiento y privacidad del micrófono
- **Alcance:**
  - Palabra de activación **"Jarvis"**, detectada con whisper sobre segmentos cortos ya filtrados por el VAD, usando un **modelo pequeño dedicado** (`ggml-tiny` o `base` cuantizado, fijado por SHA-256) y *prompt* inicial sesgado a "Jarvis". No se agrega una librería de *wake word* (ver descartes).
  - Tras cada respuesta queda abierta una **ventana de seguimiento** de 8 s (configurable) en la que no hace falta repetir "Jarvis".
  - Activación alternativa: atajo global de KDE (registrado vía D-Bus / `kglobalaccel`) y método IPC `jarvis.listen`.
  - **Modo silencio** (`jarvis.mute` por IPC, atajo y voz: "Jarvis, deja de escuchar"): cierra la captura del micrófono y la esfera lo muestra.
- **CA-4.1:** tasa de activación correcta ≥ 90 % en el corpus y **≤ 1 activación falsa por hora** con 1 h de audio de videollamada, YouTube y juego de fondo.
- **CA-4.2:** fuera de la ventana de seguimiento, **ningún audio pasa a la transcripción completa ni sale hacia el LLM** sin activación (verificado por log y test).
- **CA-4.3:** en modo silencio, el dispositivo de captura se libera (PipeWire deja de mostrar a Jarvis como cliente de grabación).

### HU-5 — Transcripción nativa (whisper-rs)
- **Alcance:** `whisper-rs` enlazado, contexto cargado una vez y reutilizado, idioma `es`, modelo principal `ggml-base-q5_1` (o `small-q5_1` si la latencia lo permite), transcripción del segmento al detectar fin de turno, más parciales para el *endpointing* adaptativo de HU-3. Se elimina el código que invoca `whisper-cli`.
- **CA-5.1:** WER ≤ 15 % sobre el corpus es-CL de la sección 9.
- **CA-5.2:** latencia fin de turno → texto final ≤ 400 ms p50 en CPU para frases de ≤ 8 s.
- **CA-5.3:** audio de 0 ms, 10 ms y 30 s se procesan sin pánicos ni lecturas inválidas (test de regresión de SEC-01).
- **CA-5.4:** `--check` informa el backend STT como `whisper-rs (whisper.cpp 1.9.x) · modelo verificado`.

### HU-6 — Cerebro conversacional con streaming (Claude Messages API)
- **Alcance:**
  - Cliente HTTP síncrono con `ureq` (rustls/ring) contra `POST https://api.anthropic.com/v1/messages`, cabeceras `x-api-key`, `anthropic-version: 2023-06-01` y `content-type: application/json`, con `"stream": true`.
  - Parser SSE propio y robusto: `message_start`, `content_block_start`, `content_block_delta` (`text_delta`), `content_block_stop`, `message_delta` (con `stop_reason`), `message_stop`, `ping` y `error`. Ignorar con tolerancia los tipos de evento y bloque desconocidos (incluidos los bloques `thinking`).
  - Modelo por defecto **`claude-opus-5-5`** con `output_config: {"effort": "low"}` para minimizar latencia en voz; modelo y *effort* configurables en TOML. No enviar `thinking: {type: "disabled"}` ni `budget_tokens` (este modelo los rechaza con 400).
  - Manejo de rechazos: revisar `stop_reason` y tratar `"refusal"` con una respuesta hablada neutra. Incluir `fallbacks: "default"` con la cabecera `anthropic-beta: server-side-fallback-2026-07-01`.
  - *System prompt* fijo (persona Jarvis, respuestas breves y conversacionales en español de Chile, sin markdown ni listas porque se van a hablar) con `cache_control: {"type": "ephemeral"}` en el último bloque estable. Nada variable (hora, fecha) antes del punto de caché. Verificar aciertos con `usage.cache_read_input_tokens`.
  - Memoria de sesión: historial de turnos en memoria, solo agregando al final (no se reescriben turnos previos), con límite configurable de turnos o tokens. Al cerrar la app no se persiste, salvo que el usuario lo habilite.
  - **Cancelación:** un barge-in aborta la petición en curso (se cierra la conexión) y en el historial queda solo el texto que alcanzó a hablarse.
  - API key en el llavero del sistema (`keyring`); comando `fokkus-jarvis --set-api-key` que la lee por stdin sin eco. **Prohibido** guardarla en el TOML o en logs.
  - Reintentos con *backoff* solo en 429, 5xx y errores de red; 400 y 401 no se reintentan y se informan por voz con un mensaje claro.
  - Sin red o sin key: Jarvis lo dice en voz alta y sigue operativo para comandos locales (mute, estado del swarm vía IPC, repetir, hora).
- **CA-6.1:** primer token de texto ≤ 900 ms p50 desde el envío (red doméstica normal).
- **CA-6.2:** segunda petición y siguientes con `cache_read_input_tokens > 0`.
- **CA-6.3:** la API key no aparece en ningún archivo bajo `~/.config`, `~/.cache`, logs ni en `ps` (test automatizado con `grep` sobre esas rutas).
- **CA-6.4:** desconectar la red en medio de una respuesta produce un aviso hablado y vuelve a `Listening` en < 2 s, sin cuelgues.
- **CA-6.5:** tests del parser SSE con eventos troceados en límites arbitrarios de bytes, eventos desconocidos, `error` a mitad de stream y `refusal`.

### HU-7 — Gestor de modelos seguro
- **Alcance:** descarga y verificación de los modelos whisper (principal y activación) y de la voz Piper. Las URLs HTTPS y los **SHA-256 quedan fijados en el código**; las URLs de Hugging Face apuntan a un **commit concreto**, no a `main`. Descarga a archivo temporal en el mismo directorio, verificación del hash y luego `rename` atómico. Límite de tamaño por modelo; permisos 0600 y directorio 0700 (`~/.local/share/fokkus-jarvis/models/`). Validación de cabecera (SEC-02) antes de cargar. Barra de progreso en la esfera y por IPC (`jarvis.modelStatus`). Primera ejecución: descarga automática con aviso hablado (con espeak-ng si todavía no hay voz neuronal).
- **CA-7.1:** un modelo con un byte alterado se rechaza, se borra y se informa; jamás se carga.
- **CA-7.2:** un modelo con `n_vocab` inválido se rechaza antes de llamar a whisper.cpp (test con un archivo sintético).
- **CA-7.3:** si la descarga se corta, se reanuda o reintenta sin dejar archivos corruptos en la ruta final.
- **CA-7.4:** `fokkus-jarvis --check` muestra el estado y el hash de cada modelo.

### HU-8 — Síntesis de voz nativa con streaming y respaldo offline
- **Alcance:**
  - **Segmentador de frases:** convierte el stream de texto del LLM en frases completas (puntuación, longitud máxima, sin cortar números ni abreviaturas comunes del español) y sintetiza la frase 1 mientras sigue llegando la 2.
  - **Edge en línea:** `msedge-tts` (síncrono) con la voz configurada (`es-CL-LorenzoNeural` por defecto), decodificación MP3 con `symphonia`, remuestreo a la tasa del dispositivo con `rubato`. Se elimina la invocación al CLI `edge-tts`. La caché de fillers existente se regenera con el motor nativo.
  - **Piper offline:** voz en español (preferir es-MX o es-AR por cercanía; documentar cuál y su licencia), fijada por SHA-256, inferencia con `ort` + onnxruntime del sistema. Conmutación automática ante 401/403/timeout > 1,5 s de Edge, y modo forzado `tts = "local"`.
  - `espeak-ng` queda solo como último recurso si Piper no puede cargar.
- **CA-8.1:** primer audio de la respuesta ≤ 1,5 s p50 desde el fin de turno del usuario (de punta a punta, con Edge).
- **CA-8.2:** sin internet, la respuesta suena con Piper (no espeak) y el primer audio llega ≤ 1,2 s p50 tras recibir el texto.
- **CA-8.3:** no hay cortes audibles entre frases consecutivas (silencio entre frases ≤ 150 ms).
- **CA-8.4:** con un fallo forzado de Edge (por ejemplo, un DNS bloqueado en el test), la frase en curso se completa con Piper sin perder contenido.

### HU-9 — Conversación de punta a punta, barge-in real y estados de la esfera
- **Alcance:** conectar todo lo anterior al `ConversationCoordinator` existente, con un único `JarvisCore` compartido. El filler se dispara en paralelo con la petición al LLM (como pide el SDD §4.1). El barge-in pasa de simulación a realidad: VAD durante `Speaking` (post-AEC) → stop atómico en el callback (fade-out de 15 ms) → cancelación del LLM y del TTS pendientes → `Listening`. Se preserva incondicionalmente `BackgroundTasks` (SDD §4.2). La esfera refleja `Idle`, `Listening`, `Thinking`, `Speaking`, `Muted` y `Offline` con colores o animaciones distintas, y su nivel de audio es el RMS real del micrófono o de la salida.
- **CA-9.1:** barge-in: voz del usuario → silencio de Jarvis ≤ 50 ms p95 (medido con *loopback*).
- **CA-9.2:** filler audible ≤ 300 ms p95 desde el fin de turno.
- **CA-9.3:** 20 conversaciones de 5 turnos seguidos (con ventana de seguimiento) sin bloqueos, sin estados inconsistentes de la esfera y sin fugas de memoria.
- **CA-9.4:** una interrupción durante una tarea del swarm no cancela la tarea (test de integración vía IPC).

### HU-10 — Privacidad y seguridad en tiempo de ejecución
- **Alcance:** el audio **nunca** sale del equipo ni se guarda en disco por defecto (modo depuración explícito con aviso y borrado automático a las 24 h). Los logs no contienen transcripciones ni respuestas salvo en el nivel `debug` activado manualmente. Lo que sale a terceros se documenta: transcripción y contexto hacia Anthropic, texto a hablar hacia Microsoft (Edge). El socket IPC queda en 0600 y se verifica `SO_PEERCRED` (dictamen QA §3). Los métodos IPC nuevos (`jarvis.listen`, `jarvis.mute`, `jarvis.modelStatus`) siguen el catálogo único del protocolo.
- **CA-10.1:** después de 1 h de uso no hay archivos de audio nuevos en disco (`find ~ -newer … -name '*.wav' -o -name '*.pcm'`), salvo la caché de fillers.
- **CA-10.2:** en el nivel de log por defecto, `journalctl --user -u fokkus-jarvis` no contiene ninguna frase dicha por el usuario.
- **CA-10.3:** la sección "Privacidad" del README describe exactamente qué datos salen, hacia quién y cómo desactivarlo.

### HU-11 — Configuración, diagnóstico y experiencia de primera ejecución
- **Alcance:** nuevas claves TOML (`input_device`, `output_device`, `wake_word_enabled`, `follow_up_seconds`, `endpoint_silence_ms`, `llm_model`, `llm_effort`, `tts`, `piper_voice`, `history_turns`), con valores por defecto sensatos y validación (un valor inválido se informa y usa el defecto). `--check` cubre todos los subsistemas nuevos con OK/AVISO/FALLA reales (probar abrir el dispositivo, cargar el modelo, resolver el DNS de la API y verificar que la key existe, **sin** gastar tokens). Asistente de primera ejecución por voz: saludo, petición de la API key si falta, descarga de modelos con progreso.
- **CA-11.1:** con una instalación limpia (sin `~/.config/fokkus-jarvis` ni modelos), el usuario llega a conversar siguiendo solo las indicaciones habladas y `--set-api-key`.
- **CA-11.2:** `--check` devuelve código ≠ 0 si algún subsistema obligatorio está en FALLA.

### HU-12 — Empaquetado, documentación y release `v0.2.0`
- **Alcance:**
  - `.spec`/RPM con `Requires:` reales (`alsa-lib`, `onnxruntime`, `espeak-ng` y lo que corresponda) y **sin** `edge-tts` ni `whisper-cli`.
  - Lanzado desde el menú de **Caelestia**, el proceso debe funcionar. El servicio de Caelestia corre con `ProtectSystem=strict` y solo puede escribir en `~/.config`, `~/.local/share`, `~/.local/state`, `~/.cache` y `/run/user/$UID`; hoy `~/.fokkus/jarvis.sock` no es escribible desde ahí. Opciones: socket en `$XDG_RUNTIME_DIR` con compatibilidad hacia `~/.fokkus/jarvis.sock`, o `Exec=` vía `systemd-run --user --collect`. Hay que coordinar el cambio de ruta con Fokkus IDE.
  - Unidad `systemd --user` opcional para el autoarranque.
  - README actualizado (sin Tauri ni `faster-whisper`), CHANGELOG, backlog y SDD padre actualizados, informe de seguridad `docs/specs/informe-seguridad-v0.2.0.md`.
- **CA-12.1:** `dnf install ./fokkus-jarvis-0.2.0-1.x86_64.rpm` en un Fedora 44 limpio resuelve todas las dependencias y la app conversa sin instalar nada a mano.
- **CA-12.2:** abrir desde el menú de Caelestia deja el socket activo y la conversación funcionando.
- **CA-12.3:** el tag `v0.2.0` apunta a un commit con CI en verde y los artefactos de HU-0 publicados.

---

## 7. Requisitos no funcionales y métricas de aceptación

| Métrica | Objetivo | Cómo se mide |
|---|---|---|
| Filler tras fin de turno | ≤ 300 ms p95 | Marca de tiempo VAD-fin → primer sample en el callback de salida |
| Primer audio de la respuesta (Edge + Claude) | ≤ 1,5 s p50 · ≤ 2,5 s p95 | Igual, hasta el primer sample de la respuesta real |
| Corte por barge-in | ≤ 50 ms p95 | Loopback: inicio de voz inyectada → RMS de salida < −60 dBFS |
| WER es-CL | ≤ 15 % | Corpus de la sección 9 |
| Activaciones falsas | ≤ 1 / hora | 1 h de audio ambiente mixto |
| CPU en reposo escuchando (VAD activo, sin habla) | ≤ 3 % de un núcleo | `pidstat` 10 min |
| RAM con modelos cargados | ≤ 300 MB RSS | `/proc/<pid>/status` tras 1 h de uso |
| Estabilidad | 8 h sin caídas, sin fugas (RSS estable ± 10 %) | Prueba de resistencia de la sección 9 |

> **Enmienda que el PO debe aprobar:** el objetivo "≤ 40 MB de RAM" del SDD padre **no es compatible** con tener cargados los modelos de whisper y Piper (solo `ggml-base-q5_1` pesa ~60 MB). Se propone ≤ 40 MB **antes** de cargar modelos (modo silencio prolongado: descargar los modelos de memoria tras 10 min sin actividad) y ≤ 300 MB con modelos cargados.

---

## 8. Gates de calidad y seguridad (CI obligatoria)

1. `make fmt-check`, `make lint` (clippy `-D warnings`, también para `--target x86_64-pc-windows-gnu`), `make test`, `make check`.
2. `cargo deny check` con un `deny.toml` que incluya como mínimo:
   - `[advisories]`: `yanked = "deny"` y ningún advisory ignorado sin justificación escrita.
   - `[licenses]`: lista permitida explícita (MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, Unlicense, CC0-1.0, MIT-0, 0BSD, MPL-2.0, CDLA-Permissive-2.0); cualquier otra falla.
   - `[bans]`: `deny = ["openssl-sys", "native-tls", "aws-lc-sys", "aws-lc-rs", "tokio", "reqwest"]`, `multiple-versions = "warn"`.
   - `[sources]`: solo crates.io; `allow-git` únicamente para el fork de `whisper-rs-sys` (SEC-01), fijado a un commit.
3. `cargo audit` sin hallazgos.
4. Verificación de la versión de whisper.cpp vendorizada (≥ 1.9.5), más la revisión manual de los issues abiertos de seguridad de whisper.cpp y ggml en cada release.
5. Test de robustez de audio (SEC-08), con ASan sobre la integración con whisper.cpp.
6. Cobertura: cada módulo nuevo con tests unitarios; el parser SSE, el segmentador de frases, el gestor de modelos y la máquina de estados con cobertura ≥ 90 % de líneas.
7. Prohibido commitear en `main` con CI roja; prohibido `--no-verify`.

---

## 9. Protocolo de aceptación de punta a punta (lo ejecuta PM/QA en el equipo del usuario)

1. **Instalación limpia:** desinstalar la versión anterior, borrar `~/.config/fokkus-jarvis` y la caché de modelos, instalar el RPM firmado del release y verificar con `rpm -K`.
2. **Primera ejecución** desde el menú de Caelestia: seguir las indicaciones habladas hasta conversar (CA-11.1).
3. **Corpus de prueba** versionado en `tests/corpus/` (grabado por el usuario, con su consentimiento, o sintético): 50 frases es-CL cortas y largas, con y sin ruido de fondo, más 1 h de audio ambiente para medir activaciones falsas. Se usa para medir CA-3.1, CA-4.1 y CA-5.1.
4. **Guion de 10 escenarios** con su resultado esperado: pregunta simple; pregunta de seguimiento sin "Jarvis"; interrupción a mitad de respuesta; interrupción con tarea del swarm en curso; corte de red a mitad de la respuesta; modo sin internet (Piper); modo silencio y reactivación; reconexión de audífonos USB; música por altavoces mientras Jarvis habla; 30 min de videollamada sin activaciones falsas.
5. **Resistencia:** 8 h encendido con interacciones periódicas automatizadas.
6. **Informe** `docs/specs/aceptacion-v0.2.0.md` con métricas reales frente a los objetivos de la sección 7, evidencia por CA y la lista de hallazgos. **Cualquier CA en rojo bloquea el tag.**

---

## 10. Entregables

- Código en una rama `feat/conversacion-nativa`, con PRs por HU revisados por PM/QA.
- `deny.toml`, workflow de CI actualizado y Makefile con `audit` funcional.
- `docs/specs/informe-seguridad-v0.2.0.md`: re-ejecución de la sección 3 sobre el `Cargo.lock` final, estado de SEC-01 a SEC-09 y SBOM.
- `docs/specs/aceptacion-v0.2.0.md` (sección 9).
- README, CHANGELOG, `backlog.md` (Épicas 2 y 3 con el estado real) y SDD padre actualizados.
- Release `v0.2.0` en GitHub: RPM firmado (más `.deb` si el pipeline existente lo genera y pasa las mismas pruebas), `SHA256SUMS`, `SHA256SUMS.asc`, `sbom.cdx.json` y notas de versión.

---

## 11. Fuera de alcance de v0.2.0

- Issue #6 (operador de sistema: archivos, terminal, computer use, MCP).
- Paridad funcional en Windows y macOS. **Deben seguir compilando** (`cargo check` en CI), pero la aceptación funcional es solo Linux x86_64.
- Proveedores LLM alternativos (Gemini, DeepSeek, Ollama). El diseño deja el cliente detrás de un *trait* para agregarlos después, sin implementarlos ahora.
- Identificación de hablante.

---

## 12. Riesgos y mitigaciones

| Riesgo | Prob. | Impacto | Mitigación |
|---|---|---|---|
| Microsoft bloquea o cambia Edge TTS | Media | Medio | Piper obligatorio con conmutación automática (HU-8); `msedge-tts` se actualiza seguido |
| `sonora` (0.x) no cancela bien el eco por altavoces | Media | Alto | Plan B con `webrtc-audio-processing` del sistema dentro del mismo release (CA-2.3) |
| `earshot` con menos *recall* que Silero en es-CL | Media | Medio | Calibración con el corpus; si no llega a CA-3.1, se evalúa Silero vía el mismo `ort` ya aprobado (requiere la addenda de la sección 0.5) |
| Latencia de whisper en CPU | Baja | Medio | Modelos cuantizados; si hace falta, la feature `hipblas` (ROCm) documentada (sección 4) |
| Nuevo CVE en whisper.cpp/ggml | Media | Alto | Hash fijo de los modelos + validación de cabecera; gate de versión; revisión de issues en cada release |
| `ort` RC incompatible con onnxruntime 1.22 de Fedora | Media | Medio | Fijar la versión exacta de `ort` compatible; prohibido `download-binaries` |
| Costo de la API de Claude | Baja | Bajo | Prompt caching; `effort: low`; respuestas breves por diseño; límite de historial |

---

## 13. Decisiones que debe tomar el PO antes de comenzar

| ID | Decisión | Recomendación |
|---|---|---|
| **D-1** | Licencia del producto: `Proprietary` (RPM) vs MIT (README) | Unificar; afecta la compatibilidad con MPL-2.0 (SEC-09) |
| **D-2** | Enmienda de RAM (sección 7) | Aprobar ≤ 40 MB sin modelos y ≤ 300 MB con modelos |
| **D-3** | Aceptar que el texto de las respuestas se envíe a Microsoft (Edge) por defecto | Sí, con `tts = "local"` disponible y documentado |
| **D-4** | Modelo LLM por defecto | `claude-opus-5-5` con `effort: low`; configurable |
| **D-5** | Ruta del socket (HU-12) | `$XDG_RUNTIME_DIR/fokkus-jarvis.sock`, con compatibilidad temporal hacia `~/.fokkus/jarvis.sock` |

---

## 14. Fuentes del análisis de seguridad

- OSV.dev (`/v1/querybatch`, ecosistema crates.io): consulta sobre los 374 paquetes del árbol candidato y los 71 del `Cargo.lock` actual, 2026-10-15.
- crates.io API: metadatos, versiones, licencias y dependencias de cada candidata.
- whisper.cpp: [releases](https://github.com/ggml-org/whisper.cpp/releases) (correcciones en 1.9.3, 1.9.4 y 1.9.5), [issue #4059](https://github.com/ggml-org/whisper.cpp/issues/4059) (n_vocab, abierto), [CVE-2026-110810](https://app.opencve.io/cve/CVE-2026-110810) (ttype, corregido en 1.9.5), [CVE-2026-17512](https://www.sentinelone.com/vulnerability-database/cve-2026-17512/) (log_mel_spectrogram).
- Edge TTS no oficial: [kothok-edge-tts](https://docs.rs/kothok-edge-tts) (rotación de `SEC_MS_GEC_VERSION`), [complemento edge-tts para Firefox](https://addons.mozilla.org/firefox/addon/edge-tts/) (bloqueo de diciembre de 2025), [foro pyvideotrans](https://bbs.pyvideotrans.com/show/1358) (errores 401/403); código de `msedge-tts` 0.4.1 (`src/constants.rs`, `src/tts/drm.rs`).
- earshot: [docs.rs](https://docs.rs/crate/earshot/1.2.2) y el benchmark comparativo publicado por [rlx-vad](https://docs.rs/crate/rlx-vad/0.2.5).
- sonora: [docs.rs](https://docs.rs/crate/sonora/0.1.0).
- Repositorios oficiales de Fedora 44: `whisper-cpp` 1.8.1 (sin `whisper-cli`), `onnxruntime` 1.22.2, `webrtc-audio-processing` 2.1, `espeak-ng` 1.52.0, `alsa-lib-devel` 1.2.16.1.
