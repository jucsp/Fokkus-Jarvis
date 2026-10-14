//! Garantía de instancia única mediante un lockfile exclusivo.
//!
//! El lock se materializa en `~/.config/fokkus-jarvis/jarvis.lock` (o en la
//! ruta que resuelva [`crate::paths::lock_file`]). Se crea con
//! `OpenOptions::create_new(true)`, que falla con `AlreadyExists` si otra
//! instancia ya lo mantiene. Al soltar el lock (Drop) se elimina el archivo.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::paths;

/// Guarda el lockfile mientras la aplicación está viva.
///
/// No debe clonarse ni compartirse entre hilos: su ciclo de vida coincide con
/// el de la instancia única.
pub struct InstanceLock {
    path: PathBuf,
    _file: File,
}

impl InstanceLock {
    /// Ruta del lockfile adquirido.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Error al adquirir el lock de instancia única.
#[derive(Debug)]
pub enum InstanceError {
    /// Ya existe una instancia de Fokkus-Jarvis en ejecución.
    AlreadyRunning,
    /// No se pudo crear el directorio de configuración.
    CreateDir(std::io::Error),
    /// Error de I/O distinto de "ya existe".
    Io(std::io::Error),
}

impl fmt::Display for InstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstanceError::AlreadyRunning => {
                write!(f, "ya hay una instancia de Fokkus-Jarvis en ejecución")
            }
            InstanceError::CreateDir(e) => {
                write!(f, "no se pudo crear el directorio del lock: {e}")
            }
            InstanceError::Io(e) => write!(f, "error de I/O al adquirir el lock: {e}"),
        }
    }
}

impl std::error::Error for InstanceError {}

/// Adquiere el lock de instancia única en la ruta por defecto.
pub fn acquire() -> Result<InstanceLock, InstanceError> {
    acquire_at(&paths::lock_file())
}

/// Adquiere un lock de instancia única en una ruta explícita (útil en tests).
pub fn acquire_at(path: &Path) -> Result<InstanceLock, InstanceError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(InstanceError::CreateDir)?;
    }

    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => {
            let lock = InstanceLock {
                path: path.to_path_buf(),
                _file: file,
            };
            // Mejor esfuerzo: dejar el PID para diagnóstico manual.
            let mut file_ref = &lock._file;
            let _ = writeln!(file_ref, "{}", std::process::id());
            Ok(lock)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(InstanceError::AlreadyRunning)
        }
        Err(e) => Err(InstanceError::Io(e)),
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // En Linux el unlink funciona aunque el archivo siga abierto; en otras
        // plataformas es best-effort (no debe bloquear la salida).
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("jarvis-lock-test-{name}-{}", std::process::id()))
            .join("jarvis.lock")
    }

    #[test]
    fn first_acquire_succeeds() {
        let path = temp_lock_path("first");
        let lock = acquire_at(&path).expect("primer lock debe adquirirse");
        assert!(path.exists());
        assert_eq!(lock.path(), path.as_path());
        drop(lock);
        assert!(!path.exists(), "el lock se libera en Drop");
    }

    #[test]
    fn second_acquire_fails_while_first_is_alive() {
        let path = temp_lock_path("second");
        let first = acquire_at(&path).expect("primer lock");
        let second = acquire_at(&path);
        assert!(
            matches!(second, Err(InstanceError::AlreadyRunning)),
            "la segunda instancia debe rechazarse"
        );
        drop(first);
        // Una vez liberado, se puede volver a adquirir.
        let third = acquire_at(&path).expect("lock tras liberar el primero");
        drop(third);
    }

    #[test]
    fn acquire_creates_missing_directories() {
        let path = temp_lock_path("dirs").join("nested").join("jarvis.lock");
        let lock = acquire_at(&path).expect("crea directorios intermedios");
        assert!(path.exists());
        drop(lock);
    }
}
