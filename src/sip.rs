//! SIP telephony via `rvoip`, replacing the `linphonec` subprocess.
//!
//! The Python version spawned `linphonec`, wrote commands into its stdin and
//! scraped its stdout with regular expressions. Here the SIP stack is a normal
//! library dependency: an [`Endpoint`] is registered against the server,
//! incoming and outgoing calls arrive as typed events, and the audio path is
//! handed over as PCM frames instead of being bound to a sound card by ALSA
//! defaults.
//!
//! Everything the rest of the program needs arrives on a single event channel,
//! so the phone logic stays independent of the SIP library.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rvoip::sip::{
    CallId, Endpoint, EndpointAudioFrame, EndpointControl, EndpointEvent, EndpointEvents,
    EndpointProfile, EndpointRegistrationStatus,
};

use crate::phone::SipCommand;

/// G.711 in SIP runs at 8 kHz; frames are 20 ms.
pub const SAMPLE_RATE: u32 = 8_000;
pub const FRAME_SAMPLES: usize = 160;
pub const FRAME_MS: u64 = 20;

/// RTP port range. A fixed range keeps the firewall rules stable instead of
/// picking a random high port on every call.
const MEDIA_PORT_START: u16 = 30000;
const MEDIA_PORT_END: u16 = 30100;

/// The local address the OS would use to reach `host`.
///
/// Connecting a UDP socket sends nothing, but it makes the kernel pick the
/// outgoing interface and address, which is what has to appear in the SDP.
fn local_addr_for(host: &str) -> Option<std::net::IpAddr> {
    use std::net::UdpSocket;

    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect((host, 9)).ok()?; // discard port: nothing is sent
    let address = socket.local_addr().ok()?;
    Some(address.ip())
}

pub use rvoip::sip::EndpointCall;
pub use rvoip::sip::EndpointIncomingCall;

/// What the phone needs to know about SIP activity.
#[derive(Debug)]
pub enum SipEvent {
    /// An incoming call is ringing.
    ///
    /// The phone decides what happens and answers over `reply`: answer the call,
    /// or decline it for do-not-disturb or an unknown caller.
    IncomingCall {
        caller: String,
        reply: tokio::sync::mpsc::Sender<CallDecision>,
    },
    /// A call was answered and audio can flow.
    CallAnswered,
    /// A call ended for any reason.
    CallEnded,
    /// The registration state at the SIP server changed.
    RegistrationChanged {
        /// `true` once the endpoint is registered.
        registered: bool,
    },
}

/// The phone's decision about a ringing call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallDecision {
    /// Accept the call and start audio.
    Answer,
    /// Reject the call.
    Decline,
}

/// Bridges call audio between the SIP stack and the sound card.
///
/// Implemented by the platform layer: on Raspberry Pi OS this is the USB sound
/// card, elsewhere it can be a no-op. Keeping it behind a trait means the phone
/// logic and the SIP logic can be tested without audio hardware.
pub trait AudioBridge: Send + Sync {
    /// Hand captured microphone samples to the caller.
    fn push_microphone(&self, samples: Vec<i16>);
    /// Take received samples for playback on the earpiece.
    fn take_speaker(&self) -> Option<Vec<i16>>;
    /// Sample rate the bridge runs at.
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }
}

/// Errors surfaced by the SIP layer.
#[derive(Debug)]
pub struct SipError(pub String);

impl std::fmt::Display for SipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SIP: {}", self.0)
    }
}

impl std::error::Error for SipError {}

/// Spell out an rvoip error.
///
/// The library redacts the detail behind a `TextDiagnostic` that reports only
/// the byte length, so every failure reads the same. Matching on the variant
/// hands back the real string, which is the only way to tell "no such session"
/// apart from "the state machine refused the transition".
fn explain(error: &rvoip::sip::SessionError) -> String {
    use rvoip::sip::SessionError as E;

    let detail = match error {
        E::SessionNotFound(m) => Some(m.as_str()),
        E::InvalidTransition(m) => Some(m.as_str()),
        E::DialogError(m) => Some(m.as_str()),
        E::MediaError(m) => Some(m.as_str()),
        _ => None,
    };

    match detail {
        Some(detail) => format!("{} [{}]", error, detail),
        None => error.to_string(),
    }
}

