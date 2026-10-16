# Changelog

## 1.4.5

Features:

- The green status LED blinks while an incoming call rings.

## 1.4.4

Fixes:

- Restore the busy tone after remote hangup.

## 1.4.3

Fixes:

- A sound card that loses its place during a call is put back to work instead of
  silencing the conversation until it ends.
- The status page no longer reports two unused fields: `hungup` and the
  configuration file's `mtime`.

## 1.4.2

Fixes:

- A failed GPIO read is retried instead of leaving the dial and the hook switch
  silently dead.
- Declining an incoming call no longer takes an ongoing call down with it.
- A panic in an audio thread no longer deadlocks everyone waiting for the sound
  card.
- Two threads stopping the same sound now both wait for the card to be free.
- Dialling a feature no longer blocks the hook switch while a tone plays.
- A `systemctl` that fails no longer exits the phone in the middle of a call.
- Starting the LED dimming thread no longer takes the phone down if the thread
  cannot be created.

Changes:

- Commands are dispatched in one place, so the dial and the web interface can no
  longer drift apart.
- Added `rustfmt.toml` (`max_width = 120`).

## 1.4.1

Initial release.
