//! Manos: entrada sintética multiplataforma (patrón Strategy).
//!
//! [`InputSynth`] es el contrato; los backends concretos (portal
//! RemoteDesktop + libei, X11 XTest, ydotool, Win32 `SendInput`, macOS
//! `CGEventPost`) se eligen en tiempo de ejecución con
//! [`probe_input_backend`]. Ninguna acción llega a un backend sin pasar por
//! [`crate::guard::Lease::perform`].

mod libei;
mod macos;
mod windows;
mod x11;
mod ydotool;

use std::fmt;

use crate::geometry::Point;

pub use libei::LibeiInput;
pub use macos::CgEventInput;
pub use windows::SendInputInput;
pub use x11::XtestInput;
pub use ydotool::{ydotool_socket, YdotoolInput};

/// Motivo por el que libei aún no actúa (D-38.1).
pub(crate) fn libei_pending() -> &'static str {
    libei::PENDING
}

/// Motivo de descarte de XTest mientras no se apruebe `x11rb`.
pub(crate) fn xtest_pending() -> &'static str {
    x11::PENDING
}

/// Motivo de descarte de `SendInput` en esta compilación.
pub(crate) fn windows_reason() -> &'static str {
    windows::unavailable_reason()
}

/// Motivo de descarte de `CGEventPost` en esta compilación.
pub(crate) fn macos_reason() -> &'static str {
    macos::unavailable_reason()
}

/// Botón del ratón.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Tecla lógica, independiente del backend y de la distribución de teclado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Ctrl,
    Shift,
    Alt,
    Super,
    MediaPlayPause,
    /// Pista/video siguiente (`KEY_NEXTSONG`).
    MediaNext,
    /// Pista/video anterior (`KEY_PREVIOUSSONG`).
    MediaPrevious,
    VolumeUp,
    VolumeDown,
    Mute,
    /// Carácter imprimible (para atajos: `Ctrl+L`). El texto libre va por
    /// [`InputAction::TypeText`].
    Char(char),
    /// Tecla de función `F1`–`F24`.
    F(u8),
}

impl Key {
    /// `true` para modificadores (se mantienen pulsados en un combo).
    pub fn is_modifier(&self) -> bool {
        matches!(self, Key::Ctrl | Key::Shift | Key::Alt | Key::Super)
    }
}

impl Key {
    /// Tecla por nombre, sin distinguir mayúsculas: `Enter`, `BackSpace`,
    /// `ctrl`, `F5`, `l`…
    pub fn from_name(name: &str) -> Option<Key> {
        let lower = name.trim().to_lowercase();
        let key = match lower.as_str() {
            "enter" | "return" | "intro" => Key::Enter,
            "escape" | "esc" => Key::Escape,
            "tab" => Key::Tab,
            "backspace" | "retroceso" => Key::Backspace,
            "delete" | "del" | "supr" | "suprimir" => Key::Delete,
            "space" | "espacio" => Key::Space,
            "up" | "arriba" => Key::Up,
            "down" | "abajo" => Key::Down,
            "left" | "izquierda" => Key::Left,
            "right" | "derecha" => Key::Right,
            "home" | "inicio" => Key::Home,
            "end" | "fin" => Key::End,
            "pageup" | "pgup" => Key::PageUp,
            "pagedown" | "pgdn" => Key::PageDown,
            "ctrl" | "control" => Key::Ctrl,
            "shift" | "mayus" => Key::Shift,
            "alt" => Key::Alt,
            "super" | "meta" | "win" => Key::Super,
            "playpause" | "play" | "pause" => Key::MediaPlayPause,
            "next" => Key::MediaNext,
            "previous" | "prev" => Key::MediaPrevious,
            "volumeup" => Key::VolumeUp,
            "volumedown" => Key::VolumeDown,
            "mute" => Key::Mute,
            _ => {
                let mut chars = lower.chars();
                return match (chars.next(), chars.next()) {
                    (Some(c), None) if !c.is_whitespace() => Some(Key::Char(c)),
                    (Some('f'), Some(_)) => lower[1..]
                        .parse::<u8>()
                        .ok()
                        .filter(|n| (1..=24).contains(n))
                        .map(Key::F),
                    _ => None,
                };
            }
        };
        Some(key)
    }

    /// Combo como `Ctrl+L` o `ctrl+shift+t` (modificadores primero).
    pub fn parse_combo(spec: &str) -> Result<Vec<Key>, String> {
        let spec = spec.trim();
        // "Ctrl++" → Ctrl y la tecla '+'.
        let parts: Vec<&str> = match spec.strip_suffix("++") {
            Some(head) => head.split('+').chain(std::iter::once("+")).collect(),
            None if spec == "+" => vec!["+"],
            None => spec.split('+').collect(),
        };
        if parts.iter().any(|p| p.trim().is_empty()) {
            return Err(format!("combinación de teclas inválida: {spec:?}"));
        }
        parts
            .iter()
            .map(|p| Key::from_name(p).ok_or_else(|| format!("tecla desconocida: {:?}", p.trim())))
            .collect()
    }
}

