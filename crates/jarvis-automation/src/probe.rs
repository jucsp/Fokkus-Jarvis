//! Sondeo de capacidades y selección de backends (CA-38.1).
//!
//! La decisión es una función pura de [`Environment`] (sesión, escritorio,
//! binarios, portales), así que el orden de preferencia de §3.1.1 queda
//! cubierto por tests sin depender del equipo donde corren.

use crate::capture::{
    self, CaptureBackendKind, CaptureError, GrimCapture, PortalCapture, ScreenCapture,
    ScreenCaptureKitCapture, SpectacleCapture, WindowsCapture, X11Capture,
};
use crate::geometry::DesktopLayout;
use crate::input::{
    self, CgEventInput, InputBackendKind, InputError, InputSynth, LibeiInput, SendInputInput,
    XtestInput, YdotoolInput,
};
use crate::proc::in_path;

/// Sistema operativo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
    MacOs,
    Other,
}

/// Tipo de sesión gráfica (Linux).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Wayland,
    X11,
    Unknown,
}

/// Lo que el sondeo sabe del sistema.
#[derive(Debug, Clone)]
pub struct Environment {
    pub os: Os,
    pub session: Session,
    /// `XDG_CURRENT_DESKTOP` en minúsculas (`kde`, `gnome`, `sway`…).
    pub desktop: String,
    pub has_spectacle: bool,
    pub has_grim: bool,
    pub has_ydotool: bool,
    /// El socket de `ydotoold` existe.
    pub ydotoold_running: bool,
    /// Versión de `org.freedesktop.portal.Screenshot` o motivo de ausencia.
    pub portal_screenshot: Result<u32, String>,
    /// Versión de `org.freedesktop.portal.RemoteDesktop` o motivo de ausencia.
    pub portal_remote_desktop: Result<u32, String>,
}

impl Environment {
    /// Detecta el entorno real (variables de sesión, `PATH`, D-Bus).
    pub fn detect() -> Self {
        let os = if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Other
        };
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let session = match var("XDG_SESSION_TYPE").to_lowercase().as_str() {
            "wayland" => Session::Wayland,
            "x11" => Session::X11,
            _ if !var("WAYLAND_DISPLAY").is_empty() => Session::Wayland,
            _ if !var("DISPLAY").is_empty() => Session::X11,
            _ => Session::Unknown,
        };
        let linux = os == Os::Linux;
        Self {
            os,
            session,
            desktop: var("XDG_CURRENT_DESKTOP").to_lowercase(),
            has_spectacle: linux && in_path("spectacle"),
            has_grim: linux && in_path("grim"),
            has_ydotool: linux && in_path("ydotool"),
            ydotoold_running: linux && input::ydotool_socket().is_some(),
            portal_screenshot: if linux {
                crate::portal_dbus::interface_version("org.freedesktop.portal.Screenshot")
            } else {
                Err("solo Linux".into())
            },
            portal_remote_desktop: if linux {
                crate::portal_dbus::interface_version("org.freedesktop.portal.RemoteDesktop")
            } else {
                Err("solo Linux".into())
            },
        }
    }

    fn is_kde(&self) -> bool {
        self.desktop.split(':').any(|d| d == "kde")
    }

    fn is_gnome(&self) -> bool {
        self.desktop.split(':').any(|d| d == "gnome")
    }
}

/// Opciones de usuario que cambian la selección.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeOptions {
    /// Permite el respaldo `ydotool` (`[automation] allow_uinput`).
    pub allow_uinput: bool,
}

impl From<&jarvis_core::AutomationConfig> for ProbeOptions {
    fn from(cfg: &jarvis_core::AutomationConfig) -> Self {
        Self {
            allow_uinput: cfg.allow_uinput,
        }
    }
}

/// Resultado de un backend: `Ok` disponible, `Err(motivo)` descartado.
pub type Verdict = Result<(), String>;

