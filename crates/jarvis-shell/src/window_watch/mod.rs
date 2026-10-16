//! Monitor de la ventana activa para detectar pantalla completa (HU-35).
//!
//! Un cliente Wayland no puede consultar ventanas ajenas, así que cada familia
//! de compositor necesita su propio backend ([`WatcherKind`]). Todos entregan
//! [`WindowEvent`] por un canal `crossbeam` que el bucle principal drena sin
//! bloquear el render. Hoy existen:
//!
//! - `KwinScriptWatcher` (Linux, KDE Plasma): script de KWin que reporta por
//!   D-Bus al servicio de sesión `io.fokkus.Jarvis.WindowWatch`.
//! - [`NullWatcher`]: sin detección (comportamiento `ignore` o entorno sin
//!   fuente fiable). Nunca finge eventos.

use std::collections::HashMap;
use std::fmt;

use crossbeam_channel::Receiver;
use jarvis_core::{FullscreenBehavior, OutputGeometry, Rect, WindowSnapshot};

#[cfg(target_os = "linux")]
mod kwin;

#[cfg(target_os = "linux")]
pub use kwin::KwinScriptWatcher;

/// Evento del monitor de ventana activa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowEvent {
    /// Ventana activa y geometría del output donde está.
    Active(WindowSnapshot, OutputGeometry),
    /// Sin ventana activa (escritorio vacío).
    None,
    /// La fuente se cayó (KWin reiniciado, D-Bus perdido).
    SourceLost(String),
}

/// Backend de detección en uso.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherKind {
    KwinScript,
    WlrToplevel,
    X11,
    Windows,
    Unavailable,
}

impl fmt::Display for WatcherKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WatcherKind::KwinScript => "script de KWin",
            WatcherKind::WlrToplevel => "wlr-foreign-toplevel",
            WatcherKind::X11 => "X11",
            WatcherKind::Windows => "Windows",
            WatcherKind::Unavailable => "no disponible",
        })
    }
}

/// Fuente de eventos de la ventana activa.
pub trait WindowWatcher: Send {
    /// Backend efectivo (para el log y `--check`).
    fn kind(&self) -> WatcherKind;

    /// Canal de eventos. Se puede pedir varias veces: todos comparten la cola.
    fn events(&self) -> Receiver<WindowEvent>;

    /// Detiene el monitor y libera lo que haya cargado en el compositor.
    /// Idempotente.
    fn shutdown(&mut self);

    /// Descripción para el log de arranque.
    fn describe(&self) -> String {
        self.kind().to_string()
    }
}

/// Monitor vacío: no emite eventos nunca.
#[derive(Debug, Clone)]
pub struct NullWatcher {
    reason: String,
}

impl NullWatcher {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// Por qué no hay detección (para el log y `--check`).
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl WindowWatcher for NullWatcher {
    fn kind(&self) -> WatcherKind {
        WatcherKind::Unavailable
    }

    fn events(&self) -> Receiver<WindowEvent> {
        crossbeam_channel::never()
    }

    fn shutdown(&mut self) {}

    fn describe(&self) -> String {
        format!("no disponible ({})", self.reason)
    }
}

/// Entorno de escritorio según `XDG_CURRENT_DESKTOP` (lista separada por `:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Desktop {
    Kde,
    Gnome,
    Wlroots,
    Other,
}

fn classify_desktop(current_desktop: &str) -> Desktop {
    let parts: Vec<String> = current_desktop
        .split(':')
        .map(|p| p.trim().to_ascii_lowercase())
        .collect();
    let has = |name: &str| parts.iter().any(|p| p == name);
    if has("kde") {
        Desktop::Kde
    } else if has("gnome") || has("ubuntu") {
        Desktop::Gnome
    } else if ["hyprland", "sway", "river", "wlroots"]
        .iter()
        .any(|n| has(n))
    {
        Desktop::Wlroots
    } else {
        Desktop::Other
    }
}

