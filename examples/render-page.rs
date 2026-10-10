//! Prints the web interface's HTML pages and a sample `/api/config` payload.
//!
//! Helper for `tests/test-webpage.js`, which drives the configuration page's
//! JavaScript in a real DOM. Mirrors `tests/render-page.py`.
//!
//!     cargo run --example render-page -- CONFIG_PAGE
//!     cargo run --example render-page -- fixture

use std::path::PathBuf;

use piphone::config;
use piphone::pages::{CONFIG_PAGE, PAGE};

fn main() {
    let Some(what) = std::env::args().nth(1) else {
        eprintln!(
            "Aufruf:\n  cargo run --example render-page -- CONFIG_PAGE\n  cargo run --example render-page -- fixture"
        );
        std::process::exit(1);
    };

    match what.as_str() {
        "PAGE" => print!("{}", PAGE),
        "CONFIG_PAGE" => print!("{}", CONFIG_PAGE),
        "fixture" => print_fixture(),
        other => {
            eprintln!("Unbekannt: {}", other);
            std::process::exit(1);
        }
    }
}

/// The sample configuration, in the shape `/api/config` returns it.
fn print_fixture() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("support/config-example.toml");
    let data = config::read(&path).expect("Beispielkonfiguration lesen");

    let payload = serde_json::json!({
        "sections": config::describe(&data),
        "mtime": 1_700_000_000.0,
        "restart_required": false,
    });

    println!("{}", serde_json::to_string_pretty(&payload).expect("JSON schreiben"));
}