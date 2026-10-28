//! Icono en la bandeja del sistema (StatusNotifierItem).
//!
//! La esfera vive en una capa overlay y no aparece en la barra de tareas; el
//! acceso permanente es un icono en la bandeja, como Steam o Discord:
//!
//! - Clic izquierdo (`Activate`) → [`ShellCommand::ToggleVisibility`].
//! - Menú contextual (`com.canonical.dbusmenu`) → Panel Rápido, Ajustes,
//!   mostrar/ocultar y Salir. Con sub-sesiones vivas (HU-67) aparece además
//!   el submenú "Sub-sesiones", con "Abrir detalle" y "Cancelar" por hilo.
//!
//! La lógica del menú ([`TrayAction`]) es pura y multiplataforma. El servicio
//! D-Bus ([`TrayIcon`]) solo existe en Linux: registra
//! `org.kde.StatusNotifierItem-<pid>-1` en el bus de sesión y lo anuncia a
//! `org.kde.StatusNotifierWatcher`. Un hilo supervisor lo vuelve a anunciar si
//! el watcher (plasmashell) se reinicia.

use crate::ShellCommand;

/// Opción del menú contextual del icono de bandeja.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Muestra u oculta la esfera.
    ToggleVisibility,
    /// Abre el Panel Rápido.
    QuickPanel,
    /// Abre Ajustes Unificados.
    Settings,
    /// Cierra Fokkus-Jarvis.
    Quit,
}

impl TrayAction {
    /// Opciones en el orden en que se muestran. Antes de `Quit` va un
    /// separador.
    pub const ALL: [TrayAction; 4] = [
        TrayAction::ToggleVisibility,
        TrayAction::QuickPanel,
        TrayAction::Settings,
        TrayAction::Quit,
    ];

    /// Identificador del elemento en `com.canonical.dbusmenu` (0 es la raíz).
    pub fn menu_id(self) -> i32 {
        match self {
            TrayAction::ToggleVisibility => 1,
            TrayAction::QuickPanel => 2,
            TrayAction::Settings => 3,
            TrayAction::Quit => 4,
        }
    }

    /// Acción asociada a un identificador del menú, si existe.
    pub fn from_menu_id(id: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.menu_id() == id)
    }

    /// Texto del elemento (el `_` marca el acelerador de teclado).
    pub fn label(self) -> &'static str {
        match self {
            TrayAction::ToggleVisibility => "_Mostrar / ocultar esfera",
            TrayAction::QuickPanel => "_Panel rápido",
            TrayAction::Settings => "_Ajustes",
            TrayAction::Quit => "_Salir",
        }
    }

    /// Icono freedesktop del elemento.
    pub fn icon_name(self) -> &'static str {
        match self {
            TrayAction::ToggleVisibility => "view-visible",
            TrayAction::QuickPanel => "configure-shortcuts",
            TrayAction::Settings => "configure",
            TrayAction::Quit => "application-exit",
        }
    }

    /// Comando que la aplicación debe ejecutar.
    pub fn command(self) -> ShellCommand {
        match self {
            TrayAction::ToggleVisibility => ShellCommand::ToggleVisibility,
            TrayAction::QuickPanel => ShellCommand::QuickPanelDefault,
            TrayAction::Settings => ShellCommand::Configure,
            TrayAction::Quit => ShellCommand::Quit,
        }
    }
}

/// Identificador del separador previo a "Salir" en `com.canonical.dbusmenu`.
pub const SEPARATOR_MENU_ID: i32 = 100;

/// Identificador del submenú "Sub-sesiones" (HU-67).
pub const SESSIONS_MENU_ID: i32 = 200;

/// Base de los identificadores por sub-sesión: cada una ocupa tres (su
/// submenú, "Abrir detalle" y "Cancelar").
const SESSION_ITEM_BASE: i32 = 1_000;

