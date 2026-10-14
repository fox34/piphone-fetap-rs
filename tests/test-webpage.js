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

function render(what) {
  // `--features harness`: die Beispiele liegen hinter diesem Feature, damit
  // `cargo test` sie nicht mitbaut. Der Renderer braucht es also auch.
  return execFileSync('cargo', ['run', '--quiet', '--features', 'harness',
                                '--example', 'render-page', '--', what],
                      {cwd: repo, encoding: 'utf8'});
}

let ok = 0, failed = 0;
function check(label, condition, detail = '') {
  if (condition) { ok++; console.log(`  ok  ${label}`); }
  else { failed++; console.log(`FAIL  ${label} ${detail}`); }
}

const html = render('CONFIG_PAGE');
const configPayload = JSON.parse(render('fixture'));

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
    JSON.stringify(sections) === JSON.stringify(['network', 'sip', 'pins', 'numbers', 'ringtones', 'sounds', 'misc', 'web']),
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

  const wake = doc.querySelector('[data-section="misc"] [data-field="wake_up_times"] textarea').value;
  check('Liste über Zeilen gefüllt', wake.split('\n').length === 7, JSON.stringify(wake));
  check('Einschlafmusik als Zeilen',
    doc.querySelector('[data-section="sounds"] [data-field="sleep_music"] textarea').value.includes('.mp3'));

  console.log('Kurzwahlen und Klingeltöne');
  const rows = selector => doc.querySelectorAll(`[data-section="${selector}"] [data-row]`);
  const inputs = row => [...row.querySelectorAll('input')].map(i => i.value);
  const numbers = [...rows('numbers')].map(inputs);
  check('alle Kurzwahlen als Zeilen', numbers.length === 9, JSON.stringify(numbers));
  check('erste Kurzwahl gefüllt', numbers[0][0] === '01' && numbers[0][1] === '01234567');
  check('Kurzbefehl als Ziel',
    numbers.some(n => n[1] === 'enable-night-mode') && numbers.some(n => n[1] === 'shutdown'));
  check('Klingeltöne als Zeilen', rows('ringtones').length === 1);
  check('Vorschläge für Kurzbefehle vorhanden',
    doc.querySelectorAll('[data-section="numbers"] datalist option').length === 6);

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
  doc.querySelector('[data-section="misc"] [data-field="wake_up_times"] textarea').value =
    '05:00\n05:00\n05:00\n05:00\n05:00\n06:00\n06:00';
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
  check('unveränderte Liste korrekt', sent.misc.wake_up_times[5] === '06:00');
  check('Einschlafmusik als Array', Array.isArray(sent.sounds.sleep_music));
  check('Kurzwahlen als Objekte', sent.numbers['01'] === '01234567');
  check('Kurzbefehl mitgesendet', sent.numbers['11'] === 'enable-night-mode');
  check('leere Zeile nicht mitgesendet', !('' in sent.numbers) && !Object.values(sent.numbers).includes(''));
  check('Klingeltöne mitgesendet', sent.ringtones['08912345'] === '/opt/piphone/sounds/ring-02.wav');
  check('Pflichtfelder mitgesendet', sent.pins.nsi === '23' && sent.pins.gabel === '15');

  console.log('Fehleranzeige');
  // Kurzwahl auf einen ungültigen Wert setzen, damit der Fehler einer Zeile zugeordnet werden kann
  rows('numbers')[0].querySelectorAll('input')[0].value = '1234567';

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