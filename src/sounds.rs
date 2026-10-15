//! Die Klänge des Telefons.
//!
//! Es gibt zwei Sorten, und beide sind Dateien statt Konfiguration:
//!
//! - **Feste Klänge** ([`Sound`]): Start, Neustart, Klingeln, Testtöne und
//!   Bestätigung. Das Telefon braucht sie immer, also hat jeder einen festen
//!   Namen und einen eigenen [`Spec`] - ein Tippfehler kann keine Datei an den
//!   falschen Klang hängen.
//! - **Sammlungen** ([`Pool`]): eigene Klingeltöne und Einschlafmusik. Das sind
//!   Listen, in die der Besitzer Dateien hochlädt, und darum ist der Name der
//!   Datei das, woran sie unterschieden werden - bei einem Klingelton der Name,
//   der die Rufnummer nennt.
//!
//! Beides liegt unter [`DIR`] und wird über die Weboberfläche gefüllt. In
//! `config.toml` steht nichts davon, auch keine Klingeltöne je Rufnummer: der
//! Name der Datei sagt das auch.
//!
//! ## Namen
//!
//! Bei den festen Klängen ist der Name bis auf die Endung fest: `boot` heißt
//! `boot.wav` oder `boot.mp3`, je nachdem was hochgeladen wurde. Die Endung folgt
//! dem Inhalt der Datei, weil eine MP3, die als WAV ausgeliefert wird, im Browser
//! nicht abspielt.
//!
//! Bei den Klingeltönen entscheidet der Name, wer klingelt: eine Datei namens
//! `ringtone-<Rufnummer>` gehört genau dieser Rufnummer, und nur sie. Der Name
//! entsteht beim Hochladen aus der Rufnummer, die neben der Dateiauswahl
//! einzugeben ist ([`ringtone_name`]); was im Verzeichnis anders heißt, wird
//! weder abgespielt noch aufgeführt. Bei der Einschlafmusik sagt der Name
//! nichts; alle Dateien zusammen sind die Playlist.
//!
//! Bei der Einschlafmusik bleibt der Name erhalten, den eine Datei hatte - er
//! ist das einzige, was sie unterscheidbar macht. Weggelassen wird nur, was in
//! einem Dateinamen nichts zu suchen hat ([`file_stem`]), und die Oberfläche
//! nennt danach den Namen, unter dem die Datei tatsächlich liegt.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{json, Value as Json};

/// Verzeichnis, in dem die Klangdateien liegen.
///
/// Der Pfad ist fest, weil die Klänge nicht konfiguriert sind; der Webserver
/// und das Telefon lesen beide hierher. Für Tests und Entwicklungsrechner
/// lässt er sich mit `--sounds` umbiegen.
pub const DIR: &str = "/usr/local/share/piphone/sounds";

/// Endungen, die eine hochgeladene Datei haben kann, in der Reihenfolge, in der
/// sie gesucht wird.
const EXTENSIONS: [&str; 2] = ["wav", "mp3"];

/// Anfang des Namens, der einen Klingelton an eine Rufnummer bindet.
const RINGTONE_PREFIX: &str = "ringtone-";

/// So lang darf der Name einer hochgeladenen Datei werden.
///
/// Länger ist kein Name, sondern ein Unfall - und ein gekürzter Name wäre nicht
/// mehr der, unter dem der Besitzer die Datei wiedererkennt.
const MAX_NAME: usize = 120;

/// Das Format einer hochgeladenen Datei, am Inhalt erkannt.
///
/// Erkannt wird nur, was symphonia hier ohnehin lesen kann - der Rest wird
/// abgelehnt, statt als Datei mit falscher Endung im Verzeichnis zu liegen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Wav,
    Mp3,
}

impl Format {
    /// Das Format am Anfang der Datei, `None` für alles andere.
    pub fn detect(bytes: &[u8]) -> Option<Format> {
        // RIFF....WAVE
        if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE".as_slice()) {
            return Some(Format::Wav);
        }
        // ID3-Tag oder MPEG-Kopf: elf Einsen, dann die Version
        if bytes.starts_with(b"ID3")
            || (bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
        {
            return Some(Format::Mp3);
        }
        None
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Wav => "wav",
            Format::Mp3 => "mp3",
        }
    }

    /// Content-Type für die Vorschau im Browser.
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Wav => "audio/wav",
            Format::Mp3 => "audio/mpeg",
        }
    }

    /// Das Format einer Datei anhand ihrer Endung, für das Ausliefern.
    pub fn from_path(path: &Path) -> Option<Format> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "wav" => Some(Format::Wav),
            "mp3" => Some(Format::Mp3),
            _ => None,
        }
    }
}

/// Die Klänge, die das Telefon immer braucht.
///
/// Feste Klänge haben einen festen Namen und genau eine Datei; die Sammlungen
/// in [`Pool`] sind diejenigen, die beliebig viele Dateien haben dürfen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    /// Beim Start des Telefons.
    Boot,
    /// Vor einem Neustart.
    Reboot,
    /// Vor dem Herunterfahren.
    Shutdown,
    /// Generischer Klingelton, wenn die Rufnummer keinen eigenen hat.
    Ringtone,
    /// Testton über den Lautsprecher.
    TestLoud,
    /// Testton über den Hörer.
    TestEarpiece,
    /// Bestätigung eines Kurzbefehls.
    Confirm,
}

/// Alles, was der Webserver über einen festen Klang wissen muss.
pub struct Spec {
    /// Name im Upload-URL und in der Oberfläche.
    pub key: &'static str,
    /// Beschriftung in der Oberfläche.
    pub label: &'static str,
    /// Dateiname ohne Endung.
    pub stem: &'static str,
    /// Erklärung in der Oberfläche.
    pub help: &'static [&'static str],
}

