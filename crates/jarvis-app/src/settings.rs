//! Cerebro según la configuración y formulario "CONFIGURACION" del menú.
//!
//! El formulario es un script Tkinter embebido en el binario (tema oscuro de
//! Fokkus) que se ejecuta con `python3 -I`: edita `config.toml` y, si se
//! escribe una API key, la entrega a `fokkus-jarvis --set-api-key` para que
//! vaya al llavero. Corre en su propio proceso: el render nunca espera.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use jarvis_audio::orchestrator::{NO_API_KEY_NOTICE, NO_CLI_NOTICE};
use jarvis_audio::{
    BrainError, BrainFactory, ClaudeClient, CliBrain, TokenStream, SUBAGENT_SYSTEM_PROMPT,
};
use jarvis_core::{BrainMode, JarvisConfig, SPHERE_OPACITY_RANGE, SPHERE_SCALE_RANGE};

/// Script del formulario (ver `assets/config_form.py`).
#[allow(dead_code)] // reemplazado por UnifiedSettings; se conserva (tests de defaults)
const FORM_SCRIPT: &str = include_str!("../assets/config_form.py");

/// Script del selector de voces (ver `assets/voice_form.py`).
#[allow(dead_code)] // reemplazado por UnifiedSettings; se conserva (tests de voces)
const VOICE_FORM_SCRIPT: &str = include_str!("../assets/voice_form.py");

/// Script del Panel Rápido (ver `assets/quick_panel.py`).
const QUICK_PANEL_SCRIPT: &str = include_str!("../assets/quick_panel.py");

/// Script de Ajustes Unificados (ver `assets/unified_settings.py`).
const UNIFIED_SETTINGS_SCRIPT: &str = include_str!("../assets/unified_settings.py");

/// Cerebro listo para el orquestador.
pub type Brain = Arc<dyn TokenStream + Send + Sync>;

/// Construye el cerebro configurado. Sin cerebro devuelve el aviso hablado.
pub fn build_brain(config: &JarvisConfig) -> Result<Brain, &'static str> {
    match config.brain_mode {
        BrainMode::Cli => {
            let command = config.effective_cli_command();
            match CliBrain::new(command).map(|brain| {
                brain.with_timeout(Duration::from_secs(u64::from(config.cli_timeout_secs())))
            }) {
                Ok(brain) if brain.is_available() => {
                    // Sesión persistente: el arranque en frío ocurre ahora y
                    // no en el primer turno hablado.
                    brain.warm_up();
                    let mode = if brain.is_persistent() {
                        "sesión persistente"
                    } else {
                        "un proceso por turno"
                    };
                    println!("[cerebro] modo CLI ({mode}): `{command}`.");
                    Ok(Arc::new(brain))
                }
                Ok(brain) => {
                    eprintln!(
                        "[cerebro] modo CLI: no se encontró `{}` en el PATH; configúralo en el menú.",
                        brain.program()
                    );
                    Err(NO_CLI_NOTICE)
                }
                Err(e) => {
                    eprintln!("[cerebro] modo CLI: {e}");
                    Err(NO_CLI_NOTICE)
                }
            }
        }
        BrainMode::Api => match jarvis_audio::resolve_api_key(None) {
            Ok(key) => {
                let mut client =
                    ClaudeClient::with_api_key(key).with_base_url(&config.api_base_url);
                if !config.api_model.trim().is_empty() {
                    client = client.with_model(config.api_model.trim());
                }
                println!(
                    "[cerebro] modo API: modelo {} en {}; la key se valida en la primera consulta.",
                    client.model(),
                    client.endpoint()
                );
                Ok(Arc::new(client))
            }
            Err(e) => {
                eprintln!(
                    "[cerebro] modo API sin API key ({e}). Guárdala desde CONFIGURACION o con: fokkus-jarvis --set-api-key"
                );
                Err(NO_API_KEY_NOTICE)
            }
        },
    }
}

