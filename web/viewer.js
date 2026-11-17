// Visor: recibe H.264 por WebSocket, lo decodifica con WebCodecs y lo pinta en un canvas.
// Devuelve al portátil el táctil, el ratón y el lápiz.
import { ClockSync, Window, b64ToBytes, findTimecodeBand, readTimecode, timecodeAge, storage } from './common.js';

const $ = (id) => document.getElementById(id);
const canvas = $('screen');
const ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
const HEADER = 20; // ver FRAME_HEADER en host/main.py

const ui = {
  panel: $('panel'), hud: $('hud'), status: $('status'), problem: $('problem'), host: $('host'),
  cursor: $('cursor'),
  key: $('key'), mode: $('mode'), fps: $('fps'), bitrate: $('bitrate'), scale: $('scale'), toast: $('toast'),
  measure: $('measure'), showhud: $('showhud'),
  start: $('start'), fullscreen: $('fullscreen'), menu: $('menu-btn'),
};

const st = {
  ws: null, want: false, connected: false,
  decoder: null, decoderConfig: null, configuring: false, queue: [], waitingKey: true,
  pending: new Map(), stream: null, last: null, rect: { x: 0, y: 0, w: 0, h: 0 },
  lastHello: null, wakeLock: null, retryTimer: null,
};
const sync = new ClockSync((msg) => send(msg));
const cursor = { w: 0, h: 0, x: 0, y: 0, visible: false };
const m = {
  frames: 0, bytes: 0, fps: 0, mbps: 0, t0: performance.now(),
  net: new Window(120), dec: new Window(120), e2e: new Window(240), host: null,
};

// ------------------------------------------------------------------ ajustes

// Claves de localStorage. "fps2": con la opción Auto nueva, se ignora el valor guardado antes
// (que solía ser el 60 por defecto).
const STORE = { mode: 'mode', fps: 'fps2', bitrate: 'bitrate', scale: 'scale' };

const normKey = (s) => s.toUpperCase().replace(/[^A-Z0-9]/g, '');
const formatKey = (s) => { const k = normKey(s); return k.length > 4 ? `${k.slice(0, 4)}-${k.slice(4, 8)}` : k; };

function loadSettings() {
  const k = new URLSearchParams(location.hash.slice(1)).get('k');
  if (k) {
    storage('key', normKey(k));
    history.replaceState(null, '', location.pathname + location.search);
  }
  ui.key.value = formatKey(storage('key') || '');
  for (const [id, key] of Object.entries(STORE)) {
    const v = storage(key);
    if (v && [...ui[id].options].some((o) => (o.value || o.text) === v)) ui[id].value = v;
  }
  ui.measure.checked = storage('measure') === '1';
  ui.showhud.checked = storage('showhud') !== '0';
}

function saveSettings() {
  storage('key', normKey(ui.key.value));
  for (const [id, key] of Object.entries(STORE)) storage(key, ui[id].value);
  storage('measure', ui.measure.checked ? '1' : '0');
  storage('showhud', ui.showhud.checked ? '1' : '0');
}

function supportProblem() {
  if (!window.isSecureContext) {
    return 'Esta página necesita HTTPS para poder decodificar vídeo. <a href="/setup">Configurar este dispositivo</a>';
  }
  if (!('VideoDecoder' in window)) {
    return 'Este navegador no tiene WebCodecs. Usa Chrome 94+ o Safari 16.4+ actualizados.';
  }
  return null;
}

// ------------------------------------------------------------------ conexión

function send(msg) {
  if (st.ws && st.ws.readyState === WebSocket.OPEN) st.ws.send(JSON.stringify(msg));
}

function connect() {
  st.want = true;
  clearTimeout(st.retryTimer);
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(`${proto}://${location.host}/ws?role=viewer`);
  ws.binaryType = 'arraybuffer';
  st.ws = ws;
  setStatus('Conectando…');
  updateControls();
  ws.onopen = () => {
    st.connected = true;
    st.lastHello = null;
    sync.start();
    sendHello();
    setStatus('Conectado. Esperando vídeo…');
    updateControls();
  };
  ws.onmessage = (ev) => (typeof ev.data === 'string' ? onText(JSON.parse(ev.data)) : onFrame(ev.data));
  ws.onclose = () => {
    if (st.ws !== ws) return;
    st.ws = null;
    st.connected = false;
    sync.stop();
    resetDecoder();
    if (st.want) {
      setStatus('Conexión perdida. Reintentando…');
      showPanel(true);
      st.retryTimer = setTimeout(connect, 1000);
    }
    updateControls();
  };
}

