//! The web interface's view of the phone.
//!
//! Everything the HTTP layer needs is [`PhoneApi`], implemented here rather than
//! in the state machine: the four methods are a view onto the phone's state plus
//! the one operation that writes the configuration file, and keeping them here
//! leaves `mod.rs` about what the phone *does* rather than about how it is
//! displayed.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::Value as Json_;

use super::Phone;
use crate::command::Command;
use crate::config;
use crate::sounds;
use crate::sync::lock;
use crate::webserver::PhoneApi;

impl PhoneApi for Arc<Phone> {
    fn get_state(&self) -> Json_ {
        serde_json::json!({
            "uptime": self.started_at.elapsed().as_secs(),
            "connected": self.is_connected.load(Ordering::SeqCst),
            "sip_registered": self.sip_registered.load(Ordering::SeqCst),
            "call_active": self.call_up.load(Ordering::SeqCst),
            "night_light": self.led.night_light_on_state(),
            "wake_up_mode": self.led.wake_light_on_state(),
            "sleep_music": self.sleep_music_running.load(Ordering::SeqCst),
            "dnd": self.is_dnd_active(),
            // Full ISO 8601, because the page hands this to `new Date()`, and a
            // bare "%H:%M" is not a parseable date. The date has to be in there
            // too: the next wake-up may well be tomorrow, and the page shows the
            // weekday.
            "next_wake_up": lock(&self.next_wake_up)
                .map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string()),
        })
    }

    fn get_config(&self) -> Json_ {
        let baseline = lock(&self.baseline);
        serde_json::json!({
            "sections": config::describe(&baseline),
            // Die Klänge stehen nicht in der Datei, sondern sind hochgeladene
            // Dateien. Sie gehören trotzdem hierher: die Seite baut daraus den
            // ganzen Abschnitt, und ein zweiter Abruf nur für die Klänge wäre
            // eine zweite Wahrheit, die nach dem Speichern auseinanderlaufen kann.
            "sounds": sounds::describe(&self.options.sounds_dir),
        })
    }

    fn sounds_dir(&self) -> PathBuf {
        self.options.sounds_dir.clone()
    }

    fn save_config(&self, payload: Json_) -> Json_ {
        // The form sends JSON, the file is TOML: convert, then run the same
        // validation the file would go through.
        let incoming = config::from_json(&payload);
        let baseline = lock(&self.baseline).clone();

        let saved = match config::save(&self.options.config, &incoming, &baseline) {
            Ok(saved) => saved,
            Err(errors) => {
                println!(
                    "Konfiguration konnte nicht gespeichert werden: {}",
                    errors.iter().map(|(_, m)| m).collect::<Vec<_>>().join(", ")
                );
                let map: serde_json::Map<String, Json_> = errors
                    .iter()
                    .map(|(key, message)| (key.to_string(), Json_::String(message.to_string())))
                    .collect();
                return serde_json::json!({ "ok": false, "errors": map });
            }
        };

        let changed = saved.changed;
        let dropped = saved.dropped;
        println!(
            "Konfiguration gespeichert: {}",
            if changed.is_empty() { "keine Änderungen".to_string() } else { changed.join(", ") }
        );

        // Was beim Schreiben aus der Datei gefallen ist, gehört in den Log und in
        // die Antwort: es liest niemand mehr, und es wäre sonst spurlos weg.
        if !dropped.is_empty() {
            println!("Unbekannte Einträge wurden aus der Datei entfernt: {}", dropped.join(", "));
        }

        *lock(&self.baseline) = saved.cleaned;

        if !changed.is_empty() {
            self.restart();
        }

        serde_json::json!({
            "ok": true,
            "changed": changed,
            "dropped": dropped,
        })
    }

    fn run_action(&self, action: Command) {
        // The same dispatch the dial uses, so a command cannot be reachable from
        // one and missing from the other. Whether systemd took a reboot is not
        // this interface's business - it answers before the action runs.
        let _ = self.run_command(action);
    }
}