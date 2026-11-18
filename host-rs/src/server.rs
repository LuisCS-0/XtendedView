//! Servidor HTTPS/WebSocket, sesión del dispositivo y panel de control del ordenador.
//!
//! - Cada puerto acepta HTTPS y HTTP (se mira el primer byte). Fuera de este ordenador, todo
//!   menos la página de configuración se redirige a HTTPS: la pantalla nunca viaja sin cifrar.
//! - Un único dispositivo activo, autenticado con el código de emparejamiento.
//! - Los ajustes (red, modo, lado, fotogramas…) se eligen en el panel del ordenador; el
//!   dispositivo solo informa de su pantalla y muestra la imagen.
//! - Control de flujo por confirmaciones: si el dispositivo se retrasa, se descartan frames
//!   y se pide un keyframe en vez de acumular retraso.
use crate::capture::{self, OutputInfo};
use crate::certs::{self, CertManager};
use crate::clock::now_ms;
use crate::display::{self, Target};
use crate::hotspot::Hotspot;
use crate::input::{Injector, PointerEvent};
use crate::pipeline::{Controller, Event, Pipeline, Settings};
use crate::prefs::{self, Prefs};
use crate::sys;
use crate::vdd::Vdd;
use crate::Args;
use anyhow::{bail, Context, Result};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Lo único que se sirve sin cifrar a otros dispositivos: lo necesario para instalar el certificado.
const PLAIN_HTTP_PATHS: [&str; 5] = ["/setup", "/ca.crt", "/info", "/style.css", "/icon.svg"];
/// Solo para este ordenador: el panel de control y sus códigos QR.
const LOCAL_ONLY_PATHS: [&str; 6] = ["/panel", "/panel.js", "/panel.css", "/qr/url.svg", "/qr/wifi.svg", "/qr/setup.svg"];

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

// ------------------------------------------------------------------ sesión

struct Viewer {
    id: u64,
    tx: mpsc::UnboundedSender<Message>,
    acked: Arc<AtomicU32>,
}

/// Lo que el dispositivo cuenta de sí mismo al conectarse.
#[derive(Clone)]
struct Client {
    name: String,
    ip: IpAddr,
    w: u32,
    h: u32,
    hz: u32,
    since: Instant,
}

impl Client {
    fn parse(p: &Value, ip: IpAddr) -> Self {
        let num = |k: &str, def: f64, lo: f64, hi: f64| p[k].as_f64().unwrap_or(def).clamp(lo, hi) as u32;
        Self {
            name: p["name"].as_str().unwrap_or("Dispositivo").chars().take(80).collect(),
            ip,
            w: num("w", 1920.0, 160.0, 8192.0),
            h: num("h", 1080.0, 160.0, 8192.0),
            hz: num("hz", 60.0, 30.0, 240.0),
            since: Instant::now(),
        }
    }
}

struct Stream {
    settings: Settings,
    ctrl: Controller,
    pipeline: Option<Pipeline>,
    forward: tokio::task::JoinHandle<()>,
}

impl Stream {
    /// Para la captura y espera a que suelte el monitor: Windows no deja duplicar dos veces el
    /// mismo monitor desde el mismo programa, así que la siguiente no puede empezar antes.
    async fn stop(mut self) {
        self.forward.abort();
        if let Some(p) = self.pipeline.take() {
            let _ = tokio::task::spawn_blocking(move || drop(p)).await;
        }
    }
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

async fn stop_stream(hub: &mut Hub) {
    if let Some(s) = hub.stream.take() {
        s.stop().await;
    }
}

/// Lo que se pide a la captura: sale de la pantalla del dispositivo y de los ajustes.
#[derive(Clone, Debug, PartialEq)]
struct Request {
    extend: bool,
    w: u32,
    h: u32,
    fps: u32,
    bitrate: u32,
    side: String,
    monitor: usize,
}

impl Request {
    fn new(c: &Client, p: &Prefs) -> Self {
        Self {
            extend: p.mode == "extend",
            w: ((c.w as f64 * p.scale) as u32).max(160),
            h: ((c.h as f64 * p.scale) as u32).max(160),
            fps: if p.fps == 0 { c.hz.clamp(30, 120) } else { p.fps },
            bitrate: p.bitrate * 1_000_000,
            side: p.side.clone(),
            monitor: p.monitor,
        }
    }
}

/// Lo que se está enviando, para el panel.
#[derive(Clone)]
struct StreamInfo {
    extend: bool,
    width: u32,
    height: u32,
    fps: u32,
    portrait: bool,
    exact: bool,
    /// Resolución que habría que registrar para que fuera exacta ("2447x1848@120").
    register: Option<String>,
    monitor: String,
}

/// Monitor virtual conectado para el dispositivo actual.
struct Virtual {
    index: u32,
    name: String,
}

#[derive(Default)]
struct Hub {
    viewer: Option<Viewer>,
    client: Option<Client>,
    stream: Option<Stream>,
    request: Option<Request>,
    virt: Option<Virtual>,
    info: Option<StreamInfo>,
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
    key: StdMutex<String>,
    root: PathBuf,
    data: PathBuf,
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
    prefs: StdMutex<Prefs>,
    hotspot: StdMutex<Option<Arc<Hotspot>>>,
    hotspot_error: StdMutex<Option<String>>,
    viewer_stats: StdMutex<Value>,
    host_stats: Arc<StdMutex<Value>>,
    /// Encendido desde el panel: solo entonces se aceptan dispositivos y hay red propia.
    on: AtomicBool,
    /// Un encendido o apagado cada vez.
    power_lock: Mutex<()>,
    /// Turno para comprobar el código de emparejamiento (ver `check_key`).
    auth_turn: Mutex<()>,
    auth_waiting: AtomicU32,
    rejected: StdMutex<HashSet<IpAddr>>,
    /// Avisa a los paneles abiertos de que algo ha cambiado.
    changed: Notify,
}

/// Lo que ve el dispositivo si intenta conectarse con la pantalla apagada.
const OFF_REASON: &str = "La pantalla está apagada en el ordenador. Enciéndela desde el panel y vuelve a escanear el código QR.";

impl App {
    pub fn new(args: Args, key: String, root: PathBuf, data: PathBuf, certs: CertManager, vdd: Option<Vdd>, prefs: Prefs) -> Result<Arc<Self>> {
        let resolver = Arc::new(CertResolver(RwLock::new(certified_key(&certs)?)));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(resolver.clone());
        let ca_fingerprint = certs.fingerprint()?;
        Ok(Arc::new(Self {
            args,
            key: StdMutex::new(key),
            web: root.join("web"),
            root,
            data,
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
            prefs: StdMutex::new(prefs),
            hotspot: StdMutex::new(None),
            hotspot_error: StdMutex::new(None),
            viewer_stats: StdMutex::new(Value::Null),
            host_stats: Arc::new(StdMutex::new(Value::Null)),
            on: AtomicBool::new(false),
            power_lock: Mutex::new(()),
            auth_turn: Mutex::new(()),
            auth_waiting: AtomicU32::new(0),
            rejected: StdMutex::new(HashSet::new()),
            changed: Notify::new(),
        }))
    }

