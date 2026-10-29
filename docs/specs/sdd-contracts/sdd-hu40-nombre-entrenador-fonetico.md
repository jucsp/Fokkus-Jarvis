# Contrato SDD — HU-40: Nombre personalizable y entrenador fonético local de wake-word

**Proyecto:** Fokkus-Jarvis
**Épica:** 8 — Personalización de Identidad y Activador Vocal Adaptativo
**Redactado por:** Claude (PM/QA)
**Destinatario:** Swarm de desarrollo (Dev Senior)
**Fecha:** 2026-10-16
**Versión base del código:** `v0.2.14`
**Prioridad:** Baja
**Estado:** Borrador para aprobación del PO
**Contratos relacionados:** [HU-36](sdd-hu36-burbujas-chat-efimeras-hud.md) (feedback visual del entrenamiento)

---

## 1. Contexto

El usuario quiere llamar al asistente con otro nombre ("Viernes", "Nova", "Kai") y que la activación funcione **con su voz y su acento**. Whisper transcribe nombres propios de forma inestable ("Jarvis" → "yarvis", "llarvis", "charvis"…); la solución actual (HU-32) es una lista fija que **solo** existe para "jarvis".

### 1.1. Estado actual verificado

| Pieza | Hoy | Brecha |
|---|---|---|
| `activation::DEFAULT_WAKE_WORD` | `"jarvis"` | — |
| `JARVIS_PHONETIC_VARIANTS` | 16 variantes **cableadas**; `contains_wake_word` y `strip_wake_word` solo las aplican si la palabra es la de por defecto. | Un nombre nuevo no tiene variantes. |
| `ActivationDetector::with_wake_word` | Existe, pero `jarvis-app/src/main.rs:362` construye con `ActivationDetector::new()`: **el nombre no es configurable** en la práctica. `wake_word: String` es inmutable tras construir. | Falta campo en config, cambio en caliente y variantes por usuario. |
| `WhisperEngine::with_initial_prompt` | Existe y no se usa para sesgar hacia el nombre. | Oportunidad barata de mejorar el acierto. |
| `brain::SYSTEM_PROMPT` | "Eres Jarvis, el asistente de desarrollo de Fokkus…" (constante). | La persona no cambia con el nombre. |
| Ajustes Unificados | PyQt6 en subproceso; ya existe pestaña **Escucha**. | Falta la UI de entrenamiento y un canal hacia el daemon (el daemon es dueño del micrófono). |

---

## 2. Criterios de aceptación (Given-When-Then)

**CA-40.1 — Nombre configurable**
- *Given* `assistant_name = "Viernes"` en `config.toml` (o en Ajustes → Escucha)
- *When* el usuario dice "Viernes, qué hora es"
- *Then* el turno se acepta con la consulta `"qué hora es"`, y "Jarvis, qué hora es" **ya no** activa (salvo que el usuario mantenga "Jarvis" como alias).

**CA-40.2 — Cambio en caliente**
- *Given* el daemon corriendo
- *When* se guarda un nombre nuevo en Ajustes
- *Then* la activación usa el nombre nuevo en el siguiente turno, sin reiniciar; la persona del cerebro dice "Soy Viernes" cuando se le pregunta.

**CA-40.3 — Entrenamiento guiado (3 muestras)**
- *Given* Ajustes → Escucha → "Entrenar mi voz"
- *When* el usuario pulsa "Empezar" y dice el nombre 3 veces cuando se le indica (cada muestra con indicador de nivel y cuenta regresiva de 3 s)
- *Then* cada muestra muestra su transcripción ("Muestra 2: 'viernes' ✓"), y al terminar se listan las variantes aprendidas para que el usuario confirme con "Guardar".

**CA-40.4 — Persistencia de variantes**
- *Given* entrenamiento guardado
- *Then* `wake_variants` en `config.toml` contiene las variantes normalizadas (máximo 12) y la activación las reconoce tras reiniciar.

