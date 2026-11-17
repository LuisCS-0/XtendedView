//! Captura del escritorio con Desktop Duplication (DXGI). La imagen nunca sale de la GPU.
//!
//! El cursor no viene dibujado en la imagen: DXGI da su posición y su forma por separado.
//! Se envían al visor aparte, así el cursor se mueve sin esperar a que se codifique un frame.
use crate::input::Rect;
use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub index: usize,
    pub name: String,
    pub adapter: String,
    pub rect: Rect,
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

fn enum_outputs() -> Result<Vec<(IDXGIAdapter1, IDXGIOutput1, OutputInfo)>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut out = Vec::new();
    let mut a = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        a += 1;
        let adapter_name = wide(&unsafe { adapter.GetDesc1()? }.Description);
        let mut o = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
            o += 1;
            let d = unsafe { output.GetDesc()? };
            if !d.AttachedToDesktop.as_bool() {
                continue;
            }
            let r = d.DesktopCoordinates;
            let info = OutputInfo {
                index: out.len(),
                name: wide(&d.DeviceName),
                adapter: adapter_name.clone(),
                rect: Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top },
            };
            out.push((adapter.clone(), output.cast::<IDXGIOutput1>()?, info));
        }
    }
    Ok(out)
}

/// Monitores conectados, en el orden de DXGI (el principal suele ser el 0).
pub fn outputs() -> Result<Vec<OutputInfo>> {
    Ok(enum_outputs()?.into_iter().map(|(_, _, i)| i).collect())
}

/// Dispositivo D3D11 en la GPU a la que está conectado el monitor.
pub struct Gpu {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    pub output: IDXGIOutput1,
    /// Tamaño del escritorio tal y como se ve (ya girado si el monitor está en vertical).
    pub desktop: (u32, u32),
}

impl Gpu {
    pub fn open(index: usize) -> Result<Self> {
        let (adapter, output, info) = enum_outputs()?
            .into_iter()
            .nth(index)
            .ok_or_else(|| anyhow!("no existe el monitor {index}"))?;
        let mut device = None;
        let mut ctx = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .context("D3D11CreateDevice")?;
        }
        let device = device.context("sin dispositivo D3D11")?;
        let ctx = ctx.context("sin contexto D3D11")?;
        // Media Foundation usa el dispositivo desde sus propios hilos.
        let mt: ID3D11Multithread = device.cast()?;
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
        Ok(Self { device, ctx, output, desktop: (info.rect.w as u32, info.rect.h as u32) })
    }
}

pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct Update {
    /// Nueva imagen del escritorio (válida hasta la siguiente llamada a `acquire`).
    pub image: Option<ID3D11Texture2D>,
    /// Esquina superior izquierda del cursor, relativa al monitor, y si es visible.
    pub pointer: Option<(i32, i32, bool)>,
    pub shape: Option<CursorShape>,
}

pub enum Acquire {
    Timeout,
    Frame(Update),
    /// Cambio de modo, escritorio seguro (UAC)…: hay que volver a crear la duplicación.
    Lost,
}

pub struct Duplicator {
    dupl: IDXGIOutputDuplication,
    pub width: u32,
    pub height: u32,
    /// Giro del monitor (DXGI_MODE_ROTATION). La imagen capturada llega SIN girar: con el
    /// monitor en vertical hay que girarla para verla derecha.
    pub rotation: i32,
    holding: bool,
    shape_buf: Vec<u8>,
}

