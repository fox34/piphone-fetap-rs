//! Schema der TOML-Konfigurationsdatei: Beschreibung aller Optionen, Prüfung und Schreiben.
//!
//! Die Weboberfläche (`webserver.rs`) nutzt dieses Modul, um die Konfiguration
//! anzuzeigen und zu speichern. Jede Option wird im TOML mit einem Kommentar
//! versehen, der aus diesem Schema erzeugt wird. Kommentare, die von Hand in der
//! Konfigurationsdatei stehen, werden beim Speichern nicht beibehalten - die Datei
//! enthält anschließend nur noch die Kommentare aus dem Schema.
//!
//! Aus demselben Grund werden Einträge, die das Schema nicht kennt, beim Speichern
//! entfernt: Die Datei entsteht vollständig aus dem Schema. Ein Eintrag, den keine
//! Version des Telefons liest, bleibt sonst als Datenmüll zurück und wird bei jedem
//! Speichern mitgeschleppt. [`unknown`] benennt, was beim Speichern verloren geht.

use regex::Regex;
use serde_json::{json, Map, Value as Json};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::LazyLock;
use toml::Value as Toml;

// --------------------------------------------------------------------------------------------
// Werte
// --------------------------------------------------------------------------------------------

/// Eine TOML-Tabelle: Schlüssel in stabiler (alphabetischer) Reihenfolge.
pub type Table = BTreeMap<String, Val>;

/// Konfigurationswert. Bildet die verschiedenen Typen ab, die aus einer TOML-Datei
/// und aus den JSON-Formularen des Webinterfaces ankommen können.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Val>),
    Table(Table),
}

impl Val {
    /// Leere Tabelle - ein fehlender oder kein Objekt enthaltender Abschnitt.
    pub fn empty_table() -> Val {
        Val::Table(Table::new())
    }

    /// Als Tabelle auffassen, wenn der Wert eine ist.
    pub fn as_table(&self) -> Option<&Table> {
        match self {
            Val::Table(t) => Some(t),
            _ => None,
        }
    }

    /// Zeichenkettenwert, sonst `""` (entspricht `str(None)` in der Oberfläche).
    pub fn as_str(&self) -> &str {
        match self {
            Val::Str(s) => s,
            _ => "",
        }
    }

    /// Ganzzahl, falls vorhanden.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Val::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Wahrheitswert, falls vorhanden.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Listeneinträge, falls vorhanden.
    pub fn as_list(&self) -> Option<&[Val]> {
        match self {
            Val::List(l) => Some(l),
            _ => None,
        }
    }

    /// Sichtbarer Text für Fehlermeldungen und Vergleiche.
    pub fn to_display_string(&self) -> String {
        match self {
            Val::Null => "None".to_string(),
            Val::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            Val::Int(i) => i.to_string(),
            Val::Float(f) => crate::config::fmt_float(*f),
            Val::Str(s) => s.clone(),
            Val::List(items) => {
                let parts: Vec<String> = items.iter().map(|i| i.to_display_string()).collect();
                format!("[{}]", parts.join(", "))
            }
            Val::Table(_) => String::from("{}"),
        }
    }
}

impl From<&Toml> for Val {
    fn from(value: &Toml) -> Self {
        match value {
            Toml::String(s) => Val::Str(s.clone()),
            Toml::Integer(i) => Val::Int(*i),
            Toml::Float(f) => Val::Float(*f),
            Toml::Boolean(b) => Val::Bool(*b),
            Toml::Datetime(d) => Val::Str(d.to_string()),
            Toml::Array(items) => Val::List(items.iter().map(Val::from).collect()),
            Toml::Table(table) => {
                Val::Table(table.iter().map(|(k, v)| (k.clone(), Val::from(v))).collect())
            }
        }
    }
}

impl From<&Json> for Val {
    fn from(value: &Json) -> Self {
        match value {
            Json::Null => Val::Null,
            Json::Bool(b) => Val::Bool(*b),
            Json::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Val::Int(i)
                } else {
                    Val::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            Json::String(s) => Val::Str(s.clone()),
            Json::Array(items) => Val::List(items.iter().map(Val::from).collect()),
            Json::Object(map) => {
                Val::Table(map.iter().map(|(k, v)| (k.clone(), Val::from(v))).collect())
            }
        }
    }
}

/// Konfigurationswert als JSON für die Weboberfläche.
pub fn to_json(value: &Val) -> Json {
    match value {
        Val::Null => Json::Null,
        Val::Bool(b) => Json::Bool(*b),
        Val::Int(i) => json!(i),
        Val::Float(f) => json!(f),
        Val::Str(s) => Json::String(s.clone()),
        Val::List(items) => Json::Array(items.iter().map(to_json).collect()),
        Val::Table(table) => {
            Json::Object(table.iter().map(|(k, v)| (k.clone(), to_json(v))).collect())
        }
    }
}

// --------------------------------------------------------------------------------------------
// Schema
// --------------------------------------------------------------------------------------------

/// Wie viele Ziffern eine Kurzwahl höchstens haben darf (längere Folgen bricht der Nummernschalter ab).
pub const MAX_DIAL_CODE: usize = 5;

const GPIO_PIN_MAX: f64 = 27.0;
const PORT_MAX: f64 = 65535.0;
const HOUR_MAX: f64 = 23.0;
const DUTY_MAX: f64 = 100.0;

// Rufnummern dürfen Ziffern, +, Leerzeichen und übliche Trennzeichen enthalten
const PHONE_PATTERN: &str = r"\+?[0-9][0-9 +()/.\-]*";
const TIME_PATTERN: &str = r"(?:[01]\d|2[0-3]):[0-5]\d";
const DIAL_CODE_PATTERN: &str = r"^\d{1,5}$";
const FILE_PATTERN: &str = r"^\S.*\S|\S$";
const BARE_KEY_PATTERN: &str = r"^[A-Za-z_][A-Za-z0-9_-]*$";

/// Compile a pattern once, on first use.
///
/// A macro rather than a function: `LazyLock` needs the pattern to live as long
/// as the cached regex, which a function argument cannot express.
macro_rules! regex {
    ($pattern:expr) => {
        LazyLock::new(|| Regex::new($pattern).expect("ungültiges Muster im Schema"))
    };
}

