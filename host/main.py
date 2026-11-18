"""XtendedView - prototipo en Python (host Windows). La versión principal es la de Rust (host-rs).

captura (Desktop Duplication) -> H.264 (QuickSync) -> WebSocket sobre HTTPS -> navegador (WebCodecs)

Uso:  python host/main.py [--encoder qsv|x264] [--pipeline gpu|cpu] [--monitor N] ...
"""
import argparse
import asyncio
import base64
import ipaddress
import json
import logging
import secrets
import socket
import ssl
import struct
import sys
import time
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

import certs
import encoder
import winapi
from websocket import OP_TEXT, ConnectionClosed, WebSocket, accept_key

ROOT = Path(__file__).resolve().parent.parent
WEB = ROOT / "web"
DATA = ROOT / "certs"

log = logging.getLogger("host")

# Cabecera binaria de cada frame (little-endian, 20 bytes):
# tipo u8 (1 = vídeo) | flags u8 (bit0 = keyframe) | reservado u16 | seq u32 |
# hora del host al recibirlo del codificador f64 (ms) | pts u32 (ms)
FRAME_HEADER = struct.Struct("<BBHIdI")

MIME = {".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8",
        ".css": "text/css; charset=utf-8", ".json": "application/json",
        ".svg": "image/svg+xml", ".png": "image/png", ".ico": "image/x-icon",
        ".webmanifest": "application/manifest+json"}
ROUTES = {"/": "index.html", "/probe": "probe.html", "/setup": "setup.html"}
# Lo único que se sirve sin cifrar a otros dispositivos: lo necesario para instalar el certificado.
PLAIN_HTTP_PATHS = {"/setup", "/ca.crt", "/info", "/style.css", "/favicon.ico", "/icon-192.png"}
KEY_ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789"


def now_ms() -> float:
    return time.perf_counter_ns() / 1e6


def pairing_key() -> str:
    """Código de emparejamiento persistente (se guarda junto a los certificados)."""
    path = DATA / "pairing.txt"
    if path.exists():
        return path.read_text().strip()
    key = "".join(secrets.choice(KEY_ALPHABET) for _ in range(8))
    path.write_text(key)
    return key


def normalize_key(k: str) -> str:
    return "".join(ch for ch in str(k).upper() if ch.isalnum())


def percentile(values: list[float], p: float) -> float:
    if not values:
        return 0.0
    s = sorted(values)
    return s[min(len(s) - 1, int(p * len(s)))]