impl Duplicator {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        let dupl = unsafe { gpu.output.DuplicateOutput(&gpu.device) }.context("DuplicateOutput")?;
        let d = unsafe { dupl.GetDesc() };
        Ok(Self {
            dupl,
            width: d.ModeDesc.Width,
            height: d.ModeDesc.Height,
            rotation: d.Rotation.0,
            holding: false,
            shape_buf: Vec::new(),
        })
    }

    /// Espera hasta `timeout_ms` a que cambie la imagen o el puntero.
    pub fn acquire(&mut self, timeout_ms: u32) -> Result<Acquire> {
        self.release();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { self.dupl.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Acquire::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Ok(Acquire::Lost),
            Err(e) => return Err(e).context("AcquireNextFrame"),
        }
        self.holding = true;
        let image = if info.LastPresentTime != 0 {
            resource.map(|r| r.cast::<ID3D11Texture2D>()).transpose()?
        } else {
            None
        };
        let pointer = (info.LastMouseUpdateTime != 0).then(|| {
            let p = info.PointerPosition;
            (p.Position.x, p.Position.y, p.Visible.as_bool())
        });
        let shape = if info.PointerShapeBufferSize > 0 {
            self.read_shape(info.PointerShapeBufferSize).ok()
        } else {
            None
        };
        Ok(Acquire::Frame(Update { image, pointer, shape }))
    }

    pub fn release(&mut self) {
        if self.holding {
            let _ = unsafe { self.dupl.ReleaseFrame() };
            self.holding = false;
        }
    }

    fn read_shape(&mut self, size: u32) -> Result<CursorShape> {
        self.shape_buf.resize(size as usize, 0);
        let mut required = 0u32;
        let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        unsafe {
            self.dupl.GetFramePointerShape(
                size,
                self.shape_buf.as_mut_ptr().cast(),
                &mut required,
                &mut si,
            )?;
        }
        Ok(convert_shape(&si, &self.shape_buf))
    }
}

impl Drop for Duplicator {
    fn drop(&mut self) {
        self.release();
    }
}

/// Convierte la forma del cursor de DXGI a RGBA. Los píxeles que en Windows invierten el
/// fondo (cursor de texto, por ejemplo) se aproximan como blancos con borde negro.
fn convert_shape(si: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, buf: &[u8]) -> CursorShape {
    let w = si.Width as usize;
    let pitch = si.Pitch as usize;
    let kind = si.Type as i32;
    if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
        let h = si.Height as usize / 2;
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let bit = 0x80u8 >> (x % 8);
                let and = buf.get(y * pitch + x / 8).map_or(1, |b| (b & bit != 0) as u8);
                let xor = buf.get((y + h) * pitch + x / 8).map_or(0, |b| (b & bit != 0) as u8);
                let px = match (and, xor) {
                    (1, 0) => [0, 0, 0, 0],         // transparente
                    (0, 0) => [0, 0, 0, 255],       // negro
                    (0, 1) => [255, 255, 255, 255], // blanco
                    _ => [255, 255, 255, 255],      // invertir el fondo: aproximación
                };
                rgba[(y * w + x) * 4..][..4].copy_from_slice(&px);
            }
        }
        return outline(CursorShape { width: w as u32, height: h as u32, rgba });
    }
    let h = si.Height as usize;
    let mut rgba = vec![0u8; w * h * 4];
    let masked = kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0;
    debug_assert!(masked || kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0);
    for y in 0..h {
        for x in 0..w {
            let Some(p) = buf.get(y * pitch + x * 4..y * pitch + x * 4 + 4) else { continue };
            let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
            let px = if masked {
                // alfa 0: el color sustituye al fondo; alfa 0xFF: se combina con XOR.
                if a == 0 { [r, g, b, 255] } else if r | g | b == 0 { [0, 0, 0, 0] } else { [255, 255, 255, 255] }
            } else {
                [r, g, b, a]
            };
            rgba[(y * w + x) * 4..][..4].copy_from_slice(&px);
        }
    }
    CursorShape { width: w as u32, height: h as u32, rgba }
}

/// Añade un borde negro a los píxeles blancos aproximados para que se vean sobre fondo claro.
fn outline(mut s: CursorShape) -> CursorShape {
    let (w, h) = (s.width as usize, s.height as usize);
    let src = s.rgba.clone();
    let opaque = |x: usize, y: usize| src[(y * w + x) * 4 + 3] != 0;
    for y in 0..h {
        for x in 0..w {
            if opaque(x, y) {
                continue;
            }
            let near = (y > 0 && opaque(x, y - 1))
                || (y + 1 < h && opaque(x, y + 1))
                || (x > 0 && opaque(x - 1, y))
                || (x + 1 < w && opaque(x + 1, y));
            if near {
                s.rgba[(y * w + x) * 4..][..4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
    s
}