function disconnect(reason) {
  st.want = false;
  clearTimeout(st.retryTimer);
  if (st.ws) { const ws = st.ws; st.ws = null; ws.close(); }
  st.connected = false;
  st.stream = null;
  sync.stop();
  resetDecoder();
  st.last?.close();
  st.last = null;
  ui.cursor.hidden = true;
  st.wakeLock?.release?.().catch(() => {});
  st.wakeLock = null;
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  layout();
  setStatus(reason || 'Desconectado.');
  showPanel(true);
  updateControls();
}

function targetSize() {
  const dpr = window.devicePixelRatio || 1;
  const s = Number(ui.scale.value) || 1;
  return { w: Math.round(innerWidth * dpr * s), h: Math.round(innerHeight * dpr * s) };
}

// ------------------------------------------------------------------ frecuencia de pantalla
// Con "Auto" se captura a la frecuencia real de la pantalla (120 Hz en el Fold): a más
// frecuencia, menos espera hasta la siguiente captura y menos retraso al mostrar cada frame.

const RATES = [30, 60, 90, 120]; // lo que admite el host
let displayHz = 60;

/** Frecuencia de la pantalla en Hz, o null si el navegador no anima (pestaña oculta, etc.). */
function measureRefresh(frames = 30, timeoutMs = 1500) {
  return new Promise((resolve) => {
    const times = [];
    let done = false;
    const timer = setTimeout(() => { done = true; resolve(null); }, timeoutMs);
    const step = (t) => {
      if (done) return;
      times.push(t);
      if (times.length < frames) { requestAnimationFrame(step); return; }
      done = true;
      clearTimeout(timer);
      const gaps = times.slice(1).map((v, i) => v - times[i]).sort((a, b) => a - b);
      const median = gaps[Math.floor(gaps.length / 2)];
      resolve(median > 0 ? 1000 / median : null);
    };
    requestAnimationFrame(step);
  });
}

async function detectRefresh() {
  if (document.visibilityState !== 'visible') return; // en segundo plano el navegador frena las animaciones
  const hz = await measureRefresh();
  if (hz === null) return; // sin medida: se mantiene la última frecuencia conocida
  // Mínimo 60: el navegador puede frenar las animaciones (ahorro de batería, pestaña tapada)
  // y medir menos de lo que da la pantalla.
  const rate = RATES.filter((r) => r >= 60).reduce((a, b) => (Math.abs(b - hz) < Math.abs(a - hz) ? b : a));
  ui.fps.options[0].textContent = `Auto (${rate})`;
  if (rate !== displayHz) {
    displayHz = rate;
    if (st.connected && ui.fps.value === 'auto') sendHello();
  }
}

function targetFps() {
  return ui.fps.value === 'auto' ? displayHz : Number(ui.fps.value);
}

function sendHello() {
  const { w, h } = targetSize();
  const hello = { t: 'hello', key: normKey(ui.key.value), mode: ui.mode.value, w, h,
    fps: targetFps(), bitrate: Number(ui.bitrate.value) };
  const sig = JSON.stringify(hello);
  if (sig === st.lastHello) return;
  st.lastHello = sig;
  send(hello);
}

function onText(msg) {
  switch (msg.t) {
    case 'pong': sync.onPong(msg); break;
    case 'config': configure(msg); break;
    case 'stats': m.host = msg; break;
    case 'cursor': Object.assign(cursor, { x: msg.x, y: msg.y, visible: msg.v }); placeCursor(); break;
    case 'cursorShape': setCursorShape(msg); break;
    case 'auth': disconnect('Código incorrecto. Usa el que aparece en la consola del portátil.'); break;
    case 'bye': disconnect(msg.reason); break;
    case 'error': setStatus(msg.message); showPanel(true); break;
    case 'info': showToast(msg.message); break;
  }
}

let toastTimer;
/** Aviso flotante que no interrumpe (se puede seleccionar el texto para copiarlo). */
function showToast(text) {
  ui.toast.textContent = text;
  ui.toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { ui.toast.hidden = true; }, 15000);
}

// ------------------------------------------------------------------ vídeo

