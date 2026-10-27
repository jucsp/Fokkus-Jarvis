# Investigación técnica — Turn-taking conversacional y voz expresiva

> Autor: Claude (PM/QA del Swarm) · Fecha: 2026-10-27 · Base: v0.2.27 (`56d18c4`)
> Estado: **propuesta para el PO**. No hay cambios de arquitectura aplicados; solo un fix menor (§1.3, H-T4).

## 0. Contexto de negocio

El usuario reporta dos problemas que hoy rompen la ilusión de conversación con Jarvis:

1. **Le cortan la palabra.** Cuando habla largo, respira o hace una pausa para pensar, Jarvis da por cerrado el turno y responde antes de tiempo. Quiere un turn-taking parecido al de una persona: que Jarvis distinga "estoy pensando" de "terminé".
2. **La voz suena plana.** Edge-TTS es nítido, pero no tiene vida: no hay entonación, pausas con intención, respiraciones ni emoción. La referencia es Gemini Live o Claude en modo voz. La idea es que el LLM (el cerebro) **guionice** cómo decir cada frase y Jarvis lo interprete.

Ambos problemas pesan más que el backlog actual porque afectan cada interacción.

---

## 1. Diagnóstico del código actual

### 1.1 Pipeline real de fin de turno

```
mic → DSP (AEC3/NS) → earshot VAD (16 ms/frame) → Endpointer → EndOfTurn{pcm}
    → jarvis-stt (whisper, turno cerrado) → ends_with_connector()? → retener 500 ms / despachar
    → dispatch(): chime inmediato + Thinking → LLM → TTS por frase (Edge)
```

Parámetros efectivos (`EndpointConfig::default()`, `vad/endpoint.rs:50-62`; **no son configurables**: `main.rs:614` usa `EngineConfig::default()`):

| Parámetro | Valor | Efecto |
|---|---|---|
| `voice_open` / `voice_hold` | 0,45 / 0,25 | histéresis razonable |
| `open_frames` | 2 (32 ms) | apertura rápida |
| `silence_ms` | **1 200 ms** | cierre puramente acústico |
| `extended_silence_ms` | 2 000 ms | solo si ya se detectó conector |
| `max_segment_ms` | **30 000 ms** | corte duro que **despacha** el turno |
| `connector_grace` (orquestador) | 500 ms | espera tras un conector ya transcrito |

### 1.2 Causas raíz del corte prematuro

