// Testet die JavaScript-Logik der Konfigurationsseite (src/pages.rs, CONFIG_PAGE)
// mit einem echten DOM. Läuft mit:  node tests/test-webpage.js
//
// Benötigt jsdom (npm install jsdom). Die Seite und die Fixture werden von
// `cargo run --features harness --example render-page` erzeugt, damit die
// Escape-Sequenzen in den Rust-Zeichenketten korrekt aufgelöst werden.
const path = require('path');
const {execFileSync} = require('child_process');
const {JSDOM} = require('jsdom');

const repo = path.resolve(__dirname, '..');

// Seite und Fixture kommen normalerweise aus `cargo`, also aus einem Build auf
// einem Raspberry Pi: auf einem Entwicklerrechner ist das nicht erlaubt
// (AGENTS.md). PIPHONE_RENDER zeigt stattdessen auf ein Verzeichnis, in dem
// beide Ausgaben liegen, etwa so erzeugt:
//   cargo run --features harness --example render-page -- CONFIG_PAGE > $DIR/CONFIG_PAGE
//   cargo run --features harness --example render-page -- fixture      > $DIR/fixture
function render(what) {
  // `--features harness`: die Beispiele liegen hinter diesem Feature, damit
  // `cargo test` sie nicht mitbaut. Der Renderer braucht es also auch.
  return execFileSync('cargo', ['run', '--quiet', '--features', 'harness',
                                '--example', 'render-page', '--', what],
                      {cwd: repo, encoding: 'utf8'});
}

function rendered(what) {
  const directory = process.env.PIPHONE_RENDER;
  if (!directory) return render(what);
  return require('fs').readFileSync(path.join(directory, what), 'utf8');
}

let ok = 0, failed = 0;
function check(label, condition, detail = '') {
  if (condition) { ok++; console.log(`  ok  ${label}`); }
  else { failed++; console.log(`FAIL  ${label} ${detail}`); }
}

const html = rendered('CONFIG_PAGE');
const configPayload = JSON.parse(rendered('fixture'));

// Die Fixture kommt aus einem Verzeichnis, in dem es die Klänge nicht gibt -
// die Seite muss auch genau diesen Zustand darstellen können. Für den Test mit
// hochgeladenen Klängen wird jeder feste Klang als vorhanden gemeldet, bis auf
// `boot`, damit beide Zustände im selben Lauf vorkommen; die beiden Listen sind
// mit Dateien gefüllt, weil eine leere Liste nichts zu zeigen hat außer dem
// Hinweis, dass noch nichts da ist.
const uploaded = key => key !== 'boot';
configPayload.sounds.fixed = configPayload.sounds.fixed.map(sound => uploaded(sound.key)
  ? {...sound, present: true, file: `${sound.key}.wav`, size: 2048, content_type: 'audio/wav'}
  : sound);

const collection = {
  ringtones: [
    {name: 'Ring-01.wav', size: 2048, content_type: 'audio/wav', caller: null},
    {name: 'ringtone-08912345.mp3', size: 4096, content_type: 'audio/mpeg', caller: '08912345'},
  ],
  sleep: [
    {name: 'Brahms_Lullaby.mp3', size: 8388608, content_type: 'audio/mpeg', caller: null},
  ],
};
configPayload.sounds.pools = configPayload.sounds.pools.map(
  pool => ({...pool, files: collection[pool.key] || []}));

const dom = new JSDOM(html, {runScripts: 'outside-only', url: 'http://localhost/config'});
const {window} = dom;

// fetch stubben, damit load() die bereitgestellte Konfiguration verwendet
const requests = [];
window.fetch = async (url, options) => {
  requests.push({url, options});
  return {json: async () => JSON.parse(JSON.stringify(configPayload))};
};
window.confirm = () => true;

// jsdom kennt CSS.escape nicht, jeder echte Browser schon (seit 2016). Die Seite
// benutzt es, um Abschnitts- und Feldnamen in Attribut-Selektoren zu setzen, und
// fieldOf() liefe ohne den Namespace in einen ReferenceError, aus dem kein Test
// mehr etwas sieht. Der Testlauf ergänzt ihn also, statt die Seite abzuwässern.
window.CSS = window.CSS || {};
window.CSS.escape = window.CSS.escape || (value => String(value).replace(/[^a-zA-Z0-9_-]/g, ch => '\\' + ch));