static BOOT: Spec = Spec {
    key: "boot",
    label: "Start",
    stem: "boot",
    help: &["Klang beim Start des Telefons."],
};
static REBOOT: Spec = Spec {
    key: "reboot",
    label: "Neustart",
    stem: "reboot",
    help: &["Klang vor einem Neustart."],
};
static SHUTDOWN: Spec = Spec {
    key: "shutdown",
    label: "Herunterfahren",
    stem: "shutdown",
    help: &["Klang vor dem Herunterfahren."],
};
static RINGTONE: Spec = Spec {
    key: "ringtone",
    label: "Klingeln",
    stem: "ringtone",
    help: &[
        "Generischer Klingelton: er klingelt für alle Anrufe, für die es keine",
        "eigene Datei in der Liste der Klingeltöne gibt.",
    ],
};
static TEST_LOUD: Spec = Spec {
    key: "test_loud",
    label: "Test Lautsprecher",
    stem: "test-loud",
    help: &["Testton über den Lautsprecher."],
};
static TEST_EARPIECE: Spec = Spec {
    key: "test_earpiece",
    label: "Test Hörer",
    stem: "test-earpiece",
    help: &["Testton über den Hörer."],
};
static CONFIRM: Spec = Spec {
    key: "confirm",
    label: "Bestätigung",
    stem: "confirm",
    help: &["Klang zur Bestätigung eines Kurzbefehls."],
};

impl Sound {
    /// Alle festen Klänge, in der Reihenfolge, in der die Oberfläche sie zeigt.
    pub const ALL: [Sound; 7] = [
        Sound::Boot,
        Sound::Reboot,
        Sound::Shutdown,
        Sound::Ringtone,
        Sound::TestLoud,
        Sound::TestEarpiece,
        Sound::Confirm,
    ];

    /// Alles über diesen Klang.
    pub const fn spec(self) -> &'static Spec {
        match self {
            Sound::Boot => &BOOT,
            Sound::Reboot => &REBOOT,
            Sound::Shutdown => &SHUTDOWN,
            Sound::Ringtone => &RINGTONE,
            Sound::TestLoud => &TEST_LOUD,
            Sound::TestEarpiece => &TEST_EARPIECE,
            Sound::Confirm => &CONFIRM,
        }
    }

    /// Der Klang mit diesem Namen, wenn es einen gibt.
    pub fn from_key(key: &str) -> Option<Sound> {
        Sound::ALL.into_iter().find(|sound| sound.spec().key == key)
    }

    /// Die Datei dieses Klangs, wenn sie hochgeladen wurde.
    pub fn file(self, dir: &Path) -> Option<PathBuf> {
        self.spec().file(dir)
    }

    /// Der Pfad als Zeichenkette für die Wiedergabe, leer ohne Datei.
    ///
    /// Leer und nicht `None`, weil die Wiedergabe einen leeren Pfad bereits
    /// als "nichts zu spielen" kennt: sie hält an, statt zu scheitern.
    pub fn path(self, dir: &Path) -> String {
        self.file(dir)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

impl Spec {
    /// Die Klänge, die noch keine Datei haben.
    ///
    /// Beim Start aufgezählt: ein fehlender Klang fällt zur Laufzeit nur als
    /// Zeile im Protokoll auf, wenn gerade der Moment kommt, in dem er laufen
    /// sollte.
    pub fn missing(dir: &Path) -> Vec<&'static Spec> {
        Sound::ALL
            .into_iter()
            .map(|sound| sound.spec())
            .filter(|spec| spec.file(dir).is_none())
            .collect()
    }

    /// Die hochgeladene Datei, wenn es eine gibt.
    pub fn file(&self, dir: &Path) -> Option<PathBuf> {
        EXTENSIONS
            .iter()
            .copied()
            .map(|extension| dir.join(self.file_name(extension)))
            .find(|path| path.is_file())
    }

    /// Der Dateiname mit dieser Endung.
    fn file_name(&self, extension: &str) -> String {
        format!("{}.{}", self.stem, extension)
    }

    /// Der Klang, wie die Oberfläche ihn anzeigt.
    pub fn describe(&self, dir: &Path) -> Json {
        let file = self.file(dir);

        json!({
            "key": self.key,
            "label": self.label,
            "help": self.help,
            "present": file.is_some(),
            "file": file
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned()),
            "size": file
                .as_ref()
                .map(|path| fs::metadata(path).map(|meta| meta.len()).unwrap_or(0))
                .unwrap_or(0),
            "content_type": file.as_deref().and_then(Format::from_path).map(Format::content_type),
        })
    }

    /// `bytes` als Datei dieses Klangs ablegen.
    ///
    /// Der Name ist fest: was hochgeladen wird, ersetzt den Klang, den es gibt,
    /// und nichts sonst. Geschrieben wird über eine temporäre Datei, die erst am
    /// Ende an ihren Platz kommt - bricht der Vorgang ab, ist der alte Klang noch
    /// da und nicht halb der neue.
    pub fn store(&self, dir: &Path, bytes: &[u8]) -> Result<Json, StoreError> {
        let format = Format::detect(bytes).ok_or(StoreError::Unsupported)?;

        fs::create_dir_all(dir).map_err(|e| StoreError::Io(format!(
            "Verzeichnis {} nicht anlegbar: {}",
            dir.display(),
            e
        )))?;

        let target = dir.join(self.file_name(format.extension()));
        write_atomically(&target, bytes)?;

        for extension in EXTENSIONS {
            let other = dir.join(self.file_name(extension));
            if other != target {
                let _ = fs::remove_file(other);
            }
        }

        Ok(self.describe(dir))
    }
}

/// Eine Sammlung hochgeladener Dateien.
///
/// Anders als die festen Klänge kennt eine Sammlung keine Namen im Voraus: was
/// hochgeladen wurde, liegt da und wird benutzt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pool {
    /// Klingeltöne, eigene je Rufnummer und einer für alle übrigen Anrufe.
    Ringtones,
    /// Einschlafmusik, mehrere Titel als Playlist.
    Sleep,
}