/// Typ einer Konfigurationsoption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    String,
    Int,
    Float,
    Bool,
    List,
}

impl ValueType {
    pub fn as_str(self) -> &'static str {
        match self {
            ValueType::String => "string",
            ValueType::Int => "int",
            ValueType::Float => "float",
            ValueType::Bool => "bool",
            ValueType::List => "list",
        }
    }
}

/// Eine einzelne Konfigurationsoption mit Typ, Standardwert und Kommentar.
#[derive(Debug)]
pub struct OptionSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub ty: ValueType,
    pub default: Option<Val>,
    pub comment: &'static [&'static str],
    /// Passwörter: in der Oberfläche verdeckt eingeben
    pub secret: bool,
    pub required: bool,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    /// Für `ValueType::List`: Muster je Eintrag
    pub item_pattern: Option<&'static str>,
    /// Für `ValueType::List`: feste Anzahl Einträge
    pub item_count: Option<usize>,
    /// Für `ValueType::List`: Beschriftung je Eintrag
    ///
    /// Gesetzt bekommt jeder Eintrag ein eigenes Eingabefeld mit eigener
    /// Beschriftung, statt in eine Zeile eines gemeinsamen Textfeldes zu
    /// wandern. Ohne Beschriftung bleibt es bei einer Zeile je Eintrag.
    pub item_labels: &'static [&'static str],
    /// Für `ValueType::List`: `type` der einzelnen Eingabefelder bei `item_labels`
    pub item_input: &'static str,
    pub item_hint: &'static str,
    /// Änderung wird erst nach einem Neustart wirksam
    pub restart: bool,
}

impl OptionSpec {
    pub fn new(key: &'static str, label: &'static str, ty: ValueType) -> Self {
        OptionSpec {
            key,
            label,
            ty,
            default: None,
            comment: &[],
            secret: false,
            required: false,
            minimum: None,
            maximum: None,
            item_pattern: None,
            item_count: None,
            item_labels: &[],
            item_input: "text",
            item_hint: "",
            restart: false,
        }
    }

    pub fn default(mut self, value: Val) -> Self {
        self.default = Some(value);
        self
    }

    pub fn comment(mut self, comment: &'static [&'static str]) -> Self {
        self.comment = comment;
        self
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    pub fn restart(mut self) -> Self {
        self.restart = true;
        self
    }

    pub fn range(mut self, minimum: Option<f64>, maximum: Option<f64>) -> Self {
        self.minimum = minimum;
        self.maximum = maximum;
        self
    }

    pub fn items(mut self, hint: &'static str) -> Self {
        self.item_hint = hint;
        self
    }

    /// Jeden Eintrag in ein eigenes Eingabefeld mit eigener Beschriftung setzen.
    ///
    /// `input` ist der `type` dieser Felder, etwa "time" für Uhrzeiten. Die
    /// Oberfläche liest die Felder in der Reihenfolge der Beschriftungen, die
    /// Reihenfolge im Formular ist also die Reihenfolge der Liste in der Datei.
    pub fn item_fields(mut self, labels: &'static [&'static str], input: &'static str) -> Self {
        self.item_labels = labels;
        self.item_input = input;
        self
    }

    pub fn item_pattern(mut self, pattern: &'static str, count: usize) -> Self {
        self.item_pattern = Some(pattern);
        self.item_count = Some(count);
        self
    }
}

/// Freie Schlüssel/Wert-Paare einer Tabelle, z.B. `[numbers]` oder `[ringtones]`.
#[derive(Debug)]
pub struct Entries {
    pub key_label: &'static str,
    pub value_label: &'static str,
    pub comment: &'static [&'static str],
    pub key_pattern: &'static str,
    pub value_pattern: Option<&'static str>,
    pub key_message: &'static str,
    pub value_message: &'static str,
    /// Vorschläge für den Wert (datalist)
    pub choices: &'static [&'static str],
}

/// Ein Abschnitt der Konfigurationsdatei.
#[derive(Debug)]
pub struct SectionSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub options: Vec<OptionSpec>,
    pub entries: Option<Entries>,
}

impl SectionSpec {
    fn options_section(name: &'static str, title: &'static str, options: Vec<OptionSpec>) -> Self {
        SectionSpec { name, title, options, entries: None }
    }
}

fn str_val(value: &str) -> Val {
    Val::Str(value.to_string())
}



const DIAL_CODE_MESSAGE: &str = "Kurzwahl muss aus 5 Ziffern bestehen.";

