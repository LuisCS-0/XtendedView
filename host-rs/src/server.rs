//! Servidor HTTPS/WebSocket, sesiones de los dispositivos y panel de control del ordenador.
//!
//! - Cada puerto acepta HTTPS y HTTP (se mira el primer byte). Fuera de este ordenador, todo
//!   menos la página de configuración se redirige a HTTPS: la pantalla nunca viaja sin cifrar.
//! - Varios dispositivos a la vez, cada uno con sus ajustes (pantalla adicional o espejo, lado,
//!   monitor, fotogramas, calidad…) y por la conexión que quiera (red del ordenador, red propia
//!   o cable). Se autentican con el código de emparejamiento y, además, hay que aceptarlos en
//!   el panel del ordenador.
//! - Los ajustes se eligen en el panel; los dispositivos solo informan de su pantalla y
//!   muestran la imagen.
//! - Control de flujo por confirmaciones: si un dispositivo se retrasa, se descartan frames y se
//!   pide un keyframe en vez de acumular retraso.
use crate::capture::{self, OutputInfo};
use crate::certs::{self, CertManager};
use crate::clock::now_ms;
use crate::display::{self, Target};
use crate::hotspot::Hotspot;
use crate::input::{Injector, PointerEvent, Rect};
use crate::pipeline::{Capture, Controller, Event, Settings};
use crate::prefs::{self, DevicePrefs, Prefs};
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
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Lo único que se sirve sin cifrar a otros dispositivos: lo necesario para instalar el certificado.
const PLAIN_HTTP_PATHS: [&str; 6] = ["/setup", "/ca.crt", "/info", "/style.css", "/favicon.ico", "/icon-192.png"];
/// Solo para este ordenador: el panel de control (y sus códigos QR, todo lo de /qr/).
const LOCAL_ONLY_PATHS: [&str; 3] = ["/panel", "/panel.js", "/panel.css"];
/// Dispositivos a la vez.
const MAX_DEVICES: usize = 4;
/// Tiempo para aceptar un dispositivo en el panel.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(90);

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

// ------------------------------------------------------------------ sesiones

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

