// Visor: recibe H.264 por WebSocket, lo decodifica con WebCodecs y lo pinta en un canvas.
// Devuelve al ordenador el táctil, el ratón y el lápiz. No tiene ajustes propios: todo se
// elige en el panel del ordenador, que le dice si mostrar estadísticas, medir la latencia o
// pasar a pantalla completa.
import { ClockSync, Window, b64ToBytes, findTimecodeBand, readTimecode, timecodeAge, storage } from './common.js';

const $ = (id) => document.getElementById(id);
const canvas = $('screen');
const ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
const HEADER = 20; // cabecera binaria de cada frame (ver host-rs/src/server.rs)

const ui = {
  gate: $('gate'), status: $('status'), problem: $('problem'), host: $('host'),
  keyForm: $('key-form'), key: $('key'), retry: $('retry'),
  hud: $('hud'), toast: $('toast'), cursor: $('cursor'), fsHint: $('fs-hint'),
};

const st = {
  ws: null, want: false, connected: false,
  decoder: null, decoderConfig: null, configuring: false, queue: [], waitingKey: true,
  pending: new Map(), stream: null, last: null, rect: { x: 0, y: 0, w: 0, h: 0 },
  lastHello: null, wakeLock: null, retryTimer: null, name: '', failures: 0,
  accepted: false, // el ordenador ha aceptado el código en esta conexión
  settings: { stats: false, measure: false, fullscreen: false, smoothMs: 0 },
};
const sync = new ClockSync((msg) => send(msg));
const cursor = { w: 0, h: 0, x: 0, y: 0, visible: false };
const m = {
  frames: 0, bytes: 0, fps: 0, mbps: 0, t0: performance.now(), netMax: 0, // el peor del último segundo
  net: new Window(120), dec: new Window(120), e2e: new Window(240), host: null,
};

// ------------------------------------------------------------------ código y dispositivo

const normKey = (s) => s.toUpperCase().replace(/[^A-Z0-9]/g, '');
const formatKey = (s) => { const k = normKey(s); return k.length > 4 ? `${k.slice(0, 4)}-${k.slice(4, 8)}` : k; };

/** El QR del ordenador abre la página con el código en la dirección (#k=…): se guarda y se quita. */
function takeKeyFromUrl() {
  const k = new URLSearchParams(location.hash.slice(1)).get('k');
  if (k) {
    storage('key', normKey(k));
    history.replaceState(null, '', location.pathname + location.search);
  }
}

const savedKey = () => normKey(storage('key') || '');

/** Identificador de este dispositivo: con él, el ordenador recuerda sus ajustes (lado, modo…)
 *  y sabe que ya lo aceptaste si la conexión se corta y vuelve. */
function deviceId() {
  let id = storage('device') || '';
  if (!/^[A-Za-z0-9]{16,40}$/.test(id)) {
    id = [...crypto.getRandomValues(new Uint8Array(12))].map((b) => b.toString(16).padStart(2, '0')).join('');
    storage('device', id);
  }
  return id;
}