class Streamer:
    """Gestiona el codificador y el único visor activo (uno nuevo sustituye al anterior)."""

    def __init__(self, args: argparse.Namespace, key: str):
        self.args = args
        self.key = key
        self.ffmpeg = encoder.find_ffmpeg(args.ffmpeg)
        self.injector = winapi.InputInjector()
        self.viewer: WebSocket | None = None
        self.task: asyncio.Task | None = None
        self.settings: encoder.EncoderSettings | None = None
        self.last_client_log = 0.0
        self._reset_flow()

    # ------------------------------------------------------------ ciclo de vida

    def _reset_flow(self) -> None:
        self.seq = self.last_sent = self.acked = 0
        self.waiting_key = False
        self.win = {"frames": 0, "sent": 0, "dropped": 0, "bytes": 0, "keys": 0}
        self.min_lag: float | None = None
        self.lags: list[float] = []

    def monitor(self) -> winapi.Monitor:
        mons = winapi.list_monitors()
        return mons[min(self.args.monitor, len(mons) - 1)]

    def make_settings(self, mon: winapi.Monitor, p: dict) -> encoder.EncoderSettings:
        def num(name, default, lo, hi, cast=float):
            try:
                v = cast(p.get(name, default))
            except (TypeError, ValueError):
                v = default
            return min(max(v, lo), hi)

        req_w, req_h = num("w", 1920, 160, 4096, int), num("h", 1080, 160, 4096, int)
        # Modo espejo: misma proporción que el monitor, nunca se escala hacia arriba.
        scale = min(req_w / mon.w, req_h / mon.h, 1.0)
        return encoder.EncoderSettings(
            monitor=self.args.monitor,
            width=max(160, int(mon.w * scale) // 2 * 2),
            height=max(160, int(mon.h * scale) // 2 * 2),
            fps=num("fps", 60, 10, 120, int),
            bitrate_mbps=num("bitrate", 20, 1, 100),
            gop_seconds=self.args.gop,
            encoder=self.args.encoder,
            pipeline=self.args.pipeline)

    async def on_message(self, ws: WebSocket, msg: dict) -> None:
        t = msg.get("t")
        if t == "hello":
            if not secrets.compare_digest(normalize_key(msg.get("key", "")), self.key):
                log.warning("Código de emparejamiento incorrecto")
                await asyncio.sleep(1)  # frena intentos por fuerza bruta
                ws.send_text(json.dumps({"t": "auth", "ok": False}))
                ws.close()
                return
            await self.attach(ws, msg)
        elif ws is not self.viewer:
            return
        elif t == "ack":
            self.acked = max(self.acked, int(msg.get("seq", 0)))
        elif t == "pointer":
            self.injector.pointer(msg)
        elif t == "wheel":
            self.injector.wheel(float(msg.get("dx", 0)), float(msg.get("dy", 0)))
        elif t == "stats":
            self.log_client(msg)

    async def attach(self, ws: WebSocket, hello: dict) -> None:
        if self.viewer is not None and self.viewer is not ws:
            old = self.viewer
            try:
                old.send_text(json.dumps({"t": "bye", "reason": "Otro dispositivo se ha conectado como pantalla."}))
            except ConnectionClosed:
                pass
            old.close()
            await self.stop()
        self.viewer = ws
        mon = self.monitor()
        settings = self.make_settings(mon, hello)
        if settings == self.settings and self.task and not self.task.done():
            return  # nada que cambiar
        await self.stop()
        self.injector.monitor = mon
        self.settings = settings
        self._reset_flow()
        self.task = asyncio.create_task(self.run(ws, settings, mon))

    async def detach(self, ws: WebSocket) -> None:
        if self.viewer is ws:
            self.viewer = None
            await self.stop()
            self.settings = None
            self.injector.release_all()

    async def stop(self) -> None:
        if self.task:
            self.task.cancel()
            try:
                await self.task
            except BaseException:
                pass
            self.task = None

    # ------------------------------------------------------------ codificador

    async def run(self, ws: WebSocket, s: encoder.EncoderSettings, mon: winapi.Monitor) -> None:
        cmd = encoder.build_command(self.ffmpeg, s)
        log.info("Codificando %dx%d a %d fps, %g Mbps (%s, escalado %s) desde %s %dx%d",
                 s.width, s.height, s.fps, s.bitrate_mbps, s.encoder, s.pipeline, mon.name, mon.w, mon.h)
        log.debug(" ".join(cmd))
        proc = await asyncio.create_subprocess_exec(
            *cmd, stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        err_task = asyncio.create_task(self._pump_stderr(proc))
        ended = False  # ffmpeg se ha parado solo (error), no porque lo paremos nosotros
        try:
            async for pkt in encoder.read_flv(proc.stdout):
                t = now_ms()
                if pkt.kind == "config":
                    ws.send_text(json.dumps({
                        "t": "config", "codec": encoder.avc_codec_string(pkt.data),
                        "description": base64.b64encode(pkt.data).decode(),
                        "width": s.width, "height": s.height, "fps": s.fps,
                        "bitrate": s.bitrate_mbps, "encoder": s.encoder, "pipeline": s.pipeline}))
                else:
                    self._on_frame(ws, pkt, t)
        except (asyncio.IncompleteReadError, ValueError):
            ended = True
        except ConnectionClosed:
            pass
        finally:
            if ended:
                try:  # dejar que termine y que se lean sus últimos mensajes de error
                    await asyncio.wait_for(asyncio.gather(proc.wait(), err_task), 2)
                except (asyncio.TimeoutError, Exception):
                    pass
            if proc.returncode is None:
                proc.kill()
                await proc.wait()
            err_task.cancel()
        if ended and not ws.closed:
            log.error("El codificador ha terminado (código %s)", proc.returncode)
            try:
                ws.send_text(json.dumps({"t": "error", "message": "El codificador se ha detenido. Revisa la consola del portátil."}))
            except ConnectionClosed:
                pass

    async def _pump_stderr(self, proc: asyncio.subprocess.Process) -> None:
        while line := await proc.stderr.readline():
            text = line.decode(errors="replace").rstrip()
            if text and "monotonically" not in text:
                log.warning("ffmpeg: %s", text)

    def _on_frame(self, ws: WebSocket, pkt: encoder.Packet, t: float) -> None:
        self.seq += 1
        w = self.win
        w["frames"] += 1
        # Variación del retardo del codificador respecto al frame más rápido visto.
        lag = t - pkt.pts_ms
        if self.min_lag is None or lag < self.min_lag:
            self.min_lag = lag
        self.lags.append(lag - self.min_lag)

        # Control de flujo: si hay demasiados frames enviados sin confirmar (red o
        # decodificador saturados), se descartan hasta el siguiente keyframe en vez de
        # acumular retraso.
        limit = max(3, round(self.settings.fps * self.args.max_inflight_ms / 1000))
        congested = self.last_sent - self.acked > limit or ws.buffered() > 4_000_000
        if (self.waiting_key and not pkt.key) or congested:
            self.waiting_key = True
            w["dropped"] += 1
            return
        self.waiting_key = False
        header = FRAME_HEADER.pack(1, int(pkt.key), 0, self.seq, t, pkt.pts_ms & 0xFFFFFFFF)
        ws.send_binary(header, pkt.data)
        self.last_sent = self.seq
        w["sent"] += 1
        w["bytes"] += len(pkt.data)
        w["keys"] += int(pkt.key)

    # ------------------------------------------------------------ estadísticas

    def host_stats(self) -> dict:
        w, lags = self.win, self.lags
        out = {"t": "stats", "encFps": w["frames"], "sentFps": w["sent"], "dropped": w["dropped"],
               "keys": w["keys"], "mbps": round(w["bytes"] * 8 / 1e6, 2),
               "inflight": self.last_sent - self.acked,
               "encJitter50": round(percentile(lags, 0.5), 1),
               "encJitter95": round(percentile(lags, 0.95), 1)}
        self.win = {k: 0 for k in w}
        self.lags = []
        return out

    def log_client(self, m: dict) -> None:
        if time.monotonic() - self.last_client_log < 5:
            return
        self.last_client_log = time.monotonic()
        e2e = f", total {m['e2e']:.0f} ms (p95 {m['e2e95']:.0f})" if m.get("e2e") else ""
        log.info("Visor: %s fps, %.1f Mbps, red %.1f ms, decodificación %.1f ms, RTT %.1f ms%s",
                 m.get("fps"), m.get("mbps", 0), m.get("net", 0), m.get("dec", 0), m.get("rtt", 0), e2e)

    async def background(self) -> None:
        tick = 0
        while True:
            await asyncio.sleep(0.05)
            self.injector.keepalive()
            tick += 1
            if tick % 20 == 0 and self.viewer is not None and self.task:
                try:
                    self.viewer.send_text(json.dumps(self.host_stats()))
                except ConnectionClosed:
                    pass


class Server:
    def __init__(self, args: argparse.Namespace, streamer: Streamer, cm: certs.CertManager):
        self.args = args
        self.streamer = streamer
        self.certs = cm
        self.ssl = ssl.create_default_context(ssl.Purpose.CLIENT_AUTH)
        self.ssl.load_cert_chain(cm.crt, cm.key)
        self.ca_fingerprint = cm.fingerprint()
        self.cert_warned: set[str] = set()

    # Cada puerto acepta HTTPS y HTTP: se mira el primer byte (0x16 = inicio de TLS). Así,
    # si alguien escribe la dirección sin "https://", recibe una redirección en vez de una
    # conexión cortada sin explicación.

    def bind(self, port: int) -> socket.socket:
        sock = socket.create_server((self.args.bind, port))
        sock.setblocking(False)
        return sock

    async def accept_loop(self, sock: socket.socket) -> None:
        loop = asyncio.get_running_loop()
        port = sock.getsockname()[1]
        while True:
            conn, addr = await loop.sock_accept(sock)
            asyncio.create_task(self._dispatch_safe(conn, addr[0], port))

    async def _dispatch_safe(self, conn: socket.socket, peer: str, port: int) -> None:
        try:
            await self.dispatch(conn, peer, port)
        except Exception:
            log.exception("Error atendiendo una conexión de %s", peer)
            conn.close()

    async def dispatch(self, conn: socket.socket, peer: str, port: int) -> None:
        loop = asyncio.get_running_loop()
        local = ipaddress.ip_address(peer).is_loopback
        try:
            conn.setblocking(True)
            conn.settimeout(10)
            first = await loop.run_in_executor(None, conn.recv, 1, socket.MSG_PEEK)
            conn.settimeout(None)
            conn.setblocking(False)
        except OSError as e:
            if not local:
                log.info("Conexión de %s al puerto %d sin datos (%s)", peer, port, e)
            conn.close()
            return
        if not first:
            conn.close()
            return
        tls = first[0] == 0x16
        log.debug("Conexión de %s al puerto %d (%s)", peer, port, "HTTPS" if tls else "HTTP")
        reader = asyncio.StreamReader(limit=1 << 16)
        protocol = asyncio.StreamReaderProtocol(reader)
        try:
            tls_args = {"ssl": self.ssl, "ssl_handshake_timeout": 15} if tls else {}
            transport, _ = await loop.connect_accepted_socket(lambda: protocol, conn, **tls_args)
        except (ssl.SSLError, OSError, asyncio.TimeoutError) as e:
            # Chrome abre varias conexiones en paralelo y rechaza algunas aunque ya se haya
            # aceptado el aviso: se avisa una sola vez por dispositivo.
            if peer not in self.cert_warned:
                self.cert_warned.add(peer)
                log.info("%s ha rechazado el certificado (%s). Es normal hasta que se acepte el aviso "
                         "del navegador o se instale el certificado desde /setup.",
                         peer, str(e)[:60] or type(e).__name__)
            conn.close()
            return
        writer = asyncio.StreamWriter(transport, protocol, reader, loop)
        await self.handle(reader, writer, tls, peer, local)

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter,
                     tls: bool, peer: str, local: bool) -> None:
        try:
            try:
                head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 10)
            except (asyncio.IncompleteReadError, asyncio.LimitOverrunError, asyncio.TimeoutError,
                    ConnectionError, ssl.SSLError):
                return
            lines = head.decode("latin-1").split("\r\n")
            parts = lines[0].split(" ")
            if len(parts) < 3:
                return
            method, url = parts[0], urlsplit(parts[1])
            headers = {}
            for line in lines[1:]:
                k, sep, v = line.partition(":")
                if sep:
                    headers[k.strip().lower()] = v.strip()
            if not local:
                log.info("%s %s %s%s", peer, method, url.path, "" if tls else " (sin cifrar)")
            # Fuera de este portátil, todo salvo la página de configuración va por HTTPS:
            # la imagen de la pantalla nunca viaja sin cifrar por la red.
            if not tls and not local and url.path not in PLAIN_HTTP_PATHS:
                host = headers.get("host", "").rsplit(":", 1)[0] or certs.usable_ips()[0]
                target = f"https://{host}:{self.args.port}{url.path or '/'}"
                if url.query:
                    target += "?" + url.query
                self.respond(writer, 302, b"", "text/plain", {"Location": target})
            elif url.path == "/ws" and headers.get("upgrade", "").lower() == "websocket":
                await self.websocket(reader, writer, headers, parse_qs(url.query), peer)
            else:
                self.http(writer, method, url.path)
            await writer.drain()
        except (ConnectionError, ssl.SSLError):
            pass
        except Exception:
            log.exception("Error atendiendo una conexión")
        finally:
            if not writer.is_closing():
                writer.close()

    def respond(self, writer, status: int, body: bytes, ctype: str, extra: dict | None = None,
                head_only: bool = False) -> None:
        reason = {200: "OK", 302: "Found", 400: "Bad Request", 403: "Forbidden", 404: "Not Found",
                  405: "Method Not Allowed"}[status]
        hdrs = {"Content-Type": ctype, "Content-Length": str(len(body)),
                "Cache-Control": "no-store", "Connection": "close",
                "X-Content-Type-Options": "nosniff", **(extra or {})}
        writer.write(f"HTTP/1.1 {status} {reason}\r\n".encode()
                     + "".join(f"{k}: {v}\r\n" for k, v in hdrs.items()).encode() + b"\r\n")
        if not head_only:
            writer.write(body)

    def http(self, writer, method: str, path: str) -> None:
        if method not in ("GET", "HEAD"):
            return self.respond(writer, 405, b"", "text/plain")
        head = method == "HEAD"
        if path == "/ca.crt":
            return self.respond(writer, 200, self.certs.ca_crt.read_bytes(), "application/x-x509-ca-cert",
                                {"Content-Disposition": 'attachment; filename="xtendedview-ca.crt"'}, head)
        if path == "/info":
            # Sin cifrar y sin código: nada del nombre del equipo ni de sus otras direcciones.
            body = json.dumps({"httpsPort": self.args.port, "httpPort": self.args.http_port,
                               "caFingerprint": self.ca_fingerprint}).encode()
            return self.respond(writer, 200, body, "application/json", head_only=head)
        name = ROUTES.get(path, path.lstrip("/"))
        file = (WEB / name).resolve()
        if WEB not in file.parents or not file.is_file():
            return self.respond(writer, 404, b"No encontrado", "text/plain; charset=utf-8", head_only=head)
        self.respond(writer, 200, file.read_bytes(), MIME.get(file.suffix, "application/octet-stream"),
                     head_only=head)

    async def websocket(self, reader, writer, headers: dict, query: dict, peer: str) -> None:
        origin, host = headers.get("origin"), headers.get("host", "")
        # Solo se aceptan conexiones desde nuestras propias páginas (evita que otra web
        # abierta en el portátil se conecte a localhost).
        if origin and urlsplit(origin).netloc != host:
            log.warning("WebSocket rechazado por origen %s", origin)
            return self.respond(writer, 403, b"", "text/plain")
        key = headers.get("sec-websocket-key")
        if not key:
            return self.respond(writer, 400, b"", "text/plain")
        writer.write(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                      f"Connection: Upgrade\r\nSec-WebSocket-Accept: {accept_key(key)}\r\n\r\n").encode())
        sock = writer.get_extra_info("socket")
        if sock is not None:
            try:
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            except OSError:
                pass
        ws = WebSocket(reader, writer)
        role = query.get("role", ["viewer"])[0]
        log.info("Conectado %s (%s)", peer, role)
        try:
            while True:
                op, data = await ws.recv()
                if op != OP_TEXT:
                    continue
                try:
                    msg = json.loads(data)
                except ValueError:
                    continue
                if msg.get("t") == "ping":
                    ws.send_text(json.dumps({"t": "pong", "c": msg.get("c"), "s": now_ms()}))
                elif role == "viewer":
                    await self.streamer.on_message(ws, msg)
        except (ConnectionClosed, asyncio.IncompleteReadError, ConnectionError, ssl.SSLError):
            pass
        finally:
            log.info("Desconectado %s (%s)", peer, role)
            if role == "viewer":
                await self.streamer.detach(ws)
            ws.close()

    async def watch_ips(self) -> None:
        """Si aparece una IP nueva (p. ej. al activar el hotspot), se reemite el certificado."""
        while True:
            await asyncio.sleep(5)
            try:
                if self.certs.ensure():
                    self.ssl.load_cert_chain(self.certs.crt, self.certs.key)
                    log.info("Certificado actualizado. Direcciones: %s", ", ".join(certs.usable_ips()))
            except Exception as e:
                log.warning("No se pudo actualizar el certificado: %s", e)