/// Fábrica de cerebros para las tareas de fondo (HU-37): misma lógica que
/// [`build_brain`] pero cada llamada crea una instancia **nueva** (historial,
/// cancelación y timeout propios). `apply_config` actualiza la configuración;
/// las tareas en curso terminan con el cerebro con que empezaron.
pub struct TaskBrainFactory {
    config: RwLock<JarvisConfig>,
}

impl TaskBrainFactory {
    pub fn new(config: &JarvisConfig) -> Self {
        Self {
            config: RwLock::new(config.clone()),
        }
    }

    /// Las tareas nuevas usarán esta configuración.
    pub fn update(&self, config: &JarvisConfig) {
        *self.config.write().unwrap_or_else(PoisonError::into_inner) = config.clone();
    }
}

impl BrainFactory for TaskBrainFactory {
    fn spawn(&self, timeout: Duration) -> Result<Brain, BrainError> {
        let config = self
            .config
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        build_task_brain(&config, timeout)
    }

    /// Las tareas en CLI lanzan un proceso por llamada (arranque en frío de
    /// varios segundos): no caben en presupuestos cortos como el del
    /// planificador de turnos.
    fn answers_within(&self, budget: Duration) -> bool {
        let config = self.config.read().unwrap_or_else(PoisonError::into_inner);
        config.brain_mode == BrainMode::Api || budget >= CLI_COLD_START
    }
}

/// Arranque en frío medido de `agy -p` hasta el primer token (5,3–9 s).
const CLI_COLD_START: Duration = Duration::from_secs(6);

/// Cerebro aislado para una tarea de fondo, sin avisos por consola.
pub fn build_task_brain(config: &JarvisConfig, timeout: Duration) -> Result<Brain, BrainError> {
    match config.brain_mode {
        BrainMode::Cli => {
            // HU-60: el subagente no recibe la persona de voz y exporta
            // `JARVIS_SUBSESSION_ID` (antirrecursión de `dev run`).
            let brain = CliBrain::new(config.effective_cli_command())?
                .with_timeout(timeout)
                .with_worker_mode(true);
            if !brain.is_available() {
                return Err(BrainError::Cli(format!(
                    "no se encontró `{}` en el PATH",
                    brain.program()
                )));
            }
            Ok(Arc::new(brain))
        }
        BrainMode::Api => {
            let key = jarvis_audio::resolve_api_key(None)?;
            let mut client = ClaudeClient::with_api_key(key)
                .with_base_url(&config.api_base_url)
                .with_system_prompt(SUBAGENT_SYSTEM_PROMPT);
            if !config.api_model.trim().is_empty() {
                client = client.with_model(config.api_model.trim());
            }
            Ok(Arc::new(client))
        }
    }
}

/// Resultado del formulario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormOutcome {
    /// El usuario guardó: hay que recargar la configuración.
    Saved,
    /// Cerrado sin cambios.
    Cancelled,
    /// El usuario pidió volver a la posición inicial de la esfera.
    ResetPosition,
}

/// Formulario abierto (un proceso a la vez).
#[allow(dead_code)] // legado: el menú abre UnifiedSettings
pub struct ConfigForm {
    rx: Receiver<FormOutcome>,
}

#[allow(dead_code)]
impl ConfigForm {
    /// Abre el formulario sobre `config_path`. Si no hay `python3`/Tkinter,
    /// abre el TOML con el editor del escritorio (`xdg-open`).
    pub fn open(config_path: &Path) -> Option<Self> {
        if let Err(e) = JarvisConfig::ensure_file(config_path) {
            eprintln!("[config] {e}");
        }
        let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fokkus-jarvis"));
        let child = Command::new("python3")
            .arg("-I")
            .arg("-c")
            .arg(FORM_SCRIPT)
            .arg(config_path)
            .arg(&binary)
            .stdin(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) => {
                eprintln!("[config] no se pudo abrir el formulario ({e}); abriendo el archivo.");
                open_in_editor(config_path);
                return None;
            }
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let path = config_path.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("jarvis-config".to_string())
            .spawn(move || {
                let outcome = match child.wait() {
                    Ok(status) if status.success() => FormOutcome::Saved,
                    Ok(status) if status.code() == Some(1) => FormOutcome::Cancelled,
                    Ok(status) => {
                        // Sin pantalla o sin Tkinter: el usuario aún puede editar.
                        eprintln!("[config] el formulario falló ({status}); abriendo el archivo.");
                        open_in_editor(&path);
                        FormOutcome::Cancelled
                    }
                    Err(e) => {
                        eprintln!("[config] el formulario falló: {e}");
                        FormOutcome::Cancelled
                    }
                };
                let _ = tx.send(outcome);
            });
        if let Err(e) = spawned {
            eprintln!("[config] no se pudo vigilar el formulario: {e}");
            return None;
        }
        println!("[config] formulario de configuración abierto.");
        Some(Self { rx })
    }