| ID | Causa | Evidencia | Severidad |
|---|---|---|---|
| **H-T1** | **El fin de turno es solo silencio acústico de 1,2 s.** Las pausas para pensar en una conversación real duran a menudo de 1,5 a 3 s. Ninguna señal prosódica (entonación descendente o sostenida) ni semántica participa en la decisión **antes** de cerrar. | `endpoint.rs:268-273` | Crítica |
| **H-T2** | **El conector se detecta tarde.** Whisper solo ve turnos cerrados, así que `signal_connector()` nunca extiende el turno *en curso*: solo afecta al turno **siguiente**, después del cierre, del STT y de una gracia fija de 500 ms. El silencio extendido de 2 s existe en el endpointer, pero en la práctica no actúa sobre la primera pausa. | `orchestrator.rs:33-39`, `1977-1985`, `2034-2075` | Crítica |
| **H-T3** | **El léxico de "no terminé" es pobre.** `CONNECTORS` solo cubre conjunciones (`y, pero, que, porque…`). Faltan muletillas (`eh, este, o sea, mmm, como que, igual`), preposiciones y artículos finales (`de, a, para, con, el, la, un, mi`) y verbos auxiliares colgados (`quiero, necesito que, me puedes`). Además, whisper suele **borrar** las muletillas y agregar un punto final, con lo que se pierde justo la pista de que el usuario no terminó. | `orchestrator.rs:1374-1391`, `stt.rs:155-170` (`suppress_blank`, `suppress_nst`, sin prompt de disfluencias) | Alta |
| **H-T4** | **El chime de aceptación hace que la continuación se lea como eco.** `dispatch()` reproduce el chime de inmediato (`orchestrator.rs:2626`). Con `barge_in_enabled = false` (valor por defecto con parlantes), la voz que empieza mientras suena el chime o dentro de `ECHO_TAIL` (400 ms) se marca como `echo_segment`. Si esa continuación dura ≤ 700 ms (`ECHO_MAX_MS`, por ejemplo "eh… y también"), **se descarta sin transcribir**. | `orchestrator.rs:1956-1966`, `1831-1836`, `2000-2005` | Alta |
| **H-T5** | **La fusión post-despacho solo funciona en `Thinking`.** Si el usuario retoma cuando Jarvis ya pasó a `Speaking` (respuestas locales rápidas o primera frase del LLM), `pending_user_turn` se limpia y la continuación se despacha como **consulta nueva**. Jarvis responde dos veces, a dos medias frases. | `orchestrator.rs:2052-2060` | Alta |
| **H-T6** | **Carrera entre STT y `VoiceStart`.** Si el usuario retoma *mientras* whisper transcribe el primer fragmento, `VoiceStart` llega con `pending = None`: no se señala el conector y no se anula el `deadline`. Cuando el fragmento 1 vuelve marcado como conector, arma un `deadline` de 500 ms que puede vencer antes de que el fragmento 2 termine su STT, y entonces el fragmento 1 se despacha solo. *Probable; confirmar con un test de orquestador con STT lento.* | `orchestrator.rs:1977-1985` vs `2063-2068` | Media |
| **H-T7** | **El corte duro a los 30 s despacha.** Un monólogo largo (dictar un requerimiento) se corta a los 30 s y esa mitad **se envía al LLM como turno completo**. | `endpoint.rs:279-298` | Media |
| **H-T8** | **No hay estadística por usuario ni observabilidad.** No se registra la distribución de pausas intra-turno ni la razón del cierre (silencio, máximo o conector). Sin ese corpus no se puede calibrar nada (lo mismo quedó pendiente en CA-3.1, `vad/mod.rs:107-111`). | — | Media |
| H-T9 | Las respiraciones: con `voice_hold = 0,25` y NS activo, una inhalación audible puede **mantener** el turno abierto (eso está bien) o, si NS la atenúa, contar como silencio. No es causa principal, pero conviene medirla con el corpus de H-T8. | `dsp/thread.rs:166-179` | Baja |

**Conclusión:** el corte no se arregla subiendo `silence_ms`. Si se sube a 2,5 s, Jarvis se vuelve lento en *todos* los turnos. Lo que falta es una **decisión de fin de turno en dos etapas** (silencio corto → clasificador "¿terminó?") más arreglar la fusión de continuaciones (H-T4, H-T5 y H-T6).

### 1.3 Causas de la voz plana

| ID | Causa | Evidencia |
|---|---|---|
| **H-V1** | `SpeechConfig` usa siempre `pitch: 0, rate: 0, volume: 0`. Todas las frases salen con la misma prosodia. | `tts/edge.rs:32-38` |
| **H-V2** | El endpoint gratuito de Edge **solo acepta un `<voice>` con un único `<prosody>`**. Microsoft rechaza el SSML personalizado (`mstts:express-as`, estilos y roles) desde edge-tts 5.0. `msedge-tts 0.4.1` arma ese SSML fijo (`tts/mod.rs:150-161` del crate) con `xml:lang='en-US'` hardcodeado. **No hay estilos emocionales disponibles por esta vía.** | crate `msedge-tts-0.4.1` |
| **H-V3** | La síntesis es **por frase independiente** (`PhraseSplitter`). Cada request reinicia la curva de entonación, así que cada frase suena como un enunciado aislado con cierre descendente. Eso produce el efecto de lista leída. | `tts/phrase.rs`, `tts/mod.rs:366` |
| **H-V4** | El `SYSTEM_PROMPT` pide "una o dos frases" y prohíbe el formato, pero no le pide al LLM puntuación expresiva (`¡…!`, `¿…?`, `…`, `—`) ni le da un canal para dirigir la prosodia. | `brain/client.rs:49-80` |
| **H-V5** | Los silencios entre frases dependen de la latencia de red, no de una intención. No hay pausas dramáticas ni respiraciones. | `speech_queue.rs`, `playback.rs` |
| **H-V6 (corregido)** | `msedge-tts` inserta el texto **sin escapar** dentro del SSML. Un `&`, `<` o `>` en la respuesta del LLM rompe el XML; Edge falla, el error se mapea a `Network` y la frase se dice con espeak-ng (voz robótica intercalada). **Corregido** con `escape_ssml_text()` en `tts/edge.rs` y su test `ssml_text_is_escaped`. | `tts/edge.rs` |

