//! Servidor HTTPS/WebSocket y sesión del visor.
//!
//! Mismo protocolo que la versión de Python, así que el cliente web sirve para ambas:
//! - Cada puerto acepta HTTPS y HTTP (se mira el primer byte). Fuera de este portátil, todo
//!   menos la página de configuración se redirige a HTTPS: la pantalla nunca viaja sin cifrar.
//! - Un único visor activo, autenticado con el código de emparejamiento.
//! - Control de flujo por confirmaciones: si el visor se retrasa, se descartan frames y se
//!   pide un keyframe en vez de acumular retraso.
use crate::capture::{self, OutputInfo};
use crate::certs::{self, CertManager};
use crate::clock::now_ms;
use crate::display::{self, Target};
use crate::vdd::Vdd;
use crate::input::{Injector, PointerEvent};
use crate::pipeline::{Controller, Event, Pipeline, Settings};
use crate::Args;
use anyhow::{bail, Context, Result};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Lo único que se sirve sin cifrar a otros dispositivos: lo necesario para instalar el certificado.
const PLAIN_HTTP_PATHS: [&str; 5] = ["/setup", "/ca.crt", "/info", "/style.css", "/icon.svg"];

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

// ------------------------------------------------------------------ certificado recargable

#[derive(Debug)]
struct CertResolver(RwLock<Arc<CertifiedKey>>);

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.read().unwrap().clone())
    }
}

fn certified_key(cm: &CertManager) -> Result<Arc<CertifiedKey>> {
    let (chain, key) = cm.load()?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).context("clave no admitida")?;
    Ok(Arc::new(CertifiedKey::new(chain, signer)))
}

// ------------------------------------------------------------------ aplicación

struct Viewer {
    id: u64,
    tx: mpsc::UnboundedSender<Message>,
}

struct Stream {
    settings: Settings,
    ctrl: Controller,
    pipeline: Option<Pipeline>,
    forward: tokio::task::JoinHandle<()>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.forward.abort();
        if let Some(p) = self.pipeline.take() {
            // Parar el hilo de captura puede tardar unos milisegundos: fuera del runtime.
            tokio::task::spawn_blocking(move || drop(p));
        }
    }
}

/// Lo que pide el visor en su "hello".
#[derive(Clone, Debug, PartialEq)]
struct Request {
    extend: bool,
    w: u32,
    h: u32,
    fps: u32,
    bitrate: u32,
}

impl Request {
    fn parse(p: &Value) -> Self {
        let num = |k: &str, def: f64, lo: f64, hi: f64| p[k].as_f64().unwrap_or(def).clamp(lo, hi);
        Self {
            extend: p["mode"].as_str() == Some("extend"),
            w: num("w", 1920.0, 160.0, 4096.0) as u32,
            h: num("h", 1080.0, 160.0, 4096.0) as u32,
            fps: num("fps", 60.0, 10.0, 120.0) as u32,
            bitrate: (num("bitrate", 20.0, 1.0, 100.0) * 1_000_000.0) as u32,
        }
    }
}

/// Monitor virtual conectado para el visor actual.
struct Virtual {
    index: u32,
    name: String,
}

#[derive(Default)]
struct Hub {
    viewer: Option<Viewer>,
    stream: Option<Stream>,
    request: Option<Request>,
    virt: Option<Virtual>,
}

fn even(v: u32) -> u32 {
    (v / 2 * 2).max(160)
}

/// Modo espejo: misma proporción que el monitor, nunca se escala hacia arriba.
fn mirror_size(mon: &OutputInfo, req: &Request) -> (u32, u32) {
    let (mw, mh) = (mon.rect.w as f64, mon.rect.h as f64);
    let scale = (req.w as f64 / mw).min(req.h as f64 / mh).min(1.0);
    ((mw * scale) as u32, (mh * scale) as u32)
}

pub struct App {
    pub args: Args,
    pub key: String,
    web: PathBuf,
    certs: CertManager,
    ca_fingerprint: String,
    tls: TlsAcceptor,
    resolver: Arc<CertResolver>,
    hub: Mutex<Hub>,
    active_viewer: AtomicU64,
    next_id: AtomicU64,
    injector: StdMutex<Injector>,
    cert_warned: StdMutex<HashSet<IpAddr>>,
    last_client_log: StdMutex<Option<Instant>>,
    vdd: StdMutex<Option<Arc<Vdd>>>,
}