/// Alles, was der Webserver über eine Sammlung wissen muss.
pub struct PoolSpec {
    /// Name im URL und als Schlüssel in der Antwort.
    pub key: &'static str,
    /// Unterverzeichnis unter [`DIR`].
    pub dir: &'static str,
    /// Beschriftung in der Oberfläche.
    pub label: &'static str,
    /// Ob der Name der Datei eine Rufnummer nennt, also die Oberfläche danach
    /// fragen muss, bevor sie hochlädt.
    pub numbered: bool,
    /// Erklärung in der Oberfläche.
    pub help: &'static [&'static str],
}

static RINGTONES: PoolSpec = PoolSpec {
    key: "ringtones",
    dir: "ringtones",
    label: "Klingeltöne",
    numbered: true,
    help: &[
        "Hochgeladene Klingeltöne, einer je Rufnummer: die Rufnummer neben der Auswahl",
        "eintragen, dann heißt die Datei ringtone-<Rufnummer> und klingelt nur für",
        "diese Nummer - verglichen werden nur ihre Ziffern. Alle anderen Anrufe",
        "klingeln mit dem festen Klang Klingeln.",
    ],
};
static SLEEP: PoolSpec = PoolSpec {
    key: "sleep",
    dir: "sleep",
    label: "Einschlafmusik",
    numbered: false,
    help: &[
        "Alle hochgeladenen Titel bilden die Playlist. Beim Start wird sie neu",
        "gemischt und mindestens 15 Minuten gespielt; eine einzelne Datei läuft",
        "einmal durch. Gestoppt wird mit demselben Kurzbefehl.",
    ],
};

impl Pool {
    /// Alle Sammlungen, in der Reihenfolge, in der die Oberfläche sie zeigt.
    pub const ALL: [Pool; 2] = [Pool::Ringtones, Pool::Sleep];

    /// Alles über diese Sammlung.
    pub const fn spec(self) -> &'static PoolSpec {
        match self {
            Pool::Ringtones => &RINGTONES,
            Pool::Sleep => &SLEEP,
        }
    }

    /// Die Sammlung mit diesem Namen, wenn es eine gibt.
    pub fn from_key(key: &str) -> Option<Pool> {
        Pool::ALL.into_iter().find(|pool| pool.spec().key == key)
    }

    /// Das Verzeichnis dieser Sammlung unterhalb von `base`.
    pub fn dir(self, base: &Path) -> PathBuf {
        base.join(self.spec().dir)
    }

    /// Alle Dateien der Sammlung, nach Name geordnet.
    ///
    /// Nach Name, nicht in der Reihenfolge, in der das Verzeichnis sie hergibt:
    /// die Oberfläche soll stabil bleiben, und die Einschlafmusik gemischt wird
    /// ohnehin.
    ///
    /// Was nicht nach Klangdatei *heißt*, zählt nicht mit - eine temporäre Datei
    /// aus einem unterbrochenen Upload oder eine Notiz im Verzeichnis. Bei
    /// Klingeltönen zählt zusätzlich, dass der Name eine Rufnummer nennt
    /// ([`counted`]). Der Inhalt wird nicht geprüft: hochgeladen wird nur, was
    /// ein Klang ist, und eine von Hand abgelegte Datei, die sich als Klang
    /// ausgibt, soll sichtbar sein und löschbar, nicht beim Spielen eine
    /// Überraschung.
    pub fn entries(self, base: &Path) -> Vec<Entry> {
        let Ok(directory) = fs::read_dir(self.dir(base)) else {
            return Vec::new(); // Noch nichts hochgeladen: das ist kein Fehler.
        };

        let mut entries: Vec<Entry> = directory
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .filter(|entry| self.counted(&entry.path()))
            .filter_map(|entry| {
                let path = entry.path();
                Format::from_path(&path).map(|format| self.entry_at(&path, format))
            })
            .collect();

        entries.sort_by(|one, other| one.name.cmp(&other.name));
        entries
    }

    /// Eine Datei dieser Sammlung, wenn es sie gibt.
    pub fn entry(self, base: &Path, name: &str) -> Option<Entry> {
        if !self.counted(Path::new(name)) {
            return None;
        }

        let stem = file_stem(name)?;

        for extension in EXTENSIONS {
            let path = self.dir(base).join(format!("{stem}.{extension}"));
            if path.is_file() {
                if let Some(format) = Format::from_path(&path) {
                    return Some(self.entry_at(&path, format));
                }
            }
        }

        None
    }

    /// Ob eine Datei in dieser Sammlung überhaupt etwas bedeutet.
    ///
    /// Bei der Einschlafmusik zählt jeder Klang, bei Klingeltönen nur einer, der
    /// nach dem Schema `ringtone-<Rufnummer>` benannt ist. Eine Datei ohne
    /// Rufnummer im Namen klingelt für niemanden; sie aufzuführen hieße, sie sei
    /// löschbar, obwohl das Telefon sie nie findet - sie bleibt deshalb liegen,
    /// bis jemand sie im Verzeichnis entfernt.
    fn counted(self, path: &Path) -> bool {
        match self {
            Pool::Ringtones => path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| ringtone_caller(name).is_some()),
            Pool::Sleep => true,
        }
    }

    /// `bytes` unter `name` ablegen, einen vorhandenen Klang ersetzend.
    ///
    /// Der Name kommt vom Aufruher - im Webinterface der Name der Datei, die
    /// ausgewählt wurde - und wird auf das gesucht, was in dieser Sammlung
    /// etwas bedeutet ([`stem_for`]). Die Endung entscheidet der Inhalt, damit
    /// die Vorschau den richtigen Content-Type bekommt.
    pub fn store(self, base: &Path, name: &str, bytes: &[u8]) -> Result<Entry, StoreError> {
        let stem = self.stem_for(name)?;
        let format = Format::detect(bytes).ok_or(StoreError::Unsupported)?;

        let dir = self.dir(base);
        fs::create_dir_all(&dir).map_err(|e| StoreError::Io(format!(
            "Verzeichnis {} nicht anlegbar: {}",
            dir.display(),
            e
        )))?;

        let target = dir.join(format!("{stem}.{}", format.extension()));
        write_atomically(&target, bytes)?;

        // Derselbe Name kann nicht zwei Endungen haben: eine Datei, die nicht
        // die gerade geschriebene ist, ist die falsche.
        for extension in EXTENSIONS {
            let other = dir.join(format!("{stem}.{extension}"));
            if other != target {
                let _ = fs::remove_file(other);
            }
        }

        Ok(self.entry_at(&target, format))
    }

    /// Der Name, unter dem eine Datei dieser Sammlung abgelegt wird.
    ///
    /// Bei der Einschlafmusik ist es der Name, den die Datei mitbringt, auf das
    /// beschränkt, was in einem Dateinamen stehen darf ([`file_stem`]). Bei
    /// Klingeltönen ist es die Rufnummer, die darin steht ([`ringtone_name`]) -
    /// der Name, den der Browser meldet, entscheidet nichts, sonst hinge ein
    /// Klingelton an einer Rufnummer, die jemand zufällig im Dateinamen gefunden
    /// hat.
    fn stem_for(self, name: &str) -> Result<String, StoreError> {
        match self {
            Pool::Ringtones => ringtone_name(name),
            Pool::Sleep => file_stem(name).ok_or(StoreError::Name),
        }
    }

    /// Eine Datei dieser Sammlung wieder entfernen.
    pub fn delete(self, base: &Path, name: &str) -> Result<(), StoreError> {
        let entry = self.entry(base, name).ok_or(StoreError::Missing)?;

        fs::remove_file(&entry.path)
            .map_err(|e| StoreError::Io(format!("Datei nicht löschbar: {}", e)))
    }

    /// Die Datei zum Ausliefern, mit ihrem Format.
    pub fn load(self, base: &Path, name: &str) -> Result<(PathBuf, Format), LoadError> {
        let entry = self.entry(base, name).ok_or(LoadError::Missing)?;
        Ok((entry.path, entry.format))
    }

    /// Die Sammlung, wie die Oberfläche sie anzeigt.
    pub fn describe(self, base: &Path) -> Json {
        let files: Vec<Json> = self.entries(base).iter().map(Entry::describe).collect();

        json!({
            "key": self.spec().key,
            "label": self.spec().label,
            "help": self.spec().help,
            "numbered": self.spec().numbered,
            "files": files,
        })
    }

    /// Eine bekannte Datei als Eintrag lesen.
    fn entry_at(self, path: &Path, format: Format) -> Entry {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Nur bei Klingeltönen sagt der Name etwas über den Anrufer; bei der
        // Einschlafmusik ist er nur ein Name.
        let caller = match self {
            Pool::Ringtones => ringtone_caller(&name),
            Pool::Sleep => None,
        };

        Entry { name, path: path.to_path_buf(), format, caller }
    }
}