/// Split a caller identity such as `sip:+49891234@10.0.0.1` down to the number.
///
/// SIP headers carry the full URI; the whitelist and ringtones in the
/// configuration are keyed by plain numbers, so only the user part is kept.
pub fn caller_number(from: &str) -> String {
    let mut uri = from.trim();

    // A From header usually carries a display name, as in
    // `"FRITZ!Fon C4" <sip:1234@host>`. The number is inside the angle brackets.
    if let (Some(start), Some(end)) = (uri.find('<'), uri.rfind('>')) {
        if start < end {
            uri = uri[start + 1..end].trim();
        }
    }

    let uri = uri.strip_prefix("sips:").or_else(|| uri.strip_prefix("sip:")).unwrap_or(uri);

    // URI parameters and headers trail the user part.
    let uri = uri.split([';', '?']).next().unwrap_or(uri);
    let user = uri.split('@').next().unwrap_or(uri);
    user.split(':').next().unwrap_or(user).to_string() // drop any port
}

/// Build a call target, adding the configured host when the number has none.
pub fn call_target(number: &str, host: &str) -> String {
    if number.starts_with("sip:") || number.starts_with("sips:") {
        return number.to_string();
    }
    format!("sip:{}@{}", number, host)
}

/// A running SIP endpoint.
pub struct SipClient {
    /// Command half of the endpoint. Cloneable, so every task can share it.
    control: EndpointControl,
    /// The peer's private event stream, kept from [`Endpoint::split`].
    ///
    /// This must *not* be re-derived from [`EndpointControl::events`]: that
    /// subscribes to the observational dispatcher, which delivers events
    /// without their session lifecycle handle. Incoming calls taken from an
    /// observation arrive unusable — `answer()` fails with "session not
    /// found". Only the stream from `split()` carries the handle.
    stream: EndpointEvents,
    call_active: Arc<AtomicBool>,
    /// Handle of the call currently up, so it can be hung up on request.
    current_call: Arc<std::sync::Mutex<Option<EndpointCall>>>,
    /// ALSA device for call audio, opened per call rather than held open.
    audio_device: &'static str,
    /// Identifier of the outgoing call we placed, if any.
    ///
    /// The `CallAnswered` event carries an `EndpointCall` built from a handle
    /// captured when the event was mapped. That snapshot goes stale, and
    /// `audio()` then refuses it with "session not found". Wrapping the id
    /// again with [`Endpoint::wrap_call`] produces a handle from the current
    /// state, which is what the outgoing path has to use.
    outgoing_id: std::sync::Mutex<Option<CallId>>,
    verbose: bool,
}

impl SipClient {
    /// Build and register an endpoint for the given account.
    ///
    /// A failed registration is reported but not fatal: the phone stays up and
    /// retries, exactly as the original did by restarting `linphonec`.
    ///
    /// `hostname` is only used to work out which local address to advertise;
    /// binding is left to all interfaces so the phone works on any network.
    pub async fn connect(
        hostname: &str,
        username: &str,
        password: &str,
        bind_addr: std::net::SocketAddr,
        audio_device: &'static str,
        verbose: bool,
    ) -> Result<Self, SipError> {
        // SDP has to carry a routable address, so the endpoint is bound
        // everywhere but advertises the address the OS would use to reach the
        // server.
        let advertised = local_addr_for(hostname)
            .map(|ip| std::net::SocketAddr::new(ip, bind_addr.port()))
            .unwrap_or(bind_addr);

        if verbose {
            println!("SIP: hört auf {}, meldet {}", bind_addr, advertised);
        }

        let endpoint = Endpoint::builder()
            .name(username)
            .account(username)
            .password(password)
            .registrar(format!("sip:{}", hostname))
            .profile(EndpointProfile::LanPbx)
            .bind_addr(bind_addr)
            .advertised_addr(advertised)
            .media_ports(MEDIA_PORT_START, MEDIA_PORT_END)
            .build()
            .await
            .map_err(|e| SipError(format!("Endpunkt nicht startbar: {}", explain(&e))))?;

        let (control, stream) = endpoint.split();

        Ok(SipClient {
            // Split rather than keep the endpoint whole: the stream half is the
            // only source of incoming calls that can actually be answered.
            control,
            stream,
            call_active: Arc::new(AtomicBool::new(false)),
            current_call: Arc::new(std::sync::Mutex::new(None)),
            audio_device,
            outgoing_id: std::sync::Mutex::new(None),
            verbose,
        })
    }

