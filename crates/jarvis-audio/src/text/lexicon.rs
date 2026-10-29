//! Corrector léxico/fonético determinista de transcripciones (HU-70).
//!
//! Whisper transcribe bien el español general pero deforma tecnicismos y
//! modismos: "cubernetis", "espectacle", "loren ypsum", "doquer". El texto
//! deformado llegaba igual al clasificador, al router, al cerebro y al HUD, así
//! que se corrige aquí, **antes** que cualquier otra heurística, con una tabla
//! embebida: sin red, sin LLM y sin asignaciones por token fuera de la salida.
//!
//! Reglas (CA-70.1, CA-70.2):
//!
//! - Se sustituyen **solo** los tramos que coinciden; el resto de la frase
//!   conserva mayúsculas, tildes, comas y signos.
//! - La comparación pliega tildes y mayúsculas, pero la salida usa la forma
//!   canónica de la tabla ("dóquer" → "Docker", "cachay" → "cachái").
//! - Coincidencia por **palabra completa**: "reporte" no coincide con "repo" ni
//!   "apio" con "api" (la regresión que arregló HU-59).
//! - **Frase más larga primero**: "git hub actions" → "GitHub Actions", no
//!   "GitHub actions".
//! - Un tramo escrito todo en mayúsculas se conserva gritado: "DOQUER" →
//!   "DOCKER".
//! - Una coma seguida de espacio **no** corta un tramo: "abla, normal" →
//!   "habla normal" (HU-74). "a,b", sin espacio, nunca casa.
//!
//! Este módulo **no** sustituye a [`crate::text::normalize`] (HU-59): aquel
//! normaliza **con pérdida** para comparar (minúsculas, sin tildes, en el
//! espacio de tokens); este conserva el texto porque su salida se habla, se
//! muestra en el HUD y se manda al LLM.
//!
//! Deliberadamente **no** incluye variantes de la palabra de activación
//! ("Jarvis"): el filtro de activación es una frontera de seguridad y este
//! módulo no la mueve. Por la misma razón un léxico de usuario no puede
//! introducir "Jarvis", "silencio" ni las órdenes de anulación (CA-70.5).
//!
//! Compromisos conocidos:
//!
//! - "plasma" se escribe siempre "Plasma" porque en este dominio casi siempre
//!   es KDE Plasma ("el plasma de la sangre" quedaría con mayúscula).
//! - "jira" se escribe "Jira" (la tela homónima es rarísima hablando).
//! - "germando" se corrige a "armando" y "volutiva" a "evolutiva" aunque las
//!   acepciones corrientes existan ("germinar"): en el historial de
//!   transcripciones nunca aparecieron y sí el nombre y el adjetivo.
//! - "como si tuvieras" se corrige a "como si estuvieras": en el historial
//!   whisper se comió el "es" pidiendo un estado de ánimo, y el costo es que
//!   "como si tuvieras hambre" también cambia.
//! - "po" se sustituye por "po" (identidad): está en la tabla para dar destino
//!   a "poh" y para fijar la forma canónica, no para cambiarla.
//! - "abla", "avla", "ablar", "ablando"… se corrigen a "habla", "hablar",
//!   "hablando": whisper se come la hache y ninguna de esas formas es español
//!   corriente. La coincidencia es por palabra completa, así que "ablandar",
//!   "ablandando" y "ablandador" no se tocan.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::text::normalize::{fold_accents, normalize};

/// Máximo de entradas que aporta el usuario desde `config.toml` (CA-70.5).
pub const MAX_USER_ENTRIES: usize = 500;

/// Longitud máxima, en caracteres, de una variante o de su forma canónica.
pub const MAX_ENTRY_CHARS: usize = 64;

/// Frases que un léxico de usuario **no** puede introducir (CA-70.5).
///
/// Si una entrada de usuario pudiera convertir una palabra cualquiera en
/// "silencio" (o en "Jarvis"), un `config.toml` malicioso —o uno sincronizado
/// desde otro equipo— dispararía órdenes con solo decir "hola".
pub const PROTECTED_PHRASES: &[&str] = &[
    "jarvis",
    "silencio",
    "modo silencio",
    "detente",
    "apagate",
    "standby",
    "kill switch",
    "para la automatizacion",
    "cancela todas las tareas",
    "cancela las tareas",
];

