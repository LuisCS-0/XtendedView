// Panel de control del ordenador: muestra el estado que envía el host cada segundo y le
// manda los ajustes y las acciones. Solo funciona desde este ordenador.
//
// Varios dispositivos a la vez: cada uno tiene su tarjeta con sus ajustes. Las conexiones
// (red del ordenador, red propia, cable) se activan por separado y puede haber varias.

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
let ws = null;
let toastTimer;
let last = null; // último estado recibido
// Lo que se está aplicando y tarda unos segundos (encender, crear la red propia…): el host
// contesta al acabar con un aviso.
const busy = { power: null, conn: null };
let startingNet = null;
let selectedConn = null; // pestaña de conexión que se está viendo
const shownQr = {}; // qué texto tiene cada QR, para no volver a pedirlo

function connect() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  ws = new WebSocket(`${proto}://${location.host}/ws?role=panel`);
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.t === 'state') render(m);
    else if (m.t === 'notice') {
      // La respuesta llega justo antes del estado nuevo, que es el que repinta.
      busy.power = null;
      busy.conn = null;
      toast(m.message, m.ok);
    }
  };
  ws.onclose = () => {
    busy.power = null;
    busy.conn = null;
    setStatus('off', 'Sin conexión con el host: ¿está abierto?');
    setTimeout(connect, 1000);
  };
}

function send(msg) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(msg));
}

function toast(text, ok = true) {
  const t = $('#toast');
  t.textContent = text;
  t.dataset.ok = String(ok);
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, ok ? 4000 : 8000);
}

function setStatus(kind, text) {
  $('#status').dataset.kind = kind;
  $('#status-text').textContent = text;
}

const CONN_NAMES = { local: 'Red del ordenador', own: 'Red propia', cable: 'Cable USB' };
const SIDES = { top: 'encima', left: 'a la izquierda', right: 'a la derecha', bottom: 'debajo' };

// ------------------------------------------------------------------ controles generales

// Encender eligiendo la conexión (las tarjetas de la pantalla de inicio) y apagar.
const POWER_ON = { local: 'powerOnLocal', own: 'powerOnOwn', cable: 'powerOnCable' };
function power(on, network = null) {
  if (busy.power !== null || !ws || ws.readyState !== WebSocket.OPEN) return;
  busy.power = on;
  startingNet = on ? network : null;
  selectedConn = network;
  send({ t: 'action', name: on ? POWER_ON[network] : 'powerOff' });
  render();
}
for (const card of $$('.connect-card')) card.addEventListener('click', () => power(true, card.dataset.network));
$('#btn-off').addEventListener('click', () => power(false));

// Pestañas de conexión.
for (const tab of $$('[data-conn]')) {
  tab.addEventListener('click', () => { selectedConn = tab.dataset.conn; render(); });
}

// Activar o desactivar una conexión.
function setConnection(kind, enabled) {
  if (busy.conn) return;
  busy.conn = kind;
  send({ t: 'connection', kind, enabled });
  render();
}
for (const b of $$('[data-toggle-conn]')) {
  b.addEventListener('click', () => {
    const kind = b.dataset.toggleConn;
    setConnection(kind, !last?.connections[kind].enabled);
  });
}
for (const b of $$('[data-toggle-on]')) {
  b.addEventListener('click', () => { selectedConn = b.dataset.toggleOn; setConnection(b.dataset.toggleOn, true); });
}

$('#measure').addEventListener('change', (e) => send({ t: 'set', key: 'measure', value: e.target.checked }));

for (const b of $$('main [data-action], .card [data-action]')) {
  b.addEventListener('click', () => {
    const name = b.dataset.action;
    if (name === 'newCode' && !confirm('Se generará un código nuevo: los dispositivos conectados se desconectarán y tendrán que volver a escanear el QR. ¿Continuar?')) return;
    if (name === 'fixCableRoute') toast('Acepta el aviso de Windows para que el ordenador no use los datos del móvil…');
    send({ t: 'action', name });
  });
}

// Aceptar o rechazar dispositivos que esperan.
$('#pending').addEventListener('click', (e) => {
  const b = e.target.closest('[data-decide]');
  if (!b) return;
  const item = b.closest('[data-id]');
  b.disabled = true;
  send({ t: 'decide', id: Number(item.dataset.id), accept: b.dataset.decide === '1' });
});

