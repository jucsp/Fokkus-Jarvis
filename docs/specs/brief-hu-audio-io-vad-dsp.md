# Brief técnico para el Dev Senior (DeepSeek V4 Pro): audio I/O, DSP y VAD nativos

**Contrato:** [`sdd-conversacion-nativa-v0.2.0.md`](sdd-conversacion-nativa-v0.2.0.md) (aprobado por el PO). Este brief cubre **HU-1, HU-2 y HU-3**.
**Redactado por:** Claude Code (PM/QA) · **Fecha:** 2026-10-15
**Rama:** `feat/conversacion-nativa-v0.2.0`
**Invocación:** `python3 ~/.vibe/deepseek_task.py "<este brief>" --cwd /home/juancarlos/Proyectos/Personales/Fokkus-Jarvis --model pro`

---

## Contexto de negocio

Juan Carlos quiere hablarle a Jarvis como a una persona: decir "Jarvis, …" con música o una videollamada de fondo, que le conteste por voz y poder interrumpirlo hablando encima. Hoy (v0.1.3) la esfera aparece en pantalla, pero Jarvis **ni escucha ni habla**: `jarvis-audio` no abre el micrófono ni los altavoces, y `AudioPlayback` es solo una bandera sin callback que la use.

Este brief pide la base de todo lo demás: **oír** (captura del micrófono), **hablar** (reproducción con corte instantáneo), **limpiar** lo que se oye (quitar el eco de la propia voz de Jarvis, el ruido y nivelar el volumen) y **saber cuándo el usuario habla y cuándo terminó**. Sin estas tres piezas no hay STT, LLM ni barge-in reales.

El PO exige entrega **completa y funcional, no un MVP** (contrato §0): nada de *stubs*, `todo!()`, `unimplemented!()`, `// TODO` en rutas de producción ni mensajes que anuncien estados que el programa no verificó.

---

## Estado del entorno (ya preparado por PM/QA)

| Elemento | Estado |
|---|---|
| Dependencias en `Cargo.toml` (workspace) y `crates/jarvis-audio/Cargo.toml` | ✅ `cpal 0.18.2`, `ringbuf 0.5.3`, `rubato 5.0.1`, `earshot 1.2.2`, `sonora 0.2.0`, `whisper-rs 0.16.0`, `msedge-tts 0.4.1`, `symphonia 0.6.1` (mp3), `ort =2.0.0-rc.14` (`load-dynamic`, `api-22`), `ureq 3.4.2` (rustls+ring), `sha2 0.11.1`; `keyring 4.2.0` solo en el workspace |
| SEC-01 | ✅ `vendor/whisper-rs-sys` con whisper.cpp **v1.9.5** (`make whisper-version`) |
| SEC-03 | ✅ `vendor/msedge-tts` parcheado a `ring`; `cargo tree -i aws-lc-sys` vacío |
| `deny.toml` | ✅ `cargo deny check` → advisories, bans, licenses y sources OK |
| `make check`, `make lint`, `make test`, `make fmt-check` | ✅ en verde (163 tests) |

**Requisitos del sistema para compilar** (si faltan, el build falla en `alsa-sys` o en bindgen):
`sudo dnf install alsa-lib-devel clang cmake gcc-c++`. Sin `clang` (solo `clang-libs`), exportar `BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/clang/22/include"`.

**No tocar:** `Cargo.toml`, `deny.toml`, `vendor/`, `.github/`. Si necesitas una dependencia nueva, **detente y repórtalo**: el contrato §0.5 exige análisis de seguridad y aprobación del PO antes de agregarla.

---

## Reglas rígidas (aplican a las tres HU)

