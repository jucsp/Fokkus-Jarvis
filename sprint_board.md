# Sprint Board — Fokkus-Jarvis

**Épica 6:** Experiencia Visual Adaptativa y Personalización de Interfaz
**Sprint:** HU-33 (tamaño dinámico) + HU-34 (opacidad variable)
**Actualizado:** 2026-10-16 · **PM/QA:** Claude
**Contratos:** [HU-33](docs/specs/sdd-contracts/sdd-hu33-tamano-dinamico-burbuja.md) · [HU-34](docs/specs/sdd-contracts/sdd-hu34-opacidad-transparencia-esfera.md)

Leyenda: `[x]` hecho y verificado con tests · `[ ]` pendiente · 🧪 requiere prueba manual

---

## HU-34 — Opacidad y transparencia variable de la esfera

### jarvis-core
- [x] 34.1 `Opacity` (newtype `u8`): `OPAQUE`, `from_pct` (piso 20 %), `as_u8`, `mul`, `is_opaque` [Responsable: DeepSeek]
- [x] 34.2 `sphere_opacity_pct` en `JarvisConfig` (default 100, saturado al cargar) + `sphere_opacity()` [Responsable: DeepSeek]
- [x] 34.3 Fix de visibilidad: `Opacity::from_raw(u8)` (`const fn`) para fundidos y atenuación automática [Responsable: Claude]
- [x] 34.4 Getter `sphere_opacity_pct()` saturado (log de `apply_config`) [Responsable: Claude]

### jarvis-render
- [x] 34.5 `apply_opacity(&mut Pixmap, Opacity)` con `(c*a+127)/255` y atajo para opaco [Responsable: DeepSeek]
- [x] 34.6 `OpacityFade` (`new`, `retarget`, `value_at`, `is_settled`) [Responsable: DeepSeek]
- [x] 34.7 Migración a `Opacity::from_raw` (el campo ya no se construye desde fuera de `jarvis-core`) [Responsable: Claude]

### jarvis-shell
- [x] 34.8 `rgba_premul_to_argb8888_with_alpha(src, dst, opacity)`; `rgba_premul_to_argb8888` delega con `Opacity::OPAQUE` [Responsable: Claude]
- [x] 34.9 `OverlaySurface::set_opacity` (default no-op) [Responsable: Claude]
- [x] 34.10 `WaylandLayerSurface::set_opacity`: la opacidad se aplica en el pase de conversión a `wl_shm` (un solo recorrido) [Responsable: Claude]
- [x] 34.11 `HeadlessSurface::set_opacity` + getter `opacity()` para tests [Responsable: Claude]
- [x] 34.12 Tests: equivalencia con la ruta opaca, blanco 50 % → `(128,128,128,128)`, invariante premultiplicado en 10⁴ píxeles [Responsable: Claude]

### jarvis-app
- [x] 34.13 Slider "Opacidad" 20–100 % (paso 5) en General → Esfera [Responsable: Claude]
- [x] 34.14 Vista previa `{"preview":{"sphere_opacity_pct":Y}}` ≤ 30 Hz, aplicada con fundido de 150 ms en `run_loop` [Responsable: Claude]
- [ ] 34.15 Composición con la atenuación automática de HU-35 (`user × auto`) — depende de HU-35 [Responsable: DeepSeek]
- [ ] 🧪 34.16 Prueba manual: esfera al 30 % sobre un editor, colores de estado distinguibles [Responsable: Claude]
- [ ] 34.17 Benchmark de sobrecoste (≤ 0.3 ms a 220 px, ≤ 1.2 ms a 440 px) [Responsable: DeepSeek]

---

## HU-33 — Tamaño dinámico de la burbuja en tiempo real

### jarvis-core
- [x] 33.1 `DEFAULT_SPHERE_SIZE_PX`, `SPHERE_SCALE_RANGE`, `sphere_scale_pct` (default 100, saturado al cargar) y `sphere_size_px()` [Responsable: DeepSeek]
- [x] 33.2 `sphere_size_for_pct(pct)` compartido por config y vista previa [Responsable: Claude]

### jarvis-render
- [x] 33.3 `SphereRenderer::resize(size)` en O(1) [Responsable: DeepSeek]

### jarvis-shell
- [x] 33.4 `OverlaySurface::set_size` (default `Err(BackendUnavailable("resize"))`) [Responsable: Claude]
- [x] 33.5 `WaylandLayerSurface::set_size`: `layer.set_size` + commit, espera del `configure` (timeout 250 ms con log único), reutiliza/crece el `SlotPool`, recalcula región de entrada circular y re-satura los márgenes de drag; si el compositor no responde, restaura el tamaño anterior [Responsable: Claude]
- [x] 33.6 `HeadlessSurface::set_size` (revalida los frames contra el tamaño nuevo) [Responsable: Claude]
- [x] 33.7 Tests: `circle_strips` a 110/440 px y márgenes re-saturados tras crecer [Responsable: Claude]

### jarvis-app
- [x] 33.8 Slider "Tamaño de la esfera" 50–200 % (paso 5) en General → Esfera, valor cargado del TOML y botón "Guardar" [Responsable: Claude]
- [x] 33.9 Emisión `{"preview":{"sphere_scale_pct":X}}` limitada a 30 Hz (`PreviewThrottle` con `QTimer`, coalescencia y flush al soltar) [Responsable: Claude]
- [x] 33.10 `UnifiedSettings` con `stdout` piped + hilo `jarvis-settings-preview` → `bounded(16)` descartando el más viejo; `parse_preview_line` y `poll_preview` [Responsable: Claude]
- [x] 33.11 `SphereView` (renderer + pixmap, reasigna solo al cambiar) y `SphereLook` en `run_loop`: como máximo un cambio de tamaño por frame [Responsable: Claude]
- [x] 33.12 Arranque con el tamaño/opacidad persistidos; `Saved` relee el TOML, `Cancelled`/proceso muerto revierte al persistido [Responsable: Claude]
- [x] 33.13 Tests: parseo (válido, fuera de rango, basura), lector con canal lleno, `SphereView::resize`, bucle con último valor por frame y reversión [Responsable: Claude]
- [ ] 33.14 Cerrar el `xdg_popup` del menú antes del resize y re-posicionar el HUD (HU-36) [Responsable: DeepSeek]
- [ ] 33.15 HiDPI: buffer en píxeles físicos con `buffer_scale` (CA-33.7) [Responsable: DeepSeek]
- [ ] 33.16 Test de no-asignación del bucle (1 000 frames sin resize → 0 asignaciones) [Responsable: DeepSeek]
- [ ] 🧪 33.17 Prueba manual en KWin: slider 50→200→50 sin parpadeo, clic en esquinas *click-through*, `--diag` a 200 % [Responsable: Claude]

---

## Verificación (2026-10-16)
- `cargo test --workspace`: ✅ 398 passed, 0 failed (2 ignored preexistentes)
- `cargo clippy --workspace --all-targets -- -D warnings`: ✅ limpio
- `cargo fmt --all -- --check`: ✅ limpio
- `unified_settings.py` en Qt offscreen: ráfaga de 151 cambios → 2 líneas JSON; valores saturados y alineados al paso; TOML guardado con `sphere_scale_pct`/`sphere_opacity_pct`
