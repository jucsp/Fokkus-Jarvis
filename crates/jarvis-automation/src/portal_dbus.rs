//! Acceso al XDG Desktop Portal por D-Bus de sesión (`zbus` blocking, la misma
//! pila que D-35.1/D-38.2). Solo Linux.

/// Servicio del portal en el bus de sesión.
pub const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
/// Objeto del portal.
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

/// Versión de una interfaz del portal (`org.freedesktop.portal.Screenshot`,
/// `…RemoteDesktop`), o el motivo por el que no está.
#[cfg(target_os = "linux")]
pub fn interface_version(interface: &str) -> Result<u32, String> {
    use zbus::blocking::Connection;
    use zbus::zvariant::OwnedValue;

    let conn = Connection::session().map_err(|e| format!("sin bus de sesión ({e})"))?;
    let reply = conn
        .call_method(
            Some(PORTAL_SERVICE),
            PORTAL_PATH,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(interface, "version"),
        )
        .map_err(|e| format!("{interface} no responde ({e})"))?;
    let value: OwnedValue = reply
        .body()
        .deserialize()
        .map_err(|e| format!("respuesta inválida de {interface} ({e})"))?;
    u32::try_from(value).map_err(|e| format!("versión ilegible de {interface} ({e})"))
}

#[cfg(not(target_os = "linux"))]
pub fn interface_version(_interface: &str) -> Result<u32, String> {
    Err("XDG Desktop Portal solo existe en Linux".into())
}

/// Decodifica una URI `file://` del portal (con escapes `%XX`) a ruta.
pub fn file_uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(out).ok()?;
    path.starts_with('/')
        .then(|| std::path::PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_decoding() {
        assert_eq!(
            file_uri_to_path("file:///tmp/Screenshot%20de%20pantalla.png").unwrap(),
            std::path::PathBuf::from("/tmp/Screenshot de pantalla.png")
        );
        assert_eq!(
            file_uri_to_path("file:///home/u/Im%C3%A1genes/a.png").unwrap(),
            std::path::PathBuf::from("/home/u/Imágenes/a.png")
        );
        assert!(file_uri_to_path("https://x/y.png").is_none());
        assert!(file_uri_to_path("file://relativo").is_none());
        assert!(file_uri_to_path("file:///a%2").is_none());
    }
}
