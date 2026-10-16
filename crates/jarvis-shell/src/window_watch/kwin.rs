//! Backend KDE Plasma: script de KWin + servicio D-Bus de sesión (HU-35).
//!
//! 1. Expone `io.fokkus.Jarvis.WindowWatch` en `/WindowWatch` con el método
//!    `Report(a{sv})`.
//! 2. Escribe el script embebido en `$XDG_RUNTIME_DIR/fokkus-jarvis/`, descarga
//!    cualquier copia huérfana de una ejecución anterior y lo carga con
//!    `org.kde.kwin.Scripting.loadScript`.
//! 3. Un hilo supervisor comprueba cada [`SUPERVISE_INTERVAL`] que el script
//!    sigue cargado; si KWin se reinicia emite [`WindowEvent::SourceLost`] y lo
//!    recarga con backoff (1 s, 5 s, 30 s).
//! 4. `shutdown`/`Drop` lo descargan para no dejar scripts huérfanos.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use zbus::blocking::connection::Builder;
use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Value};

use super::{parse_report, ReportField, WatcherKind, WindowEvent, WindowWatcher};

/// Nombre del servicio de sesión al que llama el script.
pub const SERVICE_NAME: &str = "io.fokkus.Jarvis.WindowWatch";
/// Ruta del objeto que recibe `Report`.
pub const SERVICE_PATH: &str = "/WindowWatch";
/// Nombre del plugin con el que se registra el script en KWin.
pub const SCRIPT_PLUGIN: &str = "fokkus-jarvis-fswatch";

const KWIN_SERVICE: &str = "org.kde.KWin";
const KWIN_SCRIPTING_PATH: &str = "/Scripting";
const KWIN_SCRIPTING_IFACE: &str = "org.kde.kwin.Scripting";
const KWIN_SCRIPT_IFACE: &str = "org.kde.kwin.Script";

const SCRIPT_SOURCE: &str = include_str!("../../assets/kwin_fullscreen_watch.js");
const PID_PLACEHOLDER: &str = "@@JARVIS_PID@@";

/// Cada cuánto el supervisor comprueba que el script sigue cargado.
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(2);
/// Esperas entre reintentos de carga tras perder la fuente.
const RELOAD_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];

fn dbus_error(e: zbus::Error) -> String {
    e.to_string()
}

/// Emisor compartido entre el servicio D-Bus y el supervisor: descarta
/// eventos idénticos al anterior (el script reporta cada paso de un arrastre).
struct Emitter {
    tx: Sender<WindowEvent>,
    last: Mutex<Option<WindowEvent>>,
}

impl Emitter {
    fn emit(&self, event: WindowEvent) {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_ref() == Some(&event) {
            return;
        }
        *last = Some(event.clone());
        let _ = self.tx.send(event);
    }
}

/// Objeto D-Bus que recibe los reportes del script.
struct WatchService {
    emitter: Arc<Emitter>,
    jarvis_pid: u32,
    /// Ya se avisó de un reporte malformado (se avisa una sola vez).
    warned: AtomicBool,
}

#[zbus::interface(name = "io.fokkus.Jarvis.WindowWatch")]
impl WatchService {
    fn report(&self, data: HashMap<String, OwnedValue>) {
        let fields: HashMap<String, ReportField> = data
            .iter()
            .filter_map(|(k, v)| to_field(v).map(|f| (k.clone(), f)))
            .collect();
        let pid = self.jarvis_pid;
        match parse_report(&fields, &|other| is_own_pid(pid, other)) {
            Ok(event) => self.emitter.emit(event),
            Err(e) => {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "[pantalla] reporte de KWin descartado: {e} (se omiten repeticiones)"
                    );
                }
            }
        }
    }
}