---

## 2. Estado del arte (open source)

### 2.1 Detección de fin de turno

| Proyecto | Técnica | Datos clave | Aplicabilidad a Jarvis |
|---|---|---|---|
| **Pipecat Smart Turn v3.x** | Clasificador **acústico** (encoder Whisper-tiny + cabeza lineal) sobre los últimos ≤ 8 s de audio a 16 kHz. Se ejecuta **cuando el VAD detecta silencio** y decide "completo / incompleto" usando entonación y ritmo. | ~8 M parámetros, ONNX int8 de 8 MB, ~10-65 ms en CPU, **BSD-2**, 23 idiomas **incluido español**. Es el valor por defecto de Pipecat desde v0.0.102. | **Muy alta.** Encaja exactamente después del endpointer actual, sin depender del STT. Licencia compatible. |
| **LiveKit turn-detector (MultilingualModel)** | Modelo de **texto** (Qwen2.5-0.5B fine-tuned) que evalúa la transcripción con contexto conversacional. Combina `min_endpointing_delay` (0,5 s) y `max_endpointing_delay` (6 s). | 14 idiomas incluido español, ~400 MB RAM, ~25 ms. La v0.4.1-intl reporta **−39 % de interrupciones falsas**. **Licencia LiveKit Model License** (no OSI). | Media. El patrón min/max delay es el que hay que copiar. El modelo es pesado y su licencia hay que revisarla. |
| **TEN Turn Detection + TEN VAD** | LLM de texto para full-duplex (finished / unfinished / wait) más un VAD propio de baja latencia. | Inglés y chino en el turn detector. Benchmarks del propio fabricante. | Baja para español. TEN VAD sirve como referencia de VAD. |
| **Silero VAD v5** | VAD neuronal de referencia. Pipecat lo usa con `stop_secs` corto (~0,2 s) **y delega el fin de turno a Smart Turn**. | MIT | Earshot ya cumple ese rol. Lo que importa es el patrón: **VAD con silencio corto + clasificador**. |
| **whisper_streaming (ÚFAL) / WhisperLive** | STT incremental con política **LocalAgreement-2**: confirma el prefijo que coincide en dos pasadas consecutivas. | ~3,3 s de latencia en long-form. | Permite tener texto **parcial** durante el turno, lo que hace posible el endpointing semántico *antes* del cierre. Es caro en CPU con whisper.cpp, así que va en la Fase 3. |

**Patrón común de la industria (2025-2026):**
1. El VAD detecta un silencio **corto** (200-600 ms) → *candidato* a fin de turno.
2. Un clasificador (acústico, de texto o ambos) estima P(terminó).
3. Si P es alta, se cierra ya. Si es baja, se espera hasta un **máximo** (3-6 s) y se reevalúa. Si el usuario retoma, el turno sigue abierto.
4. **Generación especulativa:** el STT y el LLM pueden arrancar en el candidato y descartarse si el usuario retoma.

### 2.2 Voz expresiva

