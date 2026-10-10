//! PiPhone - a headless SIP telephone built into a historical rotary dial phone.
//!
//! Runs on a Raspberry Pi Zero 2W inside an FeTAp-style phone: a rotary dial
//! provides the short codes, a hook switch handles the handset, two LEDs show
//! night and wake-up mode, and SIP calls are handled by the `rvoip` stack.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, NaiveTime, Timelike};
use rand::seq::SliceRandom;
use serde_json::Value as Json_;
use tokio::sync::mpsc;

use crate::audio::Audio;
use crate::config::{self, Table};
use crate::gpio::{Gpio, Led, Watcher};
use crate::sip::{CallDecision, SipClient, SipEvent};
use crate::webserver::{Action, PhoneApi};

/// ALSA device holding the earpiece and microphone. See
/// [`crate::callaudio::CALL_DEVICE`] for why the plugin wrapper is needed.
const EARPIECE_ALSA: &str = crate::callaudio::CALL_DEVICE;

/// Longest short code the rotary dial may produce.
const MAX_DIAL_CODE: usize = config::MAX_DIAL_CODE;

/// Command-line options.
#[derive(Debug, Clone)]
pub struct Options {
    pub config: PathBuf,
    pub ignore_dnd: bool,
    pub no_web: bool,
    pub silent_boot: bool,
    pub verbose: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            config: PathBuf::from("/boot/piphone/config.toml"),
            ignore_dnd: false,
            no_web: false,
            silent_boot: false,
            verbose: false,
        }
    }
}

/// Everything the phone needs, shared between the phone logic and the web server.
pub struct Phone {
    options: Options,
    config: Table,
    /// Normalised configuration as a baseline for the web interface.
    baseline: Mutex<Table>,
    config_mtime: Mutex<f64>,
    config_changed: AtomicBool,

    audio: Audio,
    led: Arc<Led>,

    /// Is the handset resting on the cradle? Kept separately from the watcher
    /// thread so the web interface and the call logic can read it.
    hungup: Arc<AtomicBool>,

    /// Handle of the runtime this phone was started on.
    ///
    /// The GPIO watcher runs on a plain thread, where `tokio::spawn` panics for
    /// lack of a reactor. Every timer is therefore started through this handle
    /// instead, which works from any thread.
    runtime: tokio::runtime::Handle,

    started_at: Instant,
    is_connected: AtomicBool,
    call_incoming: AtomicBool,
    declined_incoming_call: AtomicBool,
    manual_dnd: AtomicBool,
    sleep_music_running: AtomicBool,
    stop_sleep_music: AtomicBool,
    next_wake_up: Mutex<Option<NaiveTime>>,

    /// Set once the SIP stack reported its first registration; gates the boot sound.
    first_boot: AtomicBool,

    /// The endpoint is registered at the SIP server.
    sip_registered: AtomicBool,

    /// A call is up, in either direction.
    call_up: AtomicBool,

    /// Digits dialled so far, reset when the handset moves or a code matches.
    dial_digits: Mutex<String>,

    /// The call that is currently ringing, awaiting the user's decision.
    pending_call: Mutex<Option<mpsc::Sender<CallDecision>>>,
    ring_timer: Mutex<Option<tokio::task::JoinHandle<()>>>,

    /// Wake-up timer, cancelled when night mode is switched off again.
    night_light_timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    dial_timeout: Mutex<Option<tokio::task::JoinHandle<()>>>,
    call_duration_timer: Mutex<Option<tokio::task::JoinHandle<()>>>,

    /// Commands sent to the SIP task.
    ///
    /// Behind a lock so it can be swapped together with the receiver if a
    /// session is lost; see [`Phone::send_command`].
    sip_commands: Mutex<mpsc::Sender<SipCommand>>,
    /// Receiving end, owned by the phone and lent to the running SIP session.
    ///
    /// `None` while a session holds it; the session gives it back when it ends,
    /// so a command issued during a reconnect is not lost.
    sip_receiver: Mutex<Option<mpsc::Receiver<SipCommand>>>,
}

/// Command channel into the SIP task.
pub enum SipCommand {
    /// Place a call to a number.
    Call { number: String, host: String },
    /// End the current call.
    Hangup,
    /// Shut the SIP stack down.
    Shutdown,
}

