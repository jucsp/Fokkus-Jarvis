//! Resolución de rutas de datos de Fokkus-Jarvis.
//!
//! Centraliza la ubicación del directorio de configuración para que `config`
//! y `single_instance` compartan la misma raíz sin duplicar lógica.

use std::path::PathBuf;

/// Nombre del directorio de configuración.
pub const APP_DIR_NAME: &str = "fokkus-jarvis";

/// Nombre del archivo de configuración TOML.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Nombre del lockfile de instancia única.
pub const LOCK_FILE_NAME: &str = "jarvis.lock";

/// Devuelve el directorio de configuración:
///
/// 1. `$XDG_CONFIG_HOME/fokkus-jarvis` si `XDG_CONFIG_HOME` está definido;
/// 2. `$HOME/.config/fokkus-jarvis` en caso contrario (Linux/macOS);
/// 3. `std::env::temp_dir()/fokkus-jarvis` como último recurso.
pub fn config_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join(APP_DIR_NAME);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".config").join(APP_DIR_NAME);
        }
    }
    std::env::temp_dir().join(APP_DIR_NAME)
}

/// Ruta del archivo de configuración TOML.
pub fn config_file() -> PathBuf {
    config_dir().join(CONFIG_FILE_NAME)
}

/// Ruta del lockfile de instancia única.
pub fn lock_file() -> PathBuf {
    config_dir().join(LOCK_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_is_absolute() {
        let dir = config_dir();
        assert!(dir.is_absolute(), "la ruta debe ser absoluta: {dir:?}");
    }

    #[test]
    fn config_file_lives_inside_config_dir() {
        let dir = config_dir();
        let file = config_file();
        assert_eq!(file.parent().unwrap(), dir);
        assert_eq!(file.file_name().unwrap(), CONFIG_FILE_NAME);
    }

    #[test]
    fn lock_file_lives_inside_config_dir() {
        let dir = config_dir();
        let lock = lock_file();
        assert_eq!(lock.parent().unwrap(), dir);
        assert_eq!(lock.file_name().unwrap(), LOCK_FILE_NAME);
    }
}