    /// `Some` cuando el formulario se cerró.
    pub fn poll(&self) -> Option<FormOutcome> {
        match self.rx.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(FormOutcome::Cancelled),
        }
    }
}

/// Selector de voces abierto (un proceso a la vez).
#[allow(dead_code)] // legado: el menú abre UnifiedSettings
pub struct VoiceForm {
    rx: Receiver<FormOutcome>,
}

#[allow(dead_code)]
impl VoiceForm {
    /// Abre el selector de voces sobre `config_path`.
    ///
    /// Igual que [`ConfigForm`]: corre en su propio proceso `python3 -I` para
    /// no bloquear el render. Si falta Tkinter o el binario `edge-tts`, el
    /// formulario degrada a editar el TOML con `xdg-open`.
    pub fn open(config_path: &Path) -> Option<Self> {
        if let Err(e) = JarvisConfig::ensure_file(config_path) {
            eprintln!("[config] {e}");
        }
        let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fokkus-jarvis"));
        let mut child = match Command::new("python3")
            .arg("-I")
            .arg("-c")
            .arg(VOICE_FORM_SCRIPT)
            .arg(config_path)
            .arg(&binary)
            .stdin(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                eprintln!("[voz] no se pudo abrir el selector ({e}); abriendo el archivo.");
                open_in_editor(config_path);
                return None;
            }
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let path = config_path.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("jarvis-voice-form".to_string())
            .spawn(move || {
                let outcome = match child.wait() {
                    Ok(status) if status.success() => FormOutcome::Saved,
                    Ok(status) if status.code() == Some(1) => FormOutcome::Cancelled,
                    Ok(status) => {
                        eprintln!("[voz] el selector falló ({status}); abriendo el archivo.");
                        open_in_editor(&path);
                        FormOutcome::Cancelled
                    }
                    Err(e) => {
                        eprintln!("[voz] el selector falló: {e}");
                        FormOutcome::Cancelled
                    }
                };
                let _ = tx.send(outcome);
            });
        if let Err(e) = spawned {
            eprintln!("[voz] no se pudo vigilar el selector: {e}");
            return None;
        }
        println!("[voz] selector de voces abierto.");
        Some(Self { rx })
    }

    /// `Some` cuando el selector se cerró.
    pub fn poll(&self) -> Option<FormOutcome> {
        match self.rx.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(FormOutcome::Cancelled),
        }
    }
}

/// Resultado del Panel Rápido.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelOutcome {
    /// El usuario guardó: hay que recargar la configuración.
    Saved,
    /// Cerrado sin cambios (o error sin pantalla).
    Cancelled,
    /// El usuario pidió volver a la posición inicial de la esfera.
    ResetPosition,
    /// El usuario pidió abrir Ajustes Unificados en una pestaña concreta.
    OpenSettings(Option<String>),
}

/// Panel Rápido abierto (un proceso a la vez).
pub struct QuickPanel {
    rx: Receiver<PanelOutcome>,
}