/// Sub-sesión listada en la bandeja (HU-67).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraySession {
    pub id: u64,
    /// Texto del submenú ("#3 PR de Felipe · trabajando").
    pub label: String,
    /// Hay detalle en disco: habilita "Abrir detalle".
    pub has_detail: bool,
    /// Sigue en cola o en curso: habilita "Cancelar".
    pub cancellable: bool,
}

/// Elemento del submenú de una sub-sesión.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraySessionItem {
    /// El submenú de la sub-sesión (no ejecuta nada).
    Submenu(u64),
    Open(u64),
    Cancel(u64),
}

impl TraySessionItem {
    /// Identificador en `com.canonical.dbusmenu`; `None` si el id de la
    /// sub-sesión no cabe en el espacio de identificadores.
    pub fn menu_id(self) -> Option<i32> {
        let (id, offset) = match self {
            TraySessionItem::Submenu(id) => (id, 0),
            TraySessionItem::Open(id) => (id, 1),
            TraySessionItem::Cancel(id) => (id, 2),
        };
        i32::try_from(id)
            .ok()
            .and_then(|id| id.checked_mul(3))
            .and_then(|slot| slot.checked_add(SESSION_ITEM_BASE + offset))
    }

    /// Elemento asociado a un identificador del menú, si lo es.
    pub fn from_menu_id(menu_id: i32) -> Option<Self> {
        let slot = menu_id.checked_sub(SESSION_ITEM_BASE)?;
        if slot < 0 {
            return None;
        }
        let id = u64::try_from(slot / 3).ok()?;
        Some(match slot % 3 {
            0 => TraySessionItem::Submenu(id),
            1 => TraySessionItem::Open(id),
            _ => TraySessionItem::Cancel(id),
        })
    }

    /// Comando de la aplicación (el submenú no tiene).
    pub fn command(self) -> Option<ShellCommand> {
        match self {
            TraySessionItem::Submenu(_) => None,
            TraySessionItem::Open(id) => Some(ShellCommand::OpenSession(id)),
            TraySessionItem::Cancel(id) => Some(ShellCommand::CancelSession(id)),
        }
    }
}

/// Texto literal para `com.canonical.dbusmenu`: `_` es el acelerador, así que
/// los de un título se duplican.
pub fn menu_label_literal(text: &str) -> String {
    text.replace('_', "__")
}

/// Título del submenú de sub-sesiones.
pub fn sessions_menu_label(count: usize) -> String {
    format!("Sub-_sesiones ({count})")
}

/// Hijos de la raíz del menú: con sub-sesiones se intercala su submenú antes
/// del separador de "Salir".
pub fn root_children(with_sessions: bool) -> Vec<i32> {
    let mut ids = Vec::new();
    for action in TrayAction::ALL {
        if action == TrayAction::Quit {
            if with_sessions {
                ids.push(SESSIONS_MENU_ID);
            }
            ids.push(SEPARATOR_MENU_ID);
        }
        ids.push(action.menu_id());
    }
    ids
}

/// Sub-sesiones compartidas entre la aplicación y el servicio del menú.
#[derive(Debug, Default)]
struct SessionsState {
    sessions: Vec<TraySession>,
    /// Revisión del layout de `com.canonical.dbusmenu`.
    revision: u32,
}

/// Mango para publicar las sub-sesiones en la bandeja desde la aplicación.
///
/// Es barato de clonar. Sin bandeja (otra plataforma o sin bus) no hace nada.
#[derive(Clone, Default)]
pub struct TraySessions {
    state: std::sync::Arc<std::sync::Mutex<SessionsState>>,
    #[cfg(target_os = "linux")]
    conn: Option<zbus::blocking::Connection>,
}

impl TraySessions {
    /// Sustituye la lista; si cambió, sube la revisión y avisa a la bandeja
    /// (`LayoutUpdated`). Devuelve si cambió.
    pub fn set(&self, sessions: Vec<TraySession>) -> bool {
        let revision = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.sessions == sessions {
                return false;
            }
            state.sessions = sessions;
            state.revision = state.revision.wrapping_add(1);
            state.revision
        };
        #[cfg(target_os = "linux")]
        if let Some(conn) = &self.conn {
            linux::layout_updated(conn, revision);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = revision;
        true
    }

