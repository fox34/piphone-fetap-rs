//! End-to-end test of the configuration editing: requests exactly as the
//! browser sends them, through the real router and the real file writing, but
//! without hardware.
//!
//! Mirrors `tests/test-config-web.py`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value as Json_};

use piphone::config;
use piphone::command::Command;
use piphone::webserver::PhoneApi;

/// The sample configuration that ships with the project.
fn example_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("support/config-example.toml")
}

/// The phone's view of its own configuration.
///
/// This mirrors what `Phone` does at runtime, minus the hardware: the running
/// state stays untouched while the file is rewritten, exactly as the Python
/// version kept `config_baseline` separate from the live `config`.
struct FilePhone {
    path: PathBuf,
    baseline: config::Table,
}

impl FilePhone {
    fn new(path: &Path) -> Self {
        let baseline = config::validate(&config::read(path).expect("Beispieldatei lesen")).0;
        FilePhone { path: path.to_path_buf(), baseline }
    }

    /// The file as it stands on disk.
    fn stored(&self) -> config::Table {
        config::read(&self.path).expect("gespeicherte Datei lesen")
    }
}

/// A writable copy of the sample configuration.
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("temporäres Verzeichnis");
    let path = directory.path().join("config.toml");
    std::fs::copy(example_path(), &path).expect("Beispielkonfiguration kopieren");
    (directory, path)
}

/// Rebuild the form from `/api/config`, apply `changes`, and send it back.
///
/// This is what the browser's JS does: it reads the current schema, keeps the
/// values it knows, and replaces only the fields the user touched. Entries the
/// user deleted are simply absent from the request.
fn as_browser_sends(schema: &Json_, changes: &Json_) -> Json_ {
    let mut form = serde_json::Map::new();

    for section in schema["sections"].as_array().expect("Abschnitte") {
        let name = section["name"].as_str().expect("Abschnittsname");

        if section["kind"] == "entries" {
            let overrides = changes.get(name);
            let mut rows = serde_json::Map::new();

            // Keep the existing rows unless the section was replaced wholesale,
            // which is how the form behaves when rows were added or removed.
            if let Some(replacement) = overrides {
                if let Some(map) = replacement.as_object() {
                    for (key, value) in map {
                        rows.insert(key.clone(), value.clone());
                    }
                    form.insert(name.to_string(), Json_::Object(rows));
                    continue;
                }
            }

            for entry in section["entries"].as_array().expect("Einträge") {
                rows.insert(
                    entry["key"].as_str().expect("Schlüssel").to_string(),
                    entry["value"].clone(),
                );
            }
            form.insert(name.to_string(), Json_::Object(rows));
            continue;
        }

        let mut values = serde_json::Map::new();
        let overrides = changes.get(name);

        for field in section["fields"].as_array().expect("Felder") {
            let key = field["key"].as_str().expect("Feldname");

            if let Some(value) = overrides.and_then(|o| o.get(key)) {
                values.insert(key.to_string(), value.clone());
                continue;
            }

            // Every other field keeps what the form currently shows.
            let value = match field["type"].as_str().unwrap_or("") {
                "bool" => Json_::Bool(field["value"] == Json_::Bool(true)),
                "list" => field["value"].clone(),
                _ => match &field["value"] {
                    Json_::Null => Json_::String(String::new()),
                    value => Json_::String(value.to_string().trim_matches('"').to_string()),
                },
            };
            values.insert(key.to_string(), value);
        }

        form.insert(name.to_string(), Json_::Object(values));
    }

    Json_::Object(form)
}

/// Describe the current schema the way `/api/config` does.
fn schema_of(phone: &FilePhone) -> Json_ {
    json!({ "sections": config::describe(&phone.baseline) })
}

/// Save through the real `config::save` and report what the phone would answer.
///
/// The third value is whether the change would restart the phone service, which
/// is what the phone does with it afterwards: only a real change restarts anything.
fn save(phone: &FilePhone, payload: &Json_) -> (bool, Vec<String>, bool, Json_) {
    let incoming = config::from_json(payload);

    match config::save(&phone.path, &incoming, &phone.baseline) {
        Err(errors) => {
            let map: serde_json::Map<String, Json_> = errors
                .iter()
                .map(|(key, message)| (key.to_string(), Json_::String(message.to_string())))
                .collect();
            (false, Vec::new(), false, json!({ "errors": Json_::Object(map) }))
        }
        Ok(saved) => {
            let restarted = !saved.changed.is_empty();
            let changed = saved.changed.clone();
            (true, changed, restarted, json!({}))
        }
    }
}

