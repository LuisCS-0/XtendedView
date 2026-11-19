// Panel de control del ordenador: muestra el estado que envía el programa cada segundo y le
// manda los ajustes y las acciones. Solo funciona desde este ordenador.
//
// Tres secciones: Pantallas (colocarlas arrastrándolas y los ajustes de cada dispositivo),
// Conectar (redes y códigos QR) y Ajustes (los generales). Los dispositivos que esperan a que
// los aceptes se ven en todas.

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
let ws = null;
let toastTimer;
let last = null; // último estado recibido
// Lo que se está aplicando y tarda unos segundos (encender, crear la red propia…): el programa
// contesta al acabar con un aviso.
const busy = { power: null, conn: null };
let startingNet = null;
let selectedConn = null; // pestaña de conexión que se está viendo
let view = null; // sección que se está viendo (null: la elige el panel)
const shownQr = {}; // qué texto tiene cada QR, para no volver a pedirlo

function connect() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  // ui=1: es la ventana del panel (al cerrarla, el programa puede cerrarse del todo).
  ws = new WebSocket(`${proto}://${location.host}/ws?role=panel&ui=1`);
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
    setStatus('off', 'Sin conexión con el programa', '¿Está abierto XtendedView?');
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

// Icono del estado: pantallas enlazadas (conectado), ondas (esperando), campana (alguien pide
// conectarse) y encendido (apagada o sin programa).
const STATUS_ICONS = {
  ok: '<rect x="1.8" y="4" width="13.5" height="9.5" rx="1.6"/><path d="M5.5 17.5h6M8.5 13.5v4"/><rect x="17.2" y="7.5" width="5" height="10.5" rx="1.3"/><path d="M15.3 10.5h1.9"/>',
  wait: '<circle class="fill" cx="12" cy="12" r="2"/><path class="wave" d="M8.5 15.5a5 5 0 0 1 0-7M15.5 8.5a5 5 0 0 1 0 7"/><path class="wave" d="M5.6 18.4a9 9 0 0 1 0-12.8M18.4 5.6a9 9 0 0 1 0 12.8"/>',
  ask: '<path d="M6 16.5V11a6 6 0 0 1 12 0v5.5l1.5 2h-15z"/><path d="M10 21a2.2 2.2 0 0 0 4 0"/>',
  idle: '<path d="M12 3v8"/><path d="M6.3 6.8a8 8 0 1 0 11.4 0"/>',
};
STATUS_ICONS.off = STATUS_ICONS.idle;

function setStatus(kind, text, sub = '') {
  const el = $('#status');
  if (el.dataset.kind !== kind || !$('svg', el).innerHTML) $('.status-icon svg', el).innerHTML = STATUS_ICONS[kind];
  el.dataset.kind = kind;
  $('#status-text').textContent = text;
  $('#status-sub').textContent = sub;
}

// ------------------------------------------------------------------ tema

const darkQuery = matchMedia('(prefers-color-scheme: dark)');
let theme = 'system';
let themeSent = null;

function applyTheme(t) {
  theme = t;
  if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
  try { localStorage.setItem('theme', t); } catch { /* opcional */ }
  const dark = t === 'dark' || (t === 'system' && darkQuery.matches);
  $('#logo').src = dark ? '/logo-tema-oscuro.png' : '/logo-tema-claro.png';
  for (const b of $$('[data-theme-choice]')) b.setAttribute('aria-checked', String(b.dataset.themeChoice === t));
}
darkQuery.addEventListener('change', () => applyTheme(theme));
try { applyTheme(localStorage.getItem('theme') || 'system'); } catch { applyTheme('system'); }
for (const b of $$('[data-theme-choice]')) {
  b.addEventListener('click', () => {
    applyTheme(b.dataset.themeChoice); // en seguida, sin esperar al estado
    themeSent = b.dataset.themeChoice;
    send({ t: 'set', key: 'theme', value: b.dataset.themeChoice });
  });
}

const SIDES = { top: 'encima del', left: 'a la izquierda del', right: 'a la derecha del', bottom: 'debajo del' };

