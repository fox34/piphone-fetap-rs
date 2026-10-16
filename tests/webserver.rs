//! HTTP-level tests for the web interface.
//!
//! Mirrors `tests/test-webserver.py`: the server is driven in-process through
//! the real router, so routing, auth, body limits and status codes are all
//! exercised without needing a socket.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::http::{Request, StatusCode};
use axum::body::Body;
use http_body_util::BodyExt;
use serde_json::{json, Value as Json_};
use tower::ServiceExt; // for `oneshot`

use piphone::config;
use piphone::command::Command;
use piphone::sounds;
use piphone::webserver::{router, PhoneApi};

/// A phone that only answers questions; nothing here touches hardware.
struct StubPhone {
    saved: Mutex<Vec<config::Table>>,
    actions: Mutex<Vec<Command>>,
    /// Where the sound files would be - a directory of this test's own, so the
    /// upload and preview routes never come near the real one.
    sounds: PathBuf,
}

impl StubPhone {
    fn new(sounds: PathBuf) -> Self {
        StubPhone { saved: Mutex::new(Vec::new()), actions: Mutex::new(Vec::new()), sounds }
    }
}

impl PhoneApi for StubPhone {
    fn get_state(&self) -> Json_ {
        json!({ "uptime": 1 })
    }

    fn get_config(&self) -> Json_ {
        json!({
            "sections": config::describe(&example()),
            "sounds": sounds::describe(&self.sounds),
        })
    }

    fn save_config(&self, payload: Json_) -> Json_ {
        let incoming = config::from_json(&payload);
        let (cleaned, errors) = config::validate(&incoming);

        if !errors.is_empty() {
            let map: serde_json::Map<String, Json_> = errors
                .iter()
                .map(|(key, message)| (key.to_string(), Json_::String(message.to_string())))
                .collect();
            return json!({ "ok": false, "errors": map });
        }

        self.saved.lock().unwrap().push(cleaned.clone());
        json!({ "ok": true, "errors": {}, "changed": ["[sip]"] })
    }

    fn sounds_dir(&self) -> PathBuf {
        self.sounds.clone()
    }

    fn run_action(&self, action: Command) {
        self.actions.lock().unwrap().push(action);
    }
}

/// A configuration with the required options filled in.
fn example() -> config::Table {
    config::parse(
        r#"
[network]
wifi_test_host = "10.0.0.1"

[sip]
host = "10.0.0.1"
user = "test"
pass = "test"

[pins]
nsi = 23
nsa = 24
gabel = 15

[numbers]
"01" = "01234567"

[web]
port = 8123
"#,
    )
    .expect("Beispielkonfiguration ist gültig")
}

struct Client {
    router: axum::Router,
    phone: Arc<StubPhone>,
    /// Kept alive so the sounds directory is not removed while it is in use.
    _sounds: tempfile::TempDir,
}

impl Client {
    fn new() -> Self {
        Self::with_credentials(None)
    }

    fn with_credentials(credentials: Option<&str>) -> Self {
        Self::with_sounds(tempfile::tempdir().expect("temporäres Verzeichnis"), credentials)
    }

    /// A client whose sound files are the ones in this directory.
    ///
    /// Every client gets a directory of its own, so an upload never lands next
    /// to the real ones in `/usr/local/share/piphone`.
    fn with_sounds(sounds: tempfile::TempDir, credentials: Option<&str>) -> Self {
        let dir = sounds.path().to_path_buf();
        let phone = Arc::new(StubPhone::new(dir));
        let router = router(
            phone.clone(),
            credentials.map(|c| c.to_string()),
            false,
        );
        Client { router, phone, _sounds: sounds }
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, String, Json_) {
        let response = self.router.clone().oneshot(request).await.expect("Antwort");
        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = response.into_body().collect().await.expect("Rumpf").to_bytes();
        let text = String::from_utf8_lossy(&body).to_string();

        let json = if content_type.contains("application/json") && !text.trim().is_empty() {
            serde_json::from_str(&text).expect("gültiges JSON")
        } else {
            Json_::Null
        };

        (status, text, json)
    }

