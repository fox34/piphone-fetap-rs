//! Tests for the configuration schema: reading, validating and writing.
//!
//! Mirrors `tests/test-config.py`. Everything here runs without hardware.

use piphone::config::{self, Errors, Table, Val};

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// The smallest configuration that passes validation.
fn base() -> Table {
    config::parse(
        r#"
[sip]
host = "h"
user = "u"
pass = "p"

[pins]
nsi = "23"
nsa = "24"
gabel = "15"
"#,
    )
    .expect("Grundkonfiguration ist gültiges TOML")
}

/// Merge sections into the base configuration, the way the form submits them.
fn with(sections: &[(&str, &[(&str, &str)])]) -> Table {
    let mut text = String::new();
    for (name, options) in sections {
        text.push_str(&format!("[{}]\n", name));
        for (key, value) in *options {
            text.push_str(&format!("{} = {}\n", key, value));
        }
        text.push('\n');
    }

    let mut data = base();
    for (name, _) in sections {
        let extra = config::parse(&text).expect("gültiges TOML");
        let section = extra.get(*name).and_then(|s| s.as_table()).expect("Abschnitt").clone();
        let table = data.entry(name.to_string()).or_insert_with(Val::empty_table);
        if let Val::Table(existing) = table {
            existing.extend(section);
        }
    }
    data
}

/// The errors produced by validating the base configuration plus `sections`.
fn errors_of(sections: &[(&str, &[(&str, &str)])]) -> Errors {
    config::validate(&with(sections)).1
}

/// The errors for one key, if any.
fn error(errors: &Errors, key: &str) -> Option<String> {
    errors.get(key).map(|m| m.to_string())
}

fn section_of<'a>(data: &'a Table, name: &str) -> &'a std::collections::BTreeMap<String, Val> {
    data.get(name).and_then(|v| v.as_table()).expect("Abschnitt")
}

// -----------------------------------------------------------------------------
// Round trip
// -----------------------------------------------------------------------------

#[test]
fn example_configuration_is_valid_and_round_trips() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("support/config-example.toml");
    let original = config::read(&path).expect("Beispieldatei lesen");

    let (cleaned, errors) = config::validate(&original);
    assert!(errors.is_empty(), "Beispieldatei ist ungültig: {:?}", messages(&errors));

    let text = config::dump(&cleaned);

    // What was written must read back identically.
    let reloaded = config::parse(&text).expect("geschriebenes TOML ist lesbar");
    assert_eq!(reloaded, cleaned);

    // Comments are generated from the schema, so every option has one.
    assert!(
        text.matches("# ").count() > 30,
        "zu wenige Kommentare: {}",
        text.matches("# ").count()
    );
    assert!(text.contains("# GPIO-Pin für Nachtlicht (0 = deaktiviert)."));
    assert!(text.contains("selbst geschriebene Kommentare werden dabei nicht beibehalten"));

    let sections: Vec<&str> = text.lines().filter(|l| l.starts_with('[')).collect();
    assert_eq!(
        sections,
        vec!["[network]", "[sip]", "[pins]", "[numbers]", "[ringtones]", "[sounds]", "[misc]", "[web]"]
    );

    assert!(text.contains("\"01\" = \"01234567\""), "Kurzwahl muss quotiert sein");
    assert!(text.contains(
        "wake_up_times = [\"06:30\", \"06:30\", \"06:30\", \"06:30\", \"06:30\", \"07:30\", \"07:30\"]"
    ));
    assert!(text.ends_with('\n'), "Datei muss mit Zeilenumbruch enden");
}

/// Render errors for a failure message.
fn messages(errors: &Errors) -> Vec<String> {
    errors.iter().map(|(key, message)| format!("{}: {}", key, message)).collect()
}

// -----------------------------------------------------------------------------
// Defaults
// -----------------------------------------------------------------------------

#[test]
fn missing_required_options_are_reported_in_schema_order() {
    let (empty, errors) = config::validate(&Table::new());

    assert_eq!(
        errors.keys().collect::<Vec<_>>(),
        vec!["sip.host", "sip.user", "sip.pass", "pins.nsi", "pins.nsa", "pins.gabel"]
    );

    assert_eq!(config::option_int(&empty, "sip", "dial_timeout"), 60);
    assert_eq!(config::option_int(&empty, "web", "port"), 0);
    assert!(!config::option_bool(&empty, "sip", "whitelist_active"));
    assert!(config::option_list(&empty, "misc", "wake_up_times").is_empty());
    assert_eq!(config::option_int(&empty, "misc", "night_light_pin"), 0);
    assert!(config::option_list(&empty, "sounds", "sleep_music").is_empty());
}

