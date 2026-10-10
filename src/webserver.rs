//! HTTP interface for status and configuration.
//!
//! Endpoints are unchanged from the Python version:
//!
//! | Route                    | Purpose                                  |
//! | :----------------------- | :--------------------------------------- |
//! | `GET /`                  | status page                             |
//! | `GET /config`            | configuration page                      |
//! | `GET /api/state`         | status as JSON                          |
//! | `GET /api/config`        | schema and current values as JSON       |
//! | `POST /api/config`       | save the configuration                  |
//! | `POST /api/action/<name>`| trigger an action                       |

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::Value as Json_;

use crate::pages::{CONFIG_PAGE, PAGE};

/// Refuse request bodies larger than this, matching the Python version.
const MAX_BODY: usize = 1_000_000;

/// Actions the status page may trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NightMode,
    SleepMusic,
    TestLoudspeaker,
    TestEarpiece,
    Reboot,
    Shutdown,
}

impl Action {
    /// Parse an action name from the URL.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "night-mode" => Action::NightMode,
            "sleep-music" => Action::SleepMusic,
            "test-loudspeaker" => Action::TestLoudspeaker,
            "test-earpiece" => Action::TestEarpiece,
            "reboot" => Action::Reboot,
            "shutdown" => Action::Shutdown,
            _ => return None,
        })
    }
}

/// Callbacks into the phone, supplied by `main`.
pub trait PhoneApi: Send + Sync + 'static {
    /// Current status for the status page.
    fn get_state(&self) -> Json_;

    /// Schema plus current values for the configuration page.
    fn get_config(&self) -> Json_;

    /// Validate and write a new configuration.
    fn save_config(&self, payload: Json_) -> Json_;

    /// Run an action. May block (waiting for a tone, powering down).
    fn run_action(&self, action: Action);
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
        .route("/api/action/{name}", post(api_action))
        .fallback(not_found)
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

    if body.len() > MAX_BODY {
        return json_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            serde_json::json!({"error": "Anfrage zu groß"}),
        );
    }

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

/// Trigger an action from the web interface.
async fn api_action<A: PhoneApi>(
    State(server): State<Arc<WebServer<A>>>,
    headers: axum::http::HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    authorized!(server, &headers);

    let Some(action) = Action::parse(&name) else {
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
        fn run_action(&self, _action: Action) {}
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

    #[test]
    fn action_names_round_trip() {
        assert_eq!(Action::parse("night-mode"), Some(Action::NightMode));
        assert_eq!(Action::parse("shutdown"), Some(Action::Shutdown));
        assert_eq!(Action::parse("unknown"), None);
    }
}