    /// Lista vigente.
    pub fn sessions(&self) -> Vec<TraySession> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sessions
            .clone()
    }

    /// Suelta la conexión con el bus para que el nombre del item pueda
    /// desaparecer. La llama `TrayIcon::shutdown`; después de esto `set()`
    /// actualiza el estado pero ya no avisa a la bandeja.
    #[cfg(target_os = "linux")]
    pub(crate) fn disconnect(&mut self) {
        self.conn = None;
    }

    /// Revisión vigente del layout (la usan las pruebas de futuras HUs de la
    /// bandeja; hoy el snapshot de `linux` lee el campo directamente).
    #[allow(dead_code)]
    fn revision(&self) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .revision
    }
}

#[cfg(target_os = "linux")]
pub use linux::TrayIcon;

/// Sin backend de bandeja fuera de Linux (llega con el backend `winit`).
#[cfg(not(target_os = "linux"))]
pub struct TrayIcon;

#[cfg(not(target_os = "linux"))]
impl TrayIcon {
    pub fn start() -> Result<Self, String> {
        Err("plataforma sin backend de bandeja".to_string())
    }

    pub fn commands(&self) -> crossbeam_channel::Receiver<ShellCommand> {
        crossbeam_channel::never()
    }

    pub fn sessions(&self) -> TraySessions {
        TraySessions::default()
    }

    pub fn shutdown(&mut self) {}
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use crossbeam_channel::{Receiver, Sender};
    use zbus::blocking::connection::Builder;
    use zbus::blocking::Connection;
    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

    use super::{
        menu_label_literal, root_children, sessions_menu_label, SessionsState, TrayAction,
        TraySession, TraySessionItem, TraySessions, SEPARATOR_MENU_ID, SESSIONS_MENU_ID,
    };
    use crate::ShellCommand;
    use std::sync::Mutex;

    /// Identificador estable del item (lo usa Plasma para recordar si se
    /// muestra u oculta en la bandeja).
    pub const ITEM_ID: &str = "fokkus-jarvis";
    pub const ITEM_TITLE: &str = "Jarvis";
    pub const ITEM_ICON: &str = "audio-input-microphone";
    pub const ITEM_PATH: &str = "/StatusNotifierItem";
    pub const MENU_PATH: &str = "/MenuBar";

    const WATCHER_SERVICE: &str = "org.kde.StatusNotifierWatcher";
    const WATCHER_PATH: &str = "/StatusNotifierWatcher";
    const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";

    /// Cada cuánto el supervisor comprueba si el watcher (re)apareció.
    const SUPERVISE_INTERVAL: Duration = Duration::from_secs(2);

    /// Nombre de bus del item según la especificación SNI.
    pub fn item_service_name(pid: u32) -> String {
        format!("org.kde.StatusNotifierItem-{pid}-1")
    }

    /// `a(iiay)`: iconos en mapa de bits (ancho, alto, ARGB32).
    type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