/// Lo que se pide a la captura: sale de la pantalla del dispositivo y de sus ajustes.
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
    fn new(c: &Client, d: &DevicePrefs) -> Self {
        Self {
            extend: d.mode == "extend",
            w: ((c.w as f64 * d.scale) as u32).max(160),
            h: ((c.h as f64 * d.scale) as u32).max(160),
            fps: if d.fps == 0 { c.hz.clamp(30, 120) } else { d.fps },
            bitrate: d.bitrate * 1_000_000,
            side: d.side.clone(),
            monitor: d.monitor,
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

/// Monitor virtual de un dispositivo.
struct Virtual {
    index: u32,
    name: String,
}

/// Codificador de un dispositivo dentro de la captura de un monitor.
struct Stream {
    /// Monitor (nombre de Windows) de la captura en la que está.
    output: String,
    settings: Settings,
    ctrl: Controller,
    forward: tokio::task::JoinHandle<()>,
}

/// Un dispositivo conectado y aceptado.
struct Session {
    conn: u64,
    device: String,
    tx: mpsc::UnboundedSender<Message>,
    acked: Arc<AtomicU32>,
    client: Client,
    via: &'static str,
    request: Option<Request>,
    stream: Option<Stream>,
    virt: Option<Virtual>,
    info: Option<StreamInfo>,
    viewer_stats: Value,
    host_stats: Arc<StdMutex<Value>>,
    last_log: Option<Instant>,
}

impl Session {
    fn send(&self, v: Value) {
        let _ = self.tx.send(Message::text(v.to_string()));
    }

    fn bye(&self, reason: &str, retry: bool) {
        self.send(json!({"t": "bye", "reason": reason, "retry": retry}));
        let _ = self.tx.send(Message::Close(None));
    }
}

#[derive(Default)]
struct Hub {
    sessions: Vec<Session>,
    /// Capturas en marcha, por monitor (nombre de Windows).
    captures: HashMap<String, Capture>,
}

impl Hub {
    fn index(&self, conn: u64) -> Option<usize> {
        self.sessions.iter().position(|s| s.conn == conn)
    }

    fn virtual_names(&self) -> HashSet<String> {
        self.sessions.iter().filter_map(|s| s.virt.as_ref().map(|v| v.name.clone())).collect()
    }
}

/// Saca el codificador de la sesión de su captura; si era el último, para la captura y espera a
/// que suelte el monitor (Windows no deja duplicar dos veces el mismo monitor desde el mismo
/// programa, así que la siguiente no puede empezar antes).
async fn stop_stream(hub: &mut Hub, i: usize) {
    let Some(st) = hub.sessions[i].stream.take() else { return };
    st.forward.abort();
    let conn = hub.sessions[i].conn;
    let empty = hub.captures.get_mut(&st.output).map(|c| {
        c.remove(conn);
        c.is_empty()
    });
    if empty == Some(true) {
        if let Some(cap) = hub.captures.remove(&st.output) {
            let _ = tokio::task::spawn_blocking(move || drop(cap)).await;
        }
    }
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

/// Posición para un monitor virtual de `w×h` en el lado `side` del principal, por fuera de los
/// que ya haya en ese lado (para que dos dispositivos a la derecha queden uno tras otro).
fn stacked_position(main: Rect, (w, h): (u32, u32), side: &str, others: &[Rect]) -> (i32, i32) {
    let (x, y) = display::side_position(main, (w, h), side);
    let (w, h) = (w as i32, h as i32);
    match side {
        "left" => (others.iter().map(|o| o.x).fold(main.x, i32::min) - w, y),
        "top" => (x, others.iter().map(|o| o.y).fold(main.y, i32::min) - h),
        "bottom" => (x, others.iter().map(|o| o.y + o.h).fold(main.y + main.h, i32::max)),
        _ => (others.iter().map(|o| o.x + o.w).fold(main.x + main.w, i32::max), y),
    }
}

/// Un dispositivo esperando a que lo acepten en el panel.
struct Pending {
    id: u64,
    device: String,
    name: String,
    ip: IpAddr,
    screen: String,
    via: &'static str,
    since: Instant,
    reply: Option<oneshot::Sender<bool>>,
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
    /// Monitor de cada conexión aceptada, para el táctil (sin bloquear el hub).
    rects: StdMutex<HashMap<u64, Rect>>,
    next_id: AtomicU64,
    injector: StdMutex<Injector>,
    cert_warned: StdMutex<HashSet<IpAddr>>,
    vdd: StdMutex<Option<Arc<Vdd>>>,
    prefs: StdMutex<Prefs>,
    hotspot: StdMutex<Option<Arc<Hotspot>>>,
    hotspot_error: StdMutex<Option<String>>,
    /// Encendido desde el panel: solo entonces se aceptan dispositivos y hay red propia.
    on: AtomicBool,
    /// Un encendido o apagado cada vez.
    power_lock: Mutex<()>,
    /// Turno para comprobar el código de emparejamiento (ver `check_key`).
    auth_turn: Mutex<()>,
    auth_waiting: AtomicU32,
    rejected: StdMutex<HashSet<IpAddr>>,
    /// Dispositivos aceptados en el panel desde que se encendió.
    approved: StdMutex<HashSet<String>>,
    pending: StdMutex<Vec<Pending>>,
    /// Avisa a los paneles abiertos de que algo ha cambiado.
    changed: Notify,
}

/// Lo que ve el dispositivo si intenta conectarse con la pantalla apagada.
const OFF_REASON: &str = "La pantalla está apagada en el ordenador. Enciéndela desde el panel y vuelve a escanear el código QR.";

/// Cómo se llama cada conexión en el panel.
fn via_name(via: &str) -> &'static str {
    match via {
        "own" => "Red propia",
        "cable" => "Cable USB",
        "self" => "Este ordenador",
        _ => "Red del ordenador",
    }
}

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
            rects: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            injector: StdMutex::new(Injector::new()),
            cert_warned: StdMutex::new(HashSet::new()),
            vdd: StdMutex::new(vdd.map(Arc::new)),
            prefs: StdMutex::new(prefs),
            hotspot: StdMutex::new(None),
            hotspot_error: StdMutex::new(None),
            on: AtomicBool::new(false),
            power_lock: Mutex::new(()),
            auth_turn: Mutex::new(()),
            auth_waiting: AtomicU32::new(0),
            rejected: StdMutex::new(HashSet::new()),
            approved: StdMutex::new(HashSet::new()),
            pending: StdMutex::new(Vec::new()),
            changed: Notify::new(),
        }))
    }

    pub fn key(&self) -> String {
        self.key.lock().unwrap().clone()
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    pub fn prefs(&self) -> Prefs {
        self.prefs.lock().unwrap().clone()
    }

    fn save_prefs(&self, p: &Prefs) {
        p.save(&prefs::path(&self.data));
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

    // -------------------------------------------------------------- seguridad

    /// Por qué conexión llega `peer`: "own" (red propia), "cable", "self" (este ordenador) o
    /// "local" (la red del ordenador).
    fn classify(&self, peer: IpAddr, ifs: &[sys::NetIf]) -> &'static str {
        match peer {
            _ if peer.is_loopback() => "self",
            IpAddr::V4(v) if v.octets()[..3] == [192, 168, 137] => "own",
            IpAddr::V4(v) if sys::cable_interface(ifs).is_some_and(|c| c.same_network(v)) => "cable",
            _ => "local",
        }
    }

    /// Quién puede conectarse a este ordenador.
    ///
    /// - Apagada: nadie de fuera (ni siquiera ve las páginas ni el certificado).
    /// - Encendida: solo por las conexiones activas. Con la red propia, por ejemplo, entran los
    ///   dispositivos unidos a ella, pero no los de la Wi-Fi de una cafetería en la que siga el
    ///   ordenador (salvo que también esté activa «Red del ordenador»).
    fn peer_allowed(&self, peer: IpAddr) -> bool {
        if peer.is_loopback() {
            return true;
        }
        if !self.is_on() {
            return false;
        }
        let via = self.classify(peer, &sys::interfaces());
        self.prefs().has(via)
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

    /// Espera a que se acepte el dispositivo en el panel. Los ya aceptados desde que se encendió
    /// no vuelven a preguntar (al reconectar tras un corte o al recargar la página).
    async fn wait_approval(self: &Arc<Self>, device: &str, client: &Client, via: &'static str, stream: &mut WsStream, tx: &mpsc::UnboundedSender<Message>) -> Option<bool> {
        if via == "self" || self.approved.lock().unwrap().contains(device) {
            return Some(true);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        self.pending.lock().unwrap().push(Pending {
            id,
            device: device.into(),
            name: client.name.clone(),
            ip: client.ip,
            screen: format!("{}x{}", client.w, client.h),
            via,
            since: Instant::now(),
            reply: Some(reply),
        });
        log::info!("{} ({}) quiere conectarse por {}: acéptalo en el panel", client.name, client.ip, via_name(via));
        let _ = tx.send(Message::text(
            json!({"t": "wait", "message": "Acepta la conexión en el panel del ordenador…"}).to_string(),
        ));
        sys::flash_panel_window("XtendedView");
        self.changed.notify_waiters();

        let deadline = tokio::time::sleep(APPROVAL_TIMEOUT);
        tokio::pin!(deadline);
        tokio::pin!(rx);
        let decision = loop {
            tokio::select! {
                r = &mut rx => break Some(r.unwrap_or(false)),
                _ = &mut deadline => break Some(false),
                v = next_json(stream) => match v {
                    Some(v) if v["t"] == "ping" => pong(tx, &v),
                    Some(_) => {}
                    None => break None, // el dispositivo se ha ido
                },
            }
        };
        self.pending.lock().unwrap().retain(|p| p.id != id);
        self.changed.notify_waiters();
        decision
    }

    /// Respuesta del panel a un dispositivo que espera.
    fn decide(&self, id: u64, accept: bool) -> Result<String> {
        let mut pending = self.pending.lock().unwrap();
        let p = pending.iter_mut().find(|p| p.id == id).context("ese dispositivo ya no está esperando")?;
        if accept {
            self.approved.lock().unwrap().insert(p.device.clone());
        }
        if let Some(r) = p.reply.take() {
            let _ = r.send(accept);
        }
        log::info!("{} {}", p.name, if accept { "aceptado" } else { "rechazado" });
        Ok(format!("{} {}", p.name, if accept { "aceptado" } else { "rechazado" }))
    }

    // -------------------------------------------------------------- encendido

    /// Enciende (código de emparejamiento nuevo y, si toca, red propia) o apaga (desconecta los
    /// dispositivos, quita los monitores virtuales y cierra la red propia).
    pub async fn power(self: &Arc<Self>, on: bool) -> Result<String> {
        let _one_at_a_time = self.power_lock.lock().await;
        if on == self.is_on() {
            return Ok(if on { "Ya estaba encendida" } else { "Ya estaba apagada" }.into());
        }
        self.rejected.lock().unwrap().clear();
        self.approved.lock().unwrap().clear();
        if on {
            let key = prefs::pairing_key(&self.data, true)?;
            *self.key.lock().unwrap() = key.clone();
            self.on.store(true, Ordering::SeqCst);
            self.sync_hotspot().await;
            log::info!("Encendida. Código de emparejamiento: {}", prefs::pretty_key(&key));
            self.print_pairing();
            self.changed.notify_waiters();
            Ok("Encendida: escanea el código QR con el dispositivo".into())
        } else {
            {
                // Con el hub bloqueado: ningún dispositivo puede colarse entre medias.
                let mut hub = self.hub.lock().await;
                self.on.store(false, Ordering::SeqCst);
                for s in &hub.sessions {
                    s.bye("La pantalla se ha apagado desde el ordenador.", false);
                }
                while !hub.sessions.is_empty() {
                    self.remove_session(&mut hub, 0).await;
                }
            }
            for p in self.pending.lock().unwrap().iter_mut() {
                if let Some(r) = p.reply.take() {
                    let _ = r.send(false);
                }
            }
            self.sync_hotspot().await;
            log::info!("Apagada");
            self.changed.notify_waiters();
            Ok("Apagada".into())
        }
    }

    /// Antes de encender: la conexión elegida en la pantalla de inicio pasa a ser la única activa.
    fn choose_connection(&self, connection: &str) {
        if self.is_on() {
            return;
        }
        let mut p = self.prefs.lock().unwrap();
        p.connections = vec![connection.to_string()];
        self.save_prefs(&p);
    }

    /// Activa o desactiva una conexión (con la pantalla encendida, se crea o cierra la red propia).
    async fn set_connection(self: &Arc<Self>, connection: &str, enabled: bool) -> Result<String> {
        {
            let mut p = self.prefs.lock().unwrap();
            if !p.set_connection(connection, enabled) {
                bail!("conexión desconocida");
            }
            self.save_prefs(&p);
        }
        let _one_at_a_time = self.power_lock.lock().await;
        self.sync_hotspot().await;
        if enabled && connection == "own" {
            if let Some(e) = self.hotspot_error.lock().unwrap().clone() {
                bail!("no se pudo crear la red propia: {e}");
            }
        }
        if !enabled {
            // Los dispositivos que entraron por esa conexión se desconectan.
            let hub = self.hub.lock().await;
            for s in hub.sessions.iter().filter(|s| s.via == connection) {
                s.bye("Se ha desactivado esta conexión en el ordenador.", true);
            }
        }
        self.changed.notify_waiters();
        let name = via_name(connection);
        Ok(match (enabled, connection) {
            (true, "cable") if sys::cable_interface(&sys::interfaces()).is_none() => {
                format!("{name} activado: conecta el móvil y activa «Módem USB»")
            }
            (true, _) => format!("{name} activada"),
            (false, _) => format!("{name} desactivada"),
        })
    }

    /// Hace que Windows no salga a internet por el cable (con el anclaje USB, el móvil comparte
    /// su conexión y gastaría sus datos): se sube la métrica de esa tarjeta para que prefiera la
    /// Wi-Fi o la red del ordenador. Windows lo recuerda para esa tarjeta; sin otra conexión,
    /// seguiría usando el móvil. Pide permiso de administrador.
    async fn fix_cable_route(&self) -> Result<String> {
        let cable = sys::cable_interface(&sys::interfaces()).cloned().context("no hay ningún cable conectado")?;
        let params = format!(
            "-NoProfile -ExecutionPolicy Bypass -Command \"Set-NetIPInterface -InterfaceIndex {} -InterfaceMetric 5000 -ErrorAction Stop\"",
            cable.index
        );
        match tokio::task::spawn_blocking(move || sys::run_elevated("powershell.exe", &params)).await? {
            Ok(0) => {
                log::info!("{}: métrica 5000, Windows ya no sale a internet por el cable", cable.name);
                self.changed.notify_waiters();
                Ok("Listo: el ordenador seguirá usando su Wi-Fi o su red para internet".into())
            }
            Ok(code) => bail!("Windows no ha aceptado el cambio (código {code})"),
            Err(e) => Err(e),
        }
    }

    /// Avisa cuando se conecta o se desconecta el cable y, al conectarse, pone su dirección en
    /// el certificado enseguida (sin esperar a la revisión periódica) para que el dispositivo
    /// no vea el aviso de seguridad.
    pub async fn cable_watch(self: Arc<Self>) {
        let mut last: Option<(String, Ipv4Addr)> = None;
        loop {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let now = if self.is_on() && self.prefs().has("cable") {
                tokio::task::spawn_blocking(|| {
                    let ifs = sys::interfaces();
                    sys::cable_interface(&ifs).and_then(|c| c.ipv4.first().map(|(ip, _)| (c.name.clone(), *ip)))
                })
                .await
                .ok()
                .flatten()
            } else {
                None
            };
            if now == last {
                continue;
            }
            match (&now, &last) {
                (Some((name, ip)), _) => {
                    log::info!("Cable conectado: {name} ({ip})");
                    self.refresh_cert();
                    self.print_pairing();
                }
                (None, Some((name, _))) => log::info!("Cable desconectado ({name})"),
                (None, None) => {}
            }
            last = now;
            self.changed.notify_waiters();
        }
    }

    /// Reemite el certificado del servidor si hay direcciones nuevas.
    fn refresh_cert(&self) {
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

    /// Dirección y código QR en la consola, para quien no tenga el panel delante.
    fn print_pairing(&self) {
        let mut any = false;
        for c in self.prefs().connections {
            if let Some(url) = self.viewer_url(&c) {
                any = true;
                println!("\n {}: en el dispositivo, escanea este código o abre {url}", via_name(&c));
                if let Ok(qr) = sys::qr_text(&url) {
                    for l in qr.lines() {
                        println!("   {l}");
                    }
                }
            }
        }
        if !any {
            println!("\n Sin conexión disponible todavía: el panel mostrará el código QR cuando la haya.");
        }
    }

    // -------------------------------------------------------------- red

    /// Crea la red Wi-Fi propia si está activa y la pantalla encendida; si no, la cierra.
    async fn sync_hotspot(self: &Arc<Self>) {
        let want = self.is_on() && self.prefs().has("own");
        if !want {
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

    /// IP del ordenador en una conexión (la que tiene que abrir el dispositivo).
    fn connection_ip(&self, connection: &str) -> Option<Ipv4Addr> {
        match connection {
            "own" => {
                let ok = self.hotspot.lock().unwrap().is_some();
                ok.then(|| {
                    certs::local_ipv4s()
                        .into_iter()
                        .find(|ip| ip.octets()[..3] == [192, 168, 137])
                        .unwrap_or(Ipv4Addr::new(192, 168, 137, 1))
                })
            }
            "cable" => sys::cable_interface(&sys::interfaces()).and_then(|c| c.ipv4.first().map(|(ip, _)| *ip)),
            _ => sys::lan_ip(),
        }
    }

    fn viewer_url(&self, connection: &str) -> Option<String> {
        if !self.is_on() || !self.prefs().has(connection) {
            return None;
        }
        self.connection_ip(connection).map(|ip| format!("https://{ip}:{}/#k={}", self.args.port, self.key()))
    }

    fn setup_url(&self) -> Option<String> {
        let p = self.prefs();
        p.connections
            .iter()
            .find_map(|c| self.connection_ip(c))
            .map(|ip| format!("http://{ip}:{}/setup", self.args.http_port))
    }

    fn wifi_qr(&self) -> Option<String> {
        let h = self.hotspot.lock().unwrap().clone()?;
        Some(sys::wifi_qr_text(&h.ssid, &h.password))
    }

    // -------------------------------------------------------------- pantalla

    /// Crea (si hace falta) el monitor virtual de la sesión, le pone el modo que mejor encaja con
    /// el dispositivo y lo coloca en su lado (por fuera de los de otros dispositivos en ese lado).
    async fn prepare_virtual(&self, hub: &mut Hub, i: usize, req: &Request) -> Result<(OutputInfo, Target, bool)> {
        let vdd = self.vdd()?;
        if hub.sessions[i].virt.is_none() {
            let (index, out) = tokio::task::spawn_blocking(move || display::create_virtual(&vdd)).await??;
            log::info!("Monitor virtual conectado: {} (índice {index} en Parsec VDD)", out.name);
            hub.sessions[i].virt = Some(Virtual { index, name: out.name });
        }
        let name = hub.sessions[i].virt.as_ref().map(|v| v.name.clone()).unwrap_or_default();
        // Otros monitores virtuales en el mismo lado.
        let same_side: HashSet<String> = hub
            .sessions
            .iter()
            .enumerate()
            .filter(|(j, s)| *j != i && s.request.as_ref().is_some_and(|r| r.extend && r.side == req.side))
            .filter_map(|(_, s)| s.virt.as_ref().map(|v| v.name.clone()))
            .collect();
        let (w, h, fps, side) = (req.w, req.h, req.fps, req.side.clone());
        tokio::task::spawn_blocking(move || -> Result<(OutputInfo, Target, bool)> {
            let (target, exact) =
                display::best_target(&display::modes(&name), w, h, fps).context("el monitor virtual no tiene modos")?;
            let mut changed = display::current(&name) != Some(target);
            if changed {
                display::apply(&name, target)?;
            }
            let dims = target.desktop();
            let outputs = capture::outputs()?;
            // El monitor principal es el que está en el origen del escritorio.
            let main = outputs.iter().find(|o| o.name != name && o.rect.x == 0 && o.rect.y == 0);
            let others: Vec<Rect> = outputs.iter().filter(|o| same_side.contains(&o.name)).map(|o| o.rect).collect();
            let at = main.map(|m| stacked_position(m.rect, dims, &side, &others));
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

    async fn remove_virtual(&self, hub: &mut Hub, i: usize) {
        if let Some(v) = hub.sessions[i].virt.take() {
            stop_stream(hub, i).await; // primero se deja de capturar ese monitor
            if let Ok(vdd) = self.vdd() {
                let index = v.index;
                let _ = tokio::task::spawn_blocking(move || vdd.remove(index)).await;
            }
            log::info!("Monitor virtual {} quitado", v.name);
        }
    }

    /// Monitores reales (sin los virtuales de los dispositivos): los que se pueden duplicar.
    fn real_monitors(&self, hub: &Hub) -> Vec<OutputInfo> {
        let virt = hub.virtual_names();
        capture::outputs().unwrap_or_default().into_iter().filter(|o| !virt.contains(&o.name)).collect()
    }

    fn send_viewer_settings(&self, s: &Session) {
        let p = self.prefs();
        let d = p.device(&s.device);
        s.send(json!({"t": "settings", "stats": d.stats, "measure": p.measure, "fullscreen": d.fullscreen}));
    }

    /// Ajusta la captura de una sesión a su dispositivo y a sus ajustes.
    async fn apply(self: &Arc<Self>, hub: &mut Hub, i: usize) -> Result<()> {
        let d = self.prefs().device(&hub.sessions[i].device);
        let req = Request::new(&hub.sessions[i].client, &d);
        let s = &mut hub.sessions[i];
        if let Some(st) = s.stream.as_mut() {
            if s.request.as_ref() == Some(&req) {
                return Ok(()); // nada que cambiar
            }
            if let Some(prev) = s.request.clone() {
                if (Request { bitrate: req.bitrate, ..prev }) == req {
                    st.ctrl.bitrate(req.bitrate); // solo cambia la calidad: sin reiniciar
                    st.settings.bitrate = req.bitrate;
                    log::info!("{}: calidad {} Mbps", s.client.name, req.bitrate / 1_000_000);
                    s.request = Some(req);
                    return Ok(());
                }
            }
        }

        stop_stream(hub, i).await; // se para su captura antes de tocar los monitores
        let tx = hub.sessions[i].tx.clone();
        let info = |msg: &str| {
            let _ = tx.send(Message::text(json!({"t": "info", "message": msg}).to_string()));
        };
        let mut extend = req.extend;
        let mut target = None;
        let mut register = None;
        let (mut exact, mut portrait) = (true, false);
        if extend {
            match self.prepare_virtual(hub, i, &req).await {
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
                    self.remove_virtual(hub, i).await;
                }
            }
        } else {
            self.remove_virtual(hub, i).await;
        }
        let (mon, width, height, fps) = match target {
            Some(t) => t,
            None => {
                let monitors = self.real_monitors(hub);
                let n = monitors.len();
                let mon = monitors.into_iter().nth(req.monitor.min(n.saturating_sub(1))).context("no hay monitores")?;
                let (w, h) = mirror_size(&mon, &req);
                (mon, w, h, req.fps)
            }
        };
        let settings = Settings { monitor: mon.index, width: even(width), height: even(height), fps, bitrate: req.bitrate };
        let s = &mut hub.sessions[i];
        self.rects.lock().unwrap().insert(s.conn, mon.rect);
        log::info!(
            "{}: {} {}x{} a {} fps, {} Mbps desde {} ({}x{})",
            s.client.name,
            if extend { "pantalla adicional" } else { "espejo" },
            settings.width, settings.height, settings.fps, settings.bitrate / 1_000_000, mon.name, mon.rect.w, mon.rect.h
        );
        s.info = Some(StreamInfo {
            extend,
            width: settings.width,
            height: settings.height,
            fps: settings.fps,
            portrait,
            exact,
            register,
            monitor: mon.name.clone(),
        });
        s.request = Some(req);
        let (conn, acked, host_stats, name) = (s.conn, s.acked.clone(), s.host_stats.clone(), s.client.name.clone());
        let capture = hub.captures.entry(mon.name.clone()).or_insert_with(|| Capture::start(mon.index));
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let ctrl = capture.add(conn, settings.clone(), ev_tx);
        let forward = tokio::spawn(forward(
            ev_rx,
            tx,
            acked,
            ctrl.clone(),
            settings.clone(),
            extend,
            self.args.max_inflight_ms,
            host_stats,
            name,
        ));
        let s = &mut hub.sessions[i];
        s.stream = Some(Stream { output: mon.name, settings, ctrl, forward });
        self.send_viewer_settings(s);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Vuelve a aplicar los ajustes de un dispositivo, si está conectado.
    async fn reconfigure_device(self: &Arc<Self>, device: &str) {
        let mut hub = self.hub.lock().await;
        if let Some(i) = hub.sessions.iter().position(|s| s.device == device) {
            if let Err(e) = self.apply(&mut hub, i).await {
                log::error!("No se pudo aplicar el cambio: {e:#}");
            }
        }
    }

    /// Da de alta (o actualiza, si solo ha girado o plegado la pantalla) la sesión de una conexión.
    #[allow(clippy::too_many_arguments)]
    async fn attach(
        self: &Arc<Self>,
        conn: u64,
        device: &str,
        via: &'static str,
        tx: &mpsc::UnboundedSender<Message>,
        acked: &Arc<AtomicU32>,
        hello: &Value,
        ip: IpAddr,
    ) -> Result<()> {
        let mut hub = self.hub.lock().await;
        if !self.is_on() {
            bail!("apagada"); // se ha apagado mientras el dispositivo se conectaba
        }
        let mut client = Client::parse(hello, ip);
        if let Some(i) = hub.index(conn) {
            client.since = hub.sessions[i].client.since; // solo ha girado o plegado la pantalla
            hub.sessions[i].client = client;
            return self.apply(&mut hub, i).await;
        }
        // El mismo dispositivo con otra conexión (recargó la página, se cortó la red…): la nueva
        // sustituye a la antigua y se queda con su monitor virtual.
        let mut inherited = None;
        if let Some(j) = hub.sessions.iter().position(|s| s.device == device) {
            stop_stream(&mut hub, j).await;
            let old = hub.sessions.remove(j);
            old.bye("Este dispositivo se ha vuelto a conectar.", false);
            self.injector.lock().unwrap().release(old.conn);
            self.rects.lock().unwrap().remove(&old.conn);
            inherited = old.virt;
        } else if hub.sessions.len() >= MAX_DEVICES {
            bail!("ya hay {MAX_DEVICES} dispositivos conectados");
        }
        // Un dispositivo nuevo va a un lado libre, para no tapar a los demás.
        let used: Vec<String> = hub
            .sessions
            .iter()
            .filter_map(|s| s.request.as_ref().filter(|r| r.extend).map(|r| r.side.clone()))
            .collect();
        let free = ["right", "left", "top", "bottom"].into_iter().find(|s| !used.iter().any(|u| u == s));
        {
            let mut p = self.prefs.lock().unwrap();
            p.remember_device(device, &client.name, free);
            self.save_prefs(&p);
        }
        log::info!("Dispositivo: {} ({ip}, {}), pantalla {}x{} a {} Hz", client.name, via_name(via), client.w, client.h, client.hz);
        hub.sessions.push(Session {
            conn,
            device: device.into(),
            tx: tx.clone(),
            acked: acked.clone(),
            client,
            via,
            request: None,
            stream: None,
            virt: inherited,
            info: None,
            viewer_stats: Value::Null,
            host_stats: Arc::new(StdMutex::new(Value::Null)),
            last_log: None,
        });
        let i = hub.sessions.len() - 1;
        self.changed.notify_waiters();
        self.apply(&mut hub, i).await
    }

    async fn detach(&self, conn: u64) {
        let mut hub = self.hub.lock().await;
        if let Some(i) = hub.index(conn) {
            self.remove_session(&mut hub, i).await;
        }
    }

    /// Olvida un dispositivo: deja de capturar, suelta su táctil y quita su monitor virtual.
    async fn remove_session(&self, hub: &mut Hub, i: usize) {
        stop_stream(hub, i).await;
        self.remove_virtual(hub, i).await;
        let s = hub.sessions.remove(i);
        self.injector.lock().unwrap().release(s.conn);
        self.rects.lock().unwrap().remove(&s.conn);
        log::info!("{} desconectado", s.client.name);
        self.changed.notify_waiters();
    }

    async fn client_stats(&self, conn: u64, m: &Value) {
        let mut hub = self.hub.lock().await;
        let Some(i) = hub.index(conn) else { return };
        let s = &mut hub.sessions[i];
        s.viewer_stats = m.clone();
        if s.last_log.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        s.last_log = Some(Instant::now());
        let f = |k: &str| m[k].as_f64().unwrap_or(0.0);
        let total = if f("e2e") > 0.0 { format!(", total {:.0} ms (p95 {:.0})", f("e2e"), f("e2e95")) } else { String::new() };
        log::info!(
            "{}: {:.0} fps, {:.1} Mbps, red {:.1} ms, decodificación {:.1} ms, RTT {:.1} ms{total}",
            s.client.name, f("fps"), f("mbps"), f("net"), f("dec"), f("rtt")
        );
    }

    // -------------------------------------------------------------- panel

    /// Ajuste general (de momento, medir la latencia en todos los dispositivos).
    async fn set_pref(self: &Arc<Self>, key: &str, value: &Value) -> Result<()> {
        if key != "measure" {
            bail!("ajuste desconocido «{key}»");
        }
        {
            let mut p = self.prefs.lock().unwrap();
            p.measure = value.as_bool().unwrap_or(p.measure);
            self.save_prefs(&p);
        }
        let hub = self.hub.lock().await;
        for s in &hub.sessions {
            self.send_viewer_settings(s);
        }
        self.changed.notify_waiters();
        Ok(())
    }

    /// Ajuste de un dispositivo.
    async fn set_device(self: &Arc<Self>, device: &str, key: &str, value: &Value) -> Result<()> {
        {
            let mut p = self.prefs.lock().unwrap();
            if !p.set_device(device, key, value) {
                bail!("valor no válido para «{key}»");
            }
            self.save_prefs(&p);
        }
        match key {
            "stats" | "fullscreen" => {
                let hub = self.hub.lock().await;
                if let Some(s) = hub.sessions.iter().find(|s| s.device == device) {
                    self.send_viewer_settings(s);
                }
            }
            _ => self.reconfigure_device(device).await,
        }
        self.changed.notify_waiters();
        Ok(())
    }

    /// Acciones del panel. Devuelve un mensaje para mostrar.
    async fn action(self: &Arc<Self>, name: &str, device: &str) -> Result<String> {
        match name {
            // Encender eligiendo la conexión en la pantalla de inicio ("powerOn" usa las guardadas).
            "powerOn" => self.power(true).await,
            "powerOnLocal" | "powerOnOwn" | "powerOnCable" => {
                let connection = match name {
                    "powerOnOwn" => "own",
                    "powerOnCable" => "cable",
                    _ => "local",
                };
                self.choose_connection(connection);
                self.power(true).await?;
                Ok(match connection {
                    "cable" if sys::cable_interface(&sys::interfaces()).is_none() => {
                        "Encendida: conecta el móvil con el cable USB y activa «Módem USB»"
                    }
                    "cable" => "Encendida por cable: escanea el código QR con el móvil",
                    "own" if self.hotspot_error.lock().unwrap().is_some() => "Encendida, pero no se pudo crear la red propia",
                    "own" => "Encendida con red propia: escanea los dos códigos QR",
                    _ => "Encendida: escanea el código QR con el dispositivo",
                }
                .into())
            }
            "powerOff" => self.power(false).await,
            "fixCableRoute" => self.fix_cable_route().await,
            "disconnect" => {
                let hub = self.hub.lock().await;
                let s = hub.sessions.iter().find(|s| s.device == device).context("ese dispositivo no está conectado")?;
                s.bye("Desconectado desde el ordenador.", true);
                Ok(format!("{} desconectado", s.client.name))
            }
            "newCode" => {
                if !self.is_on() {
                    bail!("la pantalla está apagada: al encenderla ya se crea un código nuevo");
                }
                let key = prefs::pairing_key(&self.data, true)?;
                *self.key.lock().unwrap() = key.clone();
                self.approved.lock().unwrap().clear();
                let hub = self.hub.lock().await;
                for s in &hub.sessions {
                    s.bye("El código ha cambiado: vuelve a escanear el QR.", false);
                }
                log::info!("Código de emparejamiento nuevo: {}", prefs::pretty_key(&key));
                Ok(format!("Código nuevo: {}", prefs::pretty_key(&key)))
            }
            "openProbe" => {
                sys::open_url(&format!("http://localhost:{}/probe", self.args.http_port));
                Ok("Sonda abierta: déjala visible en la pantalla que se envía al dispositivo".into())
            }
            "registerResolution" => self.register_resolution(device).await,
            _ => bail!("acción desconocida"),
        }
    }

    /// Registra la resolución exacta de un dispositivo en el driver (pide permiso de
    /// administrador). El script reinicia el adaptador virtual, así que se quitan antes todos los
    /// monitores virtuales y luego se vuelven a crear.
    async fn register_resolution(self: &Arc<Self>, device: &str) -> Result<String> {
        let spec = {
            let hub = self.hub.lock().await;
            hub.sessions
                .iter()
                .find(|s| s.device == device)
                .and_then(|s| s.info.as_ref().and_then(|i| i.register.clone()))
                .context("la resolución ya es exacta")?
        };
        let script = self.root.join("tools").join("vdd-resoluciones.ps1");
        let params = format!("-NoProfile -ExecutionPolicy Bypass -File \"{}\" -Add {spec}", script.display());
        {
            let mut hub = self.hub.lock().await;
            for i in 0..hub.sessions.len() {
                if hub.sessions[i].virt.is_some() {
                    self.remove_virtual(&mut hub, i).await;
                    hub.sessions[i].request = None;
                }
            }
        }
        let result = tokio::task::spawn_blocking(move || sys::run_elevated("powershell.exe", &params)).await?;
        // Tras reiniciar el adaptador hay que volver a abrir el driver.
        *self.vdd.lock().unwrap() = None;
        tokio::time::sleep(Duration::from_secs(2)).await;
        {
            let mut hub = self.hub.lock().await;
            for i in 0..hub.sessions.len() {
                if let Err(e) = self.apply(&mut hub, i).await {
                    log::error!("No se pudo volver a aplicar: {e:#}");
                }
            }
        }
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
        let ifs = sys::interfaces();
        let cable_if = sys::cable_interface(&ifs);
        let route = cable_if.map(|c| match sys::cable_route(&ifs, c) {
            sys::CableRoute::Fine => "fine",
            sys::CableRoute::PreferredOverOther => "preferred",
            sys::CableRoute::OnlyRoute => "only",
        });
        let conn_state = |c: &str| {
            let ip = self.connection_ip(c);
            json!({
                "enabled": p.has(c),
                "ready": on && p.has(c) && ip.is_some(),
                "ip": ip.map(|i| i.to_string()),
                "url": self.viewer_url(c),
            })
        };
        let mut local = conn_state("local");
        let lan = sys::lan_ip();
        local["link"] = json!(lan.and_then(|ip| sys::interface_of(&ifs, ip)).map(|i| json!({"name": i.name, "wifi": i.wifi})));
        let mut own = conn_state("own");
        own["ssid"] = json!(hotspot.as_ref().map(|h| h.ssid.clone()));
        own["password"] = json!(hotspot.as_ref().map(|h| h.password.clone()));
        own["error"] = json!(self.hotspot_error.lock().unwrap().clone());
        own["otherWifi"] = json!(sys::on_other_wifi(&ifs));
        let mut cable = conn_state("cable");
        cable["detected"] = json!(cable_if.map(|c| json!({"name": c.name, "route": route})));
        let vdd = match self.vdd.lock().unwrap().as_ref() {
            Some(v) => json!({"available": true, "version": v.version}),
            None => json!({"available": false}),
        };
        let pending: Vec<Value> = self
            .pending
            .lock()
            .unwrap()
            .iter()
            .map(|q| {
                json!({
                    "id": q.id, "device": q.device, "name": q.name, "ip": q.ip.to_string(), "screen": q.screen,
                    "via": via_name(q.via), "waiting": q.since.elapsed().as_secs(),
                    "left": APPROVAL_TIMEOUT.as_secs().saturating_sub(q.since.elapsed().as_secs()),
                })
            })
            .collect();
        let hub = self.hub.lock().await;
        let devices: Vec<Value> = hub
            .sessions
            .iter()
            .map(|s| {
                let c = &s.client;
                json!({
                    "device": s.device,
                    "name": c.name, "ip": c.ip.to_string(), "screen": format!("{}x{}", c.w, c.h),
                    "hz": c.hz, "since": c.since.elapsed().as_secs(), "via": via_name(s.via),
                    "prefs": p.device(&s.device),
                    "stream": s.info.as_ref().map(|i| json!({
                        "extend": i.extend, "width": i.width, "height": i.height, "fps": i.fps,
                        "portrait": i.portrait, "exact": i.exact, "register": i.register, "monitor": i.monitor,
                    })),
                    "viewerStats": s.viewer_stats,
                    "hostStats": s.host_stats.lock().unwrap().clone(),
                })
            })
            .collect();
        let monitors: Vec<Value> = self
            .real_monitors(&hub)
            .into_iter()
            .enumerate()
            .map(|(i, o)| json!({"index": i, "name": o.name, "w": o.rect.w, "h": o.rect.h}))
            .collect();
        drop(hub);
        json!({
            "t": "state",
            "on": on,
            "prefs": {"connections": p.connections, "measure": p.measure},
            "code": on.then(|| prefs::pretty_key(&self.key())),
            "connections": {"local": local, "own": own, "cable": cable},
            "setupUrl": self.setup_url(),
            "pending": pending,
            "devices": devices,
            "maxDevices": MAX_DEVICES,
            "vdd": vdd,
            "monitors": monitors,
            "ports": {"https": self.args.port, "http": self.args.http_port},
            "caFingerprint": self.ca_fingerprint,
        })
    }

    /// Código QR de la dirección de una conexión ("url-local", "url-own", "url-cable"), de la red
    /// Wi-Fi propia ("wifi") o de la página de configuración ("setup").
    fn qr(&self, which: &str) -> Option<String> {
        let text = match which {
            "wifi" => self.wifi_qr()?,
            "setup" => self.setup_url()?,
            w => self.viewer_url(w.strip_prefix("url-")?)?,
        };
        sys::qr_svg(&text).ok()
    }

    // -------------------------------------------------------------- tareas de fondo

    /// Si aparece una IP nueva (p. ej. al activar la red propia), se reemite el certificado.
    pub async fn watch_ips(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            self.refresh_cert();
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
    name: String,
) {
    let limit = ((s.fps as f64 * max_inflight_ms / 1000.0).round() as u32).max(3);
    let send_json = |v: Value| {
        let _ = tx.send(Message::text(v.to_string()));
    };
    // El número de secuencia sigue desde el último confirmado: el dispositivo confirma por
    // número y una captura nueva no debe parecer atrasada.
    let mut seq = acked.load(Ordering::Relaxed);
    let mut last_sent = seq;
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
                        log::debug!("{name}: codificador {encoder}");
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
                        log::error!("{name}: captura/codificación: {e}");
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
            let why = if app.is_on() { "llega por una conexión que no está activa" } else { "la pantalla está apagada" };
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

    if !local && (LOCAL_ONLY_PATHS.contains(&path) || path.starts_with("/qr/")) {
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
            let disp = ("Content-Disposition", "attachment; filename=\"xtendedview-ca.crt\"".to_string());
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
        "png" => "image/png",
        "ico" => "image/x-icon",
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

/// Identificador del dispositivo que manda su navegador (o, si es un visor antiguo, su IP).
fn device_id(hello: &Value, peer: IpAddr) -> String {
    let id: String = hello["id"].as_str().unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric()).take(40).collect();
    if id.len() >= 8 {
        id
    } else {
        format!("ip{}", peer.to_string().replace(['.', ':'], "-"))
    }
}

async fn viewer_session(app: Arc<App>, mut stream: WsStream, tx: mpsc::UnboundedSender<Message>, peer: IpAddr, role: String) {
    let conn = app.next_id.fetch_add(1, Ordering::Relaxed);
    let acked = Arc::new(AtomicU32::new(0));
    let send = |v: Value| {
        let _ = tx.send(Message::text(v.to_string()));
    };
    let bye = |reason: &str, retry: bool| {
        let _ = tx.send(Message::text(json!({"t": "bye", "reason": reason, "retry": retry}).to_string()));
        let _ = tx.send(Message::Close(None));
    };
    log::debug!("Conectado {peer} ({role})");
    // Aceptado en esta conexión: los "hello" siguientes (al girar o plegar) solo actualizan.
    let mut accepted = false;
    while let Some(v) = next_json(&mut stream).await {
        let kind = v["t"].as_str().unwrap_or("");
        if kind == "ping" {
            pong(&tx, &v);
            continue;
        }
        if role != "viewer" {
            continue; // la sonda solo sincroniza el reloj
        }
        match kind {
            "hello" if !app.is_on() => {
                bye(OFF_REASON, false);
                break;
            }
            "hello" => {
                let device = device_id(&v, peer);
                let via = app.classify(peer, &sys::interfaces());
                if !accepted {
                    let given: String = v["key"].as_str().unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                    match app.check_key(&given).await {
                        Some(true) => {}
                        Some(false) => {
                            log::warn!("Código de emparejamiento incorrecto desde {peer}");
                            send(json!({"t": "auth", "ok": false}));
                            let _ = tx.send(Message::Close(None));
                            break;
                        }
                        None => {
                            log::warn!("Demasiados intentos de código a la vez; rechazado {peer}");
                            bye("Demasiados intentos de conexión. Espera unos segundos.", true);
                            break;
                        }
                    }
                    let client = Client::parse(&v, peer);
                    match app.wait_approval(&device, &client, via, &mut stream, &tx).await {
                        Some(true) => accepted = true,
                        Some(false) => {
                            bye("El ordenador no ha aceptado la conexión.", true);
                            break;
                        }
                        None => break,
                    }
                }
                if let Err(e) = app.attach(conn, &device, via, &tx, &acked, &v, peer).await {
                    if !app.is_on() {
                        bye(OFF_REASON, false);
                        break;
                    }
                    log::error!("No se pudo conectar el dispositivo: {e:#}");
                    if e.to_string().contains("dispositivos conectados") {
                        bye(&format!("No se puede conectar: {e:#}."), true);
                        break;
                    }
                    send(json!({"t": "error", "message": format!("Error en el ordenador: {e:#}")}));
                }
            }
            "ack" if accepted => {
                acked.fetch_max(v["seq"].as_u64().unwrap_or(0) as u32, Ordering::Relaxed);
            }
            "pointer" if accepted => {
                let rect = app.rects.lock().unwrap().get(&conn).copied();
                if let (Some(rect), Ok(ev)) = (rect, serde_json::from_value::<PointerEvent>(v)) {
                    app.injector.lock().unwrap().pointer(conn, rect, &ev);
                }
            }
            "wheel" if accepted => {
                let (dx, dy) = (v["dx"].as_f64().unwrap_or(0.0), v["dy"].as_f64().unwrap_or(0.0));
                app.injector.lock().unwrap().wheel(dx, dy);
            }
            "stats" if accepted => app.client_stats(conn, &v).await,
            _ => {}
        }
    }
    log::debug!("Desconectado {peer} ({role})");
    app.detach(conn).await;
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
                let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
                let reply: Result<Option<String>> = match v["t"].as_str().unwrap_or("") {
                    "ping" => { pong(&tx, &v); continue; }
                    "set" => app.set_pref(&s("key"), &v["value"]).await.map(|_| None),
                    "setDevice" => app.set_device(&s("device"), &s("key"), &v["value"]).await.map(|_| None),
                    "connection" => app.set_connection(&s("kind"), v["enabled"].as_bool().unwrap_or(false)).await.map(Some),
                    "decide" => app.decide(v["id"].as_u64().unwrap_or(0), v["accept"].as_bool().unwrap_or(false)).map(Some),
                    "action" => app.action(&s("name"), &s("device")).await.map(Some),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacks_virtual_monitors_on_the_same_side() {
        let main = Rect { x: 0, y: 0, w: 2880, h: 1800 };
        // Primero a la derecha, junto al portátil.
        assert_eq!(stacked_position(main, (1920, 1200), "right", &[]), (2880, 300));
        // Otro a la derecha: por fuera del primero.
        let first = Rect { x: 2880, y: 300, w: 1920, h: 1200 };
        assert_eq!(stacked_position(main, (1600, 1200), "right", &[first]), (4800, 300));
        // A la izquierda de otro que ya está a la izquierda.
        let left = Rect { x: -1920, y: 300, w: 1920, h: 1200 };
        assert_eq!(stacked_position(main, (1600, 1000), "left", &[left]), (-3520, 400));
    }

    #[test]
    fn device_ids_are_sanitized() {
        let ip: IpAddr = "192.168.0.20".parse().unwrap();
        assert_eq!(device_id(&json!({"id": "a1b2c3d4e5f6"}), ip), "a1b2c3d4e5f6");
        assert_eq!(device_id(&json!({"id": "../../x"}), ip), "ip192-168-0-20");
        assert_eq!(device_id(&json!({}), ip), "ip192-168-0-20");
    }
}