    pub fn key(&self) -> String {
        self.key.lock().unwrap().clone()
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    // -------------------------------------------------------------- seguridad

    /// Quién puede conectarse a este ordenador.
    ///
    /// - Apagada: nadie de fuera (ni siquiera ve las páginas ni el certificado).
    /// - Red propia: solo los dispositivos unidos a ella, aunque el ordenador siga conectado
    ///   a otra Wi-Fi (p. ej. la de una cafetería), cuyos equipos quedan fuera.
    /// - Red del ordenador: cualquiera de esa red; lo protege el código de emparejamiento.
    fn peer_allowed(&self, peer: IpAddr) -> bool {
        if peer.is_loopback() {
            return true;
        }
        if !self.is_on() {
            return false;
        }
        if self.prefs().network == "own" {
            return matches!(peer, IpAddr::V4(v) if v.octets()[..3] == [192, 168, 137]);
        }
        true
    }

    /// Comprueba el código de emparejamiento contra la fuerza bruta.
    ///
    /// Un intento cada vez, y cada fallo hace esperar 1 s al siguiente intento, venga de donde
    /// venga: como mucho un intento fallido por segundo aunque se abran cientos de conexiones
    /// (con 31⁸ códigos posibles, decenas de miles de años; y cambia en cada encendido). Si se
    /// acumulan demasiados en cola, se rechazan sin mirarlos. `None` = demasiados intentos.
    async fn check_key(&self, given: &str) -> Option<bool> {
        if self.auth_waiting.fetch_add(1, Ordering::SeqCst) >= 8 {
            self.auth_waiting.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        let ok = {
            let _turn = self.auth_turn.lock().await;
            let ok = same_secret(given.to_ascii_uppercase().as_bytes(), self.key().as_bytes());
            if !ok {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            ok
        };
        self.auth_waiting.fetch_sub(1, Ordering::SeqCst);
        Some(ok)
    }

    // -------------------------------------------------------------- encendido

    /// Enciende (código de emparejamiento nuevo y, si toca, red propia) o apaga (desconecta el
    /// dispositivo, quita el monitor virtual y cierra la red propia).
    pub async fn power(self: &Arc<Self>, on: bool) -> Result<String> {
        let _one_at_a_time = self.power_lock.lock().await;
        if on == self.is_on() {
            return Ok(if on { "Ya estaba encendida" } else { "Ya estaba apagada" }.into());
        }
        self.rejected.lock().unwrap().clear();
        if on {
            let key = prefs::pairing_key(&self.data, true)?;
            *self.key.lock().unwrap() = key.clone();
            self.on.store(true, Ordering::SeqCst);
            if self.prefs().network == "own" {
                self.set_network(true).await;
            }
            log::info!("Encendida. Código de emparejamiento: {}", prefs::pretty_key(&key));
            self.print_pairing();
            self.changed.notify_waiters();
            Ok("Encendida: escanea el código QR con el dispositivo".into())
        } else {
            {
                // Con el hub bloqueado: ningún dispositivo puede colarse entre medias.
                let mut hub = self.hub.lock().await;
                self.on.store(false, Ordering::SeqCst);
                if let Some(v) = &hub.viewer {
                    let bye = json!({"t": "bye", "reason": "La pantalla se ha apagado desde el ordenador.", "retry": false});
                    let _ = v.tx.send(Message::text(bye.to_string()));
                    let _ = v.tx.send(Message::Close(None));
                }
                self.clear_viewer(&mut hub).await;
            }
            self.set_network(false).await;
            log::info!("Apagada");
            self.changed.notify_waiters();
            Ok("Apagada".into())
        }
    }

    /// Dirección y código QR en la consola, para quien no tenga el panel delante.
    fn print_pairing(&self) {
        match self.viewer_url() {
            Some(url) => {
                println!("\n En el dispositivo, escanea este código o abre {url}");
                if let Ok(qr) = sys::qr_text(&url) {
                    for l in qr.lines() {
                        println!("   {l}");
                    }
                }
            }
            None => println!("\n Sin red disponible todavía: el panel mostrará el código QR cuando la haya."),
        }
    }

    pub fn prefs(&self) -> Prefs {
        self.prefs.lock().unwrap().clone()
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

    // -------------------------------------------------------------- red

    /// Activa o desactiva la red Wi-Fi propia.
    pub async fn set_network(self: &Arc<Self>, own: bool) {
        if !own {
            if self.hotspot.lock().unwrap().take().is_some() {
                log::info!("Red Wi-Fi propia cerrada");
            }
            *self.hotspot_error.lock().unwrap() = None;
            self.changed.notify_waiters();
            return;
        }
        if self.hotspot.lock().unwrap().is_some() {
            return;
        }
        match tokio::task::spawn_blocking(Hotspot::start).await {
            Ok(Ok(h)) => {
                log::info!("Red Wi-Fi propia activa: {}", h.ssid);
                *self.hotspot.lock().unwrap() = Some(Arc::new(h));
                *self.hotspot_error.lock().unwrap() = None;
            }
            Ok(Err(e)) => {
                log::error!("Red Wi-Fi propia: {e:#}");
                *self.hotspot_error.lock().unwrap() = Some(format!("{e:#}"));
            }
            Err(_) => {}
        }
        self.changed.notify_waiters();
    }

    /// Vigilante de la red propia: Windows puede tirarla (p. ej. al desconectar el ordenador
    /// de otra Wi-Fi) aunque siga diciendo que está iniciada. Si falla dos veces, se recrea.
    pub async fn hotspot_watchdog(self: Arc<Self>) {
        let mut failures = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(4)).await;
            let Some(h) = self.hotspot.lock().unwrap().clone() else {
                failures = 0;
                continue;
            };
            let h2 = h.clone();
            let healthy = tokio::task::spawn_blocking(move || h2.healthy()).await.unwrap_or(true);
            failures = if healthy { 0 } else { failures + 1 };
            if failures >= 2 {
                log::warn!("La red Wi-Fi propia ha desaparecido; volviendo a crearla…");
                let h2 = h.clone();
                match tokio::task::spawn_blocking(move || h2.restart()).await {
                    Ok(Ok(())) => log::info!("Red Wi-Fi propia activa de nuevo: {}", h.ssid),
                    Ok(Err(e)) => log::error!("No se pudo recrear la red Wi-Fi propia: {e:#}"),
                    Err(_) => {}
                }
                failures = 0;
                self.changed.notify_waiters();
            }
        }
    }

    /// IP a la que debe conectarse el dispositivo según la red elegida.
    fn device_ip(&self) -> Option<Ipv4Addr> {
        if self.prefs().network == "own" {
            let ok = self.hotspot.lock().unwrap().is_some();
            return ok.then(|| {
                certs::local_ipv4s()
                    .into_iter()
                    .find(|ip| ip.octets()[..3] == [192, 168, 137])
                    .unwrap_or(Ipv4Addr::new(192, 168, 137, 1))
            });
        }
        sys::lan_ip()
    }

    fn viewer_url(&self) -> Option<String> {
        if !self.is_on() {
            return None;
        }
        self.device_ip().map(|ip| format!("https://{ip}:{}/#k={}", self.args.port, self.key()))
    }

    fn setup_url(&self) -> Option<String> {
        self.device_ip().map(|ip| format!("http://{ip}:{}/setup", self.args.http_port))
    }

    fn wifi_qr(&self) -> Option<String> {
        let h = self.hotspot.lock().unwrap().clone()?;
        Some(sys::wifi_qr_text(&h.ssid, &h.password))
    }

    // -------------------------------------------------------------- pantalla

    /// Crea (si hace falta) el monitor virtual, le pone el modo que mejor encaja con el
    /// dispositivo y lo coloca en el lado elegido.
    async fn prepare_virtual(&self, hub: &mut Hub, req: &Request) -> Result<(OutputInfo, Target, bool)> {
        let vdd = self.vdd()?;
        if hub.virt.is_none() {
            let (index, out) = tokio::task::spawn_blocking(move || display::create_virtual(&vdd)).await??;
            log::info!("Monitor virtual conectado: {} (índice {index} en Parsec VDD)", out.name);
            hub.virt = Some(Virtual { index, name: out.name });
        }
        let name = hub.virt.as_ref().map(|v| v.name.clone()).unwrap_or_default();
        let (w, h, fps, side) = (req.w, req.h, req.fps, req.side.clone());
        tokio::task::spawn_blocking(move || -> Result<(OutputInfo, Target, bool)> {
            let (target, exact) =
                display::best_target(&display::modes(&name), w, h, fps).context("el monitor virtual no tiene modos")?;
            let mut changed = display::current(&name) != Some(target);
            if changed {
                display::apply(&name, target)?;
            }
            let dims = target.desktop();
            // El monitor principal es el que está en el origen del escritorio.
            let main = capture::outputs()?.into_iter().find(|o| o.name != name && o.rect.x == 0 && o.rect.y == 0);
            let at = main.map(|m| display::side_position(m.rect, dims, &side));
            if let Some(p) = at {
                if display::position(&name) != Some(p) {
                    display::place(&name, p)?;
                    changed = true;
                }
            }
            let out = display::wait_output(&name, dims, at)?;
            if changed {
                // Dar tiempo a Windows a terminar el cambio antes de empezar a capturar.
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok((out, target, exact))
        })
        .await?
    }

    async fn remove_virtual(&self, hub: &mut Hub) {
        if let Some(v) = hub.virt.take() {
            stop_stream(hub).await; // primero se deja de capturar ese monitor
            if let Ok(vdd) = self.vdd() {
                let index = v.index;
                let _ = tokio::task::spawn_blocking(move || vdd.remove(index)).await;
            }
            log::info!("Monitor virtual {} quitado", v.name);
        }
    }

    fn mirror_monitor(&self, index: usize, hub: &Hub) -> Result<OutputInfo> {
        let virt = hub.virt.as_ref().map(|v| v.name.clone());
        let outputs: Vec<OutputInfo> = capture::outputs()?.into_iter().filter(|o| Some(&o.name) != virt.as_ref()).collect();
        let n = outputs.len();
        outputs.into_iter().nth(index.min(n.saturating_sub(1))).context("no hay monitores")
    }

    fn send_viewer_settings(&self, hub: &Hub) {
        if let Some(v) = &hub.viewer {
            let p = self.prefs();
            let msg = json!({"t": "settings", "stats": p.stats, "measure": p.measure, "fullscreen": p.fullscreen});
            let _ = v.tx.send(Message::text(msg.to_string()));
        }
    }

    /// Ajusta la captura al dispositivo conectado y a los ajustes actuales.
    async fn apply(self: &Arc<Self>, hub: &mut Hub) -> Result<()> {
        let (Some(viewer), Some(client)) = (hub.viewer.as_ref(), hub.client.as_ref()) else { return Ok(()) };
        let (tx, acked) = (viewer.tx.clone(), viewer.acked.clone());
        let req = Request::new(client, &self.prefs());
        if hub.stream.is_some() {
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

        stop_stream(hub).await; // se para la captura anterior antes de tocar los monitores
        let info = |msg: &str| {
            let _ = tx.send(Message::text(json!({"t": "info", "message": msg}).to_string()));
        };
        let mut extend = req.extend;
        let mut target = None;
        let mut register = None;
        let (mut exact, mut portrait) = (true, false);
        if extend {
            match self.prepare_virtual(hub, &req).await {
                Ok((out, t, ok)) => {
                    let (dw, dh) = t.desktop();
                    if !ok {
                        // El driver solo admite modos horizontales: se registra el lado largo primero.
                        let spec = format!("{}x{}@{}", req.w.max(req.h), req.w.min(req.h), req.fps);
                        log::info!("Resolución aproximada {dw}x{dh} (la de la pantalla es {}x{}); exacta: {spec}", req.w, req.h);
                        register = Some(spec);
                    }
                    exact = ok;
                    portrait = t.portrait;
                    target = Some((out, dw, dh, req.fps.min(t.mode.hz).max(10)));
                }
                Err(e) => {
                    let msg = format!("Pantalla adicional no disponible ({e:#}). Se usa el modo espejo.");
                    log::warn!("{msg}");
                    info(&msg);
                    extend = false;
                    self.remove_virtual(hub).await;
                }
            }
        } else {
            self.remove_virtual(hub).await;
        }
        let (mon, width, height, fps) = match target {
            Some(t) => t,
            None => {
                let mon = self.mirror_monitor(req.monitor, hub)?;
                let (w, h) = mirror_size(&mon, &req);
                (mon, w, h, req.fps)
            }
        };
        let settings = Settings { monitor: mon.index, width: even(width), height: even(height), fps, bitrate: req.bitrate };
        self.injector.lock().unwrap().monitor = Some(mon.rect);
        log::info!(
            "{} {}x{} a {} fps, {} Mbps desde {} ({}x{})",
            if extend { "Pantalla adicional:" } else { "Espejo:" },
            settings.width, settings.height, settings.fps, settings.bitrate / 1_000_000, mon.name, mon.rect.w, mon.rect.h
        );
        hub.info = Some(StreamInfo {
            extend,
            width: settings.width,
            height: settings.height,
            fps: settings.fps,
            portrait,
            exact,
            register,
            monitor: mon.name.clone(),
        });
        hub.request = Some(req);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let pipeline = Pipeline::start(settings.clone(), ev_tx);
        let forward = tokio::spawn(forward(
            ev_rx,
            tx,
            acked,
            pipeline.controller(),
            settings.clone(),
            extend,
            self.args.max_inflight_ms,
            self.host_stats.clone(),
        ));
        let ctrl = pipeline.controller();
        hub.stream = Some(Stream { settings, ctrl, pipeline: Some(pipeline), forward });
        self.send_viewer_settings(hub);
        self.changed.notify_waiters();
        Ok(())
    }

    async fn reconfigure(self: &Arc<Self>) {
        let mut hub = self.hub.lock().await;
        if let Err(e) = self.apply(&mut hub).await {
            log::error!("No se pudo aplicar el cambio: {e:#}");
        }
    }

    async fn attach(self: &Arc<Self>, id: u64, tx: &mpsc::UnboundedSender<Message>, acked: &Arc<AtomicU32>, hello: &Value, ip: IpAddr) -> Result<()> {
        let mut hub = self.hub.lock().await;
        if !self.is_on() {
            bail!("apagada"); // se ha apagado mientras el dispositivo se conectaba
        }
        if let Some(old) = hub.viewer.as_ref().filter(|v| v.id != id) {
            let bye = json!({"t": "bye", "reason": "Otro dispositivo se ha conectado como pantalla."});
            let _ = old.tx.send(Message::text(bye.to_string()));
            let _ = old.tx.send(Message::Close(None));
            stop_stream(&mut hub).await;
            hub.request = None;
        }
        let same = hub.viewer.as_ref().is_some_and(|v| v.id == id);
        let mut client = Client::parse(hello, ip);
        if let (true, Some(prev)) = (same, hub.client.as_ref()) {
            client.since = prev.since; // mismo dispositivo que solo ha girado o plegado la pantalla
        } else {
            log::info!("Dispositivo: {} ({ip}), pantalla {}x{} a {} Hz", client.name, client.w, client.h, client.hz);
        }
        hub.viewer = Some(Viewer { id, tx: tx.clone(), acked: acked.clone() });
        hub.client = Some(client);
        self.active_viewer.store(id, Ordering::Relaxed);
        self.apply(&mut hub).await
    }

    async fn detach(&self, id: u64) {
        let mut hub = self.hub.lock().await;
        if hub.viewer.as_ref().map(|v| v.id) == Some(id) {
            self.clear_viewer(&mut hub).await;
        }
    }

    /// Olvida el dispositivo: deja de capturar, suelta el táctil y quita el monitor virtual.
    async fn clear_viewer(&self, hub: &mut Hub) {
        hub.viewer = None;
        hub.client = None;
        stop_stream(hub).await;
        hub.request = None;
        hub.info = None;
        self.injector.lock().unwrap().release_all();
        self.remove_virtual(hub).await;
        self.active_viewer.store(0, Ordering::Relaxed);
        *self.viewer_stats.lock().unwrap() = Value::Null;
        *self.host_stats.lock().unwrap() = Value::Null;
        self.changed.notify_waiters();
    }

    fn client_stats(&self, m: &Value) {
        *self.viewer_stats.lock().unwrap() = m.clone();
        let mut last = self.last_client_log.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        *last = Some(Instant::now());
        let f = |k: &str| m[k].as_f64().unwrap_or(0.0);
        let total = if f("e2e") > 0.0 { format!(", total {:.0} ms (p95 {:.0})", f("e2e"), f("e2e95")) } else { String::new() };
        log::info!(
            "Dispositivo: {:.0} fps, {:.1} Mbps, red {:.1} ms, decodificación {:.1} ms, RTT {:.1} ms{total}",
            f("fps"), f("mbps"), f("net"), f("dec"), f("rtt")
        );
    }

    // -------------------------------------------------------------- panel

    /// Aplica un ajuste elegido en el panel. Devuelve un mensaje si hay algo que contar.
    async fn set_pref(self: &Arc<Self>, key: &str, value: &Value) -> Result<Option<String>> {
        let ok = {
            let mut p = self.prefs.lock().unwrap();
            let ok = p.set(key, value);
            if ok {
                p.save(&prefs::path(&self.data));
            }
            ok
        };
        if !ok {
            bail!("valor no válido para «{key}»");
        }
        let mut message = None;
        match key {
            "network" => {
                let own = self.prefs().network == "own";
                let _one_at_a_time = self.power_lock.lock().await;
                // Mientras está apagada solo se guarda: la red propia se crea al encender.
                if self.is_on() {
                    self.set_network(own).await;
                    if let Some(e) = self.hotspot_error.lock().unwrap().clone().filter(|_| own) {
                        bail!("no se pudo crear la red propia: {e}");
                    }
                }
                message = Some(match (own, self.is_on()) {
                    (true, true) => "Red propia activa",
                    (false, true) => "Usando la red del ordenador",
                    (true, false) => "Al encender se creará la red propia",
                    (false, false) => "Al encender se usará la red del ordenador",
                }.to_string());
            }
            "stats" | "measure" | "fullscreen" => {
                let hub = self.hub.lock().await;
                self.send_viewer_settings(&hub);
            }
            _ => self.reconfigure().await,
        }
        self.changed.notify_waiters();
        Ok(message)
    }

    /// Acciones del panel. Devuelve un mensaje para mostrar.
    async fn action(self: &Arc<Self>, name: &str) -> Result<String> {
        match name {
            "powerOn" => self.power(true).await,
            "powerOff" => self.power(false).await,
            "disconnect" => {
                let hub = self.hub.lock().await;
                let Some(v) = &hub.viewer else { bail!("no hay ningún dispositivo conectado") };
                let bye = json!({"t": "bye", "reason": "Desconectado desde el ordenador."});
                let _ = v.tx.send(Message::text(bye.to_string()));
                let _ = v.tx.send(Message::Close(None));
                Ok("Dispositivo desconectado".into())
            }
            "newCode" => {
                if !self.is_on() {
                    bail!("la pantalla está apagada: al encenderla ya se crea un código nuevo");
                }
                let key = prefs::pairing_key(&self.data, true)?;
                *self.key.lock().unwrap() = key.clone();
                let hub = self.hub.lock().await;
                if let Some(v) = &hub.viewer {
                    let bye = json!({"t": "bye", "reason": "El código ha cambiado: vuelve a escanear el QR."});
                    let _ = v.tx.send(Message::text(bye.to_string()));
                    let _ = v.tx.send(Message::Close(None));
                }
                log::info!("Código de emparejamiento nuevo: {}", prefs::pretty_key(&key));
                Ok(format!("Código nuevo: {}", prefs::pretty_key(&key)))
            }
            "openProbe" => {
                sys::open_url(&format!("http://localhost:{}/probe", self.args.http_port));
                Ok("Sonda abierta: déjala visible en la pantalla que se envía al dispositivo".into())
            }
            "registerResolution" => self.register_resolution().await,
            _ => bail!("acción desconocida"),
        }
    }

    /// Registra la resolución exacta del dispositivo en el driver (pide permiso de administrador).
    async fn register_resolution(self: &Arc<Self>) -> Result<String> {
        let spec = {
            let hub = self.hub.lock().await;
            hub.info.as_ref().and_then(|i| i.register.clone()).context("la resolución ya es exacta")?
        };
        let script = self.root.join("tools").join("vdd-resoluciones.ps1");
        let params = format!("-NoProfile -ExecutionPolicy Bypass -File \"{}\" -Add {spec}", script.display());
        {
            // El script reinicia el adaptador virtual: antes se deja de capturar y se quita el monitor.
            let mut hub = self.hub.lock().await;
            stop_stream(&mut hub).await;
            self.remove_virtual(&mut hub).await;
        }
        let result = tokio::task::spawn_blocking(move || sys::run_elevated("powershell.exe", &params)).await?;
        // Tras reiniciar el adaptador hay que volver a abrir el driver.
        *self.vdd.lock().unwrap() = None;
        tokio::time::sleep(Duration::from_secs(2)).await;
        self.reconfigure().await;
        match result {
            Ok(0) => Ok(format!("Resolución {spec} registrada")),
            Ok(code) => bail!("el script terminó con el código {code} (¿ya hay 5 resoluciones registradas?)"),
            Err(e) => Err(e),
        }
    }

    /// Estado completo para el panel.
    async fn state(&self) -> Value {
        let p = self.prefs();
        let on = self.is_on();
        let hotspot = self.hotspot.lock().unwrap().clone();
        let ip = self.device_ip();
        let ifs = sys::interfaces();
        let link = ip.and_then(|ip| sys::interface_of(&ifs, ip)).map(|i| json!({"name": i.name, "wifi": i.wifi}));
        let net = json!({
            "kind": p.network,
            "ready": on && ip.is_some(),
            "ip": ip.map(|i| i.to_string()),
            "url": self.viewer_url(),
            "setupUrl": self.setup_url(),
            "ssid": hotspot.as_ref().map(|h| h.ssid.clone()),
            "password": hotspot.as_ref().map(|h| h.password.clone()),
            "error": self.hotspot_error.lock().unwrap().clone(),
            "otherWifi": sys::on_other_wifi(&ifs),
            "link": link,
        });
        let vdd = match self.vdd.lock().unwrap().as_ref() {
            Some(v) => json!({"available": true, "version": v.version}),
            None => json!({"available": false}),
        };
        let hub = self.hub.lock().await;
        let device = hub.client.as_ref().map(|c| {
            json!({
                "name": c.name, "ip": c.ip.to_string(), "screen": format!("{}x{}", c.w, c.h),
                "hz": c.hz, "since": c.since.elapsed().as_secs(),
            })
        });
        let stream = hub.info.as_ref().map(|i| {
            json!({
                "extend": i.extend, "width": i.width, "height": i.height, "fps": i.fps,
                "portrait": i.portrait, "exact": i.exact, "register": i.register, "monitor": i.monitor,
            })
        });
        let virt = hub.virt.as_ref().map(|v| v.name.clone());
        drop(hub);
        let monitors: Vec<Value> = capture::outputs()
            .unwrap_or_default()
            .into_iter()
            .filter(|o| Some(&o.name) != virt.as_ref())
            .enumerate()
            .map(|(i, o)| json!({"index": i, "name": o.name, "w": o.rect.w, "h": o.rect.h}))
            .collect();
        json!({
            "t": "state",
            "on": on,
            "prefs": p,
            "code": on.then(|| prefs::pretty_key(&self.key())),
            "net": net,
            "vdd": vdd,
            "device": device,
            "stream": stream,
            "viewerStats": self.viewer_stats.lock().unwrap().clone(),
            "hostStats": self.host_stats.lock().unwrap().clone(),
            "monitors": monitors,
            "ports": {"https": self.args.port, "http": self.args.http_port},
            "caFingerprint": self.ca_fingerprint,
        })
    }

    /// Código QR de la dirección del visor, de la red Wi-Fi propia o de la página de configuración.
    fn qr(&self, which: &str) -> Option<String> {
        let text = match which {
            "url" => self.viewer_url()?,
            "wifi" => self.wifi_qr()?,
            "setup" => self.setup_url()?,
            _ => return None,
        };
        sys::qr_svg(&text).ok()
    }

    // -------------------------------------------------------------- tareas de fondo

    /// Si aparece una IP nueva (p. ej. al activar la red propia), se reemite el certificado.
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

    pub fn panel_url(&self) -> String {
        format!("http://localhost:{}/panel", self.args.http_port)
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

#[allow(clippy::too_many_arguments)]
async fn forward(
    mut rx: mpsc::UnboundedReceiver<Event>,
    tx: mpsc::UnboundedSender<Message>,
    acked: Arc<AtomicU32>,
    ctrl: Controller,
    s: Settings,
    extend: bool,
    max_inflight_ms: f64,
    host_stats: Arc<StdMutex<Value>>,
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
    let mut encoder_name = String::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    Event::Started { encoder, src_w, src_h } => {
                        log::info!("Codificador: {encoder}");
                        encoder_name = encoder;
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
                        send_json(json!({"t": "error", "message": format!("Error en el ordenador: {e}")}));
                    }
                }
            }
            _ = tick.tick() => {
                if waiting_key && last_key_request.elapsed() > Duration::from_millis(500) {
                    ctrl.keyframe();
                    last_key_request = Instant::now();
                }
                let stats = json!({
                    "t": "stats", "encFps": frames, "sentFps": sent, "dropped": dropped, "keys": keys,
                    "mbps": (bytes as f64 * 8.0 / 1e6 * 100.0).round() / 100.0,
                    "inflight": last_sent.saturating_sub(acked.load(Ordering::Relaxed)),
                    "encMs50": (percentile(&mut encode_ms, 0.5) * 10.0).round() / 10.0,
                    "encMs95": (percentile(&mut encode_ms, 0.95) * 10.0).round() / 10.0,
                    "encoder": encoder_name,
                });
                *host_stats.lock().unwrap() = stats.clone();
                send_json(stats);
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
    if !app.peer_allowed(peer) {
        // Se corta sin responder nada. Se avisa una vez por dirección para no llenar el registro.
        if app.rejected.lock().unwrap().insert(peer) {
            let why = if app.is_on() { "solo se aceptan dispositivos de la red propia" } else { "la pantalla está apagada" };
            log::info!("Conexión de {peer} rechazada: {why}");
        }
        return Ok(());
    }
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

    if !local && LOCAL_ONLY_PATHS.contains(&path) {
        return respond(&mut io, 403, "text/plain; charset=utf-8", "Solo desde este ordenador".as_bytes(), &[], false).await;
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
    if let Some(which) = path.strip_prefix("/qr/").and_then(|p| p.strip_suffix(".svg")) {
        return match app.qr(which) {
            Some(svg) => respond(&mut io, 200, "image/svg+xml", svg.as_bytes(), &[], head_only).await,
            None => respond(&mut io, 404, "text/plain", b"", &[], head_only).await,
        };
    }
    match path {
        "/ca.crt" => {
            let body = tokio::fs::read(&app.certs.ca_crt).await?;
            let disp = ("Content-Disposition", "attachment; filename=\"pantalla-secundaria-ca.crt\"".to_string());
            respond(&mut io, 200, "application/x-x509-ca-cert", &body, &[disp], head_only).await
        }
        "/info" => {
            // Se sirve sin cifrar y sin código (lo usa la página de instalar el certificado):
            // nada del nombre del equipo ni de sus otras direcciones.
            let body = json!({
                "httpsPort": app.args.port, "httpPort": app.args.http_port,
                "caFingerprint": app.ca_fingerprint, "server": "rust",
            });
            respond(&mut io, 200, "application/json", body.to_string().as_bytes(), &[], head_only).await
        }
        _ => {
            let name = match path {
                "/" => "index.html",
                "/probe" => "probe.html",
                "/setup" => "setup.html",
                "/panel" => "panel.html",
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
    // Solo desde nuestras propias páginas: evita que otra web abierta en el ordenador se
    // conecte a localhost y controle el ratón o los ajustes.
    if let Some(origin) = headers.get("origin") {
        let netloc = origin.split_once("://").map_or(origin.as_str(), |(_, rest)| rest);
        if netloc != host {
            log::warn!("WebSocket rechazado por origen {origin}");
            return respond(&mut io, 403, "text/plain", b"", &[], false).await;
        }
    }
    let role = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("role="))
        .unwrap_or("viewer")
        .to_string();
    if role == "panel" && !peer.is_loopback() {
        return respond(&mut io, 403, "text/plain", b"", &[], false).await;
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
    let ws = WebSocketStream::from_partially_read(io, leftover, Role::Server, None).await;
    let (mut sink, stream) = ws.split();
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
    match role.as_str() {
        "panel" => panel_session(app, stream, tx).await,
        _ => viewer_session(app, stream, tx, peer, role).await,
    }
    let _ = writer.await;
    Ok(())
}

type WsStream = futures_util::stream::SplitStream<WebSocketStream<Box<dyn Io>>>;

/// Mensajes de texto del WebSocket ya convertidos a JSON (None al cerrarse).
async fn next_json(stream: &mut WsStream) -> Option<Value> {
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(t) => {
                if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                    return Some(v);
                }
            }
            Message::Close(_) => return None,
            _ => {}
        }
    }
    None
}

fn pong(tx: &mpsc::UnboundedSender<Message>, v: &Value) {
    let pong = json!({"t": "pong", "c": v["c"], "s": now_ms()});
    let _ = tx.send(Message::text(pong.to_string()));
}

async fn viewer_session(app: Arc<App>, mut stream: WsStream, tx: mpsc::UnboundedSender<Message>, peer: IpAddr, role: String) {
    let id = app.next_id.fetch_add(1, Ordering::Relaxed);
    let acked = Arc::new(AtomicU32::new(0));
    log::info!("Conectado {peer} ({role})");
    while let Some(v) = next_json(&mut stream).await {
        let kind = v["t"].as_str().unwrap_or("");
        if kind == "ping" {
            pong(&tx, &v);
            continue;
        }
        if role != "viewer" {
            continue; // la sonda solo sincroniza el reloj
        }
        let active = app.active_viewer.load(Ordering::Relaxed) == id;
        match kind {
            "hello" if !app.is_on() => {
                let bye = json!({"t": "bye", "reason": OFF_REASON, "retry": false});
                let _ = tx.send(Message::text(bye.to_string()));
                let _ = tx.send(Message::Close(None));
                break;
            }
            "hello" => {
                let given: String = v["key"].as_str().unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                match app.check_key(&given).await {
                    Some(true) => {}
                    Some(false) => {
                        log::warn!("Código de emparejamiento incorrecto desde {peer}");
                        let _ = tx.send(Message::text(json!({"t": "auth", "ok": false}).to_string()));
                        let _ = tx.send(Message::Close(None));
                        break;
                    }
                    None => {
                        log::warn!("Demasiados intentos de código a la vez; rechazado {peer}");
                        let bye = json!({"t": "bye", "reason": "Demasiados intentos de conexión. Espera unos segundos."});
                        let _ = tx.send(Message::text(bye.to_string()));
                        let _ = tx.send(Message::Close(None));
                        break;
                    }
                }
                if let Err(e) = app.attach(id, &tx, &acked, &v, peer).await {
                    if !app.is_on() {
                        let bye = json!({"t": "bye", "reason": OFF_REASON, "retry": false});
                        let _ = tx.send(Message::text(bye.to_string()));
                        let _ = tx.send(Message::Close(None));
                        break;
                    }
                    log::error!("No se pudo iniciar la captura: {e:#}");
                    let msg = json!({"t": "error", "message": format!("Error en el ordenador: {e:#}")});
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
            "stats" if active => app.client_stats(&v),
            _ => {}
        }
    }
    log::info!("Desconectado {peer} ({role})");
    app.detach(id).await;
}

/// Panel de control (solo desde este ordenador): recibe el estado cada segundo y en cuanto
/// cambia algo, y envía ajustes y acciones.
async fn panel_session(app: Arc<App>, mut stream: WsStream, tx: mpsc::UnboundedSender<Message>) {
    let send_state = |app: Arc<App>, tx: mpsc::UnboundedSender<Message>| async move {
        let _ = tx.send(Message::text(app.state().await.to_string()));
    };
    send_state(app.clone(), tx.clone()).await;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            v = next_json(&mut stream) => {
                let Some(v) = v else { break };
                let reply = match v["t"].as_str().unwrap_or("") {
                    "ping" => { pong(&tx, &v); continue; }
                    "set" => app.set_pref(v["key"].as_str().unwrap_or(""), &v["value"]).await,
                    "action" => app.action(v["name"].as_str().unwrap_or("")).await.map(Some),
                    _ => continue,
                };
                let notice = match reply {
                    Ok(Some(msg)) => Some(json!({"t": "notice", "ok": true, "message": msg})),
                    Ok(None) => None,
                    Err(e) => Some(json!({"t": "notice", "ok": false, "message": format!("{e:#}")})),
                };
                if let Some(n) = notice {
                    let _ = tx.send(Message::text(n.to_string()));
                }
                send_state(app.clone(), tx.clone()).await;
            }
            _ = tick.tick() => send_state(app.clone(), tx.clone()).await,
            _ = app.changed.notified() => send_state(app.clone(), tx.clone()).await,
        }
    }
}