/// Quita el tipo D-Bus concreto de un valor del diccionario.
fn to_field(value: &Value<'_>) -> Option<ReportField> {
    Some(match value {
        Value::Bool(b) => ReportField::Bool(*b),
        Value::U8(n) => ReportField::Num(f64::from(*n)),
        Value::I16(n) => ReportField::Num(f64::from(*n)),
        Value::U16(n) => ReportField::Num(f64::from(*n)),
        Value::I32(n) => ReportField::Num(f64::from(*n)),
        Value::U32(n) => ReportField::Num(f64::from(*n)),
        Value::I64(n) => ReportField::Num(*n as f64),
        Value::U64(n) => ReportField::Num(*n as f64),
        Value::F64(n) => ReportField::Num(*n),
        Value::Str(s) => ReportField::Str(s.to_string()),
        Value::Value(inner) => return to_field(inner),
        _ => return None,
    })
}

/// PID padre según `/proc/<pid>/stat` (`None` si no existe o no se entiende).
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // El nombre del proceso va entre paréntesis y puede contener espacios.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// `true` si `pid` es el daemon o una ventana hija suya (Ajustes, Panel).
fn is_own_pid(jarvis_pid: u32, pid: u32) -> bool {
    pid == jarvis_pid || parent_pid(pid) == Some(jarvis_pid)
}

/// Directorio privado del script: `$XDG_RUNTIME_DIR/fokkus-jarvis/`.
fn default_script_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("fokkus-jarvis")
}

/// Escribe el script con el PID sustituido en `dir` (creado con modo 0700).
fn write_script(dir: &Path, jarvis_pid: u32) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("no se pudo crear {}: {e}", dir.display()))?;
    let path = dir.join("kwin_fullscreen_watch.js");
    let source = SCRIPT_SOURCE.replace(PID_PLACEHOLDER, &jarvis_pid.to_string());
    std::fs::write(&path, source)
        .map_err(|e| format!("no se pudo escribir {}: {e}", path.display()))?;
    Ok(path)
}

fn scripting_call<B, R>(conn: &Connection, method: &str, body: &B) -> Result<R, String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = conn
        .call_method(
            Some(KWIN_SERVICE),
            KWIN_SCRIPTING_PATH,
            Some(KWIN_SCRIPTING_IFACE),
            method,
            body,
        )
        .map_err(dbus_error)?;
    reply.body().deserialize::<R>().map_err(dbus_error)
}

fn kwin_present(conn: &Connection) -> Result<bool, String> {
    let reply = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &(KWIN_SERVICE,),
        )
        .map_err(dbus_error)?;
    reply.body().deserialize::<bool>().map_err(dbus_error)
}

fn unload_script(conn: &Connection) -> Result<bool, String> {
    scripting_call(conn, "unloadScript", &(SCRIPT_PLUGIN,))
}

fn script_loaded(conn: &Connection) -> Result<bool, String> {
    scripting_call(conn, "isScriptLoaded", &(SCRIPT_PLUGIN,))
}

/// Descarga una copia huérfana, carga el script y lo arranca.
fn load_script(conn: &Connection, path: &Path) -> Result<(), String> {
    if !kwin_present(conn)? {
        return Err("KWin no está en el bus de sesión".to_string());
    }
    // Huérfano de una ejecución que crasheó: se descarta antes de cargar.
    let _ = unload_script(conn);
    let path_str = path.to_string_lossy().into_owned();
    let id: i32 = scripting_call(conn, "loadScript", &(path_str, SCRIPT_PLUGIN))?;
    if id < 0 {
        return Err(format!("loadScript rechazó el script (id {id})"));
    }
    // KWin 6 publica cada script en /Scripting/Script<id>; si no, `start`
    // arranca todos los cargados.
    let run = conn.call_method(
        Some(KWIN_SERVICE),
        format!("{KWIN_SCRIPTING_PATH}/Script{id}").as_str(),
        Some(KWIN_SCRIPT_IFACE),
        "run",
        &(),
    );
    if run.is_err() {
        conn.call_method(
            Some(KWIN_SERVICE),
            KWIN_SCRIPTING_PATH,
            Some(KWIN_SCRIPTING_IFACE),
            "start",
            &(),
        )
        .map_err(dbus_error)?;
    }
    Ok(())
}

