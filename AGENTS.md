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
- Increment the version on each change, but at most once per git commit.

# Building

- Only ever compile debug and tests on macOS or Windows, never the release.
- Access to a Raspberry Pi Zero 2W with wired-up hardware can be provided for production testing.
- Access to a Raspberry Pi 5 with 4GB of RAM can be provided for compiling the production version.
- Avoid compiling debug binaries on the Raspberry Pi 5 for efficiency reasons.

The Pi Zero cannot compile this; memory is too small for `rustc` on the `rvoip` dependency tree. Build on
a larger machine (see AGENTS.override.md) and copy the binary over: `cargo build --release`.

Tests need no hardware. They must be linked serially on a Pi, because the linker is the memory peak:
`cargo test -j 1`. The tools in `examples/` are behind the `harness` feature so `cargo test` does not build
them.

The web pages have their own tests: `npm install && node tests/test-webpage.js`. They render the pages
through `cargo run --features harness --example render-page`, which is a compile - and compiling on a
development machine is not allowed. To run them from one anyway, render on the Pi and point
`PIPHONE_RENDER` at the directory holding its output.

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
| `src/lockfile.rs` | The `flock` that keeps a second instance off the hardware |
| `src/webserver.rs` | axum routes, basic auth, actions |
| `src/pages.rs` | The HTML pages, rendered from the schema |
| `src/main.rs` | Argument parsing and startup |
| `examples/` | Hardware test tools, see below |
| `support/etc/` | systemd unit |

Hardware access lives behind `#[cfg(target_os = "linux")]`, with a stub backend elsewhere, so the
crate builds and its tests run on a development machine.

# Hardware test tools

Each opens the pins it needs, so run them one at a time, and each needs the `harness` feature, which
`cargo run --example` does not switch on by itself: `cargo run --features harness --example <name>`.

| Example | What it does |
| --- | --- |
| `hardware-connections` | Live state of dial, impulse contact and hook switch |
| `hardware-dial` | Decodes dialled digits |
| `hardware-audio` | Ringtones, loudspeaker and earpiece test, opens the call bridge |
| `gpio-probe` | Raw pin levels and edge watching, for diagnosing wiring |
| `audio-probe -- <device> <rate> <channels>` | Tries ALSA parameter combinations |
| `decode-probe -- sounds` | Decodes every file in `sounds/`, no sound card needed |
| `playback-probe -- sounds` | WAV and MP3 on both devices plus the generated tones; checks the repeat loop, an early stop and the real duration (`-- sounds tones` leaves out the files: quiet enough for a phone in service) |
| `incoming-probe` | Registers like the phone and reports one incoming call, to tell `rvoip` from our logic |
| `render-page -- CONFIG_PAGE` | Prints what `tests/test-webpage.js` renders |

# Web interface

An optional web server (`src/webserver.rs`), set in `[web]` (port 0 = disabled, or `--no-web`), shows
uptime, connectivity, call state and the state of the features. `/api/state` is the JSON,
`POST /api/action/<name>` does what a short code does, `/config` edits every option. A sound is uploaded as
the raw body with `POST /api/sound/<name>` (not a multipart form) and served back with `GET` on the same
path; a collection file is `POST`/`GET`/`DELETE /api/sound/<collection>/<file name>`. The names in those
URLs come from `src/command.rs` and `src/sounds.rs`, not from strings in the router.

# Configuration

`src/config.rs` is the single source of truth: `SECTIONS` holds every option (type, default, limits,
comments) and does the reading, validating and writing, and the JS in `src/pages.rs` renders that same
schema into the forms at `/config`, so a new option appears in the UI by itself. The sounds section of
that page is the exception - it is built from the uploaded files.

- The file is read once at startup. A save from the web interface restarts the **service**
  (`Phone::restart`, `systemctl restart --no-block piphone`) and applies nothing else; the machine reboots
  only when somebody asks for it deliberately.
- A write regenerates all comments from the schema and emits `SECTIONS` alone, so hand-written comments and
  entries no option exists for are *dropped*, not carried over. That is data loss, so it is said out loud:
  `config::unknown` at startup, `Saved::dropped` on the page.
- Saving goes through a temporary file that is written, read back, compared and only then moved into place,
  named with the process id *and* a counter, because two browser tabs are enough to make two saves overlap.
- The phone runs the values as written, not the checked ones, because a defaulted GPIO pin would quietly
  point somewhere else; that is why `Phone::new` prints the validation errors at startup.
- A list option is one entry per line of a textarea, unless `item_fields` gives each entry its own labelled
  input (the seven weekday wake-up times). It then has to name as many labels as it has entries, because the
  form sends one value per field in label order; `labelled_lists_have_one_field_per_entry` asserts that.