/** Nombre legible del dispositivo para el panel ("SM-F966B · Android · Chrome"). */
async function deviceName() {
  const ua = navigator.userAgent;
  let model = '';
  let platform = navigator.userAgentData?.platform || '';
  try {
    const v = await navigator.userAgentData?.getHighEntropyValues?.(['model', 'platform']);
    model = v?.model || '';
    platform = v?.platform || platform;
  } catch { /* opcional */ }
  if (!platform) {
    platform = /iPad/.test(ua) || (/Macintosh/.test(ua) && navigator.maxTouchPoints > 1) ? 'iPad'
      : /iPhone/.test(ua) ? 'iPhone' : /Android/.test(ua) ? 'Android'
        : /Windows/.test(ua) ? 'Windows' : /Mac/.test(ua) ? 'Mac' : /Linux/.test(ua) ? 'Linux' : '';
  }
  const browser = /Edg\//.test(ua) ? 'Edge' : /SamsungBrowser/.test(ua) ? 'Samsung Internet'
    : /Chrome\//.test(ua) ? 'Chrome' : /Firefox\//.test(ua) ? 'Firefox' : /Safari\//.test(ua) ? 'Safari' : '';
  return [model, platform, browser].filter(Boolean).join(' · ') || 'Dispositivo';
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

// El ordenador no responde a quien no debe (pantalla apagada, conexión no activa) y una
// dirección antigua (otra red) no llega a ningún sitio: en vez de quedarse en «Conectando…»,
// se explica qué pasa y se deja escribir el código, mientras se sigue reintentando.
const UNREACHABLE = 'No se puede conectar con el ordenador. Comprueba que la pantalla está encendida en su '
  + 'panel y que este dispositivo está en la misma red. Si el código ha cambiado, escanea el QR nuevo o escribe el código:';
const REJECTED = 'El ordenador ha cerrado la conexión. Si el código ha cambiado, escanea el QR nuevo o escribe el código:';

function connect() {
  st.want = true;
  clearTimeout(st.retryTimer);
  if (st.ws) { const old = st.ws; st.ws = null; old.close(); }
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(`${proto}://${location.host}/ws?role=viewer`);
  ws.binaryType = 'arraybuffer';
  st.ws = ws;
  st.accepted = false;
  let opened = false;
  if (st.failures === 0) showGate('Conectando…'); // tras un fallo se deja el aviso y el campo del código
  // Sin respuesta, el navegador puede tardar más de 20 s en rendirse.
  const giveUp = setTimeout(() => { if (ws.readyState === WebSocket.CONNECTING) ws.close(); }, 5000);
  ws.onopen = () => {
    opened = true;
    clearTimeout(giveUp);
    st.failures = 0;
    st.connected = true;
    st.lastHello = null;
    sync.start();
    sendHello();
    setStatus('Conectado. Esperando la imagen…');
  };
  ws.onmessage = (ev) => (typeof ev.data === 'string' ? onText(JSON.parse(ev.data)) : onFrame(ev.data));
  ws.onclose = () => {
    clearTimeout(giveUp);
    if (st.ws !== ws) return;
    st.ws = null;
    st.connected = false;
    sync.stop();
    resetDecoder();
    if (!st.want) return;
    if (opened && !st.accepted) {
      // El ordenador ha cerrado sin aceptar el código y su aviso no ha llegado: reintentar con
      // el mismo código no serviría de nada.
      stop(REJECTED, { askKey: true, retry: true });
    } else if (opened) {
      // Se ha cortado una conexión que funcionaba: se reintenta en seguida.
      st.failures = 0;
      showGate('Conexión perdida. Reintentando…');
      st.retryTimer = setTimeout(connect, 1000);
    } else {
      st.failures += 1;
      showGate(UNREACHABLE, { askKey: true });
      st.retryTimer = setTimeout(connect, Math.min(1000 * st.failures, 3000));
    }
  };
}

/** Conexión pedida por el usuario (o con un código nuevo): empieza mostrando «Conectando…». */
function reconnect() {
  st.failures = 0;
  connect();
}

/** Deja de intentar conectar y muestra el motivo (y, si hace falta, el campo del código). */
function stop(reason, { askKey = false, retry = false } = {}) {
  st.want = false;
  st.failures = 0;
  clearTimeout(st.retryTimer);
  if (st.ws) { const ws = st.ws; st.ws = null; ws.close(); }
  st.connected = false;
  st.stream = null;
  sync.stop();
  resetDecoder();
  st.last?.close();
  st.last = null;
  ui.cursor.hidden = true;
  ui.hud.hidden = true;
  ui.fsHint.hidden = true;
  st.wakeLock?.release?.().catch(() => {});
  st.wakeLock = null;
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  layout();
  showGate(reason, { askKey, retry });
}

/** Tamaño real de la pantalla del dispositivo; el ordenador decide la resolución final. */
function targetSize() {
  const dpr = window.devicePixelRatio || 1;
  return { w: Math.round(innerWidth * dpr), h: Math.round(innerHeight * dpr) };
}

// ------------------------------------------------------------------ frecuencia de pantalla
// El ordenador captura a la frecuencia real de la pantalla (120 Hz en el Fold) si el panel
// está en «Automático»: a más frecuencia, menos espera hasta la siguiente captura.

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
  if (rate !== displayHz) {
    displayHz = rate;
    if (st.connected) sendHello();
  }
}

