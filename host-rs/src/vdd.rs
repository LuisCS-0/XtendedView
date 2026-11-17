//! Monitor virtual con el driver Parsec VDD (firmado por Parsec).
//!
//! El driver se controla por IOCTL: añadir y quitar monitores y un "latido" que hay que
//! enviar al menos cada ~100 ms. Si el latido se para (por ejemplo, si este programa se
//! cierra de golpe) el driver quita los monitores él solo en un segundo, así que nunca queda
//! una pantalla fantasma con ventanas perdidas.
//!
//! Interfaz tomada de github.com/nomi-san/parsec-vdd (core/parsec-vdd.h, licencia MIT).
use anyhow::{bail, Context, Result};
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED,
    FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::System::IO::{DeviceIoControl, GetOverlappedResultEx, OVERLAPPED};

/// Interfaz del adaptador virtual de Parsec: {00b41627-04c4-429e-a26e-0265cf50c8fa}.
const VDD_ADAPTER_GUID: GUID = GUID::from_u128(0x00b41627_04c4_429e_a26e_0265cf50c8fa);
const IOCTL_ADD: u32 = 0x0022_e004;
const IOCTL_REMOVE: u32 = 0x0022_a008;
const IOCTL_UPDATE: u32 = 0x0022_a00c;
const IOCTL_VERSION: u32 = 0x0022_e010;

struct Device(HANDLE);
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open_device() -> Result<Device> {
    unsafe {
        let set = SetupDiGetClassDevsW(Some(&VDD_ADAPTER_GUID), PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)
            .context("SetupDiGetClassDevs")?;
        let result = (|| -> Result<Device> {
            let mut iface = SP_DEVICE_INTERFACE_DATA { cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
            SetupDiEnumDeviceInterfaces(set, None, &VDD_ADAPTER_GUID, 0, &mut iface)
                .context("no está instalado el driver Parsec VDD")?;
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(set, &iface, None, 0, Some(&mut needed), None);
            if needed == 0 {
                bail!("no se pudo obtener la ruta del adaptador virtual");
            }
            let mut buf = vec![0u32; (needed as usize).div_ceil(4)];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            SetupDiGetDeviceInterfaceDetailW(set, &iface, Some(detail), needed, None, None)?;
            let path = PCWSTR((*detail).DevicePath.as_ptr());
            let handle = CreateFileW(
                path,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED | FILE_FLAG_WRITE_THROUGH,
                None,
            )
            .context("no se pudo abrir el adaptador virtual")?;
            Ok(Device(handle))
        })();
        let _ = SetupDiDestroyDeviceInfoList(set);
        result
    }
}

fn ioctl(dev: &Device, code: u32, input: &[u8]) -> Result<u32> {
    unsafe {
        let mut inbuf = [0u8; 32];
        inbuf[..input.len()].copy_from_slice(input);
        let mut out = 0u32;
        let event = CreateEventW(None, true, false, PCWSTR::null())?;
        let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
        let sent = DeviceIoControl(
            dev.0,
            code,
            Some(inbuf.as_ptr().cast()),
            inbuf.len() as u32,
            Some((&mut out as *mut u32).cast()),
            size_of::<u32>() as u32,
            None,
            Some(&mut ov),
        );
        let result = match sent {
            Err(e) if e.code() != ERROR_IO_PENDING.to_hresult() => Err(e),
            _ => {
                let mut n = 0u32;
                GetOverlappedResultEx(dev.0, &ov, &mut n, 5000, false)
            }
        };
        let _ = CloseHandle(event);
        result.with_context(|| format!("IOCTL {code:#x}"))?;
        Ok(out)
    }
}

pub struct Vdd {
    dev: Arc<Device>,
    stop: Arc<AtomicBool>,
    pinger: Option<JoinHandle<()>>,
    pub version: u32,
}

impl Vdd {
    pub fn open() -> Result<Self> {
        let dev = Arc::new(open_device()?);
        let version = ioctl(&dev, IOCTL_VERSION, &[]).unwrap_or(0);
        let stop = Arc::new(AtomicBool::new(false));
        let (d, s) = (dev.clone(), stop.clone());
        let pinger = std::thread::Builder::new()
            .name("vdd-latido".into())
            .spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    let _ = ioctl(&d, IOCTL_UPDATE, &[]);
                    std::thread::sleep(Duration::from_millis(100));
                }
            })?;
        Ok(Self { dev, stop, pinger: Some(pinger), version })
    }

    /// Conecta un monitor virtual nuevo. Devuelve su índice en el driver.
    pub fn add(&self) -> Result<u32> {
        let index = ioctl(&self.dev, IOCTL_ADD, &[])?;
        let _ = ioctl(&self.dev, IOCTL_UPDATE, &[]);
        Ok(index)
    }

    pub fn remove(&self, index: u32) -> Result<()> {
        // El índice va como entero de 16 bits big-endian.
        ioctl(&self.dev, IOCTL_REMOVE, &(index as u16).to_be_bytes())?;
        let _ = ioctl(&self.dev, IOCTL_UPDATE, &[]);
        Ok(())
    }
}

impl Drop for Vdd {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.pinger.take() {
            let _ = t.join();
        }
    }
}