    /// `org.kde.StatusNotifierItem`.
    struct Item {
        tx: Sender<ShellCommand>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl Item {
        #[zbus(property, name = "Category")]
        fn category(&self) -> &str {
            "ApplicationStatus"
        }

        #[zbus(property, name = "Id")]
        fn id(&self) -> &str {
            ITEM_ID
        }

        #[zbus(property, name = "Title")]
        fn title(&self) -> &str {
            ITEM_TITLE
        }

        #[zbus(property, name = "Status")]
        fn status(&self) -> &str {
            "Active"
        }

        #[zbus(property, name = "WindowId")]
        fn window_id(&self) -> i32 {
            0
        }

        #[zbus(property, name = "IconName")]
        fn icon_name(&self) -> &str {
            ITEM_ICON
        }

        #[zbus(property, name = "IconPixmap")]
        fn icon_pixmap(&self) -> Pixmaps {
            Vec::new()
        }

        #[zbus(property, name = "OverlayIconName")]
        fn overlay_icon_name(&self) -> &str {
            ""
        }

        #[zbus(property, name = "AttentionIconName")]
        fn attention_icon_name(&self) -> &str {
            ""
        }

        #[zbus(property, name = "ToolTip")]
        fn tool_tip(&self) -> (String, Pixmaps, String, String) {
            (
                ITEM_ICON.to_string(),
                Vec::new(),
                ITEM_TITLE.to_string(),
                "Clic: mostrar u ocultar la esfera".to_string(),
            )
        }

        #[zbus(property, name = "ItemIsMenu")]
        fn item_is_menu(&self) -> bool {
            false
        }

        #[zbus(property, name = "Menu")]
        fn menu(&self) -> OwnedObjectPath {
            ObjectPath::from_static_str_unchecked(MENU_PATH).into()
        }

        /// Clic izquierdo: alterna la visibilidad de la esfera.
        #[zbus(name = "Activate")]
        fn activate(&self, _x: i32, _y: i32) {
            let _ = self.tx.send(ShellCommand::ToggleVisibility);
        }

        /// Clic central: abre el Panel Rápido junto al puntero.
        #[zbus(name = "SecondaryActivate")]
        fn secondary_activate(&self, x: i32, y: i32) {
            let _ = self.tx.send(ShellCommand::QuickPanel { x, y });
        }

        /// Solo se llama si el host no usa `Menu`; se ofrece el Panel Rápido.
        #[zbus(name = "ContextMenu")]
        fn context_menu(&self, x: i32, y: i32) {
            let _ = self.tx.send(ShellCommand::QuickPanel { x, y });
        }

        #[zbus(name = "Scroll")]
        fn scroll(&self, _delta: i32, _orientation: &str) {}

        #[zbus(signal, name = "NewStatus")]
        async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
    }

    type Props = HashMap<String, OwnedValue>;
    /// `(ia{sv}av)`: id, propiedades e hijos (cada hijo es otra estructura).
    type Layout = (i32, Props, Vec<OwnedValue>);

    fn owned(value: Value<'_>) -> OwnedValue {
        value
            .try_to_owned()
            .expect("valores sin descriptores de archivo")
    }

    fn item_props(id: i32, sessions: &[TraySession]) -> Props {
        let mut props = Props::new();
        if id == SEPARATOR_MENU_ID {
            props.insert("type".into(), owned(Value::from("separator")));
            return props;
        }
        if id == 0 {
            props.insert("children-display".into(), owned(Value::from("submenu")));
            return props;
        }
        if let Some(action) = TrayAction::from_menu_id(id) {
            props.insert("label".into(), owned(Value::from(action.label())));
            props.insert("icon-name".into(), owned(Value::from(action.icon_name())));
            props.insert("enabled".into(), owned(Value::from(true)));
            props.insert("visible".into(), owned(Value::from(true)));
            return props;
        }
        if id == SESSIONS_MENU_ID {
            let label = sessions_menu_label(sessions.len());
            props.insert("label".into(), owned(Value::from(label.as_str())));
            props.insert("icon-name".into(), owned(Value::from("view-list-details")));
            props.insert("children-display".into(), owned(Value::from("submenu")));
            props.insert("enabled".into(), owned(Value::from(true)));
            props.insert("visible".into(), owned(Value::from(true)));
            return props;
        }
        let Some(item) = TraySessionItem::from_menu_id(id) else {
            return props;
        };
        let session_id = match item {
            TraySessionItem::Submenu(s) | TraySessionItem::Open(s) | TraySessionItem::Cancel(s) => {
                s
            }
        };
        let Some(session) = sessions.iter().find(|s| s.id == session_id) else {
            return props;
        };
        let (label, icon, enabled) = match item {
            TraySessionItem::Submenu(_) => {
                (menu_label_literal(&session.label), "chronometer", true)
            }
            TraySessionItem::Open(_) => (
                "_Abrir detalle".to_string(),
                "document-open",
                session.has_detail,
            ),
            TraySessionItem::Cancel(_) => {
                ("_Cancelar".to_string(), "process-stop", session.cancellable)
            }
        };
        props.insert("label".into(), owned(Value::from(label.as_str())));
        props.insert("icon-name".into(), owned(Value::from(icon)));
        props.insert("enabled".into(), owned(Value::from(enabled)));
        props.insert("visible".into(), owned(Value::from(true)));
        if matches!(item, TraySessionItem::Submenu(_)) {
            props.insert("children-display".into(), owned(Value::from("submenu")));
        }
        props
    }

