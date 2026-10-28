//! Persistencia en disco de las sub-sesiones (HU-62).
//!
//! Cada sub-sesión vive en su propio directorio:
//!
//! ```text
//! <root>/<id>/meta.json         estado y metadata (sin la transcripción)
//! <root>/<id>/transcript.jsonl  un turno por línea
//! <root>/<id>/detail.md         el último detalle en markdown
//! ```
//!
//! `<root>` es `state_dir()/sessions` en producción. Los directorios se crean
//! con permisos `0700` y los archivos con `0600` en Unix: la transcripción
//! puede contener datos del usuario (correos, PRs, rutas) y no debe ser
//! legible por otros usuarios del equipo.
//!
//! La transcripción se guarda **fuera** de `meta.json` (JSONL aparte) para que
//! escribir el estado en cada transición no reescriba todo el historial.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tasks::TaskOrigin;

use super::{SessionId, SessionProfile, SessionStatus, SessionTurn, SubSession};

/// Nombre del archivo de estado.
pub const META_FILE: &str = "meta.json";

/// Nombre del archivo de transcripción (un [`SessionTurn`] JSON por línea).
pub const TRANSCRIPT_FILE: &str = "transcript.jsonl";

/// Nombre del archivo de detalle.
pub const DETAIL_FILE: &str = "detail.md";

/// Tope del detalle en disco; lo que exceda se trunca con marca.
const DETAIL_MAX_BYTES: usize = 1 << 20;

/// Estado serializable de una sub-sesión, sin la transcripción completa.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub title: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub profile: SessionProfile,
    pub status: SessionStatus,
    pub origin: TaskOrigin,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub last_summary: Option<String>,
    #[serde(default)]
    pub progress: Option<String>,
    #[serde(default)]
    pub pending_question: Option<String>,
    #[serde(default)]
    pub detail_path: Option<PathBuf>,
    #[serde(default)]
    pub unread: bool,
}

impl SessionMeta {
    /// Proyecta una sub-sesión a su metadata (descarta la transcripción).
    pub fn from_session(session: &SubSession) -> Self {
        Self {
            id: session.id,
            title: session.title.clone(),
            aliases: session.aliases.clone(),
            profile: session.profile,
            status: session.status,
            origin: session.origin,
            created_at_ms: session.created_at_ms,
            updated_at_ms: session.updated_at_ms,
            last_summary: session.last_summary.clone(),
            progress: session.progress.clone(),
            pending_question: session.pending_question.clone(),
            detail_path: session.detail_path.clone(),
            unread: session.unread,
        }
    }

    /// Reconstruye la sub-sesión con la transcripción leída del JSONL.
    pub fn into_session(self, transcript: Vec<SessionTurn>) -> SubSession {
        SubSession {
            id: self.id,
            title: self.title,
            aliases: self.aliases,
            profile: self.profile,
            status: self.status,
            origin: self.origin,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            transcript,
            last_summary: self.last_summary,
            progress: self.progress,
            pending_question: self.pending_question,
            detail_path: self.detail_path,
            unread: self.unread,
        }
    }
}

