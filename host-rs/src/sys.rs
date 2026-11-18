//! Utilidades del sistema: red, códigos QR, abrir ventanas y ejecutar como administrador.
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::Path;
use std::process::Command;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GetIfEntry2, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{ShellExecuteExW, ShellExecuteW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FlashWindowEx, LoadImageW, SendMessageTimeoutW, FLASHWINFO, FLASHW_ALL, FLASHW_TIMERNOFG, HICON,
    ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, SMTO_ABORTIFHUNG, SM_CXICON, SM_CXSMICON, SW_SHOWNORMAL,
    WM_GETICON, WM_SETICON,
};

const IF_TYPE_IEEE80211: u32 = 71;
const ERROR_BUFFER_OVERFLOW: u32 = 111;

/// IP del ordenador en la red por la que sale a internet (sin enviar nada: solo se consulta
/// la ruta). Si no hay salida, la primera IP privada que no sea de la red propia ni virtual.
pub fn lan_ip() -> Option<Ipv4Addr> {
    let routed = UdpSocket::bind("0.0.0.0:0").ok().and_then(|s| {
        s.connect("8.8.8.8:80").ok()?;
        match s.local_addr().ok()?.ip() {
            IpAddr::V4(v) if !v.is_loopback() && !v.is_link_local() => Some(v),
            _ => None,
        }
    });
    routed.or_else(|| {
        crate::certs::usable_ips()
            .into_iter()
            .find(|ip| ip.is_private() && ip.octets()[..3] != [192, 168, 137] && ip.octets()[..3] != [192, 168, 56])
    })
}

/// Tarjeta de red conectada.
#[derive(Clone, Debug, Default)]
pub struct NetIf {
    /// Índice de la interfaz (para cambiar su métrica).
    pub index: u32,
    /// Nombre en Windows ("Wi-Fi", "Ethernet 3"…).
    pub name: String,
    /// Descripción del driver ("Remote NDIS based Internet Sharing Device"…).
    pub description: String,
    pub wifi: bool,
    /// Tarjeta física (no virtual: VirtualBox, VPN, Hyper-V…).
    pub hardware: bool,
    /// IPv4 con la longitud del prefijo de su red.
    pub ipv4: Vec<(Ipv4Addr, u8)>,
    /// Tiene puerta de enlace: por ella puede salir a internet.
    pub gateway: bool,
    /// Métrica IPv4: entre varias salidas a internet, Windows usa la de menor métrica.
    pub metric: u32,
}

impl NetIf {
    pub fn has_ip(&self, ip: Ipv4Addr) -> bool {
        self.ipv4.iter().any(|(a, _)| *a == ip)
    }

    /// Si `peer` está en alguna de las redes de esta tarjeta.
    pub fn same_network(&self, peer: Ipv4Addr) -> bool {
        self.ipv4.iter().any(|(a, prefix)| {
            let mask = u32::MAX.checked_shl(32 - (*prefix).min(32) as u32).unwrap_or(0);
            u32::from(*a) & mask == u32::from(peer) & mask
        })
    }

    /// Tarjeta de un cable al dispositivo:
    /// - anclaje USB: Android (RNDIS o NCM) o iPhone/iPad (Apple Mobile Device Ethernet);
    /// - cable de red directo a otro equipo, sin router: tarjeta física con solo 169.254.x.x.
    pub fn is_cable(&self) -> bool {
        if self.wifi || self.ipv4.is_empty() {
            return false;
        }
        let d = self.description.to_ascii_lowercase();
        let tether = ["remote ndis", "rndis", "usbncm", "usb ncm", "apple mobile device ethernet"]
            .iter()
            .any(|k| d.contains(k));
        let direct = self.hardware && !self.gateway && self.ipv4.iter().all(|(ip, _)| ip.is_link_local());
        tether || direct
    }
}

