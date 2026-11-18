//! Inyección de entrada en Windows.
//!
//! - El táctil del cliente se inyecta como táctil real (InjectTouchInput): desplazar con el
//!   dedo, pellizcar, etc. funcionan como en una pantalla táctil.
//! - El lápiz (S Pen, Apple Pencil) se inyecta como lápiz real con un dispositivo sintético
//!   (InjectSyntheticPointerInput): presión, inclinación, giro, botón lateral, goma y
//!   desplazamiento sin tocar. Si Windows no lo permite, se usa el ratón.
//! - El ratón se inyecta como ratón (SendInput).
use serde::Deserialize;
use std::collections::HashMap;
use std::mem::size_of;
use std::time::Instant;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS,
};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_DEFAULT,
    POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::Pointer::{
    InitializeTouchInjection, InjectSyntheticPointerInput, InjectTouchInput, POINTER_FLAGS, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN, POINTER_FLAG_FIRSTBUTTON,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_PRIMARY, POINTER_FLAG_SECONDBUTTON, POINTER_FLAG_UP,
    POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO, TOUCH_FEEDBACK_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PT_PEN, PT_TOUCH, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

// Valores de winuser.h para el lápiz.
const PEN_FLAG_BARREL: u32 = 0x1;
const PEN_FLAG_INVERTED: u32 = 0x2;
const PEN_FLAG_ERASER: u32 = 0x4;
const PEN_MASK_ALL: u32 = 0x1 | 0x2 | 0x4 | 0x8; // presión, giro, inclinación X e Y

/// Dispositivo de lápiz sintético (el identificador se puede usar desde cualquier hilo).
struct PenDevice(HSYNTHETICPOINTERDEVICE);
unsafe impl Send for PenDevice {}

impl Drop for PenDevice {
    fn drop(&mut self) {
        unsafe { DestroySyntheticPointerDevice(self.0) };
    }
}

const MAX_CONTACTS: u32 = 10;
const TOUCH_MASK_ALL: u32 = 0x1 | 0x2 | 0x4; // área de contacto, orientación, presión

/// Rectángulo del monitor capturado, en píxeles físicos del escritorio virtual.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Deserialize)]
pub struct PointerEvent {
    pub e: String,
    #[serde(default)]
    pub id: serde_json::Value,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub b: i32,
    /// Presión (0..1).
    #[serde(default)]
    pub p: f64,
    /// Inclinación del lápiz en grados (-90..90).
    #[serde(default)]
    pub tx: f64,
    #[serde(default)]
    pub ty: f64,
    /// Giro del lápiz sobre su eje (0..359).
    #[serde(default)]
    pub tw: f64,
    /// Botones pulsados (2 = lateral, 32 = goma).
    #[serde(default)]
    pub bt: u32,
}

/// Uno para todos los dispositivos: Windows solo tiene un táctil inyectado. Cada evento llega
/// con el monitor del dispositivo que lo manda, y los contactos se distinguen por dispositivo
/// para que dos dedos en pantallas distintas no se confundan.
pub struct Injector {
    touch_ok: bool,
    contacts: Vec<(u32, i32, i32)>, // (slot, x, y)
    slots: HashMap<String, u32>,    // "dispositivo:dedo" → slot
    last_touch: Instant,
    mouse_buttons: Vec<(u64, i32)>,
    pen: Option<PenDevice>,
    pen_unavailable: bool,
    pen_contact: bool,
    pen_owner: u64,
    pen_at: (i32, i32),
}

impl Injector {
    pub fn new() -> Self {
        let touch_ok = unsafe { InitializeTouchInjection(MAX_CONTACTS, TOUCH_FEEDBACK_DEFAULT) }.is_ok();
        Self {
            touch_ok,
            contacts: Vec::new(),
            slots: HashMap::new(),
            last_touch: Instant::now(),
            mouse_buttons: Vec::new(),
            pen: None,
            pen_unavailable: false,
            pen_contact: false,
            pen_owner: 0,
            pen_at: (0, 0),
        }
    }

    /// El lápiz sintético se crea la primera vez que se usa.
    fn pen_device(&mut self) -> Option<HSYNTHETICPOINTERDEVICE> {
        if self.pen.is_none() && !self.pen_unavailable {
            match unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) } {
                Ok(dev) => self.pen = Some(PenDevice(dev)),
                Err(e) => {
                    log::warn!("Windows no permite el lápiz sintético ({e}); el lápiz funcionará como ratón");
                    self.pen_unavailable = true;
                }
            }
        }
        self.pen.as_ref().map(|d| d.0)
    }

    fn pen(&mut self, ev: &PointerEvent, x: i32, y: i32) -> bool {
        let Some(dev) = self.pen_device() else { return false };
        let barrel = ev.bt & 2 != 0;
        // En contacto: "primer botón" con la punta sola; "segundo botón" con el lateral pulsado.
        let contact_button = if barrel { POINTER_FLAG_SECONDBUTTON } else { POINTER_FLAG_FIRSTBUTTON };
        let flags = match ev.e.as_str() {
            "down" => {
                self.pen_contact = true;
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN | contact_button
            }
            "move" if self.pen_contact => POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE | contact_button,
            "move" => POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE, // flotando, sin tocar
            "up" => {
                self.pen_contact = false;
                POINTER_FLAG_INRANGE | POINTER_FLAG_UP
            }
            "cancel" => {
                self.pen_contact = false;
                POINTER_FLAG_UP | POINTER_FLAG_CANCELED
            }
            "leave" if !self.pen_contact => POINTER_FLAG_UPDATE, // fuera de alcance
            _ => return true,
        };
        let mut pen_flags = 0;
        if barrel {
            pen_flags |= PEN_FLAG_BARREL;
        }
        if ev.bt & 32 != 0 {
            pen_flags |= PEN_FLAG_ERASER | PEN_FLAG_INVERTED;
        }
        let pressure = if self.pen_contact { (ev.p.clamp(0.0, 1.0) * 1024.0).round().max(1.0) as u32 } else { 0 };
        self.pen_at = (x, y);
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerFlags: flags | POINTER_FLAG_PRIMARY,
                        ptPixelLocation: POINT { x, y },
                        ..Default::default()
                    },
                    penFlags: pen_flags,
                    penMask: PEN_MASK_ALL,
                    pressure,
                    rotation: (ev.tw.rem_euclid(360.0)) as u32,
                    tiltX: ev.tx.clamp(-90.0, 90.0) as i32,
                    tiltY: ev.ty.clamp(-90.0, 90.0) as i32,
                },
            },
        };
        if let Err(e) = unsafe { InjectSyntheticPointerInput(dev, &[info]) } {
            log::debug!("Inyección de lápiz rechazada ({}): {e}", ev.e);
        }
        true
    }

    fn to_screen(m: Rect, nx: f64, ny: f64) -> (i32, i32) {
        let nx = nx.clamp(0.0, 1.0);
        let ny = ny.clamp(0.0, 1.0);
        (m.x + (nx * (m.w - 1) as f64).round() as i32, m.y + (ny * (m.h - 1) as f64).round() as i32)
    }

    /// Evento del dispositivo `owner`, que muestra el monitor `monitor`.
    pub fn pointer(&mut self, owner: u64, monitor: Rect, ev: &PointerEvent) {
        let (x, y) = Self::to_screen(monitor, ev.x, ev.y);
        if ev.kind == "touch" && self.touch_ok {
            self.touch(&format!("{owner}:{}", ev.id), &ev.e, x, y);
        } else if ev.kind == "pen" && self.pen(ev, x, y) {
            if ev.e == "down" {
                self.pen_owner = owner;
            }
        } else if ev.e != "leave" {
            self.mouse(owner, &ev.e, x, y, ev.b);
        }
    }

    pub fn wheel(&self, dx: f64, dy: f64) {
        if dy != 0.0 {
            send_mouse(0, 0, MOUSEEVENTF_WHEEL, (-dy) as i32);
        }
        if dx != 0.0 {
            send_mouse(0, 0, MOUSEEVENTF_HWHEEL, dx as i32);
        }
    }

    fn mouse(&mut self, owner: u64, kind: &str, x: i32, y: i32, button: i32) {
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let ax = ((x - vx) as f64 * 65535.0 / (vw - 1).max(1) as f64).round() as i32;
        let ay = ((y - vy) as f64 * 65535.0 / (vh - 1).max(1) as f64).round() as i32;
        let mut flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
        let (down, up) = button_flags(button);
        match kind {
            "down" => {
                flags |= down;
                self.mouse_buttons.push((owner, button));
            }
            "up" | "cancel" if self.mouse_buttons.contains(&(owner, button)) => {
                flags |= up;
                self.mouse_buttons.retain(|b| *b != (owner, button));
            }
            _ => {}
        }
        send_mouse(ax, ay, flags, 0);
    }

    fn touch(&mut self, pid: &str, kind: &str, x: i32, y: i32) {
        if kind == "down" {
            if self.slots.contains_key(pid) || self.slots.len() as u32 >= MAX_CONTACTS {
                return;
            }
            let slot = (0..MAX_CONTACTS)
                .find(|s| !self.contacts.iter().any(|c| c.0 == *s))
                .unwrap_or(0);
            self.slots.insert(pid.to_string(), slot);
        }
        let Some(&slot) = self.slots.get(pid) else { return };
        match self.contacts.iter_mut().find(|c| c.0 == slot) {
            Some(c) => {
                c.1 = x;
                c.2 = y;
            }
            None => self.contacts.push((slot, x, y)),
        }
        let flags = match kind {
            "down" => POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            "move" => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            "up" => POINTER_FLAG_UP,
            "cancel" => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
            _ => return,
        };
        self.inject(&[(slot, flags)]);
        if kind == "up" || kind == "cancel" {
            self.contacts.retain(|c| c.0 != slot);
            self.slots.remove(pid);
        }
    }

    /// Windows cancela los contactos que no se actualizan: se reenvían si están quietos.
    pub fn keepalive(&mut self) {
        if !self.contacts.is_empty() && self.last_touch.elapsed().as_millis() > 100 {
            self.inject(&[]);
        }
    }

    /// Suelta todo lo que tenga pulsado el dispositivo `owner` (se ha desconectado a mitad).
    pub fn release(&mut self, owner: u64) {
        let prefix = format!("{owner}:");
        let mine: Vec<u32> = self.slots.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(_, s)| *s).collect();
        if !mine.is_empty() {
            let lift: Vec<(u32, POINTER_FLAGS)> = mine.iter().map(|s| (*s, POINTER_FLAG_UP | POINTER_FLAG_CANCELED)).collect();
            self.inject(&lift);
            self.contacts.retain(|c| !mine.contains(&c.0));
            self.slots.retain(|k, _| !k.starts_with(&prefix));
        }
        let (gone, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.mouse_buttons).into_iter().partition(|(o, _)| *o == owner);
        self.mouse_buttons = keep;
        for (_, b) in gone {
            send_mouse(0, 0, button_flags(b).1, 0);
        }
        if self.pen_contact && self.pen_owner == owner {
            // Levantar el lápiz si el visor se desconecta a mitad de un trazo.
            let lift = PointerEvent {
                e: "cancel".into(),
                id: serde_json::Value::Null,
                kind: "pen".into(),
                x: 0.0,
                y: 0.0,
                b: 0,
                p: 0.0,
                tx: 0.0,
                ty: 0.0,
                tw: 0.0,
                bt: 0,
            };
            let (x, y) = self.pen_at;
            self.pen(&lift, x, y);
        }
    }

    /// Cada inyección debe incluir todos los contactos activos.
    fn inject(&mut self, changed: &[(u32, POINTER_FLAGS)]) {
        let infos: Vec<POINTER_TOUCH_INFO> = self
            .contacts
            .iter()
            .map(|&(slot, x, y)| {
                let flags = changed
                    .iter()
                    .find(|c| c.0 == slot)
                    .map(|c| c.1)
                    .unwrap_or(POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT);
                POINTER_TOUCH_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_TOUCH,
                        pointerId: slot,
                        pointerFlags: flags,
                        ptPixelLocation: POINT { x, y },
                        ..Default::default()
                    },
                    touchMask: TOUCH_MASK_ALL,
                    rcContact: RECT { left: x - 2, top: y - 2, right: x + 2, bottom: y + 2 },
                    orientation: 90,
                    pressure: 32000,
                    ..Default::default()
                }
            })
            .collect();
        if !infos.is_empty() {
            let _ = unsafe { InjectTouchInput(&infos) };
        }
        self.last_touch = Instant::now();
    }
}

fn button_flags(button: i32) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
    match button {
        1 => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        2 => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    }
}

fn send_mouse(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, data: i32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(&[input], size_of::<INPUT>() as i32);
    }
}