/// Reihenfolge bestimmt die Reihenfolge in der Weboberfläche und in der Datei.
pub static SECTIONS: LazyLock<Vec<SectionSpec>> = LazyLock::new(|| {
    vec![
        SectionSpec::options_section(
            "network",
            "Netzwerk",
            vec![OptionSpec::new("wifi_test_host", "Host für Verbindungstest", ValueType::String)
                .default(str_val("10.0.0.1"))
                .comment(&["Host, über dessen Erreichbarkeit die WLAN-Verbindung geprüft wird (Port 80)."])],
        ),
        SectionSpec::options_section(
            "sip",
            "SIP-Telefonie",
            vec![
                OptionSpec::new("host", "SIP-Server", ValueType::String)
                    .required()
                    .restart()
                    .comment(&["Adresse des SIP-Servers, bspw. die FRITZ!Box."]),
                OptionSpec::new("user", "SIP-Benutzer", ValueType::String)
                    .required()
                    .restart()
                    .comment(&["SIP-Benutzername."]),
                OptionSpec::new("pass", "SIP-Passwort", ValueType::String)
                    .required()
                    .secret()
                    .restart()
                    .comment(&["SIP-Passwort."]),
                OptionSpec::new("dial_timeout", "Wählvorgang Timeout (s)", ValueType::Int)
                    .default(Val::Int(60))
                    .range(Some(0.0), Some(3600.0))
                    .comment(&["Wie lange darf ein Wählvorgang dauern, bis er automatisch abgebrochen wird (Sekunden)"]),
                OptionSpec::new("dnd_from", "Klingelsperre ab (Stunde)", ValueType::Int)
                    .default(Val::Int(0))
                    .range(Some(0.0), Some(HOUR_MAX))
                    .comment(&["Klingelsperre ab welcher Stunde am Abend (0 = deaktiviert)."]),
                OptionSpec::new("dnd_to", "Klingelsperre bis (Stunde)", ValueType::Int)
                    .default(Val::Int(0))
                    .range(Some(0.0), Some(HOUR_MAX))
                    .comment(&["Klingelsperre bis zu welcher Stunde am Morgen (0 = deaktiviert)."]),
                OptionSpec::new("whitelist_active", "Whitelist aktiv", ValueType::Bool)
                    .default(Val::Bool(false))
                    .comment(&["Weist automatisch alle Anrufer ab, die nicht in [numbers] hinterlegt sind."]),
                OptionSpec::new("max_call_duration", "Max. Gesprächsdauer (min)", ValueType::Int)
                    .default(Val::Int(0))
                    .range(Some(0.0), Some(1440.0))
                    .comment(&["Beendet ausgehende Anrufe automatisch nach X Minuten (0 = deaktiviert)."]),
            ],
        ),
        SectionSpec::options_section(
            "pins",
            "GPIO-Pins",
            vec![
                OptionSpec::new("nsi", "Nummernschalter NSI", ValueType::Int)
                    .required()
                    .restart()
                    .range(Some(0.0), Some(GPIO_PIN_MAX))
                    .comment(&["Nummern-Schalter-Impuls-Kontakt."]),
                OptionSpec::new("nsa", "Nummernschalter NSA", ValueType::Int)
                    .required()
                    .restart()
                    .range(Some(0.0), Some(GPIO_PIN_MAX))
                    .comment(&["Nummern-Schalter-Arbeits- (oder Abschalte-)Kontakt."]),
                OptionSpec::new("gabel", "Gabelkontakt", ValueType::Int)
                    .required()
                    .restart()
                    .range(Some(0.0), Some(GPIO_PIN_MAX))
                    .comment(&["Gabelkontakt."]),
            ],
        ),
        SectionSpec {
            name: "numbers",
            title: "Kurzwahlen und Kurzbefehle",
            options: Vec::new(),
            entries: Some(Entries {
                key_label: "Kurzwahl",
                value_label: "Rufnummer oder Kurzbefehl",
                comment: &[
                    "Gültige Rufnummern (Kurzwahlen) oder Kurzbefehle.",
                    "Kurzbefehle: shutdown, reboot, enable-night-mode, play-sleep-music, test-loudspeaker, test-earpiece",
                ],
                key_pattern: DIAL_CODE_PATTERN,
                value_pattern: Some(PHONE_PATTERN),
                key_message: DIAL_CODE_MESSAGE,
                value_message: "Rufnummer angeben oder einen der Kurzbefehle wählen.",
                choices: &crate::command::DIAL_NAMES,
            }),
        },
        SectionSpec {
            name: "ringtones",
            title: "Klingeltöne",
            options: Vec::new(),
            entries: Some(Entries {
                key_label: "Rufnummer",
                value_label: "Klingelton-Datei",
                comment: &[
                    "Rufnummerspezifische Klingeltöne. Fehlt eine Rufnummer,",
                    "wird der generische Klingelton aus [sounds] verwendet.",
                ],
                key_pattern: PHONE_PATTERN,
                value_pattern: Some(FILE_PATTERN),
                key_message: "Siehe Kommentar",
                value_message: "Pfad zur Klangdatei angeben.",
                choices: &[],
            }),
        },
        SectionSpec::options_section(
            "sounds",
            "Klänge",
            vec![
                OptionSpec::new("boot", "Start", ValueType::String)
                    .default(str_val(""))
                    .comment(&["Klang beim Start des Telefons."]),
                OptionSpec::new("reboot", "Neustart", ValueType::String)
                    .default(str_val(""))
                    .comment(&["Klang vor einem Neustart."]),
                OptionSpec::new("shutdown", "Herunterfahren", ValueType::String)
                    .default(str_val(""))
                    .comment(&["Klang vor dem Herunterfahren."]),
                OptionSpec::new("ring", "Klingeln", ValueType::String)
                    .default(str_val(""))
                    .comment(&["Generischer Klingelton."]),
                OptionSpec::new("test_earpiece", "Test Hörer", ValueType::String)
                    .default(str_val(""))
                    .comment(&["Testton über den Hörer."]),
                OptionSpec::new("test_loud", "Test Lautsprecher", ValueType::String)
                    .default(str_val(""))
                    .comment(&["Testton über den Lautsprecher."]),
                OptionSpec::new("action_confirmed", "Bestätigung", ValueType::String)
                    .default(str_val(""))
                    .comment(&["Klang zur Bestätigung eines Kurzbefehls."]),
                OptionSpec::new("sleep_music", "Einschlafmusik", ValueType::List)
                    .default(Val::List(Vec::new()))
                    .items("Ein Pfad je Zeile")
                    .comment(&[
                        "Einschlafmusik: mehrere Dateien werden in zufälliger Reihenfolge abgespielt.",
                    ]),
            ],
        ),
        SectionSpec::options_section(
            "misc",
            "Nacht- und Aufwachlicht",
            vec![
                OptionSpec::new("night_light_pin", "Nachtlicht: GPIO-Pin", ValueType::Int)
                    .default(Val::Int(0))
                    .range(Some(0.0), Some(GPIO_PIN_MAX))
                    .restart()
                    .comment(&["GPIO-Pin für Nachtlicht (0 = deaktiviert)."]),
                OptionSpec::new("night_light_duty", "Nachtlicht: Helligkeit (%)", ValueType::Float)
                    .default(Val::Int(50))
                    .range(Some(0.0), Some(DUTY_MAX))
                    .restart()
                    .comment(&["Nachtlicht: Helligkeit in Prozent (100 = PWM deaktiviert, volle Leistung des GPIO)."]),
                OptionSpec::new("wake_light_pin", "Aufwachlicht: GPIO-Pin", ValueType::Int)
                    .default(Val::Int(0))
                    .range(Some(0.0), Some(GPIO_PIN_MAX))
                    .restart()
                    .comment(&["GPIO-Pin für Aufwach-Licht (0 = deaktiviert)."]),
                OptionSpec::new("wake_light_duty", "Aufwachlicht: Helligkeit (%)", ValueType::Float)
                    .default(Val::Int(100))
                    .range(Some(0.0), Some(DUTY_MAX))
                    .restart()
                    .comment(&["Aufwach-Licht: Helligkeit in Prozent (100 = PWM deaktiviert, volle Leistung des GPIO)."]),
                OptionSpec::new("wake_up_times", "Aufstehzeiten", ValueType::List)
                    .default(Val::List(Vec::new()))
                    .item_pattern(TIME_PATTERN, 7)
                    .item_fields(&["Montag", "Dienstag", "Mittwoch", "Donnerstag",
                                   "Freitag", "Samstag", "Sonntag"], "time")
                    .comment(&[
                        "Uhrzeiten zum Aufstehen: Mo, Di, Mi, Do, Fr, Sa, So (Lokalzeit, ggf. inkl. Sommerzeit).",
                        "Es werden genau 7 Uhrzeiten benötigt, sonst bleibt der Nachtmodus wirkungslos.",
                    ]),
            ],
        ),
        SectionSpec::options_section(
            "web",
            "Webserver",
            vec![
                OptionSpec::new("port", "Port", ValueType::Int)
                    .default(Val::Int(0))
                    .range(Some(0.0), Some(PORT_MAX))
                    .restart()
                    .comment(&["Port des Webservers, 0 = deaktiviert."]),
                OptionSpec::new("user", "Benutzer", ValueType::String)
                    .default(str_val(""))
                    .restart()
                    .comment(&[
                        "Optionaler Zugangsschutz (HTTP Basic Auth).",
                        "Achtung: Ohne Zugangsschutz kann jeder im WLAN das Telefon herunterfahren",
                        "und die Konfiguration verändern!",
                        "Leere Felder = kein Zugangsschutz",
                    ]),
                OptionSpec::new("pass", "Passwort", ValueType::String)
                    .default(str_val(""))
                    .secret()
                    .restart()
                    .comment(&["Zum Benutzerfeld gehörendes Passwort."]),
            ],
        ),
    ]
});