impl App {
    pub fn new(args: Args, key: String, web: PathBuf, certs: CertManager, vdd: Option<Vdd>) -> Result<Arc<Self>> {
        let resolver = Arc::new(CertResolver(RwLock::new(certified_key(&certs)?)));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(resolver.clone());
        let ca_fingerprint = certs.fingerprint()?;
        Ok(Arc::new(Self {
            args,
            key,
            web,
            ca_fingerprint,
            certs,
            tls: TlsAcceptor::from(Arc::new(config)),
            resolver,
            hub: Mutex::new(Hub::default()),
            active_viewer: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
            injector: StdMutex::new(Injector::new()),
            cert_warned: StdMutex::new(HashSet::new()),
            last_client_log: StdMutex::new(None),
            vdd: StdMutex::new(vdd.map(Arc::new)),
        }))
    }

    /// Driver del monitor virtual (se reintenta abrir por si se ha instalado con el host en marcha).
    fn vdd(&self) -> Result<Arc<Vdd>> {
        let mut slot = self.vdd.lock().unwrap();
        if let Some(v) = slot.as_ref() {
            return Ok(v.clone());
        }
        let v = Arc::new(Vdd::open()?);
        *slot = Some(v.clone());
        Ok(v)
    }

    /// Crea (si hace falta) el monitor virtual y le pone el modo que mejor encaja con el cliente.
    async fn prepare_virtual(&self, hub: &mut Hub, req: &Request) -> Result<(OutputInfo, Target, bool)> {
        let vdd = self.vdd()?;
        if hub.virt.is_none() {
            let (index, out) = tokio::task::spawn_blocking(move || display::create_virtual(&vdd)).await??;
            log::info!("Monitor virtual conectado: {} (índice {index} en Parsec VDD)", out.name);
            hub.virt = Some(Virtual { index, name: out.name });
        }
        let name = hub.virt.as_ref().map(|v| v.name.clone()).unwrap_or_default();
        let (w, h, fps) = (req.w, req.h, req.fps);
        tokio::task::spawn_blocking(move || -> Result<(OutputInfo, Target, bool)> {
            let (target, exact) =
                display::best_target(&display::modes(&name), w, h, fps).context("el monitor virtual no tiene modos")?;
            let changed = display::current(&name) != Some(target);
            if changed {
                display::apply(&name, target)?;
            }
            let out = display::wait_output(&name, target.desktop())?;
            if changed {
                // Dar tiempo a Windows a terminar el cambio de modo antes de empezar a capturar.
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok((out, target, exact))
        })
        .await?
    }

    async fn remove_virtual(&self, hub: &mut Hub) {
        if let Some(v) = hub.virt.take() {
            hub.stream = None; // primero se deja de capturar ese monitor
            if let Ok(vdd) = self.vdd() {
                let index = v.index;
                let _ = tokio::task::spawn_blocking(move || vdd.remove(index)).await;
            }
            log::info!("Monitor virtual {} quitado", v.name);
        }
    }

    /// Si aparece una IP nueva (p. ej. al activar el hotspot), se reemite el certificado.
    pub async fn watch_ips(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            match self.certs.ensure().and_then(|changed| {
                if changed {
                    *self.resolver.0.write().unwrap() = certified_key(&self.certs)?;
                }
                Ok(changed)
            }) {
                Ok(true) => log::info!("Certificado actualizado. Direcciones: {:?}", certs::usable_ips()),
                Ok(false) => {}
                Err(e) => log::warn!("No se pudo actualizar el certificado: {e:#}"),
            }
        }
    }

    /// Windows cancela los contactos táctiles que no se actualizan.
    pub async fn touch_keepalive(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        loop {
            tick.tick().await;
            self.injector.lock().unwrap().keepalive();
        }
    }

    fn monitor(&self) -> Result<OutputInfo> {
        let outputs = capture::outputs()?;
        let n = outputs.len();
        outputs
            .into_iter()
            .nth(self.args.monitor.min(n.saturating_sub(1)))
            .context("no hay monitores")
    }

