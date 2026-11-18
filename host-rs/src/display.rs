//! Monitor virtual a la medida del cliente: crearlo, elegir su modo y quitarlo.
use crate::capture::{self, OutputInfo};
use crate::input::Rect;
use crate::vdd::Vdd;
use anyhow::{bail, Result};
use std::collections::HashSet;
use std::mem::size_of;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    SetDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_TYPE, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    QDC_ONLY_ACTIVE_PATHS, SDC_APPLY, SDC_TOPOLOGY_EXTEND,
};
use windows::Win32::Foundation::LUID;
use windows::Win32::Foundation::POINTL;
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsExW, EnumDisplaySettingsW, CDS_UPDATEREGISTRY, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DMDO_270, DMDO_90, DMDO_DEFAULT, DM_DISPLAYFREQUENCY, DM_DISPLAYORIENTATION,
    DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, EDS_ROTATEDMODE, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
};

/// Modo nativo del monitor (en horizontal, tal como lo ofrece el driver).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}@{}", self.width, self.height, self.hz)
    }
}

/// Modo más orientación. El driver solo ofrece modos horizontales: para una pantalla
/// vertical (el Fold plegado, una tablet en vertical) se gira el monitor virtual, como se
/// haría con un monitor físico puesto de pie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub mode: Mode,
    pub portrait: bool,
}

