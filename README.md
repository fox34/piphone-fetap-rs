# FeTAp611-VoIP-Tischtelefon auf Basis eines Raspberry Pi Zero 2 W

Diese Version ist der Rust-Port von [https://github.com/fox34/piphone-fetap](piphone-fetap).
Sie enthält einen eigenständigen SIP-Stack (`rvoip`-Crate) und benötigt keine externen Programme.
Verhalten, Konfiguration und Weboberfläche sind identisch zur Python-Version.

## Hardware-Komponenten

- FeTAp 611, nötige Originalkomponenten: Nummernschalter und Gabelkontakt
- Raspberry Pi Zero 2 W
- Netzteil: z. B. Hi-Link 5V, 3W (600mA)<br>
  *Hinweis: 3W/600mA sind knapp dimensioniert und setzen eine Drosselung des Pi voraus. Besser sind 5W/1A.*
- Verstärker für Lautsprecher: MAX98357A (bspw. auf fertiger Platine mit I2S)
- Lautsprecher: Visaton FR7, 4 Ohm, 5W Nennleistung
- USB-OTG-Adapter (Mikro-USB auf USB-A)
- USB-Soundkarte (einfacher/günstiger zu finden als USB-OTG-Soundkarte)
  hier: Vention = ID 0d8c:0014 C-Media Electronics, Inc. Audio Adapter (Unitek Y-247A)
- Hörer-Lautsprecher, bspw. aus altem Headset
- Hörer-Mikrofon, bspw. aus altem Headset

## Verkabelung

| Gerät/Funktion | Pin | GPIO | Pi-Pin-Nr. |
| :--- | :--- | ---: | ---: |
| **Hörerkontakt** | 1 | 15 | 10 |
|  | 2 | GND | 9 |
| **MAX98357** | BCLK | 18 | 12 |
|  | LRCK | 19 | 35 |
|  | DIn | 21 | 40 |
|  | GND | GND | 39 |
|  | VIn | +5V | 4 |
| **Nummernschalter** | 1 (nsi1) | GND | 14 |
|  | 2 (nsi2) | 23 | 16 |
|  | 3 (nsa1) | GND | 20 |
|  | 4 (nsa2) | 24 | 18 |

Optional verbaute LEDs (Nachtlicht, Aufwachlicht) werden über die Optionen `night_light_pin` und
`wake_light_pin` im Bereich `[pins]` konfiguriert.

# Linux konfigurieren

## Benötigte Komponenten installieren

```
sudo apt update ; sudo apt upgrade
sudo apt install git alsa-utils --no-install-recommends
sudo apt autoremove
```

Medien werden mittels [symphonia](https://crates.io/crates/symphonia) dekodiert und über
ALSA ausgegeben.
`alsa-utils` bleibt installiert, weil der Dienst `i2s-silence` unten `aplay` aufruft.

## MAX98357A aktivieren

Siehe
- https://learn.pimoroni.com/article/raspberry-pi-phat-dac-install
- https://learn.adafruit.com/adafruit-max98357-i2s-class-d-mono-amp/raspberry-pi-usage

Datei `/boot/firmware/config.txt` anpassen:

```
dtparam=i2s=on
#dtparam=audio=on
dtoverlay=max98357a
```

In `/etc/modprobe.d/raspi-blacklist.conf` folgende Einträge, falls vorhanden, entfernen:

- `i2c-bcm2708`
- `snd-soc-pcm512x`
- `snd-soc-wm8804`

In `/etc/modules` den Eintrag `snd_bcm2835`, falls vorhanden, entfernen.

## Reihenfolge der Soundkarten fixieren

Siehe https://wiki.archlinux.org/title/Advanced_Linux_Sound_Architecture#Card_index

Datei `/etc/modprobe.d/alsa-base.conf` anlegen:

```
options snd slots=snd_usb_audio
```

Damit bekommt die USB-Soundkarte (Hörer) immer Index 0.

## Soundkarten konfigurieren

Datei `/etc/asound.conf` anpassen bzw. anlegen:

```
# USB als Standardgerät
pcm.!default {
    type hw
    card 0
}

ctl.!default {
    type hw
    card 0
}

# USB-Soundkarte
pcm.usb {
    type hw
    card 0
    device 0
}

# MAX98357 über dmix, um parallel Stille zu senden (Knacken verhindern)
pcm.i2s_dmix {
    type dmix
    ipc_key 1024
    ipc_perm 0666  # Zugriff für alle Nutzer zulassen, optional
    slave {
        pcm {
            type hw
            card 1
            device 0
        }
        rate 48000
        channels 2
        format S32_LE
        period_time 0
        period_size 1024
        buffer_size 8192
    }
}

ctl.i2s_dmix {
    type hw
    card 1
}

# MAX98357 zusätzlich mit Softvol, um Lautstärke steuern zu können
pcm.i2s {
    type softvol
    slave.pcm i2s_dmix
    control {
        name "Lautspreker"
        card 1
    }
    min_dB -50.0
    # Maximale Lautstärke gleich hier konfigurieren
    max_dB -18.0
    resolution 100
}

ctl.i2s {
    type hw
    card 1
}
```

## Knacken am MAX98357 bei Start einer Wiedergabe verhindern

Siehe auch: https://github.com/volumio/Volumio2/issues/1973

Systemd-Dienst z.B. in `/etc/systemd/system/i2s-silence.service` anlegen:

```
[Unit]
Description=Play silence to I2S output using dmix to avoid pop/click noise
After=sound.target

[Service]
ExecStart=/usr/bin/aplay -D i2s -t raw -r 48000 -c 2 -f S32_LE /dev/zero
Restart=always
RestartSec=1
Nice=-20

[Install]
WantedBy=multi-user.target
```

Dann aktivieren mit 
```
systemctl daemon-reload
systemctl enable --now i2s-silence.service
```

## Lautstärke festlegen und für einen Neustart speichern

Hinweis: Funktionierte bei mir nur mit der USB-Soundkarte, nicht mit dem MAX98357.

```
alsamixer
sudo alsactl store
```

# Benutzung

```
piphone [Optionen]

  -c, --config <DATEI>   Pfad zur Konfigurationsdatei (Standard: /boot/piphone/config.toml)
      --ignore-dnd       Nicht stören für Testzwecke deaktivieren
      --no-web           Webserver zur Steuerung nicht starten
      --silent-boot      Keinen Start-Sound abspielen
  -v, --verbose          Ausführliches Logging aktivieren
  -h, --help             Diese Hilfe anzeigen
```

## Konfiguration

Die Konfiguration sollte bspw. unter `/boot/piphone/config.toml` abgelegt werden, damit sie auch in einem
schreibgeschützten Root-Dateisystem (bspw. OverlayFS) angepasst werden kann. Eine kommentierte Vorlage
gibt es in `support/config-example.toml`.

```
sudo mkdir -p /boot/piphone
sudo cp support/config-example.toml /boot/piphone/config.toml
sudo vim /boot/piphone/config.toml
```

Ein anderer Pfad kann über `-c/--config` angegeben werden.

# Bonusfunktionen

## Webserver

Über den Bereich `[web]` der Konfigurationsdatei kann ein Webserver aktiviert werden, mit dem sich
der Status des Telefons abrufen, die Konfiguration bearbeiten und die wichtigsten Funktionen auslösen lassen.
Die Statusseite wird automatisch alle zwei Sekunden aktualisiert.

```toml
[web]
port = 80           # 0 = deaktiviert
user = ""           # optionaler Zugangsschutz (HTTP Basic Auth)
pass = ""
```

Anschließend ist das Telefon im WLAN unter `http://<IP-Adresse-des-Pi>` erreichbar.

## Nacht- und Aufwachlicht

Es können über die Konfigurationsoptionen im Bereich `[misc]` sowohl eine Nachtlicht- als 
auch eine Aufwachlicht-LED konfiguriert werden.
Über die Kurzwahl `enable-night-mode` wird dann das Nachtlicht bis zur konfigurierten Uhrzeit aktiviert.

## Schlafmusik

Über die Kurzwahl `start-sleep-music` wird die Spieluhr aktiviert.
Die Spieluhr stoppt, sobald der Hörer abgehoben wird.

# Kompilieren

Kompilieren muss auf einem Linux-System (z.B. Debian Trixie) durchgeführt werden.
Der Pi Zero 2W hat zu wenig Arbeitsspeicher und der Prozess wäre sehr langsam.

Neben den u.g. Paketen wird auch eine Rust-Toolchain (`rustup`) benötigt.

```
apt install build-essentials pkg-config libasound2-dev libgpiod-dev
cargo build --release
scp target/release/aarch64-unknown-linux-gnu/release/piphone pi:/opt/piphone/piphone
```

Gestartet wird der Dienst über systemd, siehe `support/etc/systemd/system/piphone.service`:

```
sudo cp support/etc/systemd/system/piphone.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now piphone
```

## Tests

Die Tests für Schema, Weboberfläche und HTTP-Schnittstelle laufen ohne Hardware:

```
cargo test              # 94 Tests: 38 lib, 25 config, 12 config_web, 19 webserver
```

Der Test der Oberfläche benötigt zusätzlich Node.js und `jsdom`:

```
npm install jsdom
node tests/test-webpage.js
```

### Hardware-Tests

Diese sind nur für die Inbetriebnahme am Telefon gedacht. Jedes öffnet die Pins, die es braucht,
darum immer einzeln ausführen:

```
cargo run --example hardware-connections   # Nummernschalter, Impulskontakt, Hörer
cargo run --example hardware-dial          # Gewählte Nummern
cargo run --example hardware-audio         # Klingeltöne, Lautsprecher, Hörer, Gespräche
cargo run --example gpio-probe             # GPIO-Debugging (Verkabelung)
cargo run --release --example audio-probe -- plug:usb 8000 1   # ALSA-Parameter durchprobieren
```