    async fn attach(self: &Arc<Self>, id: u64, tx: &mpsc::UnboundedSender<Message>, acked: &Arc<AtomicU32>, hello: &Value) -> Result<()> {
        let req = Request::parse(hello);
        let mut hub = self.hub.lock().await;
        let same_viewer = hub.viewer.as_ref().map(|v| v.id) == Some(id);
        if let Some(old) = hub.viewer.as_ref().filter(|v| v.id != id) {
            let bye = json!({"t": "bye", "reason": "Otro dispositivo se ha conectado como pantalla."});
            let _ = old.tx.send(Message::text(bye.to_string()));
            let _ = old.tx.send(Message::Close(None));
        }
        hub.viewer = Some(Viewer { id, tx: tx.clone() });
        self.active_viewer.store(id, Ordering::Relaxed);
        if same_viewer && hub.stream.is_some() {
            if hub.request.as_ref() == Some(&req) {
                return Ok(()); // nada que cambiar
            }
            let prev = hub.request.clone();
            if let (Some(prev), Some(st)) = (prev, hub.stream.as_mut()) {
                if (Request { bitrate: req.bitrate, ..prev }) == req {
                    st.ctrl.bitrate(req.bitrate); // solo cambia la calidad: sin reiniciar
                    st.settings.bitrate = req.bitrate;
                    log::info!("Calidad cambiada a {} Mbps", req.bitrate / 1_000_000);
                    hub.request = Some(req);
                    return Ok(());
                }
            }
        }

        hub.stream = None; // se para la captura anterior antes de tocar los monitores
        let info = |msg: &str| {
            let _ = tx.send(Message::text(json!({"t": "info", "message": msg}).to_string()));
        };
        let mut extend = req.extend;
        let mut target = None;
        if extend {
            match self.prepare_virtual(&mut hub, &req).await {
                Ok((out, t, exact)) => {
                    let (dw, dh) = t.desktop();
                    if !exact {
                        // El driver solo admite modos horizontales: se registra el lado largo primero.
                        let (lw, lh) = (req.w.max(req.h), req.w.min(req.h));
                        let hint = format!(
                            "Resolución aproximada {dw}x{dh} (la de esta pantalla es {}x{}). Para usar la exacta, ejecuta una \
                             vez en el portátil, como administrador: tools\\vdd-resoluciones.ps1 -Add {lw}x{lh}@{}",
                            req.w, req.h, req.fps
                        );
                        log::info!("{hint}");
                        info(&hint);
                    }
                    target = Some((out, dw, dh, req.fps.min(t.mode.hz).max(10)));
                }
                Err(e) => {
                    let msg = format!("Modo extender no disponible ({e:#}). Se usa el modo espejo.");
                    log::warn!("{msg}");
                    info(&msg);
                    extend = false;
                    self.remove_virtual(&mut hub).await;
                }
            }
        } else {
            self.remove_virtual(&mut hub).await;
        }
        let (mon, width, height, fps) = match target {
            Some(t) => t,
            None => {
                let mon = self.monitor()?;
                let (w, h) = mirror_size(&mon, &req);
                (mon, w, h, req.fps)
            }
        };
        let settings = Settings { monitor: mon.index, width: even(width), height: even(height), fps, bitrate: req.bitrate };
        self.injector.lock().unwrap().monitor = Some(mon.rect);
        hub.request = Some(req);
        log::info!(
            "{} {}x{} a {} fps, {} Mbps desde {} ({}x{})",
            if extend { "Extendiendo:" } else { "Duplicando:" },
            settings.width, settings.height, settings.fps, settings.bitrate / 1_000_000, mon.name, mon.rect.w, mon.rect.h
        );
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let pipeline = Pipeline::start(settings.clone(), ev_tx);
        let forward = tokio::spawn(forward(
            ev_rx,
            tx.clone(),
            acked.clone(),
            pipeline.controller(),
            settings.clone(),
            extend,
            self.args.max_inflight_ms,
        ));
        let ctrl = pipeline.controller();
        hub.stream = Some(Stream { settings, ctrl, pipeline: Some(pipeline), forward });
        Ok(())
    }

    async fn detach(&self, id: u64) {
        let mut hub = self.hub.lock().await;
        if hub.viewer.as_ref().map(|v| v.id) == Some(id) {
            hub.viewer = None;
            hub.stream = None;
            hub.request = None;
            self.injector.lock().unwrap().release_all();
            self.remove_virtual(&mut hub).await;
            self.active_viewer.store(0, Ordering::Relaxed);
        }
    }

    fn log_client(&self, m: &Value) {
        let mut last = self.last_client_log.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        *last = Some(Instant::now());
        let f = |k: &str| m[k].as_f64().unwrap_or(0.0);
        let total = if f("e2e") > 0.0 { format!(", total {:.0} ms (p95 {:.0})", f("e2e"), f("e2e95")) } else { String::new() };
        log::info!(
            "Visor: {:.0} fps, {:.1} Mbps, red {:.1} ms, decodificación {:.1} ms, RTT {:.1} ms{total}",
            f("fps"), f("mbps"), f("net"), f("dec"), f("rtt")
        );
    }
}

