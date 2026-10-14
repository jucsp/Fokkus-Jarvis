# Reporte QA — Issue #1 (Fase 1): Núcleo nativo y ventana overlay

**Emisor:** Claude Code (PM/QA del Swarm)
**Fecha:** 2026-10-14
**Entrega auditada:** workspace Cargo + `jarvis-core`, `jarvis-render`, `jarvis-shell`, `jarvis-app`, `Makefile` (implementación DeepSeek V4 Pro)
**Referencias:** `docs/specs/dictamen-qa-fase1.md` §6 (tareas 1.0–1.8), `backlog.md` Issue #1
**Veredicto:** ⚠️ **ENTREGA PARCIAL APROBADA — Issue #1 NO SE CIERRA**

---

## 0. Contexto de Negocio

El Issue #1 debe entregar el "cuerpo" visible de Jarvis: una esfera flotante siempre encima, en todos los escritorios, sin marco y con ≤ 40 MB de RAM. La entrega actual construye bien los cimientos (estados, bus, render de la esfera, contrato de superficie), pero **todavía no abre ninguna ventana**: la esfera se renderiza en memoria y se valida contra una superficie *headless*. El usuario aún no puede ver a Jarvis en su escritorio.

---

## 1. Puertas de calidad (evidencia)

| Comando | Antes de QA | Después de QA |
|---|---|---|
| `make test` | ✅ 18 passed | ✅ 21 passed, 0 failed, 1 ignored (benchmark) |
| `make check` | ✅ | ✅ |
| `make fmt-check` | ❌ diffs en `jarvis-render`, `jarvis-shell`, `jarvis-core`, `jarvis-app` | ✅ |
| `make lint` (`clippy -D warnings`) | ❌ 4 errores (`derivable_impls` ×2, `field_reassign_with_default` ×2) | ✅ |
| `make audit` | ⚪ SKIP (`cargo-audit` no instalado) | ⚪ SKIP |
| `make run` | ✅ demo headless, flujo Idle→Listening→Thinking→Speaking→Idle | ✅ |
| Benchmark render 220×220 (release) | — | ✅ 1.66 ms/frame (< 2 ms) |

### Correcciones aplicadas por QA
1. `jarvis-shell`: `impl Default` manuales de `Anchor` y `KeyboardInteractivity` → `#[derive(Default)]` + `#[default]`.
2. `jarvis-render`: `Paint` y `Stroke` construidos con inicializador de struct en vez de reasignar campos.
3. `cargo fmt --all` en todo el workspace.
4. Tests nuevos:
   - `jarvis-core::state::tests::transition_table_is_fully_covered` — recorre las 25 combinaciones origen→destino (criterio 1.2: cobertura 100 % de la tabla) y verifica que un rechazo no altera el estado.
   - `reset_returns_to_idle`, `predicates_match_state`.
   - `jarvis-render::tests::render_budget_under_2ms_in_release` (`#[ignore]`; ejecutar con `cargo test -p jarvis-render --release -- --ignored`).

---

## 2. Auditoría contra criterios de aceptación (dictamen §6)

| ID | Tarea | Estado | Observación |
|---|---|---|---|
| 1.0 | Spike layer-shell + wl_shm + RSS < 25 MB | ❌ | No hay ventana real ni medición de RSS. |
| 1.1 | Workspace, `rust-toolchain.toml`, Makefile (`build/test/lint/run/audit`) | ✅ | Toolchain fijado a `stable` (= 1.99.0 local), no a `1.99`; `rust-version = "1.99"` en workspace. Aceptado. |
| 1.2 | Máquina de estados pura + bus `crossbeam-channel`, tabla 100 % cubierta | ✅ | Cobertura completa tras tests de QA. |
| 1.3 | Esfera líquida multi-armónica, `level` 0–1, paleta por estado, < 2 ms/frame | ⚠️ | Cumple tiempo (1.66 ms, margen estrecho). **La deformación no depende del tiempo**: `phase_for(state)` es constante, por lo que con `level` fijo la esfera queda estática (no "líquida"). Falta parámetro `t`. |
| 1.4 | Backend `wayland_layer` (sctk, Overlay, input region, drag) | ❌ | Solo trait `OverlaySurface` y tipos. |
| 1.5 | Backend `winit_window` + selección automática por global `zwlr_layer_shell_v1` | ❌ | `SurfaceBackend::detect()` usa solo `WAYLAND_DISPLAY`: en GNOME elegiría `WaylandLayer` erróneamente. Marcado como provisional en el código. |
| 1.6 | Instancia única (lock) + config TOML en `~/.config/fokkus-jarvis/config.toml` | ❌ | `JarvisConfig` existe solo en memoria, sin persistencia ni lock. |
| 1.7 | `fmt --check`, `clippy -D warnings`, `cargo-deny`, snapshot de pixmap | ⚠️ | fmt y clippy en verde (tras QA). Falta `cargo-deny` y snapshot con tolerancia. |
| 1.8 | Medición RSS ≤ 40 MB y CPU reposo < 1 % | ❌ | Bloqueado por 1.0/1.4. |

