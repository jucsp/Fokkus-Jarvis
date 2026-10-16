//! Bus de sesión privado para las pruebas de integración D-Bus.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

/// Bus de sesión privado (`dbus-daemon`), se mata al soltarlo.
pub(crate) struct PrivateBus {
    child: Child,
    pub(crate) address: String,
}

impl PrivateBus {
    /// `None` si `dbus-daemon` no está instalado: la prueba se omite.
    pub(crate) fn spawn() -> Option<Self> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        let address = line.trim().to_string();
        (!address.is_empty()).then_some(Self { child, address })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