/// Una acción de entrada, siempre en coordenadas lógicas.
#[derive(Debug, Clone, PartialEq)]
pub enum InputAction {
    MoveTo(Point),
    Click {
        at: Point,
        button: MouseButton,
        count: u8,
    },
    /// Clic donde esté el puntero, sin moverlo.
    ClickHere {
        button: MouseButton,
        count: u8,
    },
    Scroll {
        at: Point,
        dx: f64,
        dy: f64,
    },
    TypeText(String),
    KeyCombo(Vec<Key>),
}

impl InputAction {
    /// Nombre corto para la auditoría.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::MoveTo(_) => "move_to",
            Self::Click { .. } | Self::ClickHere { .. } => "click",
            Self::Scroll { .. } => "scroll",
            Self::TypeText(_) => "type_text",
            Self::KeyCombo(_) => "key_combo",
        }
    }

    /// Punto lógico al que apunta la acción, si tiene uno.
    pub fn point(&self) -> Option<Point> {
        match self {
            Self::MoveTo(p) | Self::Click { at: p, .. } | Self::Scroll { at: p, .. } => Some(*p),
            Self::ClickHere { .. } | Self::TypeText(_) | Self::KeyCombo(_) => None,
        }
    }
}

/// Backends de entrada conocidos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputBackendKind {
    /// XDG Portal `RemoteDesktop` + `libei` (Wayland con consentimiento).
    PortalLibei,
    /// X11 XTest (solo sesión Xorg).
    X11Xtest,
    /// `ydotool` sobre `/dev/uinput` (solo con `allow_uinput = true`).
    Ydotool,
    /// Win32 `SendInput`.
    WindowsSendInput,
    /// macOS `CGEventPost`.
    CGEvent,
}

impl InputBackendKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::PortalLibei => "portal RemoteDesktop + libei",
            Self::X11Xtest => "X11 XTest",
            Self::Ydotool => "ydotool",
            Self::WindowsSendInput => "Win32 SendInput",
            Self::CGEvent => "CGEventPost",
        }
    }
}

impl fmt::Display for InputBackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Error de entrada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    /// El backend no existe en esta plataforma/sesión (con motivo).
    Unavailable(String),
    /// El usuario rechazó o revocó el permiso (portal, TCC, UIPI).
    PermissionDenied,
    /// El backend no sabe ejecutar esa acción/tecla.
    Unsupported(String),
    /// El backend no respondió a tiempo.
    Timeout,
    /// Fallo del backend.
    Backend(String),
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "entrada no disponible: {why}"),
            Self::PermissionDenied => f.write_str("permiso de control de entrada denegado"),
            Self::Unsupported(why) => write!(f, "acción no soportada: {why}"),
            Self::Timeout => f.write_str("la acción de entrada agotó el tiempo"),
            Self::Backend(why) => write!(f, "fallo del backend de entrada: {why}"),
        }
    }
}

impl std::error::Error for InputError {}

/// Contrato de estrategia de entrada.
pub trait InputSynth: Send {
    fn kind(&self) -> InputBackendKind;
    fn perform(&mut self, action: &InputAction) -> Result<(), InputError>;
}

/// Selecciona e instancia el mejor backend de entrada disponible con las
/// opciones por defecto (sin `ydotool`: `allow_uinput = false`). Para
/// respetar la configuración del usuario usa
/// [`crate::probe::probe_input_backend_with`].
pub fn probe_input_backend() -> Result<Box<dyn InputSynth>, InputError> {
    crate::probe::probe_input_backend_with(&crate::probe::ProbeOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_are_case_insensitive() {
        assert_eq!(Key::from_name("Enter"), Some(Key::Enter));
        assert_eq!(Key::from_name("BackSpace"), Some(Key::Backspace));
        assert_eq!(Key::from_name("ESC"), Some(Key::Escape));
        assert_eq!(Key::from_name("F5"), Some(Key::F(5)));
        assert_eq!(Key::from_name("L"), Some(Key::Char('l')));
        assert_eq!(Key::from_name("F25"), None);
        assert_eq!(Key::from_name("hola"), None);
    }

    #[test]
    fn combos_keep_their_order() {
        assert_eq!(
            Key::parse_combo("Ctrl+L"),
            Ok(vec![Key::Ctrl, Key::Char('l')])
        );
        assert_eq!(
            Key::parse_combo("ctrl+shift+t"),
            Ok(vec![Key::Ctrl, Key::Shift, Key::Char('t')])
        );
        assert_eq!(
            Key::parse_combo("Ctrl++"),
            Ok(vec![Key::Ctrl, Key::Char('+')])
        );
        assert_eq!(Key::parse_combo("Tab"), Ok(vec![Key::Tab]));
        assert!(Key::parse_combo("").is_err());
        assert!(Key::parse_combo("ctrl+").is_err());
        assert!(Key::parse_combo("ctrl+nada").is_err());
    }
}