// Ajustes y acciones de cada dispositivo (una tarjeta por dispositivo).
const deviceOf = (el) => el.closest('.device')?.dataset.device;
$('#device-list').addEventListener('click', (e) => {
  const opt = e.target.closest('button[data-dkey]');
  if (opt) {
    send({ t: 'setDevice', device: deviceOf(opt), key: opt.dataset.dkey, value: opt.dataset.value });
    return;
  }
  const act = e.target.closest('[data-act]');
  if (act) {
    const name = act.dataset.act;
    if (name === 'registerResolution') toast('Acepta el aviso de Windows para registrar la resolución…');
    send({ t: 'action', name, device: deviceOf(act) });
  }
});
$('#device-list').addEventListener('change', (e) => {
  const el = e.target.closest('[data-dkey]');
  if (!el) return;
  const value = el.type === 'checkbox' ? el.checked : Number(el.value);
  send({ t: 'setDevice', device: deviceOf(el), key: el.dataset.dkey, value });
});

// ------------------------------------------------------------------ utilidades de pintado

function kv(dl, rows) {
  dl.replaceChildren(...rows.filter(Boolean).flatMap(([k, v]) => {
    const dt = document.createElement('dt');
    dt.textContent = k;
    const dd = document.createElement('dd');
    if (v instanceof Node) dd.append(v); else dd.textContent = v ?? '—';
    return [dt, dd];
  }));
}

function link(url) {
  const a = document.createElement('a');
  a.href = url;
  a.textContent = url;
  a.target = '_blank';
  a.rel = 'noopener';
  return a;
}

function setQr(img, which, text) {
  if (!text || shownQr[which] === text) return;
  shownQr[which] = text;
  img.src = `/qr/${which}.svg?v=${encodeURIComponent(text)}`;
}

const ms = (v) => (Number.isFinite(v) && v > 0 ? `${v.toFixed(v < 10 ? 1 : 0)} ms` : '—');

function duration(secs) {
  const m = Math.floor(secs / 60);
  const h = Math.floor(m / 60);
  if (h) return `${h} h ${m % 60} min`;
  if (m) return `${m} min ${secs % 60} s`;
  return `${secs} s`;
}

/** No pisar lo que el usuario está tocando. */
function setValue(el, value) {
  if (el === document.activeElement) return;
  if (el.type === 'checkbox') el.checked = Boolean(value);
  else el.value = String(value);
}

// ------------------------------------------------------------------ pintar el estado

/** Pantalla de inicio (apagada): una tarjeta por forma de conectar. */
function renderPower(s) {
  const on = s.on;
  $('#power-screen').hidden = on;
  $('#main').hidden = !on;
  $('#btn-off').hidden = !on;

  const starting = busy.power === true;
  const cable = s.connections.cable.detected; // cable ya conectado (módem USB activo)
  const BUSY_TEXT = { local: 'Encendiendo…', own: 'Creando la red propia…', cable: 'Preparando la conexión por cable…' };
  for (const card of $$('.connect-card')) {
    const n = card.dataset.network;
    const mine = starting && startingNet === n;
    card.disabled = starting;
    card.setAttribute('aria-busy', String(mine));
    $('.last-tag', card).hidden = starting || !s.prefs.connections.includes(n);
    const ready = n === 'cable' && Boolean(cable);
    card.classList.toggle('ready', ready && !starting);
    const small = $('small', card);
    small.textContent = mine ? BUSY_TEXT[n] : ready ? `Cable detectado (${cable.name}): pulsa para conectar` : small.dataset.text;
  }
  $('#power-summary').textContent = 'Puedes conectar varios dispositivos y configurar cada uno por separado.';

  const stopping = busy.power === false;
  $('#btn-off').disabled = stopping;
  $('#btn-off span').textContent = stopping ? 'Apagando…' : 'Apagar';
}

/** Dispositivos que esperan a que los aceptes. */
function renderPending(s) {
  const box = $('#pending');
  box.hidden = s.pending.length === 0;
  const ids = new Set(s.pending.map((p) => String(p.id)));
  for (const el of $$('[data-id]', box)) if (!ids.has(el.dataset.id)) el.remove();
  for (const p of s.pending) {
    let el = $(`[data-id="${p.id}"]`, box);
    if (!el) {
      el = document.createElement('div');
      el.className = 'pending-item';
      el.dataset.id = p.id;
      el.innerHTML = `
        <svg viewBox="0 0 24 24" aria-hidden="true"><rect x="6" y="2.5" width="12" height="19" rx="2.5"/><path d="M10.5 18.5h3"/></svg>
        <div class="pending-text"><strong></strong><span></span><small></small></div>
        <button type="button" class="btn" data-decide="0">Rechazar</button>
        <button type="button" class="btn primary" data-decide="1">Aceptar</button>`;
      box.append(el);
    }
    $('strong', el).textContent = `${p.name} quiere conectarse`;
    $('span', el).textContent = `Por ${p.via} · ${p.ip} · pantalla ${p.screen.replace('x', '×')}`;
    $('small', el).textContent = `Si no lo aceptas, se rechazará solo en ${p.left} s.`;
  }
}