/// Backends de captura de la plataforma, en orden de preferencia, con su
/// veredicto.
pub fn evaluate_capture(env: &Environment) -> Vec<(CaptureBackendKind, Verdict)> {
    match env.os {
        Os::Linux => {
            let portal = env
                .portal_screenshot
                .clone()
                .map(|_| ())
                .map_err(|e| format!("portal Screenshot no disponible: {e}"));
            let spectacle = if !env.has_spectacle {
                Err("spectacle no está en el PATH".to_string())
            } else if !env.is_kde() {
                Err(format!(
                    "solo en KDE Plasma (XDG_CURRENT_DESKTOP={})",
                    env.desktop
                ))
            } else {
                Ok(())
            };
            let grim = if env.is_kde() {
                Err("KWin no expone wlr-screencopy".to_string())
            } else if env.is_gnome() {
                Err("Mutter no expone wlr-screencopy".to_string())
            } else if env.session != Session::Wayland {
                Err("requiere una sesión Wayland wlroots".to_string())
            } else if !env.has_grim {
                Err("grim no está en el PATH".to_string())
            } else {
                Ok(())
            };
            let x11 = if env.session == Session::X11 {
                Err(capture::x11_pending().to_string())
            } else {
                Err("la sesión no es X11".to_string())
            };
            vec![
                (CaptureBackendKind::Portal, portal),
                (CaptureBackendKind::Spectacle, spectacle),
                (CaptureBackendKind::Grim, grim),
                (CaptureBackendKind::X11, x11),
            ]
        }
        Os::Windows => vec![
            (
                CaptureBackendKind::WindowsGraphicsCapture,
                Err(capture::windows_reason().to_string()),
            ),
            (
                CaptureBackendKind::Gdi,
                Err(capture::windows_reason().to_string()),
            ),
        ],
        Os::MacOs => vec![(
            CaptureBackendKind::ScreenCaptureKit,
            Err(capture::macos_reason().to_string()),
        )],
        Os::Other => Vec::new(),
    }
}

/// Backends de entrada de la plataforma, en orden de preferencia, con su
/// veredicto.
pub fn evaluate_input(env: &Environment, opts: &ProbeOptions) -> Vec<(InputBackendKind, Verdict)> {
    match env.os {
        Os::Linux => {
            let libei = match &env.portal_remote_desktop {
                Ok(v) => Err(format!(
                    "portal RemoteDesktop v{v} disponible, pero {}",
                    input::libei_pending()
                )),
                Err(e) => Err(format!("portal RemoteDesktop no disponible: {e}")),
            };
            let xtest = if env.session == Session::X11 {
                Err(input::xtest_pending().to_string())
            } else {
                Err("la sesión no es X11".to_string())
            };
            let ydotool = if !opts.allow_uinput {
                Err("desactivado (allow_uinput = false)".to_string())
            } else if !env.has_ydotool {
                Err("ydotool no está en el PATH".to_string())
            } else if !env.ydotoold_running {
                Err("ydotoold no está corriendo (sin socket)".to_string())
            } else {
                Ok(())
            };
            vec![
                (InputBackendKind::PortalLibei, libei),
                (InputBackendKind::X11Xtest, xtest),
                (InputBackendKind::Ydotool, ydotool),
            ]
        }
        Os::Windows => vec![(
            InputBackendKind::WindowsSendInput,
            Err(input::windows_reason().to_string()),
        )],
        Os::MacOs => vec![(
            InputBackendKind::CGEvent,
            Err(input::macos_reason().to_string()),
        )],
        Os::Other => Vec::new(),
    }
}

fn instantiate_capture(kind: CaptureBackendKind, layout: DesktopLayout) -> Box<dyn ScreenCapture> {
    match kind {
        CaptureBackendKind::Portal => Box::new(PortalCapture::new(layout)),
        CaptureBackendKind::Spectacle => Box::new(SpectacleCapture::new(layout)),
        CaptureBackendKind::Grim => Box::new(GrimCapture::new(layout)),
        CaptureBackendKind::X11 => Box::new(X11Capture),
        CaptureBackendKind::WindowsGraphicsCapture | CaptureBackendKind::Gdi => {
            Box::new(WindowsCapture::new(kind))
        }
        CaptureBackendKind::ScreenCaptureKit => Box::new(ScreenCaptureKitCapture),
    }
}

fn instantiate_input(kind: InputBackendKind) -> Box<dyn InputSynth> {
    match kind {
        InputBackendKind::PortalLibei => Box::new(LibeiInput),
        InputBackendKind::X11Xtest => Box::new(XtestInput),
        InputBackendKind::Ydotool => Box::new(YdotoolInput),
        InputBackendKind::WindowsSendInput => Box::new(SendInputInput),
        InputBackendKind::CGEvent => Box::new(CgEventInput),
    }
}

