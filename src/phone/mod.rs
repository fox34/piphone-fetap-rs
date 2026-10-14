//! PiPhone - a headless SIP telephone built into a historical rotary dial phone.
//!
//! Runs on a Raspberry Pi Zero 2W inside an FeTAp-style phone: a rotary dial
//! provides the short codes, a hook switch handles the handset, two LEDs show
//! night and wake-up mode, and SIP calls are handled by the `rvoip` stack.
//!
//! This module is the state machine: what the phone does. Two things that are
//! not that live next door - [`schedule`] for the decisions that are pure
//! functions of the clock and the configuration, and [`web`] for the view the
//! HTTP layer gets of all this.

mod schedule;
mod web;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Local, Timelike};
use rand::seq::SliceRandom;
use tokio::sync::mpsc;

use schedule::{dnd_window_active, known_caller, next_wake_up, seconds_until};

use crate::audio::{Audio, Tone};
use crate::command::Command;
use crate::config::{self, Table};
use crate::gpio::{Gpio, Led, Watcher};
use crate::sip::{CallDecision, SipClient, SipEvent};
use crate::sync::lock;

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
    next_wake_up: Mutex<Option<chrono::NaiveDateTime>>,

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

    /// Give up on a call nobody picked up.
    ring_timeout: Timer,
    /// Switch the wake-up light on when the night ends.
    night_light: Timer,
    /// Abandon a partly dialled number.
    dial_timeout: Timer,
    /// End a call that runs past its limit.
    call_duration: Timer,
}

/// A one-shot timer that replaces itself instead of stacking up.
///
/// Every timed job in the phone is "do this in a while, unless something else
/// comes along first": a second call rings while the first is still ringing, a
/// digit follows a digit, night mode is switched off before it ever woke up.
/// So the previous job has to be *aborted*, not merely forgotten - dropping its
/// handle would leave it running with nothing left to cancel it, which is how an
/// abandoned ring timer came to decline whichever call was ringing by the time
/// it fired.
///
/// Holding the invariant in one type is the point: a caller cannot forget to
/// cancel, because there is only one way to arm a timer.
struct Timer {
    /// The runtime the job is started on.
    ///
    /// Stored rather than reached for, because the dial arms these timers from
    /// the GPIO watcher thread, which has no reactor of its own.
    runtime: tokio::runtime::Handle,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Timer {
    fn new(runtime: &tokio::runtime::Handle) -> Self {
        Timer { runtime: runtime.clone(), handle: Mutex::new(None) }
    }

    /// Run `job` after `delay`, cancelling whatever this timer was running.
    fn start<F>(&self, delay: Duration, job: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.cancel();

        let handle = self.runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            job();
        });

        *lock(&self.handle) = Some(handle);
    }

    /// Stop the job, if one is running. Harmless when nothing is.
    fn cancel(&self) {
        if let Some(handle) = lock(&self.handle).take() {
            handle.abort();
        }
    }
}

/// Command channel into the SIP task.
pub enum SipCommand {
    /// Place a call to a number.
    Call { number: String, host: String },
    /// Open the sound card for the call that was just answered.
    ///
    /// Sent by the phone rather than done by the SIP stack, because both want the
    /// same card: the earpiece tone has to be stopped before the conversation can
    /// take it, and only the phone knows when that has happened.
    StartCallAudio,
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
        let (baseline, errors) = config::validate(&config, None);
        if !errors.is_empty() {
            // The phone runs on the values as they stand in the file rather than
            // on the checked ones, so a rejected value is not replaced by its
            // default here - it is used as written. Saying so is the only warning
            // the user gets, and it used to be thrown away with the tuple.
            println!("Konfiguration enthält {} Fehler; die Werte aus der Datei werden verwendet:", errors.len());
            for (key, message) in errors.iter() {
                println!(" - {}: {}", key, message);
            }
        }

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