    /// Hijos de un elemento del menú.
    fn children_of(id: i32, sessions: &[TraySession]) -> Vec<i32> {
        if id == 0 {
            return root_children(!sessions.is_empty());
        }
        if id == SESSIONS_MENU_ID {
            return sessions
                .iter()
                .filter_map(|s| TraySessionItem::Submenu(s.id).menu_id())
                .collect();
        }
        match TraySessionItem::from_menu_id(id) {
            Some(TraySessionItem::Submenu(session)) if sessions.iter().any(|s| s.id == session) => {
                [
                    TraySessionItem::Open(session),
                    TraySessionItem::Cancel(session),
                ]
                .into_iter()
                .filter_map(TraySessionItem::menu_id)
                .collect()
            }
            _ => Vec::new(),
        }
    }

    fn filter_props(props: Props, names: &[String]) -> Props {
        if names.is_empty() {
            return props;
        }
        props
            .into_iter()
            .filter(|(k, _)| names.iter().any(|n| n == k))
            .collect()
    }

    fn layout_of(id: i32, depth: i32, names: &[String], sessions: &[TraySession]) -> Layout {
        let children = if depth != 0 {
            children_of(id, sessions)
                .into_iter()
                .map(|child| {
                    let (cid, props, grand) = layout_of(child, depth - 1, names, sessions);
                    owned(Value::from((cid, props, grand)))
                })
                .collect()
        } else {
            Vec::new()
        };
        (id, filter_props(item_props(id, sessions), names), children)
    }

    /// Avisa a la bandeja de que el layout cambió (HU-67).
    pub(super) fn layout_updated(conn: &Connection, revision: u32) {
        let _ = conn.emit_signal(
            None::<&str>,
            MENU_PATH,
            "com.canonical.dbusmenu",
            "LayoutUpdated",
            &(revision, 0i32),
        );
    }

    /// `true` si el id corresponde a algo que la aplicación ejecuta.
    fn is_command_id(id: i32) -> bool {
        TrayAction::from_menu_id(id).is_some()
            || TraySessionItem::from_menu_id(id).is_some_and(|item| item.command().is_some())
    }

    /// `com.canonical.dbusmenu`, versión 3 (la que usa Plasma).
    struct Menu {
        tx: Sender<ShellCommand>,
        sessions: Arc<Mutex<SessionsState>>,
    }

    impl Menu {
        /// Copia de las sub-sesiones y revisión vigentes.
        fn snapshot(&self) -> (u32, Vec<TraySession>) {
            let state = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            (state.revision, state.sessions.clone())
        }
    }

    #[zbus::interface(name = "com.canonical.dbusmenu")]
    impl Menu {
        #[zbus(property, name = "Version")]
        fn version(&self) -> u32 {
            3
        }

        #[zbus(property, name = "TextDirection")]
        fn text_direction(&self) -> &str {
            "ltr"
        }

        #[zbus(property, name = "Status")]
        fn status(&self) -> &str {
            "normal"
        }

        #[zbus(property, name = "IconThemePath")]
        fn icon_theme_path(&self) -> Vec<String> {
            Vec::new()
        }

        #[zbus(name = "GetLayout")]
        fn get_layout(
            &self,
            parent_id: i32,
            recursion_depth: i32,
            property_names: Vec<String>,
        ) -> (u32, Layout) {
            let (revision, sessions) = self.snapshot();
            (
                revision.wrapping_add(1),
                layout_of(parent_id, recursion_depth, &property_names, &sessions),
            )
        }

