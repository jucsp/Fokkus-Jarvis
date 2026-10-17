//! Entrada con `ydotool` (uinput). Respaldo explícito: solo se elige con
//! `allow_uinput = true` porque exige `ydotoold` con acceso a `/dev/uinput`
//! (superficie de ataque alta: cualquier proceso del usuario puede inyectar).
//!
//! Limitaciones conocidas: `ydotool type` asume distribución US para los
//! símbolos; `mousemove --absolute` posiciona en coordenadas del dispositivo
//! uinput, que KWin/Mutter mapean al escritorio lógico sin conocer la escala.
//! Por eso el backend preferente en Wayland es el portal + libei.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use super::{InputAction, InputBackendKind, InputError, InputSynth, Key, MouseButton};
use crate::proc::run_with_timeout;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Socket de `ydotoold`: `$YDOTOOL_SOCKET`, `$XDG_RUNTIME_DIR/.ydotool_socket`
/// o `/tmp/.ydotool_socket` (en ese orden), si existe.
pub fn ydotool_socket() -> Option<PathBuf> {
    let candidates = [
        std::env::var_os("YDOTOOL_SOCKET").map(PathBuf::from),
        std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join(".ydotool_socket")),
        Some(PathBuf::from("/tmp/.ydotool_socket")),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

/// Código evdev (`linux/input-event-codes.h`) de una tecla.
fn evdev_code(key: &Key) -> Option<u16> {
    Some(match key {
        Key::Escape => 1,
        Key::Backspace => 14,
        Key::Tab => 15,
        Key::Enter => 28,
        Key::Ctrl => 29,
        Key::Shift => 42,
        Key::Alt => 56,
        Key::Space => 57,
        Key::Home => 102,
        Key::Up => 103,
        Key::PageUp => 104,
        Key::Left => 105,
        Key::Right => 106,
        Key::End => 107,
        Key::Down => 108,
        Key::PageDown => 109,
        Key::Delete => 111,
        Key::Mute => 113,
        Key::VolumeDown => 114,
        Key::VolumeUp => 115,
        Key::Super => 125,
        Key::MediaNext => 163,
        Key::MediaPlayPause => 164,
        Key::MediaPrevious => 165,
        Key::F(n @ 1..=10) => 58 + u16::from(*n),
        Key::F(11) => 87,
        Key::F(12) => 88,
        Key::F(n @ 13..=24) => 183 + u16::from(*n - 13),
        Key::F(_) => return None,
        Key::Char(c) => char_code(c.to_ascii_lowercase())?,
    })
}

fn char_code(c: char) -> Option<u16> {
    const ROW_Q: &str = "qwertyuiop";
    const ROW_A: &str = "asdfghjkl";
    const ROW_Z: &str = "zxcvbnm";
    if let Some(i) = ROW_Q.find(c) {
        return Some(16 + i as u16);
    }
    if let Some(i) = ROW_A.find(c) {
        return Some(30 + i as u16);
    }
    if let Some(i) = ROW_Z.find(c) {
        return Some(44 + i as u16);
    }
    Some(match c {
        '1'..='9' => 2 + (c as u16 - '1' as u16),
        '0' => 11,
        '-' => 12,
        '=' => 13,
        '[' => 26,
        ']' => 27,
        ';' => 39,
        '\'' => 40,
        '`' => 41,
        '\\' => 43,
        ',' => 51,
        '.' => 52,
        '/' => 53,
        ' ' => 57,
        _ => return None,
    })
}

fn button_code(button: MouseButton) -> &'static str {
    // 0xC0 = pulsar + soltar; 0x00 izquierdo, 0x01 derecho, 0x02 central.
    match button {
        MouseButton::Left => "0xC0",
        MouseButton::Right => "0xC1",
        MouseButton::Middle => "0xC2",
    }
}

