"""Llamadas a la API de Windows con ctypes: DPI, monitores e inyección de entrada.

- El táctil del cliente se inyecta como táctil real de Windows (InjectTouchInput),
  así funcionan el desplazamiento con el dedo, pellizcar, etc.
- El ratón y el lápiz del cliente se inyectan como ratón (SendInput). La presión del
  lápiz llegará en una fase posterior.
"""
import ctypes
import time
from ctypes import wintypes
from dataclasses import dataclass

user32 = ctypes.WinDLL("user32", use_last_error=True)


def enable_dpi_awareness() -> None:
    """Trabajar en píxeles físicos (si no, Windows escala las coordenadas)."""
    try:
        user32.SetProcessDpiAwarenessContext.argtypes = [ctypes.c_void_p]
        if user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4)):  # PER_MONITOR_AWARE_V2
            return
    except AttributeError:
        pass
    try:
        ctypes.windll.shcore.SetProcessDpiAwareness(2)
    except Exception:
        user32.SetProcessDPIAware()


# ---------------------------------------------------------------- monitores

@dataclass
class Monitor:
    name: str
    x: int
    y: int
    w: int
    h: int
    primary: bool


class MONITORINFOEXW(ctypes.Structure):
    _fields_ = [("cbSize", wintypes.DWORD), ("rcMonitor", wintypes.RECT),
                ("rcWork", wintypes.RECT), ("dwFlags", wintypes.DWORD),
                ("szDevice", wintypes.WCHAR * 32)]


MonitorEnumProc = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HMONITOR, wintypes.HDC,
                                     ctypes.POINTER(wintypes.RECT), wintypes.LPARAM)
user32.EnumDisplayMonitors.argtypes = [wintypes.HDC, ctypes.c_void_p, MonitorEnumProc,
                                       wintypes.LPARAM]
user32.GetMonitorInfoW.argtypes = [wintypes.HMONITOR, ctypes.c_void_p]


def list_monitors() -> list[Monitor]:
    """Monitores en píxeles físicos, el principal primero (suele coincidir con DXGI)."""
    found: list[Monitor] = []

    def cb(hmon, _hdc, _rect, _lp):
        info = MONITORINFOEXW()
        info.cbSize = ctypes.sizeof(info)
        if user32.GetMonitorInfoW(hmon, ctypes.byref(info)):
            r = info.rcMonitor
            found.append(Monitor(info.szDevice, r.left, r.top, r.right - r.left,
                                 r.bottom - r.top, bool(info.dwFlags & 1)))
        return True

    user32.EnumDisplayMonitors(None, None, MonitorEnumProc(cb), 0)
    found.sort(key=lambda m: (not m.primary, m.x, m.y))
    return found


# ---------------------------------------------------------------- ratón (SendInput)

class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG), ("mouseData", wintypes.DWORD),
                ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD),
                ("dwExtraInfo", ctypes.c_size_t)]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD), ("dwFlags", wintypes.DWORD),
                ("time", wintypes.DWORD), ("dwExtraInfo", ctypes.c_size_t)]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("u", _INPUTUNION)]


user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]

MOUSEEVENTF_MOVE, MOUSEEVENTF_WHEEL, MOUSEEVENTF_HWHEEL = 0x0001, 0x0800, 0x1000
MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_ABSOLUTE = 0x4000, 0x8000
BUTTON_FLAGS = {0: (0x0002, 0x0004), 1: (0x0020, 0x0040), 2: (0x0008, 0x0010)}  # izq, central, der


def _send_mouse(dx=0, dy=0, flags=0, data=0) -> None:
    inp = INPUT(type=0)
    inp.u.mi = MOUSEINPUT(dx, dy, ctypes.c_ulong(data & 0xFFFFFFFF).value, flags, 0, 0)
    user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))


# ---------------------------------------------------------------- táctil (InjectTouchInput)

class POINTER_INFO(ctypes.Structure):
    _fields_ = [("pointerType", wintypes.DWORD), ("pointerId", ctypes.c_uint32),
                ("frameId", ctypes.c_uint32), ("pointerFlags", ctypes.c_uint32),
                ("sourceDevice", wintypes.HANDLE), ("hwndTarget", wintypes.HWND),
                ("ptPixelLocation", wintypes.POINT), ("ptHimetricLocation", wintypes.POINT),
                ("ptPixelLocationRaw", wintypes.POINT), ("ptHimetricLocationRaw", wintypes.POINT),
                ("dwTime", wintypes.DWORD), ("historyCount", ctypes.c_uint32),
                ("InputData", ctypes.c_int32), ("dwKeyStates", wintypes.DWORD),
                ("PerformanceCount", ctypes.c_uint64), ("ButtonChangeType", ctypes.c_int)]


class POINTER_TOUCH_INFO(ctypes.Structure):
    _fields_ = [("pointerInfo", POINTER_INFO), ("touchFlags", ctypes.c_uint32),
                ("touchMask", ctypes.c_uint32), ("rcContact", wintypes.RECT),
                ("rcContactRaw", wintypes.RECT), ("orientation", ctypes.c_uint32),
                ("pressure", ctypes.c_uint32)]


assert ctypes.sizeof(INPUT) == 40 and ctypes.sizeof(POINTER_TOUCH_INFO) == 144