/// Motivo por el que no hay backend para `desktop` (o `None` si es KDE).
fn unsupported_reason(desktop: Desktop) -> Option<&'static str> {
    match desktop {
        Desktop::Kde => None,
        Desktop::Gnome => Some("GNOME/Mutter"),
        Desktop::Wlroots => Some("wlroots: backend wlr-foreign-toplevel pendiente"),
        Desktop::Other => Some("compositor sin fuente de detección soportada"),
    }
}

fn current_desktop() -> Desktop {
    classify_desktop(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
}

/// Elige y arranca el backend de detección.
///
/// Con [`FullscreenBehavior::Ignore`] no carga nada (CA-35.8). Si el backend
/// del entorno falla al arrancar, degrada a [`NullWatcher`] con el motivo.
pub fn detect_watcher(jarvis_pid: u32, behavior: FullscreenBehavior) -> Box<dyn WindowWatcher> {
    if behavior == FullscreenBehavior::Ignore {
        return Box::new(NullWatcher::new(
            "desactivada (fullscreen_behavior = ignore)",
        ));
    }
    let desktop = current_desktop();
    if let Some(reason) = unsupported_reason(desktop) {
        return Box::new(NullWatcher::new(reason));
    }
    #[cfg(target_os = "linux")]
    {
        match KwinScriptWatcher::start(jarvis_pid) {
            Ok(watcher) => Box::new(watcher),
            Err(e) => Box::new(NullWatcher::new(format!("KWin: {e}"))),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = jarvis_pid;
        Box::new(NullWatcher::new("plataforma sin backend de detección"))
    }
}

/// Sonda para `--check`: informa qué backend se usaría sin cargar nada en el
/// compositor. `Err` lleva el motivo de la degradación.
pub fn probe(behavior: FullscreenBehavior) -> Result<WatcherKind, String> {
    if behavior == FullscreenBehavior::Ignore {
        return Err("desactivada (fullscreen_behavior = ignore)".to_string());
    }
    if let Some(reason) = unsupported_reason(current_desktop()) {
        return Err(reason.to_string());
    }
    #[cfg(target_os = "linux")]
    {
        kwin::probe().map(|()| WatcherKind::KwinScript)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("plataforma sin backend de detección".to_string())
    }
}

/// Valor de un campo del diccionario `a{sv}` que envía el script de KWin,
/// ya despojado del tipo D-Bus concreto (KWin manda los números como `i` o
/// `d` según la versión de Qt).
#[derive(Debug, Clone, PartialEq)]
pub enum ReportField {
    Bool(bool),
    Num(f64),
    Str(String),
}

/// Error al interpretar un reporte del compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportError {
    /// Falta el campo o tiene un tipo inesperado.
    Field(&'static str),
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportError::Field(name) => write!(f, "campo «{name}» ausente o inválido"),
        }
    }
}

impl std::error::Error for ReportError {}

type Fields = HashMap<String, ReportField>;

fn get_bool(fields: &Fields, key: &'static str) -> Result<bool, ReportError> {
    match fields.get(key) {
        Some(ReportField::Bool(b)) => Ok(*b),
        Some(ReportField::Num(n)) => Ok(*n != 0.0),
        _ => Err(ReportError::Field(key)),
    }
}

fn get_str(fields: &Fields, key: &'static str) -> Result<String, ReportError> {
    match fields.get(key) {
        Some(ReportField::Str(s)) => Ok(s.clone()),
        _ => Err(ReportError::Field(key)),
    }
}

fn get_num(fields: &Fields, key: &'static str) -> Result<f64, ReportError> {
    match fields.get(key) {
        Some(ReportField::Num(n)) if n.is_finite() => Ok(*n),
        _ => Err(ReportError::Field(key)),
    }
}