async function configure(msg) {
  st.configuring = true;
  st.queue = [];
  // Con "description" (host en Python) los frames vienen en formato AVCC; sin ella (host en
  // Rust) vienen en Annex B, con SPS/PPS dentro de cada keyframe.
  const base = { codec: msg.codec, codedWidth: msg.width, codedHeight: msg.height, optimizeForLatency: true };
  if (msg.description) base.description = b64ToBytes(msg.description);
  let chosen = null;
  for (const hardwareAcceleration of ['prefer-hardware', 'no-preference']) {
    try {
      const cfg = { ...base, hardwareAcceleration };
      if ((await VideoDecoder.isConfigSupported(cfg)).supported) { chosen = cfg; break; }
    } catch { /* probar la siguiente */ }
  }
  if (!chosen) {
    st.configuring = false;
    setStatus(`Este dispositivo no puede decodificar ${msg.codec}.`);
    showPanel(true);
    return;
  }
  resetDecoder();
  st.decoder = new VideoDecoder({ output: onDecoded, error: onDecoderError });
  st.decoder.configure(chosen);
  st.decoderConfig = chosen;
  st.stream = msg;
  st.waitingKey = true;
  layout();
  st.configuring = false;
  const queued = st.queue;
  st.queue = [];
  queued.forEach(decode);
  m.net.clear(); m.dec.clear(); m.e2e.clear();
  setStatus(`Recibiendo ${msg.width}×${msg.height} a ${msg.fps} fps (${msg.codec}).`);
  showPanel(false);
}

function resetDecoder() {
  if (st.decoder && st.decoder.state !== 'closed') {
    try { st.decoder.close(); } catch { /* ya cerrado */ }
  }
  st.decoder = null;
  st.pending.clear();
}

function onDecoderError(e) {
  console.error('Error del decodificador', e);
  if (!st.decoderConfig || !st.connected) return;
  // Se reconfigura y se espera al siguiente keyframe (llega como mucho en 1 s).
  st.decoder = new VideoDecoder({ output: onDecoded, error: onDecoderError });
  st.decoder.configure(st.decoderConfig);
  st.waitingKey = true;
  st.pending.clear();
}

function onFrame(buf) {
  const arrival = performance.now();
  const dv = new DataView(buf);
  if (dv.getUint8(0) !== 1) return;
  const item = {
    key: (dv.getUint8(1) & 1) === 1,
    seq: dv.getUint32(4, true),
    data: new Uint8Array(buf, HEADER),
  };
  m.bytes += buf.byteLength;
  if (sync.ready) m.net.push(arrival + sync.offset - dv.getFloat64(8, true));
  if (st.configuring) st.queue.push(item);
  else decode(item);
}

function decode(item) {
  const d = st.decoder;
  if (!d || d.state !== 'configured') return;
  if (st.waitingKey) {
    if (!item.key) return;
    st.waitingKey = false;
  }
  st.pending.set(item.seq, performance.now());
  d.decode(new EncodedVideoChunk({ type: item.key ? 'key' : 'delta', timestamp: item.seq, data: item.data }));
}

function onDecoded(frame) {
  const now = performance.now();
  const seq = frame.timestamp;
  const t = st.pending.get(seq);
  st.pending.delete(seq);
  const r = st.rect;
  ctx.drawImage(frame, r.x, r.y, r.w, r.h);
  // Se guarda el último frame para repintar al girar o desplegar la pantalla.
  st.last?.close();
  st.last = frame;
  m.frames++;
  if (t !== undefined) m.dec.push(now - t);
  if (ui.measure.checked && sync.ready) measureE2E(frame);
  send({ t: 'ack', seq });
  if (st.pending.size > 240) st.pending.clear();
}

// Medida de punta a punta: se localiza la sonda en la imagen (como mucho dos veces por
// segundo, sobre una copia reducida) y después se lee una sola fila de píxeles por frame.
const probe = { at: null, lastSearch: 0, misses: 0, canvas: null, ctx: null };

function locateProbe(frame) {
  const now = performance.now();
  if (now - probe.lastSearch < 500) return;
  probe.lastSearch = now;
  const R = st.rect;
  const sw = Math.max(1, Math.min(1024, R.w));
  const k = sw / R.w;
  const sh = Math.max(1, Math.round(R.h * k));
  if (!probe.canvas) {
    probe.canvas = document.createElement('canvas');
    probe.ctx = probe.canvas.getContext('2d', { willReadFrequently: true });
  }
  probe.canvas.width = sw;
  probe.canvas.height = sh;
  probe.ctx.drawImage(frame, 0, 0, sw, sh);
  const box = findTimecodeBand(probe.ctx.getImageData(0, 0, sw, sh).data, sw, sh);
  if (!box) return;
  probe.at = {
    x: Math.round(R.x + (box.x0 + 1) / k),
    w: Math.max(1, Math.round((box.x1 - box.x0 - 1) / k)),
    y: Math.round(R.y + (box.yc + 0.5) / k),
  };
  probe.misses = 0;
}

