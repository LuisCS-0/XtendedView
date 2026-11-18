//! Icono en el área de notificación (junto al reloj): con un clic se abre el panel y con el
//! botón derecho, un menú con «Abrir panel» y «Salir». Instalado, XtendedView no tiene ventana
//! de consola: así se vuelve al panel después de cerrarlo y se sale del programa.
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_LARGE_ICON, NIIF_USER, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos,
    GetMessageW, GetSystemMetrics, LoadImageW, PostMessageW, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, MF_SEPARATOR,
    MF_STRING, MSG, SM_CXICON, SM_CXSMICON, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP,
    WM_CONTEXTMENU, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_USER, WNDCLASSW, WS_OVERLAPPED,
};

/// Clic en una notificación del icono (NIN_BALLOONUSERCLICK).
const BALLOON_CLICK: u32 = WM_USER + 5;
/// Ventana del icono, para notificar o quitarlo desde otros hilos.
static HWND_ICON: AtomicIsize = AtomicIsize::new(0);

pub enum TrayEvent {
    OpenPanel,
    Quit,
}

const CALLBACK: u32 = WM_APP + 1;
const MENU_OPEN: usize = 1;
const MENU_QUIT: usize = 2;

static EVENTS: OnceLock<UnboundedSender<TrayEvent>> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

/// Pone el icono y atiende sus clics en un hilo propio. Los eventos llegan por `events`.
pub fn spawn(events: UnboundedSender<TrayEvent>) {
    if EVENTS.set(events).is_err() {
        return; // ya está puesto
    }
    std::thread::Builder::new()
        .name("icono".into())
        .spawn(|| unsafe {
            if let Err(e) = run() {
                log::warn!("No se pudo poner el icono junto al reloj: {e:#}");
            }
        })
        .expect("no se pudo crear el hilo del icono");
}

fn send(ev: TrayEvent) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(ev);
    }
}

fn copy_text(dst: &mut [u16], text: &str) {
    let t: Vec<u16> = text.encode_utf16().take(dst.len() - 1).collect();
    dst[..t.len()].copy_from_slice(&t);
    dst[t.len()] = 0;
}

fn icon_hwnd() -> Option<HWND> {
    let h = HWND_ICON.load(Ordering::Relaxed);
    (h != 0).then_some(HWND(h as *mut _))
}

/// Notificación de Windows desde el icono (p. ej. un dispositivo pide conectarse con el panel
/// cerrado). Al pulsarla se abre el panel.
pub fn notify(title: &str, text: &str) {
    let Some(hwnd) = icon_hwnd() else {
        log::warn!("Sin icono junto al reloj: no se puede avisar de «{title}»");
        return;
    };
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_INFO,
            dwInfoFlags: NIIF_USER | NIIF_LARGE_ICON,
            ..Default::default()
        };
        copy_text(&mut nid.szInfoTitle, title);
        copy_text(&mut nid.szInfo, text);
        if let Ok(module) = GetModuleHandleW(PCWSTR::null()) {
            let size = GetSystemMetrics(SM_CXICON);
            if let Ok(h) = LoadImageW(Some(module.into()), PCWSTR(1 as *const u16), IMAGE_ICON, size, size, LR_DEFAULTCOLOR) {
                nid.hBalloonIcon = HICON(h.0);
            }
        }
        if Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            log::info!("Notificación de Windows: {title}");
        } else {
            log::warn!("Windows no ha mostrado la notificación «{title}»");
        }
    }
}

/// Quita el icono (al salir; si no, se queda hasta que se pasa el ratón por encima).
pub fn remove() {
    let Some(hwnd) = icon_hwnd() else { return };
    let nid = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() };
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

unsafe fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: CALLBACK,
        ..Default::default()
    };
    // Recurso 1 del ejecutable (xtendedview.rc), al tamaño de los iconos pequeños.
    if let Ok(module) = GetModuleHandleW(PCWSTR::null()) {
        let size = GetSystemMetrics(SM_CXSMICON);
        if let Ok(h) = LoadImageW(Some(module.into()), PCWSTR(1 as *const u16), IMAGE_ICON, size, size, LR_DEFAULTCOLOR) {
            nid.hIcon = HICON(h.0);
        }
    }
    copy_text(&mut nid.szTip, "XtendedView");
    nid
}

unsafe fn run() -> anyhow::Result<()> {
    let module = GetModuleHandleW(PCWSTR::null())?;
    let class = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: module.into(), lpszClassName: w!("XtendedViewIcono"), ..Default::default() };
    RegisterClassW(&class);
    // Ventana invisible: solo recibe los mensajes del icono.
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("XtendedViewIcono"),
        w!("XtendedView"),
        WS_OVERLAPPED,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(module.into()),
        None,
    )?;
    let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
    HWND_ICON.store(hwnd.0 as isize, Ordering::Relaxed);
    let nid = icon_data(hwnd);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    Ok(())
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN, w!("Abrir panel"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Salir de XtendedView"));
    let mut at = POINT::default();
    let _ = GetCursorPos(&mut at);
    // Sin esto el menú no se cierra al pulsar fuera de él.
    let _ = SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, Some(0), hwnd, None);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    match chosen.0 as usize {
        MENU_OPEN => send(TrayEvent::OpenPanel),
        MENU_QUIT => send(TrayEvent::Quit),
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == CALLBACK {
        match lparam.0 as u32 {
            WM_LBUTTONUP | BALLOON_CLICK => send(TrayEvent::OpenPanel),
            WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
            _ => {}
        }
        return LRESULT(0);
    }
    // Si el Explorador se reinicia, el icono desaparece: se vuelve a poner.
    if TASKBAR_CREATED.get() == Some(&msg) {
        let nid = icon_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
