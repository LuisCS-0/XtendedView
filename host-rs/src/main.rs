//! XtendedView: usa un móvil, una tablet o cualquier dispositivo con navegador como pantalla
//! de este ordenador.
//!
//! Desktop Duplication → procesador de vídeo D3D11 (BGRA→NV12) → QuickSync (Media Foundation)
//! → WebSocket sobre HTTPS → navegador (WebCodecs). La imagen no sale de la GPU hasta que el
//! codificador entrega el H.264. Los ajustes se eligen en un panel en el propio ordenador.
//!
//! Con la característica `gui` (la versión del instalador) no hay ventana de consola: el
//! programa vive en el área de notificación y escribe su registro en un archivo.
#![cfg_attr(feature = "gui", windows_subsystem = "windows")]
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
mod tray;
mod vdd;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::io::Write;
use std::path::{Path, PathBuf};
use windows::core::HSTRING;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Console::SetConsoleTitleW;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

/// Versión del instalador: sin consola, con icono junto al reloj y registro en un archivo.
const GUI: bool = cfg!(feature = "gui");

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
#[command(about = "XtendedView: un móvil, una tablet u otro dispositivo con navegador como pantalla de este ordenador")]
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
    /// Carpeta del programa (la que contiene web/). Por defecto se busca sola.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Carpeta de datos (certificados, código, ajustes). Por defecto, certs/ del proyecto si se
    /// ejecuta desde el código fuente y, si está instalado, %LOCALAPPDATA%\XtendedView.
    #[arg(long)]
    pub data: Option<PathBuf>,
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

/// Registro en la consola o, sin ella, en `file` (se empieza de cero en cada arranque).
fn init_logger(verbose: bool, file: Option<&Path>) {
    let mut b = env_logger::Builder::new();
    b.filter_level(if verbose { log::LevelFilter::Debug } else { log::LevelFilter::Info })
        .filter_module("rustls", log::LevelFilter::Warn)
        .filter_module("tungstenite", log::LevelFilter::Warn)
        .filter_module("tokio_tungstenite", log::LevelFilter::Warn)
        .format(|buf, rec| {
            let t = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
            writeln!(buf, "{:02}:{:02}:{:02} {}", t.hour(), t.minute(), t.second(), rec.args())
        });
    if let Some(f) = file.and_then(|p| std::fs::File::create(p).ok()) {
        b.target(env_logger::Target::Pipe(Box::new(f)));
    }
    b.init();
}

/// Dónde se guardan certificados, código y ajustes. Versión portátil (con portable.txt junto al
/// ejecutable): en datos\ a su lado, para llevarla en un USB. Desde el código fuente, en certs/
/// (como siempre). Instalado, la carpeta del programa no se puede escribir: en la del usuario.
fn data_dir(root: &Path, explicit: Option<PathBuf>) -> Result<PathBuf> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let portable = exe_dir.filter(|d| d.join("portable.txt").is_file());
    let dir = match explicit {
        Some(d) => d,
        None if portable.is_some() => portable.unwrap_or_default().join("datos"),
        None if root.join("host-rs").is_dir() || root.join("certs").is_dir() => root.join("certs"),
        None => PathBuf::from(std::env::var_os("LOCALAPPDATA").context("falta LOCALAPPDATA")?).join("XtendedView"),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("no se pudo crear {}", dir.display()))?;
    Ok(dir)
}

/// Solo una copia por puerto. Si ya hay una abierta, se le abre el panel y esta termina.
fn already_running(http_port: u16) -> bool {
    let name = HSTRING::from(format!("Local\\XtendedView-{http_port}"));
    unsafe {
        // El mutex se queda abierto mientras viva el proceso (no se cierra a propósito).
        let created = CreateMutexW(None, true, &name);
        created.is_ok() && GetLastError() == ERROR_ALREADY_EXISTS
    }
}

