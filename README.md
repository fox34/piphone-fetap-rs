# FeTAp611-VoIP-Tischtelefon auf Basis eines Raspberry Pi Zero 2 W

Diese Fassung ist die Rust-Neuschreibung von `../python-3.13`. Statt `linphonec` als Unterprozess
zu starten, bringt das Programm den SIP-Stack als `rvoip`-Crate mit. Verhalten, Konfiguration und
Weboberfläche sind ansonsten identisch geblieben.

Komponenten:

- FeTAp 611, nötige Originalkomponenten: Nummernschalter und Gabelkontakt
- Raspberry Pi Zero 2 W
- Netzteil: z.B. Hi-Link 5V, 3W (600mA)<br>
  *Hinweis: 3W/600mA sind eher knapp dimensioniert und setzen eine Drosselung des Pi voraus. Besser sind 5W/1A.*
- Verstärker für Lautsprecher: MAX98357A (auf fertiger Platine für I2S)
- Lautsprecher: Visaton FR7, 4 Ohm, 5W Nennleistung
- USB-OTG-Adapter (Mikro-USB auf USB-A)
- USB-Soundkarte (einfacher/günstiger zu finden als USB-OTG-Soundkarte)
  hier: Vention = ID 0d8c:0014 C-Media Electronics, Inc. Audio Adapter (Unitek Y-247A)
- Hörer-Lautsprecher aus altem Headset
- Hörer-Mikrofon aus altem Headset

# Verkabelung

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

Die beiden LEDs (Nachtlicht, Aufwachlicht) werden über die Optionen `night_light_pin` und
`wake_light_pin` im Bereich `[pins]` konfiguriert.

# Linux konfigurieren

## Komponenten installieren

```
sudo apt update ; sudo apt upgrade
sudo apt install git sox libsox-fmt-mp3 --no-install-recommends
sudo apt autoremove
```

Zum Bauen werden zusätzlich benötigt: `build-essential`, `pkg-config`, `libasound2-dev`, `libgpiod-dev`
und eine Rust-Toolchain (`rustup`).

## Konfigurationsdatei

Die Konfiguration sollte als TOML-Datei bspw. unter `/boot/piphone/config.toml` abgelegt werden, damit sie auch in einem
schreibgeschützten Root-Dateisystem (bspw. OverlayFS) angepasst werden kann. Eine vollständig kommentierte Vorlage
gibt es in `support/config-example.toml`.

```
sudo mkdir -p /boot/piphone
sudo cp support/config-example.toml /boot/piphone/config.toml
sudo nano /boot/piphone/config.toml
```

Ein anderer Pfad kann über `-c/--config` angegeben werden.

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

> **Hinweis zum Gesprächsaudio:** Für Gespräche öffnet der Code die USB-Soundkarte als `plug:usb`,
> nicht als `usb`. Dieser C-Media-Codec kann nur 44,1 oder 48 kHz und nur einen Kanal. G.711 im
> SIP-Stack arbeitet dagegen mit 8 kHz. Ohne den `plug`-Wrapper lehnt ALSA diese Rate ab; mit ihm
> übernimmt alsa-lib die Umwandlung, sodass der SIP-Stack bei seinen 8 kHz bleibt.
>
> Die Signale im Hörer (`waehlen-frei` und die beiden Testtöne) laufen weiterhin über `usb`, weil die
> Dateien dort bereits im Format der Karte vorliegen. ALSA kann eine USB-Karte nicht an zwei
> offene Streams gleichzeitig vergeben, deshalb wird die Gesprächsbrücke **pro Anruf** geöffnet und
> nicht dauerhaft beim Start. Sonst wäre der Freiton stumm.

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

# Bauen und installieren

Der Pi Zero ist für die Rust-Kompilierung zu klein; `rustc` wird beim `rvoip`-Abhängigkeitsbaum
vom Kernel beendet. Auf einem größeren Rechner bauen und das Binary übertragen:

```
cargo build --release
scp target/release/aarch64-unknown-linux-gnu/release/piphone pi:/opt/piphone/piphone
```

Anschließend der Dienst aus `support/etc/systemd/system/piphone.service`:

```
sudo cp support/etc/systemd/system/piphone.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now piphone
```

# Aufruf

```
piphone [Optionen]

  -c, --config <DATEI>   Pfad zur Konfigurationsdatei (Standard: /boot/piphone/config.toml)
      --ignore-dnd       Nicht stören für Testzwecke deaktivieren
      --no-web           Webserver zur Steuerung nicht starten
      --silent-boot      Keinen Start-Sound abspielen
  -v, --verbose           Ausführliches Logging aktivieren
  -h, --help              Diese Hilfe anzeigen
```

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

### Statusseite (`/`)

Angezeigt werden die Laufzeit, der Verbindungs- und Gesprächsstatus sowie der Zustand von
Nachtlicht, Aufwachmodus, Einschlafmusik, Klingelsperre und der nächsten Aufstehzeit.
Per Schaltflächen lassen sich Nachtmodus und Einschlafmusik ein- und ausschalten,
Lautsprecher und Hörer testen sowie das Telefon neu starten oder herunterfahren.