**CA-40.5 — Protección contra falsos positivos**
- *Given* una transcripción de muestra que es una palabra común del español ("mayo", "bien", "hola") o tiene < 3 letras
- *Then* se descarta como variante y la UI explica por qué ("'mayo' es una palabra común; activaría a Jarvis por error").

**CA-40.6 — Activación aislada durante el entrenamiento**
- *Given* un entrenamiento en curso
- *Then* las muestras **no** abren turnos de conversación ni llegan al cerebro, y si el usuario cierra la ventana, el entrenamiento se cancela y la activación vuelve a la normalidad en ≤ 1 s.

**CA-40.7 — Procesamiento local**
- *Then* el audio de las muestras se procesa solo con el Whisper local, nunca se guarda a disco ni se envía a la red.

**CA-40.8 — Sesgo de transcripción**
- *Given* un nombre configurado
- *Then* Whisper usa `initial_prompt` con el nombre (p. ej. `"Viernes."`) y la tasa de acierto en 20 intentos del usuario es ≥ 90 % (medida en el protocolo manual), frente a la línea base sin entrenamiento.

**CA-40.9 — Nombres de dos palabras**
- *Given* `assistant_name = "Señor Kai"`
- *Then* activa con la secuencia de dos tokens y con las variantes aprendidas de la secuencia; no activa con "Kai" solo (salvo que se agregue como alias).

---

## 3. Arquitectura técnica por crate

### 3.1. `jarvis-core`
- Campos nuevos en `JarvisConfig`:
  - `assistant_name: String` (default `"Jarvis"`; 2–24 caracteres, 1–2 palabras, letras/espacios/guiones).
  - `wake_aliases: Vec<String>` (nombres adicionales explícitos, p. ej. mantener "Jarvis").
  - `wake_variants: Vec<String>` (variantes aprendidas, normalizadas, ≤ 12).
- `JarvisConfig::wake_word()` → nombre normalizado en minúsculas.

### 3.2. `jarvis-audio`
- **`activation.rs`**
  - `WakeLexicon`: nombre + alias + variantes aprendidas + variantes **generadas** por reglas fonéticas del español (§3.4). Reemplaza la condición "solo si es jarvis": `JARVIS_PHONETIC_VARIANTS` pasa a ser las variantes generadas/curadas por defecto para "jarvis".
  - `ActivationDetector` guarda el léxico en `RwLock<Arc<WakeLexicon>>` y expone `set_lexicon` (cambio en caliente; lectura barata por turno).
  - `contains_wake_word`/`strip_wake_word` operan sobre secuencias de tokens (para nombres de 2 palabras).
  - Modo entrenamiento: `TrainingGate` (`AtomicBool`) que hace que `process_text` devuelva `Rejected` y redirija la transcripción al entrenador.
- **`wake_training.rs`** (nuevo): `WakeTrainer` — sesión de 3 muestras; para cada muestra, el orquestador entrega el **siguiente segmento cerrado por el VAD** (≤ 3 s) al `Transcriber` (sin `initial_prompt`, para no sesgar el aprendizaje), normaliza, filtra y acumula.
- **`orchestrator.rs`**: `VoiceOrchestrator::set_wake_lexicon(WakeLexicon)` y `begin_training`/`end_training`; el hilo `jarvis-stt` recibe un destino alternativo para las transcripciones mientras dura la sesión.
- **`stt.rs`**: `set_initial_prompt` en caliente (hoy solo builder).
- **`brain`**: `SYSTEM_PROMPT` pasa a plantilla `system_prompt(name: &str)`; `CliBrain`/`ClaudeClient::with_system_prompt` ya existen.

### 3.3. `jarvis-ipc` (canal UI ↔ daemon)
La UI de Ajustes (PyQt6) habla con el daemon por el **socket existente** `~/.fokkus/jarvis.sock` (JSON-RPC 2.0, misma política de peers `PeerPolicy`), sin abrir el micrófono por su cuenta:

| Método | Parámetros | Resultado |
|---|---|---|
| `jarvis.wakeTraining.begin` | `{ name }` | `{ session, samples_needed: 3 }` |
| `jarvis.wakeTraining.captureSample` | `{ session, index }` (bloquea ≤ 6 s) | `{ transcript, normalized, accepted, reason? , level_peak }` |
| `jarvis.wakeTraining.preview` | `{ session }` | `{ variants: [..], rejected: [{text, reason}] }` |
| `jarvis.wakeTraining.commit` | `{ session }` | `{ saved_variants }` (escribe `config.toml` y aplica en caliente) |
| `jarvis.wakeTraining.cancel` | `{ session }` | `{ ok }` |

Una sola sesión a la vez; la sesión expira a los 120 s sin actividad.

### 3.4. Generación y filtrado de variantes (función pura)

1. **Normalización:** minúsculas, NFD sin diacríticos (conservando `ñ`), solo `[a-zñ ]`.
2. **Reglas fonéticas del español** (generación de candidatos desde el nombre): `j↔y↔ll↔ch↔sh` en ataque silábico, `b↔v`, `c(e,i)↔s↔z`, `k↔c(a,o,u)↔qu`, `h` muda inicial, `x↔ks↔s`, `-s` final opcional. Se generan como máximo 16 y se ordenan por distancia fonética.
3. **Aprendidas:** transcripciones normalizadas de las muestras que **no** coinciden con el nombre.
4. **Filtros** (se aplican a generadas y aprendidas):
   - longitud ≥ 3;
   - no pertenece a la **lista de palabras frecuentes del español** embebida (≈ 5 000 palabras, `include_str!`);
   - distancia de Levenshtein fonética al nombre ≤ ⌈len/3⌉ (evita que una mala muestra — "¿qué?" — entre al léxico);
   - no es prefijo/sufijo de otra palabra frecuente de longitud ≤ len + 2 (evita "jarvis" ⊂ "jarvisito", ya cubierto por la coincidencia por token completo).
5. **Tope:** 12 variantes, priorizando aprendidas (con frecuencia ≥ 1) sobre generadas.

---

## 4. Interfaces y structs Rust

```rust
// jarvis-core/src/config.rs
pub struct JarvisConfig {
    // …
    pub assistant_name: String,
    pub wake_aliases: Vec<String>,
    pub wake_variants: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum NameError { Empty, TooLong, TooManyWords, InvalidChars }

pub fn validate_assistant_name(name: &str) -> Result<String, NameError>; // devuelve normalizado

// jarvis-audio/src/activation.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeLexicon {
    /// Secuencias de tokens que activan, p. ej. [["senor","kai"], ["senor","cai"]].
    entries: Vec<Vec<String>>,
    display_name: String,
}

impl WakeLexicon {
    pub fn from_config(cfg: &JarvisConfig) -> Self;
    pub fn display_name(&self) -> &str;
    /// Índices [inicio, fin) de la primera coincidencia por tokens completos.
    pub fn find(&self, tokens: &[String]) -> Option<(usize, usize)>;
}

impl ActivationDetector {
    pub fn with_lexicon(self, lexicon: WakeLexicon) -> Self;
    pub fn set_lexicon(&self, lexicon: WakeLexicon);
    pub fn set_training(&self, active: bool);
}

// jarvis-audio/src/wake_training.rs
pub fn normalize_token(s: &str) -> String;
pub fn phonetic_candidates(name: &str, max: usize) -> Vec<String>;
pub fn phonetic_distance(a: &str, b: &str) -> usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason { TooShort, CommonWord, TooFar, Empty, NoSpeech, Clipped }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleResult {
    pub index: u8,
    pub transcript: String,
    pub normalized: String,
    pub accepted: bool,
    pub reason: Option<RejectReason>,
}

pub struct WakeTrainer { /* name, samples: Vec<SampleResult>, started_at */ }
impl WakeTrainer {
    pub const SAMPLES: u8 = 3;
    pub fn new(name: &str) -> Result<Self, NameError>;
    pub fn add_sample(&mut self, transcript: &str, peak_level: f32) -> SampleResult;
    pub fn preview(&self) -> (Vec<String>, Vec<(String, RejectReason)>);
    pub fn finish(self) -> Vec<String>; // variantes finales (≤ 12)
}

// jarvis-audio/src/brain/client.rs
pub fn system_prompt(name: &str) -> String; // SYSTEM_PROMPT pasa a ser system_prompt("Jarvis")
```

