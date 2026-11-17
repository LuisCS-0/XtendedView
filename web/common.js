// Utilidades compartidas por el visor y la sonda de latencia.

/** Sincroniza el reloj local con el del portátil (estilo NTP: se usa la muestra de menor RTT). */
export class ClockSync {
  constructor(send) {
    this.send = send;
    this.samples = [];
    this.offset = 0;
    this.rtt = NaN;
    this.minRtt = NaN;
    this.ready = false;
    this.timer = null;
  }
  start() {
    this.stop();
    // Al reconectar puede ser otro proceso con otro origen de reloj: se empieza de cero.
    this.samples = [];
    this.ready = false;
    let n = 0;
    const tick = () => {
      this.send({ t: 'ping', c: performance.now() });
      n++;
      this.timer = setTimeout(tick, n < 10 ? 100 : 1000);
    };
    tick();
  }
  stop() {
    clearTimeout(this.timer);
    this.timer = null;
  }
  onPong(m) {
    const c1 = performance.now();
    const rtt = c1 - m.c;
    this.samples.push({ rtt, offset: m.s - (m.c + c1) / 2 });
    if (this.samples.length > 30) this.samples.shift();
    const best = this.samples.reduce((a, b) => (b.rtt < a.rtt ? b : a));
    this.offset = best.offset;
    this.minRtt = best.rtt;
    this.rtt = rtt;
    this.ready = true;
  }
  /** Hora del portátil en ms (mismo reloj que las marcas de los frames). */
  now() {
    return performance.now() + this.offset;
  }
}

/** Ventana deslizante de medidas con media, mínimo y percentil 95. */
export class Window {
  constructor(size = 120) {
    this.size = size;
    this.values = [];
  }
  push(v) {
    if (!Number.isFinite(v)) return;
    this.values.push(v);
    if (this.values.length > this.size) this.values.shift();
  }
  get count() { return this.values.length; }
  get avg() {
    return this.values.length ? this.values.reduce((a, b) => a + b, 0) / this.values.length : NaN;
  }
  get min() { return this.values.length ? Math.min(...this.values) : NaN; }
  pct(p) {
    if (!this.values.length) return NaN;
    const s = [...this.values].sort((a, b) => a - b);
    return s[Math.min(s.length - 1, Math.floor(p * s.length))];
  }
  clear() { this.values = []; }
}

export function b64ToBytes(b64) {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

// ------------------------------------------------------------------ código de tiempo
// La sonda (una página abierta en el portátil) dibuja la hora del portátil como una fila
// de 32 celdas blancas/negras (24 bits de milisegundos + 8 de comprobación) entre una
// celda blanca y otra negra de referencia, con una barra magenta encima y otra debajo.
// El visor busca las barras magenta en el vídeo, lee las celdas y compara esa hora con la
// actual: eso es la latencia de punta a punta. La sonda puede estar en cualquier parte
// de la pantalla, no hace falta ponerla a pantalla completa.

export const TC_CELLS = 34;
export const TC_BAND = 0.08; // alto de la franja en la sonda, en fracción de su altura

function checksum(v) {
  return (((v & 0xff) + ((v >> 8) & 0xff) + ((v >> 16) & 0xff)) & 0xff) ^ 0xa5;
}

export function drawTimecode(ctx, x, y, w, h, ms) {
  const v = Math.floor(ms) & 0xffffff;
  const word = v * 256 + checksum(v);
  const bar = Math.max(3, Math.round(h * 0.25));
  const cy = y + bar;
  const chh = h - 2 * bar;
  const cell = w / TC_CELLS;
  ctx.fillStyle = '#ff00ff';
  ctx.fillRect(x, y, w, bar);
  ctx.fillRect(x, y + h - bar, w, bar);
  ctx.fillStyle = '#000';
  ctx.fillRect(x, cy, w, chh);
  ctx.fillStyle = '#fff';
  ctx.fillRect(x, cy, Math.ceil(cell), chh);
  for (let i = 0; i < 32; i++) {
    if (Math.floor(word / 2 ** (31 - i)) % 2) {
      ctx.fillRect(x + Math.floor((i + 1) * cell), cy, Math.ceil(cell), chh);
    }
  }
}

/**
 * Busca la sonda en una imagen RGBA: la fila con más magenta, su tramo continuo más largo
 * y, en vertical, la otra barra magenta al otro lado de la fila de celdas.
 * Devuelve { x0, x1, yc } (extremos de la fila de celdas y su centro) o null.
 */
export function findTimecodeBand(rgba, w, h) {
  const mag = (x, y) => {
    const o = (y * w + x) * 4;
    return rgba[o] > 170 && rgba[o + 2] > 170 && rgba[o + 1] < 110;
  };
  let bestY = -1, best = 0;
  for (let y = 0; y < h; y++) {
    let n = 0;
    for (let x = 0; x < w; x++) if (mag(x, y)) n++;
    if (n > best) { best = n; bestY = y; }
  }
  if (best < TC_CELLS) return null;
  let xa = 0, xb = -1, start = -1, last = -10;
  for (let x = 0; x < w; x++) {
    if (!mag(x, bestY)) continue;
    if (x - last > 3) start = x; // se toleran huecos de hasta 2 px
    last = x;
    if (x - start > xb - xa) { xa = start; xb = x; }
  }
  if (xb - xa < TC_CELLS) return null;
  const xm = (xa + xb) >> 1;
  let ya = bestY, yb = bestY;
  while (ya > 0 && mag(xm, ya - 1)) ya--;
  while (yb < h - 1 && mag(xm, yb + 1)) yb++;
  const maxGap = 4 * (yb - ya + 1) + 4;
  for (let y = yb + 1; y < h && y - yb <= maxGap; y++) {
    if (mag(xm, y)) return { x0: xa, x1: xb, yc: (yb + y) / 2 };
  }
  for (let y = ya - 1; y >= 0 && ya - y <= maxGap; y--) {
    if (mag(xm, y)) return { x0: xa, x1: xb, yc: (ya + y) / 2 };
  }
  return null;
}

/** rgba: una fila de píxeles que cruza la franja. Devuelve los 24 bits de ms o null. */
export function readTimecode(rgba, width) {
  const lum = (i) => {
    const x = Math.min(width - 1, Math.floor(((i + 0.5) * width) / TC_CELLS));
    const o = x * 4;
    return (rgba[o] + rgba[o + 1] + rgba[o + 2]) / 3;
  };
  if (lum(0) < 170 || lum(TC_CELLS - 1) > 85) return null;
  let word = 0;
  for (let i = 0; i < 32; i++) word = word * 2 + (lum(i + 1) > 128 ? 1 : 0);
  const v = Math.floor(word / 256);
  return word % 256 === checksum(v) ? v : null;
}

/** Diferencia (ms) entre la hora actual y un código de 24 bits, o null si no es plausible. */
export function timecodeAge(nowMs, v) {
  let d = (Math.floor(nowMs) & 0xffffff) - v;
  if (d < 0) d += 0x1000000;
  return d < 5000 ? d : null;
}

export function storage(key, value) {
  try {
    if (value === undefined) return localStorage.getItem(key);
    localStorage.setItem(key, value);
  } catch {
    return null;
  }
  return value;
}