fn get_i32(fields: &Fields, key: &'static str) -> Result<i32, ReportError> {
    let n = get_num(fields, key)?.round();
    if n < f64::from(i32::MIN) || n > f64::from(i32::MAX) {
        return Err(ReportError::Field(key));
    }
    Ok(n as i32)
}

fn get_u32(fields: &Fields, key: &'static str) -> Result<u32, ReportError> {
    let n = get_num(fields, key)?.round();
    if !(0.0..=f64::from(u32::MAX)).contains(&n) {
        return Err(ReportError::Field(key));
    }
    Ok(n as u32)
}

fn get_rect(fields: &Fields, keys: [&'static str; 4]) -> Result<Rect, ReportError> {
    Ok(Rect {
        x: get_i32(fields, keys[0])?,
        y: get_i32(fields, keys[1])?,
        w: get_u32(fields, keys[2])?,
        h: get_u32(fields, keys[3])?,
    })
}

/// Convierte el diccionario del script de KWin en un [`WindowEvent`].
///
/// `is_own_pid` decide si un PID pertenece a Fokkus-Jarvis (el daemon o sus
/// ventanas hijas de PyQt): esas ventanas se marcan `own`. `pid` es opcional.
pub fn parse_report(
    fields: &Fields,
    is_own_pid: &dyn Fn(u32) -> bool,
) -> Result<WindowEvent, ReportError> {
    if !get_bool(fields, "active")? {
        return Ok(WindowEvent::None);
    }
    let pid_own = get_u32(fields, "pid").is_ok_and(|pid| pid != 0 && is_own_pid(pid));
    let script_own = get_bool(fields, "own").unwrap_or(false);
    let snapshot = WindowSnapshot {
        app_id: get_str(fields, "app_id")?,
        fullscreen: get_bool(fields, "fullscreen")?,
        decorated: get_bool(fields, "decorated")?,
        geometry: get_rect(fields, ["x", "y", "w", "h"])?,
        output: get_str(fields, "output")?,
        own: script_own || pid_own,
    };
    let output = OutputGeometry {
        name: snapshot.output.clone(),
        rect: get_rect(fields, ["ox", "oy", "ow", "oh"])?,
    };
    Ok(WindowEvent::Active(snapshot, output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_report() -> Fields {
        let mut f = Fields::new();
        f.insert("active".into(), ReportField::Bool(true));
        f.insert("app_id".into(), ReportField::Str("steam_app_570".into()));
        f.insert("pid".into(), ReportField::Num(4242.0));
        f.insert("fullscreen".into(), ReportField::Bool(false));
        f.insert("decorated".into(), ReportField::Bool(false));
        for (k, v) in [("x", 0.0), ("y", 0.0), ("w", 1920.0), ("h", 1080.0)] {
            f.insert(k.into(), ReportField::Num(v));
        }
        f.insert("output".into(), ReportField::Str("DP-1".into()));
        for (k, v) in [("ox", 0.0), ("oy", 0.0), ("ow", 1920.0), ("oh", 1080.0)] {
            f.insert(k.into(), ReportField::Num(v));
        }
        f.insert("own".into(), ReportField::Bool(false));
        f
    }

    fn never_own(_: u32) -> bool {
        false
    }

    #[test]
    fn parses_a_complete_report() {
        let ev = parse_report(&full_report(), &never_own).unwrap();
        let WindowEvent::Active(w, out) = ev else {
            panic!("se esperaba Active");
        };
        assert_eq!(w.app_id, "steam_app_570");
        assert!(!w.fullscreen && !w.decorated && !w.own);
        assert_eq!(
            w.geometry,
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080
            }
        );
        assert_eq!(out.name, "DP-1");
        assert_eq!(out.rect, w.geometry);
        assert!(jarvis_core::is_effectively_fullscreen(&w, &out));
    }

    #[test]
    fn inactive_report_is_none_even_without_other_fields() {
        let mut f = Fields::new();
        f.insert("active".into(), ReportField::Bool(false));
        assert_eq!(parse_report(&f, &never_own), Ok(WindowEvent::None));
    }

    #[test]
    fn fractional_geometry_is_rounded() {
        // KWin 6 usa QRectF: con escala fraccional llegan decimales.
        let mut f = full_report();
        f.insert("x".into(), ReportField::Num(-0.4));
        f.insert("w".into(), ReportField::Num(1919.6));
        let WindowEvent::Active(w, _) = parse_report(&f, &never_own).unwrap() else {
            panic!("se esperaba Active");
        };
        assert_eq!((w.geometry.x, w.geometry.w), (0, 1920));
    }

    #[test]
    fn missing_or_mistyped_fields_are_rejected() {
        for key in [
            "active",
            "app_id",
            "fullscreen",
            "decorated",
            "w",
            "output",
            "oh",
        ] {
            let mut f = full_report();
            f.remove(key);
            assert_eq!(
                parse_report(&f, &never_own),
                Err(ReportError::Field(key)),
                "{key}"
            );
        }
        let mut f = full_report();
        f.insert("w".into(), ReportField::Str("ancho".into()));
        assert_eq!(parse_report(&f, &never_own), Err(ReportError::Field("w")));
        let mut f = full_report();
        f.insert("h".into(), ReportField::Num(-5.0));
        assert_eq!(parse_report(&f, &never_own), Err(ReportError::Field("h")));
        let mut f = full_report();
        f.insert("x".into(), ReportField::Num(f64::NAN));
        assert_eq!(parse_report(&f, &never_own), Err(ReportError::Field("x")));
    }

    #[test]
    fn own_comes_from_the_script_or_from_the_pid() {
        let mut f = full_report();
        f.insert("own".into(), ReportField::Bool(true));
        let WindowEvent::Active(w, _) = parse_report(&f, &never_own).unwrap() else {
            panic!("se esperaba Active");
        };
        assert!(w.own);

        // Ventana de Ajustes (hija PyQt del daemon): el script no lo sabe.
        let f = full_report();
        let WindowEvent::Active(w, _) = parse_report(&f, &|pid| pid == 4242).unwrap() else {
            panic!("se esperaba Active");
        };
        assert!(w.own);

        // Sin `pid` ni `own` sigue siendo válido (no es de Jarvis).
        let mut f = full_report();
        f.remove("pid");
        f.remove("own");
        let WindowEvent::Active(w, _) = parse_report(&f, &|_| true).unwrap() else {
            panic!("se esperaba Active");
        };
        assert!(!w.own);
    }

    #[test]
    fn desktop_classification() {
        assert_eq!(classify_desktop("KDE"), Desktop::Kde);
        assert_eq!(classify_desktop("ubuntu:GNOME"), Desktop::Gnome);
        assert_eq!(classify_desktop("Hyprland"), Desktop::Wlroots);
        assert_eq!(classify_desktop("sway"), Desktop::Wlroots);
        assert_eq!(classify_desktop(""), Desktop::Other);
        assert_eq!(unsupported_reason(Desktop::Kde), None);
        assert_eq!(unsupported_reason(Desktop::Gnome), Some("GNOME/Mutter"));
    }

    #[test]
    fn ignore_never_starts_a_backend() {
        let watcher = detect_watcher(std::process::id(), FullscreenBehavior::Ignore);
        assert_eq!(watcher.kind(), WatcherKind::Unavailable);
        assert!(probe(FullscreenBehavior::Ignore).is_err());
    }

    #[test]
    fn null_watcher_is_silent_and_idempotent() {
        let mut watcher = NullWatcher::new("GNOME/Mutter");
        assert_eq!(watcher.reason(), "GNOME/Mutter");
        let rx = watcher.events();
        assert!(rx.try_recv().is_err());
        watcher.shutdown();
        watcher.shutdown();
        assert_eq!(watcher.kind(), WatcherKind::Unavailable);
        assert_eq!(watcher.describe(), "no disponible (GNOME/Mutter)");
    }
}