impl Phone {
    /// Read the configuration and set up hardware.
    pub async fn new(options: Options) -> Result<Arc<Self>, String> {
        if !options.config.exists() {
            return Err(format!("Konfigurationsdatei {} nicht gefunden.", options.config.display()));
        }

        let mut config = config::read(&options.config)
            .map_err(|e| format!("Konfigurationsdatei {} ist kein gültiges TOML: {}", options.config.display(), e))?;

        // The baseline is taken before --ignore-dnd so that flag never reaches the file.
        let baseline = config::validate(&config, None).0;

        if options.ignore_dnd {
            if let Some(sip) = section_mut(&mut config, "sip") {
                sip.insert("dnd_from".to_string(), config::Val::Int(0));
                sip.insert("dnd_to".to_string(), config::Val::Int(0));
            }
        }

        let config_mtime = modified(&options.config).unwrap_or(0.0);

        let gpio = Gpio::new()?;

        let led = Arc::new(Led::new(
            &gpio,
            config::option_int(&config, "misc", "night_light_pin") as u32,
            config::option_float(&config, "misc", "night_light_duty"),
            config::option_int(&config, "misc", "wake_light_pin") as u32,
            config::option_float(&config, "misc", "wake_light_duty"),
            options.verbose,
        )?);
        led.wake_light_blink(); // signal that the phone is starting

        let mut watcher = Watcher::new(
            &gpio,
            config::option_int(&config, "pins", "nsi") as u32,
            config::option_int(&config, "pins", "nsa") as u32,
            config::option_int(&config, "pins", "gabel") as u32,
        )?;
        let hungup = Arc::new(AtomicBool::new(watcher.is_hungup()));

        // Captured while still inside the runtime, so timers can be started from
        // the GPIO thread later on.
        let runtime = tokio::runtime::Handle::current();

        // SIP commands are buffered so a call can be queued while the endpoint
        // is still starting up.
        let (sip_commands, sip_receiver) = mpsc::channel(16);
        let sip_receiver = Mutex::new(Some(sip_receiver));

        let phone = Arc::new(Phone {
            options,
            config,
            baseline: Mutex::new(baseline),
            config_mtime: Mutex::new(config_mtime),
            config_changed: AtomicBool::new(false),
            audio: Audio::default(),
            led,
            hungup,
            runtime,
            started_at: Instant::now(),
            is_connected: AtomicBool::new(false),
            call_incoming: AtomicBool::new(false),
            declined_incoming_call: AtomicBool::new(false),
            manual_dnd: AtomicBool::new(false),
            sleep_music_running: AtomicBool::new(false),
            stop_sleep_music: AtomicBool::new(false),
            next_wake_up: Mutex::new(None),
            first_boot: AtomicBool::new(true),
            sip_registered: AtomicBool::new(false),
            call_up: AtomicBool::new(false),
            dial_digits: Mutex::new(String::new()),
            pending_call: Mutex::new(None),
            ring_timer: Mutex::new(None),
            night_light_timer: Mutex::new(None),
            dial_timeout: Mutex::new(None),
            call_duration_timer: Mutex::new(None),
            sip_commands: Mutex::new(sip_commands),
            sip_receiver,
        });


        // Rotary dial and hook switch share one sampling thread: the dial sets
        // the pace at 1 ms, which is far more often than the hook needs.
        let dial_phone = phone.clone();
        let hungup_flag = phone.hungup.clone();
        std::thread::Builder::new()
            .name("gpio-watch".to_string())
            .spawn(move || {
                let mut on_digit = |digit: u32| dial_phone.on_digit(digit);
                let mut on_hook = |hung: bool| {
                    hungup_flag.store(hung, Ordering::SeqCst);
                    dial_phone.on_hook_change(hung);
                };
                watcher.run(&mut on_digit, &mut on_hook);
            })
            .map_err(|e| format!("Nummernschalter nicht startbar: {}", e))?;

        Ok(phone)
    }

    // ---------------------------------------------------------------- config helpers

    /// The configuration this phone was started with.
    pub fn config(&self) -> &Table {
        &self.config
    }

    /// Command-line options in effect.
    pub fn options(&self) -> &Options {
        &self.options
    }

    /// Is the handset resting on the cradle?
    fn is_hungup(&self) -> bool {
        self.hungup.load(Ordering::SeqCst)
    }

    /// A sound path from the configuration.
    fn sound(&self, key: &str) -> String {
        config::option_str(&self.config, "sounds", key)
    }

    /// Numbers the rotary dial may call or which may call us.
    pub fn numbers(&self) -> Vec<(String, String)> {
        self.config
            .get("numbers")
            .and_then(|v| v.as_table())
            .map(|table| table.iter().map(|(k, v)| (k.clone(), v.to_display_string())).collect())
            .unwrap_or_default()
    }

    /// Ringtone configured for a caller, falling back to the generic one.
    fn ringtone_for(&self, caller: &str) -> String {
        self.config
            .get("ringtones")
            .and_then(|v| v.as_table())
            .and_then(|table| table.get(caller))
            .map(|v| v.to_display_string())
            .unwrap_or_else(|| self.sound("ring"))
    }

    // ---------------------------------------------------------------- rotary dial