    /// Whether a call is currently up.
    pub fn call_active(&self) -> bool {
        self.call_active.load(Ordering::SeqCst)
    }

    /// Place an outgoing call to a number.
    pub async fn call(&self, number: &str, host: &str) -> Result<(), SipError> {
        let target = call_target(number, host);
        if self.verbose {
            match self.control.resolve_target(&target) {
                Ok(resolved) if resolved != target => {
                    println!("--> SIP: rufe {} an (normalisiert zu {})", target, resolved)
                }
                _ => println!("--> SIP: rufe {} an", target),
            }
        }

        // `invite` only assembles the request; `send` is what puts the INVITE on
        // the wire. Without the second call nothing is transmitted at all.
        let builder = self
            .control
            .invite(&target)
            .map_err(|e| SipError(format!("Anruf {} nicht möglich: {}", target, explain(&e))))?;
        let call_id = builder
            .send()
            .await
            .map_err(|e| SipError(format!("Anruf {} nicht möglich: {}", target, explain(&e))))?;

        *self.outgoing_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(call_id);
        self.call_active.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Forget every trace of a finished call.
///
/// The outgoing id is dropped too: it survives until the call is answered, and
/// keeping it after the call is over would let a later hangup wrap a dead id.
fn forget_call(&self) {
    *self.current_call.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *self.outgoing_id.lock().unwrap_or_else(|e| e.into_inner()) = None;
    self.call_active.store(false, Ordering::SeqCst);
}

/// Remember a call as the current one and attach the audio bridge.
    ///
    /// For a call we placed, the handle is rebuilt from the stored id; for an
    /// answered incoming call the one from `answer()` is authoritative already.
    fn track_call(&self, call: &EndpointCall) {
        let handle = match self.outgoing_id.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(id) => self.control.wrap_call(id),
            None => call.clone(),
        };

        *self.current_call.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle.clone());
        self.call_active.store(true, Ordering::SeqCst);
        crate::callaudio::start(handle, self.audio_device, self.verbose);
    }

    /// End the current call, if there is one.
    pub async fn hangup(&self) {
        let answered = self.current_call.lock().unwrap_or_else(|e| e.into_inner()).take();

        // A call that is still ringing has no answered handle yet, but it still
        // has to be cancelled: without this the other end keeps ringing long
        // after the handset goes back on the cradle.
        let call = match answered {
            Some(call) => Some(call),
            None => self
                .outgoing_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .map(|id| self.control.wrap_call(id)),
        };
        self.call_active.store(false, Ordering::SeqCst);

        let Some(call) = call else { return };
        if let Err(e) = call.hangup().await {
            if self.verbose {
                println!("SIP: Auflegen fehlgeschlagen: {}", e);
            }
        }
    }

    /// Run the endpoint until it is asked to shut down.
    ///
    /// One loop handles both directions: commands from the phone (call, hang up)
    /// and events from the SIP stack. Everything the phone needs to learn leaves
    /// through `events`.
    /// The command channel is handed back with the result, so the caller can
    /// reuse it for the next session after a reconnect.
    pub async fn run(
        mut self,
        mut commands: tokio::sync::mpsc::Receiver<SipCommand>,
        events: tokio::sync::mpsc::Sender<SipEvent>,
    ) -> (Result<(), SipError>, tokio::sync::mpsc::Receiver<SipCommand>) {
        let outcome = self.pump(&mut commands, &events).await;
        (outcome, commands)
    }

    /// The event/command loop.
    async fn pump(
        &mut self,
        commands: &mut tokio::sync::mpsc::Receiver<SipCommand>,
        events: &tokio::sync::mpsc::Sender<SipEvent>,
    ) -> Result<(), SipError> {
        // The event stream is the peer's own control stream, not an
        // observational copy: incoming calls need their lifecycle handle to be
        // answerable. It exists before registering, because the registration
        // state change arrives as an event and registering first would emit it
        // to nobody.
        match self.control.register().await {
            Ok(_) => {
                if self.verbose {
                    println!("<-- SIP: REGISTER gesendet");
                }
            }
            Err(e) => println!("SIP-Registrierung fehlgeschlagen: {}", explain(&e)),
        }

        loop {
            tokio::select! {
                // Phone -> SIP
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    if !self.handle_command(command, events).await {
                        break; // shutdown requested
                    }
                }

                // SIP -> phone
                event = self.stream.next() => {
                    match event {
                        Ok(Some(event)) => self.handle_event(event, events).await,
                        Ok(None) => break,
                        Err(e) => return Err(SipError(e.to_string())),
                    }
                }
            }
        }

        self.hangup().await;
        let _ = self.control.unregister().await;
        Ok(())
    }

    /// Carry out one command from the phone.
    ///
    /// Returns `false` when the session should end.
    async fn handle_command(
        &self,
        command: SipCommand,
        events: &tokio::sync::mpsc::Sender<SipEvent>,
    ) -> bool {
        match command {
            SipCommand::Call { number, host } => {
                if let Err(e) = self.call(&number, &host).await {
                    println!("{}", e);
                    // A failed call still ends, so the phone can play busy tone.
                    let _ = events.send(SipEvent::CallEnded).await;
                }
            }
            SipCommand::Hangup => self.hangup().await,
            SipCommand::Shutdown => return false,
        }
        true
    }

    /// Forward one event from the SIP stack.
    async fn handle_event(&self, event: EndpointEvent, events: &tokio::sync::mpsc::Sender<SipEvent>) {
        match event {
            EndpointEvent::IncomingCall(incoming) => {
                // Each ringing call gets its own task. Answering waits on a
                // human, and the SIP loop has to stay free to see the caller
                // hang up while the handset is still on the cradle.
                let shared = IncomingCall {
                    incoming,
                    events: events.clone(),
                    current_call: self.current_call.clone(),
                    call_active: self.call_active.clone(),
                    audio_device: self.audio_device,
                    verbose: self.verbose,
                };
                tokio::spawn(shared.serve());
            }
            EndpointEvent::CallAnswered { call, .. } => {
                if self.verbose {
                    println!("<-- SIP: Anruf angenommen, Gespräch läuft");
                }
                self.track_call(&call);
                let _ = events.send(SipEvent::CallAnswered).await;
            }
            EndpointEvent::CallProgress { status_code, reason, has_sdp, .. } => {
                if self.verbose {
                    println!("<-- SIP: Anruf im Aufbau, {} {} (SDP: {})", status_code, reason, has_sdp);
                }
            }
            EndpointEvent::CallFailed { status_code, reason, .. } => {
                println!("SIP: Anruf fehlgeschlagen: {} {}", status_code, reason);
                self.forget_call();
                let _ = events.send(SipEvent::CallEnded).await;
            }
            EndpointEvent::CallEnded { reason, .. } => {
                if self.verbose {
                    println!("<-- SIP: Anruf beendet ({})", reason);
                }
                self.forget_call();
                let _ = events.send(SipEvent::CallEnded).await;
            }
            EndpointEvent::RegistrationChanged(info) => {
                let registered = matches!(info.status, EndpointRegistrationStatus::Registered);
                if self.verbose {
                    println!("<-- SIP: Registrierung {:?}", info.status);
                }
                let _ = events.send(SipEvent::RegistrationChanged { registered }).await;
            }
            EndpointEvent::CallCancelled { .. } => {
                if self.verbose {
                    println!("<-- SIP: Anruf vom Anrufer abgebrochen");
                }
                self.forget_call();
                let _ = events.send(SipEvent::CallEnded).await;
            }
            _ => {}
        }
    }
}