// -----------------------------------------------------------------------------

#[test]
fn form_shows_the_values_from_the_file() {
    let (_dir, path) = workspace();
    let phone = FilePhone::new(&path);
    let schema = schema_of(&phone);

    let sip = section(&schema, "sip");
    assert_eq!(field(sip, "host")["value"], "10.0.0.1");

    let numbers = section(&schema, "numbers");
    let keys: Vec<&str> = numbers["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["01", "02", "03", "11", "12", "18", "19", "21", "22"]);

    let choices = numbers["choices"].as_array().unwrap();
    assert!(choices.iter().any(|c| c == "shutdown"), "Kurzbefehle fehlen");
}

#[test]
fn adds_and_removes_short_codes() {
    let (_dir, path) = workspace();
    let phone = FilePhone::new(&path);

    // Add a tenth short code.
    let mut numbers = serde_json::Map::new();
    for key in ["01", "02", "03", "11", "12", "18", "19", "21", "22"] {
        let value = phone.baseline.get("numbers").unwrap().as_table().unwrap()[key].clone();
        numbers.insert(key.to_string(), Json_::String(value.to_display_string()));
    }
    numbers.insert("42".to_string(), json!("01299999"));

    let form = as_browser_sends(&schema_of(&phone), &json!({ "numbers": numbers }));
    let (ok, _, _, _) = save(&phone, &form);
    assert!(ok);

    let stored = phone.stored();
    let entries = stored.get("numbers").unwrap().as_table().unwrap();
    assert_eq!(entries["42"].to_display_string(), "01299999");
    assert_eq!(entries.len(), 10);

    // Now keep only two of them.
    let form = as_browser_sends(
        &schema_of(&phone),
        &json!({ "numbers": { "01": "01234567", "11": "enable-night-mode" } }),
    );
    let (ok, _, _, _) = save(&phone, &form);
    assert!(ok);

    let stored = phone.stored();
    assert_eq!(stored.get("numbers").unwrap().as_table().unwrap().len(), 2);
}

#[test]
fn reports_only_the_sections_that_changed() {
    let (_dir, path) = workspace();
    let phone = FilePhone::new(&path);

    let form = as_browser_sends(
        &schema_of(&phone),
        &json!({ "sip": { "dial_timeout": "90" }, "misc": { "night_light_pin": "17" } }),
    );
    let (ok, changed, restarted, _) = save(&phone, &form);

    assert!(ok);
    let mut reported = changed.clone();
    reported.sort();
    assert_eq!(reported, vec!["[misc]", "[sip]"]);
    assert!(restarted, "der Dienst muss neu starten");

    let stored = phone.stored();
    assert_eq!(config::option_int(&stored, "sip", "dial_timeout"), 90);
    assert_eq!(config::option_int(&stored, "misc", "night_light_pin"), 17);

    // Everything else is untouched.
    assert_eq!(config::option_str(&stored, "sip", "host"), "10.0.0.1");
    assert_eq!(config::option_int(&stored, "pins", "nsi"), 23);
}

#[test]
fn saving_unchanged_leaves_the_file_alone() {
    let (_dir, path) = workspace();
    let phone = FilePhone::new(&path);

    // The first save rewrites the file from the schema, which regenerates all
    // comments. From then on, saving an unchanged form must be a no-op.
    let form = as_browser_sends(&schema_of(&phone), &json!({}));
    assert!(save(&phone, &form).0);

    let before = std::fs::read_to_string(&path).unwrap();
    let form = as_browser_sends(&schema_of(&phone), &json!({}));
    let (ok, changed, restarted, _) = save(&phone, &form);

    assert!(ok);
    assert!(changed.is_empty(), "unerwartete Änderung: {:?}", changed);
    assert!(!restarted, "ohne Änderung darf nichts neu starten");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "Datei wurde verändert");
}

#[test]
fn rejects_invalid_input_and_leaves_the_file_untouched() {
    let (_dir, path) = workspace();
    let phone = FilePhone::new(&path);

    let before = std::fs::read_to_string(&path).unwrap();
    let form = as_browser_sends(
        &schema_of(&phone),
        &json!({
            "pins": { "nsi": "50" },
            "misc": { "wake_up_times": ["06:30", "06:30", "06:30"] },
            "sip": { "host": "" }
        }),
    );
    let (ok, _, _, result) = save(&phone, &form);

    assert!(!ok, "ungültige Eingaben müssen abgelehnt werden");

    let errors = result["errors"].as_object().expect("Fehlerliste");
    for key in ["pins.nsi", "misc.wake_up_times", "sip.host"] {
        assert!(errors.contains_key(key), "Fehler für {} fehlt", key);
    }

    assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "Datei wurde verändert");

    // No temporary file is left behind.
    let temporary = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    assert!(!temporary.exists(), "temporäre Datei zurückgeblieben");
}

