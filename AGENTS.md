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

- Never compile or test on macOS or Windows machines, except for End-to-End-Tests of the Webserver.
- Access to a Raspberry Pi Zero 2W with wired-up hardware can be provided for testing.
- Access to a Raspberry Pi 5 with 4GB of RAM can be provided for compiling.

# Layout

| Path | Contents |
| --- | --- |
| `src/phone.rs` | The state machine: dial, hook switch, features, watchdog |
| `src/sip.rs` | `rvoip` endpoint wrapper, event loop, call decisions |
| `src/callaudio.rs` | ALSA bridge between the SIP stack and the USB sound card |
| `src/audio.rs` | Sound playback: symphonia decoding plus ALSA, for the `i2s` amplifier and the `usb` earpiece; the call-progress tones are generated in memory |
| `src/gpio.rs` | Pulse decoding, watcher thread, LEDs with software PWM |
| `src/gpio/linux.rs` | `gpiod` backend, the only hardware-touching module |
| `src/config.rs` | Schema, validation, reading and writing the TOML file |
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
peak: `cargo test -j 1`.

The web pages have their own tests, run with node and jsdom:

```sh
npm install && node tests/test-webpage.js
```

# Hardware test tools

Each opens the pins it needs, so run them one at a time:

- `cargo run --example hardware-connections` — live state of dial, impulse contact and hook switch
- `cargo run --example hardware-dial` — decodes dialled digits
- `cargo run --example hardware-audio` — ringtones, loudspeaker and earpiece test, opens the call bridge
- `cargo run --example gpio-probe` — reads raw pin levels and watches for edges, for diagnosing wiring
- `cargo run --example audio-probe -- <device> <rate> <channels>` — tries ALSA parameter combinations
- `cargo run --example decode-probe -- sounds` — decodes every file in `sounds/`, no sound card needed
- `cargo run --example playback-probe -- sounds` — plays WAV and MP3 on both devices and the generated tones in
  the earpiece, checks the repeat loop and an early stop, and verifies each sound lasts as long as the file says
  (`playback-probe -- sounds tones` leaves out the files and only plays the tones, which is quiet enough to repeat
  on a phone that is in service)

# Web interface

An optional web server (`src/webserver.rs`) shows the phone status (uptime, connectivity,
call state) and the state of the features (night light, wake up mode, sleep music, dnd, next wake up time).
It is configured in the `[web]` section of the TOML config file (port 0 = disabled) and can also be disabled
with `--no-web`. The same actions as the rotary dial short codes can be triggered via
`POST /api/action/<name>`, the state is available as JSON under `/api/state`.

# Configuration

`src/config.rs` is the single source of truth for the TOML config file. It holds the schema of every
option (type, default value, description, limits) and provides reading, validation and writing.
`support/config-example.toml` is a documented sample.

The config file can also be edited in the browser at `/config`, which covers every option. Numbers
(dial codes and their targets in `[numbers]`) and ringtones are addable and removable tables. The
schema is rendered dynamically into forms by the JS in `src/pages.rs`, so adding an option to
`src/config.rs` makes it appear in the UI automatically.

Changes are never applied to the running process and never trigger a restart; the UI points out
that a restart is required. Writing the file regenerates all comments from the schema, so comments
written by hand in the file are not preserved.

# Platform notes

- **GPIO** uses `gpiod`, because the wiring needs pull-up bias, which `gpio-cdev` does not support.
- **libgpiod 2.x is a hard requirement** (`gpiod` crate feature `v2`, which links the 2.x ABI).
- **The contacts are read as edges, not polled.** All three lines are claimed in one `Chip::request_lines` call with `EdgeDetect::Both`, and a dedicated thread blocks in `Lines::read_event`. `Watcher` rebuilds the 1 ms grid `PulseDecoder` was written for from the kernel timestamps on the events, which is what keeps a dial rotation decodable while a callback is busy.
- **The reader thread cannot be shut down.** `Lines` owns the file descriptor, `read_event` blocks in the kernel and the crate does not expose the descriptor, so there is no way to interrupt it. `Watcher` and the phone live for the whole process anyway; a `Drop` would leave one thread parked, which is why there is no `Drop`.
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