1. **Rust 1.99, edition 2021.** Cero `unsafe` en código propio. Sin `unwrap()`/`expect()` fuera de `#[cfg(test)]`. Errores con `enum` propios que implementen `std::error::Error` (patrón de `SttError`/`TtsError` existentes).
2. **Callbacks de audio de cpal (entrada y salida):** sin locks (`Mutex`, `RwLock`, canales con bloqueo), sin allocations (`Vec::push` con crecimiento, `Box`, `String`, `format!`), sin syscalls bloqueantes, sin logging. Solo: leer/escribir en `ringbuf`, atómicos y aritmética sobre buffers preasignados. Esto es **CA-1.3** y se verifica con un test.
3. **Sin `tokio` ni async.** Hilos `std::thread` con nombre (`jarvis-audio-dsp`, etc.) y `crossbeam-channel` (ya en el workspace) para mensajes fuera del camino de tiempo real.
4. **Hilos (contrato §5):** (1) callback de entrada cpal → solo copia al ringbuf; (2) hilo DSP + VAD; (6) callback de salida cpal. Los hilos 3–5 (STT, LLM, TTS) **no son parte de este brief**, pero las interfaces que expongas deben permitir conectarlos sin rediseño.
5. **Toda muestra que entre en `earshot` o `sonora` se sanea antes:** NaN/±inf → 0.0 y recorte a `[-1.0, 1.0]`. Ojo: `earshot::Detector::predict_f32` tiene `debug_assert!` sobre el rango y devuelve `-1.0` si el frame no mide exactamente 256 muestras.
6. **Español en comentarios y mensajes**, con la misma densidad de comentarios que el código existente (`playback.rs`, `stt.rs`).
7. `make fmt-check`, `make lint` (clippy `-D warnings` + `cargo deny`) y `make test` deben quedar en verde. **No hagas commits** (regla de protección de git del Swarm).
8. Al terminar, aplica los cambios **en disco** (no respondas solo con código en el chat) y lista los archivos tocados.

---

## HU-1 — Captura y reproducción de audio nativas

- 📁 **Repositorio:** `Fokkus-Jarvis` · `/home/juancarlos/Proyectos/Personales/Fokkus-Jarvis`
- 🎯 **Capa a trabajar:** Backend · crate `crates/jarvis-audio` (módulos nuevos de E/S, por ejemplo `io/` o `device.rs` + `mixer.rs`; el nombre lo decides tú) y `playback.rs` existente. Integración mínima en `crates/jarvis-app` solo para arrancar/detener el motor de audio y para `--check`.
- 📌 **Objetivo técnico:**
  - Abrir con `cpal` 0.18 (host ALSA por defecto, que en Fedora pasa por `pipewire-alsa`) **un stream de entrada y uno de salida** sobre el dispositivo por defecto del sistema, o sobre uno elegido **por nombre** desde la configuración (`input_device` / `output_device`; en cpal 0.18 el nombre se obtiene con `DeviceTrait::description()`, ya no con `name()`). Negociar el formato con `supported_*_configs()` prefiriendo `f32` a 48 kHz; si el dispositivo solo ofrece otro formato o tasa, convertir sin romper el contrato de tiempo real.
  - Entrada: el callback solo empuja muestras (mono, mezclando canales si hace falta) a un `ringbuf` SPSC preasignado; si el buffer está lleno, cuenta la pérdida en un atómico (no bloquea, no aloca).
  - Salida: un **mezclador** que consume PCM desde un `ringbuf` alimentado por el futuro hilo TTS. `AudioPlayback` deja de ser solo una bandera: el callback de salida consulta `is_stop_requested()`/`generation()` y, ante una parada, aplica un **fade-out lineal de 15 ms** y vacía lo pendiente de esa generación (base del barge-in ≤ 50 ms de CA-9.1). El callback también publica, sin locks, **la referencia de salida** (lo que realmente sonó) para el AEC de HU-2 y el **RMS** de salida para la esfera.
  - **Reconexión en caliente (CA-1.2):** cuando cpal reporte que el dispositivo desapareció o cambió (`ErrorKind` del callback de error), un supervisor fuera del hilo de audio reconstruye el stream en < 3 s sin reiniciar el proceso, reintentando con *backoff* acotado. Los *xruns* (`ErrorKind::Xrun` en 0.18.2) se cuentan para CA-1.1.
  - **Modo silencio (base de CA-4.3):** debe existir una operación que **cierre y libere** el stream de entrada (no solo que descarte muestras), para que PipeWire deje de listar a Jarvis como cliente de grabación, y otra que lo reabra.
  - Exponer métricas de solo lectura (xruns, muestras perdidas, dispositivo activo, tasa negociada, RMS de entrada y salida) que `--check` y la esfera puedan consultar.
