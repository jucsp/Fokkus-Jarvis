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

/// `app_id` de las superficies del propio escritorio (fondo, paneles,
/// docks). Nunca son pantalla completa aunque cubran el output sin bordes.
const DESKTOP_SHELL_APP_IDS: &[&str] = &["plasmashell", "org.kde.plasmashell"];

/// `true` si `app_id` pertenece al shell del escritorio.
fn is_desktop_shell(app_id: &str) -> bool {
    DESKTOP_SHELL_APP_IDS
        .iter()
        .any(|id| app_id.eq_ignore_ascii_case(id))
}

/// Decide si la ventana activa es "efectivamente pantalla completa" en `out`.
///
/// Reglas (contrato HU-35, §1.2, revisado en v0.2.22):
/// 1. `own == true` → `false` (las ventanas de Jarvis se ignoran).
/// 2. Shell del escritorio (`plasmashell`) → `false`.
/// 3. `out.name != w.output` → `false` (multimonitor: solo afecta al monitor
///    donde está la esfera).
/// 4. Solo cuenta el estado fullscreen explícito del compositor
///    (`w.fullscreen`: F11, `mpv --fs`, juegos). Una ventana sin bordes que
///    cubre el output (maximizada con CSD: Chrome, VS Code, Flatpaks) **no** es
///    pantalla completa.
pub fn is_effectively_fullscreen(w: &WindowSnapshot, out: &OutputGeometry) -> bool {
    if w.own || is_desktop_shell(&w.app_id) {
        return false;
    }
    if out.name != w.output {
        return false;
    }
    w.fullscreen
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
    fn borderless_covering_output_is_not_fullscreen() {
        // Ventana maximizada con CSD (Chrome, VS Code, Flatpak): sin bordes y
        // cubriendo el output, pero sin estado fullscreen del compositor.
        let w = snap("google-chrome", false, false, FULL_HD, "DP-1", false);
        assert!(!is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn borderless_with_small_inset_is_not_fullscreen() {
        let w = snap(
            "code",
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
        assert!(!is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn desktop_shell_is_never_fullscreen() {
        // Escritorio de Plasma: `noBorder` y 1920×1080 al minimizarlo todo.
        for app in ["plasmashell", "org.kde.plasmashell", "PlasmaShell"] {
            let w = snap(app, false, false, FULL_HD, "DP-1", false);
            assert!(
                !is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)),
                "{app}"
            );
            let w = snap(app, true, false, FULL_HD, "DP-1", false);
            assert!(
                !is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)),
                "{app}"
            );
        }
    }

    #[test]
    fn declared_fullscreen_borderless_is_fullscreen() {
        // F11 en el navegador o un juego en pantalla completa.
        let w = snap("firefox", true, false, FULL_HD, "DP-1", false);
        assert!(is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn decorated_maximized_is_not_fullscreen() {
        let w = snap("editor", false, true, FULL_HD, "DP-1", false);
        assert!(!is_effectively_fullscreen(&w, &out("DP-1", FULL_HD)));
    }

    #[test]
    fn borderless_but_much_smaller_is_not_fullscreen() {
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