| Opción | Expresividad | Español | Licencia / costo | Encaje |
|---|---|---|---|---|
| **Edge-TTS (actual)** | Solo `rate`, `pitch` y `volume` **por request**. No hay `express-as` ni `break`. | Excelente (voces es-CL) | Gratis, sin SLA | Se puede modular **por frase**, porque ya sintetizamos frase a frase. Las pausas se insertan como PCM de silencio local. |
| **Azure AI Speech (oficial)** | SSML completo: `mstts:express-as` con `styledegree`, `<break>`, `<emphasis>`, voces HD/multilingües. Fuentes de terceros indican que es-MX-JorgeNeural tiene estilos (chat, cheerful, excited, sad, whispering); **hay que verificarlo con la API de voces**. | Muy bueno; los estilos son limitados en voces es-* | Pago (tiene capa gratuita) y requiere key | Backend opcional "premium" con el mismo protocolo de directivas. |
| **Chatterbox Multilingual (Resemble)** | Parámetro `exaggeration` (0-1+) y `cfg_weight` para el ritmo. Clonación de voz. | Sí (23 idiomas, más finetunes LatAm y España) | **MIT**. Agrega watermark Perth. Necesita GPU para tiempo real. Es PyTorch. | Spike: no se puede instalar por pip (regla del proyecto). Habría que buscar un export ONNX (existe `chatterbox-multilingual-ONNX` de la comunidad) o correrlo como servicio aislado. |
| **Kokoro-82M** | Baja (sin control emocional) pero natural | Solo 3 voces (ef_dora, em_alex, em_santa), acento peninsular | Apache-2.0, ONNX y CPU | **Reemplazo offline de espeak-ng** (mucho menos robótico). No resuelve la expresividad. |
| Fish Audio S2 / Higgs Audio | Tags emocionales en lenguaje natural | Variable | Licencias de investigación o no comerciales | Descartado por licencia. |
| Orpheus / Maya1 | Tags inline (`<laugh>`, `<sigh>`) | Centrados en inglés | Apache / varía | Descartado para español por ahora. |

**Técnica transversal ("prompt-to-speech"):** el LLM escribe la respuesta con **directivas inline** de entonación (emoción, ritmo, pausa, respiración). Un parser las separa del texto, y un renderer las traduce a lo que el backend activo soporte. Si el backend no soporta una directiva, la degrada sin fallar (Edge: prosodia por frase y silencios; Azure: SSML completo; modelo local: `exaggeration`). Así el "guion" queda **desacoplado del motor**.

---

## 3. Propuesta arquitectural

### 3.1 Turn-taking en dos etapas

```
Endpointer (earshot)                     TurnArbiter (nuevo, hilo jarvis-voice)
  silencio ≥ candidate_ms (600) ──► Candidate{pcm_so_far}
                                          │
                                          ├─ SmartTurn ONNX (≤8 s finales) ─► p_audio
                                          ├─ STT especulativo del parcial ─► texto
                                          │     └─ incompletitud léxica  ─► p_text
                                          │
                                          ├─ p = f(p_audio, p_text, pausa_actual, perfil_usuario)
                                          │
              ◄── Hold (seguir escuchando, HUD "te escucho") si p < umbral y pausa < max_wait (4 s)
              ◄── Commit (EndOfTurn real) si p ≥ umbral  ó  pausa ≥ max_wait
  voz retoma ─► Resume (cancela STT/LLM especulativos, el turno sigue)
```

Decisiones de diseño:

- **El endpointer deja de decidir solo.** Emite `CandidateEnd` (silencio corto) y el árbitro responde `Hold` o `Commit` por un canal de retorno, en vez de cerrar a los 1,2 s. Se respeta la regla de hilos (§5 del SDD): el DSP nunca espera al árbitro. Mientras no llega `Commit`, el endpointer sigue acumulando.
- **Smart Turn v3 en Rust:** ONNX de 8 MB en un hilo dedicado. El runtime puede ser `ort` (ONNX Runtime, binario nativo) o `tract` (Rust puro, sin binarios descargados, más fácil de pasar por `cargo deny`). **El PO debe decidirlo**; QA recomienda `tract` si cumple ≤ 50 ms en esta CPU. Hay que portar el log-mel de 80 bandas que usa Whisper; whisper.cpp ya lo implementa, así que se puede reutilizar o portar.
- **Señal semántica barata (sin LLM):** se amplía `ends_with_connector` a un `incompleteness_score()` que cubra muletillas, preposiciones y artículos finales, y verbos auxiliares colgados. Se le pasa a whisper un `initial_prompt` con disfluencias ("eh, este, mmm, o sea…") para que no las borre.
- **Arreglos de fusión (independientes del modelo):**
  - Durante una **ventana de continuación** (~1,5 s tras el `Commit`) no debe sonar nada audible. El chime pasa a ser **visual** (HUD), o se difiere hasta que el LLM tarde más de X ms. La voz que llega en esa ventana nunca es eco (H-T4).
  - La fusión de continuaciones también aplica en `Speaking` si la locución empezó hace menos de N ms (H-T5).
  - `VoiceStart` sin `pending` pero con un STT en vuelo marca "continuación esperada" (H-T6).