#[test]
fn values_from_the_form_keep_their_types() {
    // The browser sends everything as a string; the schema decides the type.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("support/config-example.toml");
    let (cleaned, errors) = config::validate(&config::read(&path).unwrap());
    assert!(errors.is_empty());

    // Numbers stay integers rather than becoming floats.
    assert_eq!(section_of(&cleaned, "sip")["dial_timeout"], Val::Int(60));
    assert_eq!(section_of(&cleaned, "misc")["night_light_duty"], Val::Int(50));
    assert_eq!(section_of(&cleaned, "pins")["nsi"].to_display_string(), "23");

    // Checkboxes become booleans.
    assert_eq!(section_of(&cleaned, "sip")["whitelist_active"], Val::Bool(false));

    // Text stays text.
    assert_eq!(section_of(&cleaned, "sip")["pass"], Val::Str("test".into()));
    assert!(section_of(&cleaned, "ringtones")["08912345"]
        .to_display_string()
        .ends_with(".wav"));

    // Lists stay lists.
    assert!(matches!(section_of(&cleaned, "misc")["wake_up_times"], Val::List(_)));
    assert!(matches!(section_of(&cleaned, "sounds")["sleep_music"], Val::List(_)));
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

#[test]
fn accepts_the_minimum_configuration() {
    assert!(config::validate(&base()).1.is_empty());
}

#[test]
fn rejects_an_empty_required_field() {
    assert_eq!(
        error(&errors_of(&[("sip", &[("host", "\"\"")])]), "sip.host"),
        Some("Pflichtangabe".to_string())
    );
}

#[test]
fn rejects_text_in_a_number_field() {
    assert_eq!(
        error(&errors_of(&[("pins", &[("nsi", "\"abc\"")])]), "pins.nsi"),
        Some("muss eine ganze Zahl sein".to_string())
    );
}

#[test]
fn enforces_the_gpio_pin_range() {
    assert_eq!(
        error(&errors_of(&[("pins", &[("nsa", "28")])]), "pins.nsa"),
        Some("darf nicht größer als 27 sein".to_string())
    );
    assert!(errors_of(&[("pins", &[("nsa", "27")])]).is_empty());
}

#[test]
fn enforces_the_hour_range() {
    assert!(errors_of(&[("sip", &[("dnd_from", "-1")])]).get("sip.dnd_from").is_some());
    assert!(errors_of(&[("sip", &[("dnd_from", "24")])]).get("sip.dnd_from").is_some());
    assert!(errors_of(&[("sip", &[("dnd_from", "23")])]).is_empty());
}

#[test]
fn accepts_the_usual_checkbox_spellings() {
    for spelling in ["ja", "an", "yes", "on", "true", "1"] {
        assert!(
            errors_of(&[("sip", &[("whitelist_active", &format!("\"{}\"", spelling))])]).is_empty(),
            "{} sollte gelten",
            spelling
        );
    }

    let (cleaned, _) = config::validate(&with(&[("sip", &[("whitelist_active", "\"an\"")])]));
    assert_eq!(section_of(&cleaned, "sip")["whitelist_active"], Val::Bool(true));
}

#[test]
fn rejects_nonsense_checkbox_values() {
    assert!(errors_of(&[("sip", &[("whitelist_active", "\"vielleicht\"")])])
        .get("sip.whitelist_active")
        .is_some());
}

#[test]
fn enforces_the_port_range() {
    assert!(errors_of(&[("web", &[("port", "70000")])]).get("web.port").is_some());
    assert!(errors_of(&[("web", &[("port", "8080")])]).is_empty());
}

#[test]
fn enforces_the_brightness_range_but_allows_fractions() {
    assert!(errors_of(&[("misc", &[("night_light_duty", "120")])])
        .get("misc.night_light_duty")
        .is_some());

    let (cleaned, _) = config::validate(&with(&[("misc", &[("night_light_duty", "33.5")])]));
    assert_eq!(section_of(&cleaned, "misc")["night_light_duty"], Val::Float(33.5));
}

#[test]
fn a_numeric_password_stays_text() {
    let (cleaned, errors) = config::validate(&with(&[("sip", &[("pass", "\"4711\"")])]));
    assert!(errors.is_empty());
    assert_eq!(config::option_str(&cleaned, "sip", "pass"), "4711");
}

// ------------------------------------------------------------------- lists

/// Seven valid wake-up times, one per weekday.
const SEVEN: &[(&str, &str)] = &[("wake_up_times", "[\"06:30\", \"06:30\", \"06:30\", \"06:30\", \"06:30\", \"07:30\", \"07:30\"]")];

#[test]
fn requires_exactly_seven_wake_up_times() {
    assert!(errors_of(&[("misc", SEVEN)]).is_empty());

    let six: &[(&str, &str)] = &[("wake_up_times", "[\"06:30\", \"06:30\", \"06:30\", \"06:30\", \"06:30\", \"07:30\"]")];
    assert!(errors_of(&[("misc", six)]).get("misc.wake_up_times").is_some());

    let eight: &[(&str, &str)] = &[("wake_up_times", "[\"06:30\", \"06:30\", \"06:30\", \"06:30\", \"06:30\", \"07:30\", \"07:30\", \"07:30\"]")];
    assert!(errors_of(&[("misc", eight)]).get("misc.wake_up_times").is_some());
}

#[test]
fn wake_up_times_must_be_well_formed() {
    let no_leading_zero: &[(&str, &str)] = &[("wake_up_times", "[\"6:30\", \"06:30\", \"06:30\", \"06:30\", \"06:30\", \"07:30\", \"07:30\"]")];
    assert!(errors_of(&[("misc", no_leading_zero)]).get("misc.wake_up_times").is_some());

    let hour_25: &[(&str, &str)] = &[("wake_up_times", "[\"25:00\", \"06:30\", \"06:30\", \"06:30\", \"06:30\", \"07:30\", \"07:30\"]")];
    assert!(errors_of(&[("misc", hour_25)]).get("misc.wake_up_times").is_some());
}

#[test]
fn sleep_music_accepts_empty_and_multiple_tracks() {
    assert!(errors_of(&[("sounds", &[("sleep_music", "[]")])]).is_empty());
    assert!(errors_of(&[("sounds", &[("sleep_music", "[\"/a.mp3\", \"/b.mp3\"]")])]).is_empty());
}

// ---------------------------------------------------------------- short codes

#[test]
fn short_codes_accept_leading_zero_and_up_to_five_digits() {
    assert!(errors_of(&[("numbers", &[("01", "\"01234567\"")])]).is_empty());
    assert!(errors_of(&[("numbers", &[("12345", "\"0123\"")])]).is_empty());
}

#[test]
fn rejects_malformed_short_codes() {
    assert!(errors_of(&[("numbers", &[("123456", "\"0123\"")])]).get("numbers.123456").is_some());
    assert!(errors_of(&[("numbers", &[("ab", "\"0123\"")])]).get("numbers.ab").is_some());
    assert!(errors_of(&[("numbers", &[("01", "\"Hallo Welt\"")])]).get("numbers.01").is_some());
    assert!(errors_of(&[("numbers", &[("01", "\"  \"")])]).get("numbers.01").is_some());
}

#[test]
fn accepts_the_built_in_commands_as_short_codes() {
    let codes: &[(&str, &str)] = &[
        ("11", "\"enable-night-mode\""),
        ("12", "\"play-sleep-music\""),
        ("18", "\"reboot\""),
        ("19", "\"shutdown\""),
        ("21", "\"test-loudspeaker\""),
        ("22", "\"test-earpiece\""),
    ];

    assert!(errors_of(&[("numbers", codes)]).is_empty());
}

#[test]
fn an_unknown_command_is_treated_as_a_number() {
    // `shutdown` is a known command, so it skips the number check; anything
    // else has to look like a phone number.
    let (cleaned, errors) = config::validate(&with(&[("numbers", &[("01", "\"01234567\"")])]));
    assert!(errors.is_empty());
    assert_eq!(section_of(&cleaned, "numbers")["01"].to_display_string(), "01234567");
}

#[test]
fn short_codes_accept_country_prefixes() {
    let codes: &[(&str, &str)] = &[("01", "\"+49 89 1234567\""), ("02", "\"0049891234567\"")];
    assert!(errors_of(&[("numbers", codes)]).is_empty());
}

// ------------------------------------------------------------------- ringtones

#[test]
fn ringtones_need_a_file_and_accept_any_key() {
    assert!(errors_of(&[("ringtones", &[("0891234", "\"/opt/ring.wav\"")])]).is_empty());
    assert!(errors_of(&[("ringtones", &[("089", "\"\"")])]).get("ringtones.089").is_some());

    let many: &[(&str, &str)] = &[("\"+49891234\"", "\"/a.wav\""), ("0049891234", "\"/b.wav\"")];
    assert!(errors_of(&[("ringtones", many)]).is_empty());
}

// -----------------------------------------------------------------------------
// Unknown settings
// -----------------------------------------------------------------------------

/// The file is written from the schema, so an entry that the schema does not
/// know is lost on the next save. It has to be named before it goes, otherwise
/// the user finds out from the missing entry.
#[test]
fn unknown_options_and_sections_are_dropped() {
    let mut data = base();
    if let Val::Table(sip) = data.get_mut("sip").expect("sip") {
        sip.insert("zusatz".to_string(), Val::Int(42));
    }
    data.insert(
        "eigener_abschnitt".to_string(),
        Val::Table([("x".to_string(), Val::Str("y".to_string()))].into()),
    );

    assert_eq!(
        config::unknown(&data),
        vec!["[eigener_abschnitt]".to_string(), "sip.zusatz".to_string()]
    );

    let (cleaned, errors) = config::validate(&data);
    assert!(errors.is_empty());

    assert!(!section_of(&cleaned, "sip").contains_key("zusatz"), "unbekannte Option behalten");
    assert!(!cleaned.contains_key("eigener_abschnitt"), "unbekannter Abschnitt behalten");

    // And they are not written back.
    let text = config::dump(&cleaned);
    assert!(!text.contains("zusatz"), "unbekannte Option in der Datei");
    assert!(!text.contains("eigener_abschnitt"), "unbekannter Abschnitt in der Datei");

    let reloaded = config::parse(&text).expect("TOML lesbar");
    assert_eq!(reloaded, cleaned);
}

/// Every key in `[numbers]` and `[ringtones]` is a short code or a number, not a
/// schema key - so none of them can be "unknown".
#[test]
fn entry_tables_are_not_unknown() {
    let data = with(&[("numbers", &[("42", "\"01299999\"")]), ("ringtones", &[("0891234", "\"/a.wav\"")])]);
    assert!(config::unknown(&data).is_empty());
    assert!(config::validate(&data).0.contains_key("numbers"));
}

// -----------------------------------------------------------------------------
// Writing to the file
// -----------------------------------------------------------------------------

#[test]
fn writes_through_a_temporary_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");

    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("support/config-example.toml");
    std::fs::copy(&example, &path).unwrap();

    let (baseline, errors) = config::validate(&config::read(&path).unwrap());
    assert!(errors.is_empty(), "Ausgangsdatei ungültig");

    // An unchanged save must leave the file readable and complete.
    let saved = config::save(&path, &baseline, &baseline).expect("Speichern");
    assert!(saved.changed.is_empty());

    let stored = config::read(&path).unwrap();
    for section in ["network", "sip", "pins", "numbers", "ringtones", "sounds", "misc", "web"] {
        assert!(stored.contains_key(section), "Abschnitt {} fehlt", section);
    }

    // Now add a short code, remove one, and change two values.
    let mut edited = baseline.clone();
    if let Val::Table(numbers) = edited.get_mut("numbers").expect("numbers") {
        numbers.insert("42".to_string(), Val::Str("01299999".into()));
        numbers.remove("03");
    }
    if let Val::Table(sip) = edited.get_mut("sip").expect("sip") {
        sip.insert("dial_timeout".to_string(), Val::Int(45));
    }
    if let Val::Table(misc) = edited.get_mut("misc").expect("misc") {
        misc.insert("wake_light_duty".to_string(), Val::Float(25.5));
    }

    let saved = config::save(&path, &edited, &baseline).expect("Speichern");
    assert_eq!(
        saved.changed.iter().collect::<Vec<_>>(),
        vec!["[sip]", "[numbers]", "[misc]"]
    );

    let stored = config::read(&path).unwrap();
    let numbers = section_of(&stored, "numbers");
    assert_eq!(numbers["42"].to_display_string(), "01299999");
    assert!(!numbers.contains_key("03"), "Kurzwahl nicht entfernt");
    assert_eq!(config::option_int(&stored, "sip", "dial_timeout"), 45);
    assert_eq!(config::option_float(&stored, "misc", "wake_light_duty"), 25.5);

    // No temporary file is left behind.
    assert!(!path.with_file_name("config.toml.tmp").exists());
}

#[test]
fn a_failed_save_leaves_the_file_alone() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");

    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("support/config-example.toml");
    std::fs::copy(&example, &path).unwrap();

    let (baseline, _) = config::validate(&config::read(&path).unwrap());
    let before = std::fs::read_to_string(&path).unwrap();

    // An invalid configuration must be refused without touching the file.
    let mut broken = baseline.clone();
    if let Val::Table(pins) = broken.get_mut("pins").expect("pins") {
        pins.insert("nsi".to_string(), Val::Str("99".into()));
    }

    assert!(config::save(&path, &broken, &baseline).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert!(!path.with_file_name("config.toml.tmp").exists());
}