# Sounds

The sounds are files, not options: they live in `/usr/local/share/piphone/sounds` (or `--sounds`) and are
uploaded at `/config`. `src/sounds.rs` holds the one table of what exists and what it is called - the fixed
ones as an enum with a `Spec` each, like `src/command.rs` holds the features - so a name cannot be mistyped
and point somewhere else.

- **Fixed** (`boot`, `reboot`, `shutdown`, `ringtone`, `test_loud`, `test_earpiece`, `confirm`) have one file
  each under a fixed stem, `boot.wav` or `boot.mp3`. Missing ones are listed at startup and silent
  afterwards, because playback of an empty path is what the audio channel has always done.
- **Collections** are the uploaded files under `ringtones/` and `sleep/` (only `.wav` and `.mp3` belong to
  one), and the extension follows the upload, not the other way round: the format is recognised from the
  first bytes and the preview is served with the content type the bytes actually have, because a MP3
  announced as `audio/wav` does not play in a browser. Playback does not depend on it - symphonia probes the
  content - so do not "fix" an extension that disagrees with the file.
- `ringtones/ringtone-<Rufnummer>.<ext>` is the ringtone of exactly that caller, and only that prefix
  followed by digits counts (`Pool::counted`): anything else there is neither played nor listed, and stays
  where it is. The name is derived server-side from the required number next to the file field, so neither
  a browser nor a script can choose one. A caller without a file of their own gets the fixed `ringtone`; a
  phone that has neither is silent rather than guessing.
- Everything in `sleep/` is one playlist, shuffled anew on every start and played for at least 15 minutes
  (a single file once). A track keeps the name it was uploaded under - that is what tells two apart.
- An upload that is neither WAV nor MP3 is refused and written through a temporary file like the config, so
  a failed upload leaves the old file rather than half a new one; replacing a file also removes the other
  extension. Names from a browser or script are trimmed to what can be a file name in that directory
  (`file_stem`), and one that would escape it is refused, not repaired. Nothing the phone did not write is
  deleted.
- `[sounds]` and `[ringtones]` are gone from the configuration: a file that still has them is told so at
  startup and loses them on the next save.

# Code structure

Three things are single points of truth on purpose, each replacing two places that had to be kept in step
by hand and each failing silently when they drifted:

- **`src/command.rs` holds every feature and both of its names.** A feature is a short-code target in
  `[numbers]` (`enable-night-mode`) and an action URL (`/api/action/night-mode`), and the two names differ.
  `Command::dial_name` / `web_name` and the `from_dial` / `from_web` lookups replace a list of dial names in
  `config.rs`, a `parse` in `webserver.rs` and two dispatch arms, and `DIAL_NAMES` - what the schema accepts
  and suggests - is derived from the enum. Adding a feature is adding a variant. Worth keeping: an unknown
  target used to fall through to the "place a call" arm, so a typo made the phone dial the typo.
- **`Timer` is the only way to arm a timed job** (see below).
- **`crate::sync::lock()` is the only way to take a mutex.** Sixty-odd call sites said
  `lock().unwrap_or_else(|e| e.into_inner())`, which is noise at every one of them and says nothing. The
  reasoning is in the module: `panic = "abort"` means a release build cannot hit it at all, and under the
  profiles that unwind the data behind a poisoned lock is still good.

The state machine is `src/phone/mod.rs`, with `schedule.rs` (the decisions that are pure functions of the
clock and the configuration) and `web.rs` (the `PhoneApi` view) next door. All three are one module, so
`web.rs` can still read the phone's private state; the split is about navigation, not access.

# Platform notes

A do-not-break list. Each line is an invariant that a change can violate silently, and the reason it is
there; what it used to cost is left out.

## GPIO and LEDs

- **`gpiod` with crate feature `v2`**, not `gpio-cdev`: the wiring needs pull-up bias.
- **Contacts are read as edges, not polled** - one request for all three lines with `EdgeDetect::Both`, a
  thread blocked in `Lines::read_event`, and `Watcher` rebuilding the 1 ms `PulseDecoder` grid from the
  kernel timestamps, so a rotation stays decodable during a callback.
- **`advance` drops the tail of a gap over `MAX_SAMPLES`**, keeping only the sub-millisecond remainder in
  `carry`; a truncated gap at constant levels can settle a debounce but never invent a rotation.
- **No `Drop` on the watcher**: `read_event` blocks in the kernel on a descriptor `Lines` does not expose.
- **`PWM_FREQUENCY` is 500 Hz** (8 kHz cost 16% of a core). Never lower it - 200 Hz flickered visibly - and
  raise it if the LED flickers again.