function measureE2E(frame) {
  if (!probe.at) locateProbe(frame);
  const p = probe.at;
  if (!p) return;
  const v = readTimecode(ctx.getImageData(p.x, p.y, p.w, 1).data, p.w);
  if (v === null) {
    if (++probe.misses > 20) probe.at = null; // la sonda se ha movido o cerrado
    return;
  }
  probe.misses = 0;
  const age = timecodeAge(sync.now(), v);
  if (age !== null) m.e2e.push(age);
}

function layout() {
  const dpr = window.devicePixelRatio || 1;
  const cw = Math.round(innerWidth * dpr);
  const ch = Math.round(innerHeight * dpr);
  if (canvas.width !== cw || canvas.height !== ch) {
    canvas.width = cw;
    canvas.height = ch;
  }
  ctx.fillStyle = '#000';
  ctx.fillRect(0, 0, cw, ch);
  probe.at = null;
  const s = st.stream;
  if (!s) { st.rect = { x: 0, y: 0, w: cw, h: ch }; return; }
  const k = Math.min(cw / s.width, ch / s.height);
  const w = Math.round(s.width * k);
  const h = Math.round(s.height * k);
  st.rect = { x: Math.floor((cw - w) / 2), y: Math.floor((ch - h) / 2), w, h };
  if (st.last) ctx.drawImage(st.last, st.rect.x, st.rect.y, w, h);
  placeCursor();
}

// ------------------------------------------------------------------ cursor