impl Target {
    /// Tamaño del escritorio tal y como se ve (ya girado).
    pub fn desktop(&self) -> (u32, u32) {
        if self.portrait {
            (self.mode.height, self.mode.width)
        } else {
            (self.mode.width, self.mode.height)
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (w, h) = self.desktop();
        write!(f, "{w}x{h}@{}{}", self.mode.hz, if self.portrait { " (vertical)" } else { "" })
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn devmode() -> DEVMODEW {
    DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() }
}

/// Modos nativos (sin girar) que admite un monitor (\\.\DISPLAYn).
pub fn modes(device: &str) -> Vec<Mode> {
    let name = wide(device);
    let mut out = Vec::new();
    for i in 0.. {
        let mut dm = devmode();
        // EDS_ROTATEDMODE: todos los modos, sea cual sea la orientación actual del monitor.
        let ok = unsafe { EnumDisplaySettingsExW(PCWSTR(name.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(i), &mut dm, EDS_ROTATEDMODE) };
        if !ok.as_bool() {
            break;
        }
        let orientation = unsafe { dm.Anonymous1.Anonymous2.dmDisplayOrientation };
        let m = Mode { width: dm.dmPelsWidth, height: dm.dmPelsHeight, hz: dm.dmDisplayFrequency };
        if orientation == DMDO_DEFAULT && dm.dmBitsPerPel >= 24 && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

pub fn current(device: &str) -> Option<Target> {
    let name = wide(device);
    let mut dm = devmode();
    if !unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool() {
        return None;
    }
    let o = unsafe { dm.Anonymous1.Anonymous2.dmDisplayOrientation };
    let portrait = o == DMDO_90 || o == DMDO_270;
    let (w, h) = if portrait { (dm.dmPelsHeight, dm.dmPelsWidth) } else { (dm.dmPelsWidth, dm.dmPelsHeight) };
    Some(Target { mode: Mode { width: w, height: h, hz: dm.dmDisplayFrequency }, portrait })
}

pub fn apply(device: &str, t: Target) -> Result<()> {
    let name = wide(device);
    let mut dm = devmode();
    let (w, h) = t.desktop();
    dm.dmPelsWidth = w;
    dm.dmPelsHeight = h;
    dm.dmDisplayFrequency = t.mode.hz;
    dm.Anonymous1.Anonymous2.dmDisplayOrientation = if t.portrait { DMDO_90 } else { DMDO_DEFAULT };
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_DISPLAYORIENTATION;
    let r = unsafe { ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(&dm), None, CDS_UPDATEREGISTRY, None) };
    if r != DISP_CHANGE_SUCCESSFUL {
        bail!("Windows no aceptó el modo {t} ({})", r.0);
    }
    Ok(())
}

/// Posición actual del monitor en el escritorio (esquina superior izquierda).
pub fn position(device: &str) -> Option<(i32, i32)> {
    let name = wide(device);
    let mut dm = devmode();
    if !unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool() {
        return None;
    }
    let p = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
    Some((p.x, p.y))
}

/// Mueve el monitor a esa posición del escritorio.
pub fn place(device: &str, (x, y): (i32, i32)) -> Result<()> {
    let name = wide(device);
    let mut dm = devmode();
    dm.Anonymous1.Anonymous2.dmPosition = POINTL { x, y };
    dm.dmFields = DM_POSITION;
    let r = unsafe { ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(&dm), None, CDS_UPDATEREGISTRY, None) };
    if r != DISP_CHANGE_SUCCESSFUL {
        bail!("Windows no aceptó mover la pantalla a ({x}, {y}) ({})", r.0);
    }
    Ok(())
}

/// Posición para un monitor de w×h al lado indicado del monitor principal, centrado en ese lado.
pub fn side_position(main: Rect, (w, h): (u32, u32), side: &str) -> (i32, i32) {
    let (w, h) = (w as i32, h as i32);
    match side {
        "left" => (main.x - w, main.y + (main.h - h) / 2),
        "top" => (main.x + (main.w - w) / 2, main.y - h),
        "bottom" => (main.x + (main.w - w) / 2, main.y + main.h),
        _ => (main.x + main.w, main.y + (main.h - h) / 2),
    }
}

/// El modo y la orientación que mejor encajan con una pantalla de w×h. El booleano indica si
/// la resolución es exacta.
pub fn best_target(available: &[Mode], w: u32, h: u32, hz: u32) -> Option<(Target, bool)> {
    let portrait = h > w;
    let (lw, lh) = if portrait { (h, w) } else { (w, h) };
    best_mode(available, lw, lh, hz).map(|(mode, exact)| (Target { mode, portrait }, exact))
}

/// El modo que mejor encaja con una pantalla de w×h a hz. El booleano indica si es exacto.
///
/// Sin modo exacto se busca uno que quepa, con la proporción más parecida (en tramos del 2 %)
/// y, entre esos, el más grande.
pub fn best_mode(available: &[Mode], w: u32, h: u32, hz: u32) -> Option<(Mode, bool)> {
    let hz_gap = |m: &Mode| -((m.hz as i64 - hz as i64).abs());
    if let Some(m) = available
        .iter()
        .filter(|m| m.width == w && m.height == h)
        .max_by_key(|m| (hz_gap(m), m.hz))
    {
        return Some((*m, true));
    }
    let aspect = w as f64 / h as f64;
    available
        .iter()
        .max_by_key(|m| {
            let a = m.width as f64 / m.height as f64;
            let err = ((a - aspect).abs() / aspect * 50.0).round() as i64;
            let fits = m.width <= w && m.height <= h;
            (fits, -err, m.width as u64 * m.height as u64, hz_gap(m))
        })
        .map(|m| (*m, false))
}

// ------------------------------------------------------------------ escala (125 %, 150 %…)
//
// La «Escala» de la configuración de pantalla de Windows, por monitor: agranda iconos, textos y
// barras sin bajar la resolución. Windows no la documenta: la guarda como pasos relativos a la
// escala recomendada para ese monitor y se lee y cambia con DisplayConfigGet/SetDeviceInfo y los
// tipos -3 y -4 (lo mismo que hace su configuración de pantalla).

/// Escalas que ofrece Windows, en orden. El paso relativo 0 es la recomendada.
const DPI_STEPS: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

#[repr(C)]
struct DpiScaleGet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    min: i32,
    cur: i32,
    max: i32,
}

#[repr(C)]
struct DpiScaleSet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    scale: i32,
}

/// Adaptador e identificador de origen de un monitor (\\.\DISPLAYn) en la API de DisplayConfig.
fn display_source(device: &str) -> Option<(LUID, u32)> {
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).ok().ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
            .ok()
            .ok()?;
        for p in &paths[..np as usize] {
            let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            name.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            name.header.adapterId = p.sourceInfo.adapterId;
            name.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut name.header) != 0 {
                continue;
            }
            let len = name.viewGdiDeviceName.iter().position(|&c| c == 0).unwrap_or(name.viewGdiDeviceName.len());
            if String::from_utf16_lossy(&name.viewGdiDeviceName[..len]) == device {
                return Some((p.sourceInfo.adapterId, p.sourceInfo.id));
            }
        }
        None
    }
}

/// (índice de la escala recomendada en DPI_STEPS, paso actual, mínimo, máximo).
fn dpi_steps(adapter: LUID, id: u32) -> Option<(i32, i32, i32, i32)> {
    let mut get = DpiScaleGet {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(-3),
            size: size_of::<DpiScaleGet>() as u32,
            adapterId: adapter,
            id,
        },
        min: 0,
        cur: 0,
        max: 0,
    };
    if unsafe { DisplayConfigGetDeviceInfo(&mut get.header) } != 0 {
        return None;
    }
    // El mínimo siempre es 100 %: la recomendada está tantos pasos por encima.
    Some((-get.min, get.cur, get.min, get.max))
}

/// Escala actual del monitor, en %.
pub fn scale(device: &str) -> Option<u32> {
    let (adapter, id) = display_source(device)?;
    let (rec, cur, _, _) = dpi_steps(adapter, id)?;
    DPI_STEPS.get((rec + cur).max(0) as usize).copied()
}

