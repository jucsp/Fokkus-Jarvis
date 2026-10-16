//! Detección pura de pantalla completa (HU-35).
//!
//! Define los tipos de dominio que comparten `jarvis-core`, `jarvis-shell` y
//! `jarvis-app` para decidir, sin depender de un compositor concreto, si la
//! ventana activa está ocupando todo el monitor ("efectivamente pantalla
//! completa"). La regla de negocio está en [`is_effectively_fullscreen`].

use serde::{Deserialize, Serialize};

/// Comportamiento de la esfera al detectar una ventana a pantalla completa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FullscreenBehavior {
    /// No se aplica ninguna acción (no se carga el monitor).
    Ignore,
    /// Se atenúa la esfera a [`crate::JarvisConfig::fullscreen_dim_pct`].
    #[default]
    Dim,
    /// Se oculta la esfera; la escucha por voz sigue activa.
    Hide,
}

/// Rectángulo en coordenadas lógicas del compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// Borde derecho (exclusivo), en `i64` para comparar con tolerancia sin
    /// desbordar.
    fn right(&self) -> i64 {
        i64::from(self.x) + i64::from(self.w)
    }

    /// Borde inferior (exclusivo), en `i64` por el mismo motivo.
    fn bottom(&self) -> i64 {
        i64::from(self.y) + i64::from(self.h)
    }
}

/// Instantánea de la ventana activa reportada por el backend de detección.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSnapshot {
    /// Identificador de la aplicación (p. ej. `"firefox"`, `"steam"`).
    pub app_id: String,
    /// Declara explícitamente el estado fullscreen del compositor.
    pub fullscreen: bool,
    /// `false` cuando la ventana no tiene decoraciones (borderless).
    pub decorated: bool,
    /// Geometría de la ventana en coordenadas lógicas.
    pub geometry: Rect,
    /// Nombre del output donde está la ventana (p. ej. `"DP-1"`).
    pub output: String,
    /// `true` si la ventana pertenece a Fokkus-Jarvis (ajustes, panel): se ignora.
    pub own: bool,
}

/// Geometría de un output (monitor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputGeometry {
    pub name: String,
    pub rect: Rect,
}

/// Umbral mínimo de cobertura de área para considerar "borderless fullscreen".
const FULLSCREEN_COVERAGE_PCT: u128 = 98;

/// Tolerancia en píxeles por borde al comparar la geometría con el output.
const EDGE_TOLERANCE_PX: i64 = 2;

/// `true` si los dos bordes están a una distancia máxima de ±2 px.
fn edge_within_tolerance(edge: i64, output_edge: i64) -> bool {
    (edge - output_edge).abs() <= EDGE_TOLERANCE_PX
}

/// Decide si `win` cubre el output como una ventana borderless a pantalla completa.
fn covers_output(win: &Rect, out: &Rect) -> bool {
    // Cada borde debe quedar dentro de la tolerancia de ±2 px del borde del
    // output (admite un pequeño margen interior o exterior).
    let left_ok = edge_within_tolerance(i64::from(win.x), i64::from(out.x));
    let top_ok = edge_within_tolerance(i64::from(win.y), i64::from(out.y));
    let right_ok = edge_within_tolerance(win.right(), out.right());
    let bottom_ok = edge_within_tolerance(win.bottom(), out.bottom());
    if !(left_ok && top_ok && right_ok && bottom_ok) {
        return false;
    }

    // Y el área debe cubrir al menos el 98 % del área del output.
    let win_area = u128::from(win.w) * u128::from(win.h);
    let out_area = u128::from(out.w) * u128::from(out.h);
    win_area * 100 >= out_area * FULLSCREEN_COVERAGE_PCT
}

