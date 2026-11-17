//! Inyección de entrada en Windows.
//!
//! - El táctil del cliente se inyecta como táctil real (InjectTouchInput): desplazar con el
//!   dedo, pellizcar, etc. funcionan como en una pantalla táctil.
//! - El ratón y el lápiz se inyectan como ratón (SendInput). La presión del lápiz, más adelante.
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
use windows::Win32::UI::Input::Pointer::{
    InitializeTouchInjection, InjectTouchInput, POINTER_FLAGS, POINTER_FLAG_CANCELED,
    POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP,
    POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_TOUCH_INFO, TOUCH_FEEDBACK_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PT_TOUCH, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

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
}

pub struct Injector {
    pub monitor: Option<Rect>,
    touch_ok: bool,
    contacts: Vec<(u32, i32, i32)>, // (slot, x, y)
    slots: HashMap<String, u32>,
    last_touch: Instant,
    mouse_buttons: Vec<i32>,
}

impl Injector {
    pub fn new() -> Self {
        let touch_ok = unsafe { InitializeTouchInjection(MAX_CONTACTS, TOUCH_FEEDBACK_DEFAULT) }.is_ok();
        Self {
            monitor: None,
            touch_ok,
            contacts: Vec::new(),
            slots: HashMap::new(),
            last_touch: Instant::now(),
            mouse_buttons: Vec::new(),
        }
    }

    fn to_screen(&self, nx: f64, ny: f64) -> Option<(i32, i32)> {
        let m = self.monitor?;
        let nx = nx.clamp(0.0, 1.0);
        let ny = ny.clamp(0.0, 1.0);
        Some((
            m.x + (nx * (m.w - 1) as f64).round() as i32,
            m.y + (ny * (m.h - 1) as f64).round() as i32,
        ))
    }

    pub fn pointer(&mut self, ev: &PointerEvent) {
        let Some((x, y)) = self.to_screen(ev.x, ev.y) else { return };
        if ev.kind == "touch" && self.touch_ok {
            self.touch(&ev.id.to_string(), &ev.e, x, y);
        } else {
            self.mouse(&ev.e, x, y, ev.b);
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

    fn mouse(&mut self, kind: &str, x: i32, y: i32, button: i32) {
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
                self.mouse_buttons.push(button);
            }
            "up" | "cancel" if self.mouse_buttons.contains(&button) => {
                flags |= up;
                self.mouse_buttons.retain(|b| *b != button);
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

    pub fn release_all(&mut self) {
        if !self.contacts.is_empty() {
            let all: Vec<(u32, POINTER_FLAGS)> = self
                .contacts
                .iter()
                .map(|c| (c.0, POINTER_FLAG_UP | POINTER_FLAG_CANCELED))
                .collect();
            self.inject(&all);
            self.contacts.clear();
            self.slots.clear();
        }
        for b in std::mem::take(&mut self.mouse_buttons) {
            send_mouse(0, 0, button_flags(b).1, 0);
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
