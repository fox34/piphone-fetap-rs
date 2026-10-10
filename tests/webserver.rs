//! HTTP-level tests for the web interface.
//!
//! Mirrors `tests/test-webserver.py`: the server is driven in-process through
//! the real router, so routing, auth, body limits and status codes are all
//! exercised without needing a socket.

use std::sync::{Arc, Mutex};

use axum::http::{Request, StatusCode};
use axum::body::Body;
use http_body_util::BodyExt;
use serde_json::{json, Value as Json_};
use tower::ServiceExt; // for `oneshot`

use piphone::config;
use piphone::webserver::{router, Action, PhoneApi};

/// A phone that only answers questions; nothing here touches hardware.
struct StubPhone {
    saved: Mutex<Vec<config::Table>>,
    actions: Mutex<Vec<Action>>,
}

impl StubPhone {
    fn new() -> Self {
        StubPhone { saved: Mutex::new(Vec::new()), actions: Mutex::new(Vec::new()) }
    }
}

impl PhoneApi for StubPhone {
    fn get_state(&self) -> Json_ {
        json!({ "uptime": 1 })
    }

    fn get_config(&self) -> Json_ {
        json!({
            "sections": config::describe(&example()),
            "mtime": 1000.0,
            "restart_required": false,
        })
    }

    fn save_config(&self, payload: Json_) -> Json_ {
        let incoming = config::from_json(&payload);
        let (cleaned, errors) = config::validate(&incoming, None);

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

    fn run_action(&self, action: Action) {
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
}

impl Client {
    fn new() -> Self {
        Self::with_credentials(None)
    }

    fn with_credentials(credentials: Option<&str>) -> Self {
        let phone = Arc::new(StubPhone::new());
        let router = router(
            phone.clone(),
            credentials.map(|c| c.to_string()),
            false,
        );
        Client { router, phone }
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
        vec!["network", "sip", "pins", "numbers", "ringtones", "sounds", "misc", "web"]
    );
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

    assert_eq!(field(section(&data, "sounds"), "sleep_music")["type"], "list");

    let misc = section(&data, "misc");
    let last = misc["fields"].as_array().unwrap().last().unwrap();
    assert_eq!(last["key"], "wake_up_times");
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
        "ringtones": { "08912345": "/opt/piphone/sounds/ring-02.wav" },
        "sounds": {
            "ring": "/opt/piphone/sounds/ring-01.wav",
            "sleep_music": ["/opt/a.mp3", "/opt/b.mp3"],
            "waehlen_frei": "/opt/frei.mp3"
        },
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
    assert_eq!(config::option_str(&stored, "sounds", "boot"), "");
    assert_eq!(config::option_float(&stored, "misc", "wake_light_duty"), 100.0);
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

#[tokio::test]
async fn triggers_the_configured_actions() {
    let client = Client::new();

    for name in [
        "night-mode",
        "sleep-music",
        "test-loudspeaker",
        "test-earpiece",
        "reboot",
        "shutdown",
    ] {
        let (status, _, result) = client.post_action(name).await;
        assert_eq!(status, StatusCode::OK, "Aktion {}", name);
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