/// Eine hochgeladene Datei in einer der Sammlungen.
#[derive(Debug)]
pub struct Entry {
    /// Der Name im Verzeichnis, mit Endung.
    pub name: String,
    /// Wo die Datei liegt.
    pub path: PathBuf,
    /// Das Format, das ihre Endung verspricht.
    pub format: Format,
    /// Die Rufnummer, für die diese Klingelton-Datei gedacht ist.
    ///
    /// Bei Klingeltönen steht hier immer eine: nur Dateien mit einer Rufnummer im
    /// Namen kommen überhaupt in die Sammlung. Bei der Einschlafmusik ist es
    /// `None`, weil ihr Name nichts über den Anrufer sagt.
    pub caller: Option<String>,
}

impl Entry {
    /// Die Größe der Datei in Bytes.
    pub fn size(&self) -> u64 {
        fs::metadata(&self.path).map(|meta| meta.len()).unwrap_or(0)
    }

    /// Der Eintrag, wie die Oberfläche ihn anzeigt.
    pub fn describe(&self) -> Json {
        json!({
            "name": self.name,
            "size": self.size(),
            "content_type": self.format.content_type(),
            "caller": self.caller,
        })
    }
}

/// Warum eine Datei nicht abgelegt werden konnte.
#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// Die Datei ist kein Klang, den das Telefon abspielen kann.
    Unsupported,
    /// Der Name ist für einen Dateinamen unbrauchbar.
    Name,
    /// Der Name nennt keine Rufnummer, für die der Klingelton gedacht ist.
    Caller,
    /// Die Datei, die entfernt werden sollte, gibt es nicht.
    Missing,
    /// Das Verzeichnis oder die Datei ist nicht schreibbar.
    Io(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Unsupported => write!(
                f,
                "Datei ist weder WAV noch MP3. Es werden nur diese beiden Formate abgespielt."
            ),
            StoreError::Name => write!(
                f,
                "Der Dateiname ist unbrauchbar. Er darf aus Buchstaben, Ziffern, \
                 Punkt, Bindestrich und Unterstrich bestehen und höchstens {} Zeichen lang sein.",
                MAX_NAME
            ),
            StoreError::Caller => write!(
                f,
                "Ein Klingelton muss für eine Rufnummer gedacht sein. Der Name muss sie \
                 enthalten, etwa {}08912345.",
                RINGTONE_PREFIX
            ),
            StoreError::Missing => write!(f, "Diese Datei gibt es nicht."),
            StoreError::Io(message) => write!(f, "{}", message),
        }
    }
}

/// Warum eine Klangdatei nicht ausgeliefert werden kann.
#[derive(Debug, PartialEq, Eq)]
pub enum LoadError {
    /// Den Klang oder die Sammlung gibt es nicht.
    Unknown,
    /// Der Klang ist vorhanden, aber es wurde noch keine Datei hochgeladen.
    Missing,
}