def print_banner(args, mon: winapi.Monitor, key: str) -> None:
    ips = certs.usable_ips()
    pretty_key = f"{key[:4]}-{key[4:]}"
    print("=" * 66)
    print(" XtendedView - prototipo en Python")
    print("=" * 66)
    print(f" Monitor capturado : {mon.name} {mon.w}x{mon.h} (modo espejo)")
    print(f" Codificador       : {args.encoder} (escalado {args.pipeline})")
    print(f" Codigo            : {pretty_key}")
    print()
    print(" En el movil o la tablet (misma Wi-Fi o hotspot del portatil):")
    for ip in ips:
        tag = "  (hotspot)" if ip in certs.HOTSPOT_IPS else ""
        print(f"   Pantalla : https://{ip}:{args.port}/#k={key}{tag}")
        print(f"   1a vez   : http://{ip}:{args.http_port}/setup")
    print()
    print(" Medir latencia: abre esto en el portatil y dejalo visible:")
    print(f"   http://localhost:{args.http_port}/probe")
    print("=" * 66)


async def main_async(args: argparse.Namespace) -> None:
    winapi.enable_dpi_awareness()
    DATA.mkdir(exist_ok=True)
    cm = certs.CertManager(DATA)
    cm.ensure()
    key = pairing_key()
    streamer = Streamer(args, key)
    server = Server(args, streamer, cm)
    socks = [server.bind(args.port), server.bind(args.http_port)]
    print_banner(args, streamer.monitor(), key)
    await asyncio.gather(*(server.accept_loop(s) for s in socks),
                         streamer.background(), server.watch_ips())