function setCursorShape(msg) {
  const bytes = b64ToBytes(msg.rgba);
  ui.cursor.width = msg.w;
  ui.cursor.height = msg.h;
  const pixels = new Uint8ClampedArray(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  ui.cursor.getContext('2d').putImageData(new ImageData(pixels, msg.w, msg.h), 0, 0);
  Object.assign(cursor, { w: msg.w, h: msg.h });
  placeCursor();
}

/** Coloca el cursor sobre la imagen, a la misma escala que el escritorio. */
function placeCursor() {
  const s = st.stream;
  if (!s || !s.srcWidth || !st.connected || !cursor.visible || !cursor.w) {
    ui.cursor.hidden = true;
    return;
  }
  const dpr = window.devicePixelRatio || 1;
  const r = st.rect;
  const k = r.w / s.srcWidth / dpr; // píxeles CSS por píxel del escritorio
  ui.cursor.style.width = `${cursor.w * k}px`;
  ui.cursor.style.height = `${cursor.h * k}px`;
  ui.cursor.style.transform = `translate(${(r.x + cursor.x * r.w) / dpr}px, ${(r.y + cursor.y * r.h) / dpr}px)`;
  ui.cursor.hidden = false;
}

// ------------------------------------------------------------------ entrada

const KIND = { pointerdown: 'down', pointermove: 'move', pointerup: 'up', pointercancel: 'cancel' };

function onPointer(e) {
  if (!st.connected || !st.stream) return;
  e.preventDefault();
  if (e.type === 'pointerdown') canvas.setPointerCapture?.(e.pointerId);
  const dpr = window.devicePixelRatio || 1;
  const r = st.rect;
  send({
    t: 'pointer', e: KIND[e.type], id: e.pointerId, type: e.pointerType, b: e.button,
    x: +((e.clientX * dpr - r.x) / r.w).toFixed(5),
    y: +((e.clientY * dpr - r.y) / r.h).toFixed(5),
    p: +e.pressure.toFixed(3),
  });
}

for (const type of Object.keys(KIND)) canvas.addEventListener(type, onPointer, { passive: false });
canvas.addEventListener('wheel', (e) => {
  e.preventDefault();
  const k = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1;
  send({ t: 'wheel', dx: e.deltaX * k, dy: e.deltaY * k });
}, { passive: false });
canvas.addEventListener('contextmenu', (e) => e.preventDefault());

// ------------------------------------------------------------------ interfaz

function setStatus(text) { ui.status.textContent = text; }
function showPanel(show) { ui.panel.hidden = !show; }

function updateControls() {
  ui.start.textContent = st.want ? 'Desconectar' : 'Conectar';
  ui.menu.hidden = !st.connected;
  ui.fullscreen.hidden = !st.connected;
}

// Ni la pantalla completa ni el bloqueo de suspensión deben retrasar la conexión:
// algunos navegadores tardan o no responden nunca a estas peticiones.
function enterFullscreen() {
  const el = document.documentElement;
  try {
    if (el.requestFullscreen) el.requestFullscreen({ navigationUI: 'hide' }).catch(() => {});
    else el.webkitRequestFullscreen?.();
  } catch { /* p. ej. iPhone: no se permite */ }
  requestWakeLock();
}

function requestWakeLock() {
  navigator.wakeLock?.request('screen').then((l) => { st.wakeLock = l; }).catch(() => {});
}

const fmt = (v, d = 1) => (Number.isFinite(v) ? v.toFixed(d) : '–');

function renderHud() {
  const show = ui.showhud.checked && st.connected && st.stream;
  ui.hud.hidden = !show;
  if (!show) return;
  const s = st.stream;
  const h = m.host;
  const hw = st.decoderConfig?.hardwareAcceleration === 'prefer-hardware' ? 'HW' : 'auto';
  const host = s.encoder === 'mf' ? 'Rust/GPU' : `Python/${s.encoder}`;
  const mode = s.mode === 'extend' ? 'extendida' : 'espejo';
  const lines = [
    `${s.width}×${s.height} · ${s.fps} fps · ${mode} · ${s.codec} · ${hw} · ${host}`,
    `Recibido   ${fmt(m.fps, 0)} fps · ${fmt(m.mbps)} Mbps`,
    `Red        ${fmt(m.net.avg)} ms (p95 ${fmt(m.net.pct(0.95))}) · RTT ${fmt(sync.rtt)}`,
    `Decodif.   ${fmt(m.dec.avg)} ms (p95 ${fmt(m.dec.pct(0.95))})`,
  ];
  if (h) {
    lines.push(h.encMs50 !== undefined
      ? `Portátil   ${h.encFps} fps · ${h.dropped} descartados · captura→cod. ${fmt(h.encMs50)} ms (p95 ${fmt(h.encMs95)})`
      : `Portátil   ${h.encFps} fps · ${h.dropped} descartados · var. cod. ${fmt(h.encJitter50)}/${fmt(h.encJitter95)} ms`);
  }
  if (ui.measure.checked) {
    lines.push(m.e2e.count
      ? `TOTAL      ${fmt(m.e2e.avg, 0)} ms (mín ${fmt(m.e2e.min, 0)} · p95 ${fmt(m.e2e.pct(0.95), 0)})`
      : 'TOTAL      sin señal: abre /probe en el portátil y déjala visible');
  }
  ui.hud.textContent = lines.join('\n');
}

setInterval(() => {
  const now = performance.now();
  const dt = (now - m.t0) / 1000;
  m.t0 = now;
  m.fps = m.frames / dt;
  m.mbps = (m.bytes * 8) / 1e6 / dt;
  m.frames = 0;
  m.bytes = 0;
  if (st.connected && st.stream) {
    send({
      t: 'stats', fps: Math.round(m.fps), mbps: +m.mbps.toFixed(2),
      net: +fmt(m.net.avg) || 0, dec: +fmt(m.dec.avg) || 0, rtt: +fmt(sync.rtt) || 0,
      e2e: m.e2e.count ? Math.round(m.e2e.avg) : 0, e2e95: m.e2e.count ? Math.round(m.e2e.pct(0.95)) : 0,
    });
  }
  renderHud();
}, 1000);

let resizeTimer;
addEventListener('resize', () => {
  layout();
  clearTimeout(resizeTimer);
  // Al plegar o desplegar puede cambiar también la frecuencia de la pantalla.
  resizeTimer = setTimeout(async () => {
    await detectRefresh();
    if (st.connected) sendHello();
  }, 400);
});

document.addEventListener('visibilitychange', () => {
  if (document.visibilityState !== 'visible') return;
  if (st.want) requestWakeLock();
  detectRefresh();
});

ui.start.addEventListener('click', () => {
  if (st.want) { disconnect(); return; }
  saveSettings();
  if (normKey(ui.key.value).length !== 8) {
    setStatus('Introduce el código de 8 caracteres que aparece en la consola del portátil.');
    ui.key.focus();
    return;
  }
  enterFullscreen();
  connect();
});
ui.fullscreen.addEventListener('click', enterFullscreen);
ui.menu.addEventListener('click', () => showPanel(ui.panel.hidden));
ui.key.addEventListener('blur', () => { ui.key.value = formatKey(ui.key.value); });
for (const id of ['mode', 'fps', 'bitrate', 'scale']) {
  ui[id].addEventListener('change', () => { saveSettings(); if (st.connected) sendHello(); });
}
ui.measure.addEventListener('change', () => { saveSettings(); m.e2e.clear(); });
ui.showhud.addEventListener('change', () => { saveSettings(); renderHud(); });

// ------------------------------------------------------------------ inicio

loadSettings();
layout();
detectRefresh();
ui.host.textContent = location.host;
const problem = supportProblem();
if (problem) {
  ui.problem.innerHTML = problem;
  ui.problem.hidden = false;
  ui.start.disabled = true;
}