// ------------------------------------------------------------------ tipo de dispositivo
// Por el nombre que manda su navegador («SM-F971B · Android · Chrome», «iPad · Safari»,
// «Windows · Edge») y, si no basta, por la forma de su pantalla: alargada, un móvil; más
// cuadrada, una tablet (el Fold desplegado sale como tablet y plegado como móvil).
const DEVICE_ICONS = {
  phone: '<rect x="6.5" y="2.5" width="11" height="19" rx="2.6"/><path d="M10.5 18.5h3"/>',
  tablet: '<rect x="3.5" y="2.5" width="17" height="19" rx="2.6"/><path d="M10.5 18.5h3"/>',
  tabletWide: '<rect x="2.5" y="4.5" width="19" height="15" rx="2.6"/><path d="M18.5 10.5v3"/>',
  laptop: '<rect x="4" y="4.5" width="16" height="11" rx="1.6"/><path d="M2 19h20"/>',
};

function deviceKind(name = '', screen = '') {
  const n = name.toLowerCase();
  const [w, h] = screen.split('x').map(Number);
  const ratio = w && h ? Math.max(w, h) / Math.min(w, h) : 2;
  if (n.includes('iphone')) return 'phone';
  if (n.includes('ipad')) return w > h ? 'tabletWide' : 'tablet';
  if (!n.includes('android') && /windows|mac|linux|chrome os|cros/.test(n)) return 'laptop';
  if (ratio >= 1.8) return 'phone';
  return w > h ? 'tabletWide' : 'tablet';
}

const deviceIcon = (name, screen) => DEVICE_ICONS[deviceKind(name, screen)];

// ------------------------------------------------------------------ secciones

function showView(name) {
  view = name;
  render();
}
for (const b of $$('[data-view]')) b.addEventListener('click', () => showView(b.dataset.view));
for (const b of $$('[data-goto]')) b.addEventListener('click', () => showView(b.dataset.goto));

function renderViews(s) {
  // Sin elegir: con dispositivos, sus pantallas; sin ellos, cómo conectarlos.
  const current = view || (s.devices.length ? 'screens' : 'connect');
  for (const b of $$('[data-view]')) {
    if (b.dataset.view === current) b.setAttribute('aria-current', 'page');
    else b.removeAttribute('aria-current');
  }
  for (const p of $$('[data-view-panel]')) p.hidden = p.dataset.viewPanel !== current;
}

// ------------------------------------------------------------------ controles generales

// Encender eligiendo la conexión (las tarjetas de la pantalla de inicio) y apagar.
const POWER_ON = { local: 'powerOnLocal', own: 'powerOnOwn', cable: 'powerOnCable' };
function power(on, network = null) {
  if (busy.power !== null || !ws || ws.readyState !== WebSocket.OPEN) return;
  busy.power = on;
  startingNet = on ? network : null;
  selectedConn = network;
  view = null;
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
$('#quit-on-close').addEventListener('change', (e) => send({ t: 'set', key: 'quitOnClose', value: e.target.checked }));
$('#blocked').addEventListener('click', (e) => {
  const b = e.target.closest('[data-unblock]');
  if (b) send({ t: 'unblock', device: b.dataset.unblock });
});
for (const b of $$('[data-smoothing]')) {
  b.addEventListener('click', () => send({ t: 'set', key: 'smoothing', value: b.dataset.smoothing }));
}

for (const b of $$('[data-action]')) {
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
  const block = b.dataset.decide === 'block';
  if (block && !confirm(`${item.dataset.name} no podrá volver a pedir conexión hasta que lo desbloquees en Ajustes. ¿Bloquearlo?`)) return;
  b.disabled = true;
  send({ t: 'decide', id: Number(item.dataset.id), accept: b.dataset.decide === '1', block });
});