/// Almacén en disco de las sub-sesiones.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    /// Almacén cuya raíz es `root` (un directorio por sub-sesión dentro).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Raíz del almacén.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directorio de una sub-sesión (`<root>/<id>`).
    pub fn session_dir(&self, id: SessionId) -> PathBuf {
        self.root.join(id.to_string())
    }

    /// Ruta del `meta.json` de una sub-sesión.
    pub fn meta_path(&self, id: SessionId) -> PathBuf {
        self.session_dir(id).join(META_FILE)
    }

    /// Ruta del `transcript.jsonl` de una sub-sesión.
    pub fn transcript_path(&self, id: SessionId) -> PathBuf {
        self.session_dir(id).join(TRANSCRIPT_FILE)
    }

    /// Ruta del `detail.md` de una sub-sesión.
    pub fn detail_path(&self, id: SessionId) -> PathBuf {
        self.session_dir(id).join(DETAIL_FILE)
    }

    /// Crea la raíz del almacén con permisos `0700` si no existe.
    pub fn create_root(&self) -> io::Result<()> {
        create_private_dir(&self.root)
    }

    /// Crea el directorio de una sub-sesión con permisos `0700`.
    pub fn ensure_dir(&self, id: SessionId) -> io::Result<()> {
        create_private_dir(&self.session_dir(id))
    }

    /// Escribe el `meta.json` de la sub-sesión.
    pub fn write_meta(&self, session: &SubSession) -> io::Result<()> {
        self.ensure_dir(session.id)?;
        let meta = SessionMeta::from_session(session);
        let json = serde_json::to_vec_pretty(&meta)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private(&self.meta_path(session.id), &json)
    }

    /// Lee el `meta.json`, o `None` si la sub-sesión no existe.
    pub fn read_meta(&self, id: SessionId) -> io::Result<Option<SessionMeta>> {
        let path = self.meta_path(id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let meta = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(meta))
    }

    /// Añade un turno al final de la transcripción.
    pub fn append_turn(&self, id: SessionId, turn: &SessionTurn) -> io::Result<()> {
        self.ensure_dir(id)?;
        let mut line =
            serde_json::to_vec(turn).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push(b'\n');
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(self.transcript_path(id))?;
        file.write_all(&line)?;
        file.flush()
    }

    /// Reescribe la transcripción completa (rehidratación y compactado).
    pub fn write_transcript(&self, id: SessionId, turns: &[SessionTurn]) -> io::Result<()> {
        self.ensure_dir(id)?;
        let mut body = Vec::new();
        for turn in turns {
            let line = serde_json::to_vec(turn)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            body.extend_from_slice(&line);
            body.push(b'\n');
        }
        write_private(&self.transcript_path(id), &body)
    }

    /// Lee la transcripción; las líneas ilegibles se descartan con un aviso, no
    /// tumban la rehidratación del resto.
    pub fn read_transcript(&self, id: SessionId) -> io::Result<Vec<SessionTurn>> {
        let path = self.transcript_path(id);
        let body = match fs::read_to_string(&path) {
            Ok(body) => body,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut turns = Vec::new();
        for (index, line) in body.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionTurn>(line) {
                Ok(turn) => turns.push(turn),
                Err(e) => eprintln!(
                    "[sesiones] #{id}: turno {} ilegible en {TRANSCRIPT_FILE}: {e}",
                    index + 1
                ),
            }
        }
        Ok(turns)
    }

    /// Escribe el detalle en markdown (sobrescribe el del turno anterior).
    pub fn write_detail(&self, id: SessionId, detail: &str) -> io::Result<PathBuf> {
        self.ensure_dir(id)?;
        let path = self.detail_path(id);
        let body = if detail.len() > DETAIL_MAX_BYTES {
            let mut cut = DETAIL_MAX_BYTES;
            while !detail.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}\n\n[truncado]\n", &detail[..cut])
        } else {
            detail.to_string()
        };
        write_private(&path, body.as_bytes())?;
        Ok(path)
    }

    /// Lee el detalle si existe.
    pub fn read_detail(&self, id: SessionId) -> io::Result<Option<String>> {
        match fs::read_to_string(self.detail_path(id)) {
            Ok(detail) => Ok(Some(detail)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Ids de las sub-sesiones guardadas, en orden ascendente.
    ///
    /// Ignora cualquier entrada que no sea un directorio con nombre numérico.
    pub fn list_ids(&self) -> io::Result<Vec<SessionId>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut ids: Vec<SessionId> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().to_str()?.parse::<SessionId>().ok())
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// Carga una sub-sesión completa (metadata + transcripción).
    pub fn load(&self, id: SessionId) -> io::Result<Option<SubSession>> {
        let Some(meta) = self.read_meta(id)? else {
            return Ok(None);
        };
        let transcript = self.read_transcript(id)?;
        Ok(Some(meta.into_session(transcript)))
    }

    /// Carga todas las sub-sesiones guardadas, en orden de id.
    ///
    /// Una sub-sesión ilegible se descarta con un aviso por `stderr`: no
    /// impide rehidratar las demás.
    pub fn load_all(&self) -> io::Result<Vec<SubSession>> {
        let mut sessions = Vec::new();
        for id in self.list_ids()? {
            match self.load(id) {
                Ok(Some(session)) => sessions.push(session),
                Ok(None) => {}
                Err(e) => eprintln!("[sesiones] #{id}: no se pudo restaurar: {e}"),
            }
        }
        Ok(sessions)
    }

    /// Borra del disco una sub-sesión (cierre definitivo).
    pub fn remove(&self, id: SessionId) -> io::Result<()> {
        match fs::remove_dir_all(self.session_dir(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Crea un directorio (y sus padres) con permisos `0700` en Unix.
fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Escribe un archivo completo con permisos `0600` en Unix.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::TurnRole;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jarvis-sessions-store-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn sample(id: SessionId) -> SubSession {
        SubSession {
            id,
            title: "revisión de PRs de Felipe".into(),
            aliases: vec!["prs".into(), "felipe".into()],
            profile: SessionProfile::ReadOnly,
            status: SessionStatus::Idle,
            origin: TaskOrigin::Voice,
            created_at_ms: 10,
            updated_at_ms: 20,
            transcript: Vec::new(),
            last_summary: Some("Dos PR abiertos.".into()),
            progress: None,
            pending_question: None,
            detail_path: None,
            unread: true,
        }
    }

    fn turn(role: TurnRole, text: &str) -> SessionTurn {
        SessionTurn {
            role,
            text: text.to_string(),
            at_ms: 7,
        }
    }

    #[test]
    fn meta_roundtrip_preserves_the_state_without_the_transcript() {
        let root = temp_root("meta");
        let store = SessionStore::new(&root);
        let mut session = sample(3);
        session.transcript = vec![turn(TurnRole::User, "hola")];
        store.write_meta(&session).unwrap();

        let raw = fs::read_to_string(store.meta_path(3)).unwrap();
        assert!(!raw.contains("hola"), "la transcripción no va en meta.json");

        let loaded = store.load(3).unwrap().expect("sesión guardada");
        assert_eq!(loaded.id, 3);
        assert_eq!(loaded.title, session.title);
        assert_eq!(loaded.profile, SessionProfile::ReadOnly);
        assert_eq!(loaded.status, SessionStatus::Idle);
        assert_eq!(
            loaded.aliases,
            vec!["prs".to_string(), "felipe".to_string()]
        );
        assert_eq!(loaded.last_summary.as_deref(), Some("Dos PR abiertos."));
        assert!(loaded.unread);
        assert!(loaded.transcript.is_empty(), "el meta no lleva historial");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn transcript_is_append_only_jsonl_and_survives_reload() {
        let root = temp_root("transcript");
        let store = SessionStore::new(&root);
        store
            .append_turn(1, &turn(TurnRole::User, "revisa las PR"))
            .unwrap();
        store
            .append_turn(1, &turn(TurnRole::Agent, "dos abiertas"))
            .unwrap();
        store
            .append_turn(1, &turn(TurnRole::System, "nota interna"))
            .unwrap();

        let body = fs::read_to_string(store.transcript_path(1)).unwrap();
        assert_eq!(body.lines().count(), 3);
        let turns = store.read_transcript(1).unwrap();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].role, TurnRole::User);
        assert_eq!(turns[2].text, "nota interna");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_transcript_lines_are_skipped() {
        let root = temp_root("malformed");
        let store = SessionStore::new(&root);
        store.ensure_dir(2).unwrap();
        fs::write(
            store.transcript_path(2),
            "{\"role\":\"user\",\"text\":\"hola\",\"at_ms\":1}\nno es json\n",
        )
        .unwrap();
        let turns = store.read_transcript(2).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].text, "hola");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detail_is_overwritten_and_list_ids_ignores_garbage() {
        let root = temp_root("detail");
        let store = SessionStore::new(&root);
        let path = store.write_detail(4, "# primer turno").unwrap();
        assert_eq!(
            store.read_detail(4).unwrap().as_deref(),
            Some("# primer turno")
        );
        store.write_detail(4, "# segundo turno").unwrap();
        assert_eq!(path, store.detail_path(4));
        assert_eq!(
            store.read_detail(4).unwrap().as_deref(),
            Some("# segundo turno")
        );

        store.write_meta(&sample(4)).unwrap();
        store.write_meta(&sample(9)).unwrap();
        fs::create_dir_all(root.join("no-numerico")).unwrap();
        fs::write(root.join("suelto.txt"), "x").unwrap();
        assert_eq!(store.list_ids().unwrap(), vec![4, 9]);

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("perms");
        let store = SessionStore::new(&root);
        store.write_meta(&sample(1)).unwrap();
        store.append_turn(1, &turn(TurnRole::User, "hola")).unwrap();
        store.write_detail(1, "# detalle").unwrap();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&store.session_dir(1)), 0o700);
        assert_eq!(mode(&store.meta_path(1)), 0o600);
        assert_eq!(mode(&store.transcript_path(1)), 0o600);
        assert_eq!(mode(&store.detail_path(1)), 0o600);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_session_reads_as_none() {
        let root = temp_root("missing");
        let store = SessionStore::new(&root);
        assert!(store.read_meta(42).unwrap().is_none());
        assert!(store.load(42).unwrap().is_none());
        assert!(store.read_transcript(42).unwrap().is_empty());
        assert!(store.list_ids().unwrap().is_empty());
        let _ = fs::remove_dir_all(root);
    }
}