/// One incoming call, carried from the ringing state to an answer or a refusal.
///
/// This is its own task rather than a branch of the SIP loop because the
/// decision belongs to the handset: the loop would otherwise sit idle for as
/// long as the call rings and miss the caller hanging up in the meantime.
struct IncomingCall {
    incoming: EndpointIncomingCall,
    events: tokio::sync::mpsc::Sender<SipEvent>,
    current_call: Arc<std::sync::Mutex<Option<EndpointCall>>>,
    call_active: Arc<AtomicBool>,
    audio_device: &'static str,
    verbose: bool,
}

impl IncomingCall {
    async fn serve(self) {
        let IncomingCall {
            incoming,
            events,
            current_call,
            call_active,
            audio_device,
            verbose,
        } = self;

        let caller = caller_number(incoming.from());
        if verbose {
            println!("<-- SIP: eingehender Anruf von {}", caller);
        }

        // Hand the decision to the phone, which rings and answers on the hook switch.
        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(1);
        if events
            .send(SipEvent::IncomingCall { caller, reply: reply_tx })
            .await
            .is_err()
        {
            let _ = incoming.busy().await;
            return;
        }

        match reply_rx.recv().await {
            Some(CallDecision::Answer) => match incoming.answer().await {
                Ok(call) => {
                    // An answered incoming call already carries the exact session
                    // handle, so it needs no rebuilding the way an outgoing one does.
                    *current_call.lock().unwrap_or_else(|e| e.into_inner()) = Some(call.clone());
                    call_active.store(true, Ordering::SeqCst);
                    crate::callaudio::start(call, audio_device, verbose);
                }
                Err(e) => {
                    println!("SIP: Annahme fehlgeschlagen: {}", explain(&e));
                    call_active.store(false, Ordering::SeqCst);
                    let _ = events.send(SipEvent::CallEnded).await;
                }
            },
            Some(CallDecision::Decline) | None => {
                let _ = incoming.busy().await;
            }
        }
    }
}

