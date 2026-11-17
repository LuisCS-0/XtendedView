// Sonda de latencia: se abre en el portátil, en cualquier ventana visible. Dibuja la hora
// del portátil como código de barras en la franja superior (lo lee el visor) y como reloj
// grande (para comprobarlo con una foto de las dos pantallas).
import { ClockSync, drawTimecode, TC_BAND } from './common.js';

const canvas = document.getElementById('probe');
const ctx = canvas.getContext('2d', { alpha: false });
const info = document.getElementById('info');
let ws = null;
const sync = new ClockSync((m) => ws?.readyState === WebSocket.OPEN && ws.send(JSON.stringify(m)));

function connect() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  ws = new WebSocket(`${proto}://${location.host}/ws?role=probe`);
  ws.onopen = () => sync.start();
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.t === 'pong') sync.onPong(m);
  };
  ws.onclose = () => {
    sync.stop();
    sync.ready = false;
    info.textContent = 'Sin conexión con el host. Reintentando…';
    setTimeout(connect, 1000);
  };
}

function resize() {
  const dpr = window.devicePixelRatio || 1;
  canvas.width = Math.round(innerWidth * dpr);
  canvas.height = Math.round(innerHeight * dpr);
}

function draw() {
  const w = canvas.width;
  const h = canvas.height;
  ctx.fillStyle = '#20232a';
  ctx.fillRect(0, 0, w, h);
  if (sync.ready) {
    const t = sync.now();
    const band = Math.max(24, Math.round(h * TC_BAND));
    drawTimecode(ctx, 0, 0, w, band, t);
    // Barra que da una vuelta por segundo: permite ver la fluidez a simple vista.
    const top = band + h * 0.08;
    ctx.fillStyle = '#4f8cff';
    ctx.fillRect(((t % 1000) / 1000) * w - w * 0.01, top, w * 0.02, h * 0.12);
    // Reloj grande en ms.
    ctx.fillStyle = '#fff';
    ctx.font = `bold ${Math.round(h * 0.2)}px ui-monospace, Consolas, monospace`;
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(String(Math.floor(t) % 100000).padStart(5, '0'), w / 2, h * 0.55);
    const text = 'Sonda activa: déjala visible y marca «Medir latencia total» en el visor.'
      + ` · RTT con el host ${sync.minRtt.toFixed(1)} ms`;
    if (info.textContent !== text) info.textContent = text;
  }
  requestAnimationFrame(draw);
}

addEventListener('resize', resize);
resize();
connect();
requestAnimationFrame(draw);
