//! Pantalla secundaria: host en Rust.
//!
//! Desktop Duplication → procesador de vídeo D3D11 (BGRA→NV12) → QuickSync (Media Foundation)
//! → WebSocket sobre HTTPS → navegador (WebCodecs). La imagen no sale de la GPU hasta que el
//! codificador entrega el H.264. Los ajustes se eligen en un panel en el propio ordenador.
mod capture;
mod certs;
mod clock;
mod convert;
mod display;
mod h264;
mod hotspot;
mod input;
mod mfenc;
mod pipeline;
mod prefs;
mod server;
mod sys;
mod vdd;

use anyhow::{bail, Result};
use clap::Parser;
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
    /// Puerto HTTP (panel y configuración). También acepta HTTPS.
    #[arg(long, default_value_t = 8080)]
    pub http_port: u16,
    #[arg(long, default_value = "0.0.0.0")]
    pub bind: String,
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
    /// Usa la red Wi-Fi propia del ordenador (se guarda como ajuste).
    #[arg(long)]
    pub hotspot: bool,
    /// No abre el panel de control al arrancar.
    #[arg(long)]
    pub no_panel: bool,
    /// Arranca ya encendida, sin esperar al botón del panel.
    #[arg(long)]
    pub start: bool,
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

fn print_banner(app: &server::App, monitor: &capture::OutputInfo, vdd: &Result<u32, String>) {
    let line = "=".repeat(66);
    println!("{line}\n Pantalla secundaria - host en Rust\n{line}");
    println!(" Monitor principal : {} {}x{} ({})", monitor.name, monitor.rect.w, monitor.rect.h, monitor.adapter);
    println!(" Codificación      : QuickSync por Media Foundation, sin copias (GPU)");
    match vdd {
        Ok(v) => println!(" Pantalla adicional: disponible (Parsec VDD, versión 0.{v})"),
        Err(e) => println!(" Pantalla adicional: no disponible ({e}); solo modo espejo"),
    }
    println!(" Panel de control  : {}", app.panel_url());
    println!();
    println!(" Pulsa el botón de encendido del panel para empezar; el código QR aparecerá");
    println!(" también aquí.");
    println!("{line}");
}

/// Avisa cuando aparece una dirección nueva (p. ej. la de la red propia).
async fn announce_new_addresses(port: u16) {
    let mut known: std::collections::HashSet<_> = certs::usable_ips().into_iter().collect();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        for ip in certs::usable_ips() {
            if known.insert(ip) {
                log::info!("Nueva dirección: https://{ip}:{port}/");
            }
        }
    }
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
    let key = prefs::pairing_key(&data, false)?;
    let mut settings = prefs::Prefs::load(&prefs::path(&data));
    if args.hotspot && settings.network != "own" {
        settings.network = "own".into();
        settings.save(&prefs::path(&data));
    }
    let outputs = capture::outputs()?;
    let monitor = outputs
        .iter()
        .find(|o| o.rect.x == 0 && o.rect.y == 0)
        .or(outputs.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no hay monitores conectados"))?;

    let https = tokio::net::TcpListener::bind((args.bind.as_str(), args.port)).await?;
    let http = tokio::net::TcpListener::bind((args.bind.as_str(), args.http_port)).await?;
    let vdd = vdd::Vdd::open();
    let vdd_status = vdd.as_ref().map(|v| v.version).map_err(|e| format!("{e:#}"));
    let app = server::App::new(args.clone(), key, root, data, certs, vdd.ok(), settings)?;
    print_banner(&app, &monitor, &vdd_status);
    if args.start {
        app.power(true).await?;
    }

    tokio::spawn(app.clone().watch_ips());
    tokio::spawn(app.clone().touch_keepalive());
    tokio::spawn(app.clone().hotspot_watchdog());
    tokio::spawn(announce_new_addresses(args.port));
    if !args.no_panel {
        sys::open_app_window(&app.panel_url());
    }
    tokio::join!(server::serve(app.clone(), https), server::serve(app, http));
    Ok(())
}