window.eval(window.document.querySelector('script').textContent);

setTimeout(async () => {
  const doc = window.document;

  console.log('Formular aufbauen');
  const sections = [...doc.querySelectorAll('[data-section]')].map(s => s.dataset.section);
  check('alle Abschnitte gerendert',
    JSON.stringify(sections) === JSON.stringify(['network', 'sip', 'pins', 'numbers', 'misc', 'web']),
    JSON.stringify(sections));
  check('Speichern erst nach dem Laden aktiv', doc.getElementById('save').disabled === false);
  check('unveränderte Konfiguration zeigt keinen Hinweis', doc.querySelector('#message').children.length === 0,
    doc.querySelector('#message').textContent);

  // Konfiguration, die seit dem letzten Start geändert wurde, weist auf den Neustart hin
  window.fetch = async () => ({json: async () => ({...configPayload, restart_required: true})});
  window.eval('load()');
  await new Promise(resolve => setTimeout(resolve, 50));
  check('Hinweis auf Neustart sichtbar', doc.querySelector('.banner.warn') !== null);
  check('Hinweis nennt Neustart',
    doc.querySelector('.banner.warn') !== null &&
    doc.querySelector('.banner.warn').textContent.includes('Neustart'),
    doc.querySelector('#message').textContent);

  window.fetch = async (url, options) => {
    requests.push({url, options});
    return {json: async () => JSON.parse(JSON.stringify(configPayload))};
  };
  window.eval('load()');
  await new Promise(resolve => setTimeout(resolve, 50));

  console.log('Werte aus der Konfiguration übernommen');
  const value = selector => doc.querySelector(`[data-section="sip"] [data-field="${selector}"] input`).value;
  check('Textfeld gefüllt', value('host') === '10.0.0.1', value('host'));
  check('Passwortfeld gefüllt', value('pass') === 'test');
  check('Passwortfeld ist verdeckt',
    doc.querySelector('[data-section="sip"] [data-field="pass"] input').type === 'password');
  check('Zahlenfeld gefüllt', value('dial_timeout') === '60');
  check('Checkbox gesetzt',
    doc.querySelector('[data-section="sip"] [data-field="whitelist_active"] input').checked === false);

  const wakeField = '[data-section="misc"] [data-field="wake_up_times"]';
  const wakePickers = [...doc.querySelectorAll(`${wakeField} [data-item]`)];
  check('Aufstehzeiten als Uhrzeitfelder', wakePickers.length === 7 && wakePickers.every(i => i.type === 'time'),
        wakePickers.map(i => i.value).join(','));
  check('Wochentage als Beschriftung',
        [...doc.querySelectorAll(`${wakeField} .item span`)].map(s => s.textContent)
          .join(',') === 'Montag,Dienstag,Mittwoch,Donnerstag,Freitag,Samstag,Sonntag');
  check('Aufstehzeiten in Wochentagsreihenfolge gefüllt',
        wakePickers.map(i => i.value).join(',') === '06:30,06:30,06:30,06:30,06:30,07:30,07:30',
        wakePickers.map(i => i.value).join(','));
  check('kein Textfeld mehr für die Aufstehzeiten',
        doc.querySelector(`${wakeField} textarea`) === null);
  check('keine Pfadeingabe mehr für die Klänge',
    doc.querySelector('[data-field="sleep_music"]') === null &&
    doc.querySelector('[data-section="sounds"]') === null);

  console.log('Kurzwahlen');
  const rows = selector => doc.querySelectorAll(`[data-section="${selector}"] [data-row]`);
  const inputs = row => [...row.querySelectorAll('input')].map(i => i.value);
  const numbers = [...rows('numbers')].map(inputs);
  check('alle Kurzwahlen als Zeilen', numbers.length === 9, JSON.stringify(numbers));
  check('erste Kurzwahl gefüllt', numbers[0][0] === '01' && numbers[0][1] === '01234567');
  check('Kurzbefehl als Ziel',
    numbers.some(n => n[1] === 'enable-night-mode') && numbers.some(n => n[1] === 'shutdown'));
  check('kein eigener Abschnitt für die Klingeltöne',
    doc.querySelector('[data-section="ringtones"]') === null);
  check('Vorschläge für Kurzbefehle vorhanden',
    doc.querySelectorAll('[data-section="numbers"] datalist option').length === 6);

  console.log('Klänge');
  const order = [...doc.querySelectorAll('#form > section')]
    .map(section => section.classList.contains('sounds') ? 'sounds' : section.dataset.section);
  check('Klänge stehen vor dem Nachtlicht',
    JSON.stringify(order) ===
      JSON.stringify(['network', 'sip', 'pins', 'numbers', 'sounds', 'misc', 'web']),
    JSON.stringify(order));

  const sound = key => doc.querySelector(`[data-sound="${key}"]`);
  check('jeder Klang hat eine Zeile',
    doc.querySelectorAll('[data-sound]').length === configPayload.sounds.fixed.length);
  check('hochgeladener Klang zeigt seinen Dateinamen',
    sound('ringtone').querySelector('.sound-file').textContent === 'ringtone.wav (2 kB)',
    sound('ringtone').querySelector('.sound-file').textContent);
  check('hochgeladener Klang lässt sich anhören',
    sound('ringtone').querySelector('.sound-preview').hidden === false);
  check('Klang ohne Datei hat keine Vorschau',
    sound('boot').querySelector('.sound-preview').hidden === true);
  check('Klang ohne Datei lädt hoch statt zu ersetzen',
    sound('boot').querySelector('label.upload span').textContent === 'Hochladen' &&
    sound('ringtone').querySelector('label.upload span').textContent === 'Ersetzen');
  check('jede Zeile hat eine Dateiauswahl',
    [...doc.querySelectorAll('[data-sound]')].every(row => row.querySelector('input[type=file]')));

  console.log('Klingeltöne und Einschlafmusik');
  const pool = key => doc.querySelector(`[data-pool="${key}"]`);
  const poolOrder = [...doc.querySelectorAll('[data-pool]')].map(node => node.dataset.pool);
  check('beide Listen stehen unter den Klängen',
    JSON.stringify(poolOrder) === JSON.stringify(['ringtones', 'sleep']), JSON.stringify(poolOrder));
  check('jede Liste zeigt ihren Namen',
    [...doc.querySelectorAll('[data-pool]')].every(node => {
      const known = configPayload.sounds.pools.find(p => p.key === node.dataset.pool);
      return node.previousElementSibling && node.previousElementSibling.textContent === known.label;
    }),
    configPayload.sounds.pools.map(p => p.label).join(','));
  check('Klingeltöne als Dateien',
    pool('ringtones').querySelectorAll('[data-file]').length === 2);
  check('Klingelton mit Rufnummer im Namen zeigt die Nummer',
    pool('ringtones').querySelector('[data-file="ringtone-08912345.mp3"] .caller')
      .textContent.includes('08912345'),
    pool('ringtones').querySelector('[data-file="ringtone-08912345.mp3"]').textContent);
  check('Klingelton ohne Rufnummer wird keiner zugeordnet',
    pool('ringtones').querySelector('[data-file="Ring-01.wav"] .caller') === null);
  check('Einschlafmusik als Dateien',
    pool('sleep').querySelectorAll('[data-file]').length === 1);
  check('jede Datei hat Vorschau, Löschen und eine Dateiauswahl für die Liste',
    [...doc.querySelectorAll('[data-pool] [data-file]')].every(row =>
      row.querySelector('button') && row.querySelector('button.danger')) &&
    [...doc.querySelectorAll('[data-pool]')].every(node => node.querySelector('input[type=file]')));

  // Vorschau: der Klang kommt als Datei vom Server, nicht aus der Konfiguration.
  const played = [];
  window.Audio = class {
    constructor(src) { this.src = src; }
    play() { played.push(this.src); return Promise.resolve(); }
    pause() {}
    addEventListener() {}
  };
  const previewOf = row => row.querySelector('button:not(.danger)');
  previewOf(sound('ringtone')).click();
  await new Promise(resolve => setTimeout(resolve, 10));
  check('Vorschau lädt die Klangdatei',
    played.length === 1 && played[0] === '/api/sound/ringtone', JSON.stringify(played));
  check('Schaltfläche wird zum Stopp',
    sound('ringtone').querySelector('.sound-preview').textContent === 'Stopp');
  previewOf(sound('ringtone')).click();
  check('Schaltfläche wird wieder zur Vorschau',
    sound('ringtone').querySelector('.sound-preview').textContent === 'Vorschau');

  previewOf(pool('ringtones').querySelector('[data-file="Ring-01.wav"]')).click();
  await new Promise(resolve => setTimeout(resolve, 10));
  check('Vorschau einer hochgeladenen Datei lädt ihren Namen',
    played.length === 2 && played[1] === '/api/sound/ringtones/Ring-01.wav', JSON.stringify(played));
  previewOf(pool('ringtones').querySelector('[data-file="Ring-01.wav"]')).click();

  // Hochladen: die Datei ist der Rumpf der Anfrage, und bei den Listen steckt
  // der Name, den die Datei behalten soll, in der Adresse.
  const choose = (input, file) => {
    Object.defineProperty(input, 'files', {value: [file], configurable: true});
    input.dispatchEvent(new window.Event('change'));
  };
  const soundFile = (name, size = 44) =>
    new window.File([new Uint8Array(size)], name, {type: 'audio/wav'});

  const uploads = [];
  window.fetch = async (url, options) => {
    if (url.startsWith('/api/sound/')) {
      uploads.push({url, options});
      const sound = configPayload.sounds.fixed.find(s => s.key === 'boot');
      return {status: 200, json: async () => ({
        ok: true,
        sound: {...sound, present: true, file: 'boot.wav', size: 44, content_type: 'audio/wav'},
      })};
    }
    return {json: async () => JSON.parse(JSON.stringify(configPayload))};
  };
  choose(sound('boot').querySelector('input[type=file]'), soundFile('start.wav'));
  await new Promise(resolve => setTimeout(resolve, 50));
  check('Upload an /api/sound/boot gesendet',
    uploads.length === 1 && uploads[0].url === '/api/sound/boot', JSON.stringify(uploads.map(u => u.url)));
  check('Datei ist der Rumpf des Uploads',
    uploads.length === 1 && uploads[0].options.method === 'POST' && uploads[0].options.body.name === 'start.wav');
  check('Zeile nach dem Upload aktualisiert',
    sound('boot').querySelector('.sound-file').textContent === 'boot.wav (44 B)',
    sound('boot').querySelector('.sound-file').textContent);
  check('Vorschau erscheint nach dem Upload',
    sound('boot').querySelector('.sound-preview').hidden === false);

  // Ein abgelehnter Upload sagt, woran es lag, und lässt den alten Klang stehen.
  window.fetch = async () => ({status: 400, json: async () => ({
    ok: false, error: 'Datei ist weder WAV noch MP3. Es werden nur diese beiden Formate abgespielt.',
  })});
  choose(sound('ringtone').querySelector('input[type=file]'), soundFile('kaputt.txt', 3));
  await new Promise(resolve => setTimeout(resolve, 50));
  check('Fehler des Uploads steht an der Zeile',
    sound('ringtone').querySelector('.status').className.includes('err') &&
    sound('ringtone').querySelector('.status').textContent.includes('WAV'),
    sound('ringtone').querySelector('.status').textContent);
  check('alter Klang nach fehlgeschlagenem Upload erhalten',
    sound('ringtone').querySelector('.sound-file').textContent.includes('ringtone.wav'));

  // Hochladen in eine Liste: der Name der Datei ist Teil der Adresse.
  const added = [];
  window.fetch = async (url, options) => {
    if (url.startsWith('/api/sound/sleep/')) {
      added.push({url, options});
      return {status: 200, json: async () => ({
        ok: true,
        file: {name: 'Gute-Nacht.mp3', size: 4096, content_type: 'audio/mpeg', caller: null},
      })};
    }
    return {json: async () => JSON.parse(JSON.stringify(configPayload))};
  };
  choose(pool('sleep').querySelector('input[type=file]'), soundFile('Gute Nacht.mp3', 4096));
  await new Promise(resolve => setTimeout(resolve, 50));
  check('Upload in die Liste trägt den Dateinamen',
    added.length === 1 && added[0].url === '/api/sound/sleep/Gute%20Nacht.mp3',
    JSON.stringify(added.map(a => a.url)));
  check('hochgeladener Titel steht in der Liste',
    pool('sleep').querySelectorAll('[data-file]').length === 2 &&
    pool('sleep').querySelector('[data-file="Gute-Nacht.mp3"]') !== null,
    [...pool('sleep').querySelectorAll('[data-file]')].map(n => n.dataset.file).join(','));
  check('Name, unter dem die Datei liegt, wird genannt',
    pool('sleep').querySelector('[data-status]').textContent.includes('Gute-Nacht.mp3'),
    pool('sleep').querySelector('[data-status]').textContent);

  // Löschen: die Datei verschwindet aus der Liste.
  const deleted = [];
  window.fetch = async (url, options) => {
    deleted.push({url, options});
    return {status: 200, json: async () => ({ok: true})};
  };
  pool('ringtones').querySelector('[data-file="Ring-01.wav"] button.danger').click();
  await new Promise(resolve => setTimeout(resolve, 50));
  check('Löschen fragt nach dem genauen Dateinamen',
    deleted.length === 1 && deleted[0].options.method === 'DELETE' &&
    deleted[0].url === '/api/sound/ringtones/Ring-01.wav',
    JSON.stringify(deleted.map(d => [d.url, d.options.method])));
  check('gelöschte Datei steht nicht mehr in der Liste',
    pool('ringtones').querySelectorAll('[data-file]').length === 1 &&
    pool('ringtones').querySelector('[data-file="Ring-01.wav"]') === null,
    [...pool('ringtones').querySelectorAll('[data-file]')].map(n => n.dataset.file).join(','));
  check('Upload löst keinen Neustart aus',
    requests.filter(r => r.url && r.url.includes('/api/action/')).length === 0);

  window.fetch = async (url, options) => {
    requests.push({url, options});
    return {json: async () => JSON.parse(JSON.stringify(configPayload))};
  };

  console.log('Nummern hinzufügen und entfernen');
  const addNumber = [...doc.querySelectorAll('[data-section="numbers"] button')]
    .find(b => b.textContent.includes('Kurzwahl'));
  check('Hinzufügen-Schaltfläche vorhanden', addNumber !== undefined);
  addNumber.click();
  check('neue Zeile vorhanden', rows('numbers').length === 10, rows('numbers').length);

  const newRow = rows('numbers')[9];
  newRow.querySelectorAll('input')[0].value = '42';
  newRow.querySelectorAll('input')[1].value = '01299999';
  check('hinzugefügte Kurzwahl erkannt',
    rows('numbers')[9].querySelectorAll('input')[0].value === '42');

  // Entfernen
  newRow.querySelector('button').click();
  check('Zeile wieder entfernt', rows('numbers').length === 9, rows('numbers').length);

  // Leere Zeile darf nicht mitgesendet werden
  addNumber.click();
  check('leere Zeile vorhanden', rows('numbers').length === 10);

  console.log('Eingaben sammeln');
  doc.querySelector('[data-section="sip"] [data-field="host"] input').value = 'fritz.box';
  doc.querySelector('[data-section="sip"] [data-field="dial_timeout"] input').value = '45';
  doc.querySelector('[data-section="sip"] [data-field="whitelist_active"] input').checked = true;
  // Ein Tag je Feld: Montag früher, Samstag später als in der Konfiguration
  '05:00,05:00,05:00,05:00,05:00,06:00,06:00'.split(',').forEach((time, index) => {
    doc.querySelectorAll(`${wakeField} [data-item]`)[index].value = time;
  });
  const emptyRow = rows('numbers')[9];
  emptyRow.querySelectorAll('input')[0].value = '';
  emptyRow.querySelectorAll('input')[1].value = '';

  doc.getElementById('save').click();
  await new Promise(resolve => setTimeout(resolve, 50));

  const post = requests.find(r => r.options && r.options.method === 'POST');
  check('POST an /api/config gesendet', post !== undefined && post.url === '/api/config');
  const sent = JSON.parse(post.options.body);

  check('geänderter SIP-Server gesendet', sent.sip.host === 'fritz.box');
  check('Zahlenwert als Text gesendet', sent.sip.dial_timeout === '45');
  check('Checkbox als Boolean gesendet', sent.sip.whitelist_active === true);
  check('Liste als Array gesendet', Array.isArray(sent.misc.wake_up_times) && sent.misc.wake_up_times.length === 7);
  check('Aufstehzeiten in Wochentagsreihenfolge gesendet',
    sent.misc.wake_up_times.join(',') === '05:00,05:00,05:00,05:00,05:00,06:00,06:00',
    JSON.stringify(sent.misc.wake_up_times));
  check('keine Klänge in der Konfiguration gesendet', !('sounds' in sent), Object.keys(sent).join(','));
  check('keine Klingeltöne in der Konfiguration gesendet', !('ringtones' in sent), Object.keys(sent).join(','));
  check('Kurzwahlen als Objekte', sent.numbers['01'] === '01234567');
  check('Kurzbefehl mitgesendet', sent.numbers['11'] === 'enable-night-mode');
  check('leere Zeile nicht mitgesendet', !('' in sent.numbers) && !Object.values(sent.numbers).includes(''));
  check('Pflichtfelder mitgesendet', sent.pins.nsi === '23' && sent.pins.gabel === '15');

  console.log('Fehleranzeige');
  // Kurzwahl auf einen ungültigen Wert setzen, damit der Fehler einer Zeile zugeordnet werden kann
  rows('numbers')[0].querySelectorAll('input')[0].value = '1234567';

  // Mittwoch leer: das ist der Grund, aus dem der Server die Woche ablehnt
  const pickers = [...doc.querySelectorAll(`${wakeField} [data-item]`)];
  pickers[2].value = '';

  const response = {
    ok: false,
    errors: {
      'pins.nsi': 'darf nicht größer als 27 sein',
      'sip.host': 'Pflichtangabe',
      'numbers.1234567': 'Kurzwahl muss aus 5 Ziffern bestehen.',
      'misc.wake_up_times': 'muss genau 7 Einträge enthalten',
    },
  };
  window.fetch = async (url, options) => ({json: async () => response});
  doc.getElementById('save').click();
  await new Promise(resolve => setTimeout(resolve, 50));

  const errors = [...doc.querySelectorAll('.error')].map(e => e.textContent);
  check('Fehlerbanner erscheint', doc.querySelector('.banner.err') !== null);
  check('vier Fehler angezeigt', errors.length === 4, JSON.stringify(errors));
  check('Fehler an der Kurzwahl angezeigt',
    errors.some(e => e.includes('Kurzwahl muss aus 5 Ziffern')), JSON.stringify(errors));
  check('Pin-Feld markiert',
    doc.querySelector('[data-section="pins"] [data-field="nsi"] input').classList.contains('invalid'));
  check('Kurzwahl-Feld markiert',
    doc.querySelector('[data-section="numbers"] [data-row] input').classList.contains('invalid'));
  check('Fehler am Feld angezeigt',
    doc.querySelector('[data-section="sip"] [data-field="host"]').parentElement.querySelector('.error') !== null);
  check('nur das leere Uhrzeitfeld markiert',
    pickers[2].classList.contains('invalid') && pickers.filter(i => i.classList.contains('invalid')).length === 1,
    pickers.map(i => i.value + (i.classList.contains('invalid') ? '!' : '')).join(','));
  check('Fehler an den Aufstehzeiten angezeigt',
    doc.querySelector(wakeField).parentElement.querySelector('.error') !== null);

  // Nach erneutem Speichern ohne Fehler müssen alle Markierungen verschwunden sein
  window.fetch = async (url, options) => ({
    json: async () => options && options.method === 'POST'
      ? {ok: true, changed: ['[sip]'], sections: configPayload.sections}
      : configPayload,
  });
  doc.getElementById('save').click();
  await new Promise(resolve => setTimeout(resolve, 50));
  check('Fehlermarkierungen entfernt',
    doc.querySelectorAll('.invalid').length === 0 && doc.querySelectorAll('.error').length === 0);
  check('Hinweis auf Neustart nach dem Speichern',
    doc.querySelector('.banner.warn') !== null &&
    doc.querySelector('.banner.warn').textContent.includes('neu gestartet'),
    doc.querySelector('.banner.warn') && doc.querySelector('.banner.warn').textContent);
  check('Neustart wird nicht automatisch ausgelöst',
    requests.filter(r => r.url && r.url.includes('/api/action/')).length === 0);

  console.log('Formularverhalten');
  check('Typ number für Ganzzahlen',
    doc.querySelector('[data-section="pins"] [data-field="nsi"] input').type === 'number');
  check('step=1 für Ganzzahlen',
    doc.querySelector('[data-section="pins"] [data-field="nsi"] input').step === '1');
  check('step=any für Helligkeit',
    doc.querySelector('[data-section="misc"] [data-field="night_light_duty"] input').step === 'any');
  check('min/max an Zahlenfeldern',
    doc.querySelector('[data-section="sip"] [data-field="dnd_from"] input').max === '23');

  console.log(`\n${ok} ok, ${failed} fehlgeschlagen`);
  process.exit(failed ? 1 : 0);
}, 50);