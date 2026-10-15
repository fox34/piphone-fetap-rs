# Project overview

This is a headless Rust application that runs on a Raspberry Pi Zero 2W inside a historical rotary dial phone.

It is ported from the [Python 3.13 implementation](https://github.com/fox34/piphone-fetap), which drove `linphonec` as a subprocess.
Here the SIP stack is the in-process [`rvoip`](https://crates.io/crates/rvoip-sip) crate.

# Hardware

- MAX98357A amplifier + speaker for ringtones and music, on `i2s`
- A generic USB Soundcard connected to the earpiece and microphone for phone calls, on `usb`
- One amber-colored LED in the back, used as night-light
- One green status LED on the front
- A rotary dial (NSI/NSA) and a hook switch

# Software

- This project always runs on Raspberry Pi OS (Linux). No macOS/Windows version.
- Outgoing/incoming calls (SIP) are performed via `rvoip`

# Features

- Outgoing calls
- Incoming calls
- Outgoing call duration limit
- Incoming number whitelist
- Do not disturb
- Night mode (immediately enables amber LED)
- Wake up mode (enables green LED in the morning)
- Sleep music (plays music for a set amount of time)
- Loudspeaker and earpiece tests
- Shutdown
- Reboot

Calling and other features are performed/toggled via short codes, entered with the rotary dial.
A sample feature-complete configuration file is provided in `support/config-example.toml`.

# Rules

- Read AGENTS.override.md, if it exists.
- Always ask before installing system-wide applications (e.g. via `apt`).
- Always ask before changing system-wide settings (e.g. systemd services).

# Building

- Only ever compile debug and tests on macOS or Windows, never the release.
- Access to a Raspberry Pi Zero 2W with wired-up hardware can be provided for production testing.
- Access to a Raspberry Pi 5 with 4GB of RAM can be provided for compiling the production version.
- Avoid compiling debug binaries on the Raspberry Pi 5 for efficiency reasons.

# Deployment

- Configuration goes to /usr/local/etc/piphone/piphone.toml
- Sound files go to /usr/local/share/piphone/
- Release binary goes to /usr/local/bin

# Layout

| Path | Contents |
| --- | --- |
| `src/phone/mod.rs` | The state machine: dial, hook switch, features, watchdog |
| `src/phone/schedule.rs` | Ringing lock window, caller whitelist, next wake-up |
| `src/phone/web.rs` | The `PhoneApi` implementation behind the web interface |
| `src/sip.rs` | `rvoip` endpoint wrapper, event loop, call decisions |
| `src/callaudio.rs` | ALSA bridge between the SIP stack and the USB sound card |
| `src/audio.rs` | Sound playback: symphonia decoding plus ALSA, for the `i2s` amplifier and the `usb` earpiece; the call-progress tones are generated in memory |
| `src/gpio.rs` | Pulse decoding, watcher thread, LEDs with software PWM |
| `src/gpio/linux.rs` | `gpiod` backend, the only hardware-touching module |
| `src/config.rs` | Schema, validation, reading and writing the TOML file |
| `src/sounds.rs` | The sound files the phone plays: fixed names, uploading, preview |
| `src/command.rs` | The features and the names the dial and the web URL use for them |
| `src/sync.rs` | `lock()`, the one place that says how poisoning is handled |
| `src/webserver.rs` | axum routes, basic auth, actions |
| `src/pages.rs` | The HTML pages, rendered from the schema |
| `src/main.rs` | Argument parsing and startup |
| `examples/` | Hardware test tools, see below |
| `support/etc/` | systemd unit |

Hardware access lives behind `#[cfg(target_os = "linux")]`, with a stub backend elsewhere, so the
crate builds and its tests run on a development machine.

# Building

The Pi Zero cannot compile this; memory is too small for `rustc` on the `rvoip` dependency tree.
Build on a larger machine and copy the binary over:

```sh
cargo build --release
```

Tests need no hardware. They must be linked serially on a Pi, because the linker is the memory
peak: `cargo test -j 1`. The hardware test tools in `examples/` are behind the `harness` feature so
that `cargo test` does not build them - that alone is nine fewer binaries to link on a machine that
barely manages one. Run them with `cargo run --features harness --example <name>`.

The web pages have their own tests, run with node and jsdom:

```sh
npm install && node tests/test-webpage.js
```

They render the pages through `cargo run --example render-page`, which is a compile - and compiling
on a development machine is not allowed. To run them from one anyway, render on the Pi and point
`PIPHONE_RENDER` at the directory holding its output:

```sh
PIPHONE_RENDER=/path/to/render node tests/test-webpage.js
```

# Hardware test tools

Each opens the pins it needs, so run them one at a time. All of them need the `harness`
feature, which `cargo run --example` does not switch on by itself:

- `cargo run --features harness --example hardware-connections` — live state of dial, impulse contact and hook switch
- `cargo run --features harness --example hardware-dial` — decodes dialled digits
- `cargo run --features harness --example hardware-audio` — ringtones, loudspeaker and earpiece test, opens the call bridge
- `cargo run --features harness --example gpio-probe` — reads raw pin levels and watches for edges, for diagnosing wiring
- `cargo run --features harness --example audio-probe -- <device> <rate> <channels>` — tries ALSA parameter combinations
- `cargo run --features harness --example decode-probe -- sounds` — decodes every file in `sounds/`, no sound card needed
- `cargo run --features harness --example playback-probe -- sounds` — plays WAV and MP3 on both devices and the generated tones in
  the earpiece, checks the repeat loop and an early stop, and verifies each sound lasts as long as the file says
  (`playback-probe -- sounds tones` leaves out the files and only plays the tones, which is quiet enough to repeat
  on a phone that is in service)

# Web interface

An optional web server (`src/webserver.rs`) shows the phone status (uptime, connectivity,
call state) and the state of the features (night light, wake up mode, sleep music, dnd, next wake up time).
It is configured in the `[web]` section of the TOML config file (port 0 = disabled) and can also be disabled
with `--no-web`. The same actions as the rotary dial short codes can be triggered via
`POST /api/action/<name>`, the state is available as JSON under `/api/state`. The sound files are
uploaded with `POST /api/sound/<name>` - the body is the file itself, not a multipart form - and
`GET /api/sound/<name>` serves the same file for the preview in the browser. A file of a
collection goes to `POST /api/sound/<collection>/<file name>`, is served from
`GET /api/sound/<collection>/<file name>` and removed with `DELETE` on the same path.

# Configuration

`src/config.rs` is the single source of truth for the TOML config file. It holds the schema of every
option (type, default value, description, limits) and provides reading, validation and writing.
`support/config-example.toml` is a documented sample.

The config file can also be edited in the browser at `/config`, which covers every option. The
dial codes and their targets in `[numbers]` are an addable and removable table. The schema is
rendered dynamically into forms by the JS in `src/pages.rs`, so adding an option to
`src/config.rs` makes it appear in the UI automatically. The sounds section of that page is the
exception: it is built from the uploaded files, not from the schema.

# Sounds

The sounds the phone plays are not options: they are files in `/usr/local/share/piphone/sounds`
(or wherever `--sounds` points), uploaded at `/config`. `src/sounds.rs` holds the one table that
says which sounds there are and what they are called - the fixed ones as an enum with a `Spec` per
variant, exactly like `src/command.rs` holds the features, so a name cannot be mistyped and point
somewhere else.

**Fixed sounds** (`boot`, `reboot`, `shutdown`, `ringtone`, `test_loud`, `test_earpiece`,
`confirm`) have one file each under a fixed name: `boot` is `boot.wav` or `boot.mp3`.

**Collections** are lists of uploaded files under `ringtones/` and `sleep/`. A track keeps the name
it was uploaded under - that name is what tells two tracks apart. A ringtone does not: the page
asks for the number next to the file field, it is required, and the server derives the whole name
from it (`ringtone_name`), so neither a browser nor a script can choose one:

- `ringtones/ringtone-<Rufnummer>.wav` is the ringtone of that one caller. Only that exact
  `ringtone-` prefix followed by digits counts; anything else in `ringtones/` is not played and
  not listed (`Pool::counted`), and it stays where it is rather than being cleaned up. A name like
  `ring-01` binds a file to nothing, or it would belong to the number `01`.
- A caller without a file of their own gets the fixed `ringtone`, and a phone that has neither is
  silent rather than guessing.
- Everything in `sleep/` is one playlist, shuffled anew on every start and played for at least
  15 minutes (a single file is played once).

Both formats are the same everywhere:

- The extension follows the upload, not the other way round: the format is recognised from the
  first bytes, so a file is `ringtone-01.wav` or `ringtone-01.mp3` and the preview is served with the
  content type the bytes actually have. Playback does not depend on it - symphonia probes the
  content - but a MP3 announced as `audio/wav` does not play in a browser.
- An upload that is neither WAV nor MP3 is refused, and written through a temporary file like the
  config: an upload that fails halfway leaves the old file, not half a new one. Replacing a file
  also removes the file of the other extension, because only one of the two can be the sound.
- A name from the browser or a script is trimmed down to what can be a file name in that directory
  (`file_stem`); a name that would escape it is refused, not repaired. Only files whose name ends
  in `.wav` or `.mp3` belong to a collection, and nothing is deleted that the phone did not write:
  a file put there by hand should stay, whether it is listed or not.
- Missing fixed sounds are listed at startup and silent afterwards: playback of an empty path is
  what the audio channel has always done, and a sound that is not there is not an error at that
  moment.
- `[sounds]` and `[ringtones]` are both gone from the configuration. A file that still has them
  gets them named as unknown at startup and loses them on the next save.

A list option puts every entry on its own line of a textarea, unless `item_fields` gives each entry a
labelled input of its own - which is what the wake-up times use: seven weekday pickers, because a
textarea of `06:30` seven times cannot say which of them is Saturday. Such an option has to name as
many labels as it requires entries, because the form sends one value per field in the order of the
labels; `labelled_lists_have_one_field_per_entry` asserts that, and it holds for every option, so a
new one cannot get it wrong quietly.

Changes are never applied to the running process and never trigger a restart; the UI points out
that a restart is required. Writing the file regenerates all comments from the schema, so comments
written by hand in the file are not preserved. For the same reason `dump` writes the file from
`SECTIONS` alone and an entry no option exists for is *dropped* rather than carried over: a leftover
from another version would otherwise sit in the file forever, unread and untended. That is data
loss, so it is not silent - `config::unknown` names what is there at startup and `Saved::dropped`
names what a save removed, which the page prints next to the restart notice.

The file is saved through a temporary file that is written, read back, compared and only then moved
into place. Its name carries the process id *and* a counter, because two tabs in a browser are
enough to make two saves overlap - with one shared `.tmp` they would take each other's control file
away and leave one of the two half written behind.

The phone runs on the values as they stand in the file, not on the checked ones, so a rejected value
is used as written rather than replaced by its default. That is deliberate for a physical device - a
defaulted GPIO pin would quietly point somewhere else - but it means the validation errors have to
be said out loud, which `Phone::new` does at startup.

# Code structure

Three things are single points of truth on purpose. Each replaced two places that used to have to be
kept in step by hand, and each is a place where being out of step was silent rather than loud:

- **`src/command.rs` holds every feature and both of its names.** A feature is reachable as the
  target of a short code in `[numbers]` (`enable-night-mode`) and as an action URL
  (`/api/action/night-mode`), and the two names are different. `Command::dial_name` /
  `web_name` and the `from_dial` / `from_web` lookups replace what used to be a list of dial names
  in `config.rs`, a `parse` in `webserver.rs`, and two dispatch arms. `DIAL_NAMES` - what the schema
  accepts and suggests - is derived from the enum, so it cannot drift from what the dial
  understands. Adding a feature is adding a variant. Worth keeping: an unknown target used to fall
  through to the "place a call" arm, so a mistyped command name made the phone dial the typo.
- **`Timer` is the only way to arm a timed job.** See the platform notes for what that costs.
- **`crate::sync::lock()` is the only way to take a mutex.** Sixty-odd call sites said
  `lock().unwrap_or_else(|e| e.into_inner())` between them, which is noise at every one of them and
  says nothing. The reasoning is in the module: `panic = "abort"` means a release build cannot hit
  it at all, and under the profiles that unwind, the data behind a poisoned lock is still good.

The state machine itself is `src/phone/mod.rs`, with the two parts of it that are not the state
machine next door: `schedule.rs` for the decisions that are pure functions of the clock and the
configuration, and `web.rs` for the `PhoneApi` view. All three are one module, so `web.rs` can still
read the phone's private state; the split is about navigation, not access.

# Platform notes

- **GPIO** uses `gpiod`, because the wiring needs pull-up bias, which `gpio-cdev` does not support.
- **libgpiod 2.x is a hard requirement** (`gpiod` crate feature `v2`, which links the 2.x ABI).
- **The contacts are read as edges, not polled.** All three lines are claimed in one `Chip::request_lines` call with `EdgeDetect::Both`, and a dedicated thread blocks in `Lines::read_event`. `Watcher` rebuilds the 1 ms grid `PulseDecoder` was written for from the kernel timestamps on the events, which is what keeps a dial rotation decodable while a callback is busy.
- **A gap longer than `MAX_SAMPLES` has to lose its tail, not carry it forward.** This was the "number too long" bug: after the cap kicked in, `advance` kept the *whole* unfed remainder in `carry` instead of only the sub-millisecond remainder, so the leftover was fed in again on the next call and the next. The phone sits unused for hours between calls, so that is not a rare edge case but the normal one — and it left `carry` holding hours, which made *every* later call step the decoder through `MAX_SAMPLES` (10 s) of contact levels at once. `NSA_DEBOUNCE` is then measured against those ten seconds instead of against real time, so the few milliseconds of contact bounce the debounce exists to reject read as a full rotation and each one yields a digit of its own. A single "0" arrives as `0` plus a few stray `0`s, no short code matches, and the phone reports "Ziffernfolge zu lang" over and over until it is restarted. The Python original never showed this because it *polled* NSA every 5 ms, so bounce shorter than the debounce was invisible to it. Note that a truncated gap fed at constant levels can still only settle a debounce, never invent a rotation — that is why dropping the tail is safe and dropping the samples entirely would not be.
- **The reader thread cannot be shut down.** `Lines` owns the file descriptor, `read_event` blocks in the kernel and the crate does not expose the descriptor, so there is no way to interrupt it. `Watcher` and the phone live for the whole process anyway; a `Drop` would leave one thread parked, which is why there is no `Drop`.
- **The LEDs are dimmed by software PWM, and the frequency is the whole cost.** Every period is two GPIO writes plus a timer wait, so the 8 kHz of the Python original woke a thread 8000 times a second for a light that cannot show the difference - measured at **16% of a core** on the Pi Zero. `PWM_FREQUENCY` is now 500 Hz, which measured **2%** at 50% duty, so 8x less for a light that looks the same. (1000 Hz measured 5%, 200 Hz measured 1% but flickered - see below.)
- **The LED frequency must not be lowered on the assumption that a lower number is flicker-free - 200 Hz flickered visibly.** The threshold depends on the observer, the brightness and the angle, and a bright amber night light in a dark room is nearly the worst case: peripheral vision resolves flicker well above what foveal vision can. This is only knowable by looking at the real LED, so the frequency was walked down 1000 -> 500 Hz by eye. If it flickers again, raise it.
- **The PWM loop anchors its two half periods to *different* points on purpose, and swapping them back breaks the light.** The on-pulse is measured from the start of the cycle, so the duty - the brightness - is right; the off-time is measured from the *end* of the on-pulse, because a sleep can only end late and the overrun has to go somewhere. A sleep cannot end early, so anchoring the gap to the cycle instead lets an overrun eat it: once the overrun is as long as the gap, the gap closes and the LED jumps to full brightness until the next cycle. Measured at 82% duty, the cycle-anchored version delivered 98.6% even with no load at all, and swung to 100% under load - that is the "light level varies" symptom. The gap is not negotiable; the frequency is. The cost is that the period stretches under load, so the effective frequency drops (measured 938 Hz asked, 766 Hz delivered at 1000 Hz), which is the correct thing to give up: brightness *is* the duty, while the frequency only has to stay above the flicker threshold.
- **A duty of 0 or 100 is a plain level, not a slow PWM.** `timings` clamps the duty and returns `None` at either end of the range, which is what stops an out-of-range duty from producing a thread with a zero-length half period that toggles the pin as fast as it can. It also takes the frequency out of the check, so a full-brightness blink is a steady light instead of a spin. The arithmetic is a free function and unit-tested without an LED.
- **`/api/state` fields that reach `new Date()` must be full ISO 8601, not a bare time.** `next_wake_up` was sent as `%H:%M`, which is not a parseable date, and the status page rendered a literal "Invalid Date". The same field was stored as a `NaiveTime`, so it had lost the date as well - and the page asks for the weekday, so a time-only value could not have been right even if it had parsed. Both are fixed: it is a `NaiveDateTime` and `%Y-%m-%dT%H:%M:%S`. Note that `new Date()` returns `Invalid Date` **silently** rather than throwing, and a time-only *scheduled* value is genuinely ambiguous, not merely unparseable - send the date. The format deliberately has no `Z`: the phone computes in local time (Europe/Berlin) and JS parses a zone-less ISO string as local, so adding `Z` would shift the displayed time by the UTC offset.
- **Call audio** opens the USB card as `plug:usb`. That codec only runs at 44.1 or 48 kHz with a single
  channel; without the `plug` wrapper ALSA rejects the 8 kHz that G.711 requires. The earpiece tones
  use the plain `usb` alias instead, which already matches the stereo files in `sounds/`. ALSA will not
  share a USB device between two open streams, so the bridge is opened **per call**, never at startup;
  holding it open would silence the dial tone and the earpiece test.
- **The call bridge has to hand the card back, and wait for it.** `UsbBridge`'s two ALSA threads loop
  until `active` goes false, and the streams live inside those threads: nothing else can close them. So
  clearing the flag and returning is not enough - the next dial tone would still find `plug:usb` busy and
  stay silent, and a second call would silently fall back to a bridge without audio. `UsbBridge::stop`
  clears the flag *and* joins both threads before it returns, `Drop` calls it as well, and `CallAudio`
  keeps the bridge of the running call so `SipClient::forget_call` can hand the card back.
- **Order matters when the card changes hands, and both directions have bitten.** The busy tone after a
  call plays on the card the conversation just held, so `forget_call` releases it *before* the end event
  goes to the phone; the ringback tone is still playing when a call we placed gets answered, so the phone
  stops it and only then sends `SipCommand::StartCallAudio` - an answered incoming call skips that wait
  because the phone freed the earpiece before it answered. Neither is a lock: both are "make sure the
  other one is finished first", and getting either backwards costs a silent earpiece or a silent call.
  `playback-probe -- sounds tones` walks both handovers on the real card.
- **The SIP endpoint** binds to all interfaces but advertises the address the OS would use to reach the
  registrar, because SDP has to carry a routable address.
- **Registering** has to happen *after* the event stream exists, or the `RegistrationChanged` event is
  emitted to nobody and the phone never shows as registered.
- **`Endpoint::events()` is observation-only and cannot answer a call.** It subscribes to the
  observational dispatcher, which strips the session lifecycle handle. An `IncomingCall` taken from
  it looks fine but `answer()` fails with "session not found" — the library's own
  `next_incoming_call` says as much in a comment. Use `Endpoint::split()` and keep the returned
  `EndpointEvents`: that stream *is* the peer's private control stream and carries the handle. Do not
  re-derive it from `EndpointControl::events()`, which is the observational path again.
- **`Endpoint::invite()` only builds the request.** Nothing reaches the wire until
  `.send().await` is called on the returned builder.
- **The `CallAnswered` event carries a stale handle.** It is captured when the event is mapped, and
  `audio()` later rejects it with "session not found". The outgoing path keeps the `CallId` returned
  by `send()` and rebuilds the call with `Endpoint::wrap_call(id)`; that handle is current.
- **`audio()` claims the call's media authority and must not be delayed.** Opening ALSA takes long
  enough to stall the SIP event loop and let a BYE tear the session down first, so the audio task
  awaits `audio()` first and only then opens the sound card on a blocking thread.
- **Call outcomes are worth logging.** `CallProgress` carries the SIP status code; without printing
  it a failed call is indistinguishable from one that was never sent.
- **Everything the dial triggers runs on the GPIO watcher thread**, which is a plain OS thread with no
  reactor. A bare `tokio::spawn` or `tokio::task::spawn_blocking` on that path panics with "there is
  no reactor running" — and only as soon as somebody dials that feature, so it never shows up in
  testing. Always go through `self.runtime`. A unit test asserts this textually, because the panic
  cannot be provoked without the hardware.
- **`Channel::wait()` waits on the playback thread, not on a child process.** Playback is in-process:
  symphonia decodes, ALSA writes, one thread per sound. `wait()` blocks on a condvar that the thread
  signals when it is done, and `stop()` sets a flag and then waits for the same signal, so the sound
  card is free again before the caller can start the next sound. The thread must call `finish()` on
  every exit path, or `stop()` would block forever.
- **Playback must drain before reporting the sound over.** ALSA buffers whatever is written, so the
  decode loop finishes long before the card has played it. Without draining, `wait()` returns while
  the last fraction of a second is still audible, and a confirmation tone sounds cut off. The drain
  polls `avail()` against the buffer size in short slices, so a stop request still cuts it short.
- **The sample rate is requested with `ValueOr::Less`, never `Nearest`.** The C-Media USB codec in
  the earpiece rejects an approximate rate: with `Nearest` the stream reaches `Prepared` and then
  `snd_pcm_start` fails with a broken pipe, while `aplay` on the same card works. `Less` makes ALSA
  commit to the rate the hardware actually runs (44100 comes back as 44099) and `plug:` covers the
  rest. This is not cosmetic — it is the difference between a working earpiece and no sound at all.
- **symphonia needs the `pcm` feature for WAV.** The `wav` feature only registers the reader; without
  `pcm` there is no decoder for plain PCM and every WAV file fails with "unsupported audio codec".
  The MP3 files are bare ADTS streams with ID3v2 tags, which is why `mp3` and `id3v2` are both on.
- **The call-progress tones are generated, not decoded.** `Tone` in `src/audio.rs` holds the sequences as
  frequency/duration steps and renders them to PCM in memory: Freizeichen is a steady 425 Hz, Besetzt 425 Hz
  for half a second then half a second of silence, Freiton a second of 425 Hz then four of silence, and so on.
  They are not configurable, because there is nothing to configure - a file would have to be decoded to arrive
  at the sine that the table already describes. Two details matter and are easy to break: the endless step is
  one second long and repeated by the playback loop, which only works because every frequency completes a whole
  number of cycles per second, and the loop checks the stop flag between short slices, so lifting the handset
  does not wait out a whole pass.
- **The earpiece is a single channel shared by everything the phone says.** The call audio opens it too, so the
  tones have to give way: the ringback stops in `CallAnswered` before the bridge grabs the card, and
  `play_earpiece_tone` silences the loudspeaker so the sleep music cannot play underneath a dial tone.
- **A call that is still ringing has no answered handle.** `current_call` is only set once a call is
  answered, so hanging up during setup used to do nothing and left the other end ringing. Hangup
  falls back to the outgoing `CallId`, which rvoip turns into a proper CANCEL.
- **A timer has to be *aborted*, not forgotten.** Every timed job is "do this in a while, unless
  something comes along first", and dropping a `JoinHandle` instead of aborting it leaves the job
  running with nothing left to cancel it. That is how an abandoned ring timer came to decline
  whichever call was ringing by the time it fired - and, if that call had meanwhile been answered,
  to send a `Hangup` into a live conversation. `Timer` in `src/phone/mod.rs` is therefore the only
  way to arm one: `start` cancels first, so there is nothing to remember.
- **The ringing lock is a window, and hour zero is inside it.** `dnd_from`/`dnd_to` are an evening-
  to-morning range, so `from > to` is the normal case and midnight belongs to the window. The old
  condition spelled that `0 < hour && hour <= to || 0 < from && from <= hour`, where the `0 < hour`
  guard kept `to = 0` switched off - and left the hour between midnight and one o'clock unlocked, so
  the phone rang at half past midnight. A zero now switches the lock off at the *bounds*, which frees
  the hour itself to be compared like any other. The same branch also stops a `from <= to` window
  from running on until midnight.
- **Night mode is "the light is on", not "a timer is running".** Both ways into the mode - night
  mode itself, and the wake-up light that replaces it - are just the night light being lit, and the
  two error paths switch it on *and then* give up when the wake-up times turn out to be unusable.
  Deriving the state from the timer left those runs with no way to switch the light off again:
  the next code took the "start" branch, because there was no timer, and lit it once more.
- **The GPIO watcher thread must not wait for anything.** It is the thread that has to notice the
  handset going down and the next digit coming in. Work that waits - a confirmation tone, the
  earpiece test with its deliberate half-second pause - goes to `background()`, which is the runtime's
  blocking pool. The dial decoding survives a blocked watcher either way, because the kernel
  timestamps the edges and they are buffered, but the hook switch does not: lifting the handset
  during the confirmation tone did nothing for as long as the tone played, and an incoming call was
  not even seen.
- **`SipCommand::Shutdown` is the clean way out.** On SIGTERM the session hangs up and unregisters
  before it ends, so the other end gets a BYE. Just exiting stops the RTP and sends nothing, and the
  caller only learns about the end from their own session timer - long enough to be a noticeable
  stretch of dead air with the handset still against their ear. `Phone::stop_sip` sends the command
  and waits a second, because the teardown is not observable from the outside.
- **The outgoing audio pump is woken, not polled.** The sender used to look for a frame and, finding
  none, wait out the whole 20 ms frame clock before looking again - which added most of a frame to
  every frame that arrived just after a look, and that is the caller's own voice. `RingBuffer` now
  carries a `Notify` and `AudioBridge::arrivals` hands it to the pump. Note that the wait has to be
  preceded by the pop: the frame can arrive in between, and `Notify` keeping a permit is what makes
  that frame count instead of being lost for another 20 ms.
- **The RTP clock advances by what was really sent.** `pcmu_sized_mono_8khz` only labels the rate, it
  does not pad the frame, and the capture thread hands back `buffer[..count]` with whatever the card
  returned. A fixed `FRAME_SAMPLES` per frame therefore drifts against the payload as soon as a read
  wakes early.