impl QuickPanel {
    /// Abre el panel sobre `config_path` pasándole el PID del proceso para que
    /// pueda enviar SIGUSR1/SIGTERM. `pos` ubica el panel en la coordenada
    /// exacta del clic y `muted` evita la consulta síncrona inicial al socket.
    /// Si no hay `python3`/PyQt6, degrada a editar el TOML con `xdg-open`.
    pub fn open(config_path: &Path, pos: Option<(i32, i32)>, muted: bool) -> Option<Self> {
        if let Err(e) = JarvisConfig::ensure_file(config_path) {
            eprintln!("[config] {e}");
        }
        let pid = std::process::id();
        let mut command = Command::new("python3");
        command
            .arg("-I")
            .arg("-c")
            .arg(QUICK_PANEL_SCRIPT)
            .arg(config_path)
            .arg("--pid")
            .arg(pid.to_string());
        if let Some((x, y)) = pos {
            command.arg("--pos").arg(x.to_string()).arg(y.to_string());
        }
        command.arg("--muted").arg(if muted { "1" } else { "0" });
        command.env("JARVIS_VERSION", env!("CARGO_PKG_VERSION"));
        // En KWin Wayland una ventana Qt nativa no puede ubicarse junto al
        // cursor (y puede quedar en mosaico): con Xwayland flota libre.
        if prefers_xcb(
            std::env::var_os("DISPLAY").as_deref(),
            std::env::var_os("QT_QPA_PLATFORM").as_deref(),
        ) {
            command.env("QT_QPA_PLATFORM", "xcb");
        }
        let child = command.stdin(Stdio::null()).spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) => {
                eprintln!("[panel] no se pudo abrir el panel ({e}); abriendo el archivo.");
                open_in_editor(config_path);
                return None;
            }
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let path = config_path.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("jarvis-quick-panel".to_string())
            .spawn(move || {
                let outcome = match child.wait() {
                    Ok(status) => match status.code() {
                        Some(0) => PanelOutcome::Saved,
                        Some(1 | 2) => PanelOutcome::Cancelled,
                        Some(3) => PanelOutcome::ResetPosition,
                        Some(10) => PanelOutcome::OpenSettings(Some("general".to_string())),
                        Some(11) => PanelOutcome::OpenSettings(Some("voz".to_string())),
                        Some(12) => PanelOutcome::OpenSettings(Some("cerebro".to_string())),
                        Some(13) => PanelOutcome::OpenSettings(Some("escucha".to_string())),
                        _ => {
                            eprintln!("[panel] el panel falló ({status}); abriendo el archivo.");
                            open_in_editor(&path);
                            PanelOutcome::Cancelled
                        }
                    },
                    Err(e) => {
                        eprintln!("[panel] el panel falló: {e}");
                        PanelOutcome::Cancelled
                    }
                };
                let _ = tx.send(outcome);
            });
        if let Err(e) = spawned {
            eprintln!("[panel] no se pudo vigilar el panel: {e}");
            return None;
        }
        println!("[panel] panel rápido abierto.");
        Some(Self { rx })
    }

    /// `Some` cuando el panel se cerró.
    pub fn poll(&self) -> Option<PanelOutcome> {
        match self.rx.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(PanelOutcome::Cancelled),
        }
    }
}

/// Cambio de vista previa emitido por Ajustes Unificados (HU-33/HU-34).
///
/// Los valores ya vienen saturados a su rango válido.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPreview {
    /// Escala de la esfera en % (50–200).
    SphereScale(u16),
    /// Opacidad de la esfera en % (20–100).
    SphereOpacity(u8),
}

/// Capacidad del canal de vista previa: solo importa el último valor.
const PREVIEW_CAPACITY: usize = 16;

/// Interpreta una línea de stdout del script de ajustes.
///
/// Acepta `{"preview":{"sphere_scale_pct":135}}` y
/// `{"preview":{"sphere_opacity_pct":60}}`; cualquier otra línea (prints de
/// PyQt, JSON sin `preview`, tipos incorrectos) se ignora con `None`.
pub fn parse_preview_line(line: &str) -> Option<SettingsPreview> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let preview = value.get("preview")?.as_object()?;
    if let Some(pct) = preview.get("sphere_scale_pct").and_then(|v| v.as_i64()) {
        let (lo, hi) = (*SPHERE_SCALE_RANGE.start(), *SPHERE_SCALE_RANGE.end());
        return Some(SettingsPreview::SphereScale(
            pct.clamp(i64::from(lo), i64::from(hi)) as u16,
        ));
    }
    if let Some(pct) = preview.get("sphere_opacity_pct").and_then(|v| v.as_i64()) {
        let (lo, hi) = (*SPHERE_OPACITY_RANGE.start(), *SPHERE_OPACITY_RANGE.end());
        return Some(SettingsPreview::SphereOpacity(
            pct.clamp(i64::from(lo), i64::from(hi)) as u8,
        ));
    }
    None
}