/// Fehler je Schlüssel `abschnitt.option`, in der Reihenfolge des Schemas.
///
/// Die Reihenfolge ist Teil des Verhaltens: die Weboberfläche zeigt die Fehler
/// in dieser Reihenfolge an, und ein sortierter Container würde sie alphabetisch
/// umsortieren und damit von der Reihenfolge im Formular weglafen.
#[derive(Debug, Clone, Default)]
pub struct Errors(Vec<(String, String)>);

impl Errors {
    pub fn new() -> Self {
        Errors(Vec::new())
    }

    /// A single problem that does not belong to any one option, such as a file
    /// that could not be written.
    pub fn from_message(message: String) -> Self {
        Errors(vec![("_".to_string(), message)])
    }

    /// Record a problem. The first message for a key wins, so the error a user
    /// sees is the one closest to the offending field.
    pub fn insert(&mut self, key: String, message: String) {
        if !self.0.iter().any(|(existing, _)| *existing == key) {
            self.0.push((key, message));
        }
    }

    /// The message recorded for a key, if there is one.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, message)| message.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The messages, keyed by `abschnitt.option`.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(key, message)| (key.as_str(), message.as_str()))
    }

    /// The keys in schema order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(key, _)| key.as_str())
    }
}

impl FromIterator<(String, String)> for Errors {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        let mut errors = Errors::new();
        for (key, message) in iter {
            errors.insert(key, message);
        }
        errors
    }
}

// --------------------------------------------------------------------------------------------
// TOML-Schreiben
// --------------------------------------------------------------------------------------------

const HEADER: [&str; 6] = [
    "# PiPhone-Konfiguration",
    "#",
    "# Diese Datei kann auch im Webinterface des Telefons bearbeitet werden.",
    "# Kommentare werden beim Speichern aus dem Schema neu erzeugt,",
    "# selbst geschriebene Kommentare werden dabei nicht beibehalten.",
    "# Einträge, die es im Schema nicht gibt, werden beim Speichern entfernt.",
];

/// TOML-Schlüssel: In Anführungszeichen, sofern kein einfacher Schlüssel möglich ist
pub fn toml_key(key: &str) -> String {
    if regex!(BARE_KEY_PATTERN).is_match(key) {
        key.to_string()
    } else {
        toml_string(key)
    }
}

/// Zeichenkette in TOML-Schreibweise
pub fn toml_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
    format!("\"{}\"", escaped)
}

/// Zahl wie Python `repr()` - ohne unnötige Nachkommastellen.
fn fmt_float(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e16 {
        format!("{:.1}", value)
    } else {
        format!("{}", value)
    }
}

/// Zahlen in der Form von Python `{:g}` für Fehlermeldungen.
fn fmt_g(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e16 {
        format!("{}", value as i64)
    } else {
        fmt_float(value)
    }
}