/// Diccionario base embebido: `(forma canónica, variantes)`.
///
/// Las variantes van plegadas (minúsculas y sin tildes) porque la comparación
/// se hace sobre el texto plegado; la forma canónica es la que se escribe.
pub const BASE_ENTRIES: &[(&str, &[&str])] = &[
    // ------------------------------------------------------- desarrollo
    ("Docker", &["docker", "doquer", "doker", "douker"]),
    (
        "Docker Compose",
        &["docker compose", "docker compus", "docker compous"],
    ),
    (
        "Kubernetes",
        &[
            "kubernetes",
            "cubernetis",
            "cubernetes",
            "kubernetis",
            "cubernets",
        ],
    ),
    (
        "kubectl",
        &["kubectl", "cube control", "kube control", "cubectl"],
    ),
    ("Helm", &["helm", "jelm"]),
    ("Rust", &["rust"]),
    ("cargo build", &["cargo build", "cargo bild"]),
    ("cargo test", &["cargo test", "cargo tes"]),
    (
        "cargo clippy",
        &["cargo clippy", "cargo clipi", "cargo clipy"],
    ),
    (
        "Cargo.toml",
        &["cargo toml", "cargo tom", "cargo punto toml"],
    ),
    (
        "GitHub",
        &["github", "git hub", "guithub", "guit hub", "yithub"],
    ),
    (
        "GitHub Actions",
        &[
            "github actions",
            "git hub actions",
            "guithub actions",
            "guit hub actions",
        ],
    ),
    ("GitLab", &["gitlab", "git lab", "yit lab"]),
    ("Git", &["git", "yit", "jit"]),
    (
        "PR",
        &[
            "pr",
            "prs",
            "pe erre",
            "pe erres",
            "pull request",
            "pull requests",
            "pul request",
        ],
    ),
    ("JSON", &["json", "yeison"]),
    ("YAML", &["yaml", "yamel", "yamil"]),
    ("API", &["api", "a pe i", "ape i"]),
    ("endpoint", &["endpoint", "end point"]),
    ("deploy", &["deploy", "deploi", "de ploy"]),
    ("commit", &["commit", "comit"]),
    ("branch", &["branch", "branche"]),
    ("merge", &["merge", "mergue"]),
    ("repo", &["repo", "repos"]),
    ("Python", &["python", "paiton", "payton"]),
    ("JavaScript", &["javascript", "java script", "yava script"]),
    ("TypeScript", &["typescript", "type script", "tai script"]),
    ("SQL", &["sql", "ese cu ele"]),
    (
        "PostgreSQL",
        &["postgresql", "postgres", "post gres", "postgre"],
    ),
    ("SQLite", &["sqlite", "sql lite", "ese cu lite"]),
    ("Linux", &["linux", "linex"]),
    ("Ubuntu", &["ubuntu", "ubunto", "ubundu"]),
    ("Markdown", &["markdown", "mark daun", "mark down"]),
    ("README", &["readme", "read me", "rid mi"]),
    ("changelog", &["changelog", "change log"]),
    ("benchmark", &["benchmark", "bench mark", "benkmark"]),
    ("frontend", &["frontend", "front end"]),
    ("backend", &["backend", "back end", "bac end"]),
    ("dashboard", &["dashboard", "dash board", "dachboard"]),
    ("webhook", &["webhook", "web juk", "gueb juk"]),
    ("Redis", &["redis", "redys"]),
    ("MongoDB", &["mongodb", "mongo db", "mongo de be"]),
    ("MySQL", &["mysql", "mai ese cu ele"]),
    ("Nginx", &["nginx", "enguinc", "en yinx"]),
    ("Slack", &["slack", "eslac"]),
    ("Gmail", &["gmail", "yi meil", "gimeil"]),
    (
        "VS Code",
        &["vs code", "vscode", "ve ese code", "vais code"],
    ),
    ("IntelliJ", &["intellij", "inteli yei"]),
    ("Figma", &["figma", "fig ma"]),
    ("Jira", &["jira"]),
    ("Trello", &["trello", "trelo"]),
    ("Neovim", &["neovim", "neo vim"]),
    ("Ollama", &["ollama", "olama"]),
    ("Whisper", &["whisper", "wisper", "guisper"]),
    ("VPN", &["vpn", "u ve ene", "ve pe ene"]),
    ("SSH", &["ssh", "ese ese hache"]),
    ("HTTP", &["http", "hache te te pe"]),
    ("HTTPS", &["https", "hache te te pe ese"]),
    ("DNS", &["dns", "de ene ese"]),
    ("IP", &["ip", "i pe"]),
    ("ping", &["ping"]),
    (
        "Lorem Ipsum",
        &[
            "lorem ipsum",
            "loren ipsum",
            "lorem ipsun",
            "loren ypsum",
            "lorem ipsu",
            "loren ipsun",
        ],
    ),
    // ------------------------------------------- asistentes y modelos IA
    (
        "Antigravity",
        &[
            "antigravity",
            "antigradity",
            "nanti-gravity",
            "nanti gravity",
            "anti gravity",
        ],
    ),
    (
        "Claude",
        &["claude", "claud", "cloudd", "clod", "clode", "cloude"],
    ),
    ("DeepSeek", &["deepseek", "dip sik", "dipsek", "deep seek"]),
    // ------------------------------------------------------- escritorio
    ("KDE", &["kde", "ka de e", "k de e"]),
    (
        "KDE Plasma",
        &["kde plasma", "ka de e plasma", "k de e plasma"],
    ),
    ("Plasma", &["plasma"]),
    ("Wayland", &["wayland", "way land", "gueyland", "ueyland"]),
    (
        "Spectacle",
        &[
            "spectacle",
            "espectacle",
            "spectakle",
            "espectakle",
            "espectacul",
        ],
    ),
    ("Konsole", &["konsole", "konsola", "kon sol"]),
    ("Dolphin", &["dolphin", "dolfin"]),
    (
        "PipeWire",
        &["pipewire", "pipe wire", "paipuair", "paip uair"],
    ),
    ("PulseAudio", &["pulseaudio", "pulsaudio", "pulse audio"]),
    (
        "WireGuard",
        &["wireguard", "wire guard", "guir gard", "wire gard"],
    ),
    ("KWin", &["kwin", "k win", "ka win"]),
    ("D-Bus", &["d bus", "de bus", "dbus"]),
    ("systemd", &["systemd", "sistem de", "sistemdi"]),
    ("journalctl", &["journalctl", "journal ctl", "yournal ctl"]),
    ("Spotify", &["spotify", "espotifai", "espotify", "spotifai"]),
    // --------------------------------------------------- jerga chilena
    // "po" es identidad (ya se escribe igual): entra para que "poh" tenga a
    // dónde llegar y para fijar la forma canónica.
    ("po", &["po", "poh"]),
    ("cachái", &["cachai", "cachay", "kashai"]),
    ("al tiro", &["altiro", "al tiro", "al-tiro"]),
    ("al toque", &["al toque", "altoke", "al-toke"]),
    (
        "compadre",
        &[
            "compadre",
            "conpadre",
            "conpadres",
            "compadrito",
            "con padricos",
            "con padrico",
        ],
    ),
    ("pantallazo", &["pantallaso"]),
    ("bacán", &["bacan", "vakan"]),
    ("cuático", &["cuatico", "kuatico"]),
    ("harto", &["arto"]),
    ("weón", &["hueon", "gueon", "weon", "huevon"]),
    ("cahuín", &["caguin", "cahuin"]),
    // ------------------------------------ confusiones del habla chilena
    ("acento", &["asento"]),
    ("acentos", &["asentos"]),
    ("armando", &["germando"]),
    ("evolutiva", &["volutiva"]),
    ("evolutivo", &["volutivo"]),
    ("recibiendo", &["resiguiendo", "resigiendo"]),
    // HU-74: palabras del propio producto que whisper oía mal.
    ("modismos", &["modismos", "comodimos"]),
    // Con el artículo dentro: corregir solo "modismo" se comería el "los".
    ("los modismos", &["los modismo"]),
    ("por voz", &["por voz", "por vos"]),
    ("asustado", &["asustado", "sustado", "azustado"]),
    // HU-74 (lote 2): deformaciones del historial real de transcripciones.
    ("Soy Juan", &["socuen", "soi juan"]),
    ("Háblalo", &["alvarlo", "albarlo"]),
    ("a velocidad", &["avelosidad", "a velosidad", "avelocidad"]),
    ("como ardilla", &["comardilla"]),
    ("un grito", &["su minito", "suminito"]),
    ("espérate", &["asperate"]),
    (
        "ticket en Plane",
        &["tiquetan plane", "ticatain plane", "tiketin plane"],
    ),
    // Con "archivo" dentro para no escribir "archivo archivo .md".
    (
        "archivo .md",
        &[
            "punto de mede",
            "punto de mete",
            "punto mede",
            "archivo punto de mede",
            "archivo punto de mete",
            "archivo punto mede",
        ],
    ),
    // "míeselo" no hace falta: la comparación pliega la tilde.
    ("envíaselo", &["mieselo"]),
    ("Game Boy Advance", &["gamboy advance", "gameboy advance"]),
    // HU-74 (lote 3): frases largas primero; la búsqueda ya prioriza el
    // tramo más largo, el orden aquí es solo para leerlas.
    ("se genere esa diferencia", &["ese genero es a diferencia"]),
    (
        "como si estuvieras",
        &[
            "que hemos y estuvieras",
            "que hemos si estuvieras",
            "como si tuvieras",
        ],
    ),
    ("se te cambió la voz", &["se te cambio la hoja"]),
    (
        "habla normal",
        &[
            "ahora lo es normal",
            "habla lo es normal",
            // HU-74 (lote 4): formas oídas en vivo. Whisper puntúa con comas,
            // así que la variante con coma se conserva; al plegarse, "abla
            // normal" y "abla, normal" comparten entrada en el índice.
            "avladon a mal",
            "havilla no de mal",
            "abla normal",
            "abla, normal",
            "avla normal",
            "habla, normal",
            "ablanormal",
        ],
    ),
    (
        "estás ahí",
        &["este este ahi", "este este ai", "estas ai", "tatai"],
    ),
    ("no me agrada", &["no me grada"]),
    ("con voz", &["con vod", "con vot", "con bos"]),
    ("Super Nintendo", &["suban nintendo", "subanintendo"]),
    ("discos", &["viscos"]),
    // HU-74 (lote 4): whisper se come la hache inicial de "hablar" y de sus
    // conjugaciones. La coincidencia es por palabra completa, así que "ablar"
    // no toca "ablandar" ni "ablando" toca "ablandando".
    ("habla", &["abla", "avla"]),
    ("hablar", &["ablar", "avlar"]),
    ("hablando", &["ablando", "avlando"]),
    // -------------------------------------------------- nombres propios
    ("Iron Man", &["iron man", "iroquman", "airom man"]),
    ("Temuco", &["temuco", "temunco"]),
];

