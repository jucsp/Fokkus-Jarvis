//! Captura con XDG Desktop Portal `org.freedesktop.portal.Screenshot`.
//!
//! Genérico (KDE, GNOME y wlroots con su portal). Se pide en modo no
//! interactivo; la primera vez el compositor puede mostrar un diálogo de
//! permiso. El portal deja el PNG en disco y devuelve su URI: se lee y se
//! borra de inmediato (CA-38.2).

use std::time::Duration;

use super::ScreenCapture;
use super::{crop_to_target, CaptureBackendKind, CaptureError, CaptureTarget, CapturedFrame};
use crate::geometry::DesktopLayout;

/// Margen para el diálogo de permiso del primer uso.
const TIMEOUT: Duration = Duration::from_secs(20);

pub struct PortalCapture {
    layout: DesktopLayout,
}

impl PortalCapture {
    pub fn new(layout: DesktopLayout) -> Self {
        Self { layout }
    }
}

impl ScreenCapture for PortalCapture {
    fn kind(&self) -> CaptureBackendKind {
        CaptureBackendKind::Portal
    }

    fn capture(&mut self, target: &CaptureTarget) -> Result<CapturedFrame, CaptureError> {
        if *target == CaptureTarget::ActiveWindow {
            return Err(CaptureError::UnsupportedTarget(
                "el portal captura el escritorio completo; usa Region con la geometría de la ventana"
                    .into(),
            ));
        }
        let uri = imp::screenshot_uri(TIMEOUT)?;
        let path = crate::portal_dbus::file_uri_to_path(&uri)
            .ok_or_else(|| CaptureError::Backend(format!("URI inesperada del portal: {uri}")))?;
        let data = std::fs::read(&path);
        // El archivo es nuestro (lo creó esta petición): se borra pase lo que pase.
        let _ = std::fs::remove_file(&path);
        let data =
            data.map_err(|e| CaptureError::Backend(format!("no se pudo leer la captura: {e}")))?;
        let frame = CapturedFrame::from_png(&data, &self.layout, None, self.kind())?;
        crop_to_target(frame, target, &self.layout)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::{OwnedValue, Value};

    use crate::capture::CaptureError;
    use crate::portal_dbus::{PORTAL_PATH, PORTAL_SERVICE};

    fn backend(e: impl std::fmt::Display) -> CaptureError {
        CaptureError::Backend(e.to_string())
    }

    /// Pide la captura y espera la señal `Response` de la petición. La
    /// suscripción se crea ANTES de la llamada (ruta predecible con
    /// `handle_token`) para no perder una respuesta inmediata.
    fn request(conn: &Connection) -> Result<String, CaptureError> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let token = format!(
            "fokkus_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let sender = conn
            .unique_name()
            .ok_or_else(|| backend("conexión D-Bus sin nombre único"))?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let handle = format!("{PORTAL_PATH}/request/{sender}/{token}");
        let proxy = Proxy::new(
            conn,
            PORTAL_SERVICE,
            handle.as_str(),
            "org.freedesktop.portal.Request",
        )
        .map_err(backend)?;
        let mut responses = proxy.receive_signal("Response").map_err(backend)?;

        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert("interactive", Value::from(false));
        conn.call_method(
            Some(PORTAL_SERVICE),
            PORTAL_PATH,
            Some("org.freedesktop.portal.Screenshot"),
            "Screenshot",
            &("", options),
        )
        .map_err(|e| CaptureError::Unavailable(format!("portal Screenshot: {e}")))?;

        let msg = responses
            .next()
            .ok_or_else(|| backend("el portal cerró la petición sin respuesta"))?;
        let (code, results): (u32, HashMap<String, OwnedValue>) =
            msg.body().deserialize().map_err(backend)?;
        match code {
            0 => {}
            1 => return Err(CaptureError::Cancelled),
            _ => return Err(CaptureError::PermissionDenied),
        }
        let uri = results
            .get("uri")
            .ok_or_else(|| backend("respuesta del portal sin 'uri'"))?;
        String::try_from(uri.try_clone().map_err(backend)?).map_err(backend)
    }

    /// Ejecuta la petición en un hilo para poder aplicar un timeout: el
    /// iterador de señales de `zbus` bloquea sin límite.
    pub fn screenshot_uri(timeout: Duration) -> Result<String, CaptureError> {
        let conn = Connection::session()
            .map_err(|e| CaptureError::Unavailable(format!("sin bus de sesión ({e})")))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("jarvis-portal-shot".into())
            .spawn(move || {
                let _ = tx.send(request(&conn));
            })
            .map_err(backend)?;
        rx.recv_timeout(timeout)
            .unwrap_or(Err(CaptureError::Timeout))
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::time::Duration;

    use crate::capture::CaptureError;

    pub fn screenshot_uri(_timeout: Duration) -> Result<String, CaptureError> {
        Err(CaptureError::Unavailable(
            "XDG Desktop Portal solo existe en Linux".into(),
        ))
    }
}
