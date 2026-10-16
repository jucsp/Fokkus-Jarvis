//! Utilidades de proceso y de archivos temporales privados compartidas por los
//! backends que delegan en binarios del sistema (spectacle, grim, ydotool).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `true` si `program` es un ejecutable en algún directorio del `PATH`.
pub fn in_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file() && is_executable(&candidate)
        })
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// Salida de un proceso terminado.
pub struct ProcOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Ejecuta `cmd` sin stdin y espera como máximo `timeout`; si se pasa, mata
/// el proceso. stdout/stderr se leen en hilos para que un proceso que escribe
/// mucho (PNG por stdout) no se bloquee con la tubería llena.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<ProcOutput, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("tiempo agotado ({} ms)", timeout.as_millis()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok(ProcOutput {
        success: status.success(),
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    })
}

/// Directorio privado (`0700`) para los temporales de captura:
/// `$XDG_RUNTIME_DIR/fokkus-jarvis` (tmpfs, por usuario) o, sin él, el
/// directorio temporal del sistema.
pub fn private_tmp_dir() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("fokkus-jarvis");
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Archivo temporal que se borra al soltarse (también si la captura falla).
pub struct TempFile {
    path: PathBuf,
}

impl TempFile {
    /// Reserva una ruta única en [`private_tmp_dir`] con la extensión dada.
    pub fn new(ext: &str) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!(
            "frame-{}-{}-{nanos}.{ext}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        Ok(Self {
            path: private_tmp_dir()?.join(name),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_file_is_removed_on_drop() {
        let path = {
            let tmp = TempFile::new("png").unwrap();
            std::fs::write(tmp.path(), b"x").unwrap();
            assert!(tmp.path().exists());
            tmp.path().to_path_buf()
        };
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_dir_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_tmp_dir().unwrap();
        let mode = dir.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_slow_process() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let started = Instant::now();
        let err = run_with_timeout(cmd, Duration::from_millis(100))
            .err()
            .unwrap();
        assert!(err.contains("tiempo agotado"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_captures_stdout() {
        let mut cmd = Command::new("printf");
        cmd.arg("hola");
        let out = run_with_timeout(cmd, Duration::from_secs(5)).unwrap();
        assert!(out.success);
        assert_eq!(out.stdout, b"hola");
    }
}
