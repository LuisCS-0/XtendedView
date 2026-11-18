//! Codificador H.264 por hardware (QuickSync) con Media Foundation, en modo asíncrono.
//!
//! Recibe texturas NV12 de la GPU (sin copias a memoria del sistema) y devuelve H.264 en
//! formato Annex B. Está configurado para baja latencia: sin B-frames, VBR de bajo retardo y
//! keyframes bajo demanda en lugar de periódicos.
use anyhow::{bail, Context, Result};
use std::mem::ManuallyDrop;
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

pub enum EncEvent {
    NeedInput,
    HaveOutput,
    Other,
}

pub struct Output {
    pub data: Vec<u8>,
    pub key: bool,
    pub time: i64,
}

pub struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: ICodecAPI,
    _manager: IMFDXGIDeviceManager,
    provides_samples: bool,
    out_size: u32,
    pub name: String,
}

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

fn friendly_name(a: &IMFActivate) -> Option<String> {
    unsafe {
        let mut p = PWSTR::null();
        let mut len = 0u32;
        a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

impl Encoder {
    pub fn new(device: &ID3D11Device, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        unsafe {
            // 1. Buscar el codificador H.264 por hardware.
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut list,
                &mut count,
            )
            .context("MFTEnumEx")?;
            if count == 0 || list.is_null() {
                bail!("no hay ningún codificador H.264 por hardware");
            }
            let activates = std::slice::from_raw_parts_mut(list, count as usize);
            // Con una sola GPU vale el primero; con varias habría que comparar MFT_ENUM_ADAPTER_LUID.
            let activate = activates[0].clone().context("activador vacío")?;
            for a in activates.iter_mut() {
                a.take();
            }
            CoTaskMemFree(Some(list as *const _));
            let name = friendly_name(&activate).unwrap_or_else(|| "codificador H.264 por hardware".into());
            let transform: IMFTransform = activate.ActivateObject().context("ActivateObject")?;

            // 2. Modo asíncrono y dispositivo D3D11 compartido (las texturas no salen de la GPU).
            let attrs = transform.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.context("sin IMFDXGIDeviceManager")?;
            manager.ResetDevice(device, token)?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("MFT_MESSAGE_SET_D3D_MANAGER")?;

            // 3. Parámetros de baja latencia.
            let codec: ICodecAPI = transform.cast()?;
            let set = |api: &GUID, v: VARIANT| {
                let supported = codec.IsSupported(api).is_ok();
                let r = codec.SetValue(api, &v);
                log::debug!("ICodecAPI {api:?}: admitido={supported} resultado={r:?}");
                r
            };
            let _ = set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            // VBR de bajo retardo (4); si no lo admite, VBR con tope (1).
            if set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(4u32)).is_err() {
                let _ = set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(1u32));
            }
            let _ = set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
            let _ = set(&CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(bitrate / 2 * 3));
            let _ = set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
            // Keyframes bajo demanda: el periódico solo como red de seguridad (cada 10 s).
            let _ = set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps * 10));
            // Medido (2026-11-18): 0 (velocidad) frente a 50 apenas cambia la latencia en este
            // portátil (10,3 frente a 10,7 ms), así que se deja el equilibrio por defecto.
            let _ = set(&CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(50u32));

            // 4. Tipos de salida (H.264) y de entrada (NV12). La salida va primero.
            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, 100)?; // High
            let _ = out_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
            let _ = out_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
            let _ = out_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
            let _ = out_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
            transform.SetOutputType(0, &out_type, 0).context("SetOutputType")?;

            let mut i = 0;
            let in_type = loop {
                let t = transform.GetInputAvailableType(0, i).context("el codificador no acepta NV12")?;
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    break t;
                }
                i += 1;
            };
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            transform.SetInputType(0, &in_type, 0).context("SetInputType")?;

            let info = transform.GetOutputStreamInfo(0)?;
            let provides_samples = info.dwFlags & (0x100 | 0x200) != 0; // PROVIDES / CAN_PROVIDE_SAMPLES

            // 5. Arrancar.
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events: IMFMediaEventGenerator = transform.cast()?;
            Ok(Self {
                transform,
                events,
                codec,
                _manager: manager,
                provides_samples,
                out_size: info.cbSize,
                name,
            })
        }
    }

    /// Siguiente evento si ya hay uno pendiente, sin esperar (la captura atiende a varios
    /// codificadores a la vez, así que no puede quedarse bloqueada en uno).
    pub fn try_event(&self) -> Result<Option<EncEvent>> {
        unsafe {
            let flags = MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(1); // MF_EVENT_FLAG_NO_WAIT
            let ev = match self.events.GetEvent(flags) {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(None),
                Err(e) => return Err(e).context("GetEvent"),
            };
            let t = ev.GetType()?;
            Ok(Some(if t == METransformNeedInput.0 as u32 {
                EncEvent::NeedInput
            } else if t == METransformHaveOutput.0 as u32 {
                EncEvent::HaveOutput
            } else {
                EncEvent::Other
            }))
        }
    }

    pub fn force_keyframe(&self) {
        unsafe {
            let _ = self.codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
        }
    }

    pub fn set_bitrate(&self, bitrate: u32) {
        unsafe {
            let _ = self.codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate));
            let _ = self.codec.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &VARIANT::from(bitrate / 2 * 3));
        }
    }

    /// Entrega una textura NV12. `time` y `duration` en unidades de 100 ns.
    pub fn input(&self, tex: &ID3D11Texture2D, time: i64, duration: i64) -> Result<()> {
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                let len = b2.GetContiguousLength()?;
                buffer.SetCurrentLength(len)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            self.transform.ProcessInput(0, &sample, 0).context("ProcessInput")
        }
    }

    pub fn output(&self) -> Result<Option<Output>> {
        unsafe {
            let own = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size.max(4 << 20))?)?;
                Some(s)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let sample = ManuallyDrop::take(&mut buffers[0].pSample);
            drop(ManuallyDrop::take(&mut buffers[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let t = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &t, 0)?;
                    return Ok(None);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) => return Err(e).context("ProcessOutput"),
            }
            let Some(sample) = sample else { return Ok(None) };
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buf.Unlock()?;
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let time = sample.GetSampleTime().unwrap_or(0);
            Ok(Some(Output { data, key, time }))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
        }
    }
}