    /// A digit came in from the rotary dial.
    fn on_digit(self: &Arc<Self>, digit: u32) {
        let mut digits = self.dial_digits.lock().unwrap_or_else(|e| e.into_inner());
        if digits.len() >= MAX_DIAL_CODE {
            // Too many digits: abandon this attempt.
            println!("Ziffernfolge zu lang, beende Wahlvorgang.");
            digits.clear();
            drop(digits);
            self.stop_dial_timeout();
            self.audio.play_earpiece(&self.sound("waehlen_ungueltig"));
            return;
        }

        digits.push(char::from_digit(digit, 10).unwrap_or('0'));
        let number = digits.clone();
        drop(digits);

        // A code fires as soon as it is complete, so longer codes still work.
        let action = self
            .numbers()
            .into_iter()
            .find(|(code, _)| *code == number)
            .map(|(_, a)| a);

        if let Some(action) = action {
            self.dial_digits.lock().unwrap_or_else(|e| e.into_inner()).clear();
            println!("Gewählt: {} -> {}", number, action);
            self.stop_dial_timeout();
            self.audio.stop_earpiece();
            self.run_dial_action(&action);
        }
    }

    /// Perform the action a short code stands for.
    fn run_dial_action(self: &Arc<Self>, action: &str) {
        match action {
            "enable-night-mode" => {
                self.start_night_mode();
                self.audio.play_speaker(&self.sound("action_confirmed")).wait();
            }
            "play-sleep-music" => self.toggle_sleep_music(),
            "test-loudspeaker" => self.test_loudspeaker(),
            "test-earpiece" => self.test_earpiece(),
            "reboot" => {
                self.reboot();
                std::process::exit(0);
            }
            "shutdown" => {
                self.shutdown();
                std::process::exit(0);
            }
            number => {
                if !self.is_connected.load(Ordering::SeqCst) {
                    self.audio.play_earpiece(&self.sound("waehlen_besetzt"));
                    return;
                }
                println!("Rufe Nummer an: {}", number);
                let host = config::option_str(&self.config, "sip", "host");
                self.send_command(SipCommand::Call {
                    number: number.to_string(),
                    host,
                });

                // Limit the length of outgoing calls.
                let minutes = config::option_int(&self.config, "sip", "max_call_duration");
                if minutes > 0 {
                    println!("Maximale Anrufdauer: {} Minuten", minutes);
                    self.start_call_duration_timer(minutes as u64 * 60);
                }
            }
        }
    }

