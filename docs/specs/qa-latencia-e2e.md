# QA — Auditoría de latencia E2E de Jarvis

**Fecha:** 2026-10-29 · **Rol:** Claude (PM/QA) · **Config auditada:** `brain_mode = "cli"`, `agy --model gemini-3.8-flash-medium --dangerously-skip-permissions -p`, STT `whisper-base-q5_1`, TTS Edge `es-US-AlonsoNeural`.

## Contexto de negocio

El usuario reporta que Jarvis "tarda bastantes segundos" en responder y no sabe si el problema es el procesamiento local o la API. Esta auditoría mide cada tramo de la cadena **fin de voz → primer audio de la respuesta**, corrige lo que estaba mal en el código y deja propuestas cuantificadas para el resto.

## 1. Mediciones (este equipo, 16 hilos, binario release)

| Tramo | Cómo se midió | Resultado |
|---|---|---|
| Silencio de cierre (endpointing) | `VoiceEndpointConfig::default` | **1 200 ms** (+500 ms si el turno termina en conector) |
| STT whisper `base-q5_1` (CPU, 8 hilos) | `bench_base_q5_1_inference_latency` con 6 s de voz real | **596–649 ms** |
| Planner LLM (solo turnos compuestos) | Código: `PLANNER_TIMEOUT` 2,5 s contra el arranque en frío del CLI | **+2 500 ms perdidos** (siempre vencía y caía a la heurística) |
| `agy -p` arranque en frío (hasta el evento `init`) | `--output-format stream-json` con marcas de tiempo | **3,2–3,9 s** por turno (binario + MCP `uvx`/`npx`/`uv`) |
| Prompt enviado al modelo | `usage.input_tokens` de `agy` | **~14 900 tokens** de harness de `agy` (60 herramientas) + ~1 k de persona Jarvis + historial (hasta 20 turnos); `cache_read_tokens = 0` |
| `agy -p` en modo `text` | TTFB contra total | Salida **bufferizada**: el primer byte llega 0,3–0,5 s antes del final. El TTS por frases no podía adelantarse |
| Edge-TTS primera frase | CLI `edge-tts` (incluye arranque de Python) | 1,2–1,5 s (la librería Rust debería ser menor; no medido aislado) |

### Cerebro real, primer token (`bench_cli_time_to_first_token`, 3 turnos)

| Modelo | Proceso por turno (antes) | Sesión persistente (ahora) |
|---|---|---|
| `gemini-3.8-flash-medium` | 10,7 / 7,4 / 8,3 s | **5,0 / 4,0 / 6,9 s** |
| `gemini-3.8-flash-low` | 7,0 / 5,0 / 5,2 s | **1,9 / 1,5 / 4,8 s** |

**Diagnóstico:** la mayor parte del tiempo no es procesamiento local. El cuello de botella es el cerebro: (a) relanzar `agy` en cada turno (~3,5 s), (b) el razonamiento de `flash-medium` (~2–3 s más que `low`) y (c) los ~15 k tokens de harness que `agy` antepone a cada turno. STT y endpointing suman ~1,8 s fijos.

## 2. Respuestas a las preguntas del brief

- **¿Cold start del CLI por turno?** Sí, 3,2–3,9 s en cada turno. Ya corregido (§3.1).
- **¿Tokens de prompt y TTFT?** ~16 k por turno; 14,9 k son del propio `agy` y no se controlan desde Jarvis. Con sesión persistente, la persona y el historial viajan solo en el primer mensaje de cada proceso.
- **VAD/endpointing:** cierre a 1 200 ms de silencio; 2 000 ms si el STT detecta un conector ("y…", "pero…"); pre-roll de 300 ms.
- **STT:** ~0,6 s con `base-q5_1` en release. No es el problema.
- **TurnRouter:** va directo (heurística) salvo en turnos compuestos. Ahí llamaba al planner LLM, que en modo CLI **nunca** respondía dentro de 2,5 s. Corregido (§3.2).
- **¿Streaming o buffer?** El coordinador ya hacía streaming por frases (`PhraseSplitter` + `PhraseGrouper`, primera frase sola). Pero `agy -p` en modo `text` solo entrega la salida al terminar, así que en la práctica era buffer completo. Corregido (§3.1).

## 3. Cambios aplicados