- **Monólogos largos:** `max_segment_ms` pasa a ser un **chunk** (`VoiceEvent::Partial`) que se transcribe y acumula sin despachar. El techo real del turno sube a 120 s (H-T7).
- **Escape hatch:** el "modo dictado" ("Jarvis, toma nota") solo cierra con una frase explícita ("eso es todo", "listo") o con 5 s de silencio.

### 3.2 Voz expresiva con guion del LLM

**Protocolo de directivas (v1, compacto para tolerar el streaming):**

```
[ánimo:alegre|serio|empático|entusiasta|calmo|curioso] [ritmo:lento|normal|rápido]
[pausa:300] [respira] [énfasis]palabra[/énfasis] [susurro]…[/susurro]
```

Ejemplo de salida del LLM:
`[ánimo:entusiasta]¡Listo, ya quedó! [pausa:300][ánimo:calmo]Dejé el desarrollo corriendo en segundo plano… te aviso cuando termine.`

Componentes:

1. **`DirectiveParser`** (dentro de `SpeechFilter` y `PhraseSplitter`): elimina los tags del texto hablado y del HUD, y adjunta a cada frase un `SpeechDirective { mood, rate, pitch, volume, pause_before_ms, pause_after_ms, breath }`. Es incremental y tolera un tag partido entre tokens, igual que `<thought>` hoy. Si el tag es desconocido, se descarta sin pronunciarlo.
2. **`ProsodyRenderer` (trait) por backend:**
   - **Edge:** `mood` → tabla de `rate/pitch/volume` (por ejemplo, entusiasta = +8 % de ritmo, +2 Hz de tono y +5 % de volumen; empático = −8 %, −2 Hz). Las pausas se insertan como **PCM de silencio local** entre frases, y la respiración como **muestra pregrabada** (2-3 clips de inhalación por voz, cacheados como los fillers). Si no hay directiva, un default heurístico usa la puntuación (`…` → 450 ms, `.` → 250 ms, `¿?` → +2 Hz al cierre).
   - **Azure (opcional):** SSML completo con `express-as` y `styledegree`, `<break>` y `<emphasis>`.
   - **Local (futuro):** Chatterbox (`exaggeration`) o Kokoro (solo ritmo y pausas).
3. **Continuidad de entonación (H-V3):** la primera frase va sola (para el TTFA) y después se **agrupan 2-3 frases por request** si comparten directiva. Así Edge entona el párrafo como unidad.
4. **`SYSTEM_PROMPT`:** se agrega un bloque corto con el protocolo, la instrucción de usar puntuación expresiva y un máximo de 1 o 2 directivas por frase. Si el LLM no las emite, el default heurístico mantiene un piso de naturalidad.
5. **Fillers con prosodia variada:** se pre-sintetizan 2 variantes por filler (neutro y cálido) para que no suene siempre igual.

---

## 4. Épica y Historias de Usuario

### ÉPICA E-12 — "Conversación natural": turn-taking humano y voz con intención

**Objetivo:** que Jarvis no corte al usuario en pausas naturales (meta: menos del 5 % de cortes prematuros en el corpus de QA, con latencia p50 de respuesta ≤ 900 ms tras el fin real) y que su voz tenga entonación dirigida por el LLM (meta: preferencia ≥ 70 % en una prueba A/B ciega contra la v0.2.27).

#### Fase 1 — Quick wins (1 sprint, sin modelos nuevos)

**HU-41 · Endpointing configurable y observable**
- 📁 Repositorio: `Fokkus-Jarvis` (`crates/jarvis-audio/src/vad/`, `io/engine.rs`, `crates/jarvis-app/src/config.rs`, `main.rs`)
- 🎯 Capa: Backend audio y config
- 📌 Objetivo técnico: exponer `EndpointConfig` y `connector_grace` en `config.toml` (`[voice.endpoint]`), con validación de rangos. Registrar por turno la razón del cierre (silencio, máximo o conector) y un histograma de pausas intra-turno (P50/P90). Agregar el flag `--diag-turns` para volcarlo. Es la base para calibrar el resto (H-T8).