/// Tabla de **negativos**: palabras españolas legítimas que se parecen a una
/// variante y no deben tocarse (CA-70.4).
///
/// Cada caso es una frase completa en la que la palabra sospechosa aparece en
/// su acepción normal. El test `negatives_are_left_untouched` exige que la
/// salida sea **idéntica** a la entrada: ni siquiera cambia una mayúscula.
pub const NEGATIVES: &[&str] = &[
    // "doce" ≠ "Docker" y "apio" ⊃ "api".
    "compré doce kilos de apio en la feria",
    // "cargo" es un puesto de trabajo: por eso "Cargo" solo entra en frases
    // ("cargo build", "Cargo.toml") y nunca suelto.
    "el cargo de gerente quedó vacante",
    // "reporte" ⊃ "repo" (la regresión de HU-59).
    "revisa el reporte de ventas del mes",
    // "gira" ≠ "Jira".
    "la gira presidencial terminó ayer",
    // "consola" ≠ "Konsole".
    "la consola del auto está sucia",
    // "delfín" ≠ "Dolphin".
    "el delfín saltó cerca del bote",
    // "pitón" ≠ "Python".
    "una pitón apareció en el zoológico",
    // "jirafa" ⊃ "jira".
    "la jirafa del zoológico come temprano",
    // "rama" no es "branch".
    "la rama del árbol se quebró",
    // "gitano" ⊃ "git".
    "el gitano del barrio canta bien",
    // "jitomate" ⊃ "jit" (variante de Git).
    "el jitomate de la feria está caro",
    // "Jason" ≠ "JSON".
    "Jason llegó tarde a la reunión",
    // "Linus" no es "Linux".
    "Linus Torvalds escribió el núcleo",
    // "artículo" ⊃ "arto" (variante de "harto").
    "el artículo de la ley es claro",
    // "guagua" y "pichanga" son léxico chileno que **no** está en la tabla.
    "la guagua llora de noche",
    "la pichanga del domingo fue buena",
    // HU-74: "como dimos" es una frase corriente, no "modismos" mal oído.
    "como dimos la vuelta llegamos antes",
    // "po hijo" es jerga legítima: la variante borraba la palabra "hijo".
    "ya po hijo apúrate",
    // "los modismo" se corregía a "modismos" y se comía el artículo.
    "explícame los modismos chilenos",
    // "alto" ≠ "altiro".
    "el alto del cerro está nublado",
    // "cachar" (entender) no es "cachái".
    "cachar la idea requiere tiempo",
    // "pasta", "mapa" y "portal" no son tecnicismos.
    "la pasta de dientes se acabó",
    "el mapa del tesoro es falso",
    "el portal del edificio se cerró",
    // "pulpo" ⊃ "pul" (variante de "pull request").
    "el pulpo del mar rojo es grande",
    // "apiario" ⊃ "api".
    "el apiario del campo produce miel",
    // "ese cuento" no es la fonética de "SQL" ("ese cu ele").
    "ese cuento es muy largo",
    // "respaldo" y "duda" son español corriente.
    "el respaldo del sillón es cómodo",
    "tengo una duda sobre el tema",
    // "espectáculo" no es "Spectacle".
    "el espectáculo de anoche fue increíble",
    // "hoja" solo se corrige dentro de "se te cambio la hoja".
    "compré una hoja de papel",
    "se te cayó la hoja del cuaderno",
    // "voz" y "normal" sueltos no se tocan: las reglas son frases completas.
    "la voz del locutor es grave",
    "todo está normal por ahora",
    // "agrada" bien transcrito no cambia.
    "no me agrada el frío",
    // HU-74 (lote 4): la regla sin hache es por palabra completa; "ablandar" y
    // sus derivados empiezan igual que "ablando" pero no son "hablando".
    "ablanda la mantequilla antes de batir",
    "el ablandador de carne está en el cajón",
    "estuvimos ablandando la masa un rato",
    // "habla" bien transcrito es la forma canónica, no una variante.
    "el habla del pueblo cambia con los años",
];