### 3.1 Sesión persistente de `agy` con streaming real (`brain/session.rs`, `brain/cli.rs`)
- Si el comando es `agy … -p` (prompt al final, sin `{prompt}` ni formatos propios), el cerebro de **voz** mantiene un proceso vivo con `--input-format stream-json --output-format stream-json`. Envía cada turno como NDJSON (`{"event":"user","message":{…}}`) y entrega los `text_delta` al TTS según llegan.
- `warm_up()` lanza el proceso al construir el cerebro (`settings.rs`), así que el arranque en frío ocurre fuera del camino crítico.
- El primer mensaje de cada proceso lleva la persona y el historial; los siguientes, solo el contexto efímero y el turno (agy conserva la conversación).
- Barge-in: el bucle revisa el token cada 25 ms, mata el grupo de procesos y deja otro calentando. Lo mismo ante *timeout*, error o caída.
- Reciclado cada `history_turns` turnos (20) para acotar el contexto; el nuevo proceso se vuelve a cebar con el historial de Jarvis.
- `Drop` mata la sesión: no quedan `agy` huérfanos al cambiar ajustes o salir.
- Los subagentes (modo `Worker`) y cualquier otro CLI siguen con un proceso por turno.

### 3.2 Planner omitido cuando el cerebro no cabe en su presupuesto (`tasks.rs`, `planner.rs`, `settings.rs`)
- Nuevo `BrainFactory::answers_within(budget)`. En modo CLI devuelve `false` para presupuestos menores a 6 s (arranque en frío medido). El planner devuelve `PlanError::Disabled` al instante sin lanzar procesos, y el router usa la heurística igual que antes, pero **2,5 s antes**.

### 3.3 Benchmarks reproducibles (`#[ignore]`)
- `JARVIS_STT_BENCH_PCM=<pcm f32le 16k> cargo test --release -p jarvis-audio --lib bench_base_q5_1 -- --ignored --nocapture`
- `JARVIS_BENCH_CLI="agy --model … -p" cargo test --release -p jarvis-audio --lib bench_cli_time -- --ignored --nocapture`

## 4. Presupuesto E2E (fin de voz → primer audio de respuesta)

| Tramo | Antes | Ahora (`flash-medium`) | Propuesta (`flash-low` + `silence_ms` 800) |
|---|---|---|---|
| Silencio de cierre | 1,2 s | 1,2 s | 0,8 s |
| STT | 0,6 s | 0,6 s | 0,6 s |
| Planner (compuestos) | +2,5 s | 0 | 0 |
| Cerebro hasta el 1er token | 7,4–10,7 s | 4,0–6,9 s | 1,5–1,9 s (pico 4,8 s) |
| Espera de la respuesta completa | +0,3–0,5 s | 0 (streaming) | 0 |
| Síntesis 1ª frase (Edge) | ~1 s | ~1 s | ~1 s |
| **Total típico** | **~10–13 s** | **~7–9 s** | **~4–4,5 s** |

El filler pregrabado sigue sonando a menos de 300 ms tras el STT (~1,8 s desde que el usuario calla), así que el silencio percibido es menor que el total.

## 5. Propuestas para acercarse a < 1,5 s (no aplicadas)

1. **Modelo `gemini-3.8-flash-low`** en `cli_command` (config del usuario). Es la palanca más grande que queda: −2 a −3 s de TTFT. Implica un trade-off de calidad, por eso no lo cambié.
2. **`[voice_endpoint] silence_ms = 800`**: −400 ms. La extensión por conector (2 s) y la retención por incompletitud mitigan cortar al usuario. Validar contra los P50/P90 de `TurnPauseStats` antes de fijarlo.
3. **Modo `api` directo** (Anthropic con `effort = low` y *prompt caching*): elimina el harness de 15 k tokens y el proceso intermedio. No se midió en esta auditoría (requiere API key). Es la única ruta realista para que el cerebro baje de 1 s de TTFT.
4. **Pre-conexión del websocket de Edge-TTS** (o Piper local para la primera frase): estimado −200 a −500 ms en la primera síntesis.
5. **STT incremental durante el habla** (transcribir fragmentos mientras el usuario sigue hablando): ocultaría los ~0,6 s del STT.

> [!IMPORTANT]
> Con la arquitectura actual, **< 1,5 s desde que el usuario calla hasta escuchar la respuesta no es alcanzable**: solo el endpointing y el STT suman ~1,4–1,8 s. Hoy, la meta de < 1,5 s solo se cumple para el *primer audio* (el filler). Bajar la respuesta real de ~2,5 s requiere combinar las propuestas 2, 3 y 5.

## 6. Verificación

- ✅ `make test`: 947 pasados, 0 fallidos, 6 ignorados (9 tests nuevos: persistencia, reciclado, error, barge-in, worker one-shot, parser NDJSON, args de sesión, mensaje NDJSON y planner omitido).
- ✅ `cargo clippy --workspace --all-targets -D warnings` y `cargo fmt --check` limpios; `cargo deny`: advisories/bans/licenses/sources ok.
- ✅ Benchmark contra el `agy` real: la sesión persistente mantiene el contexto entre turnos ("¿Y quién lo predijo?" respondido sobre el turno anterior).
- ⚠️ El binario instalado (`~/.local/bin/fokkus-jarvis`, 28-oct) es anterior a estos cambios: hay que recompilarlo e instalarlo para notarlos.