- ✅ **Criterios de aceptación que debes dejar demostrables:**
  - **CA-1.3:** test que ejecute la función de procesamiento de los callbacks (extráela a una función pura que reciba slices) bajo un *allocator* contador de `#[global_allocator]` **solo en el binario de test** y verifique 0 allocations. No agregues `assert_no_alloc` como dependencia sin aprobación; escribe el contador tú mismo con `std::alloc::System`.
  - Tests unitarios del fade-out (la rampa llega a 0 en 15 ms ± 1 frame a 48 kHz y a 44,1 kHz), de la invalidación por generación y del conteo de pérdidas con buffer lleno.
  - CA-1.1, CA-1.2 y CA-1.4 se miden en el equipo real (PM/QA). Deja un `--check` que abra y cierre ambos dispositivos de verdad e informe OK/AVISO/FALLA.

## HU-2 — Limpieza de señal: AEC3 + supresión de ruido + AGC2

- 📁 **Repositorio:** `Fokkus-Jarvis` · `/home/juancarlos/Proyectos/Personales/Fokkus-Jarvis`
- 🎯 **Capa a trabajar:** Backend · crate `crates/jarvis-audio` (módulo DSP nuevo que corre en el hilo `jarvis-audio-dsp`).
- 📌 **Objetivo técnico:**
  - Remuestrear la captura de la tasa del dispositivo a **16 kHz mono** con `rubato` 5 (API nueva: `Fft`/`FixedSync` o `Async`/`FixedAsync` con `audioadapter`; elige y justifica) y hacer lo mismo con la referencia de salida.
  - Procesar con `sonora` 0.2 (`AudioProcessing::builder()`, `apply_config`, `process_render_f32` para la referencia y `process_capture_f32` para el micrófono, frames de **10 ms = 160 muestras** a 16 kHz, *deinterleaved*) con AEC3, NS y AGC2 activados.
  - **Alineación temporal de la referencia:** el AEC solo funciona si la referencia corresponde a lo que el micrófono capta. Calcula el retardo a partir de las latencias que reporta cpal (timestamps de callback y de reproducción) e infórmalo con `set_stream_delay_ms`; documenta en el código de dónde sale cada término.
  - **Re-encuadre:** `sonora` trabaja con 160 muestras y `earshot` con 256. Diseña el acumulador entre ambos sin allocations por frame.
  - Si `sonora` devuelve `Err` en un frame, se cuenta y se deja pasar la señal sin procesar ese frame; nunca debe tumbar el hilo.
  - Deja el procesador detrás de un *trait* interno pequeño para que el **plan B** (CA-2.3: `webrtc-audio-processing` 2.1 del sistema) pueda enchufarse sin tocar el resto. **No implementes el plan B** ni agregues su crate: requiere aprobación del PO.
- ✅ **Criterios de aceptación que debes dejar demostrables:**
  - Test sintético de AEC: referencia = tono o ruido conocido, captura = la misma señal con retardo y atenuación más una "voz" (otro tono). Tras la convergencia, la energía residual de la referencia cae al menos 20 dB y la "voz" se conserva. Si no lo logras, repórtalo con números: decide si se activa el plan B (CA-2.3).
  - Tests de robustez SEC-08 sobre la cadena remuestreo → sonora → re-encuadre: silencio, saturación, NaN/inf, frames de largo 0 y 1, tasas de 8, 22,05, 44,1, 48 y 96 kHz. Sin pánicos.
  - CA-2.1 y CA-2.2 (altavoces reales, 50 locuciones) los mide PM/QA. Deja un modo de diagnóstico **solo en memoria** que exponga por métrica el ERLE del AEC (contrato HU-10: el audio no se escribe a disco).

