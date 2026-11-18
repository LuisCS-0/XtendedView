// Panel de control del ordenador: muestra el estado que envía el host cada segundo y le
// manda los ajustes y las acciones. Solo funciona desde este ordenador.

const $ = (sel) => document.querySelector(sel);
let ws = null;
let toastTimer;
const shown = { url: null, wifi: null };
let last = null; // último estado recibido
// Lo que se está aplicando: encendido (true/false) o red ('local'/'own'). Cada cosa tarda
// unos segundos (crear la red propia, quitar el monitor virtual…) y el host contesta al acabar.
const busy = { power: null, net: null };
let pendingNet = null; // red elegida pero aún sin aplicar

function connect() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  ws = new WebSocket(`${proto}://${location.host}/ws?role=panel`);
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.t === 'state') render(m);
    else if (m.t === 'notice') {
      // La respuesta llega justo antes del estado nuevo, que es el que repinta.
      busy.power = null;
      busy.net = null;
      if (!m.ok) pendingNet = null;
      toast(m.message, m.ok);
    }
  };
  ws.onclose = () => {
    busy.power = null;
    busy.net = null;
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

// ------------------------------------------------------------------ controles

for (const group of document.querySelectorAll('.seg[data-pref], .side-picker[data-pref]')) {
  for (const b of group.querySelectorAll('button[data-value]')) {
    b.addEventListener('click', () => send({ t: 'set', key: group.dataset.pref, value: b.dataset.value }));
  }
}
for (const el of document.querySelectorAll('input[type=checkbox][data-pref]')) {
  el.addEventListener('change', () => send({ t: 'set', key: el.dataset.pref, value: el.checked }));
}
for (const el of document.querySelectorAll('select[data-pref]')) {
  el.addEventListener('change', () => send({ t: 'set', key: el.dataset.pref, value: Number(el.value) }));
}
for (const b of document.querySelectorAll('[data-action]')) {
  b.addEventListener('click', () => {
    const name = b.dataset.action;
    if (name === 'newCode' && !confirm('Se generará un código nuevo y el dispositivo conectado tendrá que volver a escanear el QR. ¿Continuar?')) return;
    if (name === 'registerResolution') toast('Acepta el aviso de Windows para registrar la resolución…');
    send({ t: 'action', name });
  });
}

// Encender y apagar
function power(on) {
  if (busy.power !== null || !ws || ws.readyState !== WebSocket.OPEN) return;
  busy.power = on;
  send({ t: 'action', name: on ? 'powerOn' : 'powerOff' });
  render();
}
$('#btn-on').addEventListener('click', () => power(true));
$('#btn-off').addEventListener('click', () => power(false));

// La red no se cambia al tocarla: hay que darle a «Aplicar» (crear la red propia tarda).
for (const b of document.querySelectorAll('#network button[data-value]')) {
  b.addEventListener('click', () => {
    if (!last || busy.net) return;
    pendingNet = b.dataset.value === last.prefs.network ? null : b.dataset.value;
    render();
  });
}
$('#net-cancel').addEventListener('click', () => { pendingNet = null; render(); });
$('#net-apply-btn').addEventListener('click', () => {
  if (!pendingNet || busy.net) return;
  busy.net = pendingNet;
  pendingNet = null;
  send({ t: 'set', key: 'network', value: busy.net });
  render();
});

// ------------------------------------------------------------------ pintar el estado

function setRadio(group, value) {
  for (const b of document.querySelectorAll(`[data-pref="${group}"] button[data-value]`)) {
    b.setAttribute('aria-checked', String(b.dataset.value === String(value)));
  }
}

function setControl(key, value) {
  for (const el of document.querySelectorAll(`[data-pref="${key}"]`)) {
    if (el === document.activeElement) continue; // no pisar lo que el usuario está tocando
    if (el.type === 'checkbox') el.checked = Boolean(value);
    else if (el.tagName === 'SELECT') el.value = String(value);
  }
}

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

const linkName = (l) => (l ? (l.wifi ? 'Wi-Fi' : `Cable (${l.name})`) : 'Cable u otra red');

const ms = (v) => (Number.isFinite(v) && v > 0 ? `${v.toFixed(v < 10 ? 1 : 0)} ms` : '—');

function tile(id, value, sub = '', main = false) {
  const el = document.getElementById(id);
  el.querySelector('b').textContent = value;
  el.querySelector('small').textContent = sub;
  el.classList.toggle('main', main);
}

function duration(secs) {
  const m = Math.floor(secs / 60);
  const h = Math.floor(m / 60);
  if (h) return `${h} h ${m % 60} min`;
  if (m) return `${m} min ${secs % 60} s`;
  return `${secs} s`;
}

const SIDES = { top: 'encima', left: 'a la izquierda', right: 'a la derecha', bottom: 'debajo' };

/** Pantalla de encendido (apagada) o panel completo (encendida). */
function renderPower(s) {
  const p = s.prefs;
  const on = s.on;
  $('#power-screen').hidden = on;
  $('#main').hidden = !on;
  $('#btn-off').hidden = !on;

  const starting = busy.power === true;
  $('#btn-on').disabled = starting;
  $('#power-title').textContent = starting ? 'Encendiendo…' : 'Encender';
  $('#power-sub').textContent = starting
    ? (p.network === 'own' ? 'Creando la red propia…' : 'Preparando la conexión…')
    : 'Empareja el dispositivo de forma inalámbrica';
  $('#power-summary').textContent = [
    p.network === 'own' ? 'Red propia (el ordenador crea su Wi-Fi)' : 'Red del ordenador',
    p.mode === 'extend' ? `Pantalla adicional ${SIDES[p.side] || ''}` : 'Espejo',
  ].join(' · ');

  const stopping = busy.power === false;
  $('#btn-off').disabled = stopping;
  $('#btn-off span').textContent = stopping ? 'Apagando…' : 'Apagar';
}

/** Selector de red con el cambio pendiente de aplicar. */
function renderNetwork(s) {
  const current = s.prefs.network;
  if (pendingNet === current) pendingNet = null;
  const shownNet = busy.net || pendingNet || current;
  for (const b of document.querySelectorAll('#network button[data-value]')) {
    const sel = b.dataset.value === shownNet;
    b.setAttribute('aria-checked', String(sel));
    b.dataset.pending = String(sel && shownNet !== current);
    b.disabled = Boolean(busy.net);
  }
  const change = busy.net || pendingNet;
  $('#net-apply').hidden = !change;
  if (!change) return;
  let text;
  if (busy.net) text = busy.net === 'own' ? 'Creando la red propia…' : 'Cerrando la red propia…';
  else if (pendingNet === 'own') text = 'El ordenador creará su propia Wi-Fi para conectar el dispositivo.';
  else text = 'Se cerrará la red propia y se usará la red a la que está conectado el ordenador.';
  if (!busy.net && s.device && current === 'own') text += ' El dispositivo conectado perderá la conexión.';
  $('#net-apply-text').textContent = text;
  $('#net-apply-btn').textContent = busy.net ? 'Aplicando…' : 'Aplicar';
  $('#net-apply-btn').disabled = Boolean(busy.net);
  $('#net-cancel').disabled = Boolean(busy.net);
}

function render(s = last) {
  if (!s) return;
  last = s;
  const p = s.prefs;
  const net = s.net;

  renderPower(s);
  if (!s.on) {
    setStatus('idle', busy.power === true ? 'Encendiendo…' : 'Apagada');
    return;
  }

  // Estado general
  if (busy.power === false) setStatus('wait', 'Apagando…');
  else if (s.device) setStatus('ok', `Conectado: ${s.device.name}`);
  else if (net.ready) setStatus('wait', 'Esperando un dispositivo');
  else setStatus('wait', 'Preparando la red…');

  // Ajustes
  renderNetwork(s);
  setRadio('mode', p.mode);
  setRadio('side', p.side);
  for (const k of ['fps', 'bitrate', 'scale', 'fullscreen', 'stats', 'measure']) setControl(k, p[k]);
  $('#extend-opts').hidden = p.mode !== 'extend';
  $('#mirror-opts').hidden = p.mode !== 'mirror';
  $('#vdd-missing').hidden = s.vdd.available;
  const monSel = $('#monitor');
  const monKey = JSON.stringify(s.monitors);
  if (monSel.dataset.key !== monKey) {
    monSel.dataset.key = monKey;
    monSel.replaceChildren(...s.monitors.map((m) => new Option(`${m.index + 1}: ${m.name} (${m.w}×${m.h})`, m.index)));
  }
  setControl('monitor', p.monitor);

  // Red y códigos QR
  const own = p.network === 'own';
  const warn = $('#net-warning');
  let warning = '';
  if (own && net.error) warning = `No se pudo crear la red propia: ${net.error}`;
  else if (own && net.otherWifi) warning = 'El ordenador sigue conectado a otra Wi-Fi. La tarjeta se reparte entre las dos redes y habrá más lag: para la mínima latencia, desconéctalo de esa red.';
  else if (!net.ready) warning = own ? 'Creando la red propia…' : 'El ordenador no está conectado a ninguna red. Conéctalo o elige «Red propia».';
  warn.textContent = warning;
  warn.hidden = !warning;

  $('#qr-empty').hidden = net.ready;
  $('#qr-url-box').hidden = !net.ready;
  if (net.ready && net.url !== shown.url) {
    shown.url = net.url;
    $('#qr-url').src = `/qr/url.svg?v=${encodeURIComponent(net.url)}`;
  }
  const wifiText = own && net.ssid ? `${net.ssid}|${net.password}` : null;
  $('#qr-wifi-box').hidden = !wifiText;
  if (wifiText && wifiText !== shown.wifi) {
    shown.wifi = wifiText;
    $('#qr-wifi').src = `/qr/wifi.svg?v=${encodeURIComponent(wifiText)}`;
  }
  $('#qr-ssid').textContent = net.ssid ? `«${net.ssid}»` : '';
  $('#qr-url-caption').innerHTML = wifiText
    ? '<b>Paso 2.</b> Escanéalo para abrir la pantalla en el navegador'
    : '<b>Escanéalo con la cámara</b> para abrir la pantalla en el navegador';
  kv($('#net-kv'), [
    ['Red', own ? (net.ssid ? `Wi-Fi propia «${net.ssid}»` : 'Wi-Fi propia') : linkName(net.link)],
    own && net.password ? ['Contraseña', net.password] : null,
    ['Dirección', net.url ? link(net.url.split('#')[0]) : '—'],
    ['Código', s.code ?? '—'],
  ]);
  $('#ca-fp').textContent = s.caFingerprint ? `SHA-256 ${s.caFingerprint}` : '—';
  if (net.setupUrl) {
    $('#setup-link').href = net.setupUrl;
    $('#setup-link').textContent = net.setupUrl;
  }

  // Información de la conexión
  $('#code').textContent = s.code ?? '—';
  $('#btn-disconnect').disabled = !s.device;
  $('#no-device').hidden = Boolean(s.device);
  $('#device-info').hidden = !s.device;
  if (!s.device) return;
  const v = s.viewerStats || {};
  const h = s.hostStats || {};
  const st = s.stream;
  tile('t-e2e', ms(v.e2e), v.e2e ? `p95 ${ms(v.e2e95)}` : (p.measure ? 'Abre la sonda' : 'Activa «Medir latencia»'), true);
  tile('t-net', ms(v.net), 'envío de cada imagen');
  tile('t-rtt', ms(v.rtt), 'ping');
  tile('t-dec', ms(v.dec), 'en el dispositivo');
  tile('t-enc', ms(h.encMs50), h.encMs95 ? `p95 ${ms(h.encMs95)}` : '');
  tile('t-fps', Number.isFinite(v.fps) ? `${v.fps}` : '—', st ? `de ${st.fps} fps` : '');
  tile('t-mbps', Number.isFinite(v.mbps) ? `${v.mbps.toFixed(1)} Mbps` : '—', `máx. ${p.bitrate} Mbps`);
  tile('t-drop', Number.isFinite(h.dropped) ? String(h.dropped) : '—', 'por segundo');
  const d = s.device;
  kv($('#info-kv'), [
    ['Dispositivo', d.name],
    ['Dirección IP', d.ip],
    ['Pantalla del dispositivo', `${d.screen.replace('x', '×')} a ${d.hz} Hz`],
    ['Conectado desde hace', duration(d.since)],
    ['Imagen enviada', st ? `${st.width}×${st.height} a ${st.fps} fps${st.portrait ? ' · vertical' : ''}` : '—'],
    ['Modo', st ? (st.extend ? `Pantalla adicional (${st.monitor})` : `Espejo de ${st.monitor}`) : '—'],
    ['Codificador', h.encoder || '—'],
    ['Red', own ? 'Wi-Fi propia' : 'Red del ordenador'],
  ]);
  const exact = st && st.extend && !st.exact && st.register;
  $('#exact-box').hidden = !exact;
  if (exact) {
    $('#exact-text').textContent = `La pantalla adicional usa ${st.width}×${st.height}, la resolución de serie más parecida. Para que encaje exacta (${st.register.split('@')[0].replace('x', '×')}), regístrala una vez:`;
  }
}

connect();