    fn start_dial_timeout(self: &Arc<Self>) {
        self.stop_dial_timeout();
        let seconds = config::option_int(&self.config, "sip", "dial_timeout");
        let phone = self.clone();
        let handle = self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds.max(0) as u64)).await;
            phone.cancel_dialing();
        });
        *self.dial_timeout.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    fn stop_dial_timeout(&self) {
        if let Some(handle) = self.dial_timeout.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }

    fn start_call_duration_timer(self: &Arc<Self>, seconds: u64) {
        if let Some(handle) = self.call_duration_timer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
        let phone = self.clone();
        let handle = self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            phone.timeout_call();
        });
        *self.call_duration_timer.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// Give up dialling after the configured timeout.
    fn cancel_dialing(self: &Arc<Self>) {
        println!("Wählvorgang nach Timeout automatisch abgebrochen.");
        self.dial_digits.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.stop_dial_timeout();
        // Repeats, because there is nobody around to pick the handset up.
        self.audio.play_earpiece_repeat(&self.sound("waehlen_besetzt"));
    }

    /// End an outgoing call that ran past its limit.
    fn timeout_call(self: &Arc<Self>) {
        println!("Maximale Telefondauer erreicht. Gespräch wird beendet.");
        self.send_command(SipCommand::Hangup);
        self.audio.play_earpiece(&self.sound("waehlen_besetzt"));
    }

    // ---------------------------------------------------------------- hook switch

    /// The handset was lifted or put down.
    fn on_hook_change(self: &Arc<Self>, hungup: bool) {
        if hungup {
            println!("Hörer aufgelegt");

            // A call that was still ringing stops ringing now.
            if self.call_incoming.swap(false, Ordering::SeqCst) {
                self.audio.stop_speaker();
                self.hang_up_incoming(CallDecision::Decline);
            }

            self.dial_digits.lock().unwrap_or_else(|e| e.into_inner()).clear();
            self.stop_dial_timeout();
            self.audio.stop_earpiece();
            self.send_command(SipCommand::Hangup);
            self.declined_incoming_call.store(false, Ordering::SeqCst);

            if let Some(handle) = self.call_duration_timer.lock().unwrap_or_else(|e| e.into_inner()).take() {
                handle.abort();
            }
        } else {
            println!("Hörer abgehoben");

            // Answering a call that is ringing.
            if self.call_incoming.load(Ordering::SeqCst) {
                self.audio.stop_earpiece();
                self.audio.stop_speaker();
                self.hang_up_incoming(CallDecision::Answer);
                return;
            }

            // Dial tone, but only when telephony is actually available.
            if self.is_connected.load(Ordering::SeqCst) {
                self.audio.play_earpiece(&self.sound("waehlen_frei"));
            } else {
                self.audio.play_earpiece(&self.sound("waehlen_nicht_verbunden"));
            }

            self.dial_digits.lock().unwrap_or_else(|e| e.into_inner()).clear();
            self.start_dial_timeout();
        }
    }

    // ---------------------------------------------------------------- SIP events

    /// Handle an event from the SIP stack.
    pub async fn on_sip_event(self: &Arc<Self>, event: SipEvent) {
        match event {
            SipEvent::IncomingCall { caller, reply } => self.incoming_call(&caller, reply).await,
            SipEvent::CallAnswered => {
                self.call_incoming.store(false, Ordering::SeqCst);
                self.call_up.store(true, Ordering::SeqCst);
                self.on_sip_ready();
            }
            SipEvent::CallEnded => self.hung_up(),
            SipEvent::RegistrationChanged { registered } => {
                self.sip_registered.store(registered, Ordering::SeqCst);
                if registered {
                    self.on_sip_ready();
                }
            }
        }
    }

    /// The SIP stack finished starting: play the boot sound once.
    fn on_sip_ready(&self) {
        if self.first_boot.swap(false, Ordering::SeqCst) {
            if !self.options.silent_boot {
                self.audio.play_speaker(&self.sound("boot"));
            }
            self.led.wake_light_off();
        }
    }

    /// An incoming call is ringing: decide whether to let it through.
    ///
    /// The decision is not taken here: the call stays ringing until the handset
    /// is lifted or the ring timeout expires, and both of those are handled by
    /// [`Phone::on_hook_change`] and the timer below. Blocking here would keep
    /// the SIP event loop from handling anything else.
    async fn incoming_call(self: &Arc<Self>, caller: &str, reply: mpsc::Sender<CallDecision>) {
        println!("Eingehender Anruf von {}", caller);

        // Never ring while the handset is off the cradle or do-not-disturb is on.
        let decline = if !self.is_hungup() || self.is_dnd_active() {
            println!("Hörer ist abgehoben oder Klingelsperre ist aktiv: weise Anruf ab");
            true
        } else if config::option_bool(&self.config, "sip", "whitelist_active") {
            println!("Whitelist aktiv, prüfe Anrufer.");
            if self.is_known_caller(caller) {
                false
            } else {
                println!("Anrufer nicht in hinterlegten Nummern: weise Anruf ab");
                true
            }
        } else {
            false
        };

        if decline {
            self.declined_incoming_call.store(true, Ordering::SeqCst);
            let _ = reply.send(CallDecision::Decline).await;
            return;
        }

        self.call_incoming.store(true, Ordering::SeqCst);
        self.audio.stop_earpiece();
        self.audio.play_speaker_repeat(&self.ringtone_for(caller));

        // Hand the decision to the hook handler and give up after the ring timeout.
        *self.pending_call.lock().unwrap_or_else(|e| e.into_inner()) = Some(reply);
        self.start_ring_timeout();
    }

    /// Give up on a call nobody picked up.
    fn start_ring_timeout(self: &Arc<Self>) {
        let phone = self.clone();
        let handle = self.runtime.spawn(async move {
            tokio::time::sleep(RING_TIMEOUT).await;
            phone.ring_timeout();
        });
        *self.ring_timer.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    fn stop_ring_timeout(self: &Arc<Self>) {
        if let Some(handle) = self.ring_timer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }

    /// Nobody lifted the handset: stop ringing and let the call go.
    fn ring_timeout(self: &Arc<Self>) {
        println!("Kein Abheben während des Klingelns, Anruf wird beendet.");
        self.hang_up_incoming(CallDecision::Decline);
    }

    /// Stop ringing and pass the decision to the SIP task.
    fn hang_up_incoming(self: &Arc<Self>, decision: CallDecision) {
        let reply = self.pending_call.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.stop_ring_timeout();
        self.call_incoming.store(false, Ordering::SeqCst);
        self.audio.stop_speaker();

        match reply {
            // Answering and declining both happen inside the SIP task.
            Some(reply) => {
                self.runtime.spawn(async move {
                    reply.send(decision).await.ok();
                });
            }
            // No call was waiting, so this was an ongoing conversation.
            None => self.send_command(SipCommand::Hangup),
        }
    }

/// Is this caller listed in `[numbers]`, in any of the usual formats?
    fn is_known_caller(&self, caller: &str) -> bool {
        let numbers: Vec<String> = self.numbers().into_iter().map(|(_, target)| target).collect();
        known_caller(caller, &numbers)
    }

    /// A call ended, either by us or by the other side.
    fn hung_up(self: &Arc<Self>) {
        println!("Anruf beendet");
        self.call_up.store(false, Ordering::SeqCst);

        let was_ringing = self.call_incoming.swap(false, Ordering::SeqCst);
        self.stop_ring_timeout();
        *self.pending_call.lock().unwrap_or_else(|e| e.into_inner()) = None;

        // A call we already declined needs no further cleanup.
        if self.declined_incoming_call.swap(false, Ordering::SeqCst) {
            return;
        }

        self.audio.stop_speaker();

        if let Some(handle) = self.call_duration_timer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }

        // The handset is off the cradle: it is waiting for the next number.
        if !was_ringing && !self.is_hungup() {
            self.audio.play_earpiece(&self.sound("waehlen_besetzt"));
        }
    }

    // ---------------------------------------------------------------- features

    /// Is do-not-disturb active, either on a timer or switched on manually?
    fn is_dnd_active(&self) -> bool {
        if self.manual_dnd.load(Ordering::SeqCst) {
            return true;
        }
        let hour = Local::now().hour() as i64;
        let from = config::option_int(&self.config, "sip", "dnd_from");
        let to = config::option_int(&self.config, "sip", "dnd_to");
        (0 < hour && hour <= to) || (0 < from && from <= hour)
    }

    /// Start or stop night mode.
    pub fn start_night_mode(self: &Arc<Self>) {
        // Without any LED there is nothing to show, so say so instead of
        // silently switching on do-not-disturb.
        if config::option_int(&self.config, "misc", "night_light_pin") == 0
            && config::option_int(&self.config, "misc", "wake_light_pin") == 0
        {
            println!("Kann Nachtmodus nicht aktivieren: Weder Nachtlicht noch Aufwachlicht sind konfiguriert.");
            return;
        }

        self.led.wake_light_off();

        // Already in night mode or waking up: switch it off again.
        let running = self.night_light_timer.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        if running {
            println!("Deaktiviere Nacht-/Aufwachlicht.");
            self.cancel_night_timer();
            self.led.night_light_off();
            self.manual_dnd.store(false, Ordering::SeqCst);
            *self.next_wake_up.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return;
        }

        self.manual_dnd.store(true, Ordering::SeqCst);
        self.led.night_light_on(None);

        let times = config::option_list(&self.config, "misc", "wake_up_times");
        if times.len() != 7 {
            println!("Kann Nachtmodus nicht aktivieren: Wochentage für wake_up_times unvollständig!");
            return;
        }

        let Some(wake_up) = next_wake_up(&times) else {
            println!("Kann Nachtmodus nicht aktivieren: Uhrzeit ungültig.");
            return;
        };

        println!("Aktiviere Nachtlicht bis {}.", wake_up.format("%Y-%m-%d %H:%M"));
        *self.next_wake_up.lock().unwrap_or_else(|e| e.into_inner()) = Some(wake_up.time());

        let seconds = seconds_until(wake_up);
        let phone = self.clone();
        let handle = self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            phone.start_wakeup_light();
        });
        *self.night_light_timer.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    fn cancel_night_timer(&self) {
        if let Some(handle) = self.night_light_timer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }

    /// Turn the wake-up light on for two hours.
    fn start_wakeup_light(self: &Arc<Self>) {
        println!("Aktiviere Aufwachlicht für zwei Stunden.");
        self.led.night_light_on(Some(100.0)); // night light at full brightness
        self.led.wake_light_on(None);

        let phone = self.clone();
        let handle = self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_secs(2 * 60 * 60)).await;
            phone.stop_wakeup_light();
        });
        *self.night_light_timer.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    fn stop_wakeup_light(self: &Arc<Self>) {
        println!("Deaktiviere Aufwachlicht.");
        self.led.night_light_off();
        self.led.wake_light_off();
        self.cancel_night_timer();
        self.manual_dnd.store(false, Ordering::SeqCst);
        *self.next_wake_up.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Start or stop the sleep music.
    pub fn toggle_sleep_music(self: &Arc<Self>) {
        if self.sleep_music_running.load(Ordering::SeqCst) {
            println!("Stoppe Einschlafmusik.");
            self.stop_sleep_music.store(true, Ordering::SeqCst);
            self.audio.stop_speaker();
            return;
        }

        self.stop_sleep_music.store(false, Ordering::SeqCst);
        self.sleep_music_running.store(true, Ordering::SeqCst);

        let phone = self.clone();
        // Through the stored runtime handle: this is reached from the GPIO
        // watcher thread, where a bare `spawn_blocking` has no reactor to
        // attach to and panics.
        self.runtime.spawn_blocking(move || phone.play_sleep_music());
    }

    /// Play the sleep music; runs on a blocking thread because it waits for playback.
    fn play_sleep_music(self: &Arc<Self>) {
        let tracks = config::option_list(&self.config, "sounds", "sleep_music");
        if tracks.is_empty() {
            println!("Kann Einschlafmusik nicht starten: keine Datei angegeben!");
            self.sleep_music_running.store(false, Ordering::SeqCst);
            return;
        }

        self.manual_dnd.store(true, Ordering::SeqCst);

        if tracks.len() == 1 {
            println!("Spiele Einschlafmusik.");
            self.audio.play_speaker(&tracks[0]).wait();
        } else {
            // Several tracks: play in random order for at least 15 minutes.
            let mut order = tracks.clone();
            order.shuffle(&mut rand::rng());
            let start = Instant::now();

            for track in order {
                if self.stop_sleep_music.load(Ordering::SeqCst) {
                    if self.options.verbose {
                        println!("Einschlafmusik auf Anforderung gestoppt.");
                    }
                    break;
                }

                println!("Spiele Einschlafmusik: {}", track);
                self.audio.play_speaker(&track).wait();

                if start.elapsed() >= Duration::from_secs(15 * 60) {
                    if self.options.verbose {
                        println!("Einschlafmusik nach 15+ Minuten gestoppt.");
                    }
                    break;
                }

                if !self.is_hungup() {
                    if self.options.verbose {
                        println!("Einschlafmusik durch Hörer gestoppt.");
                    }
                    break;
                }
            }
        }

        if self.options.verbose {
            println!("Einschlafmusik beendet.");
        }

        // Do not disturb was only meant for the music.
        let night_active = self.night_light_timer.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        if !night_active {
            self.manual_dnd.store(false, Ordering::SeqCst);
        }

        self.stop_sleep_music.store(false, Ordering::SeqCst);
        self.sleep_music_running.store(false, Ordering::SeqCst);
    }

    /// Play a test tone on the loudspeaker.
    pub fn test_loudspeaker(&self) {
        self.audio.play_speaker(&self.sound("test_loud")).wait();
    }

    /// Play a test tone in the earpiece.
    pub fn test_earpiece(&self) {
        self.audio.stop_speaker();
        // A short pause so the tone is not masked by whatever was still playing.
        std::thread::sleep(Duration::from_millis(500));
        self.audio.play_earpiece(&self.sound("test_earpiece")).wait();
    }

    /// Reboot the phone after playing a tone.
    pub fn reboot(&self) {
        println!("Starte System neu.");
        self.audio.play_speaker(&self.sound("reboot")).wait();
        let _ = std::process::Command::new("systemctl").args(["reboot", "-i"]).status();
    }

    /// Power the phone down after playing a tone.
    pub fn shutdown(&self) {
        println!("Fahre System herunter.");
        self.audio.play_speaker(&self.sound("shutdown")).wait();
        let _ = std::process::Command::new("systemctl").args(["poweroff", "-i"]).status();
    }

    // ---------------------------------------------------------------- watchdog

    /// Watch the network connection and (re)start SIP when it comes back.
    ///
    /// Replaces the `linphonec` supervision of the Python version: while the
    /// network is down no endpoint exists, and once it returns a fresh one is
    /// built and registered.
    pub async fn watchdog(self: &Arc<Self>) {
        tokio::time::sleep(Duration::from_secs(1)).await;

        let host = config::option_str(&self.config, "network", "wifi_test_host");

        loop {
            if !reachable(&host).await {
                if self.is_connected.swap(false, Ordering::SeqCst) {
                    println!("WLAN-Verbindung wurde getrennt.");
                    self.send_command(SipCommand::Shutdown);
                    // Give the endpoint a moment to tear down before retrying.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                } else {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                continue;
            }

            if !self.is_connected.swap(true, Ordering::SeqCst) {
                println!("WLAN-Verbindung verfügbar.");
            }

            match self.run_sip().await {
                Ok(()) => {}
                Err(e) => println!("{}", e),
            }

            // The endpoint dropped: wait for the network check again.
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// Run one SIP session until it ends.
    async fn run_sip(self: &Arc<Self>) -> Result<(), crate::sip::SipError> {
        let (events_tx, mut events_rx) = mpsc::channel::<SipEvent>(32);

        let client = SipClient::connect(
            &config::option_str(&self.config, "sip", "host"),
            &config::option_str(&self.config, "sip", "user"),
            &config::option_str(&self.config, "sip", "pass"),
            sip_bind_addr(),
            EARPIECE_ALSA,
            self.options.verbose,
        )
        .await?;

        println!("SIP-Client gestartet.");

        // The command receiver belongs to the phone, not to a session: it is handed
        // to the session for the duration of the call setup and given back
        // afterwards, so commands issued during a reconnect are not lost.
        let commands = self.sip_receiver.lock().unwrap_or_else(|e| e.into_inner()).take();

        let session = match commands {
            Some(commands) => self.runtime.spawn(async move { client.run(commands, events_tx).await }),
            None => {
                return Err(crate::sip::SipError(
                    "SIP-Kanal wird bereits von einer anderen Sitzung benutzt.".to_string(),
                ))
            }
        };

        // Events are handled by the phone, which may await the user.
        let handler_phone = self.clone();
        let handler = self.runtime.spawn(async move {
            while let Some(event) = events_rx.recv().await {
                handler_phone.on_sip_event(event).await;
            }
        });

        // The session hands the command channel back so the next reconnect can use it.
        let (result, commands) = match session.await {
            Ok(pair) => pair,
            Err(e) => {
                // The session panicked, so the channel it owned is gone. Replace
                // both ends rather than leaving the phone unable to command SIP.
                let (sender, receiver) = mpsc::channel(16);
                self.replace_command_channel(sender, receiver);
                return Err(crate::sip::SipError(format!("SIP-Sitzung abgebrochen: {}", e)));
            }
        };

        handler.abort();
        *self.sip_receiver.lock().unwrap_or_else(|e| e.into_inner()) = Some(commands);

        result
    }

    /// Install a fresh command channel, used when the old one was lost.
    fn replace_command_channel(
        &self,
        sender: mpsc::Sender<SipCommand>,
        receiver: mpsc::Receiver<SipCommand>,
    ) {
        *self.sip_commands.lock().unwrap_or_else(|e| e.into_inner()) = sender;
        *self.sip_receiver.lock().unwrap_or_else(|e| e.into_inner()) = Some(receiver);
    }

    /// Queue a command for the SIP task.
    ///
    /// Dropping it is not an error: the phone keeps working without a session,
    /// and the watchdog starts a new one when the network is back.
    fn send_command(&self, command: SipCommand) {
        let sender = self.sip_commands.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = sender.try_send(command) {
            if self.options.verbose {
                println!("SIP-Befehl nicht zugestellt: {}", e);
            }
        }
    }
}

/// Where the SIP endpoint binds.
fn sip_bind_addr() -> std::net::SocketAddr {
    ([0, 0, 0, 0], 5060).into()
}

/// How long the phone rings before giving up on an incoming call.
const RING_TIMEOUT: Duration = Duration::from_secs(30);

    /// Is `caller` among the configured numbers, in any of the usual spellings?
///
/// A caller is accepted if their own number is listed, or if the one other
/// spelling we can derive from it is. The same German number reaches the phone
/// as `0891234`, `+49891234` or `0049891234`, depending on the provider, and the
/// user should not have to list all three.
fn known_caller(caller: &str, configured: &[String]) -> bool {
    let alternative = if let Some(rest) = caller.strip_prefix("00") {
        // International format without plus sign: 0049891234 => +49891234
        format!("+{}", rest)
    } else if let Some(rest) = caller.strip_prefix('+') {
        // International format with plus sign: +49891234 => 0049891234
        format!("00{}", rest)
    } else if let Some(rest) = caller.strip_prefix('0') {
        // National format: 0891234 => +49891234. The country code is not
        // configurable, so it is fixed here.
        format!("+49{}", rest)
    } else {
        // Unknown format: no additional check.
        String::new()
    };

    configured.iter().any(|number| number == caller)
        || (!alternative.is_empty() && configured.iter().any(|number| *number == alternative))
}

/// Is a TCP connection to `host:80` possible? Used as the reachability check.
async fn reachable(host: &str) -> bool {
    match tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect((host, 80))).await {
        Ok(Ok(_)) => true,
        _ => false,
    }
}

/// Modification time of a file, used to notice changes from outside.
fn modified(path: &Path) -> Option<f64> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since_epoch.as_secs_f64())
}