    async fn get(&self, path: &str) -> (StatusCode, String, Json_) {
        self.send(Request::builder().uri(path).body(Body::empty()).unwrap()).await
    }

    async fn post(&self, path: &str, body: &str) -> (StatusCode, String, Json_) {
        self.send(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn post_action(&self, name: &str) -> (StatusCode, String, Json_) {
        self.send(
            Request::builder()
                .method("POST")
                .uri(format!("/api/action/{}", name))
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post_sound(&self, key: &str, bytes: &[u8]) -> (StatusCode, String, Json_) {
        self.send(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sound/{}", key))
                .header("content-type", "audio/wav")
                .body(Body::from(bytes.to_vec()))
                .unwrap(),
        )
        .await
    }

    /// Upload a file into one of the collections, under the name it keeps.
    async fn post_file(&self, pool: &str, name: &str, bytes: &[u8]) -> (StatusCode, String, Json_) {
        self.send(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sound/{}/{}", pool, name))
                .header("content-type", "audio/wav")
                .body(Body::from(bytes.to_vec()))
                .unwrap(),
        )
        .await
    }

    async fn delete_file(&self, pool: &str, name: &str) -> (StatusCode, String, Json_) {
        self.send(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/sound/{}/{}", pool, name))
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// The raw response of a preview request, for the headers as well.
    async fn get_sound(&self, key: &str) -> (StatusCode, String, Vec<u8>) {
        let request = Request::builder()
            .uri(format!("/api/sound/{}", key))
            .body(Body::empty())
            .unwrap();
        self.preview(request).await
    }

    /// The raw response of a collection file request, for the headers as well.
    async fn get_file(&self, pool: &str, name: &str) -> (StatusCode, String, Vec<u8>) {
        let request = Request::builder()
            .uri(format!("/api/sound/{}/{}", pool, name))
            .body(Body::empty())
            .unwrap();
        self.preview(request).await
    }

    async fn preview(&self, request: Request<Body>) -> (StatusCode, String, Vec<u8>) {
        let response = self.router.clone().oneshot(request).await.expect("Antwort");

        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let disposition = response
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let bytes = response.into_body().collect().await.expect("Rumpf").to_bytes().to_vec();
        (status, format!("{}|{}", content_type, disposition), bytes)
    }
}

/// Find a section of the config schema by name.
fn section<'a>(data: &'a Json_, name: &str) -> &'a Json_ {
    data["sections"]
        .as_array()
        .expect("Abschnitte als Liste")
        .iter()
        .find(|section| section["name"] == name)
        .unwrap_or_else(|| panic!("Abschnitt {} fehlt", name))
}

/// Find one field of a section by key.
fn field<'a>(section: &'a Json_, key: &str) -> &'a Json_ {
    section["fields"]
        .as_array()
        .expect("Felder als Liste")
        .iter()
        .find(|field| field["key"] == key)
        .unwrap_or_else(|| panic!("Feld {} fehlt", key))
}

/// The fixed sounds, as the page sees them.
fn fixed_sounds(data: &Json_) -> &[Json_] {
    data["sounds"]["fixed"].as_array().expect("feste Klänge als Liste")
}

/// The files of a collection, as the page sees them.
fn pool_files<'a>(data: &'a Json_, key: &str) -> &'a [Json_] {
    data["sounds"]["pools"]
        .as_array()
        .expect("Sammlungen als Liste")
        .iter()
        .find(|pool| pool["key"] == key)
        .unwrap_or_else(|| panic!("Sammlung {} fehlt", key))["files"]
        .as_array()
        .expect("Dateien als Liste")
}

// -----------------------------------------------------------------------------

#[tokio::test]
async fn serves_the_status_page() {
    let client = Client::new();
    let (status, body, _) = client.get("/").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<h1>PiPhone</h1>"), "Statusseite ohne Überschrift");
}

#[tokio::test]
async fn serves_the_configuration_page() {
    let client = Client::new();
    let (status, body, _) = client.get("/config").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("PiPhone"), "Konfigurationsseite ohne Titel");
}

#[tokio::test]
async fn state_is_json() {
    let client = Client::new();
    let (status, _, data) = client.get("/api/state").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["uptime"], 1);
}