function sendHello() {
  const { w, h } = targetSize();
  const hello = {
    t: 'hello', key: savedKey(), id: deviceId(), w, h, hz: displayHz,
    dpr: window.devicePixelRatio || 1, name: st.name,
  };
  const sig = JSON.stringify(hello);
  if (sig === st.lastHello) return;
  st.lastHello = sig;
  send(hello);
}

function onText(msg) {
  // Cualquier otra cosa que el reloj, el rechazo o la despedida: el código se ha aceptado.
  if (!['pong', 'auth', 'bye'].includes(msg.t)) st.accepted = true;
  switch (msg.t) {
    case 'pong': sync.onPong(msg); break;
    case 'config': configure(msg); break;
    case 'stats': m.host = msg; break;
    case 'settings': applySettings(msg); break;
    case 'cursor':
      if (smoothOn() && msg.ms) {
        smooth.cursors.push({ at: msg.ms - sync.offset, x: msg.x, y: msg.y, v: msg.v });
        scheduleSmooth();
      } else {
        Object.assign(cursor, { x: msg.x, y: msg.y, visible: msg.v });
        placeCursor();
      }
      break;
    case 'cursorShape': setCursorShape(msg); break;
    case 'auth':
      storage('key', '');
      stop('El código no es correcto. Escanea otra vez el código QR del ordenador o escribe su código:', { askKey: true });
      break;
    // Apagada o con código nuevo en el ordenador: el guardado ya no vale, se pide el nuevo.
    case 'bye':
      if (msg.askKey) storage('key', '');
      stop(msg.reason, { retry: msg.retry !== false, askKey: Boolean(msg.askKey) });
      break;
    case 'error': showToast(msg.message); break;
    case 'info': showToast(msg.message); break;
    // Hay que aceptar la conexión en el panel del ordenador.
    case 'wait': showGate(msg.message); break;
  }
}