/// Una entrada ya compilada del índice.
#[derive(Debug, Clone)]
struct Entry {
    /// Variante plegada, token a token.
    folded: Vec<String>,
    /// Forma canónica que se escribe en la salida.
    canonical: String,
}

/// Corrección aplicada a un turno (CA-70.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    /// Tramo original transcrito por whisper.
    pub matched: String,
    /// Forma canónica que lo reemplazó.
    pub canonical: String,
}

impl fmt::Display for Correction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} → {}", self.matched, self.canonical)
    }
}

/// Entrada de usuario descartada por inválida (CA-70.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedEntry {
    /// Clave tal como venía en la configuración.
    pub key: String,
    /// Forma canónica tal como venía en la configuración.
    pub value: String,
    /// Motivo del descarte, ya en texto legible para el log.
    pub reason: &'static str,
}

impl fmt::Display for RejectedEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "«{}» → «{}»: {}", self.key, self.value, self.reason)
    }
}

/// Las palabras de un texto con su posición en bytes.
#[derive(Debug)]
struct Word {
    /// Texto plegado (minúsculas y sin tildes) de la palabra.
    folded: String,
    /// Byte donde empieza.
    start: usize,
    /// Byte donde termina (exclusivo).
    end: usize,
}

impl Word {
    fn new(text: &str, start: usize, end: usize) -> Self {
        Self {
            folded: fold_accents(&text[start..end]),
            start,
            end,
        }
    }
}

/// Corrector léxico/fonético determinista.
///
/// El índice se construye **una vez** al arrancar ([`Lexicon::base`] y, si hay
/// configuración de usuario, [`Lexicon::with_user_entries`]); corregir un turno
/// es entonces un recorrido lineal con búsqueda en tabla por primera palabra.
#[derive(Debug)]
pub struct Lexicon {
    entries: Vec<Entry>,
    /// Primera palabra plegada → índices de entrada, la más larga primero.
    by_first: HashMap<String, Vec<u32>>,
    /// Número de palabras de la entrada más larga.
    max_len: usize,
}

impl Default for Lexicon {
    fn default() -> Self {
        Self::base()
    }
}

impl Lexicon {
    /// Diccionario base embebido, sin extensiones de usuario.
    pub fn base() -> Self {
        let mut entries = Vec::new();
        for (canonical, variants) in BASE_ENTRIES {
            for variant in *variants {
                entries.push(Entry {
                    folded: fold_tokens(variant),
                    canonical: (*canonical).to_string(),
                });
            }
        }
        Self::build(entries)
    }

    /// Copia con las entradas del usuario por delante (tienen prioridad).
    ///
    /// Devuelve también las entradas descartadas para que el binario las
    /// registre; una configuración inválida **no** aborta el arranque
    /// (CA-70.5).
    pub fn with_user_entries(&self, entries: &[(String, String)]) -> (Self, Vec<RejectedEntry>) {
        let mut rejected = Vec::new();
        let mut user = Vec::new();
        for (key, value) in entries {
            match validate_entry(key, value) {
                Ok(()) if user.len() >= MAX_USER_ENTRIES => rejected.push(RejectedEntry {
                    key: key.clone(),
                    value: value.clone(),
                    reason: "se superó el máximo de 500 entradas",
                }),
                Ok(()) => user.push(Entry {
                    folded: fold_tokens(key),
                    canonical: value.trim().to_string(),
                }),
                Err(reason) => rejected.push(RejectedEntry {
                    key: key.clone(),
                    value: value.clone(),
                    reason,
                }),
            }
        }
        let mut all = user;
        all.extend(self.entries.iter().cloned());
        (Self::build(all), rejected)
    }

    /// Número de entradas del índice (base + usuario).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` si el índice quedó vacío (no ocurre con el diccionario base).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Corrige `text` y devuelve la frase con los tramos canónicos.
    pub fn correct(&self, text: &str) -> String {
        self.rewrite(text, None)
    }