> **Achtung:** Der Webserver ist unverschlüsselt und erlaubt das Herunterfahren des Telefons.
> Im WLAN daher möglichst durch eine Zugangskontrolle (Firewall, separates VLAN) schützen.
> Für die Konfigurationsseite gilt das erst recht.

Alternativ lässt sich der Webserver auch per `--no-web` deaktivieren.

### Konfigurationsseite (`/config`)

Auf `/config` lässt sich die komplette TOML-Datei über Formulare bearbeiten - alle Abschnitte
und Optionen der Konfigurationsdatei. Dazu gehören:

- alle festen Optionen aus `[network]`, `[sip]`, `[pins]`, `[sounds]`, `[misc]` und `[web]`,
  jeweils mit Beschreibung und, wo sinnvoll, mit Grenzwerten (Pflichtfelder, GPIOPins 0-27,
  Stunden 0-23, Helligkeit 0-100, Port 0-65535)
- `[numbers]`: Kurzwahlen und Kurzbefehle als Tabelle, Zeilen lassen sich hinzufügen und entfernen,
  die gültigen Kurzbefehle werden als Vorschläge angeboten
- `[ringtones]`: rufnummerspezifische Klingeltöne, ebenfalls hinzufüg- und entfernbar
- Listen wie `sleep_music` und `wake_up_times`: ein Eintrag je Zeile

Eingaben werden vor dem Speichern geprüft, Fehler erscheinen direkt am betroffenen Feld.
Getestet wird die geschriebene Datei anschließend noch einmal - nur wenn sie gültiges TOML mit
den erwarteten Werten ist, ersetzt sie die alte Datei.

> **Neustart erforderlich:** Änderungen wirken erst nach einem Neustart des Telefons, da GPIO-Pins,
> SIP-Zugangsdaten und der Webserver nur beim Start eingerichtet werden. Das Telefon startet sich
> deshalb **nicht** von selbst neu. Die Oberfläche weist nach dem Speichern darauf hin, auf der
> Statusseite steht "Konfiguration: Geändert, Neustart nötig", und `/config` bietet eine
> Neustart-Schaltfläche an.

#### Kommentare in der Konfigurationsdatei

Beim Speichern wird die Datei aus dem Schema in `src/config.rs` neu geschrieben. Jede Option
erhält dabei wieder ihren erklärenden Kommentar, damit die Datei weiterhin gut lesbar bleibt.

Eigene Kommentare werden dabei **nicht beibehalten**: Sie gehen beim Speichern verloren.
Wer Kommentare dauerhaft behalten möchte, sollte sie nach dem Speichern erneut einfügen oder
die Kommentare in `src/config.rs` ergänzen.

#### Schnittstellen

| Aufruf | Zweck |
| :--- | :--- |
| `GET /api/state` | Status als JSON |
| `GET /api/config` | Schema und aktuelle Werte als JSON |
| `POST /api/config` | Konfiguration als JSON speichern, antwortet mit `{"ok":true,"changed":[...]}` oder `{"ok":false,"errors":{...}}` |
| `POST /api/action/<name>` | Aktion auslösen |

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

Auf einem Pi muss `cargo test -j 1` verwendet werden, weil der Linker der Speicher-Höhepunkt ist.

### Hardware-Testwerkzeuge

Diese sind nur für die Inbetriebnahme am Telefon gedacht. Jedes öffnet die Pins, die es braucht,
darum immer einzeln ausführen:

```
cargo run --example hardware-connections   # Nummernschalter, Impulskontakt, Hörer live
cargo run --example hardware-dial          # wählt eine Ziffer und gibt sie aus
cargo run --example hardware-audio         # Klingeltöne, Lautsprecher, Hörer, Gesprächsbrücke
cargo run --example gpio-probe             # rohe Pin-Pegel, zur Fehlersuche in der Verkabelung
cargo run --release --example audio-probe -- plug:usb 8000 1   # ALSA-Parameter durchprobieren
```

## Nacht- und Aufwachlicht

Es können über die Konfigurationsoptionen im Bereich `[misc]` sowohl eine Nachtlicht- als auch eine Aufwachlicht-LED konfiguriert werden.
Über die Kurzwahl `enable-night-mode` wird dann das Nachtlicht bis zur konfigurierten Uhrzeit aktiviert.

## Schlafmusik

Über die Kurzwahl `start-sleep-music` wird die Spieluhr aktiviert. Die Spieluhr stoppt, sobald der Hörer abgehoben wird.

## Hinweise zur GPIO-Anbindung

- Die Pins werden über `gpiod` angesprochen, weil die Verkabelung einen Pull-up braucht.
- Die Impulse werden im 1-ms-Takt abgefragt statt über Flankenerkennung, damit die Lösung auch mit
  libgpiod 1.x und 2.x funktioniert.
- Fehlt `libgpiod-dev`, lässt sich das Projekt auf einem Raspberry Pi OS gar nicht bauen. Auf
  anderen Systemen gibt es eine Ersatzimplementierung, mit der die Tests ohne Hardware laufen.