#[tokio::test]
async fn next_wake_up_is_a_parseable_date() {
    // The status page runs this string through `new Date()`, which cannot parse
    // a bare "HH:MM" - it rendered "Invalid Date" on the page. It also needs the
    // date, because the next wake-up can be tomorrow and the page shows the
    // weekday.
    let client = Client::new();
    let (_, _, data) = client.get("/api/state").await;

    let Some(value) = data["next_wake_up"].as_str() else {
        // No night mode running is a legitimate state, and then it must be null.
        assert!(data["next_wake_up"].is_null(), "next_wake_up is neither a string nor null");
        return;
    };

    assert!(
        chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S").is_ok(),
        "next_wake_up {value:?} is not ISO 8601, so the page cannot parse it"
    );
}

#[tokio::test]
async fn config_schema_has_every_section_in_order() {
    let client = Client::new();
    let (status, _, data) = client.get("/api/config").await;

    assert_eq!(status, StatusCode::OK);

    let names: Vec<&str> = data["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();

    assert_eq!(
        names,
        vec!["network", "sip", "pins", "numbers", "misc", "web"]
    );
}

/// The sounds are not options any more: they come as their own list, with the
/// state of the file behind each name, and the page builds the whole section
/// from it.
#[tokio::test]
async fn sounds_are_described_next_to_the_sections() {
    let client = Client::new();
    let (_, _, data) = client.get("/api/config").await;

    let fixed = data["sounds"]["fixed"].as_array().expect("feste Klänge als Liste");
    assert_eq!(fixed.len(), piphone::sounds::Sound::ALL.len());

    let keys: Vec<&str> = fixed.iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert!(keys.contains(&"ringtone"), "generischer Klingelton fehlt: {:?}", keys);
    assert!(!keys.contains(&"sleep_music"), "die Einschlafmusik ist eine Liste, kein fester Klang");

    for sound in fixed {
        assert!(sound["present"] == false, "kein Klang darf als vorhanden gelten");
        assert!(sound["file"].is_null());
        assert!(sound["label"].as_str().unwrap().len() > 1);
        assert!(!sound["help"].as_array().expect("Hilfe").is_empty());
    }

    // The lists start empty, each with the help the page shows above it.
    let pools = data["sounds"]["pools"].as_array().expect("Sammlungen als Liste");
    let keys: Vec<&str> = pools.iter().map(|p| p["key"].as_str().unwrap()).collect();
    assert_eq!(keys, vec!["ringtones", "sleep"]);

    for pool in pools {
        assert!(pool["files"].as_array().expect("Dateien").is_empty(), "{}", pool["key"]);
        assert!(!pool["label"].as_str().unwrap().is_empty());
        assert!(!pool["help"].as_array().expect("Hilfe").is_empty());
    }
}

#[tokio::test]
async fn sip_section_describes_every_option() {
    let client = Client::new();
    let (_, _, data) = client.get("/api/config").await;
    let sip = section(&data, "sip");

    let keys: Vec<&str> = sip["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["key"].as_str().unwrap())
        .collect();

    assert_eq!(
        keys,
        vec!["host", "user", "pass", "dial_timeout", "dnd_from", "dnd_to", "whitelist_active", "max_call_duration"]
    );
}

#[tokio::test]
async fn schema_carries_types_secrets_and_limits() {
    let client = Client::new();
    let (_, _, data) = client.get("/api/config").await;
    let sip = section(&data, "sip");

    assert_eq!(field(sip, "pass")["secret"], true, "Passwort muss Geheimfeld sein");
    assert_eq!(field(sip, "whitelist_active")["type"], "bool");
    assert_eq!(field(sip, "dnd_from")["maximum"], 23);
}

#[tokio::test]
async fn pins_are_marked_required() {
    let client = Client::new();
    let (_, _, data) = client.get("/api/config").await;
    let pins = section(&data, "pins");

    for key in ["nsi", "nsa", "gabel"] {
        assert_eq!(field(pins, key)["required"], true, "{} ist ein Pflichtfeld", key);
    }
}

#[tokio::test]
async fn numbers_are_rendered_as_entries_with_suggestions() {
    let client = Client::new();
    let (_, _, data) = client.get("/api/config").await;
    let numbers = section(&data, "numbers");

    assert_eq!(numbers["kind"], "entries");
    assert_eq!(numbers["entries"], json!([{ "key": "01", "value": "01234567" }]));

    let choices = numbers["choices"].as_array().unwrap();
    assert!(choices.iter().any(|c| c == "reboot"), "Kurzbefehle fehlen");
}

#[tokio::test]
async fn list_options_are_rendered_as_lists() {
    let client = Client::new();
    let (_, _, data) = client.get("/api/config").await;

    let misc = section(&data, "misc");
    let last = misc["fields"].as_array().unwrap().last().unwrap();
    assert_eq!(last["key"], "wake_up_times");
    assert_eq!(last["type"], "list");
}

/// A complete configuration as the browser form would submit it.
fn valid_payload() -> Json_ {
    json!({
        "network": { "wifi_test_host": "10.0.0.1" },
        "sip": {
            "host": "fritz.box", "user": "621", "pass": "geheim", "dial_timeout": "45",
            "dnd_from": "21", "dnd_to": "7", "whitelist_active": "true", "max_call_duration": "10"
        },
        "pins": { "nsi": "23", "nsa": "24", "gabel": "15" },
        "numbers": { "01": "01234567", "07": "+4989123456", "11": "enable-night-mode" },
        "misc": {
            "wake_up_times": ["06:30", "06:30", "06:30", "06:30", "06:30", "07:30", "07:30"]
        },
        "web": { "port": "8123", "user": "", "pass": "" }
    })
}

#[tokio::test]
async fn saves_a_valid_configuration() {
    let client = Client::new();
    let (status, _, result) = client.post("/api/config", &valid_payload().to_string()).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["ok"], true, "{}", result);

    let stored = client.phone.saved.lock().unwrap().last().cloned().expect("gespeichert");

    // Values keep their type: the form sends everything as a string.
    assert_eq!(config::option_int(&stored, "sip", "dial_timeout"), 45);
    assert!(matches!(
        stored.get("sip").and_then(|s| s.as_table()).and_then(|s| s.get("dial_timeout")),
        Some(config::Val::Int(45))
    ));
    assert!(config::option_bool(&stored, "sip", "whitelist_active"));

    // Defaults are filled in for everything the form did not send.
    assert_eq!(config::option_str(&stored, "sip", "host"), "fritz.box");
    assert_eq!(config::option_float(&stored, "misc", "wake_light_duty"), 100.0);

    // The sounds are files, not options: nothing the form sends may bring a
    // section back that no longer exists.
    assert!(!stored.contains_key("sounds"), "Abschnitt [sounds] zurückgekehrt");
    assert!(!stored.contains_key("ringtones"), "Abschnitt [ringtones] zurückgekehrt");
}

#[tokio::test]
async fn keeps_added_short_codes() {
    let client = Client::new();
    client.post("/api/config", &valid_payload().to_string()).await;
    let stored = client.phone.saved.lock().unwrap().last().cloned().unwrap();

    let numbers = stored.get("numbers").and_then(|n| n.as_table()).unwrap();
    assert_eq!(numbers.get("07").map(|v| v.to_display_string()), Some("+4989123456".into()));
    // A short code may also hold one of the built-in commands.
    assert_eq!(numbers.get("11").map(|v| v.to_display_string()), Some("enable-night-mode".into()));
}

#[tokio::test]
async fn rejects_an_invalid_configuration_with_field_errors() {
    let client = Client::new();

    let mut payload = valid_payload();
    payload["pins"] = json!({ "nsi": "99", "nsa": "x", "gabel": "15" });
    payload["numbers"] = json!({ "1234567": "0123" });
    payload["misc"] = json!({ "wake_up_times": ["06:30"] });

    let (status, _, result) = client.post("/api/config", &payload.to_string()).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["ok"], false);

    let errors = &result["errors"];
    assert!(
        errors["pins.nsi"].as_str().unwrap().contains("darf nicht größer als 27"),
        "{}",
        errors
    );
    assert!(errors["pins.nsa"].as_str().unwrap().contains("ganze Zahl"));
    assert!(errors.get("numbers.1234567").is_some(), "zu lange Kurzwahl fehlt");
    assert!(errors["misc.wake_up_times"].as_str().unwrap().contains("genau 7"));

    assert!(client.phone.saved.lock().unwrap().is_empty(), "Nichts speichern");
}