        // The timed jobs get the same handle, for the same reason.
        let ring_timeout = Timer::new(&runtime);
        let night_light = Timer::new(&runtime);
        let dial_timeout = Timer::new(&runtime);
        let call_duration = Timer::new(&runtime);

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
            ring_timeout,
            night_light,
            dial_timeout,
            call_duration,
            sip_commands: Mutex::new(sip_commands),
            sip_receiver,
        });


        // Rotary dial and hook switch share one thread that sleeps until a
        // contact changes level.
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
        self.numbers_table()
            .map(|table| table.iter().map(|(k, v)| (k.clone(), v.to_display_string())).collect())
            .unwrap_or_default()
    }

    /// The `[numbers]` table itself, without copying it.
    fn numbers_table(&self) -> Option<&Table> {
        self.config.get("numbers").and_then(|v| v.as_table())
    }

    /// The action `code` stands for, if `[numbers]` has an entry for it.
    ///
    /// Straight from the table: looking one code up used to copy every
    /// configured number into a list first, and this runs once per digit while
    /// a number is being dialled.
    fn action_for(&self, code: &str) -> Option<String> {
        self.numbers_table()
            .and_then(|table| table.get(code))
            .map(|v| v.to_display_string())
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
        let mut digits = lock(&self.dial_digits);
        if digits.len() >= MAX_DIAL_CODE {
            // Too many digits: abandon this attempt.
            println!("Ziffernfolge zu lang, beende Wahlvorgang.");
            digits.clear();
            drop(digits);
            self.stop_dial_timeout();
            self.audio.play_earpiece_tone(Tone::Ungueltig);
            return;
        }

        digits.push(char::from_digit(digit, 10).unwrap_or('0'));
        let number = digits.clone();
        drop(digits);

        // A code fires as soon as it is complete, so longer codes still work.
        let action = self.action_for(&number);

        if let Some(action) = action {
            lock(&self.dial_digits).clear();
            println!("Gewählt: {} -> {}", number, action);
            self.stop_dial_timeout();
            self.audio.stop_earpiece();
            self.run_dial_action(&action);
        }
    }

    /// Hand a piece of dial-triggered work to a blocking task and return at once.
    ///
    /// Everything the rotary dial triggers runs on the GPIO watcher thread, and
    /// that thread is the one that notices the handset going back down and the
    /// next digit coming in. Work that waits - for a tone to finish, or for the
    /// deliberate pause in front of the earpiece test - used to hold it for the
    /// whole sound, so lifting the handset during the confirmation tone did
    /// nothing for as long as the tone played, and an incoming call was not even
    /// seen. The runtime's blocking pool is where that work belongs; this is the
    /// same route the sleep music already took, and it needs the stored handle,
    /// because the watcher has no reactor of its own.
    fn background<F>(self: &Arc<Self>, work: F)
    where
        F: FnOnce(&Arc<Self>) + Send + 'static,
    {
        let phone = self.clone();
        self.runtime.spawn_blocking(move || work(&phone));
    }

    /// Perform the action a short code stands for.
    fn run_dial_action(self: &Arc<Self>, action: &str) {
        // Anything that is not one of the commands is a number, and that is the
        // one thing on the dial that needs the handset in the hand: the commands
        // are allowed to be triggered from the cradle (the night light is
        // switched on before going to bed, with the phone lying unused). Without
        // this the phone dialled out while it was sitting there.
        let Some(command) = Command::from_dial(action) else {
            self.place_call(action);
            return;
        };

        self.run_command(command);
    }

    /// Carry out one command, whichever name it arrived under.
    fn run_command(self: &Arc<Self>, command: Command) {
        match command {
            Command::NightMode => {
                self.start_night_mode();
                self.background(|phone| {
                    phone.audio.play_speaker(&phone.sound("action_confirmed")).wait()
                });
            }
            Command::SleepMusic => self.toggle_sleep_music(),
            Command::TestLoudspeaker => self.test_loudspeaker(),
            Command::TestEarpiece => self.test_earpiece(),
            Command::Reboot => {
                self.reboot();
                std::process::exit(0);
            }
            Command::Shutdown => {
                self.shutdown();
                std::process::exit(0);
            }
        }
    }

    /// Dial `number`, which is known to be a number rather than a command.
    fn place_call(self: &Arc<Self>, number: &str) {
        if self.is_hungup() {
            println!("Hörer liegt auf der Gabel, {} wird nicht gewählt.", number);
            return;
        }

        if !self.is_connected.load(Ordering::SeqCst) {
            self.audio.play_earpiece_tone(Tone::Besetzt);
            return;
        }
        println!("Rufe Nummer an: {}", number);
        let host = config::option_str(&self.config, "sip", "host");

        // The number is complete, so the time to enter one is over. The
        // old timer would otherwise fire in the middle of the call and
        // put the busy tone into the conversation.
        self.stop_dial_timeout();

        self.send_command(SipCommand::Call {
            number: number.to_string(),
            host,
        });

        // Nothing comes back from the other end while it is still
        // ringing, so the caller hears the ringback until the call is
        // answered or ends. Both stop it again.
        self.audio.play_earpiece_tone(Tone::Freiton);

        // Limit the length of outgoing calls.
        let minutes = config::option_int(&self.config, "sip", "max_call_duration");
        if minutes > 0 {
            println!("Maximale Anrufdauer: {} Minuten", minutes);
            self.start_call_duration_timer(minutes as u64 * 60);
        }
    }

    fn start_dial_timeout(self: &Arc<Self>) {
        let seconds = config::option_int(&self.config, "sip", "dial_timeout");
        let phone = self.clone();
        self.dial_timeout.start(Duration::from_secs(seconds.max(0) as u64), move || phone.cancel_dialing());
    }

    fn stop_dial_timeout(&self) {
        self.dial_timeout.cancel();
    }

    fn start_call_duration_timer(self: &Arc<Self>, seconds: u64) {
        let phone = self.clone();
        self.call_duration.start(Duration::from_secs(seconds), move || phone.timeout_call());
    }

    fn stop_call_duration_timer(&self) {
        self.call_duration.cancel();
    }

    /// Give up dialling after the configured timeout.
    fn cancel_dialing(self: &Arc<Self>) {
        println!("Wählvorgang nach Timeout automatisch abgebrochen.");
        lock(&self.dial_digits).clear();
        self.stop_dial_timeout();
        // Repeats, because there is nobody around to pick the handset up.
        self.audio.play_earpiece_tone(Tone::Besetzt);
    }

    /// End an outgoing call that ran past its limit.
    fn timeout_call(self: &Arc<Self>) {
        println!("Maximale Telefondauer erreicht. Gespräch wird beendet.");
        self.send_command(SipCommand::Hangup);
        self.audio.play_earpiece_tone(Tone::Besetzt);
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

            lock(&self.dial_digits).clear();
            self.stop_dial_timeout();
            self.audio.stop_earpiece();
            self.send_command(SipCommand::Hangup);
            self.declined_incoming_call.store(false, Ordering::SeqCst);
            self.stop_call_duration_timer();
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
                self.audio.play_earpiece_tone(Tone::Frei);
            } else {
                self.audio.play_earpiece_tone(Tone::NichtVerbunden);
            }

            lock(&self.dial_digits).clear();
            self.start_dial_timeout();
        }
    }

    // ---------------------------------------------------------------- SIP events

    /// Handle an event from the SIP stack.
    pub async fn on_sip_event(self: &Arc<Self>, event: SipEvent) {
        match event {
            SipEvent::IncomingCall { caller, reply } => self.incoming_call(&caller, reply).await,
            SipEvent::CallAnswered => {
                // The conversation wants the same sound card as the ringback, and
                // ALSA gives it to whoever opens it first. So the tone goes first,
                // and only then is the card handed over for the call.
                self.audio.stop_earpiece();
                self.call_incoming.store(false, Ordering::SeqCst);
                self.call_up.store(true, Ordering::SeqCst);
                self.send_command(SipCommand::StartCallAudio);
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
        *lock(&self.pending_call) = Some(reply);
        self.start_ring_timeout();
    }

    /// Give up on a call nobody picked up.
    fn start_ring_timeout(self: &Arc<Self>) {
        let phone = self.clone();
        self.ring_timeout.start(RING_TIMEOUT, move || phone.ring_timeout());
    }

    fn stop_ring_timeout(&self) {
        self.ring_timeout.cancel();
    }

    /// Nobody lifted the handset: stop ringing and let the call go.
    fn ring_timeout(self: &Arc<Self>) {
        println!("Kein Abheben während des Klingelns, Anruf wird beendet.");
        self.hang_up_incoming(CallDecision::Decline);
    }

    /// Stop ringing and pass the decision to the SIP task.
    fn hang_up_incoming(self: &Arc<Self>, decision: CallDecision) {
        let reply = lock(&self.pending_call).take();
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
        let numbers: Vec<String> = self.numbers_table()
            .map(|table| table.values().map(|v| v.to_display_string()).collect())
            .unwrap_or_default();
        known_caller(caller, &numbers)
    }

    /// A call ended, either by us or by the other side.
    fn hung_up(self: &Arc<Self>) {
        println!("Anruf beendet");
        self.call_up.store(false, Ordering::SeqCst);

        let was_ringing = self.call_incoming.swap(false, Ordering::SeqCst);
        self.stop_ring_timeout();
        *lock(&self.pending_call) = None;

        // A call we already declined needs no further cleanup.
        if self.declined_incoming_call.swap(false, Ordering::SeqCst) {
            return;
        }

        self.audio.stop_speaker();
        self.stop_call_duration_timer();

        // The handset is off the cradle: it is waiting for the next number. The
        // tone repeats until the handset goes back down, so it does not run out
        // while nobody is there to pick it up again.
        if !was_ringing && !self.is_hungup() {
            self.audio.play_earpiece_tone(Tone::Besetzt);
        }
    }

    // ---------------------------------------------------------------- features

    /// Is the night light lit, and with it the night or wake-up mode?
    ///
    /// The light is the state, because it is what the user sees and because the
    /// two ways into the mode (night mode itself, and the wake-up light that
    /// replaces it) are both just "the light is on". A timer is not the same
    /// thing: night mode can be lit without one, when the wake-up times turn out
    /// to be unusable.
    fn night_mode_active(&self) -> bool {
        self.led.night_light_on_state()
    }

    /// Is do-not-disturb active, either on a timer or switched on manually?
    fn is_dnd_active(&self) -> bool {
        if self.manual_dnd.load(Ordering::SeqCst) {
            return true;
        }
        let hour = Local::now().hour() as i64;
        let from = config::option_int(&self.config, "sip", "dnd_from");
        let to = config::option_int(&self.config, "sip", "dnd_to");
        dnd_window_active(hour, from, to)
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
        //
        // The light itself is the state, not the timer: both error paths below
        // switch the light on and *then* give up, so a run that never got as far
        // as a timer would read as "not running" and could only ever switch the
        // light on again - there was no way left to turn it off.
        if self.night_mode_active() {
            println!("Deaktiviere Nacht-/Aufwachlicht.");
            self.cancel_night_timer();
            self.led.night_light_off();
            self.manual_dnd.store(false, Ordering::SeqCst);
            *lock(&self.next_wake_up) = None;
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
        *lock(&self.next_wake_up) = Some(wake_up);

        let seconds = seconds_until(wake_up);
        let phone = self.clone();
        self.night_light.start(Duration::from_secs(seconds), move || phone.start_wakeup_light());
    }

    fn cancel_night_timer(&self) {
        self.night_light.cancel();
    }

    /// Turn the wake-up light on for two hours.
    fn start_wakeup_light(self: &Arc<Self>) {
        println!("Aktiviere Aufwachlicht für zwei Stunden.");
        self.led.night_light_on(Some(100.0)); // night light at full brightness
        self.led.wake_light_on(None);

        let phone = self.clone();
        self.night_light.start(Duration::from_secs(2 * 60 * 60), move || phone.stop_wakeup_light());
    }

    fn stop_wakeup_light(self: &Arc<Self>) {
        println!("Deaktiviere Aufwachlicht.");
        self.led.night_light_off();
        self.led.wake_light_off();
        self.cancel_night_timer();
        self.manual_dnd.store(false, Ordering::SeqCst);
        *lock(&self.next_wake_up) = None;
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

        // The music is played and waited for on a blocking task, so the dial
        // thread goes on watching the hook switch: Through the stored runtime
        // handle, because a bare `spawn_blocking` on that thread has no reactor
        // to attach to and panics.
        self.background(|phone| phone.play_sleep_music());
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
        if !self.night_mode_active() {
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
        let mut retry = NETWORK_RETRY_MIN;

        loop {
            if !reachable(&host).await {
                if self.is_connected.swap(false, Ordering::SeqCst) {
                    println!("WLAN-Verbindung wurde getrennt.");
                    self.send_command(SipCommand::Shutdown);
                }

                // The phone spends hours with no network at all, and it used to
                // open a connection to look for one every second the whole time.
                // The wait grows while nothing turns up and is back to its
                // shortest the moment the network is there again, so a phone
                // that is unplugged wakes a few times a minute rather than sixty
                // times - at the price of up to `NETWORK_RETRY_MAX` seconds
                // before a reconnect is noticed, which is nothing next to how
                // long a session then needs to register.
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(NETWORK_RETRY_MAX);
                continue;
            }

            retry = NETWORK_RETRY_MIN;

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
        let commands = lock(&self.sip_receiver).take();

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
        *lock(&self.sip_receiver) = Some(commands);

        result
    }

    /// Install a fresh command channel, used when the old one was lost.
    fn replace_command_channel(
        &self,
        sender: mpsc::Sender<SipCommand>,
        receiver: mpsc::Receiver<SipCommand>,
    ) {
        *lock(&self.sip_commands) = sender;
        *lock(&self.sip_receiver) = Some(receiver);
    }

    /// Queue a command for the SIP task.
    ///
    /// Dropping it is not an error: the phone keeps working without a session,
    /// and the watchdog starts a new one when the network is back.
    fn send_command(&self, command: SipCommand) {
        let sender = lock(&self.sip_commands);
        if let Err(e) = sender.try_send(command) {
            if self.options.verbose {
                println!("SIP-Befehl nicht zugestellt: {}", e);
            }
        }
    }

    /// Ask the SIP stack to end its session, and give it time to do so.
    ///
    /// Used on the way out. A `Shutdown` makes the session hang up and
    /// unregister before it returns, which is what the other end needs: without
    /// it the process simply stops, the RTP stream dries up and no BYE is sent,
    /// so the caller only learns about the end from their own session timer -
    /// which on a phone is long enough to be a noticeable stretch of dead air
    /// with the handset still against their ear.
    ///
    /// How long the teardown takes is not observable from here, hence the wait.
    /// A phone that is idle costs nothing: there is no session, the command sits
    /// in the channel and the process exits when the wait is over.
    pub async fn stop_sip(&self) {
        self.send_command(SipCommand::Shutdown);
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    }
}

/// Where the SIP endpoint binds.
fn sip_bind_addr() -> std::net::SocketAddr {
    ([0, 0, 0, 0], 5060).into()
}

/// How long the phone rings before giving up on an incoming call.
const RING_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the watchdog waits before looking for the network again, and how far
/// that wait may grow while the network stays away.
const NETWORK_RETRY_MIN: Duration = Duration::from_secs(1);
const NETWORK_RETRY_MAX: Duration = Duration::from_secs(10);

/// How long the SIP session is given to hang up and unregister on the way out.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1000);

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

#[cfg(test)]
mod tests {
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
        let source = include_str!("mod.rs")
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
