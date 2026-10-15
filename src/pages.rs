//! Static HTML pages of the web interface.
//!
//! The status page and the configuration page are served unchanged from the
//! Python version so the interface behaves identically after the port; only the
//! server that delivers them changed.

pub const PAGE: &str = r##"<!DOCTYPE html>
<html lang="de">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>PiPhone</title>
<style>
  :root { color-scheme: dark; }
  body {
    margin: 0 auto; padding: 1.5rem; max-width: 26rem;
    background: #111; color: #ddd;
    font: 1rem/1.5 system-ui, -apple-system, sans-serif;
  }
  h1 { font-size: 1.1rem; font-weight: 600; letter-spacing: .2em; text-transform: uppercase; color: #888; }
  section { border-top: 1px solid #2a2a2a; }
  .row { display: flex; justify-content: space-between; padding: .5rem 0; border-bottom: 1px solid #2a2a2a; }
  .row span:last-child { color: #fff; text-align: right; }
  .row span.on { color: #7ddc7d; }
  .row span.off { color: #777; }
  .buttons { display: grid; grid-template-columns: 1fr 1fr; gap: .5rem; margin-top: 1.5rem; }
  button {
    padding: .8rem .5rem; border: 1px solid #333; border-radius: .4rem;
    background: #1c1c1c; color: #ddd; font: inherit; cursor: pointer;
  }
  button:hover { background: #262626; }
  button:active { background: #333; }
  button:disabled { opacity: .5; cursor: default; }
  button.on { background: #16281a; border-color: #3d6b45; color: #7ddc7d; }
  button.danger { grid-column: span 2; color: #e88; }
  button .key { display: block; font-size: .7rem; color: #666; }
  button.on .key { color: #4c7d55; }
  nav { margin-bottom: 1rem; font-size: .85rem; }
  nav a { color: #7aa7d0; text-decoration: none; margin-right: 1rem; }
  nav a.active { color: #fff; text-decoration: underline; }
</style>
</head>
<body>
<h1>PiPhone</h1>

<nav>
  <a href="/" class="active">Status</a>
  <a href="/config">Konfiguration</a>
</nav>

<section>
  <div class="row"><span>Laufzeit</span><span id="uptime">–</span></div>
  <div class="row"><span>WLAN</span><span id="connected">–</span></div>
  <div class="row"><span>Telefonie</span><span id="sip">–</span></div>
  <div class="row"><span>Nachtlicht</span><span id="night_light">–</span></div>
  <div class="row"><span>Aufwachmodus</span><span id="wake_up_mode">–</span></div>
  <div class="row"><span>Einschlafmusik</span><span id="sleep_music">–</span></div>
  <div class="row"><span>Nicht stören</span><span id="dnd">–</span></div>
  <div class="row"><span>Nächstes Aufstehen</span><span id="next_wake_up">–</span></div>
  <div class="row"><span>Konfiguration</span><span id="config_changed">–</span></div>
</section>

<div class="buttons">
  <button data-action="night-mode" id="night-mode-button"><span class="key">Nachtmodus</span>Nachtlicht</button>
  <button data-action="sleep-music" id="sleep-music-button"><span class="key">Einschlafmusik</span>Musik</button>
  <button data-action="test-loudspeaker"><span class="key">Test</span>Lautsprecher</button>
  <button data-action="test-earpiece"><span class="key">Test</span>Hörer</button>
  <button data-action="reboot" class="danger"><span class="key">Achtung</span>Neustart</button>
  <button data-action="shutdown" class="danger"><span class="key">Achtung</span>Herunterfahren</button>
</div>

<script>
const formatUptime = s => s == null ? '–'
  : [Math.floor(s / 86400), Math.floor(s / 3600) % 24, Math.floor(s / 60) % 60, s % 60]
      .map((v, i) => i === 0 ? `${v} Tage` : i === 1 ? `${v} Std` : i === 2 ? `${v} Min` : `${v} Sek`).join(' ');

// A date that does not parse used to surface as a literal "Invalid Date" on the
// page, which looks like a bug rather than a missing value - so fall back to the
// dash instead. The API sends full ISO 8601, which is what `new Date` wants;
// note that a bare "HH:MM" is *not* parseable, so a time-only field cannot be
// passed through this.
const formatDate = s => {
  if (!s) return '–';
  const d = new Date(s);
  return isNaN(d) ? '–' : d.toLocaleString('de-DE', {weekday: 'short', hour: '2-digit', minute: '2-digit'});
};
const bool = v => v ? 'Ja' : 'Nein';
const classFor = v => v ? 'on' : 'off';

async function refresh() {
  try {
    const state = await (await fetch('/api/state')).json();

    document.getElementById('uptime').textContent = formatUptime(state.uptime);
    for (const key of ['connected', 'night_light', 'wake_up_mode', 'sleep_music', 'dnd']) {
      const el = document.getElementById(key);
      el.textContent = bool(state[key]);
      el.className = classFor(state[key]);
    }
    document.getElementById('sip').textContent = state.call_active ? 'Gespräch aktiv' : state.sip_registered ? 'Bereit' : 'Nicht verfügbar';
    document.getElementById('next_wake_up').textContent = formatDate(state.next_wake_up);

    const changed = document.getElementById('config_changed');
    changed.textContent = state.config_changed ? 'Geändert, Neustart nötig' : 'Unverändert';
    changed.className = state.config_changed ? 'off' : '';

    document.getElementById('night-mode-button').classList.toggle('on', state.night_light);
    document.getElementById('sleep-music-button').classList.toggle('on', state.sleep_music);
  } catch (e) {
    console.error(e);
  }
}

document.querySelectorAll('button').forEach(button => button.addEventListener('click', async () => {
  const action = button.dataset.action;
  if (action === 'reboot' && !confirm('Telefon wirklich neu starten?')) return;
  if (action === 'shutdown' && !confirm('Telefon wirklich herunterfahren?')) return;

  button.disabled = true;
  try {
    await fetch(`/api/action/${action}`, {method: 'POST'});
  } finally {
    setTimeout(() => button.disabled = false, 1000);
  }
  refresh();
}));

refresh();
setInterval(refresh, 2000);
</script>
</body>
</html>
"##;

pub const CONFIG_PAGE: &str = r##"<!DOCTYPE html>
<html lang="de">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>PiPhone – Konfiguration</title>
<style>
  :root { color-scheme: dark; }
  body {
    margin: 0 auto; padding: 1.5rem; max-width: 40rem;
    background: #111; color: #ddd;
    font: 1rem/1.5 system-ui, -apple-system, sans-serif;
  }
  h1 { font-size: 1.1rem; font-weight: 600; letter-spacing: .2em; text-transform: uppercase; color: #888; }
  h2 { font-size: 1rem; font-weight: 600; color: #ccc; margin: 0 0 .25rem; }
  nav { margin-bottom: 1rem; font-size: .85rem; }
  nav a { color: #7aa7d0; text-decoration: none; margin-right: 1rem; }
  nav a.active { color: #fff; text-decoration: underline; }
  section.config { border-top: 1px solid #2a2a2a; padding: 1rem 0; }
  p.hint { color: #777; font-size: .8rem; margin: 0 0 .75rem; }
  p.hint code { color: #999; }
  label { display: block; font-size: .9rem; margin-bottom: .2rem; }
  label small { color: #666; font-weight: normal; }
  .caption { display: block; font-size: .9rem; margin-bottom: .2rem; }
  input[type=text], input[type=number], input[type=password], input[type=time], input[type="tel"], select, textarea {
    width: 100%; box-sizing: border-box; padding: .5rem;
    background: #1c1c1c; color: #eee; border: 1px solid #333; border-radius: .3rem;
    font: inherit;
  }
  input:focus, select:focus, textarea:focus { outline: 1px solid #7aa7d0; }
  input.invalid { border-color: #c66; }
  .error { color: #e88; font-size: .78rem; margin: .25rem 0 .75rem; }
  .field { margin-bottom: .85rem; }
  .check { display: flex; align-items: center; gap: .5rem; margin-bottom: .75rem; }
  .check input { width: auto; }
  .check label { margin: 0; }
  .items { display: grid; grid-template-columns: repeat(2, 1fr); gap: .5rem .75rem; }
  .item span { display: block; font-size: .75rem; color: #777; }
  table { width: 100%; border-collapse: collapse; margin-bottom: .5rem; }
  th { font-size: .7rem; text-transform: uppercase; letter-spacing: .08em; color: #666; text-align: left; padding: 0 .5rem .25rem 0; font-weight: 600; }
  td { padding: 0 .5rem .5rem 0; vertical-align: top; }
  td.actions { width: 2.5rem; }
  button {
    padding: .6rem .7rem; border: 1px solid #333; border-radius: .4rem;
    background: #1c1c1c; color: #ddd; font: inherit; cursor: pointer;
  }
  button:hover { background: #262626; }
  button:active { background: #333; }
  button:disabled { opacity: .5; cursor: default; }
  button.small { padding: .5rem .6rem; font-size: .85rem; }
  button.danger { color: #e88; }
  button.icon { width: 100%; }
  .rows { margin-bottom: .6rem; }
  .toolbar { display: flex; justify-content: space-between; align-items: center; gap: .5rem; margin-bottom: 1rem; }
  .sticky {
    position: sticky; bottom: 0; background: #111; border-top: 1px solid #2a2a2a;
    padding: .85rem 0; margin-top: 1rem;
  }
  .sticky .buttons { display: flex; gap: .5rem; flex-wrap: wrap; }
  .banner { padding: .8rem 1rem; border-radius: .4rem; margin-bottom: 1rem; font-size: .9rem; }
  .banner.ok { background: #16281a; border: 1px solid #3d6b45; color: #a5e8a5; }
  .banner.warn { background: #2a2416; border: 1px solid #6b5d3d; color: #e8d59b; }
  .banner.err { background: #2a1616; border: 1px solid #6b3d3d; color: #e8a5a5; }
  .banner ul { margin: .4rem 0 0; padding-left: 1.1rem; }
  .spinner { color: #777; font-size: .85rem; }
  .sound-row { display: flex; align-items: center; gap: .5rem; flex-wrap: wrap; }
  .sound-file { color: #999; font-size: .85rem; flex: 1; min-width: 7rem; }
  .sound-file .caller { color: #6a8fa8; margin-left: .5rem; }
  .pool { margin-bottom: 1rem; }
  .pool h3 { font-size: .9rem; font-weight: 600; color: #bbb; margin: 1.2rem 0 .3rem; }
  .pool-files { display: flex; flex-direction: column; gap: .35rem; margin: .4rem 0; }
  .pool-files .sound-row { padding: .1rem 0; }
  label.upload {
    display: inline-flex; align-items: center; padding: .5rem .6rem; font-size: .85rem;
    border: 1px solid #333; border-radius: .4rem; background: #1c1c1c; color: #ddd;
    cursor: pointer;
  }
  label.upload:hover { background: #262626; }
  label.upload input { display: none; }
  .pool-upload { display: flex; align-items: center; gap: .5rem; flex-wrap: wrap; }
  .pool-upload input { flex: 1; min-width: 8rem; width: auto; }
  .status { color: #7ddc7d; font-size: .8rem; min-height: 1.2em; }
  .status.err { color: #e88; }
  .status.busy { color: #777; }
</style>
</head>
<body>
<h1>PiPhone</h1>

<nav>
  <a href="/">Status</a>
  <a href="/config" class="active">Konfiguration</a>
</nav>

<div id="message"></div>

<form id="form">
  <div class="spinner">Lade Konfiguration …</div>
</form>

<div class="sticky">
  <div class="buttons">
    <button id="save" disabled>Speichern</button>
    <button id="reload" class="small">Neu laden</button>
    <button data-action="reboot" class="small danger" style="margin-left:auto">Neustart</button>
  </div>
</div>

<script>
let config = null;

// ---------------------------------------------------------------- Hilfsfunktionen

const el = (tag, attrs = {}, children = []) => {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (key === 'class') node.className = value;
    else if (key === 'text') node.textContent = value;
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else if (value !== null && value !== undefined) node.setAttribute(key, value);
  }
  for (const child of [].concat(children)) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child);
  }
  return node;
};

// Alle Eingabefelder eines Formularabschnitts als Schlüssel/Wert-Paare einsammeln
function collect(root) {
  const data = {};

  for (const section of root.querySelectorAll('[data-section]')) {
    const name = section.dataset.section;
    const values = {};

    for (const field of section.querySelectorAll('[data-field]')) {
      const input = field.querySelector('input, select, textarea');
      if (field.dataset.type === 'bool') {
        values[field.dataset.field] = input.checked;
      } else if (field.dataset.type === 'list') {
        // Entweder ein Feld je Eintrag (Aufstehzeiten je Wochentag) oder eine
        // Zeile je Eintrag im Textfeld (Pfade zur Einschlafmusik).
        const items = field.querySelectorAll('[data-item]');
        values[field.dataset.field] = items.length
          ? [...items].map(item => item.value.trim())
          : input.value.split('\n').map(line => line.trim()).filter(Boolean);
      } else {
        values[field.dataset.field] = input.value;
      }
    }

    for (const row of section.querySelectorAll('[data-row]')) {
      const [key, value] = row.querySelectorAll('input');
      if (key.value.trim() || value.value.trim()) values[key.value.trim()] = value.value.trim();
    }

    data[name] = values;
  }

  return data;
}

// Zu einem Schlüssel `abschnitt.option` das Eingabefeld und seinen Container finden
function fieldOf(path) {
  const separator = path.indexOf('.');
  if (separator < 0) return null;

  const root = document.querySelector(`[data-section="${CSS.escape(path.slice(0, separator))}"]`);
  if (!root) return null;

  const option = path.slice(separator + 1);

  // Feste Option: das Feld mit diesem Schlüssel
  const field = root.querySelector(`[data-field="${CSS.escape(option)}"]`);
  if (field) return {control: field.querySelector('input, select, textarea'), wrapper: field};

  // Eintrag (Kurzwahl/Klingelton): die Zeile, deren Schlüssel übereinstimmt
  for (const row of root.querySelectorAll('[data-row]')) {
    const [key] = row.querySelectorAll('input');
    if (key.value.trim() === option) return {control: key, wrapper: key.closest('td')};
  }

  return null;
}

// Fehler aus der Antwort neben den betroffenen Feldern anzeigen
// Gibt die Fehler zurück, die keinem Feld zugeordnet werden konnten.
function showErrors(errors) {
  for (const node of document.querySelectorAll('.invalid')) node.classList.remove('invalid');
  for (const node of document.querySelectorAll('.error')) node.remove();

  const general = [];

  for (const [path, message] of Object.entries(errors || {})) {
    const found = fieldOf(path);

    if (!found || !found.control) {
      general.push(`${path.replace('.', ': ')} – ${message}`);
      continue;
    }

    // Bei einem Feld aus mehreren Eingaben (Aufstehzeiten je Wochentag) ist das
    // leere Feld das falsche: eine Uhrzeit, die der Browser in ein
    // `type=time` übernommen hat, ist auch eine gültige. Ohne eine davon zu
    // finden - etwa weil die Liste zu kurz ist - werden alle markiert.
    const items = [...found.wrapper.querySelectorAll('[data-item]')];
    const marked = items.filter(item => !item.value);
    (marked.length ? marked : items).forEach(item => item.classList.add('invalid'));
    if (!items.length) found.control.classList.add('invalid');

    found.wrapper.after(el('div', {class: 'error', text: message}));
  }

  return general;
}

const banner = (kind, text, items = []) =>
  el('div', {class: `banner ${kind}`},
     [text, items.length ? el('ul', {}, items.map(i => el('li', {text: i}))) : null].filter(Boolean));

// ---------------------------------------------------------------- Eingabefelder

function optionField(field, value) {
  const id = `f-${field.key}`;
  let input;

  if (field.type === 'bool') {
    input = el('input', {type: 'checkbox', id, 'data-key': field.key});
    input.checked = value === true || value === 'true';
    return el('div', {class: 'check', 'data-field': field.key, 'data-type': field.type}, [
      input,
      el('label', {for: id, text: field.label + (field.required ? ' *' : '')}),
    ]);
  }

  if (field.type === 'list') {
    // Beschriftete Einträge bekommen ein eigenes Feld, unbeschriftete eine
    // Zeile je Eintrag im Textfeld.
    if (field.item_labels && field.item_labels.length) return itemFields(field, value);

    input = el('textarea', {id, rows: 3, 'data-key': field.key});
    input.value = Array.isArray(value) ? value.join('\n') : (value || '');
    return el('div', {class: 'field', 'data-field': field.key, 'data-type': field.type}, [
      el('label', {for: id}, [
        document.createTextNode(field.label + (field.required ? ' *' : '')),
        field.item_hint ? el('small', {text: ` – ${field.item_hint} je Zeile`}) : null,
      ]),
      input,
    ]);
  }

  input = el('input', {
    id, 'data-key': field.key,
    type: field.secret ? 'password' : field.type === 'int' || field.type === 'float' ? 'number' : 'text',
  });
  input.value = value === null || value === undefined ? '' : String(value);
  if (field.minimum !== null && field.minimum !== undefined) input.min = field.minimum;
  if (field.maximum !== null && field.maximum !== undefined) input.max = field.maximum;
  if (field.type === 'int') input.step = 1;
  if (field.type === 'float') input.step = 'any';

  return el('div', {class: 'field', 'data-field': field.key, 'data-type': field.type}, [
    el('label', {for: id, text: field.label + (field.required ? ' *' : '')}),
    input,
  ]);
}

// Eine Liste, deren Einträge im Schema beschriftet sind, bekommt ein eigenes
// Eingabefeld je Eintrag: bei den Aufstehzeiten kann ein Textfeld über die
// Wochentage hinweg nicht mehr sagen, welcher Wert zu welchem Tag gehört.
// Die Felder stehen in der Reihenfolge der Beschriftungen, das ist die
// Reihenfolge der Liste in der Datei (Montag zuerst).
function itemFields(field, value) {
  const values = Array.isArray(value) ? value : [];

  const fields = field.item_labels.map((caption, index) => {
    const input = el('input', {
      id: `f-${field.key}-${index}`, type: field.item_input || 'text', 'data-item': '',
    });
    input.value = values[index] === null || values[index] === undefined ? '' : String(values[index]);

    return el('label', {class: 'item'}, [el('span', {text: caption}), input]);
  });

  return el('div', {class: 'field', 'data-field': field.key, 'data-type': field.type}, [
    el('span', {class: 'caption', text: field.label + (field.required ? ' *' : '')}),
    el('div', {class: 'items'}, fields),
  ]);
}

function entryRow(section, entry) {
  const key = el('input', {type: 'text', placeholder: section.key_label});
  key.value = entry.key;

  const value = el('input', {
    type: 'text', placeholder: section.value_label,
    list: `choices-${section.name}`, autocomplete: 'off',
  });
  value.value = entry.value;

  const row = el('tr', {'data-row': 1}, [
    el('td', {}, [key]),
    el('td', {}, [value]),
    el('td', {class: 'actions'}, [
      el('button', {type: 'button', class: 'small danger', text: '✕', title: 'Entfernen',
                    onclick: () => row.remove()}),
    ]),
  ]);

  return row;
}

function renderSection(section) {
  const node = el('section', {class: 'config', 'data-section': section.name}, [
    el('h2', {text: section.title}),
  ]);

  if (section.kind === 'entries') {
    if (section.help && section.help.length) node.append(el('p', {class: 'hint'}, section.help.map(h => el('div', {text: h}))));

    if (section.choices && section.choices.length) {
      node.append(el('datalist', {id: `choices-${section.name}`}, section.choices.map(c => el('option', {value: c}))));
    }

    const rows = el('tbody', {class: 'rows'});
    for (const entry of section.entries) rows.append(entryRow(section, entry));

    node.append(el('table', {}, [
      el('thead', {}, [el('tr', {}, [
        el('th', {text: section.key_label}),
        el('th', {text: section.value_label}),
        el('th', {}),
      ])]),
      rows,
    ]));

    node.append(el('button', {
      type: 'button', class: 'small', text: `+ ${section.key_label}`,
      onclick: () => rows.append(entryRow(section, {key: '', value: ''})),
    }));

    return node;
  }

  for (const field of section.fields) {
    node.append(optionField(field, field.value));
    if (field.help && field.help.length) {
      node.append(el('p', {class: 'hint'}, field.help.map(h => el('div', {text: h}))));
    }
  }

  return node;
}

// ---------------------------------------------------------------- Klänge

// Die Klänge stehen nicht in der Konfiguration, sondern sind hochgeladene
// Dateien, und der Server schickt zu jeder den Zustand, den die Seite anzeigt:
// feste Klänge mit dem Klarnamen, unter dem sie liegen, und zwei Listen
// hochgeladener Dateien, die ihren Namen behalten.
//
// Der Rumpf der Anfrage ist jeweils die Datei selbst - an /api/sound/<name> für
// einen festen Klang, an /api/sound/<sammlung>/<dateiname> für eine Datei in
// einer Liste.

const soundUrl = key => `/api/sound/${encodeURIComponent(key)}`;
const poolUrl = (pool, name) => `/api/sound/${encodeURIComponent(pool)}/${encodeURIComponent(name)}`;

// Die Endung, unter der eine Auswahl hochgeladen wird. Welches Format sie
// wirklich hat, entscheidet der Server am Inhalt; hier steht nur, was der
// Browser für eine Endung hält.
const extensionOf = file => {
  const match = /\.([a-z0-9]{1,5})$/i.exec(file.name || '');
  return match ? match[1] : 'wav';
};

const formatSize = bytes => bytes < 1024 ? `${bytes} B`
  : bytes < 1024 * 1024 ? `${Math.round(bytes / 1024)} kB`
  : `${(bytes / 1048576).toFixed(1).replace('.', ',')} MB`;

// Was gerade läuft, damit ein zweiter Klang den ersten beendet.
let preview = null;

function say(status, text, kind = 'busy') {
  status.textContent = text;
  status.className = `status ${kind}`;
}

// Die Antwort des Servers lesen. Sie ist JSON - außer der Request war zu groß,
// dann kommt der Text der Fehlerseite und `json()` scheitert.
async function answer(response) {
  return response.json().catch(
    () => ({ok: false, error: `Die Anfrage ist fehlgeschlagen (${response.status})`}));
}

// Ein Klang anhören; derselbe Klang noch einmal anklicken hält ihn an.
function togglePreview(url, button, status) {
  if (preview && preview.url === url) {
    stopPreview();
    return;
  }
  stopPreview();

  const audio = new Audio(url);
  // Der Klang läuft genau einmal; danach ist die Schaltfläche wieder "Vorschau".
  audio.addEventListener('ended', stopPreview);
  audio.addEventListener('error', () => say(status, 'Klang nicht abspielbar.', 'err'));

  preview = {url, audio, button};
  button.textContent = 'Stopp';

  audio.play().catch(e => {
    console.error(e);
    stopPreview();
    say(status, 'Klang nicht abspielbar.', 'err');
  });
}

function stopPreview() {
  if (!preview) return;
  preview.audio.pause();
  preview.button.textContent = 'Vorschau';
  preview = null;
}

// Ein Auswahlfeld für eine Audiodatei, als Beschriftung über einem Knopf.
function uploadButton(text, onFile) {
  return el('label', {class: 'upload'}, [
    el('span', {text}),
    el('input', {
      type: 'file', accept: 'audio/wav,audio/mpeg,audio/*',
      onchange: event => {
        const file = event.target.files[0];
        // Zurückgesetzt, damit dieselbe Datei noch einmal gewählt werden kann -
        // sonst ändert der Browser nichts und es passiert nichts.
        event.target.value = '';
        if (file) onFile(file);
      },
    }),
  ]);
}

// ---------------------------------------------------------------- feste Klänge

// Zustand einer Zeile: Dateiname und Größe, Vorschau nur wenn es etwas zu
// hören gibt. Die Angaben kommen vom Server, nicht aus dem Dateinamen der
// Auswahl - der kann heißen wie er will.
function paintSound(row, sound) {
  row.querySelector('.sound-file').textContent = sound.present
    ? `${sound.file} (${formatSize(sound.size)})`
    : 'keine Datei';
  row.querySelector('.sound-preview').hidden = !sound.present;
  row.querySelector('label.upload span').textContent = sound.present ? 'Ersetzen' : 'Hochladen';
}

function soundRow(sound) {
  const status = el('div', {class: 'status'});

  const previewButton = el('button', {
    type: 'button', class: 'small sound-preview', text: 'Vorschau',
    onclick: () => togglePreview(soundUrl(sound.key), previewButton, status),
  });

  const row = el('div', {class: 'field', 'data-sound': sound.key}, [
    el('span', {class: 'caption', text: sound.label}),
    el('p', {class: 'hint'}, (sound.help || []).map(h => el('div', {text: h}))),
    el('div', {class: 'sound-row'}, [
      el('span', {class: 'sound-file'}),
      previewButton,
      uploadButton(sound.present ? 'Ersetzen' : 'Hochladen',
        file => uploadSound(sound.key, file, row)),
    ]),
    status,
  ]);

  paintSound(row, sound);
  return row;
}

// Nur diese eine Zeile wird neu aufgebaut, damit die übrigen Eingaben des
// Formulars unangetastet bleiben - ein Neuaufbau des ganzen Formulars würde sie
// verwerfen.
async function uploadSound(key, file, row) {
  const status = row.querySelector('.status');
  say(status, `${file.name || 'Datei'} wird hochgeladen …`);

  try {
    const result = await answer(await fetch(soundUrl(key), {method: 'POST', body: file}));
    if (!result.ok) throw new Error(result.error || 'Hochladen fehlgeschlagen');

    if (preview && preview.url === soundUrl(key)) stopPreview();
    paintSound(row, result.sound);
    say(status, `${result.sound.file} gespeichert.`);
  } catch (e) {
    console.error(e);
    say(status, e.message, 'err');
  }
}

// ---------------------------------------------------------------- Listen

// Eine Liste hochgeladener Dateien: Klingeltöne oder Einschlafmusik. Bei den
// Klingeltönen entscheidet die Rufnummer über den Namen der Datei, also steht sie
// im Feld daneben und ist Pflicht: ohne sie hätte die Datei keinen Namen, unter
// dem das Telefon sie wiederfindet. Bei der Einschlafmusik behält die Datei den
// Namen, den sie hat.
function poolSection(pool) {
  const list = el('div', {class: 'pool-files'});
  const status = el('div', {class: 'status', 'data-status': ''});

  // Die Dateien aus der Antwort werden hier weitergeführt: nach einem Upload
  // oder einem Löschen wird nur dieser Abschnitt neu aufgebaut, damit
  // ungespeicherte Eingaben im Formular erhalten bleiben.
  let files = (pool.files || []).slice();

  const repaint = () => {
    files.sort((one, other) => one.name.localeCompare(other.name));

    if (!files.length) {
      list.replaceChildren(el('p', {class: 'hint'}, [el('div', {text: 'Noch nichts hochgeladen.'})]));
      return;
    }

    list.replaceChildren(...files.map(file => poolFileRow(pool.key, file, files, repaint, status)));
  };

  const number = pool.numbered
    ? el('input', {type: 'tel', required: '', placeholder: 'Rufnummer', 'aria-label': 'Rufnummer'})
    : null;

  const upload = uploadButton('Hochladen',
    file => uploadPoolFile(pool, file, number ? number.value : '', files, repaint, status));

  const node = el('div', {class: 'pool', 'data-pool': pool.key}, [
    el('p', {class: 'hint'}, (pool.help || []).map(h => el('div', {text: h}))),
    list,
    status,
    number === null ? upload : el('div', {class: 'pool-upload'}, [number, upload]),
  ]);

  repaint();
  return node;
}

function poolFileRow(pool, file, files, repaint, status) {
  const own = el('div', {class: 'status', 'data-row-status': ''});

  const previewButton = el('button', {
    type: 'button', class: 'small', text: 'Vorschau',
    onclick: () => togglePreview(poolUrl(pool, file.name), previewButton, own),
  });

  return el('div', {class: 'sound-row', 'data-file': file.name}, [
    el('span', {class: 'sound-file'}, [
      el('span', {text: `${file.name} (${formatSize(file.size)})`}),
      // Jeder Klingelton in der Liste gehört genau einer Rufnummer, und sein
      // Name sagt, welcher - das ist der ganze Unterschied zwischen den Dateien.
      file.caller ? el('span', {class: 'caller', text: `nur für ${file.caller}`}) : null,
    ]),
    previewButton,
    el('button', {
      type: 'button', class: 'small danger', text: 'Löschen', title: 'Datei entfernen',
      // Die Meldung landet an der Liste, nicht an der Zeile: die Zeile ist
      // danach weg, und die Meldung soll trotzdem zu sehen sein.
      onclick: () => deletePoolFile(pool, file.name, files, repaint, status),
    }),
    own,
  ]);
}

async function uploadPoolFile(pool, file, number, files, repaint, status) {
  let name = file.name;

  if (pool.numbered) {
    const digits = String(number || '').replace(/\D/g, '');
    if (!digits) {
      say(status, 'Ohne Rufnummer gibt es keinen Namen, unter dem die Datei wiedergefunden wird.', 'err');
      return;
    }
    name = `ringtone-${digits}.${extensionOf(file)}`;
  }

  say(status, `${file.name || 'Datei'} wird hochgeladen …`);

  try {
    const result = await answer(await fetch(poolUrl(pool.key, name), {method: 'POST', body: file}));
    if (!result.ok) throw new Error(result.error || 'Hochladen fehlgeschlagen');

    // Der Server nennt den Namen, unter dem die Datei liegt: Leerzeichen und
    // andere unbrauchbare Zeichen hat er dabei ersetzt.
    const stored = result.file;
    const known = files.findIndex(entry => entry.name === stored.name);
    if (known >= 0) files[known] = stored;
    else files.push(stored);

    repaint();
    say(status, `${stored.name} gespeichert.`);
  } catch (e) {
    console.error(e);
    say(status, e.message, 'err');
  }
}

async function deletePoolFile(pool, name, files, repaint, status) {
  if (!confirm(`"${name}" wirklich löschen?`)) return;

  say(status, `${name} wird gelöscht …`);

  try {
    const result = await answer(await fetch(poolUrl(pool, name), {method: 'DELETE'}));
    if (!result.ok) throw new Error(result.error || 'Löschen fehlgeschlagen');

    const known = files.findIndex(entry => entry.name === name);
    if (known >= 0) files.splice(known, 1);

    repaint();
    say(status, `${name} gelöscht.`);
  } catch (e) {
    console.error(e);
    say(status, e.message, 'err');
  }
}

function renderSounds(sounds) {
  const node = el('section', {class: 'config sounds'}, [
    el('h2', {text: 'Klänge'}),
    el('p', {class: 'hint'}, [el('div', {
      text: 'Diese Klänge spielt das Telefon. Sie werden hier hochgeladen und gelten ' +
            'sofort - ohne Neustart. Ein fester Klang ohne Datei bleibt stumm; die ' +
            'Listen sind leer, bis etwas hochgeladen wird.',
    })]),
  ]);

  for (const sound of (sounds && sounds.fixed) || []) node.append(soundRow(sound));
  for (const pool of (sounds && sounds.pools) || []) {
    node.append(el('h3', {text: pool.label}));
    node.append(poolSection(pool));
  }

  return node;
}

// ---------------------------------------------------------------- Laden und Speichern

const savedBanner = (changed = [], dropped = []) => {
  const items = ['Gespeichert in der Konfigurationsdatei.'];
  if (changed.length) items.push('Geändert wurden: ' + changed.join(', '));
  // Beim Speichern fällt weg, was das Schema nicht kennt. Das gehört in die
  // Meldung, sonst verschwindet der Eintrag ersatzlos.
  if (dropped.length) items.push('Unbekannte Einträge wurden entfernt: ' + dropped.join(', '));
  return banner('warn', 'Das Telefon muss neu gestartet werden, damit die Änderungen wirksam werden.', items);
};

function render(config) {
  const form = document.getElementById('form');
  // Eine laufende Vorschau gehört zu einer Zeile, die gleich ersetzt wird.
  stopPreview();

  const sections = config.sections.map(renderSection);
  // Die Klänge gehören vor das Nachtlicht, wo früher der gleichnamige Abschnitt
  // der Konfiguration stand: sie sind Dateien und keine Optionen, und sie lassen
  // sich deshalb nur schwer an anderer Stelle unterbringen.
  const before = sections.findIndex(section => section.dataset.section === 'misc');
  sections.splice(before < 0 ? sections.length : before, 0, renderSounds(config.sounds));

  form.replaceChildren(...sections);
  document.getElementById('save').disabled = false;

  // Ladehinweis entfernen und nur zeigen, wenn die Konfiguration vom laufenden
  // Betrieb abweicht - nach dem Speichern wird die Meldung gezielt gesetzt.
  if (config.restart_required) {
    document.getElementById('message').replaceChildren(
      banner('warn', 'Seit dem letzten Start wurde die Konfiguration geändert. ' +
                      'Sie wird erst nach einem Neustart des Telefons verwendet.')
    );
  } else {
    document.getElementById('message').replaceChildren();
  }
}

async function load() {
  const message = document.getElementById('message');
  document.getElementById('save').disabled = true;
  message.replaceChildren(el('div', {class: 'banner warn', text: 'Lade Konfiguration …'}));

  try {
    config = await (await fetch('/api/config')).json();
    render(config);
  } catch (e) {
    console.error(e);
    message.replaceChildren(banner('err', 'Konfiguration konnte nicht geladen werden.'));
  }
}

document.getElementById('save').addEventListener('click', async () => {
  const button = document.getElementById('save');
  const message = document.getElementById('message');
  button.disabled = true;
  showErrors({});

  try {
    const response = await (await fetch('/api/config', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify(collect(document.getElementById('form'))),
    })).json();

    if (response.ok) {
      // Formular mit dem gespeicherten Stand neu aufbauen, damit fehlende
      // Standardwerte und Prüfungen sichtbar werden
      config = await (await fetch('/api/config')).json();
      render(config);
      message.replaceChildren(savedBanner(response.changed || [], response.dropped || []));
      return;
    }

    const general = showErrors(response.errors);
    message.replaceChildren(banner('err', 'Nicht gespeichert, bitte Eingaben prüfen.', general));
  } catch (e) {
    console.error(e);
    message.replaceChildren(banner('err', 'Konfiguration konnte nicht gespeichert werden.'));
  } finally {
    button.disabled = false;
  }
});

document.getElementById('reload').addEventListener('click', load);

document.querySelectorAll('button[data-action]').forEach(button => button.addEventListener('click', async () => {
  if (!confirm('Telefon wirklich neu starten? Die Konfiguration wird erst danach wirksam.')) return;
  button.disabled = true;
  await fetch(`/api/action/${button.dataset.action}`, {method: 'POST'});
}));

load();
</script>
</body>
</html>
"##;