// ------------------------------------------------------------------ reenvío de frames

fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((p * v.len() as f64) as usize).min(v.len() - 1)]
}

async fn forward(
    mut rx: mpsc::UnboundedReceiver<Event>,
    tx: mpsc::UnboundedSender<Message>,
    acked: Arc<AtomicU32>,
    ctrl: Controller,
    s: Settings,
    extend: bool,
    max_inflight_ms: f64,
) {
    let limit = ((s.fps as f64 * max_inflight_ms / 1000.0).round() as u32).max(3);
    let send_json = |v: Value| {
        let _ = tx.send(Message::text(v.to_string()));
    };
    let (mut seq, mut last_sent) = (0u32, 0u32);
    let mut waiting_key = false;
    let mut last_key_request = Instant::now() - Duration::from_secs(1);
    let (mut frames, mut sent, mut dropped, mut keys, mut bytes) = (0u32, 0u32, 0u32, 0u32, 0usize);
    let mut encode_ms: Vec<f64> = Vec::new();
    let mut src = (0u32, 0u32);
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    Event::Started { encoder, src_w, src_h } => {
                        log::info!("Codificador: {encoder}");
                        src = (src_w, src_h);
                    }
                    Event::Config { codec } => send_json(json!({
                        "t": "config", "codec": codec, "width": s.width, "height": s.height,
                        "fps": s.fps, "bitrate": s.bitrate / 1_000_000, "encoder": "mf", "pipeline": "gpu",
                        "srcWidth": src.0, "srcHeight": src.1, "mode": if extend { "extend" } else { "mirror" },
                    })),
                    Event::Frame { data, key, host_ms, encode_ms: e } => {
                        seq += 1;
                        frames += 1;
                        encode_ms.push(e);
                        let congested = last_sent.saturating_sub(acked.load(Ordering::Relaxed)) > limit;
                        if congested || (waiting_key && !key) {
                            waiting_key = true;
                            dropped += 1;
                            if !congested && last_key_request.elapsed() > Duration::from_millis(200) {
                                ctrl.keyframe(); // ya hay hueco: recuperar con un keyframe
                                last_key_request = Instant::now();
                            }
                            continue;
                        }
                        waiting_key = false;
                        let mut buf = Vec::with_capacity(20 + data.len());
                        buf.extend_from_slice(&[1, key as u8, 0, 0]);
                        buf.extend_from_slice(&seq.to_le_bytes());
                        buf.extend_from_slice(&host_ms.to_le_bytes());
                        buf.extend_from_slice(&(e.max(0.0) as u32).to_le_bytes());
                        buf.extend_from_slice(&data);
                        bytes += data.len();
                        sent += 1;
                        keys += key as u32;
                        last_sent = seq;
                        if tx.send(Message::binary(buf)).is_err() {
                            break;
                        }
                    }
                    Event::Cursor { x, y, visible } => send_json(json!({"t": "cursor", "x": x, "y": y, "v": visible})),
                    Event::CursorShape(shape) => send_json(json!({
                        "t": "cursorShape", "w": shape.width, "h": shape.height,
                        "rgba": base64::engine::general_purpose::STANDARD.encode(&shape.rgba),
                    })),
                    Event::Error(e) => {
                        log::error!("Captura/codificación: {e}");
                        send_json(json!({"t": "error", "message": format!("Error en el portátil: {e}")}));
                    }
                }
            }
            _ = tick.tick() => {
                if waiting_key && last_key_request.elapsed() > Duration::from_millis(500) {
                    ctrl.keyframe();
                    last_key_request = Instant::now();
                }
                send_json(json!({
                    "t": "stats", "encFps": frames, "sentFps": sent, "dropped": dropped, "keys": keys,
                    "mbps": (bytes as f64 * 8.0 / 1e6 * 100.0).round() / 100.0,
                    "inflight": last_sent.saturating_sub(acked.load(Ordering::Relaxed)),
                    "encMs50": (percentile(&mut encode_ms, 0.5) * 10.0).round() / 10.0,
                    "encMs95": (percentile(&mut encode_ms, 0.95) * 10.0).round() / 10.0,
                }));
                (frames, sent, dropped, keys, bytes) = (0, 0, 0, 0, 0);
                encode_ms.clear();
            }
        }
    }
}

// ------------------------------------------------------------------ conexiones