/// Pump audio frames between the SIP stack and the sound card.
///
/// Shared by the answer path and the outgoing path so both behave identically.
pub async fn pump_audio(
    sender: rvoip::sip::EndpointAudioSender,
    mut receiver: rvoip::sip::EndpointAudioReceiver,
    bridge: Arc<dyn AudioBridge>,
    verbose: bool,
) {
    let rate = bridge.sample_rate();
    let sender_bridge = bridge.clone();

    // Outgoing direction: microphone -> RTP
    let sender_task = tokio::spawn(async move {
        let mut timestamp = 0u32;
        loop {
            // One frame per 20 ms; if the card has nothing ready, stay in step
            // with the frame clock instead of sending early or late.
            match sender_bridge.take_speaker() {
                Some(samples) if !samples.is_empty() => {
                    let frame = if rate == SAMPLE_RATE {
                        EndpointAudioFrame::pcmu_sized_mono_8khz(samples, timestamp)
                    } else {
                        EndpointAudioFrame::new(samples, rate, 1, timestamp)
                    };
                    if sender.send(frame).await.is_err() {
                        return;
                    }
                    timestamp = timestamp.wrapping_add(FRAME_SAMPLES as u32);
                }
                _ => tokio::time::sleep(Duration::from_millis(FRAME_MS)).await,
            }
        }
    });

    // Incoming direction: RTP -> earpiece
    while let Some(frame) = receiver.recv().await {
        bridge.push_microphone(frame.samples);
    }

    if verbose {
        println!("SIP: Audio-Übertragung beendet.");
    }
    sender_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_number() {
        assert_eq!(caller_number("sip:08912345@10.0.0.1"), "08912345");
        assert_eq!(caller_number("sip:+49891234@fritz.box"), "+49891234");
        assert_eq!(caller_number("sips:12345@secure.host"), "12345");
    }

    #[test]
    fn strips_uri_parameters_and_ports() {
        assert_eq!(caller_number("sip:1234@host;transport=tcp"), "1234");
        assert_eq!(caller_number("sip:1234@host:5060"), "1234");
    }

    #[test]
    fn reads_the_number_out_of_a_display_name() {
        // What the SIP stack actually hands over for a real caller.
        assert_eq!(caller_number("\"FRITZ!Fon C4\" <sip:1234@fritz.box>"), "1234");
        assert_eq!(caller_number("Wohnzimmer <sip:+49891234@fritz.box>"), "+49891234");
        assert_eq!(caller_number("<sip:1234@host>"), "1234");
        assert_eq!(caller_number("\"Nur ein Name\" <sip:1234@host;tag=abc>"), "1234");
    }

    #[test]
    fn builds_call_target() {
        assert_eq!(call_target("08912345", "10.0.0.1"), "sip:08912345@10.0.0.1");
        assert_eq!(call_target("sip:already@host", "other"), "sip:already@host");
    }
}