**Resumen:** 2 ✅ · 2 ⚠️ · 5 ❌.

---

## 3. Hallazgos de código para la siguiente iteración

| # | Archivo | Severidad | Hallazgo |
|---|---|---|---|
| H1 | `jarvis-render/src/lib.rs` (`phase_for`, `render`) | 🟠 Alta | Sin entrada temporal: la esfera no se anima en reposo. Añadir `t: f32` (segundos) a `render` y desplazar las fases por armónico. |
| H2 | `jarvis-render/src/lib.rs` (`render`) | 🟡 Media | `Pixmap::new` por frame = 193,6 KB de asignación a 60 fps. Ofrecer `render_into(&mut Pixmap, …)` reutilizando el buffer (también reduce el tiempo de 1.66 ms). |
| H3 | `jarvis-render/src/lib.rs` | 🟡 Media | `expect()` en código de producción (`Pixmap::new`, `builder.finish()`), contrario a la regla del dictamen. Devolver `Option`/`Result`. |
| H4 | `jarvis-shell/src/lib.rs` (`detect`) | 🟡 Media | Heurística por variable de entorno; sustituir por sonda del registro Wayland (1.5). |
| H5 | `jarvis-core/src/bus.rs` | 🟢 Baja | Canal de audio `bounded(64)` + `try_send`: cuando se llena se descartan las muestras **nuevas** y el consumidor ve niveles viejos. Para un medidor conviene "último valor gana" (p. ej. `AtomicU32` con bits de `f32`). |
| H6 | `jarvis-shell/src/lib.rs` (`FrameBuffer`) | 🟢 Baja | Documenta RGBA premultiplicado; `wl_shm` ARGB8888 es BGRA little-endian → el backend 1.4 debe hacer el swizzle R↔B. Dejarlo explícito en el contrato. |

---

## 4. Conclusión

| Punto | Estado |
|---|---|
| Compilación (`make check`) | ✅ |
| Tests (`make test`) | ✅ 21/21 (+1 benchmark ignorado, pasa en release) |
| Formato (`make fmt-check`) | ✅ (corregido por QA) |
| Lint (`make lint`) | ✅ (corregido por QA) |
| Auditoría de dependencias (`make audit` / `cargo-deny`) | ⚪ No ejecutada: herramienta no instalada |
| Núcleo de estados y bus | ✅ |
| Render de esfera | ⚠️ Estática en el tiempo (H1) |
| Ventana overlay real (Wayland/winit) | ❌ No implementada |
| Instancia única + config persistente | ❌ No implementada |
| RSS ≤ 40 MB | ❌ No medible aún |

**Decisión:** el Issue #1 **permanece abierto**. Las tareas 1.1 y 1.2 se dan por cerradas; 1.3 y 1.7 quedan parciales. Para cerrar el Issue se requieren 1.0/1.4 (ventana layer-shell en KWin), 1.5, 1.6 y 1.8, más H1.

**Pendiente del usuario/PO:** instalar `cargo-deny` (o `cargo-audit`) para cubrir el riesgo R4 de supply chain, y definir si la persistencia TOML puede usar los crates `serde` + `toml` (no están en la lista aprobada).