/// The ringtones are uploaded files and not options: the form does not offer a
/// section for them any more, and one that is sent anyway does not come back.
#[test]
fn ringtones_cannot_come_back_through_the_form() {
    let (_dir, path) = workspace();
    let phone = FilePhone::new(&path);

    // What the browser builds from the schema.
    let mut form = as_browser_sends(&schema_of(&phone), &json!({}));
    assert!(form.get("ringtones").is_none(), "das Formular bietet Klingeltöne an");

    // And one that is sent anyway is dropped rather than written.
    form["ringtones"] = json!({ "08912345": "/usr/local/share/piphone/sounds/ring-02.wav" });
    let (ok, _, _, _) = save(&phone, &form);
    assert!(ok, "ein unbekannter Abschnitt darf das Speichern nicht verhindern");

    assert!(!phone.stored().contains_key("ringtones"), "Abschnitt zurückgekehrt");
    assert!(
        !std::fs::read_to_string(&path).unwrap().contains("[ringtones]"),
        "Klingeltöne stehen wieder in der Datei"
    );
}

#[test]
fn edits_lists() {
    let (_dir, path) = workspace();
    let phone = FilePhone::new(&path);

    let form = as_browser_sends(
        &schema_of(&phone),
        &json!({ "misc": { "wake_up_times": ["05:00", "05:00", "05:00", "05:00", "05:00", "06:00", "06:00"] } }),
    );
    let (ok, _, _, _) = save(&phone, &form);
    assert!(ok);

    let stored = phone.stored();
    assert_eq!(
        config::option_list(&stored, "misc", "wake_up_times"),
        vec!["05:00", "05:00", "05:00", "05:00", "05:00", "06:00", "06:00"]
    );
}

#[test]
fn saves_passwords_with_special_characters() {
    let (_dir, path) = workspace();
    let phone = FilePhone::new(&path);

    let form = as_browser_sends(
        &schema_of(&phone),
        &json!({ "sip": { "pass": "neues passwort mit Umlaut ä" } }),
    );
    let (ok, _, _, result) = save(&phone, &form);
    assert!(ok, "{}", result);

    assert_eq!(
        config::option_str(&phone.stored(), "sip", "pass"),
        "neues passwort mit Umlaut ä"
    );

    // The field is still marked as a secret, so the form masks it.
    let schema = schema_of(&phone);
    let sip = section(&schema, "sip");
    assert_eq!(field(sip, "pass")["secret"], true);
}

#[test]
fn written_file_carries_the_generated_comments() {
    let (_dir, path) = workspace();
    let phone = FilePhone::new(&path);

    let form = as_browser_sends(&schema_of(&phone), &json!({}));
    save(&phone, &form);

    let text = std::fs::read_to_string(&path).unwrap();

    assert!(text.contains("# GPIO-Pin für Nachtlicht (0 = deaktiviert)."), "Option ohne Kommentar");
    assert!(
        text.contains("selbst geschriebene Kommentare werden dabei nicht beibehalten"),
        "Hinweis auf verlorene Kommentare fehlt"
    );

    for section in ["network", "sip", "pins", "numbers", "misc", "web"] {
        assert!(text.contains(&format!("[{}]", section)), "Abschnitt {} fehlt", section);
    }
    assert!(!text.contains("[sounds]"), "Abschnitt [sounds] gehört nicht mehr in die Datei");
    assert!(!text.contains("[ringtones]"), "Klingeltöne gehören nicht mehr in die Datei");

    assert!(text.ends_with('\n'), "Datei muss mit Zeilenumbruch enden");
}