    /// Como [`correct`](Self::correct) pero devuelve además cada sustitución
    /// aplicada, en orden, para el log local (CA-70.8).
    pub fn correct_traced(&self, text: &str) -> (String, Vec<Correction>) {
        let mut corrections = Vec::new();
        let corrected = self.rewrite(text, Some(&mut corrections));
        (corrected, corrections)
    }

    fn build(entries: Vec<Entry>) -> Self {
        let mut max_len = 1;
        let mut seen = HashSet::new();
        let mut kept: Vec<Entry> = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.folded.is_empty() {
                continue;
            }
            // La primera aparición gana: las de usuario van delante y así
            // pueden redefinir una variante del diccionario base.
            if !seen.insert(entry.folded.join(" ")) {
                continue;
            }
            max_len = max_len.max(entry.folded.len());
            kept.push(entry);
        }
        let mut by_first: HashMap<String, Vec<u32>> = HashMap::new();
        for (idx, entry) in kept.iter().enumerate() {
            if let Some(first) = entry.folded.first() {
                by_first.entry(first.clone()).or_default().push(idx as u32);
            }
        }
        for list in by_first.values_mut() {
            // Frase más larga primero; `sort_by_key` es estable, así que entre
            // iguales se respeta el orden de inserción (usuario antes que base).
            list.sort_by_key(|&idx| std::cmp::Reverse(kept[idx as usize].folded.len()));
        }
        Self {
            entries: kept,
            by_first,
            max_len,
        }
    }

    fn rewrite(&self, text: &str, mut trail: Option<&mut Vec<Correction>>) -> String {
        if self.entries.is_empty() || self.max_len == 0 {
            return text.to_string();
        }
        let words = scan_words(text);
        if words.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len() + 8);
        // Byte hasta el que ya se copió texto original.
        let mut copied = 0;
        let mut i = 0;
        while i < words.len() {
            let Some((entry_idx, len)) = self.longest_match(text, &words, i) else {
                i += 1;
                continue;
            };
            let entry = &self.entries[entry_idx as usize];
            let first = &words[i];
            let last = &words[i + len - 1];
            out.push_str(&text[copied..first.start]);
            let matched = &text[first.start..last.end];
            let canonical = display_form(matched, &entry.canonical);
            if let Some(list) = trail.as_deref_mut() {
                list.push(Correction {
                    matched: matched.to_string(),
                    canonical: canonical.clone(),
                });
            }
            out.push_str(&canonical);
            copied = last.end;
            i += len;
        }
        out.push_str(&text[copied..]);
        out
    }

    /// Entrada más larga que empieza en `words[start]`, si hay alguna.
    fn longest_match(&self, text: &str, words: &[Word], start: usize) -> Option<(u32, usize)> {
        let candidates = self.by_first.get(&words[start].folded)?;
        for &idx in candidates {
            let entry = &self.entries[idx as usize];
            let len = entry.folded.len();
            if len == 0 || start + len > words.len() {
                continue;
            }
            let window = &words[start..start + len];
            if entry.folded.iter().zip(window).all(|(a, b)| *a == b.folded)
                && separators_ok(text, window)
            {
                return Some((idx, len));
            }
        }
        None
    }
}

/// Separa las palabras de `text` conservando sus posiciones en bytes.
fn scan_words(text: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, c) in text.char_indices() {
        if c.is_alphanumeric() {
            start.get_or_insert(idx);
        } else if let Some(begin) = start.take() {
            words.push(Word::new(text, begin, idx));
        }
    }
    if let Some(begin) = start {
        words.push(Word::new(text, begin, text.len()));
    }
    words
}

/// `true` si entre palabra y palabra solo hay separadores débiles (espacios,
/// guion, guion bajo, punto, apóstrofo o coma seguida de espacio): así
/// "cargo.toml" y "lorem-ipsum" también casan, pero nunca dos palabras pegadas
/// sin separador.
///
/// La coma solo cuenta si va **seguida de un espacio** (", "), que es como
/// puntúa whisper (HU-74, lote 4): "abla, normal" → "habla normal". Sin esa
/// condición, "a,b" uniría dos palabras que el hablante separó.
fn separators_ok(text: &str, words: &[Word]) -> bool {
    words.windows(2).all(|pair| {
        let mut gap = text[pair[0].end..pair[1].start].chars().peekable();
        while let Some(c) = gap.next() {
            let weak = c.is_whitespace() || matches!(c, '-' | '_' | '.' | '\'');
            let comma = c == ',' && gap.peek().is_some_and(|next| next.is_whitespace());
            if !weak && !comma {
                return false;
            }
        }
        true
    })
}

/// Forma que se escribe: la canónica de tabla, salvo que el original venga
/// gritado (todo en mayúsculas), en cuyo caso se respeta el énfasis.
fn display_form(matched: &str, canonical: &str) -> String {
    let letters: Vec<char> = matched.chars().filter(|c| c.is_alphabetic()).collect();
    let shouted = letters.len() >= 2 && letters.iter().all(|c| c.is_uppercase());
    if shouted && canonical.chars().any(char::is_alphabetic) {
        canonical.to_uppercase()
    } else {
        canonical.to_string()
    }
}

/// Tokens plegados de una frase ("Pe  Erre" → `["pe", "erre"]`).
fn fold_tokens(phrase: &str) -> Vec<String> {
    scan_words(phrase)
        .into_iter()
        .map(|word| word.folded)
        .collect()
}