#[tokio::test]
async fn rejects_broken_json() {
    let client = Client::new();
    let (status, _, _) = client.post("/api/config", "kein json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rejects_an_oversized_body() {
    let client = Client::new();
    let huge = format!("{{\"padding\":\"{}\"}}", "x".repeat(1_100_000));
    let (status, _, _) = client.post("/api/config", &huge).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn rejects_unknown_actions_and_paths() {
    let client = Client::new();

    assert_eq!(client.post_action("gibtsnicht").await.0, StatusCode::NOT_FOUND);
    assert_eq!(client.get("/gibtsnicht").await.0, StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------------------- sounds

/// A WAV file, so the upload tests do not need a real recording.
fn wav() -> Vec<u8> {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&[36, 0, 0, 0]);
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(b"\x01\x00\x00\x00\x01\x00\x01\x00");
    bytes
}

/// An MP3 file, to tell the two formats apart.
fn mp3() -> Vec<u8> {
    b"\xff\xfb\x90\x00".to_vec()
}

#[tokio::test]
async fn uploads_a_sound_and_hands_out_the_file_again() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().to_path_buf();
    let client = Client::with_sounds(directory, None);

    // Nothing uploaded yet: there is nothing to play.
    let (status, _, _) = client.get_sound("ringtone").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, result) = client.post_sound("ringtone", &wav()).await;
    assert_eq!(status, StatusCode::OK, "{}", result);
    assert_eq!(result["ok"], true);
    assert_eq!(result["sound"]["present"], true);
    assert_eq!(result["sound"]["file"], "ringtone.wav");
    assert_eq!(result["sound"]["content_type"], "audio/wav");
    assert_eq!(result["sound"]["size"], wav().len());

    // The file lands in the directory under the name the phone looks for, not
    // under whatever the upload was called on the computer it came from.
    assert!(dir.join("ringtone.wav").is_file());

    let (status, headers, bytes) = client.get_sound("ringtone").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, wav());
    assert!(headers.starts_with("audio/wav"), "{}", headers);
    assert!(headers.contains("ringtone.wav"), "{}", headers);

    // And the configuration page now says so.
    let (_, _, data) = client.get("/api/config").await;
    let ringtone = fixed_sounds(&data)
        .iter()
        .find(|sound| sound["key"] == "ringtone")
        .expect("Klingelton");
    assert_eq!(ringtone["present"], true);
}