// Ajustes y acciones de cada dispositivo (una tarjeta por dispositivo).
const deviceOf = (el) => el.closest('.device')?.dataset.device;
$('#device-list').addEventListener('click', (e) => {
  const opt = e.target.closest('button[data-dkey]');
  if (opt) {
    send({ t: 'setDevice', device: deviceOf(opt), key: opt.dataset.dkey, value: opt.dataset.value });
    return;
  }
  const side = e.target.closest('[data-side]');
  if (side) {
    const device = deviceOf(side);
    const m = arrange.mons.find((x) => x.device === device);
    if (m) commitPosition(m, sidePosition(m, side.dataset.side));
    return;
  }
  const act = e.target.closest('[data-act]');
  if (act) {
    const name = act.dataset.act;
    const device = deviceOf(act);
    if (name === 'registerResolution') toast('Acepta el aviso de Windows para registrar la resolución…');
    send({ t: 'action', name, device });
    return;
  }
  const card = e.target.closest('.device');
  if (card) select(card.dataset.device);
});
$('#device-list').addEventListener('change', (e) => {
  const el = e.target.closest('[data-dkey]');
  if (!el) return;
  const value = el.type === 'checkbox' ? el.checked : Number(el.value);
  send({ t: 'setDevice', device: deviceOf(el), key: el.dataset.dkey, value });
});

// ------------------------------------------------------------------ colocación de las pantallas
// Como en la configuración de pantalla de Windows: los monitores del ordenador están fijos y la
// pantalla de cada dispositivo (su monitor virtual) se arrastra. Al soltarla se pega al borde más
// cercano de otro monitor, sin solaparse, y el ordenador la mueve ahí sin cortar la imagen.

const arrange = {
  els: new Map(), // nombre del monitor → elemento
  mons: [], // posiciones (en píxeles del escritorio) que se están mostrando
  scale: 1,
  ox: 0,
  oy: 0,
  drag: null,
  pending: {}, // posición recién elegida, hasta que el ordenador la confirme
  selected: null,
};

const overlaps = (a, b) => a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y;
const clamp = (v, lo, hi) => Math.min(Math.max(v, lo), hi);

/** Ajusta `v` a un borde o al centro del otro monitor si está cerca (como Windows al soltar). */
function align(v, size, start, length) {
  const t = Math.min(size, length) * 0.08;
  for (const target of [start, start + length - size, start + (length - size) / 2]) {
    if (Math.abs(v - target) <= t) return target;
  }
  return v;
}

/** Posición válida más cercana para `r`: pegada a un lado de otro monitor, sin solaparse con ninguno. */
function snapPosition(r, others) {
  let best = null;
  for (const o of others) {
    const minY = Math.min(r.h, o.h) * 0.15; // que compartan al menos un trozo de borde
    const minX = Math.min(r.w, o.w) * 0.15;
    const y = align(clamp(r.y, o.y - r.h + minY, o.y + o.h - minY), r.h, o.y, o.h);
    const x = align(clamp(r.x, o.x - r.w + minX, o.x + o.w - minX), r.w, o.x, o.w);
    for (const c of [{ x: o.x + o.w, y }, { x: o.x - r.w, y }, { x, y: o.y - r.h }, { x, y: o.y + o.h }]) {
      const rect = { x: c.x, y: c.y, w: r.w, h: r.h };
      if (others.some((q) => overlaps(rect, q))) continue;
      const dist = Math.hypot(c.x - r.x, c.y - r.y);
      if (!best || dist < best.dist) best = { x: Math.round(c.x), y: Math.round(c.y), dist };
    }
  }
  return best;
}

/** El monitor principal del ordenador (el que está en el origen del escritorio). */
const mainMonitor = () => arrange.mons.find((x) => x.primary) || arrange.mons.find((x) => !x.device);

/** Posición exacta en un lado del ordenador: centrada en ese lado y, si ya hay otra pantalla ahí,
 *  por fuera de ella (para tener dos a la derecha, una tras otra). */