**HU-42 · Ventana de continuación sin eco falso**
- 📁 Repositorio: `Fokkus-Jarvis` (`orchestrator.rs`, `conversation.rs`)
- 🎯 Capa: Backend orquestador
- 📌 Objetivo técnico: que la voz del usuario que llega justo después de un despacho **nunca** se descarte como eco ni se responda por separado. Hay que resolver H-T4 (chime audible y `ECHO_TAIL`), H-T5 (fusión en `Speaking` temprano) y H-T6 (carrera STT/`VoiceStart`). Cada caso debe quedar con un test de orquestador con dobles.

**HU-43 · Detector léxico de incompletitud**
- 📁 Repositorio: `Fokkus-Jarvis` (`orchestrator.rs`, `stt.rs`)
- 🎯 Capa: Backend STT y semántica
- 📌 Objetivo técnico: reemplazar `ends_with_connector` por un puntaje de incompletitud en español chileno que cubra muletillas, preposiciones y artículos finales, y verbos colgados. Lograr que whisper conserve las disfluencias. Tabla de verdad con más de 60 casos en tests.

**HU-44 · Turnos largos sin corte duro**
- 📁 Repositorio: `Fokkus-Jarvis` (`vad/endpoint.rs`, `events.rs`, `orchestrator.rs`)
- 🎯 Capa: Backend audio
- 📌 Objetivo técnico: que alcanzar `max_segment_ms` produzca un fragmento parcial que se transcribe y acumula, no un turno despachado. Techo total de 120 s, con memoria acotada.

**HU-45 · Protocolo de directivas de prosodia**
- 📁 Repositorio: `Fokkus-Jarvis` (`tts/phrase.rs`, `brain/client.rs`)
- 🎯 Capa: Backend TTS y prompt
- 📌 Objetivo técnico: definir la gramática de directivas (§3.2), un parser incremental robusto a tags partidos, y la extensión del `SYSTEM_PROMPT`. Los tags nunca deben llegar al audio ni al HUD. Tests de fuzz con tags rotos.

**HU-46 · Render expresivo sobre Edge-TTS**
- 📁 Repositorio: `Fokkus-Jarvis` (`tts/edge.rs`, `tts/mod.rs`, `speech_queue.rs`, `playback.rs`)
- 🎯 Capa: Backend TTS y audio
- 📌 Objetivo técnico: traducir cada `SpeechDirective` a `rate/pitch/volume` por frase, silencios PCM y clips de respiración cacheados por voz, con defaults heurísticos por puntuación. Que la variación sea perceptible sin sonar caricaturesca; validar con una medición de la varianza de F0 antes y después.

#### Fase 2 — Endpointing inteligente (1-2 sprints)

**HU-47 · Fin de turno en dos etapas con Smart Turn v3**
- 📁 Repositorio: `Fokkus-Jarvis` (nuevo `crates/jarvis-audio/src/turn/`, `vad/endpoint.rs`, `dsp/thread.rs`, `models.rs`)
- 🎯 Capa: Backend audio y ML
- 📌 Objetivo técnico: integrar el clasificador acústico (ONNX, BSD-2) como árbitro de los candidatos de silencio, con un protocolo Hold/Commit/Resume entre el DSP y el árbitro que no bloquee el hilo DSP. Verificar el modelo por hash (como whisper en `models.rs`). Elegir runtime (`tract` u `ort`) con un benchmark de latencia y `cargo deny` en verde. Fallback al comportamiento actual si el modelo no carga.

**HU-48 · Perfil de pausas adaptativo**
- 📁 Repositorio: `Fokkus-Jarvis` (`turn/`, `config.rs`)
- 🎯 Capa: Backend
- 📌 Objetivo técnico: ajustar `candidate_ms` y `max_wait` a la distribución real de pausas del usuario (HU-41), con límites seguros y persistencia local.

**HU-49 · Indicador "sigo escuchando" y modo dictado**
- 📁 Repositorio: `Fokkus-Jarvis` (`orchestrator.rs`, overlay/HUD)
- 🎯 Capa: Backend y HUD
- 📌 Objetivo técnico: cuando el árbitro retiene el turno, la esfera muestra un estado visual distinto (sin audio). Agregar el modo "toma nota", que cierra solo con una frase explícita o con un silencio largo.