/// Tarjetas de red conectadas y sus IPv4.
///
/// No se pregunta el nombre de la Wi-Fi (SSID): desde Windows 11 24H2 eso cuenta como usar la
/// ubicación, y Windows muestra el aviso en la barra de tareas cada vez que se consulta.
pub fn interfaces() -> Vec<NetIf> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER | GAA_FLAG_INCLUDE_GATEWAYS;
    let mut size = 16 * 1024u32;
    for _ in 0..3 {
        let mut buf = vec![0u64; (size as usize).div_ceil(8)]; // u64: alineado como pide la API
        let first = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        let rc = unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(first), &mut size) };
        if rc == ERROR_BUFFER_OVERFLOW {
            continue; // `size` ya trae lo que hace falta
        }
        if rc != 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        unsafe {
            let mut p = first;
            while let Some(a) = p.as_ref() {
                if a.OperStatus == IfOperStatusUp {
                    let mut ipv4 = Vec::new();
                    let mut u = a.FirstUnicastAddress;
                    while let Some(ua) = u.as_ref() {
                        let sa = ua.Address.lpSockaddr;
                        if !sa.is_null() && (*sa).sa_family == AF_INET {
                            let sin = &*(sa as *const SOCKADDR_IN);
                            ipv4.push((Ipv4Addr::from(sin.sin_addr.S_un.S_addr.to_ne_bytes()), ua.OnLinkPrefixLength));
                        }
                        u = ua.Next;
                    }
                    let index = a.Anonymous1.Anonymous.IfIndex;
                    out.push(NetIf {
                        index,
                        name: a.FriendlyName.to_string().unwrap_or_default(),
                        description: a.Description.to_string().unwrap_or_default(),
                        wifi: a.IfType == IF_TYPE_IEEE80211,
                        hardware: is_hardware(index),
                        ipv4,
                        gateway: !a.FirstGatewayAddress.is_null(),
                        metric: a.Ipv4Metric,
                    });
                }
                p = a.Next;
            }
        }
        return out;
    }
    Vec::new()
}

/// Si Windows marca la interfaz como tarjeta física (no VirtualBox, VPN, Hyper-V…).
fn is_hardware(index: u32) -> bool {
    let mut row = MIB_IF_ROW2 { InterfaceIndex: index, ..Default::default() };
    unsafe { GetIfEntry2(&mut row).is_ok() && row.InterfaceAndOperStatusFlags._bitfield & 1 != 0 }
}

/// IP de una red de verdad (ni la red propia, ni de enlace local, ni la del propio equipo).
fn is_lan(ip: &Ipv4Addr) -> bool {
    !ip.is_loopback() && !ip.is_link_local() && ip.octets()[..3] != [192, 168, 137]
}

/// Si el ordenador está conectado a otra Wi-Fi (además de la red propia, si la hay).
pub fn on_other_wifi(ifs: &[NetIf]) -> bool {
    ifs.iter().any(|i| i.wifi && i.ipv4.iter().any(|(ip, _)| is_lan(ip)))
}

/// Tarjeta que tiene esa IP.
pub fn interface_of(ifs: &[NetIf], ip: Ipv4Addr) -> Option<&NetIf> {
    ifs.iter().find(|i| i.has_ip(ip))
}

/// La tarjeta del cable al dispositivo, si hay una conectada.
pub fn cable_interface(ifs: &[NetIf]) -> Option<&NetIf> {
    ifs.iter().find(|i| i.is_cable())
}

/// Cómo saldría el ordenador a internet con el cable conectado.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CableRoute {
    /// El cable no da salida a internet, o Windows prefiere otra conexión: no se gastan datos.
    Fine,
    /// Windows prefiere el cable a la Wi-Fi o la red del ordenador: usaría los datos del móvil.
    PreferredOverOther,
    /// El ordenador no tiene otra salida a internet: si sale, será por el móvil.
    OnlyRoute,
}

pub fn cable_route(ifs: &[NetIf], cable: &NetIf) -> CableRoute {
    if !cable.gateway {
        return CableRoute::Fine;
    }
    let others = ifs.iter().filter(|i| i.index != cable.index && i.gateway);
    match others.map(|i| i.metric).min() {
        None => CableRoute::OnlyRoute,
        Some(best) if cable.metric <= best => CableRoute::PreferredOverOther,
        Some(_) => CableRoute::Fine,
    }
}