/// Sin consola, los errores al arrancar se avisan con una ventana.
fn report_fatal(e: &anyhow::Error) {
    log::error!("{e:#}");
    if GUI {
        let text = HSTRING::from(format!("XtendedView no ha podido arrancar:\n\n{e:#}"));
        unsafe {
            MessageBoxW(None, &text, &HSTRING::from("XtendedView"), MB_OK | MB_ICONERROR);
        }
    } else {
        eprintln!("Error: {e:#}");
    }
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
    println!("{line}\n XtendedView\n{line}");
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
async fn main() {
    if let Err(e) = run().await {
        report_fatal(&e);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    clock::init();
    let args = Args::parse();
    // Antes que nada (ni siquiera el registro, que es el mismo archivo): si ya está abierto, se
    // muestra su panel y esta copia termina.
    if !args.list && already_running(args.http_port) {
        sys::open_app_window(&format!("http://localhost:{}/panel", args.http_port));
        return Ok(());
    }
    let root = find_root(args.root.clone());
    let data = root.as_ref().map_err(|e| anyhow::anyhow!("{e:#}")).and_then(|r| data_dir(r, args.data.clone()));
    init_logger(args.verbose, if GUI { data.as_ref().ok().map(|d| d.join("xtendedview.log")) } else { None }.as_deref());
    unsafe {
        // Píxeles físicos en toda la API de Windows (captura, coordenadas del táctil…).
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = SetConsoleTitleW(windows::core::w!("XtendedView"));
    }
    tune_process();
    if args.list {
        println!("Monitores:");
        for o in capture::outputs()? {
            println!("  {}: {} {}x{} en ({}, {}) - {}", o.index, o.name, o.rect.w, o.rect.h, o.rect.x, o.rect.y, o.adapter);
        }
        println!("Tarjetas de red conectadas:");
        let ifs = sys::interfaces();
        for i in &ifs {
            let ips: Vec<String> = i.ipv4.iter().map(|(ip, p)| format!("{ip}/{p}")).collect();
            let mut tags = vec![if i.hardware { "física" } else { "virtual" }];
            if i.gateway {
                tags.push("con salida a internet");
            }
            if i.is_cable() {
                tags.push("CABLE");
            }
            println!("  {} ({}): {} · métrica {} · {}", i.name, i.description, ips.join(", "), i.metric, tags.join(", "));
            if i.is_cable() {
                println!("    → internet por el cable: {:?}", sys::cable_route(&ifs, i));
            }
        }
        return Ok(());
    }
    let root = root?;
    let data = data?;
    log::info!("Datos en {}", data.display());
    let certs = certs::CertManager::new(&data)?;
    certs.ensure()?;
    let key = prefs::pairing_key(&data, false)?;
    let mut settings = prefs::Prefs::load(&prefs::path(&data));
    if args.hotspot && !settings.has("own") {
        settings.set_connection("own", true);
    }
    settings.save(&prefs::path(&data)); // también pasa los ajustes de versiones anteriores al formato nuevo
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
    tokio::spawn(app.clone().cable_watch());
    tokio::spawn(announce_new_addresses(args.port));
    if !args.no_panel {
        sys::open_app_window(&app.panel_url());
    }
    sys::keep_panel_icon("XtendedView"); // el título de panel.html

    // Icono junto al reloj: abrir el panel o salir (apagando antes: quita los monitores
    // virtuales y cierra la red propia).
    let (tray_tx, mut tray_rx) = tokio::sync::mpsc::unbounded_channel();
    tray::spawn(tray_tx);
    let tray_app = app.clone();
    tokio::spawn(async move {
        while let Some(ev) = tray_rx.recv().await {
            match ev {
                tray::TrayEvent::OpenPanel => sys::open_app_window(&tray_app.panel_url()),
                tray::TrayEvent::Quit => {
                    log::info!("Saliendo");
                    tray_app.quit().await;
                }
            }
        }
    });
    tokio::join!(server::serve(app.clone(), https), server::serve(app, http));
    Ok(())
}