/** Tarjeta de conexiones: pestañas, activar/desactivar, códigos QR y datos. */
function renderConnections(s) {
  const c = s.connections;
  if (!selectedConn || !c[selectedConn]) selectedConn = s.prefs.connections[0] || 'local';
  for (const tab of $$('[data-conn]')) {
    const k = tab.dataset.conn;
    tab.setAttribute('aria-selected', String(k === selectedConn));
    tab.dataset.state = c[k].ready ? 'ok' : c[k].enabled ? 'wait' : 'off';
  }
  for (const panel of $$('[data-conn-panel]')) {
    const k = panel.dataset.connPanel;
    const st = c[k];
    panel.hidden = k !== selectedConn;
    const toggle = $('[data-toggle-conn]', panel);
    const working = busy.conn === k;
    toggle.disabled = Boolean(busy.conn);
    toggle.className = `btn small ${st.enabled ? '' : 'primary'}`;
    toggle.textContent = working ? (st.enabled ? 'Desactivando…' : 'Activando…') : st.enabled ? 'Desactivar' : 'Activar';
    $('[data-qrs]', panel).hidden = !st.ready;
    for (const img of $$('[data-qr]', panel)) {
      const which = img.dataset.qr;
      setQr(img, which, which === 'wifi' ? (st.ssid ? `${st.ssid}|${st.password}` : null) : st.url);
    }
    const warn = $('[data-warn]', panel);
    let text = '';
    if (!st.enabled) text = '';
    else if (k === 'local' && !st.ready) text = 'El ordenador no está conectado a ninguna red. Conéctalo o usa la red propia.';
    else if (k === 'own' && st.error) text = `No se pudo crear la red propia: ${st.error}`;
    else if (k === 'own' && !st.ready) text = 'Creando la red propia…';
    else if (k === 'own' && st.otherWifi) text = 'El ordenador sigue conectado a otra Wi-Fi. La tarjeta se reparte entre las dos redes y habrá más lag: para la mínima latencia, desconéctalo de esa red.';
    if (warn) { warn.textContent = text; warn.hidden = !text; }
    const rows = [];
    if (!st.enabled) rows.push(['Estado', 'Desactivada: nadie puede conectarse por aquí']);
    else if (k === 'local') rows.push(['Red', st.link ? (st.link.wifi ? 'Wi-Fi' : `Cable (${st.link.name})`) : '—']);
    else if (k === 'own') rows.push(['Red', st.ssid ? `«${st.ssid}»` : '—'], st.password ? ['Contraseña', st.password] : null);
    else rows.push(['Tarjeta', st.detected ? st.detected.name : 'Sin cable']);
    if (st.enabled) rows.push(['Dirección', st.url ? link(st.url.split('#')[0]) : '—']);
    kv($('[data-kv]', panel), rows);
    if (k === 'own') $('[data-ssid]', panel).textContent = st.ssid ? `«${st.ssid}»` : '';
    if (k === 'cable') {
      $('[data-cable-help]', panel).hidden = !(st.enabled && !st.detected);
      const route = st.enabled && st.detected ? st.detected.route : 'fine';
      const note = $('[data-route]', panel);
      note.hidden = route === 'fine';
      note.classList.toggle('warn', route === 'preferred');
      $('[data-route-fix]', panel).hidden = route !== 'preferred';
      $('[data-route-text]', panel).textContent = route === 'preferred'
        ? 'Con el módem USB, Windows está saliendo a internet por el móvil y gastará sus datos (salvo que el móvil esté en una Wi-Fi).'
        : 'El ordenador no tiene otra conexión a internet: si la necesita, usará la del móvil.';
    }
  }
  const offer = c.cable.detected && !c.cable.enabled && busy.conn !== 'cable';
  $('#cable-offer').hidden = !offer;
  if (offer) $('#cable-offer-name').textContent = c.cable.detected.name;
  $('#ca-fp').textContent = s.caFingerprint ? `SHA-256 ${s.caFingerprint}` : '—';
  if (s.setupUrl) {
    $('#setup-link').href = s.setupUrl;
    $('#setup-link').textContent = s.setupUrl;
  }
}

function tile(card, key, value, sub = '') {
  const el = $(`[data-t="${key}"]`, card);
  $('b', el).textContent = value;
  $('small', el).textContent = sub;
}

