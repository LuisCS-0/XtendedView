//! Conversión BGRA → NV12 y escalado con el procesador de vídeo de D3D11 (en la GPU).
use crate::capture::Gpu;
use anyhow::{Context, Result};
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VIDEO_ENCODER,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_ROTATION_180, D3D11_VIDEO_PROCESSOR_ROTATION_270, D3D11_VIDEO_PROCESSOR_ROTATION_90,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

pub fn create_texture(device: &ID3D11Device, w: u32, h: u32, format: DXGI_FORMAT, bind: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.context("CreateTexture2D")
}

/// Copia de la imagen capturada (la de DXGI hay que soltarla cuanto antes). La comparten los
/// conversores de todos los dispositivos que muestran ese monitor.
pub fn create_source(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    create_texture(device, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32)
}

pub struct Converter {
    vctx: ID3D11VideoContext,
    vp: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    /// Último NV12 convertido: se reutiliza si hace falta un keyframe con la pantalla quieta.
    pub last: Option<ID3D11Texture2D>,
}

impl Converter {
    /// `rotation`: giro del monitor según DXGI (DXGI_MODE_ROTATION); la imagen se endereza aquí.
    /// `src`: la copia compartida de la imagen capturada (ver `create_source`).
    pub fn new(gpu: &Gpu, src: &ID3D11Texture2D, src_w: u32, src_h: u32, dst_w: u32, dst_h: u32, fps: u32, pool: usize, rotation: i32) -> Result<Self> {
        let vdev: ID3D11VideoDevice = gpu.device.cast()?;
        let vctx: ID3D11VideoContext = gpu.ctx.cast()?;
        let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: src_w,
            InputHeight: src_h,
            OutputFrameRate: rate,
            OutputWidth: dst_w,
            OutputHeight: dst_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let en = unsafe { vdev.CreateVideoProcessorEnumerator(&desc) }.context("CreateVideoProcessorEnumerator")?;
        let vp = unsafe { vdev.CreateVideoProcessor(&en, 0) }.context("CreateVideoProcessor")?;

        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut input_view = None;
        unsafe { vdev.CreateVideoProcessorInputView(src, &en, &in_desc, Some(&mut input_view)) }
            .context("CreateVideoProcessorInputView")?;

        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut outputs = Vec::new();
        for _ in 0..pool {
            // Con BIND_VIDEO_ENCODER el codificador lee la textura sin copiarla; si el driver
            // no lo admite, se crea sin él.
            let tex = create_texture(
                &gpu.device,
                dst_w,
                dst_h,
                DXGI_FORMAT_NV12,
                (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
            )
            .or_else(|_| create_texture(&gpu.device, dst_w, dst_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 as u32))?;
            let mut view = None;
            unsafe { vdev.CreateVideoProcessorOutputView(&tex, &en, &out_desc, Some(&mut view)) }
                .context("CreateVideoProcessorOutputView")?;
            outputs.push((tex, view.context("sin vista de salida")?));
        }

        unsafe {
            // RGB de rango completo → YCbCr BT.709 de rango limitado (lo que espera el decodificador).
            if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            // DXGI: 2 = 90°, 3 = 180°, 4 = 270°. La imagen capturada está en la orientación del
            // panel; aplicando ese mismo giro queda como la ve el usuario (comprobado: con el
            // giro contrario sale del revés).
            let undo = match rotation {
                2 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_90),
                3 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_180),
                4 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_270),
                _ => None,
            };
            if let Some(r) = undo {
                vctx.VideoProcessorSetStreamRotation(&vp, 0, true, r);
            }
        }
        Ok(Self {
            vctx,
            vp,
            input_view: input_view.context("sin vista de entrada")?,
            outputs,
            next: 0,
            last: None,
        })
    }

    /// Convierte `src` en la siguiente textura NV12 del conjunto y la devuelve.
    pub fn convert(&mut self) -> Result<ID3D11Texture2D> {
        let (tex, view) = &self.outputs[self.next];
        self.next = (self.next + 1) % self.outputs.len();
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ..Default::default()
        }];
        let result = unsafe { self.vctx.VideoProcessorBlt(&self.vp, view, 0, &streams) };
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result.context("VideoProcessorBlt")?;
        self.last = Some(tex.clone());
        Ok(tex.clone())
    }
}