/// Pone la escala del monitor (en %; 0 = la recomendada por Windows). Si no la admite, la más
/// cercana que admita.
pub fn set_scale(device: &str, percent: u32) -> Result<()> {
    let Some((adapter, id)) = display_source(device) else { bail!("no encuentro el monitor {device}") };
    let Some((rec, cur, min, max)) = dpi_steps(adapter, id) else { bail!("Windows no da la escala de {device}") };
    let target = if percent == 0 {
        0
    } else {
        let idx = DPI_STEPS.iter().position(|&s| s >= percent).unwrap_or(DPI_STEPS.len() - 1) as i32;
        (idx - rec).clamp(min, max)
    };
    if target == cur {
        return Ok(());
    }
    let set = DpiScaleSet {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(-4),
            size: size_of::<DpiScaleSet>() as u32,
            adapterId: adapter,
            id,
        },
        scale: target,
    };
    let r = unsafe { DisplayConfigSetDeviceInfo(&set.header) };
    if r != 0 {
        bail!("Windows no aceptó la escala ({r})");
    }
    Ok(())
}

/// Conecta un monitor virtual y espera a que Windows lo añada al escritorio.
pub fn create_virtual(vdd: &Vdd) -> Result<(u32, OutputInfo)> {
    let before: HashSet<String> = capture::outputs()?.into_iter().map(|o| o.name).collect();
    let index = vdd.add()?;
    let start = Instant::now();
    let mut extended = false;
    loop {
        if let Some(o) = capture::outputs()?.into_iter().find(|o| !before.contains(&o.name)) {
            return Ok((index, o));
        }
        // Si Windows lo ha conectado como duplicado, se pasa a "extender".
        if !extended && start.elapsed() > Duration::from_secs(2) {
            unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND) };
            extended = true;
        }
        if start.elapsed() > Duration::from_secs(8) {
            let _ = vdd.remove(index);
            bail!("Windows no ha activado el monitor virtual");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Espera a que el monitor tenga el tamaño (y la posición, si se indica) pedidos y lo devuelve.
pub fn wait_output(name: &str, (w, h): (u32, u32), at: Option<(i32, i32)>) -> Result<OutputInfo> {
    let start = Instant::now();
    loop {
        let found = capture::outputs()?.into_iter().find(|o| o.name == name);
        let placed = |o: &OutputInfo| at.is_none_or(|(x, y)| o.rect.x == x && o.rect.y == y);
        match found {
            Some(o) if o.rect.w as u32 == w && o.rect.h as u32 == h && placed(&o) => return Ok(o),
            Some(o) if start.elapsed() > Duration::from_secs(3) => return Ok(o),
            None if start.elapsed() > Duration::from_secs(3) => bail!("el monitor virtual ha desaparecido"),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(width: u32, height: u32, hz: u32) -> Mode {
        Mode { width, height, hz }
    }

    #[test]
    fn prefers_exact_then_aspect_then_size() {
        let list = [m(1920, 1080, 60), m(1600, 1200, 60), m(2560, 1600, 60), m(2560, 1600, 144), m(2184, 1968, 60)];
        assert_eq!(best_mode(&list, 2184, 1968, 120), Some((m(2184, 1968, 60), true)));
        assert_eq!(best_mode(&list, 2560, 1600, 120), Some((m(2560, 1600, 144), true)));
        // Fold sin modo exacto: lo más cuadrado que quepa.
        assert_eq!(best_mode(&list[..4], 2184, 1968, 60), Some((m(1600, 1200, 60), false)));
        // iPad Air 11" (2360x1640): 16:10 cabe y es lo más parecido.
        assert_eq!(best_mode(&list[..4], 2360, 1640, 60).map(|x| x.0), Some(m(1600, 1200, 60)));
    }

    #[test]
    fn portrait_uses_a_rotated_landscape_mode() {
        let list = [m(1920, 1080, 60), m(1600, 1200, 60), m(2560, 1080, 60)];
        // Fold plegado (1247x1714 en vertical): 1600x1200 girado, escritorio de 1200x1600.
        let (t, exact) = best_target(&list, 1247, 1714, 60).unwrap();
        assert_eq!((t, exact), (Target { mode: m(1600, 1200, 60), portrait: true }, false));
        assert_eq!(t.desktop(), (1200, 1600));
        assert!(!best_target(&list, 1866, 1021, 60).unwrap().0.portrait);
    }

    #[test]
    fn side_positions_are_centered_and_adjacent() {
        let main = Rect { x: 0, y: 0, w: 2880, h: 1800 };
        assert_eq!(side_position(main, (1600, 1200), "right"), (2880, 300));
        assert_eq!(side_position(main, (1600, 1200), "left"), (-1600, 300));
        assert_eq!(side_position(main, (1600, 1200), "top"), (640, -1200));
        assert_eq!(side_position(main, (1200, 1600), "bottom"), (840, 1800));
    }
}