**HU-50 · Continuidad de entonación**
- 📁 Repositorio: `Fokkus-Jarvis` (`tts/phrase.rs`, `conversation.rs`)
- 🎯 Capa: Backend TTS
- 📌 Objetivo técnico: agrupar frases con la misma directiva para la síntesis sin empeorar el time-to-first-audio (primera frase sola). Medir el TTFA antes y después.

#### Fase 3 — Premium / experimental

**HU-51 · Backend Azure Speech con SSML completo** (requiere decisión de costo del PO)
- 📁 `Fokkus-Jarvis` (`tts/azure.rs`, keyring) · 🎯 Backend TTS
- 📌 `ProsodyRenderer` con `express-as` y `styledegree`, `<break>` y `<emphasis>`. La key va en el llavero, como la de Anthropic. Hay que verificar con la API qué voces es-* tienen estilos.

**HU-52 · TTS local neural**
- 📁 `Fokkus-Jarvis` (`tts/`) · 🎯 Backend TTS
- 📌 Reemplazar espeak-ng por Kokoro-82M ONNX como fallback offline. Spike de Chatterbox Multilingual (ONNX de la comunidad o servicio aislado, **sin pip**) para medir latencia y calidad en español en el hardware del usuario.

**HU-53 · STT especulativo y pre-generación**
- 📁 `Fokkus-Jarvis` (`stt.rs`, `turn/`, `conversation.rs`) · 🎯 Backend
- 📌 Transcribir y arrancar el LLM en el candidato de silencio, y descartar si hay `Resume`. Evaluar LocalAgreement-2 para tener parciales en vivo. Medir el costo de CPU.

**HU-54 · Corpus y suite QA de turn-taking y expresividad** (transversal, dueño: Claude QA)
- 📁 `Fokkus-Jarvis` (`tests/fixtures/turns/`, `make test-turns`) · 🎯 QA
- 📌 Corpus etiquetado de más de 100 grabaciones del usuario (pausas, respiraciones, muletillas, monólogos y turnos cortos) para medir la tasa de cortes prematuros, la latencia de cierre y la tasa de falsos "hold". Protocolo de A/B ciego para la voz.

### Orden recomendado

`HU-41 → HU-42 → HU-43 → HU-45 → HU-46 → HU-44` (Fase 1, el usuario nota la mejora de inmediato) → `HU-54` en paralelo desde el inicio → `HU-47` (el cambio de fondo) → resto.

### Decisiones pendientes del PO

1. Runtime ONNX: `tract` (Rust puro) u `ort` (binario nativo de ONNX Runtime).
2. ¿El chime de aceptación pasa a ser solo visual, o se difiere?
3. ¿Se habilita un backend pago (Azure) para tener estilos reales?
4. Licencia del modelo de LiveKit: se descarta y se usa Smart Turn (BSD-2), salvo que el PO indique lo contrario.

---

## 5. Fuentes

- Pipecat Smart Turn: <https://github.com/pipecat-ai/smart-turn> · <https://huggingface.co/pipecat-ai/smart-turn-v3> · <https://docs.pipecat.ai/server/utilities/turn-detection/smart-turn-overview>
- LiveKit turn detector: <https://docs.livekit.io/agents/build/turns/turn-detector> · <https://livekit.com/blog/improved-end-of-turn-model-cuts-voice-ai-interruptions-39>
- TEN Turn Detection / TEN VAD: <https://github.com/TEN-framework/ten-turn-detection> · <https://huggingface.co/TEN-framework/ten-vad>
- whisper_streaming (LocalAgreement): <https://github.com/ufal/whisper_streaming> · <https://arxiv.org/abs/2307.14743>
- edge-tts y la restricción de SSML: <https://github.com/rany2/edge-tts>
- Chatterbox: <https://huggingface.co/ResembleAI/chatterbox> · <https://huggingface.co/KitsuMate/chatterbox-multilingual-ONNX>
- Kokoro (voces en español): <https://huggingface.co/simonfxr/kokoro.cpp-GGUF>
- Panorama TTS 2026: <https://www.bentoml.com/blog/exploring-the-world-of-open-source-text-to-speech-models>
- Estilos de es-MX-JorgeNeural (terceros, sin verificar): <https://json2video.com/ai-voices/azure/voices/es-mx-jorgeneural>
