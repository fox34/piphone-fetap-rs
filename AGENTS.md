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

- This project runs on Raspberry Pi OS
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

# Layout

| Path | Contents |
| --- | --- |
| `src/phone.rs` | The state machine: dial, hook switch, features, watchdog |
| `src/sip.rs` | `rvoip` endpoint wrapper, event loop, call decisions |
| `src/callaudio.rs` | ALSA bridge between the SIP stack and the USB sound card |
| `src/audio.rs` | Sound playback on the `i2s` amplifier (ringtones, tests, sleep music) |
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
- `cargo run --example gpio-probe` — reads raw pin levels, for diagnosing wiring
- `cargo run --example audio-probe -- <device> <rate> <channels>` — tries ALSA parameter combinations

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
- **Pulse detection** polls at 1 ms instead of using edge detection: libgpiod's `read_event` is
  `todo!()` without the `v2` feature, and polling also works across libgpiod 1.x and 2.x.
- **Call audio** opens the USB card as `plug:usb`. That codec only runs at 44.1 or 48 kHz with a single
  channel; without the `plug` wrapper ALSA rejects the 8 kHz that G.711 requires. The earpiece tones
  use the plain `usb` alias instead, which already matches the stereo files in `sounds/`. ALSA will not
  share a USB device between two open streams, so the bridge is opened **per call**, never at startup;
  holding it open would silence the dial tone and the earpiece test.
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
- **`Channel::wait()` must leave the child in its slot.** Taking it out first makes the waiting
  thread the only owner, and a later `stop()` then finds nothing to kill — the sleep music runs to
  the end of its track however often it is asked to stop. It polls `try_wait()` instead, releasing
  the lock between checks so `stop` can still reach the child.
- **A call that is still ringing has no answered handle.** `current_call` is only set once a call is
  answered, so hanging up during setup used to do nothing and left the other end ringing. Hangup
  falls back to the outgoing `CallId`, which rvoip turns into a proper CANCEL.