/// Der Name, unter dem eine Datei abgelegt werden darf, ohne Endung.
///
/// Von dem Namen, den der Browser meldet, bleibt nur der letzte Pfadanteil -
/// `C:\fakepath\Ring 01.wav` ist `Ring 01.wav`, und `../../etc/passwd` ist
/// `passwd`. Alles, was in einem Dateinamen nichts zu suchen hat, wird zu einem
/// Bindestrich, und was danach übrig bliebe oder länger als [`MAX_NAME`] wäre,
/// wird abgelehnt: ein Name, aus dem beliebiges entstehen kann, ist keiner.
///
/// Die Endung ist nicht Teil des Ergebnisses - die legt der Inhalt der Datei
/// fest ([`Format`]).
fn file_stem(name: &str) -> Option<String> {
    // Beide Trennzeichen, nicht nur das dieses Systems: ein Name kann von einem
    // Windows-Browser kommen (`C:\fakepath\Ring 01.wav`), und dessen
    // Backslashes blieben sonst als Bindestriche mitten im Namen stehen.
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let stem = Path::new(last).file_stem()?.to_str()?;

    let mut cleaned = String::with_capacity(stem.len());
    let mut dash = true; // Am Anfang steht kein Bindestrich, auch nicht leer.
    for character in stem.chars() {
        let keep = character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_');
        if keep {
            if character == '-' && dash {
                continue;
            }
            cleaned.push(character);
            dash = character == '-';
        } else if !dash {
            cleaned.push('-');
            dash = true;
        }
    }

    let cleaned = cleaned.trim_matches(|c| matches!(c, '-' | '.'));
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    if cleaned.len() > MAX_NAME {
        return None;
    }

    Some(cleaned.to_string())
}

/// Die Rufnummer, an die ein Klingelton gebunden ist.
///
/// Nur Dateien namens `ringtone-<Ziffern>` sind an eine Rufnummer gebunden; ein
/// Name wie `ring-01` ist das nicht, sonst hinge der Klingelton plötzlich an der
/// Rufnummer "01". Bei `Ringtone-` ist die Groß- und Kleinschreibung egal: wer
/// sich beim Eintippen vertut, soll nicht stillschweigend einen Klingelton für
/// alle Anrufe bekommen, wo einer für eine Rufnummer gemeint war. Verglichen
/// werden nur die Ziffern, weil dieselbe Nummer je nach Wählweise als
/// `+49891234` oder `0891234` dasteht.
fn ringtone_caller(name: &str) -> Option<String> {
    let stem = Path::new(name).file_stem()?.to_str()?;

    // `get` statt eines directen Schnitts: ein Name, in dem die Stelle kein
    // Zeichenanfang ist, ist hier schon kein Name mit diesem Präfix.
    let prefix = stem.get(..RINGTONE_PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(RINGTONE_PREFIX) {
        return None;
    }
    let number = &stem[RINGTONE_PREFIX.len()..];

    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(number.to_string())
}

/// Der Name, unter dem ein Klingelton für eine Rufnummer abgelegt wird.
///
/// Übrig bleibt `ringtone-<Rufnummer>`, und zwar aus genau den Ziffern, die der
/// Name enthält: das Präfix ist optional, ein Leerzeichen in der Nummer stört
/// nicht, und was keine Ziffer ist, fällt weg. Damit ist der Name, unter dem die
/// Datei liegt, derselbe, unter dem [`ringtone_caller`] sie wiederfindet - der
/// Name, den der Browser meldet, entscheidet nichts, sonst hinge ein Klingelton
/// an einer Rufnummer, die jemand zufällig im Dateinamen gefunden hat.
///
/// Ohne Ziffer ist es kein Klingelton für jemanden und wird abgelehnt, statt
/// unter einem Namen abgelegt zu werden, den das Telefon nicht findet.
fn ringtone_name(name: &str) -> Result<String, StoreError> {
    let stem = file_stem(name).ok_or(StoreError::Name)?;

    let number = match stem.get(..RINGTONE_PREFIX.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(RINGTONE_PREFIX) => digits(&stem[RINGTONE_PREFIX.len()..]),
        _ => digits(&stem),
    };

    if number.is_empty() || number.len() > MAX_NAME - RINGTONE_PREFIX.len() {
        return Err(StoreError::Caller);
    }

    Ok(format!("{RINGTONE_PREFIX}{number}"))
}

/// Die Ziffern einer Rufnummer, wie sie im Dateinamen stehen.
fn digits(number: &str) -> String {
    number.chars().filter(char::is_ascii_digit).collect()
}

/// Der Klingelton für einen Anruf von `caller`, wenn es einen gibt.
///
/// In dieser Reihenfolge: die Datei, die für diese Rufnummer hochgeladen wurde,
/// und dann der feste Klingelton. Bleibt beides aus, klingelt das Telefon nicht -
/// eine Datei, die keiner Rufnummer zugeordnet ist, gibt es in dieser Sammlung
/// gar nicht, und ein Klingelton einer fremden Nummer ist kein Ersatz dafür.
pub fn ringtone_for(base: &Path, caller: &str) -> Option<PathBuf> {
    let number = digits(caller);

    if !number.is_empty() {
        let bound = Pool::Ringtones
            .entries(base)
            .into_iter()
            .find(|entry| entry.caller.as_deref() == Some(number.as_str()));
        if let Some(entry) = bound {
            return Some(entry.path);
        }
    }

    Sound::Ringtone.file(base)
}

/// Die Titel der Einschlafmusik, in der Reihenfolge des Verzeichnisses.
///
/// Das Mischen macht das Telefon, weil es hier nur um die Dateien geht: die
/// Reihenfolge, in der sie hochgeladen wurden, sagt nichts darüber, in welcher
/// Reihenfolge jemand einschlafen möchte.
pub fn sleep_music(base: &Path) -> Vec<PathBuf> {
    Pool::Sleep
        .entries(base)
        .into_iter()
        .map(|entry| entry.path)
        .collect()
}

/// Alles, was die Oberfläche über die Klänge wissen muss: die festen Klänge
/// und die Sammlungen, in der Reihenfolge, in der sie angezeigt werden.
pub fn describe(base: &Path) -> Json {
    let fixed: Vec<Json> = Sound::ALL
        .into_iter()
        .map(|sound| sound.spec().describe(base))
        .collect();

    let pools: Vec<Json> = Pool::ALL.into_iter().map(|pool| pool.describe(base)).collect();

    json!({ "fixed": fixed, "pools": pools })
}

/// Der feste Klang mit diesem Namen, wie die Oberfläche ihn anzeigt.
pub fn spec(key: &str) -> Option<&'static Spec> {
    Sound::from_key(key).map(|sound| sound.spec())
}