pub async fn serve(app: Arc<App>, listener: TcpListener) {
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    loop {
        let Ok((stream, addr)) = listener.accept().await else { continue };
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(app, stream, addr.ip(), port).await {
                log::debug!("Conexión de {} terminada: {e:#}", addr.ip());
            }
        });
    }
}

async fn handle_conn(app: Arc<App>, stream: TcpStream, peer: IpAddr, port: u16) -> Result<()> {
    stream.set_nodelay(true)?;
    let local = peer.is_loopback();
    let mut first = [0u8; 1];
    if timeout(Duration::from_secs(10), stream.peek(&mut first)).await?? == 0 {
        return Ok(());
    }
    let tls = first[0] == 0x16;
    log::debug!("Conexión de {peer} al puerto {port} ({})", if tls { "HTTPS" } else { "HTTP" });
    let io: Box<dyn Io> = if tls {
        match timeout(Duration::from_secs(15), app.tls.accept(stream)).await {
            Ok(Ok(s)) => Box::new(s),
            Ok(Err(e)) => {
                // Chrome abre varias conexiones y rechaza algunas aunque ya se haya aceptado
                // el aviso: se avisa una sola vez por dispositivo.
                if app.cert_warned.lock().unwrap().insert(peer) {
                    log::info!(
                        "{peer} ha rechazado el certificado ({e}). Es normal hasta que se acepte el aviso \
                         del navegador o se instale el certificado desde /setup."
                    );
                }
                return Ok(());
            }
            Err(_) => bail!("TLS sin respuesta"),
        }
    } else {
        Box::new(stream)
    };
    handle_http(app, io, tls, peer, local).await
}

/// Comparación en tiempo constante (no revela cuántos caracteres coinciden).
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn respond(io: &mut Box<dyn Io>, status: u16, ctype: &str, body: &[u8], extra: &[(&str, String)], head: bool) -> Result<()> {
    let reason = match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    let mut h = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         Connection: close\r\nX-Content-Type-Options: nosniff\r\n",
        body.len()
    );
    for (k, v) in extra {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str("\r\n");
    io.write_all(h.as_bytes()).await?;
    if !head {
        io.write_all(body).await?;
    }
    io.flush().await?;
    Ok(())
}

async fn handle_http(app: Arc<App>, mut io: Box<dyn Io>, tls: bool, peer: IpAddr, local: bool) -> Result<()> {
    let mut buf = Vec::with_capacity(2048);
    let end = timeout(Duration::from_secs(10), async {
        let mut chunk = [0u8; 2048];
        loop {
            let n = io.read(&mut chunk).await?;
            if n == 0 {
                bail!("conexión cerrada");
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(p) = find(&buf, b"\r\n\r\n") {
                return Ok(p + 4);
            }
            if buf.len() > 16 * 1024 {
                bail!("cabecera demasiado grande");
            }
        }
    })
    .await??;
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let leftover = buf[end..].to_vec();
    let mut lines = head.split("\r\n");
    let mut req = lines.next().unwrap_or("").split(' ');
    let (method, target) = (req.next().unwrap_or(""), req.next().unwrap_or("/"));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let header = |k: &str| headers.get(k).map(String::as_str).unwrap_or("");
    if !local {
        log::info!("{peer} {method} {path}{}", if tls { "" } else { " (sin cifrar)" });
    }

    if !tls && !local && !PLAIN_HTTP_PATHS.contains(&path) {
        let host = header("host").rsplit_once(':').map_or(header("host"), |(h, _)| h).to_string();
        let mut location = format!("https://{host}:{}{path}", app.args.port);
        if !query.is_empty() {
            location = format!("{location}?{query}");
        }
        return respond(&mut io, 302, "text/plain", b"", &[("Location", location)], false).await;
    }
    if path == "/ws" && header("upgrade").eq_ignore_ascii_case("websocket") {
        return websocket(app, io, &headers, query, leftover, peer).await;
    }
    if method != "GET" && method != "HEAD" {
        return respond(&mut io, 405, "text/plain", b"", &[], false).await;
    }
    let head_only = method == "HEAD";
    match path {
        "/ca.crt" => {
            let body = tokio::fs::read(&app.certs.ca_crt).await?;
            let disp = ("Content-Disposition", "attachment; filename=\"pantalla-poc-ca.crt\"".to_string());
            respond(&mut io, 200, "application/x-x509-ca-cert", &body, &[disp], head_only).await
        }
        "/info" => {
            let ips: Vec<String> = certs::usable_ips().iter().map(|i| i.to_string()).collect();
            let body = json!({
                "httpsPort": app.args.port, "httpPort": app.args.http_port, "ips": ips,
                "host": certs::hostname(), "caFingerprint": app.ca_fingerprint, "server": "rust",
            });
            respond(&mut io, 200, "application/json", body.to_string().as_bytes(), &[], head_only).await
        }
        _ => {
            let name = match path {
                "/" => "index.html",
                "/probe" => "probe.html",
                "/setup" => "setup.html",
                p => p.trim_start_matches('/'),
            };
            match static_file(&app.web, name).await {
                Some((body, ctype)) => respond(&mut io, 200, ctype, &body, &[], head_only).await,
                None => respond(&mut io, 404, "text/plain; charset=utf-8", "No encontrado".as_bytes(), &[], head_only).await,
            }
        }
    }
}

async fn static_file(root: &Path, name: &str) -> Option<(Vec<u8>, &'static str)> {
    let rel = Path::new(name);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None; // nada de "..", rutas absolutas ni unidades
    }
    let path = root.join(rel);
    let ctype = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    };
    Some((tokio::fs::read(&path).await.ok()?, ctype))
}