/// Valida una entrada de usuario (CA-70.5).
fn validate_entry(key: &str, value: &str) -> Result<(), &'static str> {
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() {
        return Err("la variante está vacía");
    }
    if value.is_empty() {
        return Err("la forma canónica está vacía");
    }
    if key.chars().count() > MAX_ENTRY_CHARS {
        return Err("la variante supera los 64 caracteres");
    }
    if value.chars().count() > MAX_ENTRY_CHARS {
        return Err("la forma canónica supera los 64 caracteres");
    }
    if key.chars().chain(value.chars()).any(char::is_control) {
        return Err("contiene caracteres de control");
    }
    if fold_tokens(key).is_empty() {
        return Err("la variante no tiene palabras");
    }
    if normalize(key).has_any(PROTECTED_PHRASES) {
        return Err("la variante pisa la activación o una orden de anulación");
    }
    if normalize(value).has_any(PROTECTED_PHRASES) {
        return Err("la forma canónica pisa la activación o una orden de anulación");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Lexicon {
        Lexicon::base()
    }

    // ------------------------------------------------------------- CA-70.1

    #[test]
    fn corrects_only_the_matching_span() {
        let lex = base();
        assert_eq!(
            lex.correct("Oye, ¿arreglamos el doquer de Producción? ¡Es urgente!"),
            "Oye, ¿arreglamos el Docker de Producción? ¡Es urgente!"
        );
        // Sin coincidencias, la salida es idéntica (misma capitalización,
        // mismas tildes, misma puntuación).
        let plain = "Mañana reviso el informe con José, ¡a las 8:30!";
        assert_eq!(lex.correct(plain), plain);
        assert_eq!(lex.correct(""), "");
    }

    #[test]
    fn keeps_accents_and_capitals_of_the_untouched_words() {
        let lex = base();
        assert_eq!(
            lex.correct("CUÉNTAME por qué Cubernetis falló en Ámsterdam"),
            "CUÉNTAME por qué Kubernetes falló en Ámsterdam"
        );
        // Tildes y ñ del resto de la frase sobreviven.
        assert_eq!(
            lex.correct("el niño pidió un lápiz para el loren ypsum"),
            "el niño pidió un lápiz para el Lorem Ipsum"
        );
    }

    #[test]
    fn shouted_spans_keep_the_emphasis() {
        let lex = base();
        assert_eq!(lex.correct("revisa el DOQUER"), "revisa el DOCKER");
        assert_eq!(lex.correct("el Doquer"), "el Docker");
    }

    // ------------------------------------------------------------- CA-70.2

    #[test]
    fn matching_is_by_whole_word() {
        let lex = base();
        for untouched in [
            "revisa el reporte",
            "compré apio",
            "el cargo de gerente",
            "mi jirafa favorita",
            "el gitano canta",
        ] {
            assert_eq!(lex.correct(untouched), untouched, "{untouched}");
        }
        // Y la palabra suelta sí se corrige.
        assert_eq!(lex.correct("revisa el repo"), "revisa el repo");
        assert_eq!(lex.correct("la api responde"), "la API responde");
    }

    #[test]
    fn the_longest_phrase_wins() {
        let lex = base();
        assert_eq!(
            lex.correct("revisa git hub actions"),
            "revisa GitHub Actions"
        );
        assert_eq!(lex.correct("revisa git hub"), "revisa GitHub");
        assert_eq!(lex.correct("abre kde plasma"), "abre KDE Plasma");
        assert_eq!(lex.correct("abre kde"), "abre KDE");
        assert_eq!(
            lex.correct("toca el cargo punto toml"),
            "toca el Cargo.toml"
        );
    }

    #[test]
    fn multi_word_variants_collapse_to_one_span() {
        let lex = base();
        assert_eq!(lex.correct("revisa las pe erre"), "revisa las PR");
        assert_eq!(lex.correct("el pull request de Melón"), "el PR de Melón");
        assert_eq!(lex.correct("las prs viejas"), "las PR viejas");
        // Puntuación y separadores entre las palabras del tramo.
        assert_eq!(
            lex.correct("el loren-ipsum del readme"),
            "el Lorem Ipsum del README"
        );
    }

    // ------------------------------------------------------------- CA-70.3

    #[test]
    fn base_dictionary_has_at_least_sixty_entries() {
        assert!(
            BASE_ENTRIES.len() >= 60,
            "solo {} entradas",
            BASE_ENTRIES.len()
        );
        for (canonical, _) in BASE_ENTRIES {
            assert!(!canonical.is_empty());
        }
    }

    #[test]
    fn every_variant_maps_to_its_canonical_form() {
        let lex = base();
        for (canonical, variants) in BASE_ENTRIES {
            for variant in *variants {
                assert_eq!(
                    lex.correct(variant),
                    *canonical,
                    "la variante {variant:?} no llegó a {canonical:?}"
                );
            }
        }
    }

    #[test]
    fn frequent_terms_are_present() {
        let lex = base();
        for (input, expected) in [
            ("loren ypsum", "Lorem Ipsum"),
            ("cubernetis", "Kubernetes"),
            ("espectacle", "Spectacle"),
            ("doquer", "Docker"),
            ("pe erre", "PR"),
            ("guit hub", "GitHub"),
            ("yeison", "JSON"),
            ("yamel", "YAML"),
            ("ka de e plasma", "KDE Plasma"),
            ("güeyland", "Wayland"),
            ("cargo clipi", "cargo clippy"),
            ("pantallaso", "pantallazo"),
            ("al tiro", "al tiro"),
            ("cachay", "cachái"),
        ] {
            assert_eq!(lex.correct(input), expected, "{input}");
        }
    }

    /// Términos del historial real de transcripciones: nombres del ecosistema
    /// (Antigravity, Claude, DeepSeek), apps, lugares y las confusiones del
    /// habla chilena que whisper devuelve deformadas.
    #[test]
    fn transcript_history_terms_are_present() {
        let lex = base();
        for (input, expected) in [
            ("nanti gravity", "Antigravity"),
            ("nanti-gravity", "Antigravity"),
            ("antigradity", "Antigravity"),
            ("claud", "Claude"),
            ("clod", "Claude"),
            ("dip sik", "DeepSeek"),
            ("dipsek", "DeepSeek"),
            ("espotifai", "Spotify"),
            ("spotifai", "Spotify"),
            ("temunco", "Temuco"),
            ("iroquman", "Iron Man"),
            ("airom man", "Iron Man"),
            ("asento", "acento"),
            ("asentos", "acentos"),
            ("po", "po"),
            ("poh", "po"),
            ("los modismo", "los modismos"),
            ("comodimos", "modismos"),
            ("al-tiro", "al tiro"),
            ("altiro", "al tiro"),
            ("conpadre", "compadre"),
            ("compadrito", "compadre"),
            ("volutiva", "evolutiva"),
            ("germando", "armando"),
            ("resiguiendo", "recibiendo"),
            ("socuen", "Soy Juan"),
            ("alvarlo", "Háblalo"),
            (
                "habla a velosidad de ardilla",
                "habla a velocidad de ardilla",
            ),
            ("comardilla", "como ardilla"),
            ("dame su minito", "dame un grito"),
            ("asperate", "espérate"),
            ("crea un tiketin plane", "crea un ticket en Plane"),
            ("guárdalo en un punto de mede", "guárdalo en un archivo .md"),
            (
                "guárdalo en un archivo punto mede",
                "guárdalo en un archivo .md",
            ),
            ("míeselo", "envíaselo"),
            ("míeselo a Felipe", "envíaselo a Felipe"),
            ("gamboy advance", "Game Boy Advance"),
            // Lote 3.
            ("hola este este ahi", "hola estás ahí"),
            ("jarvis estas ai", "jarvis estás ahí"),
            ("tatai", "estás ahí"),
            (
                "habla que hemos y estuvieras cansado",
                "habla como si estuvieras cansado",
            ),
            (
                "habla como si tuvieras asustado",
                "habla como si estuvieras asustado",
            ),
            ("responde con vod grave", "responde con voz grave"),
            ("háblame con bos", "háblame con voz"),
            ("oye se te cambio la hoja", "oye se te cambió la voz"),
            ("ahora lo es normal", "habla normal"),
            ("habla lo es normal por favor", "habla normal por favor"),
            ("eso no me grada", "eso no me agrada"),
            (
                "no quiero que ese genero es a diferencia",
                "no quiero que se genere esa diferencia",
            ),
            ("juegos de suban nintendo", "juegos de Super Nintendo"),
            ("subanintendo", "Super Nintendo"),
            ("los viscos duros", "los discos duros"),
            ("crea un tiquetan plane", "crea un ticket en Plane"),
            ("crea un ticatain plane", "crea un ticket en Plane"),
            // Lote 4: "habla normal" y las formas sin hache de hablar.
            ("Avladón a mal", "habla normal"),
            ("Havilla no de mal", "habla normal"),
            ("Abla, normal", "habla normal"),
            ("avladon a mal", "habla normal"),
            ("ablanormal", "habla normal"),
            ("habla, normal", "habla normal"),
            ("avla normal", "habla normal"),
            ("abla con el equipo", "habla con el equipo"),
            ("estoy ablando", "estoy hablando"),
            ("quiero ablar", "quiero hablar"),
            ("estamos avlando de eso", "estamos hablando de eso"),
        ] {
            assert_eq!(lex.correct(input), expected, "{input}");
        }
    }

    // ------------------------------------------------------------- CA-70.4

    #[test]
    fn negatives_are_left_untouched() {
        assert!(NEGATIVES.len() >= 20, "solo {} negativos", NEGATIVES.len());
        let lex = base();
        for sentence in NEGATIVES {
            assert_eq!(
                lex.correct(sentence),
                *sentence,
                "el negativo se tocó: {sentence}"
            );
        }
    }

    /// HU-74 (lote 4): una coma seguida de espacio no corta un tramo, pero sin
    /// espacio no une dos palabras.
    #[test]
    fn a_comma_followed_by_space_does_not_block_a_span() {
        let lex = base();
        assert_eq!(lex.correct("abla, normal"), "habla normal");
        assert_eq!(lex.correct("Abla, normal"), "habla normal");
        // Espacio antes de la coma: también es puntuación normal.
        assert_eq!(lex.correct("habla ,  normal"), "habla normal");
        // Sin espacio la coma no une: solo se corrige la palabra suelta.
        assert_eq!(lex.correct("abla,normal"), "habla,normal");
        // Y la coma fuera de un tramo se conserva tal cual.
        assert_eq!(
            lex.correct("buenas, abla con el equipo"),
            "buenas, habla con el equipo"
        );
    }

    // ------------------------------------------------------------- CA-70.5

    #[test]
    fn user_entries_extend_the_dictionary() {
        let (lex, rejected) = Lexicon::base().with_user_entries(&[
            ("melon".to_string(), "Melón".to_string()),
            ("espresa".to_string(), "Express".to_string()),
        ]);
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(
            lex.correct("despliega la espresa de melon"),
            "despliega la Express de Melón"
        );
        assert_eq!(lex.len(), Lexicon::base().len() + 2);
    }

    #[test]
    fn user_entries_can_override_a_base_variant() {
        let (lex, rejected) = Lexicon::base()
            .with_user_entries(&[("doquer".to_string(), "Docker Desktop".to_string())]);
        assert!(rejected.is_empty());
        assert_eq!(lex.correct("abre el doquer"), "abre el Docker Desktop");
        // El resto del diccionario base sigue intacto.
        assert_eq!(lex.correct("abre el docker"), "abre el Docker");
    }

    #[test]
    fn invalid_user_entries_are_rejected_with_a_reason() {
        let too_long = "a".repeat(MAX_ENTRY_CHARS + 1);
        let cases = [
            ("".to_string(), "Algo".to_string()),
            ("algo".to_string(), "".to_string()),
            (too_long.clone(), "Algo".to_string()),
            ("algo".to_string(), too_long),
            ("al\ngo".to_string(), "Algo".to_string()),
            // Una variante sin letras no sirve de nada.
            ("!!!".to_string(), "Algo".to_string()),
            // Fronteras de seguridad: activación y órdenes de anulación.
            ("jarvis".to_string(), "Jarvis".to_string()),
            ("hola".to_string(), "Jarvis".to_string()),
            ("hola".to_string(), "silencio".to_string()),
            ("hola".to_string(), "cancela todas las tareas".to_string()),
            ("modo silencio".to_string(), "Modo Silencio".to_string()),
        ];
        let owned: Vec<(String, String)> = cases.to_vec();
        let (lex, rejected) = Lexicon::base().with_user_entries(&owned);
        assert_eq!(rejected.len(), owned.len(), "todas debían rechazarse");
        for entry in &rejected {
            assert!(!entry.reason.is_empty());
            assert!(entry.to_string().contains(entry.reason));
        }
        // Nada de eso entró: el índice es el del diccionario base.
        assert_eq!(lex.len(), Lexicon::base().len());
        assert_eq!(lex.correct("hola"), "hola");
    }

    #[test]
    fn the_entry_limit_is_enforced() {
        let entries: Vec<(String, String)> = (0..MAX_USER_ENTRIES + 1)
            .map(|i| (format!("variante{i}"), format!("Canonica{i}")))
            .collect();
        let (lex, rejected) = Lexicon::base().with_user_entries(&entries);
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].key, format!("variante{MAX_USER_ENTRIES}"));
        assert_eq!(lex.len(), Lexicon::base().len() + MAX_USER_ENTRIES);
    }

    // ------------------------------------------------------------- CA-70.6

    /// Cien correcciones de un turno de 120 palabras con 560 entradas: si el
    /// índice fuera cuadrático esto tardaría segundos.
    #[test]
    fn large_dictionary_stays_linear() {
        let entries: Vec<(String, String)> = (0..MAX_USER_ENTRIES)
            .map(|i| (format!("variante{i}"), format!("Canonica{i}")))
            .collect();
        let (lex, rejected) = Lexicon::base().with_user_entries(&entries);
        assert!(rejected.is_empty());
        let words: Vec<&str> = (0..120)
            .map(|i| if i % 7 == 0 { "cubernetis" } else { "palabra" })
            .collect();
        let text = words.join(" ");
        let started = std::time::Instant::now();
        for _ in 0..100 {
            let out = lex.correct(&text);
            assert!(out.contains("Kubernetes"));
        }
        // Margen enorme: en modo depuración esto tarda milisegundos.
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    /// CA-70.6: p99 < 1 ms. Se mide con el binario optimizado, así que va
    /// marcado como `#[ignore]`: `cargo test -p jarvis-audio -- --ignored`.
    #[test]
    #[ignore = "mide tiempo: ejecutar con --ignored y perfil release"]
    fn correction_stays_under_one_millisecond() {
        let entries: Vec<(String, String)> = (0..MAX_USER_ENTRIES)
            .map(|i| (format!("variante{i}"), format!("Canonica{i}")))
            .collect();
        let (lex, _) = Lexicon::base().with_user_entries(&entries);
        assert_eq!(lex.len(), Lexicon::base().len() + MAX_USER_ENTRIES);
        let words: Vec<&str> = (0..120)
            .map(|i| if i % 7 == 0 { "cubernetis" } else { "palabra" })
            .collect();
        let text = words.join(" ");
        // Calentamiento (page faults, caché de cadenas).
        for _ in 0..20 {
            let _ = lex.correct(&text);
        }
        let mut samples = Vec::with_capacity(200);
        for _ in 0..200 {
            let started = std::time::Instant::now();
            let out = lex.correct(&text);
            samples.push(started.elapsed());
            assert!(!out.is_empty());
        }
        samples.sort();
        let p99 = samples[samples.len() * 99 / 100];
        assert!(p99 < std::time::Duration::from_millis(1), "p99 = {p99:?}");
    }

    // ------------------------------------------------------------- CA-70.8

    #[test]
    fn tracing_reports_each_applied_correction() {
        let lex = base();
        let (corrected, corrections) = lex.correct_traced("abre el doquer y el cubernetis");
        assert_eq!(corrected, "abre el Docker y el Kubernetes");
        assert_eq!(
            corrections,
            vec![
                Correction {
                    matched: "doquer".to_string(),
                    canonical: "Docker".to_string(),
                },
                Correction {
                    matched: "cubernetis".to_string(),
                    canonical: "Kubernetes".to_string(),
                },
            ]
        );
        assert_eq!(corrections[0].to_string(), "doquer → Docker");
        // Sin correcciones no hay nada que registrar.
        let (_, none) = lex.correct_traced("hola, ¿qué hora es?");
        assert!(none.is_empty());
    }

    // ------------------------------------------------------------ varios

    #[test]
    fn tokens_are_folded_for_lookup() {
        assert_eq!(fold_tokens("Pe  Erre!"), ["pe", "erre"]);
        assert_eq!(fold_tokens("Cubernetis,"), ["cubernetis"]);
        assert!(fold_tokens("!!").is_empty());
    }

    #[test]
    fn an_empty_lexicon_returns_the_text_untouched() {
        let lex = Lexicon::build(Vec::new());
        assert!(lex.is_empty());
        assert_eq!(lex.correct("el doquer"), "el doquer");
        let (out, corrections) = lex.correct_traced("el doquer");
        assert_eq!(out, "el doquer");
        assert!(corrections.is_empty());
    }
}