/** Ajustes que manda el panel del ordenador. */
function applySettings(s) {
  const was = st.settings;
  st.settings = {
    stats: Boolean(s.stats), measure: Boolean(s.measure), fullscreen: Boolean(s.fullscreen),
    smoothMs: Math.max(0, Number(s.smoothMs) || 0),
  };
  if (!st.settings.measure) m.e2e.clear();
  if (was.smoothMs !== st.settings.smoothMs) { flushSmooth(); m.e2e.clear(); }
  if (was.fullscreen && !st.settings.fullscreen && isFullscreen()) {
    document.exitFullscreen?.().catch(() => {});
  }
  updateFullscreenHint();
  renderHud();
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
  // Los frames vienen en Annex B, con SPS/PPS dentro de cada keyframe (sin "description").
  const base = { codec: msg.codec, codedWidth: msg.width, codedHeight: msg.height, optimizeForLatency: true };
  let chosen = null;
  for (const hardwareAcceleration of ['prefer-hardware', 'no-preference']) {
    try {
      const cfg = { ...base, hardwareAcceleration };
      if ((await VideoDecoder.isConfigSupported(cfg)).supported) { chosen = cfg; break; }
    } catch { /* probar la siguiente */ }
  }
  if (!chosen) {
    st.configuring = false;
    showGate(`Este dispositivo no puede decodificar ${msg.codec}.`);
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
  hideGate();
  requestWakeLock();
  updateFullscreenHint();
}

function resetDecoder() {
  flushSmooth({ draw: false });
  if (st.decoder && st.decoder.state !== 'closed') {
    try { st.decoder.close(); } catch { /* ya cerrado */ }
  }
  st.decoder = null;
  st.pending.clear();
}

function onDecoderError(e) {
  console.error('Error del decodificador', e);
  if (!st.decoderConfig || !st.connected) return;
  // Se reconfigura y se pide un keyframe (el periódico solo llega cada 10 s).
  st.decoder = new VideoDecoder({ output: onDecoded, error: onDecoderError });
  st.decoder.configure(st.decoderConfig);
  st.waitingKey = true;
  st.pending.clear();
  send({ t: 'keyframe' });
}

function onFrame(buf) {
  const arrival = performance.now();
  const dv = new DataView(buf);
  if (dv.getUint8(0) !== 1) return;
  const item = {
    key: (dv.getUint8(1) & 1) === 1,
    seq: dv.getUint32(4, true),
    // Cuándo se capturó, en el reloj del ordenador: hora de salida menos lo que tardó en codificarse.
    captured: dv.getFloat64(8, true) - dv.getUint32(16, true),
    data: new Uint8Array(buf, HEADER),
  };
  m.bytes += buf.byteLength;
  if (sync.ready) {
    const net = arrival + sync.offset - dv.getFloat64(8, true);
    m.net.push(net);
    m.netMax = Math.max(m.netMax, net);
  }
  if (st.configuring) st.queue.push(item);
  else if (smoothOn()) hold(item);
  else decode(item);
}

function decode(item) {
  const d = st.decoder;
  if (!d || d.state !== 'configured') return;
  if (st.waitingKey) {
    if (!item.key) return;
    st.waitingKey = false;
  }
  st.pending.set(item.seq, { start: performance.now(), acked: item.acked });
  d.decode(new EncodedVideoChunk({ type: item.key ? 'key' : 'delta', timestamp: item.seq, data: item.data }));
}

function onDecoded(frame) {
  const now = performance.now();
  const seq = frame.timestamp;
  const p = st.pending.get(seq);
  st.pending.delete(seq);
  m.frames++;
  if (p) m.dec.push(now - p.start);
  if (!p?.acked) send({ t: 'ack', seq });
  if (st.pending.size > 240) st.pending.clear();
  show(frame);
}

function show(frame) {
  const r = st.rect;
  ctx.drawImage(frame, r.x, r.y, r.w, r.h);
  // Se guarda el último frame para repintar al girar o desplegar la pantalla.
  st.last?.close();
  st.last = frame;
  if (st.settings.measure && sync.ready) measureE2E(frame);
}

// ------------------------------------------------------------------ imagen más suave
// Con «Imagen más suave por Wi-Fi», cada imagen se decodifica y se muestra con el mismo
// retraso respecto a cuándo se capturó: el habitual más un colchón (25, 50 o 100 ms según el
// nivel elegido en el panel). Así un parón
// corto de la Wi-Fi se absorbe en vez de verse como un tirón. Se retienen las imágenes aún sin
// decodificar (pocos KB): las decodificadas ocupan memoria de la gráfica y el decodificador
// tiene muy pocas, así que retenerlas lo atascaba. El cursor va con el mismo retraso.

const smooth = { chunks: [], cursors: [], age: new Window(120), timer: 0 };

const smoothOn = () => st.settings.smoothMs > 0 && sync.ready;

/** Retraso con el que se decodifica cada imagen desde su captura: el habitual (mediana) más el colchón. */
const smoothDelay = () => (smooth.age.count ? smooth.age.pct(0.5) : 0) + st.settings.smoothMs;

function hold(item) {
  const at = item.captured - sync.offset; // en el reloj de este dispositivo
  smooth.age.push(performance.now() - at);
  // Se confirma al recibirla: lo que se retiene a propósito no es retraso de la red.
  send({ t: 'ack', seq: item.seq });
  item.acked = true;
  smooth.chunks.push({ item, at });
  scheduleSmooth();
}

/** Espera justo hasta la hora de lo siguiente retenido (con un temporizador: las animaciones
 *  del navegador pueden ir frenadas). */
function scheduleSmooth() {
  if (smooth.timer) return;
  const next = Math.min(smooth.chunks[0]?.at ?? Infinity, smooth.cursors[0]?.at ?? Infinity);
  if (next === Infinity) return;
  smooth.timer = setTimeout(smoothTick, Math.max(0, next + smoothDelay() - performance.now()));
}

function smoothTick() {
  smooth.timer = 0;
  const due = performance.now() - smoothDelay();
  while (smooth.chunks.length && (smooth.chunks[0].at <= due || smooth.chunks.length > 120)) {
    decode(smooth.chunks.shift().item);
  }
  let c = null;
  while (smooth.cursors.length && smooth.cursors[0].at <= due) c = smooth.cursors.shift();
  if (c) {
    Object.assign(cursor, { x: c.x, y: c.y, visible: c.v });
    placeCursor();
  }
  scheduleSmooth();
}

/** Suelta lo retenido: lo decodifica ya (al desactivar el modo) o lo descarta (al reconfigurar). */
function flushSmooth({ draw = true } = {}) {
  clearTimeout(smooth.timer);
  smooth.timer = 0;
  const chunks = smooth.chunks;
  smooth.chunks = [];
  if (draw) chunks.forEach((c) => decode(c.item));
  const c = smooth.cursors.pop();
  smooth.cursors = [];
  if (c && draw) {
    Object.assign(cursor, { x: c.x, y: c.y, visible: c.v });
    placeCursor();
  }
  smooth.age.clear();
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

const KIND = {
  pointerdown: 'down', pointermove: 'move', pointerup: 'up', pointercancel: 'cancel',
  pointerleave: 'leave', // el lápiz se aleja de la pantalla (deja de flotar)
};

function pointerMessage(e, kind) {
  const dpr = window.devicePixelRatio || 1;
  const r = st.rect;
  const msg = {
    t: 'pointer', e: kind, id: e.pointerId, type: e.pointerType, b: e.button,
    x: +((e.clientX * dpr - r.x) / r.w).toFixed(5),
    y: +((e.clientY * dpr - r.y) / r.h).toFixed(5),
    p: +e.pressure.toFixed(3),
  };
  if (e.pointerType === 'pen') {
    // Inclinación (-90..90 grados), giro sobre su eje (0..359) y botones: 2 = botón
    // lateral, 32 = goma de borrar.
    msg.tx = Math.round(e.tiltX || 0);
    msg.ty = Math.round(e.tiltY || 0);
    msg.tw = Math.round(e.twist || 0);
    msg.bt = e.buttons;
  }
  return msg;
}

function onPointer(e) {
  if (!st.connected || !st.stream) return;
  const kind = KIND[e.type];
  if (kind === 'leave' && e.pointerType !== 'pen') return;
  e.preventDefault();
  if (kind === 'down') canvas.setPointerCapture?.(e.pointerId);
  // El lápiz da muchas más muestras por segundo que fotogramas: se envían todas para que los
  // trazos salgan suaves y no a tramos rectos.
  const samples = e.pointerType === 'pen' && kind === 'move' && e.getCoalescedEvents ? e.getCoalescedEvents() : [];
  for (const s of samples.length ? samples : [e]) send(pointerMessage(s, kind));
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

function showGate(text, { askKey = false, retry = false } = {}) {
  ui.gate.hidden = false;
  setStatus(text);
  // Solo al aparecer: los reintentos de fondo no deben borrar lo que se está escribiendo.
  const opening = askKey && ui.keyForm.hidden;
  ui.keyForm.hidden = !askKey;
  ui.retry.hidden = !retry;
  if (opening) {
    ui.key.value = '';
    ui.key.focus();
  }
}

function hideGate() { ui.gate.hidden = true; }

// ------------------------------------------------------------------ pantalla completa
// El navegador solo deja ponerse a pantalla completa tras un toque: cuando el panel la pide,
// se muestra un aviso y ese toque no se envía al ordenador.

const isFullscreen = () => Boolean(document.fullscreenElement || document.webkitFullscreenElement)
  || matchMedia('(display-mode: fullscreen)').matches; // abierta desde la pantalla de inicio

const canFullscreen = () => Boolean(document.fullscreenEnabled || document.webkitFullscreenEnabled);

function updateFullscreenHint() {
  ui.fsHint.hidden = !(st.settings.fullscreen && st.stream && !isFullscreen() && canFullscreen());
}

// Ni la pantalla completa ni el bloqueo de suspensión deben retrasar nada:
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
  const show = st.settings.stats && st.connected && st.stream;
  ui.hud.hidden = !show;
  if (!show) return;
  const s = st.stream;
  const h = m.host;
  const hw = st.decoderConfig?.hardwareAcceleration === 'prefer-hardware' ? 'HW' : 'auto';
  const mode = s.mode === 'extend' ? 'extendida' : 'espejo';
  const lines = [
    `${s.width}×${s.height} · ${s.fps} fps · ${mode} · ${s.codec} · ${hw}${smoothOn() ? ` · suave (${fmt(smoothDelay(), 0)} ms desde la captura)` : ''}`,
    `Recibido   ${fmt(m.fps, 0)} fps · ${fmt(m.mbps)} Mbps`,
    `Red        ${fmt(m.net.avg)} ms (p95 ${fmt(m.net.pct(0.95))}) · RTT ${fmt(sync.rtt)}`,
    `Decodif.   ${fmt(m.dec.avg)} ms (p95 ${fmt(m.dec.pct(0.95))})`,
  ];
  if (h) lines.push(`Ordenador  ${h.encFps} fps · ${h.dropped} descartados · captura→cod. ${fmt(h.encMs50)} ms (p95 ${fmt(h.encMs95)})`);
  if (st.settings.measure) {
    lines.push(m.e2e.count
      ? `TOTAL      ${fmt(m.e2e.avg, 0)} ms (mín ${fmt(m.e2e.min, 0)} · p95 ${fmt(m.e2e.pct(0.95), 0)})`
      : 'TOTAL      sin señal: abre la sonda en el ordenador y déjala visible');
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
      netMax: Math.round(m.netMax),
    });
  }
  m.netMax = 0;
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

document.addEventListener('fullscreenchange', updateFullscreenHint);
document.addEventListener('webkitfullscreenchange', updateFullscreenHint);
ui.fsHint.addEventListener('click', (e) => {
  e.preventDefault();
  enterFullscreen();
  ui.fsHint.hidden = true;
});

ui.keyForm.addEventListener('submit', (e) => {
  e.preventDefault();
  const k = normKey(ui.key.value);
  if (k.length !== 8) {
    setStatus('El código tiene 8 caracteres: lo verás en el panel del ordenador.');
    return;
  }
  storage('key', k);
  reconnect();
});
ui.key.addEventListener('blur', () => { ui.key.value = formatKey(ui.key.value); });
ui.retry.addEventListener('click', () => reconnect());

// ------------------------------------------------------------------ inicio

takeKeyFromUrl();
layout();
detectRefresh();
ui.host.textContent = location.host;
const problem = supportProblem();
// Si el QR nuevo (cada encendido trae otro código) se abre en esta misma pestaña, el
// navegador solo cambia el "#k=..." sin recargar la página.
addEventListener('hashchange', () => {
  const k = new URLSearchParams(location.hash.slice(1)).get('k');
  if (!k || problem) return;
  const changed = normKey(k) !== savedKey();
  takeKeyFromUrl();
  if (!changed && st.want && st.failures === 0) return; // ya conectado (o conectando) con ese código
  reconnect();
});

deviceName().then((name) => {
  st.name = name;
  if (problem) {
    ui.problem.innerHTML = problem;
    ui.problem.hidden = false;
    showGate('No se puede mostrar la pantalla en este navegador.');
  } else if (savedKey().length === 8) {
    connect(); // abierto desde el QR (o con el código ya guardado): se conecta solo
  } else {
    showGate('Escanea el código QR que aparece en el ordenador, o escribe su código:', { askKey: true });
  }
});
