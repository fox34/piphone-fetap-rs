//! The web interface's view of the phone.
//!
//! Everything the HTTP layer needs is [`PhoneApi`], implemented here rather than
//! in the state machine: the four methods are a view onto the phone's state plus
//! the one operation that writes the configuration file, and keeping them here
//! leaves `mod.rs` about what the phone *does* rather than about how it is
//! displayed.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::Value as Json_;

use super::{modified, Phone};
use crate::command::Command;
use crate::config;
use crate::sync::lock;
use crate::webserver::PhoneApi;

impl PhoneApi for Arc<Phone> {
    fn get_state(&self) -> Json_ {
        serde_json::json!({
            "uptime": self.started_at.elapsed().as_secs(),
            "connected": self.is_connected.load(Ordering::SeqCst),
            "sip_registered": self.sip_registered.load(Ordering::SeqCst),
            "call_active": self.call_up.load(Ordering::SeqCst),
            "hungup": self.is_hungup(),
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
            "config_changed": self.config_changed.load(Ordering::SeqCst),
        })
    }

    fn get_config(&self) -> Json_ {
        let baseline = lock(&self.baseline);
        serde_json::json!({
            "sections": config::describe(&baseline),
            "mtime": *lock(&self.config_mtime),
            "restart_required": self.config_changed.load(Ordering::SeqCst),
        })
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
        *lock(&self.config_mtime) = modified(&self.options.config).unwrap_or(0.0);

        // Ohne echte Änderung ist kein Neustart nötig.
        let has_changes = !changed.is_empty();
        if has_changes {
            self.config_changed.store(true, Ordering::SeqCst);
        }

        serde_json::json!({
            "ok": true,
            "changed": changed,
            "dropped": dropped,
            "restart_required": has_changes
        })
    }

    fn run_action(&self, action: Command) {
        match action {
            Command::NightMode => self.start_night_mode(),
            Command::SleepMusic => self.toggle_sleep_music(),
            Command::TestLoudspeaker => self.test_loudspeaker(),
            Command::TestEarpiece => self.test_earpiece(),
            Command::Reboot => self.reboot(),
            Command::Shutdown => self.shutdown(),
        }
    }
}