/// Decide si la ventana activa es "efectivamente pantalla completa" en `out`.
///
/// Reglas (contrato HU-35, §1.2):
/// 1. `own == true` → `false` (las ventanas de Jarvis se ignoran).
/// 2. `out.name != w.output` → `false` (multimonitor: solo afecta al monitor
///    donde está la esfera).
/// 3. `w.fullscreen == true` → `true`.
/// 4. Sin decoraciones y geometría que cubre ≥ 98 % del área del output con
///    tolerancia de ±2 px por borde → `true`.
/// 5. Cualquier otro caso → `false`.
pub fn is_effectively_fullscreen(w: &WindowSnapshot, out: &OutputGeometry) -> bool {
    if w.own {
        return false;
    }
    if out.name != w.output {
        return false;
    }
    if w.fullscreen {
        return true;
    }
    if w.decorated {
        return false;
    }
    covers_output(&w.geometry, &out.rect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    const FULL_HD: Rect = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };

    fn snap(
        app_id: &str,
        fullscreen: bool,
        decorated: bool,
        geometry: Rect,
        output: &str,
        own: bool,
    ) -> WindowSnapshot {
        WindowSnapshot {
            app_id: app_id.to_string(),
            fullscreen,
            decorated,
            geometry,
            output: output.to_string(),
            own,
        }
    }

    fn out(name: &str, rect: Rect) -> OutputGeometry {
        OutputGeometry {
            name: name.to_string(),
            rect,
        }
    }

    #[test]
    fn declared_fullscreen_wins_even_if_small_and_decorated() {
        let w = snap(
            "game",
            true,
            true,
            Rect {
                x: 0,
                y: 0,
                w: 320,
                h: 200,
            },
            "DP-1",
            false,
        );
        assert!(is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn own_windows_are_ignored() {
        let w = snap("fokkus-jarvis", true, false, FULL_HD, "DP-1", true);
        assert!(!is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn window_on_another_monitor_is_ignored() {
        let w = snap("game", true, false, FULL_HD, "DP-2", false);
        assert!(!is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn borderless_exact_match_is_fullscreen() {
        let w = snap("game", false, false, FULL_HD, "DP-1", false);
        assert!(is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn borderless_with_two_px_inset_is_fullscreen() {
        // 2 px más pequeña en cada borde: sigue dentro de la tolerancia.
        let w = snap(
            "game",
            false,
            false,
            Rect {
                x: 2,
                y: 2,
                w: 1916,
                h: 1076,
            },
            "DP-1",
            false,
        );
        assert!(is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn borderless_with_two_px_overshoot_is_fullscreen() {
        // 2 px más grande en cada borde: también se acepta.
        let w = snap(
            "game",
            false,
            false,
            Rect {
                x: -2,
                y: -2,
                w: 1924,
                h: 1084,
            },
            "DP-1",
            false,
        );
        assert!(is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn borderless_1919_wide_counts_via_98pct_coverage() {
        // 1919×1080 cubre ~99,95 % del output: debe contar.
        let w = snap(
            "game",
            false,
            false,
            Rect {
                x: 0,
                y: 0,
                w: 1919,
                h: 1080,
            },
            "DP-1",
            false,
        );
        assert!(is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn decorated_maximized_is_not_fullscreen() {
        let w = snap("editor", false, true, FULL_HD, "DP-1", false);
        assert!(!is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn borderless_but_much_smaller_is_not_fullscreen() {
        // No cubre el área ni llega a los bordes.
        let w = snap(
            "video",
            false,
            false,
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 500,
            },
            "DP-1",
            false,
        );
        assert!(!is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn borderless_off_by_more_than_two_px_is_not_fullscreen() {
        // El borde izquierdo queda a 3 px del output: fuera de tolerancia.
        let w = snap(
            "game",
            false,
            false,
            Rect {
                x: 3,
                y: 0,
                w: 1917,
                h: 1080,
            },
            "DP-1",
            false,
        );
        assert!(!is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn fullscreen_behavior_serde_is_lowercase_and_defaults_to_dim() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            behavior: FullscreenBehavior,
        }

        assert_eq!(FullscreenBehavior::default(), FullscreenBehavior::Dim);

        let toml = toml::to_string(&Wrap {
            behavior: FullscreenBehavior::Hide,
        })
        .unwrap();
        assert!(toml.contains("behavior = \"hide\""), "{toml}");

        let parsed: Wrap = toml::from_str("behavior = \"dim\"").unwrap();
        assert_eq!(parsed.behavior, FullscreenBehavior::Dim);
    }
}
