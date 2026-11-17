//! Hilo de captura y codificación: Desktop Duplication → procesador de vídeo → QuickSync.
//!
//! El codificador asíncrono pide entrada (NeedInput) y entrega salida (HaveOutput). Cuando pide
//! entrada se espera al siguiente cambio de pantalla, que se convierte y se entrega en el
//! acto: no hay colas intermedias, así que cada frame lleva la imagen más reciente.
use crate::capture::{Acquire, CursorShape, Duplicator, Gpu};
use crate::clock::now_ms;
use crate::convert::Converter;
use crate::h264;
use crate::mfenc::{EncEvent, Encoder};
use anyhow::{bail, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_NOSOCKET, MF_VERSION};
use windows::core::w;
use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;

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

pub enum Control {
    KeyFrame,
    Bitrate(u32),
}

#[derive(Clone)]
pub struct Controller(mpsc::Sender<Control>);

impl Controller {
    pub fn keyframe(&self) {
        let _ = self.0.send(Control::KeyFrame);
    }

    pub fn bitrate(&self, bps: u32) {
        let _ = self.0.send(Control::Bitrate(bps));
    }
}

pub struct Pipeline {
    ctrl: mpsc::Sender<Control>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Pipeline {
    pub fn start(settings: Settings, events: UnboundedSender<Event>) -> Self {
        let (ctrl, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("captura".into())
            .spawn(move || {
                if let Err(e) = run(&settings, &events, &rx, &flag) {
                    let _ = events.send(Event::Error(format!("{e:#}")));
                }
            })
            .expect("no se pudo crear el hilo de captura");
        Self { ctrl, stop, thread: Some(thread) }
    }

    pub fn controller(&self) -> Controller {
        Controller(self.ctrl.clone())
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct MfGuard;

impl MfGuard {
    fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;
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
    loop {
        if stop.load(Ordering::Relaxed) {
            bail!("detenido");
        }
        match Duplicator::new(gpu) {
            Ok(d) => return Ok(d),
            // Escritorio seguro (UAC, bloqueo): se reintenta hasta que vuelva el normal.
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn run(s: &Settings, events: &UnboundedSender<Event>, ctrl: &mpsc::Receiver<Control>, stop: &AtomicBool) -> Result<()> {
    // Prioridad multimedia (MMCSS) para el hilo de captura: Windows lo programa antes que el
    // trabajo normal y no lo manda a los núcleos de bajo consumo.
    let mut task = 0u32;
    let _mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Capture"), &mut task) };
    let _mf = MfGuard::new()?;
    let gpu = Gpu::open(s.monitor)?;
    let mut dup = Duplicator::new(&gpu)?;
    let (src_w, src_h) = (dup.width, dup.height);
    // El conversor se crea con las medidas reales de la primera imagen (con el monitor en
    // vertical, la imagen capturada llega sin girar).
    let mut conv: Option<Converter> = None;
    let enc = Encoder::new(&gpu.device, s.width, s.height, s.fps, s.bitrate)?;
    let (desk_w, desk_h) = gpu.desktop;
    log::debug!("captura: modo {src_w}x{src_h}, giro DXGI {}, escritorio {desk_w}x{desk_h}", dup.rotation);
    let _ = events.send(Event::Started { encoder: enc.name.clone(), src_w: desk_w, src_h: desk_h });

    let interval = Duration::from_secs_f64(1.0 / s.fps as f64);
    let duration_100ns = (10_000_000 / s.fps) as i64;
    let t0 = Instant::now();
    let mut last_feed = t0 - interval;
    let mut key_wanted = true;
    let mut need = 0u32; // entradas que ha pedido el codificador
    let mut out = Outputs { events, pending: VecDeque::new(), config_sent: false, delivered: 0 };
    let mut last_pointer: Option<(i32, i32, bool)> = None;
    let bench = std::env::var("PANTALLA_BENCH").is_ok();
    // Diagnóstico (-v): qué devuelve la captura en los primeros segundos.
    let (mut n_timeout, mut n_image, mut n_pointer, mut last_diag) = (0u32, 0u32, 0u32, Instant::now());
    // Una duplicación recién creada entrega la imagen actual en seguida. Si se crea mientras
    // Windows aún está ajustando un monitor recién conectado, puede quedarse muda sin dar
    // error: entonces se vuelve a crear.
    let mut dup_started = Instant::now();
    let mut have_image = false;

    // El codificador puede pedir la siguiente entrada antes de entregar la salida anterior.
    // Por eso nunca se espera a la pantalla mucho rato mientras haya un frame en proceso:
    // la salida se recoge en cuanto está, sin esperar al siguiente cambio de pantalla.
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        while let Ok(c) = ctrl.try_recv() {
            match c {
                Control::KeyFrame => key_wanted = true,
                Control::Bitrate(b) => enc.set_bitrate(b),
            }
        }
        while let Some(ev) = enc.try_event()? {
            match ev {
                EncEvent::NeedInput => need += 1,
                EncEvent::HaveOutput => out.deliver(&enc)?,
                EncEvent::Other => {}
            }
        }
        if need == 0 {
            // El codificador está ocupado: esperar a su siguiente evento.
            match enc.next_event()? {
                EncEvent::NeedInput => need += 1,
                EncEvent::HaveOutput => out.deliver(&enc)?,
                EncEvent::Other => {}
            }
            continue;
        }

        let busy = out.busy();
        let since = last_feed.elapsed();
        if since < interval {
            // Ritmo máximo de fotogramas; con un frame en proceso, en pasos de 1 ms.
            let wait = interval - since;
            std::thread::sleep(if busy { wait.min(Duration::from_millis(1)) } else { wait });
            continue;
        }
        // Diagnóstico: PANTALLA_BENCH=1 recodifica la última imagen sin capturar, para medir
        // el codificador aislado de la captura.
        if bench && out.delivered >= 5 && out.delivered < 65 {
            if let Some(t) = conv.as_ref().and_then(|c| c.last.clone()) {
                if !busy {
                    let time = (t0.elapsed().as_nanos() / 100) as i64;
                    enc.input(&t, time, duration_100ns)?;
                    out.pending.push_back((time, now_ms()));
                    last_feed = Instant::now();
                    need -= 1;
                }
                continue;
            }
        }
        if out.delivered < 30 && last_diag.elapsed() > Duration::from_secs(2) {
            log::debug!("captura: {n_timeout} esperas vacías, {n_image} imágenes, {n_pointer} solo cursor; el codificador pide {need}");
            last_diag = Instant::now();
        }
        let acquired = dup.acquire(if busy { 1 } else { 10 })?;
        match &acquired {
            Acquire::Timeout => n_timeout += 1,
            Acquire::Frame(u) if u.image.is_some() => n_image += 1,
            Acquire::Frame(_) => n_pointer += 1,
            Acquire::Lost => log::debug!("captura: acceso perdido, se vuelve a crear la duplicación"),
        }
        let frame = match acquired {
            Acquire::Lost => {
                dup = reopen(&gpu, stop)?;
                if (dup.width, dup.height) != (src_w, src_h) {
                    bail!("ha cambiado la resolución del monitor; vuelve a conectar");
                }
                None
            }
            // Pantalla quieta pero se ha pedido un keyframe (p. ej. tras descartar frames por
            // congestión): se vuelve a codificar la última imagen.
            Acquire::Timeout if !have_image && dup_started.elapsed() > Duration::from_millis(1500) => {
                log::debug!("captura: la duplicación no entrega imagen; se vuelve a crear");
                drop(dup);
                dup = reopen(&gpu, stop)?;
                dup_started = Instant::now();
                None
            }
            Acquire::Timeout if key_wanted && last_feed.elapsed() > Duration::from_millis(30) => {
                conv.as_ref().and_then(|c| c.last.clone()).map(|t| (t, now_ms()))
            }
            Acquire::Timeout => None,
            Acquire::Frame(u) => {
                // La posición del cursor viene en coordenadas del escritorio (ya girado).
                if let Some(p) = u.pointer.filter(|p| last_pointer != Some(*p)) {
                    last_pointer = Some(p);
                    let _ = events.send(Event::Cursor {
                        x: p.0 as f64 / desk_w as f64,
                        y: p.1 as f64 / desk_h as f64,
                        visible: p.2,
                    });
                }
                if let Some(shape) = u.shape {
                    let _ = events.send(Event::CursorShape(shape));
                }
                match u.image {
                    Some(image) => {
                        have_image = true;
                        let captured = now_ms();
                        if conv.is_none() {
                            let mut d = D3D11_TEXTURE2D_DESC::default();
                            unsafe { image.GetDesc(&mut d) };
                            log::debug!("captura: imagen de {}x{}", d.Width, d.Height);
                            conv = Some(Converter::new(&gpu, d.Width, d.Height, s.width, s.height, s.fps, 4, dup.rotation)?);
                        }
                        let c = conv.as_mut().expect("conversor creado");
                        unsafe { gpu.ctx.CopyResource(&c.src, &image) };
                        dup.release();
                        let nv12 = c.convert()?;
                        // D3D11 acumula los comandos y los envía a la GPU cuando le parece;
                        // sin esto, el codificador espera a la conversión hasta el siguiente envío.
                        unsafe { gpu.ctx.Flush() };
                        Some((nv12, captured))
                    }
                    None => None,
                }
            }
        };
        let Some((tex, captured)) = frame else { continue };
        if key_wanted {
            enc.force_keyframe();
            key_wanted = false;
        }
        let time = (t0.elapsed().as_nanos() / 100) as i64;
        enc.input(&tex, time, duration_100ns)?;
        out.pending.push_back((time, captured));
        last_feed = Instant::now();
        need -= 1;
        if out.delivered < 30 {
            log::debug!("entrada: {:.1} ms tras la captura, el codificador pide {need} más", now_ms() - captured);
        }
    }
}

/// Frames entregados al codificador y todavía sin salida, y envío de lo que sale.
struct Outputs<'a> {
    events: &'a UnboundedSender<Event>,
    pending: VecDeque<(i64, f64)>, // (tiempo de muestra, captura en ms)
    config_sent: bool,
    delivered: u32,
}

impl Outputs<'_> {
    /// Hay un frame dentro del codificador (se ignoran los que llevan mucho: los habrá descartado).
    fn busy(&self) -> bool {
        self.pending.back().is_some_and(|&(_, c)| now_ms() - c < 200.0)
    }

    fn deliver(&mut self, enc: &Encoder) -> Result<()> {
        let Some(out) = enc.output()? else { return Ok(()) };
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
        if self.delivered <= 70 {
            log::debug!(
                "salida #{}: {:.1} ms desde la captura, {} bytes, key={}, quedan {} dentro",
                self.delivered, host_ms - captured, out.data.len(), out.key, self.pending.len()
            );
        }
        if !self.config_sent {
            let Some(codec) = h264::codec_string(&out.data) else {
                return Ok(()); // hasta el primer keyframe con SPS no se puede decodificar
            };
            let _ = self.events.send(Event::Config { codec });
            self.config_sent = true;
        }
        let _ = self.events.send(Event::Frame {
            data: out.data,
            key: out.key,
            host_ms,
            encode_ms: host_ms - captured,
        });
        Ok(())
    }
}