/// Resume los descartes para el error de "ningún backend".
fn summarize<K: std::fmt::Display>(verdicts: &[(K, Verdict)]) -> String {
    if verdicts.is_empty() {
        return "plataforma sin backends conocidos".into();
    }
    verdicts
        .iter()
        .filter_map(|(k, v)| v.as_ref().err().map(|why| format!("{k}: {why}")))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Primer backend de captura disponible para `env`.
pub fn select_capture(
    env: &Environment,
    layout: DesktopLayout,
) -> Result<Box<dyn ScreenCapture>, CaptureError> {
    let verdicts = evaluate_capture(env);
    verdicts
        .iter()
        .find(|(_, v)| v.is_ok())
        .map(|(k, _)| instantiate_capture(*k, layout))
        .ok_or_else(|| CaptureError::Unavailable(summarize(&verdicts)))
}

/// Primer backend de entrada disponible para `env`.
pub fn select_input(
    env: &Environment,
    opts: &ProbeOptions,
) -> Result<Box<dyn InputSynth>, InputError> {
    let verdicts = evaluate_input(env, opts);
    verdicts
        .iter()
        .find(|(_, v)| v.is_ok())
        .map(|(k, _)| instantiate_input(*k))
        .ok_or_else(|| InputError::Unavailable(summarize(&verdicts)))
}

/// [`input::probe_input_backend`] respetando las opciones del usuario.
pub fn probe_input_backend_with(opts: &ProbeOptions) -> Result<Box<dyn InputSynth>, InputError> {
    select_input(&Environment::detect(), opts)
}

/// Capacidades detectadas: todos los backends de la plataforma con su
/// veredicto, en orden de preferencia (el elegido es el primero `Ok`).
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub capture: Vec<(CaptureBackendKind, Verdict)>,
    pub input: Vec<(InputBackendKind, Verdict)>,
}

impl Capabilities {
    pub fn from_env(env: &Environment, opts: &ProbeOptions) -> Self {
        Self {
            capture: evaluate_capture(env),
            input: evaluate_input(env, opts),
        }
    }

    /// Backend de captura que se usaría.
    pub fn chosen_capture(&self) -> Option<CaptureBackendKind> {
        self.capture
            .iter()
            .find(|(_, v)| v.is_ok())
            .map(|(k, _)| *k)
    }

    /// Backend de entrada que se usaría.
    pub fn chosen_input(&self) -> Option<InputBackendKind> {
        self.input.iter().find(|(_, v)| v.is_ok()).map(|(k, _)| *k)
    }

    /// Líneas legibles para `--check`: `elegido` / `disponible` /
    /// `descartado (motivo)`.
    pub fn report_lines<K: std::fmt::Display + PartialEq + Copy>(
        verdicts: &[(K, Verdict)],
        chosen: Option<K>,
    ) -> Vec<String> {
        verdicts
            .iter()
            .map(|(k, v)| match v {
                Ok(()) if Some(*k) == chosen => format!("{k}: elegido"),
                Ok(()) => format!("{k}: disponible"),
                Err(why) => format!("{k}: descartado ({why})"),
            })
            .collect()
    }
}

/// Sondeo con las opciones por defecto.
pub fn probe() -> Capabilities {
    probe_with(&ProbeOptions::default())
}