/// Comprueba para `--check` que hay bus de sesión y KWin escuchando.
pub(super) fn probe() -> Result<(), String> {
    let conn = Connection::session().map_err(|e| format!("sin bus de sesión: {e}"))?;
    if kwin_present(&conn)? {
        Ok(())
    } else {
        Err("KWin no está en el bus de sesión".to_string())
    }
}

/// Monitor de ventana activa basado en un script de KWin.
pub struct KwinScriptWatcher {
    conn: Option<Connection>,
    rx: Receiver<WindowEvent>,
    stop: Option<Sender<()>>,
    supervisor: Option<JoinHandle<()>>,
    script_path: PathBuf,
}

impl KwinScriptWatcher {
    /// Registra el servicio en el bus de sesión y carga el script en KWin.
    pub fn start(jarvis_pid: u32) -> Result<Self, String> {
        let builder = Builder::session().map_err(|e| format!("sin bus de sesión: {e}"))?;
        Self::start_with(builder, &default_script_dir(), jarvis_pid)
    }

    fn start_with(
        builder: Builder<'_>,
        script_dir: &Path,
        jarvis_pid: u32,
    ) -> Result<Self, String> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let emitter = Arc::new(Emitter {
            tx,
            last: Mutex::new(None),
        });
        let service = WatchService {
            emitter: Arc::clone(&emitter),
            jarvis_pid,
            warned: AtomicBool::new(false),
        };
        let conn = builder
            .name(SERVICE_NAME)
            .and_then(|b| b.serve_at(SERVICE_PATH, service))
            .and_then(|b| b.build())
            .map_err(|e| format!("no se pudo registrar {SERVICE_NAME}: {e}"))?;

        let script_path = write_script(script_dir, jarvis_pid)?;
        if let Err(e) = load_script(&conn, &script_path) {
            let _ = std::fs::remove_file(&script_path);
            return Err(e);
        }

        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let supervisor = {
            let conn = conn.clone();
            let path = script_path.clone();
            std::thread::Builder::new()
                .name("jarvis-fswatch".to_string())
                .spawn(move || supervise(&conn, &path, &emitter, &stop_rx))
                .map_err(|e| format!("no se pudo lanzar el supervisor: {e}"))?
        };

        Ok(Self {
            conn: Some(conn),
            rx,
            stop: Some(stop_tx),
            supervisor: Some(supervisor),
            script_path,
        })
    }
}

/// Bucle del supervisor: detecta la pérdida del script y lo recarga.
fn supervise(conn: &Connection, path: &Path, emitter: &Emitter, stop: &Receiver<()>) {
    let mut failures = 0usize;
    let mut retry_at: Option<Instant> = None;
    loop {
        let wait = retry_at.map_or(SUPERVISE_INTERVAL, |at| {
            at.saturating_duration_since(Instant::now())
                .min(SUPERVISE_INTERVAL)
        });
        match stop.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }

        if let Some(at) = retry_at {
            if Instant::now() < at {
                continue;
            }
            match load_script(conn, path) {
                Ok(()) => {
                    eprintln!("[pantalla] script de KWin recargado");
                    failures = 0;
                    retry_at = None;
                }
                Err(e) => {
                    let delay = RELOAD_BACKOFF[failures.min(RELOAD_BACKOFF.len() - 1)];
                    failures += 1;
                    eprintln!(
                        "[pantalla] no se pudo recargar el script de KWin ({e}); reintento en {} s",
                        delay.as_secs()
                    );
                    retry_at = Some(Instant::now() + delay);
                }
            }
            continue;
        }

        if script_loaded(conn).unwrap_or(false) {
            continue;
        }
        let reason = "KWin descargó el script o se reinició".to_string();
        eprintln!("[pantalla] fuente perdida: {reason}");
        emitter.emit(WindowEvent::SourceLost(reason));
        retry_at = Some(Instant::now() + RELOAD_BACKOFF[0]);
        failures = 1;
    }
}