---

## 5. Casos de borde

| Caso | Comportamiento |
|---|---|
| Nombre que es palabra común ("Luz", "Sol", "Hola") | Se permite con advertencia en la UI ("activará con conversación normal"); se recomienda alias de dos palabras ("Hola Luz"). |
| Muestra en silencio o con ruido | `NoSpeech` (VAD no cerró segmento en 6 s) → se repite esa muestra. |
| Muestra saturada (pico ≥ 0.99) | `Clipped` → se pide hablar más lejos del micrófono. |
| Las 3 muestras dan la misma transcripción exacta del nombre | Éxito sin variantes aprendidas: Whisper ya acierta; solo se guardan las generadas que pasan filtros. |
| Las 3 muestras difieren entre sí y del nombre | Se aceptan las que pasan filtros; si ninguna pasa, la UI sugiere repetir o elegir un nombre más distintivo. |
| Daemon en `Muted` al iniciar el entrenamiento | `begin` devuelve error "Activa el micrófono para entrenar". |
| Daemon sin transcriptor cargado (modelo descargándose) | `begin` devuelve error con el progreso de descarga. |
| Usuario cambia el nombre sin entrenar | Se usan nombre + variantes generadas; `wake_variants` previas de otro nombre **se descartan** (pertenecían al nombre anterior). |
| `config.toml` editado a mano con 50 variantes | Se truncan a 12 al cargar, con aviso en el log. |
| Persona del cerebro CLI | El nombre se inyecta en el prompt del `CliBrain`; si el usuario usa `{prompt}` en su comando, igual se respeta. |
| Frases fijas que dicen "Jarvis" (`NO_CLI_NOTICE`, fillers) | Se revisan; las que nombran al asistente usan `display_name`; los fillers pre-sintetizados en caché se invalidan si contienen el nombre. |

---

## 6. Plan de tests

**Unitarios**
- `validate_assistant_name`: vacío, 25 caracteres, 3 palabras, dígitos, tildes ("Ámbar" → válido).
- `normalize_token`: "¡Viérnes!" → "viernes"; conserva `ñ`.
- `phonetic_candidates("jarvis")` ⊇ las 11 variantes actuales de HU-32 (regresión: no se pierde acierto existente).
- `phonetic_candidates("viernes")` contiene "biernes"; no contiene palabras de la lista frecuente.
- Filtros: "mayo", "si", "que" rechazadas con el motivo correcto; "yarvis" aceptada para "jarvis".
- `WakeLexicon::find` con nombres de 1 y 2 palabras; "jarvisito" no activa; "señor kai" activa, "kai" solo no.
- `WakeTrainer`: 3 muestras iguales al nombre; 3 distintas válidas; 1 válida + 2 rechazadas; tope de 12.
- Todos los tests actuales de `activation.rs` siguen verdes con `WakeLexicon::from_config(&JarvisConfig::default())`.

**Integración**
- Orquestador con `Transcriber` falso: con `set_training(true)`, tres segmentos producen `SampleResult` y **ningún** `BrainJob::Turn`; `cancel` restaura la activación.
- IPC: secuencia `begin → captureSample×3 → preview → commit` escribe `config.toml` (ruta temporal) y el detector activa con la variante nueva sin reiniciar.
- `set_lexicon` concurrente con `process_text` en otro hilo (sin bloqueos ni pánicos).

**Manual (equipo de referencia, acento chileno)**
- Línea base: 20 intentos de "Viernes, qué hora es" sin entrenamiento → registrar tasa.
- Entrenar con 3 muestras → 20 intentos → tasa ≥ 90 % y ≥ línea base.
- 10 minutos de conversación de oficina/llamada de fondo con el nombre nuevo: 0 activaciones falsas.
