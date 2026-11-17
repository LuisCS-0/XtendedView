"""Captura + codificación con FFmpeg y lectura de la salida FLV.

FFmpeg captura el escritorio con Desktop Duplication (ddagrab), codifica H.264
(QuickSync o x264) y escribe FLV por stdout. FLV se usa solo como "sobre": cada
frame llega con su tamaño en la cabecera, así que se puede reenviar en cuanto
llega, sin esperar al siguiente (que es lo que pasaría con H.264 en bruto).
"""
import asyncio
import os
import shutil
import struct
from dataclasses import dataclass
from pathlib import Path

DEFAULT_FFMPEG = r"C:\Software\ffmpeg-6.0-essentials_build\bin\ffmpeg.exe"


@dataclass
class EncoderSettings:
    monitor: int = 0          # índice de salida DXGI (0 = pantalla principal)
    width: int = 1920
    height: int = 1080
    fps: int = 60
    bitrate_mbps: float = 20.0
    gop_seconds: float = 1.0  # cada cuánto hay un keyframe (para recuperarse de pérdidas)
    encoder: str = "qsv"      # qsv | x264
    pipeline: str = "cpu"     # cpu (escalado con swscale) | gpu (escalado con QuickSync)
    draw_mouse: bool = True


def find_ffmpeg(explicit: str | None = None) -> str:
    for c in (explicit, os.environ.get("FFMPEG"), shutil.which("ffmpeg"), DEFAULT_FFMPEG):
        if c and Path(c).is_file():
            return c
    raise FileNotFoundError("No encuentro ffmpeg. Usa --ffmpeg o la variable FFMPEG.")


def build_command(ffmpeg: str, s: EncoderSettings) -> list[str]:
    w, h = s.width, s.height
    gop = max(1, round(s.fps * s.gop_seconds))
    # VBR con tope: con maxrate == bitrate QuickSync hace CBR y rellena cada frame hasta el
    # máximo aunque la pantalla no cambie.
    br = f"{s.bitrate_mbps:g}M"
    maxrate = f"{s.bitrate_mbps * 1.5:g}M"
    # Búfer de vídeo de ~100 ms: limita el tamaño de los frames grandes (picos de latencia).
    vbv = f"{s.bitrate_mbps * 0.15:g}M"
    grab = f"ddagrab=output_idx={s.monitor}:framerate={s.fps}:draw_mouse={int(s.draw_mouse)}"

    cmd = [ffmpeg, "-hide_banner", "-loglevel", "warning", "-nostats"]
    if s.encoder == "qsv":
        if s.pipeline == "gpu":
            cmd += ["-init_hw_device", "d3d11va=d3d", "-filter_hw_device", "d3d"]
            graph = (f"{grab},hwdownload,format=bgra,"
                     f"hwupload=derive_device=qsv:extra_hw_frames=16,"
                     f"vpp_qsv=w={w}:h={h}:format=nv12")
        else:
            graph = f"{grab},hwdownload,format=bgra,scale={w}:{h}:flags=bilinear,format=nv12"
        enc = ["-c:v", "h264_qsv", "-preset", "veryfast", "-scenario", "displayremoting",
               "-async_depth", "1", "-bf", "0", "-g", str(gop), "-forced_idr", "1",
               "-look_ahead", "0", "-low_delay_brc", "1",
               "-b:v", br, "-maxrate", maxrate, "-bufsize", vbv]
    elif s.encoder == "x264":
        graph = f"{grab},hwdownload,format=bgra,scale={w}:{h}:flags=bilinear,format=yuv420p"
        enc = ["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
               "-bf", "0", "-g", str(gop), "-b:v", br, "-maxrate", maxrate, "-bufsize", vbv]
    else:
        raise ValueError(f"codificador desconocido: {s.encoder}")

    cmd += ["-filter_complex", graph, *enc,
            # Marcas de tiempo en ms (relativas al inicio) en vez de redondeadas a 1/fps.
            # vfr: si la pantalla no cambia no se envían frames repetidos.
            "-enc_time_base", "1:1000", "-fps_mode", "vfr",
            "-f", "flv", "-flvflags", "no_duration_filesize", "-flush_packets", "1", "pipe:1"]
    return cmd


@dataclass
class Packet:
    kind: str        # "config" | "frame"
    pts_ms: int
    key: bool
    data: bytes


def avc_codec_string(avcc: bytes) -> str:
    return f"avc1.{avcc[1]:02x}{avcc[2]:02x}{avcc[3]:02x}"


async def read_flv(stream: asyncio.StreamReader):
    """Genera Packet a partir de un flujo FLV con vídeo H.264."""
    head = await stream.readexactly(9)
    if head[:3] != b"FLV":
        raise ValueError("la salida de ffmpeg no es FLV")
    offset = struct.unpack(">I", head[5:9])[0]
    if offset > 9:
        await stream.readexactly(offset - 9)
    await stream.readexactly(4)  # PreviousTagSize0
    while True:
        th = await stream.readexactly(11)
        tag_type = th[0] & 0x1F
        size = int.from_bytes(th[1:4], "big")
        ts = int.from_bytes(th[4:7], "big") | (th[7] << 24)
        data = await stream.readexactly(size)
        await stream.readexactly(4)  # PreviousTagSize
        if tag_type != 9 or len(data) < 5 or (data[0] & 0x0F) != 7:
            continue  # solo nos interesa vídeo AVC
        key = (data[0] >> 4) == 1
        avc_type = data[1]
        if avc_type == 0:
            yield Packet("config", ts, True, data[5:])
        elif avc_type == 1:
            yield Packet("frame", ts, key, data[5:])
