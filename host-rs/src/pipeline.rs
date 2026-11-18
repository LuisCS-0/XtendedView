//! Captura por monitor y codificación por dispositivo:
//! Desktop Duplication → procesador de vídeo → QuickSync.
//!
//! Windows solo deja duplicar cada monitor una vez por programa, así que hay un hilo de captura
//! por monitor (`Capture`) y, dentro, un codificador por cada dispositivo que lo muestra
//! (`Sink`). Dos dispositivos en espejo del mismo monitor comparten la captura y cada uno recibe
//! la imagen a su tamaño, fotogramas y calidad.
//!
//! Cada codificador asíncrono pide entrada (NeedInput) y entrega salida (HaveOutput). La imagen
//! capturada se copia a una textura compartida que siempre tiene la más reciente: un
//! codificador que estaba ocupado cuando cambió la pantalla la convierte en cuanto queda libre,
//! así que ningún dispositivo se queda con una imagen antigua aunque la pantalla no vuelva a
//! cambiar. No hay colas intermedias.
use crate::capture::{Acquire, CursorShape, Duplicator, Gpu};
use crate::clock::now_ms;
use crate::convert::{self, Converter};
use crate::h264;
use crate::mfenc::{EncEvent, Encoder};
use anyhow::{bail, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use windows::core::w;
use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_NOSOCKET, MF_VERSION};
use windows::Win32::System::Com::{CoIncrementMTAUsage, CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;

/// Lo que recibe un dispositivo.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub monitor: usize,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

pub enum Event {
    Started { encoder: String, src_w: u32, src_h: u32 },
    Config { codec: String },
    Frame { data: Vec<u8>, key: bool, host_ms: f64, encode_ms: f64 },
    Cursor { x: f64, y: f64, visible: bool },
    CursorShape(CursorShape),
    Error(String),
}

/// Control de flujo de un dispositivo: imágenes enviadas y confirmadas.
///
/// Si el dispositivo se retrasa (la Wi-Fi se para un momento), se deja de codificar para él
/// hasta que se pone al día. Descartar imágenes ya codificadas sería peor: rompe la cadena de
/// referencias de H.264 y obliga a mandar un keyframe, mucho más grande, justo cuando la red va
/// peor. Al reanudar, la siguiente imagen ya trae la pantalla actual.
pub struct Flow {
    /// Número de la última imagen enviada (lo actualiza el servidor).
    pub sent: AtomicU32,
    /// Número de la última imagen que el dispositivo ha confirmado.
    pub acked: Arc<AtomicU32>,
    /// Imágenes sin confirmar a partir de las cuales se espera (cambia con la imagen suave).
    limit: AtomicU32,
    /// Milisegundos esperando (con algo que enviar) desde la última vez que se leyó.
    pub paused_ms: AtomicU32,
}

impl Flow {
    pub fn new(acked: Arc<AtomicU32>, limit: u32) -> Self {
        let sent = AtomicU32::new(acked.load(Ordering::Relaxed));
        Self { sent, acked, limit: AtomicU32::new(limit), paused_ms: AtomicU32::new(0) }
    }

    pub fn limit(&self) -> u32 {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn set_limit(&self, limit: u32) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    fn behind(&self) -> bool {
        self.sent.load(Ordering::Relaxed).saturating_sub(self.acked.load(Ordering::Relaxed)) > self.limit()
    }
}

enum Command {
    Add { id: u64, settings: Settings, events: UnboundedSender<Event>, flow: Arc<Flow> },
    Remove(u64),
    KeyFrame(u64),
    Bitrate(u64, u32),
}

/// Para pedir un keyframe o cambiar la calidad del codificador de un dispositivo.
#[derive(Clone)]
pub struct Controller {
    cmd: mpsc::Sender<Command>,
    id: u64,
}

impl Controller {
    pub fn keyframe(&self) {
        let _ = self.cmd.send(Command::KeyFrame(self.id));
    }

    pub fn bitrate(&self, bps: u32) {
        let _ = self.cmd.send(Command::Bitrate(self.id, bps));
    }
}

/// Captura de un monitor, con los codificadores de los dispositivos que lo muestran.
pub struct Capture {
    cmd: mpsc::Sender<Command>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    sinks: Vec<u64>,
}

impl Capture {
    pub fn start(monitor: usize) -> Self {
        let (cmd, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name(format!("captura {monitor}"))
            .spawn(move || run(monitor, &rx, &flag))
            .expect("no se pudo crear el hilo de captura");
        Self { cmd, stop, thread: Some(thread), sinks: Vec::new() }
    }

    /// Añade el codificador de un dispositivo. Los eventos le llegan por `events`.
    pub fn add(&mut self, id: u64, settings: Settings, events: UnboundedSender<Event>, flow: Arc<Flow>) -> Controller {
        let _ = self.cmd.send(Command::Add { id, settings, events, flow });
        self.sinks.push(id);
        Controller { cmd: self.cmd.clone(), id }
    }

    pub fn remove(&mut self, id: u64) {
        let _ = self.cmd.send(Command::Remove(id));
        self.sinks.retain(|s| *s != id);
    }

    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// COM y Media Foundation del hilo de captura.
///
/// Además, la primera vez los deja iniciados para todo el programa. Si no, al cerrar la última
/// captura, COM descarga las DLL del codificador de Intel mientras sus hilos internos aún están
/// terminando. Eso cerraba el programa de golpe (0xC0000005 en `mfx_mft_h264ve_64.dll`) en
/// cuanto el único dispositivo conectado se iba o cambiaba de ajustes.
struct MfGuard;

impl MfGuard {
    fn new() -> Result<Self> {
        static KEEP: std::sync::Once = std::sync::Once::new();
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;
            KEEP.call_once(|| {
                let _ = CoIncrementMTAUsage();
                let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
            });
        }
        Ok(Self)
    }
}

impl Drop for MfGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

fn reopen(gpu: &Gpu, stop: &AtomicBool) -> Result<Duplicator> {
    let start = Instant::now();
    let mut warned = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            bail!("detenido");
        }
        match Duplicator::new(gpu) {
            Ok(d) => {
                if warned {
                    log::info!("Captura recuperada tras {:.1} s", start.elapsed().as_secs_f64());
                }
                return Ok(d);
            }
            // Escritorio seguro (UAC, bloqueo): se reintenta hasta que vuelva el normal. Si tarda,
            // se deja dicho por qué (sin imagen, el dispositivo se queda esperando).
            Err(e) => {
                if !warned && start.elapsed() > Duration::from_secs(3) {
                    log::warn!("No se puede capturar el monitor ({e:#}); se sigue intentando");
                    warned = true;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Vuelve a abrir la duplicación tras perder el acceso, con la resolución de antes. Al conectar
/// o quitar el monitor de otro dispositivo, Windows puede ponerle otro modo un momento, hasta
/// que el servidor le devuelve el suyo: se espera unos segundos antes de darlo por perdido.
fn reopen_same_size(gpu: &Gpu, stop: &AtomicBool, size: (u32, u32)) -> Result<Duplicator> {
    let start = Instant::now();
    loop {
        let dup = reopen(gpu, stop)?;
        if (dup.width, dup.height) == size {
            return Ok(dup);
        }
        if start.elapsed() > Duration::from_secs(4) {
            bail!("ha cambiado la resolución del monitor; vuelve a conectar");
        }
        log::debug!("captura: el monitor está a {}x{}; se espera a que vuelva a {}x{}", dup.width, dup.height, size.0, size.1);
        drop(dup);
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Copia compartida de la última imagen capturada.
struct Source {
    tex: ID3D11Texture2D,
    width: u32,
    height: u32,
    /// Cuándo se capturó (para medir captura → codificado).
    captured: f64,
}

/// Codificador de un dispositivo.
struct Sink {
    id: u64,
    s: Settings,
    events: UnboundedSender<Event>,
    flow: Arc<Flow>,
    /// Desde cuándo espera a que el dispositivo se ponga al día.
    paused_since: Option<Instant>,
    enc: Encoder,
    conv: Option<Converter>,
    interval: Duration,
    duration_100ns: i64,
    t0: Instant,
    need: u32,
    last_feed: Instant,
    /// Hay una imagen capturada que este codificador aún no ha recibido.
    stale: bool,
    key_wanted: bool,
    pending: VecDeque<(i64, f64)>, // (tiempo de muestra, captura en ms)
    config_sent: bool,
    delivered: u32,
}

impl Sink {
    fn new(gpu: &Gpu, id: u64, s: Settings, events: UnboundedSender<Event>, flow: Arc<Flow>) -> Result<Self> {
        let enc = Encoder::new(&gpu.device, s.width, s.height, s.fps, s.bitrate)?;
        let interval = Duration::from_secs_f64(1.0 / s.fps.max(1) as f64);
        Ok(Self {
            id,
            duration_100ns: (10_000_000 / s.fps.max(1)) as i64,
            s,
            events,
            flow,
            paused_since: None,
            enc,
            conv: None,
            interval,
            t0: Instant::now(),
            need: 0,
            last_feed: Instant::now() - interval,
            stale: true,
            key_wanted: true,
            pending: VecDeque::new(),
            config_sent: false,
            delivered: 0,
        })
    }

    /// Hay un frame dentro del codificador (se ignoran los que llevan mucho: los habrá descartado).
    fn busy(&self) -> bool {
        self.pending.back().is_some_and(|&(_, c)| now_ms() - c < 200.0)
    }

    /// Quiere una imagen ahora: el codificador la ha pedido, toca por ritmo de fotogramas y el
    /// dispositivo no va atrasado.
    fn ready(&self) -> bool {
        self.need > 0 && self.last_feed.elapsed() >= self.interval && !self.flow.behind()
    }

    /// Cuenta el tiempo que pasa esperando al dispositivo cuando, por ritmo, ya le tocaba otra
    /// imagen (mientras espera no se captura, así que no se sabe si la pantalla ha cambiado).
    fn track_pause(&mut self) {
        let waiting = self.need > 0 && self.last_feed.elapsed() >= self.interval && self.flow.behind();
        match (waiting, self.paused_since) {
            (true, None) => self.paused_since = Some(Instant::now()),
            (false, Some(t)) => {
                self.flow.paused_ms.fetch_add(t.elapsed().as_millis() as u32, Ordering::Relaxed);
                self.paused_since = None;
            }
            _ => {}
        }
    }

    fn poll(&mut self) -> Result<()> {
        while let Some(ev) = self.enc.try_event()? {
            match ev {
                EncEvent::NeedInput => self.need += 1,
                EncEvent::HaveOutput => self.deliver()?,
                EncEvent::Other => {}
            }
        }
        Ok(())
    }

    /// Convierte la imagen compartida a su tamaño y se la da al codificador.
    fn feed(&mut self, gpu: &Gpu, src: &Source, rotation: i32) -> Result<()> {
        if self.conv.is_none() {
            self.conv = Some(Converter::new(
                gpu, &src.tex, src.width, src.height, self.s.width, self.s.height, self.s.fps, 4, rotation,
            )?);
        }
        let nv12 = self.conv.as_mut().expect("conversor creado").convert()?;
        // D3D11 acumula los comandos y los envía a la GPU cuando le parece; sin esto, el
        // codificador espera a la conversión hasta el siguiente envío.
        unsafe { gpu.ctx.Flush() };
        self.input(&nv12, src.captured)?;
        self.stale = false;
        Ok(())
    }

    fn input(&mut self, tex: &ID3D11Texture2D, captured: f64) -> Result<()> {
        if self.key_wanted {
            self.enc.force_keyframe();
            self.key_wanted = false;
        }
        let time = (self.t0.elapsed().as_nanos() / 100) as i64;
        self.enc.input(tex, time, self.duration_100ns)?;
        self.pending.push_back((time, captured));
        self.last_feed = Instant::now();
        self.need -= 1;
        Ok(())
    }

    fn deliver(&mut self) -> Result<()> {
        let Some(out) = self.enc.output()? else { return Ok(()) };
        let host_ms = now_ms();
        let mut captured = host_ms;
        while let Some(&(t, c)) = self.pending.front() {
            if t > out.time {
                break;
            }
            self.pending.pop_front();
            captured = c;
        }
        self.delivered += 1;
        if self.delivered <= 30 {
            log::debug!(
                "dispositivo {} salida #{}: {:.1} ms desde la captura, {} bytes, key={}",
                self.id, self.delivered, host_ms - captured, out.data.len(), out.key
            );
        }
        if !self.config_sent {
            let Some(codec) = h264::codec_string(&out.data) else {
                return Ok(()); // hasta el primer keyframe con SPS no se puede decodificar
            };
            let _ = self.events.send(Event::Config { codec });
            self.config_sent = true;
        }
        let _ = self.events.send(Event::Frame { data: out.data, key: out.key, host_ms, encode_ms: host_ms - captured });
        Ok(())
    }
}

fn run(monitor: usize, cmd: &mpsc::Receiver<Command>, stop: &AtomicBool) {
    // Los codificadores se sueltan antes de cerrar Media Foundation y COM en este hilo: soltar
    // un objeto de Media Foundation después de MFShutdown/CoUninitialize no es válido.
    let mf = MfGuard::new();
    let mut sinks: Vec<Sink> = Vec::new();
    let result = match &mf {
        Ok(_) => capture_loop(monitor, cmd, stop, &mut sinks),
        Err(e) => Err(anyhow::anyhow!("Media Foundation: {e:#}")),
    };
    if let Err(e) = result {
        log::debug!("captura del monitor {monitor}: {e:#}");
        for s in &sinks {
            let _ = s.events.send(Event::Error(format!("{e:#}")));
        }
        // Los que se añadan después de un error también se enteran.
        while !stop.load(Ordering::Relaxed) {
            match cmd.recv_timeout(Duration::from_millis(100)) {
                Ok(Command::Add { events, .. }) => {
                    let _ = events.send(Event::Error(format!("{e:#}")));
                }
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    drop(sinks);
    drop(mf);
}

fn capture_loop(monitor: usize, cmd: &mpsc::Receiver<Command>, stop: &AtomicBool, sinks: &mut Vec<Sink>) -> Result<()> {
    // Prioridad multimedia (MMCSS) para el hilo de captura: Windows lo programa antes que el
    // trabajo normal y no lo manda a los núcleos de bajo consumo.
    let mut task = 0u32;
    let _mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Capture"), &mut task) };
    let gpu = Gpu::open(monitor)?;
    // Si una captura anterior de este monitor aún se está cerrando, Windows rechaza la nueva
    // durante un momento: se reintenta un par de segundos.
    let mut tries = 0;
    let mut dup = loop {
        match Duplicator::new(&gpu) {
            Ok(d) => break d,
            Err(e) if tries < 20 && !stop.load(Ordering::Relaxed) => {
                tries += 1;
                log::debug!("captura: {e:#}; reintentando");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    };
    let (src_w, src_h) = (dup.width, dup.height);
    let (desk_w, desk_h) = gpu.desktop;
    log::debug!("captura {monitor}: modo {src_w}x{src_h}, giro DXGI {}, escritorio {desk_w}x{desk_h}", dup.rotation);
    let mut source: Option<Source> = None;
    let mut cursor: Option<(f64, f64, bool)> = None;
    let mut shape: Option<CursorShape> = None;
    // Una duplicación recién creada entrega la imagen actual en seguida. Si se crea mientras
    // Windows aún está ajustando los monitores (uno recién conectado, o el de otro dispositivo
    // que se añade o se quita), puede quedarse muda sin dar error: entonces se vuelve a crear.
    let mut dup_started = Instant::now();
    let mut fresh = true; // aún sin imagen desde que se creó la duplicación

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Órdenes del servidor: dispositivos que llegan o se van, keyframes, calidad.
        while let Ok(c) = cmd.try_recv() {
            match c {
                Command::Add { id, settings, events, flow } => match Sink::new(&gpu, id, settings, events.clone(), flow) {
                    Ok(s) => {
                        let _ = events.send(Event::Started { encoder: s.enc.name.clone(), src_w: desk_w, src_h: desk_h });
                        if let Some(sh) = &shape {
                            let _ = events.send(Event::CursorShape(sh.clone()));
                        }
                        if let Some((x, y, v)) = cursor {
                            let _ = events.send(Event::Cursor { x, y, visible: v });
                        }
                        sinks.push(s);
                    }
                    Err(e) => {
                        let _ = events.send(Event::Error(format!("{e:#}")));
                    }
                },
                Command::Remove(id) => sinks.retain(|s| s.id != id),
                Command::KeyFrame(id) => {
                    if let Some(s) = sinks.iter_mut().find(|s| s.id == id) {
                        s.key_wanted = true;
                        s.stale = true; // con la pantalla quieta, se vuelve a mandar la imagen actual
                    }
                }
                Command::Bitrate(id, b) => {
                    if let Some(s) = sinks.iter().find(|s| s.id == id) {
                        s.enc.set_bitrate(b);
                    }
                }
            }
        }
        // Eventos de los codificadores. Si uno falla, solo se quita ese.
        sinks.retain_mut(|s| match s.poll() {
            Ok(()) => true,
            Err(e) => {
                let _ = s.events.send(Event::Error(format!("{e:#}")));
                false
            }
        });
        if sinks.is_empty() {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        for s in sinks.iter_mut() {
            s.track_pause();
        }
        // Los que aún no tienen la última imagen la reciben de la copia compartida.
        if let Some(src) = &source {
            for s in sinks.iter_mut().filter(|s| s.stale && s.ready()) {
                if let Err(e) = s.feed(&gpu, src, dup.rotation) {
                    let _ = s.events.send(Event::Error(format!("{e:#}")));
                }
            }
        }
        let busy = sinks.iter().any(|s| s.busy());
        if !sinks.iter().any(|s| s.ready()) {
            // Nadie quiere imagen todavía (ritmo de fotogramas o codificador ocupado).
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        let frame = match dup.acquire(if busy { 1 } else { 4 })? {
            Acquire::Lost => {
                log::debug!("captura {monitor}: acceso perdido, se vuelve a crear la duplicación");
                drop(dup);
                dup = reopen_same_size(&gpu, stop, (src_w, src_h))?;
                dup_started = Instant::now();
                fresh = true;
                for s in sinks.iter_mut() {
                    s.key_wanted = true; // por si se perdió algo mientras tanto
                }
                None
            }
            Acquire::Timeout if fresh && dup_started.elapsed() > Duration::from_millis(1500) => {
                log::debug!("captura {monitor}: la duplicación no entrega imagen; se vuelve a crear");
                drop(dup);
                dup = reopen(&gpu, stop)?;
                dup_started = Instant::now();
                None
            }
            Acquire::Timeout => None,
            Acquire::Frame(u) => {
                // La posición del cursor viene en coordenadas del escritorio (ya girado).
                if let Some(p) = u.pointer {
                    let c = (p.0 as f64 / desk_w as f64, p.1 as f64 / desk_h as f64, p.2);
                    if cursor != Some(c) {
                        cursor = Some(c);
                        for s in sinks.iter() {
                            let _ = s.events.send(Event::Cursor { x: c.0, y: c.1, visible: c.2 });
                        }
                    }
                }
                if let Some(sh) = u.shape {
                    for s in sinks.iter() {
                        let _ = s.events.send(Event::CursorShape(sh.clone()));
                    }
                    shape = Some(sh);
                }
                u.image
            }
        };
        let Some(image) = frame else { continue };
        fresh = false;
        let captured = now_ms();
        if source.is_none() {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            unsafe { image.GetDesc(&mut d) };
            log::debug!("captura {monitor}: imagen de {}x{}", d.Width, d.Height);
            source = Some(Source { tex: convert::create_source(&gpu.device, d.Width, d.Height)?, width: d.Width, height: d.Height, captured });
        }
        let src = source.as_mut().expect("copia creada");
        unsafe { gpu.ctx.CopyResource(&src.tex, &image) };
        dup.release();
        src.captured = captured;
        for s in sinks.iter_mut() {
            s.stale = true;
        }
        let src = source.as_ref().expect("copia creada");
        for s in sinks.iter_mut().filter(|s| s.ready()) {
            if let Err(e) = s.feed(&gpu, src, dup.rotation) {
                let _ = s.events.send(Event::Error(format!("{e:#}")));
            }
        }
    }
}