function sidePosition(m, side) {
  const main = mainMonitor();
  const others = arrange.mons.filter((x) => x.name !== m.name);
  let x = main.x + Math.round((main.w - m.w) / 2);
  let y = main.y + Math.round((main.h - m.h) / 2);
  if (side === 'right') x = main.x + main.w;
  else if (side === 'left') x = main.x - m.w;
  else if (side === 'top') y = main.y - m.h;
  else y = main.y + main.h;
  for (let i = 0; i < others.length; i++) {
    const hit = others.find((o) => overlaps({ x, y, w: m.w, h: m.h }, o));
    if (!hit) break;
    if (side === 'right') x = hit.x + hit.w;
    else if (side === 'left') x = hit.x - m.w;
    else if (side === 'top') y = hit.y - m.h;
    else y = hit.y + hit.h;
  }
  return { x, y };
}

/** En qué lado del monitor principal está una pantalla (para marcar su botón). */
function sideOf(m) {
  const main = mainMonitor();
  if (!main) return null;
  if (m.x >= main.x + main.w) return 'right';
  if (m.x + m.w <= main.x) return 'left';
  if (m.y + m.h <= main.y) return 'top';
  if (m.y >= main.y + main.h) return 'bottom';
  return null;
}

/** Escala para que quepan todos los monitores con margen alrededor para arrastrar. */
function fitScale(box, mons) {
  const devs = mons.filter((m) => m.device);
  const margin = Math.max(0, ...devs.map((m) => Math.max(m.w, m.h))) * 0.45;
  const minX = Math.min(...mons.map((m) => m.x)) - margin;
  const minY = Math.min(...mons.map((m) => m.y)) - margin;
  const maxX = Math.max(...mons.map((m) => m.x + m.w)) + margin;
  const maxY = Math.max(...mons.map((m) => m.y + m.h)) + margin;
  const pad = 16;
  const W = box.clientWidth - 2 * pad;
  const H = box.clientHeight - 2 * pad;
  const s = Math.min(W / (maxX - minX), H / (maxY - minY));
  arrange.scale = s;
  arrange.ox = pad + (W - (maxX - minX) * s) / 2 - minX * s;
  arrange.oy = pad + (H - (maxY - minY) * s) / 2 - minY * s;
}

function placeEl(el, r) {
  const s = arrange.scale;
  el.style.left = `${arrange.ox + r.x * s}px`;
  el.style.top = `${arrange.oy + r.y * s}px`;
  el.style.width = `${r.w * s}px`;
  el.style.height = `${r.h * s}px`;
  el.classList.toggle('tiny', r.h * s < 48);
}

function select(device) {
  arrange.selected = device;
  for (const el of arrange.els.values()) el.classList.toggle('selected', Boolean(device) && el.dataset.device === device);
  for (const card of $$('.device')) card.classList.toggle('selected', card.dataset.device === device);
}

function renderArrange(s) {
  const box = $('#arrange');
  $('#arrange-card').hidden = s.devices.length === 0;
  if (!s.devices.length || arrange.drag || !s.layout?.length) return;
  const now = performance.now();
  // Mientras el ordenador no confirma una posición recién elegida, se muestra la elegida.
  const mons = s.layout.map((m) => {
    const p = m.device && arrange.pending[m.device];
    if (p && (m.x !== p.x || m.y !== p.y) && now < p.until) return { ...m, x: p.x, y: p.y };
    if (p) delete arrange.pending[m.device];
    return m;
  });
  arrange.mons = mons;
  fitScale(box, mons);
  const mirrors = {};
  for (const d of s.devices) {
    if (d.stream && !d.stream.extend) (mirrors[d.stream.monitor] ||= []).push(d.name);
  }
  const seen = new Set();
  let n = 1;
  for (const m of [...mons].sort((a, b) => Number(b.primary) - Number(a.primary))) {
    seen.add(m.name);
    let el = arrange.els.get(m.name);
    if (!el) {
      el = document.createElement('div');
      el.innerHTML = '<b></b><small></small><span class="mirror-tag" hidden></span>';
      box.append(el);
      arrange.els.set(m.name, el);
    }
    const dev = Boolean(m.device);
    el.className = `mon ${dev ? 'dev' : 'host'}${m.primary ? ' primary' : ''}${dev && m.device === arrange.selected ? ' selected' : ''}`;
    el.dataset.name = m.name;
    el.dataset.device = m.device || '';
    if (dev) {
      el.tabIndex = 0;
      el.setAttribute('role', 'button');
      el.setAttribute('aria-label', `Pantalla de ${m.deviceName}: arrástrala o muévela con las flechas`);
      el.title = 'Arrástrala para colocarla';
    } else {
      el.removeAttribute('tabindex');
      el.removeAttribute('role');
      el.removeAttribute('aria-label');
      el.title = '';
    }
    $('b', el).textContent = dev ? m.deviceName : m.primary ? 'Este ordenador' : `Monitor ${++n}`;
    $('small', el).textContent = `${m.w}×${m.h}`;
    const tag = $('.mirror-tag', el);
    tag.hidden = !mirrors[m.name];
    if (mirrors[m.name]) tag.textContent = `Espejo: ${mirrors[m.name].join(', ')}`;
    placeEl(el, m);
  }
  for (const [name, el] of arrange.els) {
    if (!seen.has(name)) {
      el.remove();
      arrange.els.delete(name);
    }
  }
}

