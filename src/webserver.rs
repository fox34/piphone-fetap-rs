//! HTTP interface for status and configuration.
//!
//! Endpoints are unchanged from the Python version:
//!
//! | Route                          | Purpose                                    |
//! | :----------------------------- | :----------------------------------------- |
//! | `GET /`                        | status page                               |
//! | `GET /config`                  | configuration page                        |
//! | `GET /api/state`               | status as JSON                            |
//! | `GET /api/config`              | schema, values and uploaded sounds as JSON |
//! | `POST /api/config`             | save the configuration                    |
//! | `GET /api/sound/<key>`         | an uploaded sound file, for the preview   |
//! | `POST /api/sound/<key>`        | upload a sound file                       |
//! | `GET /api/sound/<pool>/<name>` | a file of a collection, for the preview   |
//! | `POST /api/sound/<pool>/<name>`| upload a file into a collection            |
//! | `DELETE /api/sound/<pool>/<name>` | delete a file from a collection        |
//! | `POST /api/action/<name>`      | trigger an action                         |

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::Value as Json_;

use crate::command::Command;
use crate::pages::{CONFIG_PAGE, PAGE};
use crate::sounds::{self, Pool, StoreError};

/// Refuse request bodies larger than this, matching the Python version.
///
/// Applied to the router, so an oversized body is turned away while it is still
/// arriving instead of after it has been buffered.
const MAX_BODY: usize = 1_000_000;

/// How much a sound file may be.
///
/// Sound files are orders of magnitude bigger than a configuration, and the
/// sleep music is a whole track rather than a beep, so the general limit does
/// not apply to the upload route. Sixteen megabytes is an hour of music at
/// 128 kbit/s and far more than any of the tones.
const MAX_SOUND: usize = 16_000_000;

/// Callbacks into the phone, supplied by `main`.
pub trait PhoneApi: Send + Sync + 'static {
    /// Current status for the status page.
    fn get_state(&self) -> Json_;

    /// Schema plus current values for the configuration page.
    fn get_config(&self) -> Json_;

    /// Validate and write a new configuration.
    fn save_config(&self, payload: Json_) -> Json_;

    /// Where the uploaded sound files are.
    ///
    /// Asked for per request rather than held in the server: the server does
    /// not own the files, it only reads and replaces the ones the phone plays.
    fn sounds_dir(&self) -> PathBuf;

    /// Run an action. May block (waiting for a tone, powering down).
    fn run_action(&self, action: Command);
}

/// Shared state handed to the handlers.
pub struct WebServer<A: PhoneApi> {
    pub api: Arc<A>,
    /// `user:password` for HTTP Basic Auth, if configured.
    credentials: Option<String>,
    verbose: bool,
}

/// Build the router for a phone implementation.
pub fn router<A: PhoneApi>(api: Arc<A>, credentials: Option<String>, verbose: bool) -> Router {
    let server = WebServer { api, credentials, verbose };

    Router::new()
        .route("/", get(index))
        .route("/config", get(config_page))
        .route("/api/state", get(api_state))
        .route("/api/config", get(api_config).post(post_config))
        // Feste Klänge: der Name im URL wählt den Klang, den es trifft.
        .route(
            "/api/sound/{key}",
            get(api_sound).post(post_sound).layer(axum::extract::DefaultBodyLimit::max(MAX_SOUND)),
        )
        // Sammlungen: der Name im URL ist der Name der Datei, die ersetzt,
        // hinzugefügt oder gelöscht wird.
        .route(
            "/api/sound/{pool}/{name}",
            get(api_collection_file)
                .post(post_collection_file)
                .delete(delete_collection_file)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_SOUND)),
        )
        .route("/api/action/{name}", post(api_action))
        .fallback(not_found)
        // Refuse a body while it is still coming in, rather than reading all of
        // it into memory and measuring afterwards: the whole point of a limit is
        // not to hand the limit's worth of bytes to the process first.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .with_state(Arc::new(server))
}

async fn index() -> Response {
    html(PAGE)
}