- **The PWM on-pulse is timed from the cycle start, the off-time from the end of the on-pulse**: a sleep
  can only overrun, and anchoring both to the cycle lets the overrun eat the gap. The period stretches
  under load instead: brightness is the duty, the frequency only has to stay flicker-free.
- **Duty 0 or 100 is a plain level**: `timings` clamps and returns `None`, so there is no thread with a
  zero-length half period.

## Audio

- **Call audio opens the card as `plug:usb`** - that codec runs only at 44.1 or 48 kHz mono, and the
  wrapper is what makes the 8 kHz G.711 possible. Earpiece tones use the plain `usb` alias.
- **The bridge is opened per call and must give the card back**: the streams live inside its two threads,
  so `UsbBridge::stop` clears `active` *and* joins both, `Drop` calls it, and `CallAudio` keeps the running
  bridge so `forget_call` can release it. Clearing the flag alone leaves the next dial tone without a card.
- **Order at a handover**: `forget_call` releases the card *before* the end event (the busy tone plays on
  it), the ringback stops *before* `SipCommand::StartCallAudio`. Not locks - "let the other finish".
  `playback-probe -- sounds tones` walks both on the real card.
- **Playback is in-process**: `wait()` blocks on a condvar that the thread signals from `finish()` on
  *every* exit path, and `stop()` waits for the same signal, so the card is free before the next sound.
- **Playback drains `avail()` before it reports done**, or the confirmation tone sounds cut off; polling in
  short slices keeps a stop request short.
- **Sample rate with `ValueOr::Less`, never `Nearest`** - the C-Media codec rejects an approximate rate
  and fails in `snd_pcm_start` with a broken pipe, while `aplay` on the same card works.
- **symphonia needs `pcm` for WAV** (`wav` only registers the reader); the MP3s are bare ADTS with ID3v2.
- **The tones are generated in memory** (`Tone`), not configured: the endless step is one second, which
  works only because every frequency completes whole cycles per second, and the loop checks the stop flag
  between short slices.
- **The earpiece is one channel shared by everything**: the ringback stops before the bridge takes the
  card, `play_earpiece_tone` silences the loudspeaker.

## SIP (`rvoip`)

- **Advertise the address the OS would use to reach the registrar** (SDP has to carry a routable one) and
  **register after the event stream exists**, or `RegistrationChanged` reaches nobody.
- **`Endpoint::events()` cannot answer a call**: the observational dispatcher strips the session handle.
  Use `Endpoint::split()` and keep its `EndpointEvents`; never re-derive from `EndpointControl::events()`.
- **`invite()` only builds the request**; `.send().await` puts it on the wire.
- **`CallAnswered` carries a stale handle** - rebuild with `Endpoint::wrap_call(id)` from the `CallId`.
- **`audio()` must be awaited before ALSA is opened**, or the event loop stalls long enough for a BYE to
  tear the session down.
- **Hangup while still ringing** falls back to the outgoing `CallId` (a CANCEL), because `current_call`
  exists only once answered.
- **Log `CallProgress`** - it carries the status code, and without it a failed call looks unsent.
- **Exit with `SipCommand::Shutdown`**, which hangs up and unregisters before the process ends, so the
  other end gets a BYE instead of dead air. `Phone::stop_sip` waits a second: the teardown is invisible.

## Threads, timing and state

- **No `tokio::spawn` on the GPIO watcher thread** - it has no reactor, and the panic only shows when
  somebody dials that feature. Go through `self.runtime`; a unit test asserts this textually.
- **Nothing may block the watcher thread**, since it has to notice the handset going down: waiting work
  goes to `background()`. Dial decoding survives a blocked watcher (the kernel buffers the timestamps),
  the hook switch does not.
- **Timers must be aborted, not forgotten**: `Timer::start` cancels first, so a job left by a dropped
  `JoinHandle` is still running with nothing to cancel it.
- **The ringing lock is a wrapping window with midnight inside it**: `from > to` is normal, and a zero
  switches the lock off at the *bounds*. The old `0 < hour` guard kept `to = 0` switched off and left the
  hour between midnight and one o'clock unlocked, so the phone rang at half past midnight.
- **Night mode is "the light is on", not "a timer is running"** - both ways in are the light and both error
  paths light it and give up, so the state must be derivable from the light.
- **The outgoing audio pump is woken, not polled**: `RingBuffer` carries a `Notify`, and the wait must be
  *preceded* by the pop, so a frame arriving in between still counts.
- **The RTP clock advances by what was really sent**, not by a fixed `FRAME_SAMPLES` per frame.
- **Times that reach `new Date()` are full ISO 8601** (`%Y-%m-%dT%H:%M:%S`, no `Z`, since the phone
  computes in local time): a bare time fails silently, and the page asks for the weekday.