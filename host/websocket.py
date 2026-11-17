"""WebSocket mínimo (RFC 6455) sobre asyncio streams, sin dependencias externas."""
import asyncio
import base64
import hashlib
import struct

GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

OP_CONT, OP_TEXT, OP_BIN, OP_CLOSE, OP_PING, OP_PONG = 0x0, 0x1, 0x2, 0x8, 0x9, 0xA


class ConnectionClosed(Exception):
    pass


def accept_key(key: str) -> str:
    return base64.b64encode(hashlib.sha1(key.encode() + GUID).digest()).decode()


def _unmask(data: bytes, mask: bytes) -> bytes:
    n = len(data)
    if n == 0:
        return data
    key = (mask * (n // 4 + 1))[:n]
    return (int.from_bytes(data, "big") ^ int.from_bytes(key, "big")).to_bytes(n, "big")


class WebSocket:
    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter,
                 max_message: int = 1 << 20):
        self.reader = reader
        self.writer = writer
        self.max_message = max_message
        self.closed = False

    @staticmethod
    def _header(opcode: int, length: int) -> bytes:
        if length < 126:
            return struct.pack("!BB", 0x80 | opcode, length)
        if length < 1 << 16:
            return struct.pack("!BBH", 0x80 | opcode, 126, length)
        return struct.pack("!BBQ", 0x80 | opcode, 127, length)

    def _send(self, opcode: int, *parts: bytes) -> None:
        if self.closed or self.writer.is_closing():
            raise ConnectionClosed()
        self.writer.write(self._header(opcode, sum(len(p) for p in parts)))
        for p in parts:
            self.writer.write(p)

    def send_text(self, text: str) -> None:
        self._send(OP_TEXT, text.encode())

    def send_binary(self, *parts: bytes) -> None:
        self._send(OP_BIN, *parts)

    def buffered(self) -> int:
        return self.writer.transport.get_write_buffer_size()

    async def recv(self) -> tuple[int, bytes]:
        """Devuelve (opcode, payload) del siguiente mensaje de datos completo."""
        message = bytearray()
        msg_op = None
        while True:
            h = await self.reader.readexactly(2)
            fin, op = h[0] & 0x80, h[0] & 0x0F
            masked, n = h[1] & 0x80, h[1] & 0x7F
            if n == 126:
                n = struct.unpack("!H", await self.reader.readexactly(2))[0]
            elif n == 127:
                n = struct.unpack("!Q", await self.reader.readexactly(8))[0]
            if n > self.max_message:
                raise ConnectionClosed("mensaje demasiado grande")
            mask = await self.reader.readexactly(4) if masked else None
            data = await self.reader.readexactly(n)
            if mask:
                data = _unmask(data, mask)
            if op == OP_PING:
                self._send(OP_PONG, data)
                continue
            if op == OP_PONG:
                continue
            if op == OP_CLOSE:
                self.close()
                raise ConnectionClosed()
            if op in (OP_TEXT, OP_BIN):
                msg_op, message = op, bytearray(data)
            elif op == OP_CONT:
                message += data
            if fin and msg_op is not None:
                return msg_op, bytes(message)

    def close(self, code: int = 1000) -> None:
        if self.closed:
            return
        try:
            self._send(OP_CLOSE, struct.pack("!H", code))
        except Exception:
            pass
        self.closed = True
        self.writer.close()