/// Die Datei eines festen Klangs zum Ausliefern, mit ihrem Format.
///
/// Der Klang wird gelesen, nicht die ganze Datei: die ist mehrere Megabyte
/// groß, und für den Content-Type reicht der Name.
pub fn load(base: &Path, key: &str) -> Result<(PathBuf, Format), LoadError> {
    let spec = spec(key).ok_or(LoadError::Unknown)?;
    let path = spec.file(base).ok_or(LoadError::Missing)?;
    let format = Format::from_path(&path).ok_or(LoadError::Missing)?;
    Ok((path, format))
}

/// `bytes` in `target` schreiben, ohne den alten Inhalt anzufassen.
///
/// Erst über eine temporäre Datei, die im letzten Augenblick an ihren Platz
/// kommt: bricht der Vorgang ab, ist die alte Datei noch da und nicht halb die
/// neue.
fn write_atomically(target: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let temporary = temporary_path(target);

    fs::write(&temporary, bytes)
        .map_err(|e| StoreError::Io(format!("Datei nicht schreibbar: {}", e)))?;

    if let Err(e) = fs::rename(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(StoreError::Io(format!("Datei nicht schreibbar: {}", e)));
    }

    Ok(())
}

/// Pfad der temporären Datei, über die eine Datei geschrieben wird.
///
/// Der Name trägt Prozess und Zähler, weil zwei gleichnamige temporäre Dateien
/// sich beim [`fs::rename`] gegenseitig die Zieldatei wegschnappen würden.
fn temporary_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    let unique = format!("{}.{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{unique}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav() -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes
    }

    fn mp3() -> Vec<u8> {
        b"\xff\xfb\x90\x00rest".to_vec()
    }

    /// The names of the files in a collection, in the order they are played in.
    fn names(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_keys_round_trip() {
        for sound in Sound::ALL {
            assert_eq!(Sound::from_key(sound.spec().key), Some(sound));
        }
        assert_eq!(Sound::from_key("gibtsnicht"), None);
        assert_eq!(Sound::from_key(""), None);

        for pool in Pool::ALL {
            assert_eq!(Pool::from_key(pool.spec().key), Some(pool));
        }
        assert_eq!(Pool::from_key("gibtsnicht"), None);
    }

    #[test]
    fn no_name_is_used_twice() {
        // Zwei Klänge mit demselben `key` würden unabhängig voneinander
        // geschrieben und der letzte gewönne; dieselbe Endung hieße zusätzlich
        // dieselbe Datei, und einer von beiden wäre nie wieder zu finden. Ein
        // fester Klang, dessen Name dem einer Sammlung entspricht, geriete
        // zusätzlich in deren Verzeichnis.
        for sound in Sound::ALL {
            assert!(Pool::from_key(sound.spec().key).is_none());
            for other in Sound::ALL {
                if sound == other {
                    continue;
                }
                assert_ne!(sound.spec().key, other.spec().key);
                assert_ne!(sound.spec().stem, other.spec().stem);
            }
        }

        for pool in Pool::ALL {
            assert!(Sound::from_key(pool.spec().key).is_none());
            assert!(Sound::from_key(pool.spec().dir).is_none());
            for other in Pool::ALL {
                if pool == other {
                    continue;
                }
                assert_ne!(pool.spec().key, other.spec().key);
                assert_ne!(pool.spec().dir, other.spec().dir);
            }
        }
    }

    #[test]
    fn formats_are_recognised_by_their_content() {
        assert_eq!(Format::detect(&wav()), Some(Format::Wav));
        assert_eq!(Format::detect(&mp3()), Some(Format::Mp3));
        assert_eq!(Format::detect(b"ID3\x04rest"), Some(Format::Mp3));
        // Eine Textdatei mit der Endung .wav ist keine Klangdatei.
        assert_eq!(Format::detect(b"RIFFnicht really"), None);
        assert_eq!(Format::detect(b"<html>"), None);
        assert_eq!(Format::detect(&[]), None);
    }

    #[test]
    fn a_sound_is_absent_until_it_is_uploaded() {
        let dir = tempfile::tempdir().unwrap();
        let sound = Sound::Ringtone;

        assert!(sound.file(dir.path()).is_none());
        assert_eq!(sound.path(dir.path()), "", "ohne Datei gibt es nichts abzuspielen");
        assert!(sound.spec().describe(dir.path())["present"] == false);

        sound.spec().store(dir.path(), &wav()).expect("speichern");

        assert_eq!(
            sound.file(dir.path()).unwrap().file_name().unwrap().to_string_lossy(),
            "ringtone.wav"
        );
        assert!(sound.path(dir.path()).ends_with("ringtone.wav"));
        assert!(sound.spec().describe(dir.path())["present"] == true);
    }

    #[test]
    fn the_extension_follows_the_upload() {
        let dir = tempfile::tempdir().unwrap();
        let spec = Sound::Boot.spec();

        let described = spec.store(dir.path(), &mp3()).expect("speichern");
        assert_eq!(described["file"], "boot.mp3");
        assert_eq!(described["content_type"], "audio/mpeg");

        // Ein zweiter Upload mit anderer Endung ersetzt den ersten, statt sich
        // daneben zu legen: von beiden kann nur einer der richtige sein.
        let described = spec.store(dir.path(), &wav()).expect("speichern");
        assert_eq!(described["file"], "boot.wav");
        assert!(!dir.path().join("boot.mp3").exists(), "alte Datei entfernt");
        assert_eq!(spec.store(dir.path(), &wav()).unwrap()["size"], wav().len() as u64);
    }

    #[test]
    fn a_file_that_is_no_sound_is_refused() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            Sound::Confirm.spec().store(dir.path(), b"<?xml version=\"1.0\"?>").unwrap_err(),
            StoreError::Unsupported
        );
        assert_eq!(
            Pool::Sleep.store(dir.path(), "track.wav", b"<?xml version=\"1.0\"?>").unwrap_err(),
            StoreError::Unsupported
        );
        assert!(names(dir.path()).is_empty(), "trotzdem eine Datei abgelegt");
    }

    #[test]
    fn the_directory_is_created_when_it_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("share/piphone/sounds");

        Sound::Boot.spec().store(&nested, &wav()).expect("speichern");
        Pool::Sleep.store(&nested, "track.wav", &wav()).expect("speichern");

        assert!(nested.join("boot.wav").is_file());
        assert!(nested.join("sleep/track.wav").is_file());
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        Sound::Boot.spec().store(dir.path(), &wav()).unwrap();
        Pool::Sleep.store(dir.path(), "track.wav", &wav()).unwrap();

        let left: Vec<String> = names(dir.path())
            .into_iter()
            .chain(names(&dir.path().join("sleep")))
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "temporäre Datei zurückgeblieben: {:?}", left);
    }

    #[test]
    fn loading_reports_what_is_wrong() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(load(dir.path(), "gibtsnicht"), Err(LoadError::Unknown));
        assert_eq!(load(dir.path(), "ringtone"), Err(LoadError::Missing));

        Sound::Ringtone.spec().store(dir.path(), &wav()).unwrap();
        let (path, format) = load(dir.path(), "ringtone").expect("geladen");
        assert_eq!(path.file_name().unwrap(), "ringtone.wav");
        assert_eq!(format, Format::Wav);
    }

    #[test]
    fn the_missing_ones_are_named() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Spec::missing(dir.path()).len(), Sound::ALL.len());

        Sound::Boot.spec().store(dir.path(), &wav()).unwrap();
        let missing: Vec<&str> = Spec::missing(dir.path()).iter().map(|s| s.key).collect();
        assert_eq!(missing.len(), Sound::ALL.len() - 1);
        assert!(!missing.contains(&"boot"), "hochgeladen, aber als fehlend gemeldet");
    }

    #[test]
    fn every_sound_is_described_for_the_page() {
        let dir = tempfile::tempdir().unwrap();
        Sound::Boot.spec().store(dir.path(), &wav()).unwrap();

        let described = describe(dir.path());
        let fixed = described["fixed"].as_array().expect("feste Klänge als Liste");
        assert_eq!(fixed.len(), Sound::ALL.len());

        for (sound, expected) in fixed.iter().zip(Sound::ALL) {
            assert_eq!(sound["key"], expected.spec().key);
            assert_eq!(sound["label"], expected.spec().label);
            assert!(!sound["help"].as_array().expect("Hilfe").is_empty(), "{}", expected.spec().key);
        }
        assert_eq!(fixed[0]["present"], true);
        assert_eq!(fixed[0]["file"], "boot.wav");
        assert_eq!(fixed[1]["present"], false);
        assert!(fixed[1]["file"].is_null());

        for pool in Pool::ALL {
            let described = described["pools"]
                .as_array()
                .expect("Sammlungen als Liste")
                .iter()
                .find(|entry| entry["key"] == pool.spec().key)
                .expect(pool.spec().key);

            assert_eq!(described["label"], pool.spec().label);
            assert!(described["files"].as_array().expect("Dateien").is_empty());
        }
    }

    // ------------------------------------------------------------------ Namen

    #[test]
    fn a_name_keeps_what_can_stand_in_a_filename() {
        assert_eq!(file_stem("Ring-01.wav").as_deref(), Some("Ring-01"));
        assert_eq!(file_stem("Brahms_Lullaby.mp3").as_deref(), Some("Brahms_Lullaby"));
        assert_eq!(file_stem("ringtone-0123456.wav").as_deref(), Some("ringtone-0123456"));
        // Keine Endung: der Name ist auch dann brauchbar.
        assert_eq!(file_stem("track").as_deref(), Some("track"));
        // Leerzeichen und Umlaute werden zu Bindestrichen, mehrfache nicht.
        assert_eq!(file_stem("Mein Klingelton.wav").as_deref(), Some("Mein-Klingelton"));
        assert_eq!(file_stem("über.wav").as_deref(), Some("ber"));
        assert_eq!(file_stem("a  b.wav").as_deref(), Some("a-b"));
    }

    #[test]
    fn a_name_cannot_point_outside_the_directory() {
        // Der Browser meldet je nach Betriebssystem einen vollständigen Pfad,
        // und ein Aufrufer kann alles schicken: übrig bleiben darf nur der
        // Name selbst.
        assert_eq!(file_stem(r"C:\fakepath\Ring 01.wav").as_deref(), Some("Ring-01"));
        assert_eq!(file_stem("/home/fox/Musik/ring.wav").as_deref(), Some("ring"));
        assert_eq!(file_stem("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(file_stem(".."), None);
        assert_eq!(file_stem("../.."), None);
        assert_eq!(file_stem("."), None);
        assert_eq!(file_stem(""), None);
        assert_eq!(file_stem("///"), None);
        // Nichts als Trennzeichen ist auch kein Name.
        assert_eq!(file_stem("----"), None);
        // Versteckte Dateien und Namen, die es sonst nirgends gibt.
        assert_eq!(file_stem(".hidden.wav").as_deref(), Some("hidden"));
        assert_eq!(file_stem(&format!("{}.wav", "a".repeat(MAX_NAME + 1))), None);
    }

    #[test]
    fn an_unusable_name_is_refused_rather_than_guessed() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            Pool::Ringtones.store(dir.path(), "..", &wav()).unwrap_err(),
            StoreError::Name
        );
        assert!(Pool::Ringtones.entries(dir.path()).is_empty(), "trotzdem abgelegt");
    }

    // -------------------------------------------------------------- Klingeltöne

    #[test]
    fn a_ringtone_is_named_after_the_number_in_its_name() {
        let dir = tempfile::tempdir().unwrap();

        let entry = Pool::Ringtones
            .store(dir.path(), "Ringtone-0123456.wav", &wav())
            .expect("speichern");
        assert_eq!(entry.name, "ringtone-0123456.wav");
        assert_eq!(entry.caller.as_deref(), Some("0123456"));

        // Die Endung folgt dem Inhalt, der Name nicht: derselbe Name, andere
        // Datei, und die alte ist weg.
        let entry = Pool::Ringtones
            .store(dir.path(), "Ringtone-0123456.wav", &mp3())
            .expect("speichern");
        assert_eq!(entry.name, "ringtone-0123456.mp3");
        assert_eq!(names(&dir.path().join("ringtones")), vec!["ringtone-0123456.mp3".to_string()]);

        assert_eq!(
            Pool::Ringtones.entry(dir.path(), "ringtone-0123456.wav").map(|e| e.name),
            Some("ringtone-0123456.mp3".to_string()),
            "wiedergefunden über den Namen, den die Rufnummer ergibt"
        );
    }

    #[test]
    fn only_a_ringtone_for_a_number_belongs_to_one() {
        assert_eq!(ringtone_caller("ringtone-0123456.wav").as_deref(), Some("0123456"));
        assert_eq!(ringtone_caller("Ringtone-0123456.MP3").as_deref(), Some("0123456"));
        // Kein Name, keine Rufnummer - sonst hinge "ring-01" an der "01".
        assert_eq!(ringtone_caller("ring-01.wav"), None);
        assert_eq!(ringtone_caller("ringtone-.wav"), None);
        assert_eq!(ringtone_caller("ringtone-0123-45.wav"), None);
        assert_eq!(ringtone_caller("ringtone-Brahms.wav"), None);
    }

    #[test]
    fn a_caller_gets_the_file_named_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        Sound::Ringtone.spec().store(base, &wav()).unwrap();
        Pool::Ringtones.store(base, "ringtone-08912345.wav", &wav()).unwrap();
        Pool::Ringtones.store(base, "ringtone-030123456.wav", &wav()).unwrap();

        assert!(ringtone_for(base, "08912345").unwrap().ends_with("ringtones/ringtone-08912345.wav"));
        // Eine andere Rufnummer bekommt nicht den Klang einer fremden - hier den
        // festen Klingelton.
        assert!(ringtone_for(base, "+4908912345").unwrap().ends_with("ringtone.wav"));
    }

    #[test]
    fn a_caller_without_a_file_of_their_own_gets_the_fixed_ringtone() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        // Weder eine Datei für diese Nummer noch der feste Klingelton: still.
        assert_eq!(ringtone_for(base, "030123456"), None);

        // Der Klingelton einer fremden Nummer ist kein Ersatz.
        Pool::Ringtones.store(base, "ringtone-0301234567.wav", &mp3()).unwrap();
        assert_eq!(ringtone_for(base, "08999999"), None);

        Sound::Ringtone.spec().store(base, &wav()).unwrap();
        assert!(ringtone_for(base, "08999999").unwrap().ends_with("ringtone.wav"));
        // Und die eigene Datei gewinnt gegen den festen Klingelton, auch wenn sie
        // als `.wav` hochgeladen wurde: die Endung sagt, was der Inhalt ist.
        assert!(ringtone_for(base, "0301234567").unwrap().ends_with("ringtones/ringtone-0301234567.mp3"));
    }

    #[test]
    fn a_deleted_ringtone_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        Pool::Ringtones.store(base, "ringtone-08912345.wav", &wav()).unwrap();

        Pool::Ringtones.delete(base, "ringtone-08912345.wav").expect("löschen");

        assert!(Pool::Ringtones.entries(base).is_empty());
        assert_eq!(ringtone_for(base, "08912345"), None);
        // Und ein zweites Löschen sagt, dass es die Datei nicht gibt, statt sie
        // als Fehler zu melden, den der Benutzer nicht auslösen kann.
        assert_eq!(
            Pool::Ringtones.delete(base, "ringtone-08912345.wav").unwrap_err(),
            StoreError::Missing
        );
    }

    // ------------------------------------------------------------ Einschlafmusik

    #[test]
    fn the_sleep_music_is_every_uploaded_track() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        assert!(sleep_music(base).is_empty(), "ohne Upload gibt es keine Musik");

        Pool::Sleep.store(base, "zweiter.wav", &wav()).unwrap();
        Pool::Sleep.store(base, "erster.mp3", &mp3()).unwrap();

        // Der Name entscheidet nichts über die Reihenfolge, das Verzeichnis
        // schon - gemischt wird später.
        let tracks: Vec<String> = sleep_music(base)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(tracks, vec!["erster.mp3".to_string(), "zweiter.wav".to_string()]);
    }

    #[test]
    fn only_a_sound_name_counts_as_a_sound() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        Pool::Sleep.store(base, " titel.wav", &wav()).unwrap();

        // Aufgeräumt wird nicht, und wer Dateien von Hand in das Verzeichnis legt,
        // soll sie nicht zerstören - eine Notiz zählt nur nicht mit, weil sie
        // keine Klangdatei heißt.
        fs::write(base.join("sleep/notizen.txt"), "Ringtone für Oma").unwrap();
        let tracks: Vec<String> = sleep_music(base)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(tracks, vec!["titel.wav".to_string()]);
        assert_eq!(names(&base.join("sleep")).len(), 2, "von Hand angelegte Dateien bleiben");
        // Und sie lassen sich auch nicht ausliefern.
        assert_eq!(Pool::Sleep.load(base, "notizen.txt").err(), Some(LoadError::Missing));

        // Eine Datei, die sich als Klang ausgibt, aber keiner ist, wird nicht
        // weggeworfen, sondern aufgeführt: sie zu sehen und zu löschen ist
        // besser, als zur Laufzeit an ihr zu scheitern. Der Inhalt wird nur beim
        // Hochladen geprüft.
        fs::write(base.join("sleep/kaputt.wav"), b"RIFF").unwrap();
        let tracks: Vec<String> = sleep_music(base)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(tracks, vec!["kaputt.wav".to_string(), "titel.wav".to_string()]);
    }
}