async fn config_page() -> Response {
    html(CONFIG_PAGE)
}

/// Unknown paths, including `/favicon.ico`.
async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "Not Found")
        .into_response()
}

fn html(body: &'static str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Require credentials for every route that is not the login-less page.
macro_rules! authorized {
    ($server:expr, $headers:expr) => {
        if let Err(denied) = $server.check_auth($headers) {
            return denied;
        }
    };
}

async fn api_state<A: PhoneApi>(
    State(server): State<Arc<WebServer<A>>>,
    headers: axum::http::HeaderMap,
) -> Response {
    authorized!(server, &headers);
    json(server.api.get_state())
}

async fn api_config<A: PhoneApi>(
    State(server): State<Arc<WebServer<A>>>,
    headers: axum::http::HeaderMap,
) -> Response {
    authorized!(server, &headers);
    json(server.api.get_config())
}

async fn post_config<A: PhoneApi>(
    State(server): State<Arc<WebServer<A>>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    authorized!(server, &headers);

    let payload: Json_ = if body.is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"ok": false, "error": "Ungültiges JSON"}),
                )
            }
        }
    };

    json(server.api.save_config(payload))
}

/// Hand out an uploaded sound file, for the preview in the browser.
///
/// The file is served under its own name and with the content type its format
/// asks for: a preview that is announced as WAV and turns out to be an MP3 is
/// a preview that does not play.
async fn api_sound<A: PhoneApi>(
    State(server): State<Arc<WebServer<A>>>,
    headers: axum::http::HeaderMap,
    AxumPath(key): AxumPath<String>,
) -> Response {
    authorized!(server, &headers);

    // A sound without a file and a sound that does not exist are the same thing
    // to the browser: there is nothing to play.
    match sounds::load(&server.api.sounds_dir(), &key) {
        Ok(found) => play(found, &key).await,
        Err(_) => unknown(format!("Kein Klang: {}", key)),
    }
}

/// A file of one of the collections, for the preview in the browser.
async fn api_collection_file<A: PhoneApi>(
    State(server): State<Arc<WebServer<A>>>,
    headers: axum::http::HeaderMap,
    AxumPath((pool, name)): AxumPath<(String, String)>,
) -> Response {
    authorized!(server, &headers);

    let Some(pool) = Pool::from_key(&pool) else {
        return unknown(format!("Unbekannte Sammlung: {}", pool));
    };

    match pool.load(&server.api.sounds_dir(), &name) {
        Ok(found) => play(found, &name).await,
        Err(_) => unknown(format!("Keine Datei: {}", name)),
    }
}

/// Read a sound file and answer with it.
async fn play((path, format): (PathBuf, sounds::Format), what: &str) -> Response {
    // Reading a whole track takes long enough to be worth keeping off the
    // runtime's workers.
    let to_read = path.clone();
    let bytes = match tokio::task::spawn_blocking(move || std::fs::read(to_read)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => return unreadable(what, e),
        Err(e) => return unreadable(what, e),
    };

    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let disposition = HeaderValue::from_str(&format!("inline; filename=\"{}\"", name))
        .unwrap_or_else(|_| HeaderValue::from_static("inline"));

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(format.content_type())),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        bytes,
    )
        .into_response()
}

/// Take over a sound file.
///
/// The body is the file itself rather than a multipart form: one file, one URL,
/// and no boundary parsing in between. What it is called on the computer it
/// came from does not matter here - the name in the URL says which sound it
/// becomes, and nothing else.
async fn post_sound<A: PhoneApi>(
    State(server): State<Arc<WebServer<A>>>,
    headers: axum::http::HeaderMap,
    AxumPath(key): AxumPath<String>,
    body: Bytes,
) -> Response {
    authorized!(server, &headers);

    let Some(spec) = sounds::spec(&key) else {
        return unknown(format!("Unbekannter Klang: {}", key));
    };

    if let Some(denied) = refuse_empty(&body) {
        return denied;
    }

    let dir = server.api.sounds_dir();
    let bytes = body.to_vec();
    let stored = tokio::task::spawn_blocking(move || spec.store(&dir, &bytes)).await;

    match stored {
        Ok(Ok(sound)) => {
            let file = sound["file"].as_str().unwrap_or("?").to_string();
            println!("Klang {} ({}) gespeichert: {}", spec.label, spec.stem, file);
            json(serde_json::json!({"ok": true, "sound": sound}))
        }
        Ok(Err(error)) => stored_response(spec.label, error),
        Err(e) => unwritable(spec.label, e),
    }
}

