//! Pantalla secundaria: host en Rust.
//!
//! Desktop Duplication → procesador de vídeo D3D11 (BGRA→NV12) → QuickSync (Media Foundation)
//! → WebSocket sobre HTTPS → navegador (WebCodecs). La imagen no sale de la GPU hasta que el
//! codificador entrega el H.264.
mod capture;
mod certs;
mod clock;
mod convert;
mod display;
mod h264;
mod input;
mod mfenc;
mod pipeline;
mod server;
mod vdd;

use anyhow::{bail, Result};
use clap::Parser;
use rand::Rng;
use std::io::Write;
use std::path::{Path, PathBuf};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Threading::{
    GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

/// Ajustes de proceso para baja latencia.
///
/// El codificador de Intel espera internamente con el temporizador de Windows, que por
/// defecto avanza cada 15,6 ms: sin esto, cada frame sale en el siguiente "tic" (hasta 15 ms
/// de retraso). También se evita que Windows lo ralentice o lo mande a los núcleos de bajo
/// consumo por ahorro de energía.
fn tune_process() {
    unsafe {
        let _ = timeBeginPeriod(1);
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

#[derive(Parser, Clone, Debug)]
#[command(about = "Host de pantalla secundaria (captura y codificación en la GPU)")]
pub struct Args {
    /// Puerto HTTPS (visor).
    #[arg(long, default_value_t = 8443)]
    pub port: u16,
    /// Puerto HTTP (configuración y localhost). También acepta HTTPS.
    #[arg(long, default_value_t = 8080)]
    pub http_port: u16,
    #[arg(long, default_value = "0.0.0.0")]
    pub bind: String,
    /// Monitor a capturar (0 = el primero que da DXGI, normalmente el principal).
    #[arg(long, default_value_t = 0)]
    pub monitor: usize,
    /// Retraso acumulado a partir del cual se descartan frames y se pide un keyframe.
    #[arg(long, default_value_t = 200.0)]
    pub max_inflight_ms: f64,
    /// Carpeta del proyecto (la que contiene web/ y certs/). Por defecto se busca sola.
    #[arg(long)]
    pub root: Option<PathBuf>,
    #[arg(short, long)]
    pub verbose: bool,
    /// Muestra los monitores que ve DXGI (y su adaptador) y sale.
    #[arg(long)]
    pub list: bool,
}

fn init_logger(verbose: bool) {
    env_logger::Builder::new()
        .filter_level(if verbose { log::LevelFilter::Debug } else { log::LevelFilter::Info })
        .filter_module("rustls", log::LevelFilter::Warn)
        .filter_module("tungstenite", log::LevelFilter::Warn)
        .filter_module("tokio_tungstenite", log::LevelFilter::Warn)
        .format(|buf, rec| {
            let t = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
            writeln!(buf, "{:02}:{:02}:{:02} {}", t.hour(), t.minute(), t.second(), rec.args())
        })
        .init();
}

/// Carpeta del proyecto: la primera, subiendo desde el directorio actual o desde el
/// ejecutable, que contiene web/index.html.
fn find_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let mut starts: Vec<PathBuf> = explicit.into_iter().collect();
    starts.extend(std::env::current_dir().ok());
    starts.extend(std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)));
    for start in starts {
        for dir in start.ancestors() {
            if dir.join("web").join("index.html").is_file() {
                return Ok(dir.to_path_buf());
            }
        }
    }
    bail!("no encuentro la carpeta web/ del proyecto; indícala con --root")
}

fn pairing_key(dir: &Path) -> Result<String> {
    let path = dir.join("pairing.txt");
    if let Ok(k) = std::fs::read_to_string(&path) {
        let k = k.trim().to_string();
        if k.len() == 8 {
            return Ok(k);
        }
    }
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let key: String = (0..8).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect();
    std::fs::write(&path, &key)?;
    Ok(key)
}

fn print_banner(args: &Args, key: &str, monitor: &capture::OutputInfo, vdd: &Result<vdd::Vdd>) {
    let line = "=".repeat(66);
    println!("{line}\n Pantalla secundaria - host en Rust\n{line}");
    println!(" Monitor principal : {} {}x{} ({})", monitor.name, monitor.rect.w, monitor.rect.h, monitor.adapter);
    println!(" Codificación      : QuickSync por Media Foundation, sin copias (GPU)");
    match vdd {
        Ok(v) => println!(" Modo extender     : disponible (Parsec VDD, versión 0.{})", v.version),
        Err(e) => println!(" Modo extender     : no disponible ({e:#}); se usará el modo espejo"),
    }
    println!(" Código            : {}-{}", &key[..4], &key[4..]);
    println!();
    println!(" En el móvil o la tablet (misma Wi-Fi o hotspot del portátil):");
    for ip in certs::usable_ips() {
        let tag = if certs::HOTSPOT_IPS.contains(&ip) { "  (hotspot)" } else { "" };
        println!("   Pantalla : https://{ip}:{}/#k={key}{tag}", args.port);
        println!("   1a vez   : http://{ip}:{}/setup", args.http_port);
    }
    println!();
    println!(" Medir latencia: abre esto en el portátil y déjalo visible:");
    println!("   http://localhost:{}/probe", args.http_port);
    println!("{line}");
}

#[tokio::main]
async fn main() -> Result<()> {
    clock::init();
    let args = Args::parse();
    init_logger(args.verbose);
    unsafe {
        // Píxeles físicos en toda la API de Windows (captura, coordenadas del táctil…).
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    tune_process();
    if args.list {
        for o in capture::outputs()? {
            println!("{}: {} {}x{} en ({}, {}) - {}", o.index, o.name, o.rect.w, o.rect.h, o.rect.x, o.rect.y, o.adapter);
        }
        return Ok(());
    }
    let root = find_root(args.root.clone())?;
    let data = root.join("certs");
    let certs = certs::CertManager::new(&data)?;
    certs.ensure()?;
    let key = pairing_key(&data)?;
    let outputs = capture::outputs()?;
    let monitor = outputs
        .get(args.monitor)
        .or(outputs.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no hay monitores conectados"))?;

    let https = tokio::net::TcpListener::bind((args.bind.as_str(), args.port)).await?;
    let http = tokio::net::TcpListener::bind((args.bind.as_str(), args.http_port)).await?;
    let vdd = vdd::Vdd::open();
    print_banner(&args, &key, &monitor, &vdd);
    let app = server::App::new(args.clone(), key.clone(), root.join("web"), certs, vdd.ok())?;

    tokio::spawn(app.clone().watch_ips());
    tokio::spawn(app.clone().touch_keepalive());
    tokio::join!(server::serve(app.clone(), https), server::serve(app, http));
    Ok(())
}
