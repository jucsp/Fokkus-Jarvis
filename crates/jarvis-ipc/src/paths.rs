//! Resolución de la ruta del socket IPC.
//!
//! El SDD §3 fija `~/.fokkus/jarvis.sock`. La enmienda I7 del dictamen QA
//! propone preferir `$XDG_RUNTIME_DIR/fokkus/jarvis.sock` (tmpfs, limpieza
//! automática) manteniendo `~/.fokkus/jarvis.sock` por compatibilidad. Ambas
//! rutas se calculan aquí; la elección es una decisión del PO y por ahora el
//! valor por defecto es el del SDD.
//!
//! `FOKKUS_JARVIS_SOCKET` permite sobrescribir la ruta (tests, múltiples
//! perfiles).

use std::path::{Path, PathBuf};

/// Directorio por defecto del socket dentro del `$HOME`.
pub const DEFAULT_DIR_NAME: &str = ".fokkus";
/// Nombre del archivo del socket.
pub const SOCKET_FILE_NAME: &str = "jarvis.sock";
/// Directorio usado bajo `$XDG_RUNTIME_DIR` (enmienda I7).
pub const RUNTIME_DIR_NAME: &str = "fokkus";
/// Variable de entorno que sobrescribe la ruta del socket.
pub const ENV_SOCKET_PATH: &str = "FOKKUS_JARVIS_SOCKET";

/// Resuelve la ruta del socket de forma pura (sin leer el entorno).
///
/// Prioridad:
/// 1. `override_path` (variable de entorno o configuración explícita);
/// 2. `home/.fokkus/jarvis.sock`;
/// 3. `temp/fokkus-jarvis/jarvis.sock` si no hay `$HOME`.
pub fn resolve_socket_path(
    override_path: Option<&Path>,
    home: Option<&Path>,
    temp: &Path,
) -> PathBuf {
    if let Some(path) = override_path {
        if !path.as_os_str().is_empty() {
            return path.to_path_buf();
        }
    }
    match home {
        Some(home) => home.join(DEFAULT_DIR_NAME).join(SOCKET_FILE_NAME),
        None => temp.join("fokkus-jarvis").join(SOCKET_FILE_NAME),
    }
}

/// Ruta del socket según el entorno actual.
pub fn default_socket_path() -> PathBuf {
    let override_path = std::env::var_os(ENV_SOCKET_PATH).map(PathBuf::from);
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    resolve_socket_path(
        override_path.as_deref(),
        home.as_deref(),
        &std::env::temp_dir(),
    )
}

/// Ruta alternativa en `$XDG_RUNTIME_DIR` (enmienda I7), si está definida.
pub fn runtime_socket_path(runtime_dir: Option<&Path>) -> Option<PathBuf> {
    runtime_dir
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(RUNTIME_DIR_NAME).join(SOCKET_FILE_NAME))
}

/// Ruta alternativa en `$XDG_RUNTIME_DIR` según el entorno actual.
pub fn runtime_socket_path_from_env() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    runtime_socket_path(runtime.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_over_home() {
        let path = resolve_socket_path(
            Some(Path::new("/tmp/custom.sock")),
            Some(Path::new("/home/tester")),
            Path::new("/tmp"),
        );
        assert_eq!(path, PathBuf::from("/tmp/custom.sock"));
    }

    #[test]
    fn empty_override_is_ignored() {
        let path = resolve_socket_path(
            Some(Path::new("")),
            Some(Path::new("/home/tester")),
            Path::new("/tmp"),
        );
        assert_eq!(path, PathBuf::from("/home/tester/.fokkus/jarvis.sock"));
    }

    #[test]
    fn home_is_used_when_no_override() {
        let path = resolve_socket_path(None, Some(Path::new("/home/tester")), Path::new("/tmp"));
        assert_eq!(path, PathBuf::from("/home/tester/.fokkus/jarvis.sock"));
        assert_eq!(path.file_name().unwrap(), SOCKET_FILE_NAME);
    }

    #[test]
    fn temp_is_the_last_resort() {
        let path = resolve_socket_path(None, None, Path::new("/tmp"));
        assert_eq!(path, PathBuf::from("/tmp/fokkus-jarvis/jarvis.sock"));
    }

    #[test]
    fn runtime_path_is_optional() {
        assert_eq!(runtime_socket_path(None), None);
        assert_eq!(runtime_socket_path(Some(Path::new(""))), None);
        assert_eq!(
            runtime_socket_path(Some(Path::new("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000/fokkus/jarvis.sock"))
        );
    }

    #[test]
    fn default_path_is_absolute_and_named_jarvis_sock() {
        let path = default_socket_path();
        assert!(path.is_absolute(), "ruta no absoluta: {path:?}");
        assert_eq!(path.file_name().unwrap(), SOCKET_FILE_NAME);
    }
}