/// Put a file into one of the collections.
///
/// The name in the URL says which file it is. For the sleep music that is the
/// name the file keeps, because it is what tells two tracks apart. For the
/// ringtones it is the number the ringtone belongs to, which the server turns
/// into the name the phone later reads it under - the name a browser or a script
/// sends cannot point outside the directory either way.
async fn post_collection_file<A: PhoneApi>(
    State(server): State<Arc<WebServer<A>>>,
    headers: axum::http::HeaderMap,
    AxumPath((pool, name)): AxumPath<(String, String)>,
    body: Bytes,
) -> Response {
    authorized!(server, &headers);

    let Some(pool) = Pool::from_key(&pool) else {
        return unknown(format!("Unbekannte Sammlung: {}", pool));
    };

    if let Some(denied) = refuse_empty(&body) {
        return denied;
    }

    let label = pool.spec().label;
    let dir = server.api.sounds_dir();
    let bytes = body.to_vec();
    let stored = tokio::task::spawn_blocking(move || pool.store(&dir, &name, &bytes)).await;

    match stored {
        Ok(Ok(file)) => {
            println!("{} gespeichert: {} ({} Bytes)", label, file.name, file.size());
            json(serde_json::json!({"ok": true, "file": file.describe()}))
        }
        Ok(Err(error)) => stored_response(label, error),
        Err(e) => unwritable(label, e),
    }
}

/// Take a file out of one of the collections again.
async fn delete_collection_file<A: PhoneApi>(
    State(server): State<Arc<WebServer<A>>>,
    headers: axum::http::HeaderMap,
    AxumPath((pool, name)): AxumPath<(String, String)>,
) -> Response {
    authorized!(server, &headers);

    let Some(pool) = Pool::from_key(&pool) else {
        return unknown(format!("Unbekannte Sammlung: {}", pool));
    };

    let label = pool.spec().label;
    let dir = server.api.sounds_dir();
    let deleted = name.clone();
    let removed = tokio::task::spawn_blocking(move || pool.delete(&dir, &name)).await;

    match removed {
        Ok(Ok(())) => {
            println!("{} gelöscht: {}", label, deleted);
            json(serde_json::json!({"ok": true}))
        }
        Ok(Err(error)) => stored_response(label, error),
        Err(e) => unwritable(label, e),
    }
}

/// A request that carries no file.
fn refuse_empty(body: &Bytes) -> Option<Response> {
    if body.is_empty() {
        return Some(json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"ok": false, "error": "Leere Datei."}),
        ));
    }
    None
}

/// What came of storing or removing a file, as the answer.
fn stored_response(what: &str, error: StoreError) -> Response {
    match error {
        // All three are the uploader's mistake, and the only ones worth saying in
        // the page rather than only in the log.
        StoreError::Unsupported | StoreError::Name | StoreError::Caller => json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"ok": false, "error": error.to_string()}),
        ),
        StoreError::Missing => unknown(format!("Keine Datei: {}", what)),
        StoreError::Io(message) => {
            println!("{} nicht verarbeitbar: {}", what, message);
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({"ok": false, "error": message}),
            )
        }
    }
}

/// Something the server does not know.
fn unknown(message: String) -> Response {
    json_response(StatusCode::NOT_FOUND, serde_json::json!({"ok": false, "error": message}))
}

/// A sound file that is there but cannot be read.
fn unreadable(what: &str, cause: impl std::fmt::Display) -> Response {
    println!("{} nicht lesbar: {}", what, cause);
    json_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        serde_json::json!({"ok": false, "error": "Datei nicht lesbar."}),
    )
}