/** Recuadro discontinuo donde se va a quedar la pantalla al soltarla. */
function showTarget(r) {
  let t = $('#arrange .target');
  if (!r) { t?.remove(); return; }
  if (!t) {
    t = document.createElement('div');
    t.className = 'mon target';
    $('#arrange').append(t);
  }
  placeEl(t, r);
}

function commitPosition(m, pos) {
  arrange.pending[m.device] = { x: pos.x, y: pos.y, until: performance.now() + 5000 };
  send({ t: 'setDevice', device: m.device, key: 'position', value: [pos.x, pos.y] });
  render();
}

const arrangeBox = $('#arrange');
arrangeBox.addEventListener('pointerdown', (e) => {
  const el = e.target.closest('.mon.dev');
  if (!el || e.button !== 0) return;
  const m = arrange.mons.find((x) => x.name === el.dataset.name);
  if (!m) return;
  e.preventDefault();
  el.focus({ preventScroll: true });
  el.setPointerCapture(e.pointerId);
  arrange.drag = { el, m, x0: e.clientX, y0: e.clientY, moved: false, target: null };
  select(m.device);
});
arrangeBox.addEventListener('pointermove', (e) => {
  const d = arrange.drag;
  if (!d) return;
  if (!d.moved && Math.hypot(e.clientX - d.x0, e.clientY - d.y0) < 4) return;
  d.moved = true;
  d.el.classList.add('dragging');
  const r = { x: d.m.x + (e.clientX - d.x0) / arrange.scale, y: d.m.y + (e.clientY - d.y0) / arrange.scale, w: d.m.w, h: d.m.h };
  placeEl(d.el, r);
  const others = arrange.mons.filter((x) => x.name !== d.m.name);
  const snap = snapPosition(r, others);
  d.target = snap && { x: snap.x, y: snap.y, w: r.w, h: r.h };
  showTarget(d.target);
});
function endDrag(commit) {
  const d = arrange.drag;
  arrange.drag = null;
  if (!d) return;
  d.el.classList.remove('dragging');
  showTarget(null);
  if (commit && d.moved && d.target && (d.target.x !== d.m.x || d.target.y !== d.m.y)) commitPosition(d.m, d.target);
  else render();
}
arrangeBox.addEventListener('pointerup', () => endDrag(true));
arrangeBox.addEventListener('pointercancel', () => endDrag(false));