PT_TOUCH = 2
PF_INRANGE, PF_INCONTACT = 0x2, 0x4
PF_DOWN, PF_UPDATE, PF_UP, PF_CANCELED = 0x10000, 0x20000, 0x40000, 0x8000
TOUCH_MASK_ALL = 0x1 | 0x2 | 0x4  # área de contacto, orientación, presión
MAX_CONTACTS = 10

user32.InitializeTouchInjection.argtypes = [ctypes.c_uint32, wintypes.DWORD]
user32.InjectTouchInput.argtypes = [ctypes.c_uint32, ctypes.POINTER(POINTER_TOUCH_INFO)]


class InputInjector:
    def __init__(self) -> None:
        self.monitor: Monitor | None = None
        self.touch_ok = bool(user32.InitializeTouchInjection(MAX_CONTACTS, 1))  # 1 = feedback visual
        self.contacts: dict[int, tuple[int, int]] = {}  # slot -> (x, y)
        self.slots: dict[str, int] = {}                 # id del navegador -> slot
        self.last_touch = 0.0
        self.mouse_buttons: set[int] = set()

    # Coordenadas normalizadas (0..1 sobre la imagen) -> píxeles físicos del monitor.
    def _to_screen(self, nx: float, ny: float) -> tuple[int, int]:
        m = self.monitor
        nx, ny = min(max(nx, 0.0), 1.0), min(max(ny, 0.0), 1.0)
        return m.x + round(nx * (m.w - 1)), m.y + round(ny * (m.h - 1))

    def pointer(self, ev: dict) -> None:
        if not self.monitor:
            return
        kind = ev.get("e")
        x, y = self._to_screen(float(ev.get("x", 0)), float(ev.get("y", 0)))
        if ev.get("type") == "touch" and self.touch_ok:
            self._touch(str(ev.get("id")), kind, x, y)
        else:
            self._mouse(kind, x, y, int(ev.get("b", 0)))

    def wheel(self, dx: float, dy: float) -> None:
        if dy:
            _send_mouse(flags=MOUSEEVENTF_WHEEL, data=int(-dy))
        if dx:
            _send_mouse(flags=MOUSEEVENTF_HWHEEL, data=int(dx))

    def _mouse(self, kind: str, x: int, y: int, button: int) -> None:
        vx, vy = user32.GetSystemMetrics(76), user32.GetSystemMetrics(77)
        vw, vh = user32.GetSystemMetrics(78), user32.GetSystemMetrics(79)
        ax = round((x - vx) * 65535 / max(vw - 1, 1))
        ay = round((y - vy) * 65535 / max(vh - 1, 1))
        flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
        down, up = BUTTON_FLAGS.get(button, BUTTON_FLAGS[0])
        if kind == "down":
            flags |= down
            self.mouse_buttons.add(button)
        elif kind in ("up", "cancel") and button in self.mouse_buttons:
            flags |= up
            self.mouse_buttons.discard(button)
        _send_mouse(ax, ay, flags)

    def _touch(self, pid: str, kind: str, x: int, y: int) -> None:
        if kind == "down":
            if pid in self.slots or len(self.slots) >= MAX_CONTACTS:
                return
            slot = next(i for i in range(MAX_CONTACTS) if i not in self.contacts)
            self.slots[pid] = slot
        slot = self.slots.get(pid)
        if slot is None:
            return
        self.contacts[slot] = (x, y)
        flags = {"down": PF_DOWN | PF_INRANGE | PF_INCONTACT,
                 "move": PF_UPDATE | PF_INRANGE | PF_INCONTACT,
                 "up": PF_UP,
                 "cancel": PF_UP | PF_CANCELED}.get(kind)
        if flags is None:
            return
        self._inject({slot: flags})
        if kind in ("up", "cancel"):
            del self.contacts[slot]
            del self.slots[pid]

    def keepalive(self) -> None:
        """Windows cancela los contactos que no se actualizan; se reenvían si están quietos."""
        if self.contacts and time.monotonic() - self.last_touch > 0.1:
            self._inject({})

    def release_all(self) -> None:
        if self.contacts:
            self._inject({slot: PF_UP | PF_CANCELED for slot in self.contacts})
            self.contacts.clear()
            self.slots.clear()
        for b in list(self.mouse_buttons):
            _send_mouse(flags=BUTTON_FLAGS.get(b, BUTTON_FLAGS[0])[1])
        self.mouse_buttons.clear()

    def _inject(self, changed: dict[int, int]) -> None:
        # Cada inyección debe incluir todos los contactos activos.
        arr = (POINTER_TOUCH_INFO * len(self.contacts))()
        for i, (slot, (x, y)) in enumerate(self.contacts.items()):
            t = arr[i]
            t.pointerInfo.pointerType = PT_TOUCH
            t.pointerInfo.pointerId = slot
            t.pointerInfo.ptPixelLocation = wintypes.POINT(x, y)
            t.pointerInfo.pointerFlags = changed.get(slot, PF_UPDATE | PF_INRANGE | PF_INCONTACT)
            t.touchMask = TOUCH_MASK_ALL
            t.rcContact = wintypes.RECT(x - 2, y - 2, x + 2, y + 2)
            t.orientation = 90
            t.pressure = 32000
        if len(arr):
            user32.InjectTouchInput(len(arr), arr)
        self.last_touch = time.monotonic()