/** Una tarjeta por dispositivo conectado, con sus ajustes y sus medidas. */
function renderDevices(s) {
  const list = $('#device-list');
  $('#no-device').hidden = s.devices.length > 0;
  const ids = new Set(s.devices.map((d) => d.device));
  for (const el of $$('.device', list)) if (!ids.has(el.dataset.device)) el.remove();
  const monKey = JSON.stringify(s.monitors);
  for (const d of s.devices) {
    let card = $(`.device[data-device="${CSS.escape(d.device)}"]`, list);
    if (!card) {
      card = $('#device-template').content.firstElementChild.cloneNode(true);
      card.dataset.device = d.device;
      list.append(card);
    }
    const p = d.prefs;
    const st = d.stream;
    const v = d.viewerStats || {};
    const h = d.hostStats || {};
    $('[data-f="name"]', card).textContent = d.name;
    $('[data-f="sub"]', card).textContent = `${d.via} · ${d.ip} · conectado hace ${duration(d.since)}`;
    for (const b of $$('button[data-dkey]', card)) {
      b.setAttribute('aria-checked', String(String(p[b.dataset.dkey]) === b.dataset.value));
    }
    $('[data-f="extend"]', card).hidden = p.mode !== 'extend';
    $('[data-f="mirror"]', card).hidden = p.mode !== 'mirror';
    const monSel = $('select[data-dkey="monitor"]', card);
    if (monSel.dataset.key !== monKey) {
      monSel.dataset.key = monKey;
      monSel.replaceChildren(...s.monitors.map((m) => new Option(`${m.index + 1}: ${m.name} (${m.w}×${m.h})`, m.index)));
    }
    for (const el of $$('select[data-dkey], input[data-dkey]', card)) setValue(el, p[el.dataset.dkey]);

    tile(card, 'e2e', ms(v.e2e), v.e2e ? `p95 ${ms(v.e2e95)}` : (s.prefs.measure ? 'Abre la sonda' : 'Activa «Medir latencia»'));
    tile(card, 'net', ms(v.net), 'envío de cada imagen');
    tile(card, 'rtt', ms(v.rtt), 'ping');
    tile(card, 'dec', ms(v.dec), 'en el dispositivo');
    tile(card, 'enc', ms(h.encMs50), h.encMs95 ? `p95 ${ms(h.encMs95)}` : '');
    tile(card, 'fps', Number.isFinite(v.fps) ? `${v.fps}` : '—', st ? `de ${st.fps} fps` : '');
    tile(card, 'mbps', Number.isFinite(v.mbps) ? `${v.mbps.toFixed(1)} Mbps` : '—', `máx. ${p.bitrate} Mbps`);
    tile(card, 'drop', Number.isFinite(h.dropped) ? String(h.dropped) : '—', 'por segundo');
    kv($('[data-f="kv"]', card), [
      ['Pantalla', `${d.screen.replace('x', '×')} a ${d.hz} Hz`],
      ['Imagen enviada', st ? `${st.width}×${st.height} a ${st.fps} fps${st.portrait ? ' · vertical' : ''}` : '—'],
      ['Modo', st ? (st.extend ? `Pantalla adicional ${SIDES[p.side] || ''} (${st.monitor})` : `Espejo de ${st.monitor}`) : '—'],
      ['Codificador', h.encoder || '—'],
    ]);
    const exact = st && st.extend && !st.exact && st.register;
    $('[data-f="exact"]', card).hidden = !exact;
    if (exact) {
      $('[data-f="exact-text"]', card).textContent = `Usa ${st.width}×${st.height}, la resolución de serie más parecida. Para que encaje exacta (${st.register.split('@')[0].replace('x', '×')}), regístrala una vez:`;
    }
  }
}

function render(s = last) {
  if (!s) return;
  last = s;
  renderPower(s);
  if (!s.on) {
    setStatus('idle', busy.power === true ? 'Encendiendo…' : 'Apagada');
    return;
  }
  const n = s.devices.length;
  const anyReady = Object.values(s.connections).some((c) => c.ready);
  if (busy.power === false) setStatus('wait', 'Apagando…');
  else if (s.pending.length) setStatus('wait', s.pending.length === 1 ? 'Un dispositivo espera tu aprobación' : `${s.pending.length} dispositivos esperan tu aprobación`);
  else if (n) setStatus('ok', n === 1 ? `Conectado: ${s.devices[0].name}` : `${n} dispositivos conectados`);
  else if (anyReady) setStatus('wait', 'Esperando un dispositivo');
  else setStatus('wait', s.prefs.connections.includes('cable') ? 'Esperando el cable…' : 'Preparando la conexión…');

  renderPending(s);
  renderConnections(s);
  $('#code').textContent = s.code ?? '—';
  $('#device-count').textContent = `${n} de ${s.maxDevices}`;
  setValue($('#measure'), s.prefs.measure);
  $('#vdd-missing').hidden = s.vdd.available;
  renderDevices(s);
}

connect();