// Con el teclado: las flechas la mueven un poco y se pega al borde más cercano.
arrangeBox.addEventListener('keydown', (e) => {
  const el = e.target.closest('.mon.dev');
  const step = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }[e.key];
  if (!el || !step) return;
  const m = arrange.mons.find((x) => x.name === el.dataset.name);
  if (!m) return;
  e.preventDefault();
  const r = { x: m.x + step[0] * m.w * 0.25, y: m.y + step[1] * m.h * 0.25, w: m.w, h: m.h };
  const snap = snapPosition(r, arrange.mons.filter((x) => x.name !== m.name));
  if (snap) commitPosition(m, snap);
});
arrangeBox.addEventListener('focusin', (e) => {
  const el = e.target.closest('.mon.dev');
  if (el) select(el.dataset.device);
});
addEventListener('resize', () => render());

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
        <svg viewBox="0 0 24 24" aria-hidden="true">${deviceIcon(p.name, p.screen)}</svg>
        <div class="pending-text"><strong></strong><span></span><small></small></div>
        <button type="button" class="btn" data-decide="block">Bloquear</button>
        <button type="button" class="btn" data-decide="0">Rechazar</button>
        <button type="button" class="btn primary" data-decide="1">Aceptar</button>`;
      box.append(el);
    }
    el.dataset.name = p.name;
    $('strong', el).textContent = `${p.name} quiere conectarse`;
    $('span', el).textContent = `Por ${p.via} · ${p.ip} · pantalla ${p.screen.replace('x', '×')}`;
    $('small', el).textContent = `Si no lo aceptas, se rechazará solo en ${p.left} s.`;
  }
}

/** Sección Conectar: pestañas, activar/desactivar, códigos QR y datos. */
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
    else if (k === 'own' && st.otherWifi) text = 'El ordenador sigue conectado a otra Wi-Fi. La tarjeta se reparte entre las dos redes y habrá más tirones: para la mínima latencia, desconéctalo de esa red o usa «Imagen más suave» en Ajustes.';
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

/** El tema guardado (también con la pantalla apagada, en la pantalla de inicio). */
function syncTheme(s) {
  // Recién elegido en este panel: hasta que el programa lo confirme, se mantiene.
  if (themeSent && s.prefs.theme === themeSent) themeSent = null;
  if (!themeSent && s.prefs.theme !== theme) applyTheme(s.prefs.theme);
}

/** Sección Ajustes. */
function renderSettings(s) {
  setValue($('#quit-on-close'), s.prefs.quitOnClose);
  const blocked = s.blocked || [];
  $('#no-blocked').hidden = blocked.length > 0;
  const key = JSON.stringify(blocked);
  if ($('#blocked').dataset.key !== key) {
    $('#blocked').dataset.key = key;
    $('#blocked').replaceChildren(...blocked.map((b) => {
      const li = document.createElement('li');
      li.innerHTML = `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M5.6 5.6l12.8 12.8"/></svg>
        <div><strong></strong><small></small></div>
        <button type="button" class="btn small">Desbloquear</button>`;
      $('strong', li).textContent = b.name || 'Dispositivo sin nombre';
      $('small', li).textContent = `${b.ip} · bloqueado el ${b.since}`;
      $('button', li).dataset.unblock = b.device;
      return li;
    }));
  }
  setValue($('#measure'), s.prefs.measure);
  for (const b of $$('[data-smoothing]')) b.setAttribute('aria-checked', String(b.dataset.smoothing === s.prefs.smoothing));
  $('#code-settings').textContent = s.code ?? '—';
  $('#vdd-text').textContent = s.vdd.available
    ? `Disponible (Parsec VDD 0.${s.vdd.version}): cada dispositivo en «Pantalla adicional» tiene su propio monitor virtual.`
    : 'Falta el driver Parsec VDD, que crea los monitores virtuales. Mientras no esté instalado, los dispositivos se verán en modo espejo.';
  $('#vdd-link').hidden = s.vdd.available;
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
    card.classList.toggle('selected', d.device === arrange.selected);
    const p = d.prefs;
    const st = d.stream;
    const v = d.viewerStats || {};
    const h = d.hostStats || {};
    $('[data-f="name"]', card).textContent = d.name;
    const icon = deviceIcon(d.name, d.screen);
    const svg = $('.device-icon svg', card);
    if (svg.dataset.icon !== icon) {
      svg.dataset.icon = icon;
      svg.innerHTML = icon; // cambia si el Fold se pliega o se despliega
    }
    $('[data-f="sub"]', card).textContent = `${d.via} · ${d.ip} · conectado hace ${duration(d.since)}`;
    for (const b of $$('button[data-dkey]', card)) {
      b.setAttribute('aria-checked', String(String(p[b.dataset.dkey]) === b.dataset.value));
    }
    const extend = p.mode === 'extend';
    $('[data-f="extend"]', card).hidden = !extend;
    $('[data-f="mirror"]', card).hidden = extend;
    // La escala de Windows solo tiene sentido en su propia pantalla (en espejo cambiaría la del
    // monitor del ordenador).
    $('[data-f="dpi"]', card).hidden = !extend;
    $('[data-f="dpi-hint"]', card).hidden = !extend;
    const mon = arrange.mons.find((x) => x.device === d.device) || s.layout?.find((x) => x.device === d.device);
    const at = mon ? sideOf(mon) : null;
    let place;
    if (st && !st.extend) place = 'Sin el driver de monitores virtuales se ve en espejo (mira en Ajustes).';
    else if (!mon) place = 'Preparando su pantalla…';
    else place = `Está ${SIDES[at] || 'junto al'} ordenador. Arrástrala en el plano de arriba o colócala en un lado:`;
    $('[data-f="place-text"]', card).textContent = place;
    $('[data-f="sides"]', card).hidden = !mon;
    for (const b of $$('[data-side]', card)) b.setAttribute('aria-pressed', String(b.dataset.side === at));
    const monSel = $('select[data-dkey="monitor"]', card);
    if (monSel.dataset.key !== monKey) {
      monSel.dataset.key = monKey;
      monSel.replaceChildren(...s.monitors.map((m) => new Option(`${m.index + 1}: ${m.name} (${m.w}×${m.h})`, m.index)));
    }
    for (const el of $$('select[data-dkey], input[data-dkey]', card)) setValue(el, p[el.dataset.dkey]);

    tile(card, 'e2e', ms(v.e2e), v.e2e ? `p95 ${ms(v.e2e95)}` : (s.prefs.measure ? 'Abre la sonda' : 'Activa la medida en Ajustes'));
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
      ['Modo', st ? (st.extend ? `Pantalla adicional (${st.monitor})` : `Espejo de ${st.monitor}`) : '—'],
      st?.extend && st.dpi ? ['Escala de Windows', `${st.dpi} %${p.dpi ? '' : ' (la recomendada)'}`] : null,
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
  syncTheme(s);
  renderPower(s);
  if (!s.on) {
    if (busy.power === true) setStatus('wait', 'Encendiendo…', 'Preparando la conexión');
    else setStatus('idle', 'Apagada', 'Elige cómo conectar para empezar');
    return;
  }
  const n = s.devices.length;
  const anyReady = Object.values(s.connections).some((c) => c.ready);
  const names = (list) => list.map((d) => d.name).join(' · ');
  if (busy.power === false) setStatus('wait', 'Apagando…', 'Quitando las pantallas de los dispositivos');
  else if (s.pending.length) setStatus('ask', s.pending.length === 1 ? 'Quiere conectarse' : `${s.pending.length} quieren conectarse`, names(s.pending));
  else if (n === 1) setStatus('ok', 'Conectado', `${s.devices[0].name} · ${s.devices[0].via}`);
  else if (n) setStatus('ok', `${n} dispositivos conectados`, names(s.devices));
  else if (anyReady) setStatus('wait', 'Esperando dispositivos', `Código ${s.code ?? '—'}`);
  else setStatus('wait', 'Preparando la conexión…', s.prefs.connections.includes('cable') ? 'Esperando el cable' : '');

  $('#code').textContent = s.code ?? '—';
  $('#nav-count').hidden = n === 0;
  $('#nav-count').textContent = `${n}/${s.maxDevices}`;
  renderViews(s);
  renderPending(s);
  renderConnections(s);
  renderSettings(s);
  renderDevices(s);
  renderArrange(s);
}

connect();