/// A sound file that cannot be written.
fn unwritable(what: &str, cause: impl std::fmt::Display) -> Response {
    println!("{} nicht speicherbar: {}", what, cause);
    json_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        serde_json::json!({"ok": false, "error": "Datei nicht schreibbar."}),
    )
}

/// Trigger an action from the web interface.
async fn api_action<A: PhoneApi>(
    State(server): State<Arc<WebServer<A>>>,
    headers: axum::http::HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    authorized!(server, &headers);

    let Some(action) = Command::from_web(&name) else {
        return json_response(
            StatusCode::NOT_FOUND,
            serde_json::json!({"error": format!("Unbekannte Aktion: {}", name)}),
        );
    };

    if server.verbose {
        println!("--> web: Aktion {}", name);
    }

    // Answer first, then run the action: it may wait for a tone to finish or
    // shut the machine down, which would otherwise stall the response.
    let response = json_response(StatusCode::OK, serde_json::json!({"ok": true, "action": name}));

    let api = server.api.clone();
    tokio::task::spawn_blocking(move || api.run_action(action));

    response
}

fn json(value: Json_) -> Response {
    json_response(StatusCode::OK, value)
}

fn json_response(status: StatusCode, value: Json_) -> Response {
    let body = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
    (
        status,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response()
}

impl<A: PhoneApi> WebServer<A> {
    /// Verify HTTP Basic Auth when the phone is configured with a login.
    fn check_auth(&self, headers: &axum::http::HeaderMap) -> Result<(), Response> {
        let Some(credentials) = &self.credentials else {
            return Ok(()); // no access protection configured
        };

        let provided = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Basic "))
            .and_then(|encoded| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(encoded).ok()
            })
            .and_then(|decoded| String::from_utf8(decoded).ok());

        match provided {
            Some(provided) if constant_time_eq(&provided, credentials) => Ok(()),
            _ => {
                let mut response = (
                    StatusCode::UNAUTHORIZED,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "Unauthorized",
                )
                    .into_response();
                response.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Basic realm=\"PiPhone\""),
                );
                Err(response)
            }
        }
    }
}

/// Compare without leaking the position of the first difference.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn server_with(credentials: Option<&str>) -> WebServer<StubApi> {
        WebServer {
            api: Arc::new(StubApi),
            credentials: credentials.map(|c| c.to_string()),
            verbose: false,
        }
    }

    struct StubApi;

    impl PhoneApi for StubApi {
        fn get_state(&self) -> Json_ {
            serde_json::json!({"uptime": 1})
        }
        fn get_config(&self) -> Json_ {
            serde_json::json!({})
        }
        fn save_config(&self, _payload: Json_) -> Json_ {
            serde_json::json!({"ok": true})
        }
        fn sounds_dir(&self) -> PathBuf {
            PathBuf::from("/tmp")
        }
        fn run_action(&self, _action: Command) {}
    }

    fn header(value: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    fn encode(credentials: &str) -> String {
        use base64::Engine;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials)
        )
    }

    #[test]
    fn allows_everything_without_credentials() {
        let server = server_with(None);
        assert!(server.check_auth(&axum::http::HeaderMap::new()).is_ok());
    }

    #[test]
    fn accepts_matching_credentials() {
        let server = server_with(Some("user:pass"));
        assert!(server.check_auth(&header(&encode("user:pass"))).is_ok());
    }

    #[test]
    fn rejects_wrong_or_missing_credentials() {
        let server = server_with(Some("user:pass"));
        assert!(server.check_auth(&header(&encode("user:wrong"))).is_err());
        assert!(server.check_auth(&axum::http::HeaderMap::new()).is_err());
        assert!(server.check_auth(&header("Bearer token")).is_err());
    }

    #[test]
    fn sends_www_authenticate_header() {
        let server = server_with(Some("user:pass"));
        let response = server.check_auth(&axum::http::HeaderMap::new()).unwrap_err();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
    }
}
