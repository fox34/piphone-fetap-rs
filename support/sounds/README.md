Dateien werden am effizientesten als WAV abgespielt.
MP3 ist auch möglich, hat aber einen geringen Overhead.

Nötiges Format, falls WAV:
- MAX98357: Signed 32-bit LE, 48 kHz, Stereo
- USB: Signed 16-bit LE, 44,1 kHz, Stereo

Diese Dateien sind die Quelle, nicht der Ort, an dem sie abgespielt werden:
Das Telefon liest sie aus `/usr/local/share/piphone/sounds`, wohin sie über
`/config` hochgeladen werden. Dort gelten die Namen des Abschnitts „Klänge" in
der README:

| Datei hier | Datei im Telefon |
| --- | --- |
| `boot-loud.wav` | `boot.wav` |
| `reboot-loud.wav` | `reboot.wav` |
| `shutdown-loud.wav` | `shutdown.wav` |
| `test-loud.wav` | `test-loud.wav` |
| `test-earpiece.wav` | `test-earpiece.wav` |
| `confirm.wav` | `confirm.wav` |
| `ring-0X.wav`/`.mp3` | `ringtones/ringtone-<Rufnummer>.<Endung>`; die Rufnummer wird beim Hochladen im Webinterface eingegeben, nicht aus dem Dateinamen gelesen |
| `sleep-*.mp3` | `sleep/<Titel>.mp3`; alle Titel zusammen bilden die Playlist |
