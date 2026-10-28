//! Puente entre el monitor de notificaciones del shell y el orquestador de voz
//! (HU-72).
//!
//! `jarvis-audio` no depende de D-Bus: consulta un
//! [`NotificationIntrospection`]. Este módulo lo implementa sobre el
//! [`NotificationWatcher`] de `jarvis-shell` y arma las respuestas con sus
//! plantillas locales (sin LLM, CA-72.4).

use std::sync::Mutex;
use std::time::Instant;

use jarvis_audio::{NotificationIntrospection, NotificationQuery};
use jarvis_core::NotificationsConfig;
use jarvis_shell::notification_watch::{
    spoken_all, spoken_by_app, spoken_latest, start_notification_watcher, NotificationWatchOptions,
    NotificationWatcher, DEFAULT_CAPACITY, DEFAULT_EXCLUDED_APPS,
};

/// Notificaciones del escritorio para el orquestador.
pub struct DesktopNotifications {
    /// El monitor es `Send` pero no `Sync`: el mutex permite consultarlo desde
    /// el hilo de voz y apagarlo desde el principal.
    watcher: Mutex<Box<dyn NotificationWatcher>>,
}

impl DesktopNotifications {
    pub fn new(watcher: Box<dyn NotificationWatcher>) -> Self {
        Self {
            watcher: Mutex::new(watcher),
        }
    }

    /// Arranca el monitor según `[notifications]`. Nunca falla: si el bus no
    /// lo permite, queda el monitor nulo (CA-72.6).
    pub fn start(config: &NotificationsConfig) -> Self {
        Self::new(start_notification_watcher(&watch_options(config)))
    }

    /// Descripción para el log de arranque.
    pub fn describe(&self) -> String {
        self.lock().describe()
    }

    /// Detiene el monitor y libera la conexión. Idempotente.
    pub fn shutdown(&self) {
        self.lock().shutdown();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Box<dyn NotificationWatcher>> {
        self.watcher.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl NotificationIntrospection for DesktopNotifications {
    fn is_available(&self) -> bool {
        self.lock().is_available()
    }

    fn answer(&self, query: &NotificationQuery) -> Option<String> {
        let now = Instant::now();
        let watcher = self.lock();
        match query {
            NotificationQuery::Latest => watcher
                .query_latest()
                .map(|event| spoken_latest(&event, now)),
            NotificationQuery::All => {
                let events = watcher.query_all();
                (!events.is_empty()).then(|| spoken_all(&events, now))
            }
            NotificationQuery::ByApp(app) => {
                let events = watcher.query_by_app(app);
                (!events.is_empty()).then(|| spoken_by_app(app, &events, now))
            }
        }
    }
}

/// Opciones del monitor: la lista fija de exclusiones más la del usuario (el
/// usuario puede añadir apps, nunca quitar los gestores de contraseñas).
fn watch_options(config: &NotificationsConfig) -> NotificationWatchOptions {
    let mut excluded_apps: Vec<String> = DEFAULT_EXCLUDED_APPS
        .iter()
        .map(|app| (*app).to_string())
        .collect();
    excluded_apps.extend(
        config
            .excluded_apps
            .iter()
            .map(|app| app.trim().to_string())
            .filter(|app| !app.is_empty()),
    );
    NotificationWatchOptions {
        enabled: config.introspection,
        capacity: DEFAULT_CAPACITY,
        ttl: config.ttl(),
        excluded_apps,
        bus_address: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_shell::notification_watch::{NotificationEvent, NotifyFields};

    /// Monitor falso con eventos fijos.
    struct FakeWatcher {
        events: Vec<NotificationEvent>,
        shutdowns: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl NotificationWatcher for FakeWatcher {
        fn is_available(&self) -> bool {
            true
        }
        fn query_latest(&self) -> Option<NotificationEvent> {
            self.events.first().cloned()
        }
        fn query_all(&self) -> Vec<NotificationEvent> {
            self.events.clone()
        }
        fn query_by_app(&self, app: &str) -> Vec<NotificationEvent> {
            self.events
                .iter()
                .filter(|event| event.matches_app(app))
                .cloned()
                .collect()
        }
        fn shutdown(&mut self) {
            self.shutdowns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn describe(&self) -> String {
            "falso".into()
        }
    }

    fn event(app: &str, summary: &str) -> NotificationEvent {
        NotifyFields {
            app_name: app.into(),
            summary: summary.into(),
            ..NotifyFields::default()
        }
        .into_event(None, Instant::now())
    }

    #[test]
    fn answers_each_query_with_the_shell_templates() {
        let shutdowns = std::sync::Arc::default();
        let notifications = DesktopNotifications::new(Box::new(FakeWatcher {
            events: vec![
                event("Spectacle", "Captura guardada"),
                event("Discord", "Hola"),
            ],
            shutdowns: std::sync::Arc::clone(&shutdowns),
        }));
        assert!(notifications.is_available());

        let latest = notifications.answer(&NotificationQuery::Latest).unwrap();
        assert!(latest.starts_with("La última notificación"), "{latest}");
        assert!(latest.contains("«Captura guardada»"));

        let all = notifications.answer(&NotificationQuery::All).unwrap();
        assert!(all.starts_with("Llegaron 2 notificaciones"), "{all}");

        let discord = notifications
            .answer(&NotificationQuery::ByApp("discord".into()))
            .unwrap();
        assert!(discord.contains("«Hola»"), "{discord}");
        assert_eq!(
            notifications.answer(&NotificationQuery::ByApp("slack".into())),
            None
        );

        notifications.shutdown();
        assert_eq!(shutdowns.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn disabled_config_yields_an_unavailable_source() {
        let config = NotificationsConfig {
            introspection: false,
            ..NotificationsConfig::default()
        };
        let notifications = DesktopNotifications::start(&config);
        assert!(!notifications.is_available());
        assert_eq!(notifications.answer(&NotificationQuery::Latest), None);
        notifications.shutdown();
    }

    #[test]
    fn user_exclusions_extend_the_fixed_list() {
        let config = NotificationsConfig {
            excluded_apps: vec![" signal ".into(), "".into()],
            ttl_minutes: 5,
            ..NotificationsConfig::default()
        };
        let opts = watch_options(&config);
        assert!(opts.excluded_apps.iter().any(|app| app == "keepassxc"));
        assert!(opts.excluded_apps.iter().any(|app| app == "signal"));
        assert!(!opts.excluded_apps.iter().any(String::is_empty));
        assert_eq!(opts.ttl, std::time::Duration::from_secs(300));
        assert!(opts.enabled);
    }
}