/// Borrow a section of the configuration as a mutable table.
fn section_mut<'a>(config: &'a mut Table, name: &str) -> Option<&'a mut BTreeMap<String, config::Val>> {
    config.get_mut(name).and_then(|v| match v {
        config::Val::Table(table) => Some(table),
        _ => None,
    })
}

/// Next wake-up time: today if it is still ahead, otherwise tomorrow.
fn next_wake_up(times: &[String]) -> Option<chrono::NaiveDateTime> {
    let now = Local::now();
    let today = now.date_naive();

    let time_for = |date: chrono::NaiveDate| -> Option<NaiveTime> {
        let index = date.weekday().num_days_from_monday() as usize;
        times.get(index)?.parse::<NaiveTime>().ok()
    };

    if let Some(time) = time_for(today) {
        let candidate = today.and_time(time);
        if now.naive_local() < candidate {
            return Some(candidate);
        }
    }

    let tomorrow = today + chrono::Duration::days(1);
    Some(tomorrow.and_time(time_for(tomorrow)?))
}

/// Seconds from now until the given moment, never negative.
fn seconds_until(target: chrono::NaiveDateTime) -> u64 {
    let now = Local::now().naive_local();
    (target - now).num_seconds().max(0) as u64
}

// --------------------------------------------------------------------------------------
// Web interface
// --------------------------------------------------------------------------------------

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
            "next_wake_up": self.next_wake_up
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .map(|t| t.format("%H:%M").to_string()),
            "config_changed": self.config_changed.load(Ordering::SeqCst),
        })
    }

    fn get_config(&self) -> Json_ {
        let baseline = self.baseline.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({
            "sections": config::describe(&baseline),
            "mtime": *self.config_mtime.lock().unwrap_or_else(|e| e.into_inner()),
            "restart_required": self.config_changed.load(Ordering::SeqCst),
        })
    }

    fn save_config(&self, payload: Json_) -> Json_ {
        // The form sends JSON, the file is TOML: convert, then run the same
        // validation the file would go through.
        let incoming = config::from_json(&payload);
        let baseline = self.baseline.lock().unwrap_or_else(|e| e.into_inner()).clone();

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
        println!(
            "Konfiguration gespeichert: {}",
            if changed.is_empty() { "keine Änderungen".to_string() } else { changed.join(", ") }
        );

        *self.baseline.lock().unwrap_or_else(|e| e.into_inner()) = saved.cleaned;
        *self.config_mtime.lock().unwrap_or_else(|e| e.into_inner()) =
            modified(&self.options.config).unwrap_or(0.0);

        // Ohne echte Änderung ist kein Neustart nötig.
        let has_changes = !changed.is_empty();
        if has_changes {
            self.config_changed.store(true, Ordering::SeqCst);
        }

        serde_json::json!({ "ok": true, "changed": changed, "restart_required": has_changes })
    }

    fn run_action(&self, action: Action) {
        match action {
            Action::NightMode => self.start_night_mode(),
            Action::SleepMusic => self.toggle_sleep_music(),
            Action::TestLoudspeaker => self.test_loudspeaker(),
            Action::TestEarpiece => self.test_earpiece(),
            Action::Reboot => self.reboot(),
            Action::Shutdown => self.shutdown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_todays_wake_up_when_still_ahead() {
        let now = Local::now();
        let later = now.format("%H:%M").to_string();
        let times: Vec<String> = std::iter::repeat(later).take(7).collect();

        match next_wake_up(&times) {
            Some(target) => assert!(target >= now.naive_local()),
            None => panic!("Uhrzeit sollte gültig sein"),
        }
    }

    #[test]
    fn moves_to_tomorrow_once_the_time_has_passed() {
        let times: Vec<String> = std::iter::repeat("00:00".to_string()).take(7).collect();
        let target = next_wake_up(&times).expect("gültige Uhrzeit");
        assert!(target > Local::now().naive_local());
    }

    #[test]
    fn rejects_incomplete_week() {
        assert!(next_wake_up(&["06:30".to_string()]).is_none());
    }

    #[test]
    fn seconds_until_is_never_negative() {
        let past = Local::now().naive_local() - chrono::Duration::hours(1);
        assert_eq!(seconds_until(past), 0);
    }

    /// The three spellings of a German number, as they arrive from a SIP server.
    const NATIONAL: &str = "0891234";
    const INTERNATIONAL: &str = "+49891234";
    const INTERNATIONAL_PLAIN: &str = "0049891234";

    fn known(caller: &str, configured: &[&str]) -> bool {
        let owned: Vec<String> = configured.iter().map(|n| n.to_string()).collect();
        known_caller(caller, &owned)
    }

    #[test]
    fn whitelist_accepts_an_exact_number() {
        assert!(known(NATIONAL, &[NATIONAL, INTERNATIONAL]));
    }

    #[test]
    fn whitelist_accepts_the_derived_spelling() {
        // Exactly one alternative is derived, and it depends on the caller's
        // own format: 00 and + are each other's alternative, while the national
        // format derives the international one.
        assert!(known(NATIONAL, &[INTERNATIONAL]));
        assert!(known(INTERNATIONAL, &[INTERNATIONAL_PLAIN]));
        assert!(known(INTERNATIONAL_PLAIN, &[INTERNATIONAL]));
    }

    #[test]
    fn whitelist_is_asymmetric_by_design() {
        // Only one alternative is derived, so a caller in international format
        // does not match a number stored nationally. Pinning this so the
        // behaviour is a decision rather than an accident.
        assert!(!known(INTERNATIONAL, &[NATIONAL]));
        assert!(!known(INTERNATIONAL_PLAIN, &[NATIONAL]));
    }

    #[test]
    fn whitelist_rejects_unknown_callers() {
        assert!(!known("+49899999", &[NATIONAL, INTERNATIONAL]));
        assert!(!known("12345", &[NATIONAL]));
    }

    #[test]
    fn whitelist_rejects_everything_when_empty() {
        for caller in [NATIONAL, INTERNATIONAL, INTERNATIONAL_PLAIN, "12345", ""] {
            assert!(!known(caller, &[]), "{} dürfte nicht durchkommen", caller);
        }
    }

    /// Every feature here is triggered from the rotary dial, and the dial is
    /// decoded on the GPIO watcher thread — a plain OS thread with no reactor.
    /// A bare `tokio::spawn` or `spawn_blocking` on that path panics at runtime
    /// with "there is no reactor running", and only as soon as somebody dials
    /// that feature. The check is textual because the panic cannot be provoked
    /// without the hardware.
    #[test]
    fn gpio_thread_never_spawns_onto_a_missing_reactor() {
        // Everything from the test module onwards is compiled for the test
        // harness, which does have a runtime; only the code above it runs on
        // the GPIO thread. Cutting here also keeps this test from matching the
        // patterns it searches for.
        let source = include_str!("phone.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("Quelle vor den Tests");

        for (pattern, what) in [
            ("tokio::spawn(", "tokio::spawn"),
            ("tokio::task::spawn_blocking(", "tokio::task::spawn_blocking"),
        ] {
            assert!(
                !source.contains(pattern),
                "{} wird auf dem GPIO-Thread aufgerufen, wo es ohne Laufzeitumgebung abbricht. \
                 Über self.runtime.spawn(...) starten.",
                what
            );
        }
    }
}