/// Beliebigen Konfigurationswert in TOML-Schreibweise
pub fn toml_value(value: &Val) -> String {
    match value {
        Val::Null => "\"\"".to_string(),
        Val::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Val::Int(i) => i.to_string(),
        Val::Float(f) => fmt_float(*f),
        Val::Str(s) => toml_string(s),
        Val::List(items) => {
            let parts: Vec<String> = items.iter().map(toml_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Val::Table(_) => String::from("{}"),
    }
}

/// Konfiguration mit Kommentaren als TOML-Text erzeugen
///
/// Es werden nur Abschnitte und Optionen aus [`SECTIONS`] geschrieben. Alles
/// andere in `data` fällt hier still weg - [`unknown`] sagt vorher, was es ist.
pub fn dump(data: &Table) -> String {
    let mut lines: Vec<String> = HEADER.iter().map(|l| l.to_string()).collect();

    for section in SECTIONS.iter() {
        let empty = Table::new();
        let values = data.get(section.name).and_then(|v| v.as_table()).unwrap_or(&empty);

        lines.push(String::new());
        lines.push(format!("[{}]", section.name));

        if let Some(entries) = &section.entries {
            for line in entries.comment {
                lines.push(format!("# {}", line));
            }
            for (key, value) in values {
                lines.push(format!("{} = {}", toml_key(key), toml_value(value)));
            }
        } else {
            for option in &section.options {
                for line in option.comment {
                    lines.push(format!("# {}", line));
                }
                let value = values.get(option.key).or(option.default.as_ref()).unwrap_or(&Val::Null);
                lines.push(format!("{} = {}", option.key, toml_value(value)));
            }
        }
    }

    lines.join("\n") + "\n"
}

/// Die Einträge einer Konfiguration, die das Schema nicht kennt.
///
/// Sie werden von [`dump`] nicht geschrieben, also beim Speichern entfernt. Das
/// ist der Fall für einen Abschnitt, den es nicht gibt, und für einen Schlüssel
/// in einem bekannten Abschnitt. In einer Eintragstabelle wie `[numbers]` gibt es
/// keine festen Schlüssel - jeder Eintrag dort ist eine Kurzwahl bzw. eine
/// Rufnummer, und ein unbekannter ist einfach ein unbekannter Kurzbefehl.
///
/// Die Reihenfolge ist alphabetisch nach Abschnitt, weil `data` eine
/// `BTreeMap` ist und die Meldung sonst von der Schreibweise der Datei abhinge.
pub fn unknown(data: &Table) -> Vec<String> {
    let mut found = Vec::new();

    for (name, value) in data {
        let Some(section) = SECTIONS.iter().find(|section| section.name == name) else {
            found.push(format!("[{}]", name));
            continue;
        };
        if section.entries.is_some() {
            continue;
        }

        for key in value.as_table().map(|t| t.keys()).into_iter().flatten() {
            if !section.options.iter().any(|option| option.key == key.as_str()) {
                found.push(format!("{}.{}", section.name, key));
            }
        }
    }

    found
}

// --------------------------------------------------------------------------------------------
// Lesen
// --------------------------------------------------------------------------------------------

/// Fehler beim Lesen der Konfigurationsdatei.
#[derive(Debug)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(value: std::io::Error) -> Self {
        ConfigError(value.to_string())
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(value: toml::de::Error) -> Self {
        ConfigError(value.to_string())
    }
}

/// TOML-Text in eine Konfigurationstabelle umwandeln
pub fn parse(text: &str) -> Result<Table, ConfigError> {
    let parsed: toml::Table = toml::from_str(text)?;
    Ok(parsed.iter().map(|(k, v)| (k.clone(), Val::from(v))).collect())
}

/// Konfigurationsdatei einlesen
pub fn read(path: &Path) -> Result<Table, ConfigError> {
    parse(&fs::read_to_string(path)?)
}

/// Pfad der temporären Datei, über die geschrieben wird.
///
/// Der Name ist einmalig, damit zwei Speichervorgänge sich nicht ins Gehege
/// kommen: Die Zieldatei wird erst am Ende ersetzt, und zwei gleichnamige
/// temporäre Dateien würden sich gegenseitig die Kontrolldatei wegschnappen und
/// am Ende eine von beiden halb geschrieben stehen lassen.
fn temporary_path(path: &Path) -> std::path::PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    // Einer für den Prozess, einer für den Aufruf: die echte Datei wird erst ganz
    // am Ende ersetzt, und zwei gleichnamige temporäre Dateien würden sich
    // gegenseitig die Kontrolldatei wegschnappen und eine davon halb geschrieben
    // zurücklassen. Zwei Tabs im Browser genügen, um das auszulösen.
    let unique = format!("{}.{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    path.with_file_name(format!("{name}.{unique}.tmp"))
}

/// Ergebnis eines Speichervorgangs.
pub struct Saved {
    /// Die geprüfte Konfiguration, wie sie jetzt in der Datei steht.
    pub cleaned: Table,
    /// Die Abschnitte, die sich gegenüber `previous` geändert haben.
    pub changed: Vec<String>,
    /// Die Einträge, die beim Schreiben aus der Datei verschwunden sind.
    ///
    /// Sie stehen in keinem Schema, das diese Version kennt - die Oberfläche
    /// zeigt sie nicht an, und niemand liest sie. Sie werden aber samt und sonders
    /// aus der Datei entfernt, und das gehört gesagt, sonst ist es ein stiller
    /// Datenverlust.
    pub dropped: Vec<String>,
}

/// Eine Konfiguration prüfen und in die Datei schreiben.
///
/// `previous` ist der Stand, gegen den die Änderungen gemessen werden; er ist der
/// letzte Stand aus der Datei, nicht der des laufenden Betriebs. Der laufende
/// Betrieb wird bewusst nicht verändert - die Änderungen wirken erst nach einem
/// Neustart.
///
/// Was das Schema nicht kennt, wird nicht geschrieben, auch dann nicht, wenn es in
/// der Datei stand (siehe [`unknown`]). Gemessen wird das an der Datei selbst, weil
/// genau sie überschrieben wird.
///
/// Geschrieben wird über eine temporäre Datei, die geschrieben, zurückgelesen
/// und verglichen wird, bevor sie die echte Datei ersetzt. So kann ein Fehler
/// die Konfiguration nicht beschädigen, und die temporäre Datei wird in jedem
/// Fall wieder entfernt.
pub fn save(
    path: &Path,
    incoming: &Table,
    previous: &Table,
) -> Result<Saved, Errors> {
    let (cleaned, errors) = validate(incoming);
    if !errors.is_empty() {
        return Err(errors);
    }

    let changed: Vec<String> = SECTIONS
        .iter()
        .filter(|section| cleaned.get(section.name) != previous.get(section.name))
        .map(|section| format!("[{}]", section.name))
        .collect();

    // Was beim Schreiben aus der Datei verschwindet, vorher feststellen: danach
    // steht es nirgends mehr, auch nicht in einem Protokoll.
    let dropped = read(path).map(|stored| unknown(&stored)).unwrap_or_default();

    let temporary = temporary_path(path);
    let write = fs::write(&temporary, dump(&cleaned))
        .map_err(|e| Errors::from_message(format!("Datei nicht schreibbar: {}", e)))
        .and_then(|()| {
            // Prüfen, ob die geschriebene Datei gültiges TOML ist und alle
            // Werte übernommen wurden.
            match read(&temporary) {
                Ok(reloaded) if reloaded == cleaned => fs::rename(&temporary, path)
                    .map_err(|e| Errors::from_message(format!("Datei nicht schreibbar: {}", e))),
                _ => Err(Errors::from_message(
                    "Kontrolle der geschriebenen Datei fehlgeschlagen.".to_string(),
                )),
            }
        });

    let _ = fs::remove_file(&temporary);

    write?;
    Ok(Saved { cleaned, changed, dropped })
}

/// JSON aus der Weboberfläche in eine Konfigurationstabelle umwandeln.
///
/// Die Oberfläche sendet jedes Eingabefeld als String; die Typen übernimmt
/// anschließend [`validate`] anhand des Schemas. Zahlen werden hier bewusst
/// schon erkannt, damit `50` nicht als `50.0` in der Datei landet.
pub fn from_json(value: &Json) -> Table {
    match value {
        Json::Object(map) => map
            .iter()
            .map(|(key, value)| (key.clone(), Val::from(value)))
            .collect(),
        _ => Table::new(),
    }
}

// --------------------------------------------------------------------------------------------
// Prüfen
// --------------------------------------------------------------------------------------------

/// Rohwert aus der Oberfläche in den konfigurierten Typ umwandeln.
/// Fehler: `Err(Klartextmeldung)`.
fn coerce(option: &OptionSpec, raw: Option<&Val>) -> Result<Val, String> {
    let raw = match raw {
        None | Some(Val::Null) => None,
        Some(Val::Str(s)) if s.trim().is_empty() => None,
        Some(value) => Some(value),
    };

    let raw = match raw {
        None => {
            // Leere Eingaben: Standardwert verwenden, sonst ablehnen
            if let Some(default) = &option.default {
                return Ok(default.clone());
            }
            if option.required {
                return Err("Pflichtangabe".to_string());
            }
            return Err("Wert fehlt".to_string());
        }
        Some(value) => value,
    };

    let value = match option.ty {
        ValueType::Bool => {
            let text = match raw {
                Val::Bool(b) => return Ok(Val::Bool(*b)),
                other => other.to_display_string(),
            };
            let text = text.trim().to_ascii_lowercase();
            match text.as_str() {
                "true" | "1" | "ja" | "yes" | "on" | "an" | "wahr" => Val::Bool(true),
                "false" | "0" | "nein" | "no" | "off" | "aus" | "falsch" => Val::Bool(false),
                _ => return Err("muss an oder aus sein".to_string()),
            }
        }

        ValueType::Int => {
            if matches!(raw, Val::Bool(_)) {
                return Err("muss eine ganze Zahl sein".to_string());
            }
            let text = raw.to_display_string();
            match text.trim().parse::<i64>() {
                Ok(number) => Val::Int(number),
                Err(_) => return Err("muss eine ganze Zahl sein".to_string()),
            }
        }

        ValueType::Float => {
            let text = raw.to_display_string();
            let text = text.trim().to_string();
            let number: f64 = match text.parse() {
                Ok(number) => number,
                Err(_) => return Err("muss eine Zahl sein".to_string()),
            };
            // Ganzzahlige Eingaben auch als Ganzzahl speichern, sonst wird 50 zu 50.0
            if text.contains('.') || text.contains('e') || text.contains('E') {
                Val::Float(number)
            } else {
                Val::Int(number as i64)
            }
        }

        ValueType::String => Val::Str(raw.to_display_string()),

        ValueType::List => {
            let items: Vec<String> = match raw {
                Val::Str(text) => text.split('\n').map(|s| s.to_string()).collect(),
                Val::List(items) => items.iter().map(|i| i.to_display_string()).collect(),
                _ => return Err("muss eine Liste sein".to_string()),
            };
            let items: Vec<String> = items
                .into_iter()
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect();

            if let Some(count) = option.item_count {
                if items.len() != count {
                    return Err(format!("muss genau {} Einträge enthalten", count));
                }
            }

            if let Some(pattern) = option.item_pattern {
                let re = Regex::new(pattern).expect("ungültiges Muster im Schema");
                for item in &items {
                    if !re.is_match(item) {
                        return Err("enthält einen ungültigen Eintrag".to_string());
                    }
                }
            }

            Val::List(items.into_iter().map(Val::Str).collect())
        }
    };

    // Grenzwerte nur für Zahlen
    if let Val::Int(_) | Val::Float(_) = &value {
        let number = match &value {
            Val::Int(i) => *i as f64,
            Val::Float(f) => *f,
            _ => unreachable!(),
        };
        if let Some(minimum) = option.minimum {
            if number < minimum {
                return Err(format!("darf nicht kleiner als {} sein", fmt_g(minimum)));
            }
        }
        if let Some(maximum) = option.maximum {
            if number > maximum {
                return Err(format!("darf nicht größer als {} sein", fmt_g(maximum)));
            }
        }
    }

    if option.required && matches!(&value, Val::Str(s) if s.is_empty()) {
        return Err("Pflichtangabe".to_string());
    }

    Ok(value)
}

/// Konfiguration prüfen und in eine vollständige, speicherbare Form bringen.
///
/// Fehlende Optionen erhalten ihren Standardwert. Was das Schema nicht kennt, wird
/// verworfen: unbekannte Optionen und Abschnitte stehen danach weder in der
/// geputzten Konfiguration noch in der aus ihr geschriebenen Datei, denn [`dump`]
/// erzeugt die Datei vollständig aus dem Schema. Liefert (bereinigte
/// Konfiguration, Fehler je Schlüssel `abschnitt.option`).
pub fn validate(data: &Table) -> (Table, Errors) {
    let empty = Table::new();

    let mut cleaned = Table::new();
    let mut errors = Errors::new();

    for section in SECTIONS.iter() {
        let source: &Table = match data.get(section.name).and_then(|v| v.as_table()) {
            Some(source) => source,
            None => {
                if data.contains_key(section.name) {
                    errors.insert(section.name.to_string(), "Abschnitt ist keine Tabelle.".to_string());
                }
                &empty
            }
        };

        let mut values = Table::new();

        if let Some(entries) = &section.entries {
            let key_re = Regex::new(entries.key_pattern).expect("ungültiges Muster im Schema");
            let value_re = entries
                .value_pattern
                .map(|pattern| Regex::new(pattern).expect("ungültiges Muster im Schema"));

            for (raw_key, raw_value) in source {
                let key = raw_key.trim().to_string();
                let value = raw_value.to_display_string().trim().to_string();

                if !key_re.is_match(&key) {
                    errors.insert(format!("{}.{}", section.name, key), entries.key_message.to_string());
                    continue;
                }

                let is_command = entries.choices.contains(&value.as_str());
                if let Some(value_re) = &value_re {
                    if !is_command && !value_re.is_match(&value) {
                        errors.insert(format!("{}.{}", section.name, key), entries.value_message.to_string());
                        continue;
                    }
                }

                if values.contains_key(&key) {
                    errors.insert(format!("{}.{}", section.name, key), "Doppelt belegt.".to_string());
                    continue;
                }

                values.insert(key, str_val(&value));
            }
        } else {
            for option in &section.options {
                match coerce(option, source.get(option.key)) {
                    Ok(value) => {
                        values.insert(option.key.to_string(), value);
                    }
                    Err(message) => {
                        errors.insert(format!("{}.{}", section.name, option.key), message);
                        let fallback = match &option.default {
                            Some(default) => default.clone(),
                            None if option.ty == ValueType::String => str_val(""),
                            None => Val::Null,
                        };
                        values.insert(option.key.to_string(), fallback);
                    }
                }
            }
        }

        cleaned.insert(section.name.to_string(), Val::Table(values));
    }

    (cleaned, errors)
}

// --------------------------------------------------------------------------------------------
// Beschreibung für die Weboberfläche
// --------------------------------------------------------------------------------------------

/// Schema und aktuelle Werte als JSON-serialisierbare Struktur für die Weboberfläche
pub fn describe(data: &Table) -> Json {
    let empty = Table::new();
    let mut description: Vec<Json> = Vec::new();

    for section in SECTIONS.iter() {
        let values = data.get(section.name).and_then(|v| v.as_table()).unwrap_or(&empty);

        if let Some(entries) = &section.entries {
            description.push(json!({
                "name": section.name,
                "title": section.title,
                "kind": "entries",
                "help": entries.comment,
                "key_label": entries.key_label,
                "value_label": entries.value_label,
                "choices": entries.choices,
                "entries": values
                    .iter()
                    .map(|(key, value)| json!({ "key": key, "value": value.to_display_string() }))
                    .collect::<Vec<Json>>(),
            }));
            continue;
        }

        let fields: Vec<Json> = section
            .options
            .iter()
            .map(|option| {
                let value = values.get(option.key).or(option.default.as_ref()).unwrap_or(&Val::Null);
                json!({
                    "key": option.key,
                    "label": option.label,
                    "type": option.ty.as_str(),
                    "value": to_json(value),
                    "help": option.comment,
                    "secret": option.secret,
                    "required": option.required,
                    // Limits are stored as floats so one field covers int and
                    // float options, but the form must see them in the option's
                    // own type - `<input type=number>` would step by 1.0.
                    "minimum": limit(option, option.minimum),
                    "maximum": limit(option, option.maximum),
                    "item_hint": option.item_hint,
                    // Beschriftet: ein Feld je Eintrag (Aufstehzeiten je
                    // Wochentag) statt einer Zeile je Eintrag im Textfeld.
                    "item_labels": option.item_labels,
                    "item_input": option.item_input,
                    "restart": option.restart,
                })
            })
            .collect();

        description.push(json!({
            "name": section.name,
            "title": section.title,
            "kind": "options",
            "fields": fields,
        }));
    }

    Json::Array(description)
}

/// Grenzwert in dem Typ der Option ausgeben.
///
/// Die Grenzen liegen im Schema als Fließkommazahl vor, weil dieselbe
/// Beschreibung sowohl ganze als auch gebrochene Optionen abdeckt. Für eine
/// Ganzzahloption muss aber auch die Grenze eine ganze Zahl sein, sonst zeigt
/// das Formularfeld `step=1.0` und den Wert `23.0`.
fn limit(option: &OptionSpec, value: Option<f64>) -> Json {
    match value {
        None => Json::Null,
        Some(value) if option.ty == ValueType::Int => Json::from(value as i64),
        Some(value) => json!(value),
    }
}

/// Bequemlichkeit: Option aus einer geprüften Konfiguration lesen.
pub fn option_int(data: &Table, section: &str, key: &str) -> i64 {
    data.get(section)
        .and_then(|v| v.as_table())
        .and_then(|t| t.get(key))
        .and_then(|v| v.as_int())
        .unwrap_or(0)
}

/// Bequemlichkeit: Zeichenkettenoption aus einer geprüften Konfiguration lesen.
pub fn option_str(data: &Table, section: &str, key: &str) -> String {
    data.get(section)
        .and_then(|v| v.as_table())
        .and_then(|t| t.get(key))
        .map(|v| v.to_display_string())
        .unwrap_or_default()
}

/// Bequemlichkeit: Helligkeit als Fließkommazahl lesen (Ganzzahlen werden akzeptiert).
pub fn option_float(data: &Table, section: &str, key: &str) -> f64 {
    data.get(section)
        .and_then(|v| v.as_table())
        .and_then(|t| t.get(key))
        .map(|v| match v {
            Val::Int(i) => *i as f64,
            Val::Float(f) => *f,
            _ => 0.0,
        })
        .unwrap_or(0.0)
}

/// Bequemlichkeit: bool-Option lesen.
pub fn option_bool(data: &Table, section: &str, key: &str) -> bool {
    data.get(section)
        .and_then(|v| v.as_table())
        .and_then(|t| t.get(key))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Bequemlichkeit: Liste von Zeichenketten lesen.
pub fn option_list(data: &Table, section: &str, key: &str) -> Vec<String> {
    data.get(section)
        .and_then(|v| v.as_table())
        .and_then(|t| t.get(key))
        .and_then(|v| v.as_list())
        .map(|items| items.iter().map(|i| i.to_display_string()).collect())
        .unwrap_or_default()
}

/// JSON-Objekt aus Schlüssel/Wert-Paaren (für kleine Antworten).
pub fn object(pairs: Vec<(&str, Json)>) -> Json {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert(key.to_string(), value);
    }
    Json::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Table {
        read(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/support/config-example.toml"))).unwrap()
    }

    #[test]
    fn roundtrip() {
        let original = example();
        let (cleaned, errors) = validate(&original);
        assert!(errors.is_empty(), "{:?}", errors);

        let text = dump(&cleaned);
        assert_eq!(parse(&text).unwrap(), cleaned);
        assert!(text.contains("# GPIO-Pin für Nachtlicht (0 = deaktiviert)."));
        assert!(text.contains("\"01\" = \"01234567\""));
        assert!(text.contains(
            "wake_up_times = [\"06:30\", \"06:30\", \"06:30\", \"06:30\", \"06:30\", \"07:30\", \"07:30\"]"
        ));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn labelled_lists_have_one_field_per_entry() {
        // Die Oberfläche baut aus den Beschriftungen genau so viele Eingabefelder,
        // wie die Option Einträge verlangt. Weichen die Zahlen ab, landen die
        // Werte an den Wochentagen, zu denen sie nicht gehören.
        for section in SECTIONS.iter() {
            for option in &section.options {
                if option.item_labels.is_empty() {
                    continue;
                }
                assert_eq!(
                    Some(option.item_labels.len()),
                    option.item_count,
                    "{}.{}: {} Beschriftungen für {:?} Einträge",
                    section.name, option.key, option.item_labels.len(), option.item_count
                );
            }
        }
    }

    #[test]
    fn wake_up_times_are_described_as_a_weekday_per_field() {
        let described = describe(&example());
        let misc = described
            .as_array()
            .expect("Abschnitte")
            .iter()
            .find(|s| s["name"] == "misc")
            .expect("Abschnitt misc");
        let wake = misc["fields"]
            .as_array()
            .expect("Felder")
            .iter()
            .find(|f| f["key"] == "wake_up_times")
            .expect("Aufstehzeiten");

        assert_eq!(wake["type"], "list");
        assert_eq!(wake["item_input"], "time");
        // Montage zuerst, weil der Wochentag in `schedule.rs` genauso gezählt wird
        assert_eq!(
            wake["item_labels"].as_array().expect("Beschriftungen"),
            &vec![
                json!("Montag"), json!("Dienstag"), json!("Mittwoch"), json!("Donnerstag"),
                json!("Freitag"), json!("Samstag"), json!("Sonntag")
            ]
        );
        assert_eq!(
            wake["value"].as_array().expect("Werte"),
            &vec![
                json!("06:30"), json!("06:30"), json!("06:30"), json!("06:30"),
                json!("06:30"), json!("07:30"), json!("07:30")
            ]
        );
    }

    #[test]
    fn section_order() {
        let text = dump(&example());
        let sections: Vec<&str> =
            text.lines().filter(|l| l.starts_with('[')).collect();
        assert_eq!(
            sections,
            vec!["[network]", "[sip]", "[pins]", "[numbers]", "[ringtones]", "[sounds]", "[misc]", "[web]"]
        );
    }

    #[test]
    fn defaults_and_required() {
        let (empty, errors) = validate(&Table::new());
        let expected: Vec<&str> = vec![
            "sip.host", "sip.user", "sip.pass", "pins.nsi", "pins.nsa", "pins.gabel",
        ];
        assert_eq!(errors.keys().collect::<Vec<_>>(), expected);

        assert_eq!(option_int(&empty, "sip", "dial_timeout"), 60);
        assert_eq!(option_int(&empty, "web", "port"), 0);
        assert!(!option_bool(&empty, "sip", "whitelist_active"));
        assert!(option_list(&empty, "misc", "wake_up_times").is_empty());
    }

    #[test]
    fn validation_errors() {
        let mut data = example();
        if let Some(Val::Table(sip)) = data.get_mut("sip") {
            sip.insert("host".to_string(), str_val(""));
        }
        if let Some(Val::Table(pins)) = data.get_mut("pins") {
            pins.insert("nsa".to_string(), Val::Int(28));
        }
        let (_, errors) = validate(&data);
        assert_eq!(errors.get("sip.host"), Some("Pflichtangabe"));
        assert_eq!(errors.get("pins.nsa"), Some("darf nicht größer als 27 sein"));
    }

    /// Die Datei entsteht vollständig aus dem Schema: was es nicht kennt, steht
    /// danach weder in der geprüften Konfiguration noch in der Datei - und wird
    /// benannt, damit der Verlust beim Speichern nicht still bleibt.
    #[test]
    fn unknown_values_are_dropped() {
        let (mut data, _) = validate(&example());
        if let Val::Table(sip) = data.get_mut("sip").unwrap() {
            sip.insert("zusatz".to_string(), Val::Int(42));
        }
        let mut special = Table::new();
        special.insert("x".to_string(), str_val("y"));
        data.insert("eigener_abschnitt".to_string(), Val::Table(special));

        assert_eq!(
            unknown(&data),
            vec!["[eigener_abschnitt]".to_string(), "sip.zusatz".to_string()]
        );

        let (cleaned, errors) = validate(&data);
        assert!(errors.is_empty());
        assert!(!cleaned.contains_key("eigener_abschnitt"), "unbekannter Abschnitt behalten");
        assert!(
            !cleaned["sip"].as_table().unwrap().contains_key("zusatz"),
            "unbekannte Option behalten"
        );

        let text = dump(&cleaned);
        assert!(!text.contains("zusatz"), "unbekannte Option in der Datei");
        assert!(!text.contains("eigener_abschnitt"), "unbekannter Abschnitt in der Datei");
        assert_eq!(parse(&text).unwrap(), cleaned);
    }

    /// Kurzwahlen und Rufnummern haben keine festen Schlüssel: dort ist ein
    /// unbekannter Eintrag einfach ein unbekannter Kurzbefehl, kein Unbekannter.
    #[test]
    fn entry_tables_have_no_unknown_keys() {
        let (mut data, _) = validate(&example());
        if let Val::Table(numbers) = data.get_mut("numbers").unwrap() {
            numbers.insert("42".to_string(), str_val("01299999"));
        }

        assert!(unknown(&data).is_empty());
        assert!(dump(&validate(&data).0).contains("\"42\" = \"01299999\""));
    }
}
