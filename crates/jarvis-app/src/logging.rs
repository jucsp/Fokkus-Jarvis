//! Log persistente del daemon.
//!
//! Lanzado desde el `.desktop` el proceso hereda stdout/stderr apuntando a
//! `/dev/null` y todo el diagnóstico se pierde. Si stderr no es una terminal,
//! ambos descriptores se redirigen (con `dup2`) a
//! `$XDG_CACHE_HOME/fokkus-jarvis/jarvis.log` (por defecto
//! `~/.cache/fokkus-jarvis/jarvis.log`), así los `println!`/`eprintln!` de
//! todos los crates —y los avisos de las librerías C— acaban en el archivo.
//! Desde una terminal no se toca nada.

use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Tamaño a partir del cual el log se rota a `jarvis.log.1` al arrancar.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Nombre del archivo de log.
const LOG_FILE: &str = "jarvis.log";

/// Ruta del log: `$XDG_CACHE_HOME/fokkus-jarvis/jarvis.log` o
/// `~/.cache/fokkus-jarvis/jarvis.log`.
pub fn log_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })?;
    Some(base.join("fokkus-jarvis").join(LOG_FILE))
}

/// Rota `path` a `<path>.1` si supera `max_bytes` (se conserva una copia).
fn rotate_if_large(path: &Path, max_bytes: u64) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(meta) if meta.len() > max_bytes => {
            let mut rotated = path.as_os_str().to_owned();
            rotated.push(".1");
            fs::rename(path, rotated)
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Abre (o crea) el log en modo append, rotándolo si es demasiado grande.
fn open_log(path: &Path, max_bytes: u64) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    rotate_if_large(path, max_bytes)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Redirige stdout y stderr al log si stderr no es una terminal.
///
/// Devuelve la ruta del log cuando se activó la redirección. Un fallo nunca
/// impide arrancar: el proceso sigue con los descriptores heredados.
pub fn redirect_if_detached() -> Option<PathBuf> {
    if io::stderr().is_terminal() {
        return None;
    }
    let path = log_path()?;
    let file = match open_log(&path, MAX_LOG_BYTES) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("[log] no se pudo abrir {}: {e}", path.display());
            return None;
        }
    };
    if let Err(e) = redirect_stdio(&file) {
        eprintln!(
            "[log] no se pudo redirigir la salida a {}: {e}",
            path.display()
        );
        return None;
    }
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut out = io::stdout();
    let _ = writeln!(
        out,
        "\n=== Fokkus-Jarvis {} — arranque pid={} unix={since_epoch} ===",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
    );
    let _ = out.flush();
    Some(path)
}

#[cfg(unix)]
fn redirect_stdio(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    for target in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: `fd` es un descriptor válido mientras `file` vive y `dup2`
        // solo duplica descriptores; el archivo puede cerrarse después porque
        // stdout/stderr conservan su propia copia.
        if unsafe { libc::dup2(fd, target) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn redirect_stdio(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "redirección de stdio no soportada en esta plataforma",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn open_log_creates_dir_and_appends() {
        let dir = temp_dir("append");
        let path = dir.join("sub").join(LOG_FILE);
        writeln!(open_log(&path, MAX_LOG_BYTES).unwrap(), "uno").unwrap();
        writeln!(open_log(&path, MAX_LOG_BYTES).unwrap(), "dos").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "uno\ndos\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn large_log_is_rotated_once() {
        let dir = temp_dir("rotate");
        let path = dir.join(LOG_FILE);
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "x".repeat(32)).unwrap();

        // Bajo el umbral no se rota.
        drop(open_log(&path, 64).unwrap());
        assert!(!dir.join("jarvis.log.1").exists());

        // Sobre el umbral pasa a `.1` y el log arranca vacío.
        drop(open_log(&path, 16).unwrap());
        assert_eq!(
            fs::read_to_string(dir.join("jarvis.log.1")).unwrap().len(),
            32
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_path_ends_in_cache_dir() {
        if let Some(path) = log_path() {
            assert!(path.ends_with("fokkus-jarvis/jarvis.log"));
        }
    }
}