/// Invocaciones de `ydotool` (sin el nombre del programa) para una acción.
pub(crate) fn ydotool_argvs(action: &InputAction) -> Result<Vec<Vec<String>>, InputError> {
    let move_to = |x: f64, y: f64| -> Vec<String> {
        vec![
            "mousemove".into(),
            "--absolute".into(),
            "-x".into(),
            format!("{}", x.round() as i64),
            "-y".into(),
            format!("{}", y.round() as i64),
        ]
    };
    Ok(match action {
        InputAction::MoveTo(p) => vec![move_to(p.x, p.y)],
        InputAction::Click { at, button, count } => vec![
            move_to(at.x, at.y),
            vec![
                "click".into(),
                "-r".into(),
                count.max(&1).to_string(),
                "-D".into(),
                "60".into(),
                button_code(*button).into(),
            ],
        ],
        InputAction::ClickHere { button, count } => vec![vec![
            "click".into(),
            "-r".into(),
            count.max(&1).to_string(),
            "-D".into(),
            "60".into(),
            button_code(*button).into(),
        ]],
        InputAction::Scroll { at, dx, dy } => vec![
            move_to(at.x, at.y),
            vec![
                "mousemove".into(),
                "--wheel".into(),
                "-x".into(),
                format!("{}", dx.round() as i64),
                "-y".into(),
                format!("{}", dy.round() as i64),
            ],
        ],
        InputAction::TypeText(text) => {
            vec![vec!["type".into(), "--".into(), text.clone()]]
        }
        InputAction::KeyCombo(keys) => {
            if keys.is_empty() {
                return Err(InputError::Unsupported("combinación vacía".into()));
            }
            let codes = keys
                .iter()
                .map(|k| {
                    evdev_code(k).ok_or_else(|| {
                        InputError::Unsupported(format!("tecla {k:?} sin código evdev"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            // Pulsa en orden y suelta en orden inverso: Ctrl↓ L↓ L↑ Ctrl↑.
            let mut argv = vec!["key".to_string()];
            argv.extend(codes.iter().map(|c| format!("{c}:1")));
            argv.extend(codes.iter().rev().map(|c| format!("{c}:0")));
            vec![argv]
        }
    })
}

pub struct YdotoolInput;

impl InputSynth for YdotoolInput {
    fn kind(&self) -> InputBackendKind {
        InputBackendKind::Ydotool
    }

    fn perform(&mut self, action: &InputAction) -> Result<(), InputError> {
        for argv in ydotool_argvs(action)? {
            let mut cmd = Command::new("ydotool");
            if let Some(socket) = ydotool_socket() {
                cmd.env("YDOTOOL_SOCKET", socket);
            }
            cmd.args(&argv);
            let out = run_with_timeout(cmd, TIMEOUT).map_err(|e| {
                if e.contains("tiempo agotado") {
                    InputError::Timeout
                } else {
                    InputError::Backend(format!("ydotool: {e}"))
                }
            })?;
            if !out.success {
                // Sin `ydotoold`, ydotool avisa por stdout, no por stderr.
                let mut err = out.stderr.trim().to_string();
                if err.is_empty() {
                    err = String::from_utf8_lossy(&out.stdout).trim().to_string();
                }
                return Err(if err.contains("ermission") {
                    InputError::PermissionDenied
                } else {
                    InputError::Backend(format!("ydotool {}: {err}", argv[0]))
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn ctrl_l_presses_in_order_and_releases_in_reverse() {
        let argvs = ydotool_argvs(&InputAction::KeyCombo(vec![Key::Ctrl, Key::Char('L')])).unwrap();
        assert_eq!(argvs, vec![s(&["key", "29:1", "38:1", "38:0", "29:0"])]);
    }

    #[test]
    fn click_moves_then_clicks() {
        let argvs = ydotool_argvs(&InputAction::Click {
            at: Point::new(100.4, 200.6),
            button: MouseButton::Right,
            count: 2,
        })
        .unwrap();
        assert_eq!(
            argvs,
            vec![
                s(&["mousemove", "--absolute", "-x", "100", "-y", "201"]),
                s(&["click", "-r", "2", "-D", "60", "0xC1"]),
            ]
        );
    }

    #[test]
    fn click_here_does_not_move_the_pointer() {
        let argvs = ydotool_argvs(&InputAction::ClickHere {
            button: MouseButton::Left,
            count: 1,
        })
        .unwrap();
        assert_eq!(argvs, vec![s(&["click", "-r", "1", "-D", "60", "0xC0"])]);
    }

    #[test]
    fn type_text_is_passed_as_single_argument_after_double_dash() {
        let argvs = ydotool_argvs(&InputAction::TypeText("-rf ¿Añadir café? ñ@".into())).unwrap();
        assert_eq!(argvs, vec![s(&["type", "--", "-rf ¿Añadir café? ñ@"])]);
    }

    #[test]
    fn function_and_media_keys_have_codes() {
        assert_eq!(evdev_code(&Key::F(1)), Some(59));
        assert_eq!(evdev_code(&Key::F(10)), Some(68));
        assert_eq!(evdev_code(&Key::F(12)), Some(88));
        assert_eq!(evdev_code(&Key::F(13)), Some(183));
        assert_eq!(evdev_code(&Key::MediaPlayPause), Some(164));
        assert_eq!(evdev_code(&Key::MediaNext), Some(163));
        assert_eq!(evdev_code(&Key::MediaPrevious), Some(165));
        assert_eq!(evdev_code(&Key::Char('a')), Some(30));
        assert_eq!(evdev_code(&Key::Char('m')), Some(50));
        assert_eq!(evdev_code(&Key::Char('0')), Some(11));
        assert!(evdev_code(&Key::Char('ñ')).is_none());
        assert!(ydotool_argvs(&InputAction::KeyCombo(vec![Key::Char('ñ')])).is_err());
        assert!(ydotool_argvs(&InputAction::KeyCombo(vec![])).is_err());
    }
}
