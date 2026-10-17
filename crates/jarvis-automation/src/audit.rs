//! Auditoría JSONL de la automatización (CA-38.8).
//!
//! Una línea por evento en `~/.local/state/fokkus-jarvis/automation.jsonl`
//! (`0600`): *lease* adquirido/liberado, acción, captura y kill switch. El
//! texto tecleado se registra solo como longitud + SHA-256 salvo
//! `audit_log_text = true`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::input::{InputAction, Key, MouseButton};

/// Nombre del archivo de auditoría dentro del directorio de estado.
pub const AUDIT_FILE_NAME: &str = "automation.jsonl";

/// Ruta por defecto: `<state_dir>/automation.jsonl`.
pub fn default_audit_path() -> PathBuf {
    jarvis_core::state_dir().join(AUDIT_FILE_NAME)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn key_name(key: &Key) -> String {
    match key {
        Key::Char(c) => c.to_string(),
        Key::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    }
}

/// Descripción de una acción para el JSONL; el texto se redacta salvo
/// `log_text`.
pub fn action_json(action: &InputAction, log_text: bool) -> Value {
    let button = |b: &MouseButton| format!("{b:?}").to_lowercase();
    match action {
        InputAction::MoveTo(p) => json!({ "type": "move_to", "x": p.x, "y": p.y }),
        InputAction::Click {
            at,
            button: b,
            count,
        } => json!({
            "type": "click", "x": at.x, "y": at.y, "button": button(b), "count": count,
        }),
        InputAction::ClickHere { button: b, count } => json!({
            "type": "click", "button": button(b), "count": count,
        }),
        InputAction::Scroll { at, dx, dy } => json!({
            "type": "scroll", "x": at.x, "y": at.y, "dx": dx, "dy": dy,
        }),
        InputAction::TypeText(text) => {
            let mut v = json!({
                "type": "type_text",
                "text_len": text.chars().count(),
                "text_sha256": sha256_hex(text),
            });
            if log_text {
                v["text"] = Value::String(text.clone());
            }
            v
        }
        InputAction::KeyCombo(keys) => json!({
            "type": "key_combo",
            "keys": keys.iter().map(key_name).collect::<Vec<_>>(),
        }),
    }
}

/// Registro de auditoría. Thread-safe: el kill switch escribe desde otro hilo.
pub struct AuditLog {
    path: PathBuf,
    log_text: bool,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(path: impl Into<PathBuf>, log_text: bool) -> Self {
        Self {
            path: path.into(),
            log_text,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn logs_text(&self) -> bool {
        self.log_text
    }

    /// Añade un evento (se le agrega `ts_ms`). Un fallo de E/S se informa por
    /// stderr pero nunca bloquea la automatización ni el kill switch.
    pub fn record(&self, mut event: Value) {
        if let Value::Object(map) = &mut event {
            map.insert("ts_ms".into(), json!(now_ms()));
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = self.append(&event) {
            eprintln!(
                "[automatización] no se pudo escribir la auditoría en {}: {e}",
                self.path.display()
            );
        }
    }

    fn append(&self, event: &Value) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        file.write_all(&line)
    }

    /// Acción de entrada con su resultado.
    pub fn action(&self, lease: u64, app: &str, action: &InputAction, result: &Result<(), String>) {
        self.record(json!({
            "event": "action",
            "lease": lease,
            "app": app,
            "action": action_json(action, self.log_text),
            "result": result_json(result),
        }));
    }
}

/// `"ok"` o `{"error": "..."}`.
pub fn result_json(result: &Result<(), String>) -> Value {
    match result {
        Ok(()) => json!("ok"),
        Err(e) => json!({ "error": e }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "jarvis-automation-audit-{}-{name}",
                std::process::id()
            ))
            .join(AUDIT_FILE_NAME)
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn typed_text_is_redacted_by_default() {
        let path = temp_path("redacted");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::new(&path, false);
        let secret = "mi clave es hunter2";
        log.action(
            7,
            "org.telegram.desktop",
            &InputAction::TypeText(secret.into()),
            &Ok(()),
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("hunter2"), "{raw}");
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let a = &lines[0]["action"];
        assert_eq!(a["type"], "type_text");
        assert_eq!(a["text_len"], secret.chars().count());
        assert_eq!(a["text_sha256"], sha256_hex(secret));
        assert!(a.get("text").is_none());
        assert_eq!(lines[0]["lease"], 7);
        assert_eq!(lines[0]["app"], "org.telegram.desktop");
        assert_eq!(lines[0]["result"], "ok");
        assert!(lines[0]["ts_ms"].as_u64().unwrap() > 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn typed_text_is_logged_only_when_enabled() {
        let path = temp_path("plain");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::new(&path, true);
        log.action(
            1,
            "kwrite",
            &InputAction::TypeText("hola ñandú".into()),
            &Ok(()),
        );
        let lines = read_lines(&path);
        assert_eq!(lines[0]["action"]["text"], "hola ñandú");
        assert_eq!(lines[0]["action"]["text_len"], 10);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn actions_carry_coordinates_and_errors() {
        let path = temp_path("coords");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::new(&path, false);
        log.action(
            3,
            "firefox",
            &InputAction::Click {
                at: Point::new(10.5, -20.0),
                button: MouseButton::Left,
                count: 1,
            },
            &Err("rate limited".into()),
        );
        log.action(
            3,
            "firefox",
            &InputAction::KeyCombo(vec![Key::Ctrl, Key::Char('l')]),
            &Ok(()),
        );
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["action"]["x"], 10.5);
        assert_eq!(lines[0]["action"]["y"], -20.0);
        assert_eq!(lines[0]["action"]["button"], "left");
        assert_eq!(lines[0]["result"]["error"], "rate limited");
        assert_eq!(lines[1]["action"]["keys"], json!(["Ctrl", "l"]));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn audit_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("perms");
        let _ = std::fs::remove_file(&path);
        AuditLog::new(&path, false).record(json!({ "event": "test" }));
        let mode = path.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn default_path_lives_in_state_dir() {
        let p = default_audit_path();
        assert_eq!(p.file_name().unwrap(), AUDIT_FILE_NAME);
        assert_eq!(p.parent().unwrap(), jarvis_core::state_dir());
    }
}