        #[zbus(name = "GetGroupProperties")]
        fn get_group_properties(
            &self,
            ids: Vec<i32>,
            property_names: Vec<String>,
        ) -> Vec<(i32, Props)> {
            let (_, sessions) = self.snapshot();
            ids.into_iter()
                .map(|id| (id, filter_props(item_props(id, &sessions), &property_names)))
                .collect()
        }

        #[zbus(name = "GetProperty")]
        fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
            let (_, sessions) = self.snapshot();
            item_props(id, &sessions)
                .remove(&name)
                .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("{id}.{name}")))
        }

        #[zbus(name = "Event")]
        fn event(&self, id: i32, event_id: &str, _data: Value<'_>, _timestamp: u32) {
            if event_id != "clicked" {
                return;
            }
            let command = TrayAction::from_menu_id(id)
                .map(TrayAction::command)
                .or_else(|| TraySessionItem::from_menu_id(id).and_then(TraySessionItem::command));
            if let Some(command) = command {
                let _ = self.tx.send(command);
            }
        }

        #[zbus(name = "EventGroup")]
        fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
            let mut unknown = Vec::new();
            for (id, event_id, data, timestamp) in events {
                if !is_command_id(id) {
                    unknown.push(id);
                    continue;
                }
                self.event(id, &event_id, Value::from(data), timestamp);
            }
            unknown
        }

        #[zbus(name = "AboutToShow")]
        fn about_to_show(&self, _id: i32) -> bool {
            false
        }

        #[zbus(name = "AboutToShowGroup")]
        fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
            (Vec::new(), Vec::new())
        }

        #[zbus(signal, name = "LayoutUpdated")]
        async fn layout_updated(
            emitter: &SignalEmitter<'_>,
            revision: u32,
            parent: i32,
        ) -> zbus::Result<()>;
    }

    fn watcher_present(conn: &Connection) -> bool {
        let Ok(proxy) = zbus::blocking::fdo::DBusProxy::new(conn) else {
            return false;
        };
        WATCHER_SERVICE
            .try_into()
            .is_ok_and(|name| proxy.name_has_owner(name).unwrap_or(false))
    }

    fn register_with_watcher(conn: &Connection, service: &str) -> Result<(), String> {
        conn.call_method(
            Some(WATCHER_SERVICE),
            WATCHER_PATH,
            Some(WATCHER_IFACE),
            "RegisterStatusNotifierItem",
            &(service,),
        )
        .map(|_| ())
        .map_err(|e| format!("RegisterStatusNotifierItem: {e}"))
    }

    /// Icono de Jarvis en la bandeja del sistema.
    pub struct TrayIcon {
        rx: Receiver<ShellCommand>,
        sessions: TraySessions,
        stop: Arc<AtomicBool>,
        supervisor: Option<JoinHandle<()>>,
        conn: Option<Connection>,
    }

    impl TrayIcon {
        /// Publica el item en el bus de sesión y lo anuncia a la bandeja. Si
        /// todavía no hay bandeja (Plasma arrancando), el supervisor lo anuncia
        /// en cuanto aparezca.
        pub fn start() -> Result<Self, String> {
            let builder = Builder::session().map_err(|e| format!("sin bus de sesión: {e}"))?;
            Self::start_with(builder, std::process::id())
        }

        pub(crate) fn start_with(builder: Builder<'_>, pid: u32) -> Result<Self, String> {
            let (tx, rx) = crossbeam_channel::unbounded();
            let service = item_service_name(pid);
            let shared = Arc::new(Mutex::new(SessionsState::default()));
            let menu = Menu {
                tx: tx.clone(),
                sessions: Arc::clone(&shared),
            };
            let conn = builder
                .name(service.as_str())
                .and_then(|b| b.serve_at(ITEM_PATH, Item { tx }))
                .and_then(|b| b.serve_at(MENU_PATH, menu))
                .and_then(|b| b.build())
                .map_err(|e| format!("no se pudo registrar {service}: {e}"))?;

            let stop = Arc::new(AtomicBool::new(false));
            let supervisor = {
                let conn = conn.clone();
                let stop = Arc::clone(&stop);
                std::thread::Builder::new()
                    .name("jarvis-tray".into())
                    .spawn(move || supervise(&conn, &service, &stop))
                    .map_err(|e| format!("no se pudo lanzar el supervisor del tray: {e}"))?
            };
            let sessions = TraySessions {
                state: shared,
                conn: Some(conn.clone()),
            };
            Ok(Self {
                rx,
                sessions,
                stop,
                supervisor: Some(supervisor),
                conn: Some(conn),
            })
        }

        /// Comandos pedidos desde la bandeja.
        pub fn commands(&self) -> Receiver<ShellCommand> {
            self.rx.clone()
        }

        /// Mango para publicar las sub-sesiones en el menú (HU-67).
        pub fn sessions(&self) -> TraySessions {
            self.sessions.clone()
        }

        /// Retira el icono de la bandeja. Idempotente.
        pub fn shutdown(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.supervisor.take() {
                let _ = handle.join();
            }
            // El nombre de bus solo desaparece cuando se suelta la última
            // referencia a la `Connection`, y `TraySessions` guarda una copia
            // para poder emitir `LayoutUpdated`. Si no se suelta también esa,
            // el nombre sigue vivo y el watcher no retira el item.
            self.sessions.disconnect();
            self.conn = None;
        }
    }

    impl Drop for TrayIcon {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    /// Anuncia el item cada vez que el watcher aparece (arranque de sesión o
    /// reinicio de plasmashell).
    fn supervise(conn: &Connection, service: &str, stop: &AtomicBool) {
        let mut registered = false;
        let mut warned = false;
        while !stop.load(Ordering::SeqCst) {
            if watcher_present(conn) {
                if !registered {
                    match register_with_watcher(conn, service) {
                        Ok(()) => {
                            registered = true;
                            warned = false;
                        }
                        Err(e) if !warned => {
                            eprintln!("[tray] {e}");
                            warned = true;
                        }
                        Err(_) => {}
                    }
                }
            } else {
                registered = false;
            }
            let deadline = Instant::now() + SUPERVISE_INTERVAL;
            while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::test_bus::PrivateBus;
        use std::sync::Mutex;

        /// Bandeja falsa: guarda los items registrados.
        struct FakeWatcher {
            items: Arc<Mutex<Vec<String>>>,
        }

        #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
        impl FakeWatcher {
            #[zbus(name = "RegisterStatusNotifierItem")]
            fn register(&self, service: String) {
                self.items.lock().unwrap().push(service);
            }
        }

        fn wait_for(cond: impl Fn() -> bool) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if cond() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        }

        #[test]
        fn layout_lists_actions_with_separator_before_quit() {
            assert_eq!(root_children(false), vec![1, 2, 3, SEPARATOR_MENU_ID, 4]);
            let (id, _, children) = layout_of(0, -1, &[], &[]);
            assert_eq!(id, 0);
            assert_eq!(children.len(), 5);
            let (_, props, _) = layout_of(4, 0, &[], &[]);
            assert!(props.contains_key("label"));
            let (_, props, _) = layout_of(SEPARATOR_MENU_ID, 0, &[], &[]);
            assert!(props.contains_key("type"));
            // Profundidad 0: sin hijos.
            assert!(layout_of(0, 0, &[], &[]).2.is_empty());
            // Filtro de propiedades.
            let (_, props, _) = layout_of(2, 0, &["label".to_string()], &[]);
            assert_eq!(props.len(), 1);
        }

        #[test]
        fn end_to_end_on_a_private_bus() {
            let Some(bus) = PrivateBus::spawn() else {
                eprintln!("dbus-daemon no disponible: se omite la prueba de integración");
                return;
            };
            let items = Arc::new(Mutex::new(Vec::new()));
            let _watcher = Builder::address(bus.address.as_str())
                .unwrap()
                .name(WATCHER_SERVICE)
                .unwrap()
                .serve_at(
                    WATCHER_PATH,
                    FakeWatcher {
                        items: Arc::clone(&items),
                    },
                )
                .unwrap()
                .build()
                .unwrap();

            let pid = 4242;
            let mut tray =
                TrayIcon::start_with(Builder::address(bus.address.as_str()).unwrap(), pid).unwrap();
            let service = item_service_name(pid);
            assert!(wait_for(|| items.lock().unwrap().contains(&service)));

            let host = Builder::address(bus.address.as_str())
                .unwrap()
                .build()
                .unwrap();
            let item = zbus::blocking::Proxy::new(
                &host,
                service.as_str(),
                ITEM_PATH,
                "org.kde.StatusNotifierItem",
            )
            .unwrap();
            assert_eq!(item.get_property::<String>("Id").unwrap(), ITEM_ID);
            assert_eq!(item.get_property::<String>("Title").unwrap(), ITEM_TITLE);
            assert_eq!(
                item.get_property::<String>("Category").unwrap(),
                "ApplicationStatus"
            );
            assert_eq!(item.get_property::<String>("Status").unwrap(), "Active");
            assert_eq!(item.get_property::<String>("IconName").unwrap(), ITEM_ICON);
            assert_eq!(
                item.get_property::<OwnedObjectPath>("Menu")
                    .unwrap()
                    .as_str(),
                MENU_PATH
            );

            let rx = tray.commands();
            item.call_method("Activate", &(10i32, 20i32)).unwrap();
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(2)),
                Ok(ShellCommand::ToggleVisibility)
            );

            let menu = zbus::blocking::Proxy::new(
                &host,
                service.as_str(),
                MENU_PATH,
                "com.canonical.dbusmenu",
            )
            .unwrap();
            let (_rev, (root, _props, children)): (u32, Layout) = menu
                .call("GetLayout", &(0i32, -1i32, Vec::<String>::new()))
                .unwrap();
            assert_eq!(root, 0);
            assert_eq!(children.len(), 5);

            for (action, expected) in [
                (TrayAction::QuickPanel, ShellCommand::QuickPanelDefault),
                (TrayAction::Settings, ShellCommand::Configure),
                (TrayAction::Quit, ShellCommand::Quit),
            ] {
                menu.call_method(
                    "Event",
                    &(action.menu_id(), "clicked", Value::from(0i32), 0u32),
                )
                .unwrap();
                assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(expected));
            }
            // Eventos que no son clic no disparan nada.
            menu.call_method(
                "Event",
                &(
                    TrayAction::Quit.menu_id(),
                    "hovered",
                    Value::from(0i32),
                    0u32,
                ),
            )
            .unwrap();
            assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());

            tray.shutdown();
            tray.shutdown();
            let dbus = zbus::blocking::fdo::DBusProxy::new(&host).unwrap();
            assert!(wait_for(|| !dbus
                .name_has_owner(service.as_str().try_into().unwrap())
                .unwrap_or(true)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_round_trip_and_are_unique() {
        for action in TrayAction::ALL {
            assert_eq!(TrayAction::from_menu_id(action.menu_id()), Some(action));
            assert_ne!(action.menu_id(), 0, "0 es la raíz del menú");
            assert_ne!(action.menu_id(), SEPARATOR_MENU_ID);
        }
        assert_eq!(TrayAction::from_menu_id(0), None);
        assert_eq!(TrayAction::from_menu_id(SEPARATOR_MENU_ID), None);
    }

    #[test]
    fn actions_map_to_shell_commands() {
        assert_eq!(
            TrayAction::ToggleVisibility.command(),
            ShellCommand::ToggleVisibility
        );
        assert_eq!(
            TrayAction::QuickPanel.command(),
            ShellCommand::QuickPanelDefault
        );
        assert_eq!(TrayAction::Settings.command(), ShellCommand::Configure);
        assert_eq!(TrayAction::Quit.command(), ShellCommand::Quit);
        assert_eq!(TrayAction::ALL.last(), Some(&TrayAction::Quit));
    }
}