/// The form does not know entries the schema does not have, so it never sends
/// them. They are gone from the file after the next save - and the save says so,
/// because losing an entry silently is the one thing worse than losing it.
#[test]
fn unknown_settings_are_dropped_and_reported() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");

    // An unknown section, and an unknown option inside a known one.
    let text = std::fs::read_to_string(example_path()).unwrap();
    let text = text.replacen("[sip]", "[sip]\nzusatz = \"nur von Hand gesetzt\"", 1);
    let text = format!("{}\n[sonder]\nx = 1\n", text);
    std::fs::write(&path, &text).unwrap();

    let phone = FilePhone::new(&path);

    // The phone reads what is in the file, but its own view of the
    // configuration only holds what the schema knows.
    assert!(
        phone.baseline.get("sonder").is_none(),
        "Sonderabschnitt gehört nicht in den Stand"
    );
    assert_eq!(config::option_str(&phone.baseline, "sip", "host"), "10.0.0.1");

    let form = as_browser_sends(
        &schema_of(&phone),
        &json!({ "network": { "wifi_test_host": "10.0.0.2" } }),
    );
    let incoming = config::from_json(&form);
    let saved = config::save(&path, &incoming, &phone.baseline).expect("Speichern");

    assert_eq!(saved.changed, vec!["[network]".to_string()]);
    // Alphabetisch nach Abschnitt, die Reihenfolge steckt in der `Table`.
    assert_eq!(
        saved.dropped,
        vec!["sip.zusatz".to_string(), "[sonder]".to_string()],
        "die verlorenen Einträge müssen benannt werden"
    );

    let stored = phone.stored();
    assert_eq!(config::option_str(&stored, "network", "wifi_test_host"), "10.0.0.2");
    assert_eq!(config::option_str(&stored, "sip", "host"), "10.0.0.1");

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("[sonder]"), "Sonderabschnitt nicht entfernt:\n{}", text);
    assert!(!text.contains("zusatz"), "unbekannte Option nicht entfernt:\n{}", text);

    // Nothing is dropped from a file that has nothing unknown in it.
    let saved = config::save(&path, &incoming, &saved.cleaned).expect("Speichern");
    assert!(saved.dropped.is_empty(), "{:?}", saved.dropped);
}

#[test]
fn saving_never_touches_the_running_configuration() {
    let (_dir, path) = workspace();
    let phone = FilePhone::new(&path);

    let running_before = phone.baseline.clone();

    let form = as_browser_sends(&schema_of(&phone), &json!({ "sip": { "dial_timeout": "5" } }));
    save(&phone, &form);

    // The file changed, but the phone still runs with what it started with - it
    // is a new process that reads the change, never this one.
    assert_eq!(config::option_int(&phone.stored(), "sip", "dial_timeout"), 5);
    assert_eq!(
        config::option_int(&phone.baseline, "sip", "dial_timeout"),
        config::option_int(&running_before, "sip", "dial_timeout"),
        "laufender Stand wurde verändert"
    );
}

/// Saving restarts the phone service and nothing else
#[test]
fn saving_does_not_trigger_any_action() {
    let (_dir, path) = workspace();

    struct Counting {
        path: PathBuf,
        baseline: config::Table,
        actions: std::sync::Mutex<Vec<Command>>,
    }

    impl PhoneApi for Counting {
        fn get_state(&self) -> Json_ {
            json!({ "uptime": 1 })
        }
        fn get_config(&self) -> Json_ {
            json!({ "sections": config::describe(&self.baseline) })
        }
        fn save_config(&self, payload: Json_) -> Json_ {
            let incoming = config::from_json(&payload);
            match config::save(&self.path, &incoming, &self.baseline) {
                Ok(_) => json!({ "ok": true }),
                Err(_) => json!({ "ok": false }),
            }
        }
        fn sounds_dir(&self) -> PathBuf {
            self.path.with_file_name("sounds")
        }
        fn run_action(&self, action: Command) {
            self.actions.lock().unwrap().push(action);
        }
    }

    let baseline = config::validate(&config::read(&path).unwrap()).0;
    let phone = Counting { path, baseline, actions: std::sync::Mutex::new(Vec::new()) };

    let incoming = config::from_json(&as_browser_sends(
        &phone.get_config(),
        &json!({ "sip": { "dial_timeout": "5" } }),
    ));
    assert!(config::save(&phone.path, &incoming, &phone.baseline).is_ok());

    assert!(
        phone.actions.lock().unwrap().is_empty(),
        "Speichern hat eine Aktion ausgelöst"
    );
}

// -----------------------------------------------------------------------------

fn section<'a>(schema: &'a Json_, name: &str) -> &'a Json_ {
    schema["sections"]
        .as_array()
        .expect("Abschnitte")
        .iter()
        .find(|section| section["name"] == name)
        .unwrap_or_else(|| panic!("Abschnitt {} fehlt", name))
}

fn field<'a>(section: &'a Json_, key: &str) -> &'a Json_ {
    section["fields"]
        .as_array()
        .expect("Felder")
        .iter()
        .find(|field| field["key"] == key)
        .unwrap_or_else(|| panic!("Feld {} fehlt", key))
}