impl WindowWatcher for KwinScriptWatcher {
    fn kind(&self) -> WatcherKind {
        WatcherKind::KwinScript
    }

    fn events(&self) -> Receiver<WindowEvent> {
        self.rx.clone()
    }

    fn shutdown(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        // Primero el supervisor, para que no recargue lo que se va a descargar.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.supervisor.take() {
            let _ = handle.join();
        }
        if let Err(e) = unload_script(&conn) {
            eprintln!("[pantalla] no se pudo descargar el script de KWin: {e}");
        }
        let _ = std::fs::remove_file(&self.script_path);
    }
}

impl Drop for KwinScriptWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    #[test]
    fn values_lose_their_dbus_type() {
        assert_eq!(to_field(&Value::Bool(true)), Some(ReportField::Bool(true)));
        assert_eq!(to_field(&Value::I32(-3)), Some(ReportField::Num(-3.0)));
        assert_eq!(to_field(&Value::U32(7)), Some(ReportField::Num(7.0)));
        assert_eq!(to_field(&Value::F64(1.5)), Some(ReportField::Num(1.5)));
        assert_eq!(
            to_field(&Value::from("DP-1")),
            Some(ReportField::Str("DP-1".into()))
        );
        let nested = Value::Value(Box::new(Value::I64(9)));
        assert_eq!(to_field(&nested), Some(ReportField::Num(9.0)));
    }

    #[test]
    fn parent_pid_reads_proc() {
        let me = std::process::id();
        assert!(parent_pid(me).is_some());
        assert!(is_own_pid(me, me));
        assert!(!is_own_pid(me, u32::MAX));
    }

    #[test]
    fn script_source_keeps_the_contract() {
        assert!(SCRIPT_SOURCE.contains(PID_PLACEHOLDER));
        for needle in [
            "workspace.windowActivated.connect",
            "fullScreenChanged",
            "frameGeometryChanged",
            "callDBus(SERVICE, \"/WindowWatch\", SERVICE, \"Report\"",
            "\"io.fokkus.Jarvis.WindowWatch\"",
        ] {
            assert!(SCRIPT_SOURCE.contains(needle), "falta {needle}");
        }
        // Privacidad: el título de la ventana nunca sale del compositor.
        assert!(!SCRIPT_SOURCE.contains("caption"));
    }

    #[test]
    fn emitter_drops_repeated_events() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let emitter = Emitter {
            tx,
            last: Mutex::new(None),
        };
        emitter.emit(WindowEvent::None);
        emitter.emit(WindowEvent::None);
        emitter.emit(WindowEvent::SourceLost("x".into()));
        emitter.emit(WindowEvent::None);
        assert_eq!(rx.try_iter().count(), 3);
    }

    /// Bus de sesión privado (`dbus-daemon`), se mata al soltarlo.
    struct PrivateBus {
        child: Child,
        address: String,
    }

    impl PrivateBus {
        fn spawn() -> Option<Self> {
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

    /// KWin falso: registra las llamadas de scripting.
    struct FakeScripting {
        calls: Arc<Mutex<Vec<String>>>,
        loaded: Arc<AtomicBool>,
    }

    #[zbus::interface(name = "org.kde.kwin.Scripting")]
    impl FakeScripting {
        #[zbus(name = "loadScript")]
        fn load_script(&self, path: String, plugin: String) -> i32 {
            let source = std::fs::read_to_string(&path).unwrap_or_default();
            let pid_ok = source.contains(&format!("const JARVIS_PID = {};", std::process::id()));
            self.calls
                .lock()
                .unwrap()
                .push(format!("load:{plugin}:pid_ok={pid_ok}"));
            self.loaded.store(true, Ordering::SeqCst);
            1
        }

        #[zbus(name = "unloadScript")]
        fn unload_script(&self, plugin: String) -> bool {
            self.calls.lock().unwrap().push(format!("unload:{plugin}"));
            self.loaded.swap(false, Ordering::SeqCst)
        }

        #[zbus(name = "isScriptLoaded")]
        fn is_script_loaded(&self, _plugin: String) -> bool {
            self.loaded.load(Ordering::SeqCst)
        }

        #[zbus(name = "start")]
        fn start(&self) {
            self.calls.lock().unwrap().push("start".into());
        }
    }

    struct FakeScript {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "org.kde.kwin.Script")]
    impl FakeScript {
        #[zbus(name = "run")]
        fn run(&self) {
            self.calls.lock().unwrap().push("run".into());
        }
    }

    fn report_from_kwin(bus: &str, active: bool) {
        let conn = Builder::address(bus).unwrap().build().unwrap();
        let mut data: HashMap<&str, Value<'_>> = HashMap::new();
        data.insert("active", Value::Bool(active));
        if active {
            data.insert("app_id", Value::from("mpv"));
            data.insert("pid", Value::I32(1));
            data.insert("fullscreen", Value::Bool(true));
            data.insert("decorated", Value::Bool(false));
            for (k, v) in [("x", 0.0), ("y", 0.0), ("w", 2560.0), ("h", 1440.0)] {
                data.insert(k, Value::F64(v));
            }
            data.insert("output", Value::from("DP-2"));
            for (k, v) in [("ox", 0), ("oy", 0), ("ow", 2560), ("oh", 1440)] {
                data.insert(k, Value::I32(v));
            }
            data.insert("own", Value::Bool(false));
        }
        conn.call_method(
            Some(SERVICE_NAME),
            SERVICE_PATH,
            Some(SERVICE_NAME),
            "Report",
            &(data,),
        )
        .unwrap();
    }

    #[test]
    fn end_to_end_on_a_private_bus() {
        let Some(bus) = PrivateBus::spawn() else {
            eprintln!("dbus-daemon no disponible: se omite la prueba de integración");
            return;
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let loaded = Arc::new(AtomicBool::new(false));
        let _kwin = Builder::address(bus.address.as_str())
            .unwrap()
            .name(KWIN_SERVICE)
            .unwrap()
            .serve_at(
                KWIN_SCRIPTING_PATH,
                FakeScripting {
                    calls: Arc::clone(&calls),
                    loaded: Arc::clone(&loaded),
                },
            )
            .unwrap()
            .serve_at(
                "/Scripting/Script1",
                FakeScript {
                    calls: Arc::clone(&calls),
                },
            )
            .unwrap()
            .build()
            .unwrap();

        let builder = Builder::address(bus.address.as_str()).unwrap();
        let dir = std::env::temp_dir().join(format!("jarvis-fswatch-test-{}", std::process::id()));
        let mut watcher = KwinScriptWatcher::start_with(builder, &dir, std::process::id()).unwrap();
        assert_eq!(watcher.kind(), WatcherKind::KwinScript);
        assert!(watcher.script_path.exists());
        let rx = watcher.events();

        report_from_kwin(&bus.address, true);
        report_from_kwin(&bus.address, true); // repetido: se descarta
        report_from_kwin(&bus.address, false);
        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let WindowEvent::Active(w, out) = first else {
            panic!("se esperaba Active, llegó {first:?}");
        };
        assert_eq!((w.app_id.as_str(), w.fullscreen), ("mpv", true));
        assert_eq!(out.name, "DP-2");
        assert_eq!(out.rect.w, 2560);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            WindowEvent::None
        );
        assert!(rx.try_recv().is_err());

        let script_path = watcher.script_path.clone();
        watcher.shutdown();
        watcher.shutdown(); // idempotente
        assert!(!loaded.load(Ordering::SeqCst), "el script quedó cargado");
        assert!(!script_path.exists());
        let _ = std::fs::remove_dir(&dir);
        let calls = calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                format!("unload:{SCRIPT_PLUGIN}"),
                format!("load:{SCRIPT_PLUGIN}:pid_ok=true"),
                "run".to_string(),
                format!("unload:{SCRIPT_PLUGIN}"),
            ]
        );
    }
}