/// Sondeo con las opciones del usuario (`--check`).
pub fn probe_with(opts: &ProbeOptions) -> Capabilities {
    Capabilities::from_env(&Environment::detect(), opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Equipo de referencia (§1.1): Fedora 44, KDE Plasma en KWin Wayland,
    /// spectacle y grim presentes, sin ydotool.
    fn kde_wayland() -> Environment {
        Environment {
            os: Os::Linux,
            session: Session::Wayland,
            desktop: "kde".into(),
            has_spectacle: true,
            has_grim: true,
            has_ydotool: false,
            ydotoold_running: false,
            portal_screenshot: Ok(4),
            portal_remote_desktop: Ok(2),
        }
    }

    #[test]
    fn kde_wayland_prefers_portal_and_discards_grim_with_reason() {
        let caps = Capabilities::from_env(&kde_wayland(), &ProbeOptions::default());
        assert_eq!(caps.chosen_capture(), Some(CaptureBackendKind::Portal));
        let grim = caps
            .capture
            .iter()
            .find(|(k, _)| *k == CaptureBackendKind::Grim)
            .unwrap();
        assert_eq!(grim.1, Err("KWin no expone wlr-screencopy".to_string()));
        let lines = Capabilities::report_lines(&caps.capture, caps.chosen_capture());
        assert_eq!(lines[0], "portal Screenshot: elegido");
        assert_eq!(lines[1], "spectacle: disponible");
        assert!(lines[2].starts_with("grim: descartado (KWin"));
    }

    #[test]
    fn kde_without_portal_falls_back_to_spectacle() {
        let env = Environment {
            portal_screenshot: Err("sin portal".into()),
            ..kde_wayland()
        };
        assert_eq!(
            select_capture(&env, DesktopLayout::default())
                .unwrap()
                .kind(),
            CaptureBackendKind::Spectacle
        );
    }

    #[test]
    fn wlroots_uses_grim_when_portal_is_missing() {
        let env = Environment {
            desktop: "sway".into(),
            has_spectacle: false,
            portal_screenshot: Err("sin portal".into()),
            ..kde_wayland()
        };
        assert_eq!(
            select_capture(&env, DesktopLayout::default())
                .unwrap()
                .kind(),
            CaptureBackendKind::Grim
        );
    }

    #[test]
    fn gnome_discards_grim_and_spectacle() {
        let env = Environment {
            desktop: "ubuntu:gnome".into(),
            ..kde_wayland()
        };
        let caps = Capabilities::from_env(&env, &ProbeOptions::default());
        assert_eq!(caps.chosen_capture(), Some(CaptureBackendKind::Portal));
        assert!(caps
            .capture
            .iter()
            .any(|(k, v)| *k == CaptureBackendKind::Grim
                && v.as_ref().unwrap_err().contains("Mutter")));
        assert!(caps
            .capture
            .iter()
            .any(|(k, v)| *k == CaptureBackendKind::Spectacle
                && v.as_ref().unwrap_err().contains("solo en KDE")));
    }

    #[test]
    fn no_capture_backend_explains_every_discard() {
        let env = Environment {
            portal_screenshot: Err("sin portal".into()),
            has_spectacle: false,
            ..kde_wayland()
        };
        let err = select_capture(&env, DesktopLayout::default())
            .err()
            .unwrap();
        let CaptureError::Unavailable(why) = err else {
            panic!("{err:?}")
        };
        assert!(why.contains("spectacle no está en el PATH"), "{why}");
        assert!(why.contains("KWin no expone wlr-screencopy"), "{why}");
    }

    #[test]
    fn ydotool_requires_explicit_opt_in() {
        let env = Environment {
            has_ydotool: true,
            ydotoold_running: true,
            ..kde_wayland()
        };
        let off = Capabilities::from_env(&env, &ProbeOptions::default());
        assert_eq!(off.chosen_input(), None);
        assert!(off
            .input
            .iter()
            .any(|(k, v)| *k == InputBackendKind::Ydotool
                && v.as_ref().unwrap_err().contains("allow_uinput = false")));
        let on = Capabilities::from_env(&env, &ProbeOptions { allow_uinput: true });
        assert_eq!(on.chosen_input(), Some(InputBackendKind::Ydotool));
        assert_eq!(
            select_input(&env, &ProbeOptions { allow_uinput: true })
                .unwrap()
                .kind(),
            InputBackendKind::Ydotool
        );
    }

    #[test]
    fn ydotool_without_daemon_is_discarded() {
        let env = Environment {
            has_ydotool: true,
            ydotoold_running: false,
            ..kde_wayland()
        };
        let caps = Capabilities::from_env(&env, &ProbeOptions { allow_uinput: true });
        assert_eq!(caps.chosen_input(), None);
        assert!(caps.input.iter().any(|(_, v)| v
            .as_ref()
            .is_err_and(|e| e.contains("ydotoold no está corriendo"))));
    }

    #[test]
    fn libei_reports_portal_version_and_pending_decision() {
        let caps = Capabilities::from_env(&kde_wayland(), &ProbeOptions::default());
        let (kind, verdict) = &caps.input[0];
        assert_eq!(*kind, InputBackendKind::PortalLibei);
        let why = verdict.as_ref().unwrap_err();
        assert!(why.contains("RemoteDesktop v2 disponible"), "{why}");
        assert!(why.contains("D-38.1"), "{why}");
    }

    #[test]
    fn x11_session_lists_x11_backends_with_reasons() {
        let env = Environment {
            session: Session::X11,
            desktop: "xfce".into(),
            portal_screenshot: Err("sin portal".into()),
            has_spectacle: false,
            ..kde_wayland()
        };
        let caps = Capabilities::from_env(&env, &ProbeOptions::default());
        assert!(caps.capture.iter().any(
            |(k, v)| *k == CaptureBackendKind::X11 && v.as_ref().unwrap_err().contains("x11rb")
        ));
        assert!(caps
            .capture
            .iter()
            .any(|(k, v)| *k == CaptureBackendKind::Grim
                && v.as_ref().unwrap_err().contains("Wayland")));
    }

    #[test]
    fn other_platforms_have_their_own_strategies() {
        let win = Environment {
            os: Os::Windows,
            ..kde_wayland()
        };
        let caps = Capabilities::from_env(&win, &ProbeOptions::default());
        assert_eq!(
            caps.capture.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![
                CaptureBackendKind::WindowsGraphicsCapture,
                CaptureBackendKind::Gdi
            ]
        );
        assert_eq!(caps.input[0].0, InputBackendKind::WindowsSendInput);
        let mac = Environment {
            os: Os::MacOs,
            ..kde_wayland()
        };
        let caps = Capabilities::from_env(&mac, &ProbeOptions::default());
        assert_eq!(caps.capture[0].0, CaptureBackendKind::ScreenCaptureKit);
        assert_eq!(caps.input[0].0, InputBackendKind::CGEvent);
    }

    #[test]
    fn probe_on_this_machine_does_not_panic() {
        let caps = probe();
        if cfg!(target_os = "linux") {
            assert_eq!(caps.capture.len(), 4);
            assert_eq!(caps.input.len(), 3);
        }
    }
}