#[tokio::test]
async fn an_upload_replaces_the_previous_one() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().to_path_buf();
    let client = Client::with_sounds(directory, None);

    client.post_sound("ringtone", &wav()).await;
    let (_, _, result) = client.post_sound("ringtone", &mp3()).await;

    assert_eq!(result["sound"]["file"], "ringtone.mp3");
    assert_eq!(result["sound"]["content_type"], "audio/mpeg");
    assert!(!dir.join("ringtone.wav").exists(), "alte Datei liegen geblieben");

    // The extension follows the content, so the preview announces the right
    // format - a MP3 served as audio/wav does not play.
    let (_, headers, bytes) = client.get_sound("ringtone").await;
    assert!(headers.starts_with("audio/mpeg"), "{}", headers);
    assert_eq!(bytes, mp3());
}

#[tokio::test]
async fn refuses_an_upload_that_is_no_sound() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().to_path_buf();
    let client = Client::with_sounds(directory, None);

    let (status, _, result) = client.post_sound("ringtone", b"<html>404</html>").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(result["ok"], false);
    assert!(result["error"].as_str().unwrap().contains("WAV"), "{}", result);
    assert_eq!(
        client.post_file("sleep", "track.mp3", b"<html>404</html>").await.0,
        StatusCode::BAD_REQUEST
    );
    // Nothing was written: a file that cannot be played would only be found out
    // about when the phone tries to ring.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[tokio::test]