/// Escapa un campo para el formato de QR de Wi-Fi (WIFI:T:WPA;S:...;P:...;;).
fn wifi_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| if "\\;,:\"".contains(c) { vec!['\\', c] } else { vec![c] })
        .collect()
}

pub fn wifi_qr_text(ssid: &str, password: &str) -> String {
    format!("WIFI:T:WPA;S:{};P:{};;", wifi_escape(ssid), wifi_escape(password))
}

/// Código QR en SVG (para el panel).
pub fn qr_svg(text: &str) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes())?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build())
}

/// Código QR con caracteres de bloque (para la consola).
pub fn qr_text(text: &str) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes())?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .build())
}

/// Abre una dirección con el navegador predeterminado.
pub fn open_url(url: &str) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(url), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Abre una página como ventana de aplicación (Edge en modo app, sin barras de navegador).
/// Si no hay Edge, en el navegador predeterminado.
pub fn open_app_window(url: &str) {
    let edge = [
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ];
    if let Some(exe) = edge.iter().find(|p| Path::new(p).exists()) {
        if Command::new(exe).arg(format!("--app={url}")).arg("--window-size=1220,900").spawn().is_ok() {
            return;
        }
    }
    open_url(url);
}

/// Mantiene el icono de XtendedView, nítido, en las ventanas del panel (las de Edge en modo
/// aplicación con ese título exacto).
///
/// Edge saca el icono de esas ventanas del favicon, que siempre trata a 16 puntos (32 px con la
/// pantalla al 200 %), y lo estira hasta el tamaño de la barra de tareas: se ve borroso. Aquí se
/// les pone el icono del propio ejecutable al tamaño que pide su pantalla, y se vuelve a poner
/// si Edge lo cambia (lo hace al recargar la página). Comprobarlo cada segundo no cuesta nada.
pub fn keep_panel_icon(title: &'static str) {
    std::thread::spawn(move || {
        let title = HSTRING::from(title);
        let mut cache: HashMap<i32, HICON> = HashMap::new();
        let mut icon = |size: i32| -> Option<HICON> {
            if let Some(h) = cache.get(&size) {
                return Some(*h);
            }
            let module = unsafe { GetModuleHandleW(PCWSTR::null()) }.ok()?;
            // Recurso 1 del ejecutable (xtendedview.rc), al tamaño exacto: Windows elige la imagen
            // más cercana del .ico y la ajusta.
            let h = unsafe { LoadImageW(Some(module.into()), PCWSTR(1 as *const u16), IMAGE_ICON, size, size, LR_DEFAULTCOLOR) }.ok()?;
            let h = HICON(h.0);
            cache.insert(size, h);
            Some(h)
        };
        loop {
            let mut after = None;
            while let Ok(hwnd) = unsafe { FindWindowExW(None, after, w!("Chrome_WidgetWin_1"), &title) } {
                after = Some(hwnd);
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
                let big = icon(unsafe { GetSystemMetricsForDpi(SM_CXICON, dpi) });
                let small = icon(unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) });
                let (Some(big), Some(small)) = (big, small) else { continue };
                let mut current = 0usize;
                unsafe {
                    SendMessageTimeoutW(hwnd, WM_GETICON, WPARAM(ICON_BIG as usize), LPARAM(0), SMTO_ABORTIFHUNG, 200, Some(&mut current));
                    if current != big.0 as usize {
                        SendMessageTimeoutW(hwnd, WM_SETICON, WPARAM(ICON_BIG as usize), LPARAM(big.0 as isize), SMTO_ABORTIFHUNG, 200, None);
                        SendMessageTimeoutW(hwnd, WM_SETICON, WPARAM(ICON_SMALL as usize), LPARAM(small.0 as isize), SMTO_ABORTIFHUNG, 200, None);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn netif(description: &str, hardware: bool, ips: &[(&str, u8)], gateway: bool, metric: u32) -> NetIf {
        NetIf {
            index: metric, // único en cada prueba
            name: description.into(),
            description: description.into(),
            wifi: description.contains("Wi-Fi"),
            hardware,
            ipv4: ips.iter().map(|(ip, p)| (ip.parse().unwrap(), *p)).collect(),
            gateway,
            metric,
        }
    }

    #[test]
    fn recognises_cables_but_not_virtual_cards() {
        // Anclaje USB de Android (RNDIS y NCM) y de iPhone/iPad.
        assert!(netif("Remote NDIS based Internet Sharing Device", true, &[("192.168.42.10", 24)], true, 25).is_cable());
        assert!(netif("SAMSUNG Mobile USB Remote NDIS Network Device", true, &[("10.80.4.2", 24)], true, 25).is_cable());
        assert!(netif("UsbNcm Host Device", true, &[("192.168.10.5", 24)], true, 25).is_cable());
        assert!(netif("Apple Mobile Device Ethernet", true, &[("172.20.10.2", 28)], true, 25).is_cable());
        // Cable de red directo: tarjeta física con dirección automática 169.254.x.x.
        assert!(netif("Realtek USB GbE Family Controller", true, &[("169.254.10.20", 16)], false, 25).is_cable());
        // No son cables: VirtualBox (también con 169.254), la Wi-Fi, la red de casa por cable.
        assert!(!netif("VirtualBox Host-Only Ethernet Adapter", false, &[("169.254.90.16", 16)], false, 25).is_cable());
        assert!(!netif("Intel(R) Wi-Fi 7 BE201", true, &[("192.168.0.15", 24)], true, 35).is_cable());
        assert!(!netif("Realtek PCIe GbE Family Controller", true, &[("192.168.1.20", 24)], true, 25).is_cable());
    }

    #[test]
    fn same_network_uses_the_prefix() {
        let c = netif("Remote NDIS", true, &[("192.168.42.10", 24)], true, 25);
        assert!(c.same_network("192.168.42.129".parse().unwrap()));
        assert!(!c.same_network("192.168.0.20".parse().unwrap()));
    }

    #[test]
    fn detects_when_windows_would_use_the_phone_for_internet() {
        let wifi = netif("Intel(R) Wi-Fi 7", true, &[("192.168.0.15", 24)], true, 35);
        let cable = netif("Remote NDIS", true, &[("192.168.42.10", 24)], true, 25);
        assert_eq!(cable_route(&[wifi.clone(), cable.clone()], &cable), CableRoute::PreferredOverOther);
        let fixed = NetIf { metric: 5000, ..cable.clone() };
        assert_eq!(cable_route(&[wifi, fixed.clone()], &fixed), CableRoute::Fine);
        assert_eq!(cable_route(&[cable.clone()], &cable), CableRoute::OnlyRoute);
        let direct = netif("Realtek USB GbE", true, &[("169.254.1.2", 16)], false, 25);
        assert_eq!(cable_route(&[direct.clone()], &direct), CableRoute::Fine);
    }
}

/// Si hay abierta alguna ventana del panel (las de Edge en modo aplicación con ese título).
pub fn panel_window_open(title: &str) -> bool {
    let title = HSTRING::from(title);
    unsafe { FindWindowExW(None, None, w!("Chrome_WidgetWin_1"), &title) }.is_ok()
}

/// Hace parpadear en la barra de tareas la ventana del panel (hasta que se ponga delante), para
/// avisar de que un dispositivo espera a que lo acepten.
pub fn flash_panel_window(title: &str) {
    let title = HSTRING::from(title);
    let mut after = None;
    while let Ok(hwnd) = unsafe { FindWindowExW(None, after, w!("Chrome_WidgetWin_1"), &title) } {
        after = Some(hwnd);
        let info = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        unsafe {
            let _ = FlashWindowEx(&info);
        }
    }
}

/// Ejecuta un programa como administrador (aviso de Windows) y espera a que termine.
/// Devuelve su código de salida.
pub fn run_elevated(file: &str, params: &str) -> Result<u32> {
    let file = HSTRING::from(file);
    let params = HSTRING::from(params);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        if ShellExecuteExW(&mut info).is_err() {
            bail!("no se ha dado permiso de administrador");
        }
        let mut code = 1u32;
        WaitForSingleObject(info.hProcess, INFINITE);
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        Ok(code)
    }
}