## HU-3 — Detección de voz (VAD) y fin de turno

- 📁 **Repositorio:** `Fokkus-Jarvis` · `/home/juancarlos/Proyectos/Personales/Fokkus-Jarvis`
- 🎯 **Capa a trabajar:** Backend · crate `crates/jarvis-audio` (módulo VAD/endpointing en el mismo hilo DSP) y tipos de evento compartidos en `crates/jarvis-core` si el estado de la esfera los necesita.
- 📌 **Objetivo técnico:**
  - Pasar cada frame limpio de 256 muestras (16 ms) a `earshot::Detector` (`Detector::default_boxed()` o `const_default()`), obteniendo un puntaje 0–1.
  - Construir encima un **endpointer** con: umbral configurable, *hangover*/histéresis (cuántos frames de voz para abrir y de silencio para cerrar), **pre-roll de 300 ms** (el segmento entregado incluye los 300 ms previos al inicio detectado, desde un buffer circular preasignado) y **silencio de cierre configurable** (`endpoint_silence_ms`, 700 ms por defecto).
  - **Fin de turno adaptativo:** el endpointer acepta una señal externa "la transcripción parcial termina en conector" (y, pero, entonces, o sea…) que extiende la espera hasta **1,2 s**. La detección del conector la hará el STT (fuera de este brief); aquí solo diseña la entrada y la lógica de extensión.
  - Emitir eventos por `crossbeam-channel` hacia el coordinador: inicio de voz (con marca de tiempo monotónica, para medir el barge-in), segmento completo (PCM 16 kHz `f32` listo para whisper, con su duración) y fin de turno (con marca de tiempo, base de los 300 ms del filler en CA-9.2). Durante el estado `Speaking`, el evento de inicio de voz **post-AEC** es el disparador del barge-in (HU-9); deja el gancho, sin implementar HU-9.
  - Limitar la duración máxima de un segmento (30 s, CA-5.3) y cortarlo limpio si se excede.
- ✅ **Criterios de aceptación que debes dejar demostrables:**
  - Tests del endpointer con secuencias sintéticas de puntajes: pausa de 500 ms dentro de una frase **no** corta (CA-3.2); 700 ms sí; con la señal de conector, 1,0 s no corta y 1,2 s sí; el pre-roll entrega exactamente 300 ms ± 1 frame; segmento de 30 s se corta.
  - Test de `earshot` con silencio digital (puntaje bajo) y con ruido blanco a −20 dBFS, que documente el puntaje real obtenido (calibración inicial para CA-3.1).
  - CA-3.1 (recall ≥ 95 %, FP ≤ 5 % sobre el corpus de §9) lo mide PM/QA con el corpus real; deja el umbral y el *hangover* en configuración para poder calibrarlos sin recompilar.

---

## Entrega esperada de DeepSeek

1. Código aplicado en disco, con tests, en `crates/jarvis-audio` (y lo mínimo en `jarvis-core`/`jarvis-app`).
2. Resumen con: archivos creados/modificados, diagrama de hilos y buffers final, decisiones tomadas (resampler elegido, cálculo del retardo del AEC, parámetros por defecto del endpointer) y cualquier CA que no pudiste dejar demostrable, con el motivo.
3. Salida de `make fmt-check`, `make lint` y `make test`.

PM/QA validará después con `make test`, revisión de código de los callbacks (CA-1.3) y pruebas en el equipo real (Fedora 44, KWin Wayland, PipeWire 1.6).