/// Lee líneas de vista previa de `reader` hasta EOF y las envía por `tx`.
///
/// Si el canal está lleno descarta la más vieja: el bucle solo aplica el
/// último valor. Termina en EOF o si el receptor desapareció.
fn forward_previews(
    reader: impl Read,
    tx: &Sender<SettingsPreview>,
    rx: &Receiver<SettingsPreview>,
) {
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        let Some(preview) = parse_preview_line(&line) else {
            continue;
        };
        let mut pending = preview;
        loop {
            match tx.try_send(pending) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) => {
                    let _ = rx.try_recv();
                    pending = back;
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

/// Ajustes Unificados abiertos (un proceso a la vez).
pub struct UnifiedSettings {
    rx: Receiver<FormOutcome>,
    previews: Receiver<SettingsPreview>,
}

impl UnifiedSettings {
    /// Abre Ajustes Unificados sobre `config_path` en la pestaña indicada.
    ///
    /// Igual que los demás formularios: proceso `python3 -I` propio para no
    /// bloquear el render. Si no hay PyQt6/pantalla, degrada a `xdg-open`.
    pub fn open(config_path: &Path, tab: Option<&str>) -> Option<Self> {
        if let Err(e) = JarvisConfig::ensure_file(config_path) {
            eprintln!("[config] {e}");
        }
        let pid = std::process::id();
        let mut command = Command::new("python3");
        command
            .arg("-I")
            .arg("-c")
            .arg(UNIFIED_SETTINGS_SCRIPT)
            .arg(config_path)
            .arg("--pid")
            .arg(pid.to_string());
        if let Some(tab) = tab {
            command.arg("--tab").arg(tab);
        }
        // Igual que QuickPanel: con Xwayland la ventana flota libre sobre el
        // escritorio activo en lugar de quedar en mosaico.
        if prefers_xcb(
            std::env::var_os("DISPLAY").as_deref(),
            std::env::var_os("QT_QPA_PLATFORM").as_deref(),
        ) {
            command.env("QT_QPA_PLATFORM", "xcb");
        }
        // stdout es el canal de vista previa en vivo (HU-33/HU-34).
        let mut child = match command.stdin(Stdio::null()).stdout(Stdio::piped()).spawn() {
            Ok(child) => child,
            Err(e) => {
                eprintln!("[config] no se pudo abrir los ajustes ({e}); abriendo el archivo.");
                open_in_editor(config_path);
                return None;
            }
        };
        let (preview_tx, previews) = crossbeam_channel::bounded(PREVIEW_CAPACITY);
        if let Some(stdout) = child.stdout.take() {
            let drain = previews.clone();
            let spawned = std::thread::Builder::new()
                .name("jarvis-settings-preview".to_string())
                .spawn(move || forward_previews(stdout, &preview_tx, &drain));
            if let Err(e) = spawned {
                // Sin lector los ajustes siguen funcionando, sin vista previa.
                eprintln!("[config] sin vista previa en vivo: {e}");
            }
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let path = config_path.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("jarvis-unified-settings".to_string())
            .spawn(move || {
                let outcome = match child.wait() {
                    Ok(status) if status.success() => FormOutcome::Saved,
                    Ok(status) if status.code() == Some(1) => FormOutcome::Cancelled,
                    Ok(status) if status.code() == Some(3) => FormOutcome::ResetPosition,
                    Ok(status) => {
                        eprintln!("[config] los ajustes fallaron ({status}); abriendo el archivo.");
                        open_in_editor(&path);
                        FormOutcome::Cancelled
                    }
                    Err(e) => {
                        eprintln!("[config] los ajustes fallaron: {e}");
                        FormOutcome::Cancelled
                    }
                };
                let _ = tx.send(outcome);
            });
        if let Err(e) = spawned {
            eprintln!("[config] no se pudo vigilar los ajustes: {e}");
            return None;
        }
        println!("[config] ajustes unificados abiertos.");
        Some(Self { rx, previews })
    }

    /// `Some` cuando los ajustes se cerraron.
    pub fn poll(&self) -> Option<FormOutcome> {
        match self.rx.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(FormOutcome::Cancelled),
        }
    }

    /// Siguiente cambio de vista previa pendiente, si lo hay (no bloquea).
    ///
    /// El llamador drena hasta `None` y se queda con el último de cada tipo.
    pub fn poll_preview(&self) -> Option<SettingsPreview> {
        self.previews.try_recv().ok()
    }
}

/// `true` si hay Xwayland (`DISPLAY`) y nadie fijó `QT_QPA_PLATFORM`.
fn prefers_xcb(display: Option<&std::ffi::OsStr>, platform: Option<&std::ffi::OsStr>) -> bool {
    display.is_some_and(|d| !d.is_empty()) && platform.is_none_or(|p| p.is_empty())
}

fn open_in_editor(path: &Path) {
    let opened = Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = opened {
        eprintln!(
            "[config] edita a mano {} (xdg-open no disponible: {e}).",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_mode_with_missing_program_has_no_brain() {
        let config = JarvisConfig::default().with_cli_command("no-existe-jarvis-cli -p");
        assert_eq!(build_brain(&config).err(), Some(NO_CLI_NOTICE));
    }

    #[test]
    fn task_factory_builds_independent_brains_and_follows_updates() {
        let factory = TaskBrainFactory::new(&JarvisConfig::default().with_cli_command("echo"));
        let a = factory.spawn(Duration::from_secs(5)).expect("cerebro");
        let b = factory.spawn(Duration::from_secs(5)).expect("cerebro");
        assert!(!Arc::ptr_eq(&a, &b), "una instancia por tarea");

        factory.update(&JarvisConfig::default().with_cli_command("no-existe-jarvis-cli -p"));
        assert!(matches!(
            factory.spawn(Duration::from_secs(5)),
            Err(BrainError::Cli(_))
        ));
    }

    #[test]
    fn task_brain_runs_as_sub_session_worker_hu60() {
        let config = JarvisConfig::default().with_cli_command("echo");
        let brain = build_task_brain(&config, Duration::from_secs(5)).expect("cerebro");
        let prompt = brain
            .stream_tokens("revisa las PR de Felipe", &mut |_| {})
            .unwrap()
            .text;
        assert!(!prompt.contains("frase breve"), "{prompt}");
        assert!(!prompt.contains("fokkus-jarvis dev run"), "{prompt}");
        assert!(prompt.contains("subagente de fondo"), "{prompt}");

        let config = JarvisConfig::default()
            .with_cli_command("sh -c 'echo \"$JARVIS_SUBSESSION_ID\"' {prompt}");
        let brain = build_task_brain(&config, Duration::from_secs(5)).expect("cerebro");
        let marked = brain.stream_tokens("hola", &mut |_| {}).unwrap().text;
        assert!(
            !marked.trim().is_empty(),
            "el hijo debe ver la marca de sub-sesión"
        );
    }

    #[test]
    fn cli_mode_with_available_program_builds_a_brain() {
        let config = JarvisConfig::default().with_cli_command("echo");
        assert!(build_brain(&config).is_ok());
    }

    #[test]
    fn form_script_targets_the_same_defaults_as_the_core() {
        let defaults = JarvisConfig::default();
        assert!(FORM_SCRIPT.contains(&format!("\"cli_command\": \"{}\"", defaults.cli_command)));
        assert!(FORM_SCRIPT.contains("\"brain_mode\": \"cli\""));
        assert!(FORM_SCRIPT.contains("\"barge_in_enabled\": False"));
        assert!(FORM_SCRIPT.contains("--set-api-key"));
    }

    #[test]
    fn voice_form_script_lists_spanish_and_english_voices_and_saves_the_voice() {
        // Debe cubrir las tres categorías del selector y persistir la voz.
        for voice in [
            "es-CL-LorenzoNeural",
            "es-ES-ElviraNeural",
            "en-US-AriaNeural",
            "en-GB-SoniaNeural",
        ] {
            assert!(VOICE_FORM_SCRIPT.contains(voice), "falta {voice}");
        }
        assert!(VOICE_FORM_SCRIPT.contains("\"voice_id\""));
        assert!(VOICE_FORM_SCRIPT.contains("\"voice_lang\""));
        assert!(VOICE_FORM_SCRIPT.contains("edge-tts"));
    }

    #[test]
    fn quick_panel_script_matches_current_defaults_and_exit_codes() {
        assert!(QUICK_PANEL_SCRIPT
            .contains("\"cli_command\": \"agy --effort low --dangerously-skip-permissions -p\""));
        assert!(QUICK_PANEL_SCRIPT.contains("\"continuous_listening\": False"));
        assert!(QUICK_PANEL_SCRIPT.contains("\"barge_in_enabled\": False"));
        assert!(QUICK_PANEL_SCRIPT.contains("\"brain_mode\": \"cli\""));
        // Mapeo de códigos de salida hacia el runtime Rust.
        assert!(QUICK_PANEL_SCRIPT.contains("EXIT_OPEN_GENERAL = 10"));
        assert!(QUICK_PANEL_SCRIPT.contains("EXIT_OPEN_VOZ = 11"));
        assert!(QUICK_PANEL_SCRIPT.contains("EXIT_OPEN_CEREBRO = 12"));
        assert!(QUICK_PANEL_SCRIPT.contains("EXIT_OPEN_ESCUCHA = 13"));
    }

    #[test]
    fn quick_panel_uses_xwayland_only_when_platform_is_not_forced() {
        use std::ffi::OsStr;
        let display = Some(OsStr::new(":0"));
        assert!(prefers_xcb(display, None));
        assert!(prefers_xcb(display, Some(OsStr::new(""))));
        assert!(!prefers_xcb(display, Some(OsStr::new("wayland"))));
        assert!(!prefers_xcb(None, None));
        assert!(!prefers_xcb(Some(OsStr::new("")), None));
        // El script aplica la misma regla antes de importar PyQt6.
        assert!(QUICK_PANEL_SCRIPT.contains("os.environ[\"QT_QPA_PLATFORM\"] = \"xcb\""));
        assert!(QUICK_PANEL_SCRIPT.contains("SizeConstraint.SetFixedSize"));
    }

    #[test]
    fn unified_settings_script_matches_current_defaults_and_tabs() {
        assert!(UNIFIED_SETTINGS_SCRIPT
            .contains("\"cli_command\": \"agy --effort low --dangerously-skip-permissions -p\""));
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("\"continuous_listening\": False"));
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("\"barge_in_enabled\": False"));
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("\"brain_mode\": \"cli\""));
        for tab in ["cerebro", "voz", "escucha", "general"] {
            assert!(
                UNIFIED_SETTINGS_SCRIPT.contains(&format!("\"{tab}\"")),
                "falta {tab}"
            );
        }
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("--tab"));
    }

    #[test]
    fn preview_lines_are_parsed_and_saturated() {
        assert_eq!(
            parse_preview_line(r#"{"preview":{"sphere_scale_pct":135}}"#),
            Some(SettingsPreview::SphereScale(135))
        );
        assert_eq!(
            parse_preview_line(r#"  {"preview": {"sphere_opacity_pct": 60}}"#),
            Some(SettingsPreview::SphereOpacity(60))
        );
        // Fuera de rango: se satura (el piso de opacidad es inviolable).
        assert_eq!(
            parse_preview_line(r#"{"preview":{"sphere_scale_pct":900}}"#),
            Some(SettingsPreview::SphereScale(200))
        );
        assert_eq!(
            parse_preview_line(r#"{"preview":{"sphere_scale_pct":0}}"#),
            Some(SettingsPreview::SphereScale(50))
        );
        assert_eq!(
            parse_preview_line(r#"{"preview":{"sphere_opacity_pct":5}}"#),
            Some(SettingsPreview::SphereOpacity(20))
        );
        // Basura, JSON sin `preview` o con tipos incorrectos: se ignora.
        for line in [
            "",
            "qt.qpa.xcb: could not connect",
            "{no es json",
            r#"{"sphere_scale_pct":100}"#,
            r#"{"preview":{}}"#,
            r#"{"preview":{"sphere_scale_pct":"grande"}}"#,
            r#"{"preview":42}"#,
        ] {
            assert_eq!(parse_preview_line(line), None, "{line}");
        }
    }

    #[test]
    fn preview_reader_keeps_the_latest_values_when_full() {
        let (tx, rx) = crossbeam_channel::bounded(PREVIEW_CAPACITY);
        let mut input = String::from("ruido de PyQt\n");
        for pct in 50..=200u16 {
            input.push_str(&format!("{{\"preview\":{{\"sphere_scale_pct\":{pct}}}}}\n"));
        }
        input.push_str(r#"{"preview":{"sphere_opacity_pct":40}}"#);
        forward_previews(input.as_bytes(), &tx, &rx);

        let received: Vec<_> = rx.try_iter().collect();
        assert_eq!(received.len(), PREVIEW_CAPACITY);
        assert_eq!(received.last(), Some(&SettingsPreview::SphereOpacity(40)));
        assert!(received.contains(&SettingsPreview::SphereScale(200)));
    }

    #[test]
    fn unified_settings_script_emits_sphere_previews() {
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("\"sphere_scale_pct\": 100"));
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("\"sphere_opacity_pct\": 100"));
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("SPHERE_SCALE_RANGE = (50, 200, 5)"));
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("SPHERE_OPACITY_RANGE = (20, 100, 5)"));
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("PREVIEW_INTERVAL_MS = 33"));
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("{\"preview\": {key: int(value)}}"));
    }

    #[test]
    fn unified_settings_script_offers_fullscreen_behaviors() {
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("\"fullscreen_behavior\": \"dim\""));
        for (key, label) in [
            ("dim", "Atenuar al 25% (recomendado)"),
            ("hide", "Ocultar esfera"),
            ("ignore", "Ignorar (desactivado)"),
        ] {
            assert!(
                UNIFIED_SETTINGS_SCRIPT.contains(&format!("(\"{key}\", \"{label}\")")),
                "falta {key}"
            );
            // El valor debe ser uno de los que acepta `FullscreenBehavior`.
            let parsed: jarvis_core::FullscreenBehavior =
                serde_json::from_str(&format!("\"{key}\"")).unwrap();
            assert_eq!(
                serde_json::to_string(&parsed).unwrap(),
                format!("\"{key}\"")
            );
        }
        assert!(UNIFIED_SETTINGS_SCRIPT
            .contains("\"fullscreen_behavior\": self.fullscreen_combo.currentData()"));
    }
    #[test]
    fn unified_settings_script_toggles_the_hud() {
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("\"hud_enabled\": True"));
        assert!(UNIFIED_SETTINGS_SCRIPT.contains("Mostrar burbujas de conversación (HUD)"));
        assert!(
            UNIFIED_SETTINGS_SCRIPT.contains("\"hud_enabled\": bool(self.hud_check.isChecked())")
        );
        // La clave coincide con la del núcleo y su default.
        let path =
            std::env::temp_dir().join(format!("jarvis-hud-toggle-{}.toml", std::process::id()));
        std::fs::write(&path, "hud_enabled = false\n").unwrap();
        let config = jarvis_core::JarvisConfig::load_from(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(!config.hud_enabled());
        assert!(jarvis_core::JarvisConfig::default().hud_enabled());
    }
}