async fn refuses_an_empty_or_unknown_sound() {
    let client = Client::new();

    let (status, _, result) = client.post_sound("ringtone", b"").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(result["ok"], false);

    // The name in the URL selects a slot and nothing else: a name that looks
    // like a path does not name one, so it cannot write outside the directory.
    let (status, _, result) = client.post_sound("%2e%2e%2fconfig.toml", &wav()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", result);
    assert!(result["error"].as_str().unwrap().contains("config.toml"), "{}", result);

    // The same for the collections, which are named in the URL too.
    let (status, _, result) = client.post_file("gibtsnicht", "Ring-01.wav", &wav()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", result);
    assert!(result["error"].as_str().unwrap().contains("gibtsnicht"), "{}", result);
}

/// A file of a collection keeps the name it was uploaded under, because that
/// name is what the phone reads: for a track it is simply its own name, and a
/// ringtone is named after the number it belongs to.
#[tokio::test]
async fn a_file_in_a_collection_keeps_its_name() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().to_path_buf();
    let client = Client::with_sounds(directory, None);

    let (status, _, result) = client.post_file("ringtones", "Ring-01.wav", &wav()).await;
    assert_eq!(status, StatusCode::OK, "{}", result);
    assert_eq!(result["ok"], true);
    // Der Name, den der Browser meldet, entscheidet nichts: die Rufnummer in ihm
    // macht das Gerät zu dem, was das Telefon später liest.
    assert_eq!(result["file"]["name"], "ringtone-01.wav");
    assert_eq!(result["file"]["size"], wav().len());
    assert_eq!(result["file"]["caller"], "01");

    let (_, _, result) = client.post_file("ringtones", "08912345.wav", &mp3()).await;
    assert_eq!(result["file"]["caller"], "08912345");
    assert_eq!(result["file"]["name"], "ringtone-08912345.mp3");

    let (_, _, result) = client.post_file("sleep", "Brahms_Lullaby.mp3", &mp3()).await;
    assert_eq!(result["file"]["name"], "Brahms_Lullaby.mp3");
    assert_eq!(result["file"]["content_type"], "audio/mpeg");

    assert!(dir.join("ringtones/ringtone-01.wav").is_file());
    assert!(dir.join("sleep/Brahms_Lullaby.mp3").is_file());
    // The collections are directories of their own, so a fixed sound and a
    // track can have the same name without fighting over one file.
    assert!(!dir.join("Brahms_Lullaby.mp3").exists());

    // And the page lists them under the collection they belong to.
    let (_, _, data) = client.get("/api/config").await;
    let ringtones: Vec<&str> = pool_files(&data, "ringtones")
        .iter()
        .map(|file| file["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        ringtones,
        vec!["ringtone-01.wav", "ringtone-08912345.mp3"],
        "nach Name geordnet: {:?}",
        ringtones
    );
    assert_eq!(pool_files(&data, "sleep").len(), 1);
}

#[tokio::test]
async fn a_file_of_a_collection_is_handed_out_for_the_preview() {
    let directory = tempfile::tempdir().unwrap();
    let client = Client::with_sounds(directory, None);
    client.post_file("sleep", "titel.mp3", &mp3()).await;

    let (status, headers, bytes) = client.get_file("sleep", "titel.mp3").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, mp3());
    assert!(headers.starts_with("audio/mpeg"), "{}", headers);

    // The name is part of what identifies the file, so the preview has to be
    // found under the name it is stored under - including the extension the
    // content decided on.
    assert_eq!(client.get_file("sleep", "gibtsnicht.mp3").await.0, StatusCode::NOT_FOUND);
    assert_eq!(client.get_file("ringtones", "titel.mp3").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_file_of_a_collection_can_be_deleted() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().to_path_buf();
    let client = Client::with_sounds(directory, None);

    client.post_file("ringtones", "ringtone-01.wav", &wav()).await;
    client.post_file("ringtones", "ringtone-02.wav", &wav()).await;
    assert!(dir.join("ringtones/ringtone-02.wav").is_file());

    let (status, _, result) = client.delete_file("ringtones", "ringtone-01.wav").await;
    assert_eq!(status, StatusCode::OK, "{}", result);
    assert_eq!(result["ok"], true);
    assert!(!dir.join("ringtones/ringtone-01.wav").exists());
    assert!(dir.join("ringtones/ringtone-02.wav").exists(), "die andere Datei blieb");

    let (_, _, data) = client.get("/api/config").await;
    let names: Vec<&str> = pool_files(&data, "ringtones")
        .iter()
        .map(|file| file["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["ringtone-02.wav"]);

    // Deleting it a second time says what went wrong instead of pretending.
    let (status, _, _) = client.delete_file("ringtones", "ringtone-01.wav").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A name from a browser or from a script decides where the file goes, so it is
/// trimmed down to something that can be a file name in that directory - and
/// nothing else.
#[tokio::test]
async fn a_name_lands_inside_the_collection_whatever_it_looks_like() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().to_path_buf();
    let client = Client::with_sounds(directory, None);

    // A path in the name decides nothing: only its last piece is a name, and it
    // is stored under that one - with the extension the content has, not the one
    // the name claimed.
    let (status, _, result) = client
        .post_file("sleep", "..%2F..%2Fconfig.toml", &wav())
        .await;
    assert_eq!(status, StatusCode::OK, "{}", result);
    assert_eq!(result["file"]["name"], "config.wav");
    assert!(dir.join("sleep/config.wav").is_file());
    assert!(!dir.parent().unwrap().join("config.wav").exists(), "außerhalb gelandet");

    // A name that is nothing at all is refused rather than turned into one.
    // Percent-encoded, because a path of `..` would be tidied away on the way in.
    let (status, _, result) = client.post_file("sleep", "%2e%2e", &wav()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", result);
    assert!(result["error"].as_str().unwrap().contains("Dateiname"), "{}", result);
}

#[tokio::test]
async fn an_oversized_sound_is_refused() {
    // A sound file may be far bigger than a configuration, but not endless: the
    // limit is the one the upload route carries, not the one for the form.
    let client = Client::new();
    let huge = vec![0u8; 17_000_000];

    assert_eq!(client.post_sound("ringtone", &huge).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        client.post_file("sleep", "lang.mp3", &huge).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn sounds_are_behind_the_login() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().to_path_buf();
    let client = Client::with_sounds(directory, Some("user:pass"));

    assert_eq!(client.get_sound("ringtone").await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(client.post_sound("ringtone", &wav()).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(client.get_file("ringtones", "Ring-01.wav").await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(client.post_file("ringtones", "Ring-01.wav", &wav()).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(client.delete_file("ringtones", "Ring-01.wav").await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[tokio::test]
async fn triggers_the_configured_actions() {
    let client = Client::new();

    // The action names come from the commands themselves: a new feature adds its
    // web name where it is defined, and this covers it without being told.
    for command in Command::ALL {
        let (status, _, result) = client.post_action(command.web_name()).await;
        assert_eq!(status, StatusCode::OK, "Aktion {}", command.web_name());
        assert_eq!(result["ok"], true);
    }
}

#[tokio::test]
async fn requires_credentials_when_configured() {
    let client = Client::with_credentials(Some("user:pass"));

    // Without a login, everything is refused.
    assert_eq!(client.get("/api/state").await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(client.post("/api/config", "{}").await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(client.post_action("reboot").await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn accepts_correct_credentials() {
    use base64::Engine;

    let client = Client::with_credentials(Some("user:pass"));
    let encoded = base64::engine::general_purpose::STANDARD.encode("user:pass");

    let response = Request::builder()
        .uri("/api/state")
        .header("authorization", format!("Basic {}", encoded))
        .body(Body::empty())
        .unwrap();

    let (status, _, _) = client.send(response).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn pages_are_reachable_without_a_login() {
    // The pages themselves are the login screen's target, so they stay open.
    let client = Client::with_credentials(Some("user:pass"));

    assert_eq!(client.get("/").await.0, StatusCode::OK);
    assert_eq!(client.get("/config").await.0, StatusCode::OK);
}