async fn websocket(app: Arc<App>, mut io: Box<dyn Io>, headers: &HashMap<String, String>, query: &str, leftover: Vec<u8>, peer: IpAddr) -> Result<()> {
    let host = headers.get("host").map(String::as_str).unwrap_or("");
    // Solo desde nuestras propias páginas: evita que otra web abierta en el portátil se
    // conecte a localhost y controle el ratón.
    if let Some(origin) = headers.get("origin") {
        let netloc = origin.split_once("://").map_or(origin.as_str(), |(_, rest)| rest);
        if netloc != host {
            log::warn!("WebSocket rechazado por origen {origin}");
            return respond(&mut io, 403, "text/plain", b"", &[], false).await;
        }
    }
    let Some(key) = headers.get("sec-websocket-key") else {
        return respond(&mut io, 400, "text/plain", b"", &[], false).await;
    };
    let accept = derive_accept_key(key.as_bytes());
    io.write_all(
        format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")
            .as_bytes(),
    )
    .await?;
    let role = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("role="))
        .unwrap_or("viewer")
        .to_string();
    let ws = WebSocketStream::from_partially_read(io, leftover, Role::Server, None).await;
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let closing = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() || closing {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let id = app.next_id.fetch_add(1, Ordering::Relaxed);
    let acked = Arc::new(AtomicU32::new(0));
    log::info!("Conectado {peer} ({role})");
    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
        let kind = v["t"].as_str().unwrap_or("");
        if kind == "ping" {
            let pong = json!({"t": "pong", "c": v["c"], "s": now_ms()});
            let _ = tx.send(Message::text(pong.to_string()));
            continue;
        }
        if role != "viewer" {
            continue;
        }
        let active = app.active_viewer.load(Ordering::Relaxed) == id;
        match kind {
            "hello" => {
                let given: String = v["key"].as_str().unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                if !same_secret(given.to_ascii_uppercase().as_bytes(), app.key.as_bytes()) {
                    log::warn!("Código de emparejamiento incorrecto desde {peer}");
                    tokio::time::sleep(Duration::from_secs(1)).await; // frena la fuerza bruta
                    let _ = tx.send(Message::text(json!({"t": "auth", "ok": false}).to_string()));
                    let _ = tx.send(Message::Close(None));
                    break;
                }
                if let Err(e) = app.attach(id, &tx, &acked, &v).await {
                    log::error!("No se pudo iniciar la captura: {e:#}");
                    let msg = json!({"t": "error", "message": format!("Error en el portátil: {e:#}")});
                    let _ = tx.send(Message::text(msg.to_string()));
                }
            }
            "ack" if active => {
                acked.fetch_max(v["seq"].as_u64().unwrap_or(0) as u32, Ordering::Relaxed);
            }
            "pointer" if active => {
                if let Ok(ev) = serde_json::from_value::<PointerEvent>(v) {
                    app.injector.lock().unwrap().pointer(&ev);
                }
            }
            "wheel" if active => {
                let (dx, dy) = (v["dx"].as_f64().unwrap_or(0.0), v["dy"].as_f64().unwrap_or(0.0));
                app.injector.lock().unwrap().wheel(dx, dy);
            }
            "stats" if active => app.log_client(&v),
            _ => {}
        }
    }
    log::info!("Desconectado {peer} ({role})");
    app.detach(id).await;
    drop(tx);
    let _ = writer.await;
    Ok(())
}