def main() -> None:
    p = argparse.ArgumentParser(description="Host de la prueba de concepto de pantalla secundaria")
    p.add_argument("--port", type=int, default=8443, help="puerto HTTPS (visor)")
    p.add_argument("--http-port", type=int, default=8080, help="puerto HTTP (configuración y localhost)")
    p.add_argument("--bind", default="0.0.0.0")
    p.add_argument("--monitor", type=int, default=0, help="monitor a capturar (0 = principal)")
    p.add_argument("--encoder", choices=["qsv", "x264"], default="qsv")
    # Con FFmpeg 6.0 la ruta "gpu" no evita copias (descarga + resubida) y es más lenta.
    p.add_argument("--pipeline", choices=["gpu", "cpu"], default="cpu",
                   help="dónde se escala la imagen antes de codificar (solo qsv)")
    p.add_argument("--gop", type=float, default=1.0, help="segundos entre keyframes")
    p.add_argument("--max-inflight-ms", type=float, default=200,
                   help="retraso acumulado a partir del cual se descartan frames")
    p.add_argument("--ffmpeg", help="ruta a ffmpeg.exe")
    p.add_argument("-v", "--verbose", action="store_true")
    args = p.parse_args()
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(encoding="utf-8", errors="replace", line_buffering=True)
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(asctime)s %(message)s", datefmt="%H:%M:%S")
    try:
        asyncio.run(main_async(args))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
