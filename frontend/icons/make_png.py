"""Render the terminal mark with only the Python standard library."""
from pathlib import Path
import struct
import zlib


def distance_squared(x, y, ax, ay, bx, by):
    t = max(0, min(1, ((x - ax) * (bx - ax) + (y - ay) * (by - ay))
                   / ((bx - ax) ** 2 + (by - ay) ** 2)))
    return (x - ax - t * (bx - ax)) ** 2 + (y - ay - t * (by - ay)) ** 2


def render(size):
    rows = bytearray()
    segments = [(144, 176, 224, 256), (224, 256, 144, 336), (272, 336, 368, 336)]
    for y in range(size):
        rows.append(0)
        for x in range(size):
            # Four samples per axis smooth the rounded edges at every icon size.
            samples = []
            for sy in range(4):
                for sx in range(4):
                    px, py = (x + (sx + .5) / 4) * 512 / size, (y + (sy + .5) / 4) * 512 / size
                    dx, dy = max(96 - px, 0, px - 416), max(96 - py, 0, py - 416)
                    alpha = 255 if dx * dx + dy * dy <= 96 * 96 else 0
                    accent = any(distance_squared(px, py, *s) <= 16 * 16 for s in segments)
                    samples.append((60, 200, 191, alpha) if accent else (15, 22, 29, alpha))
            rows.extend(round(sum(s[i] for s in samples) / 16) for i in range(4))

    def chunk(kind, data):
        return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data))

    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!2I5B', size, size, 8, 6, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(rows, 9)) + chunk(b'IEND', b''))


if __name__ == '__main__':
    for size, name in [(192, 'icon-192.png'), (512, 'icon-512.png'), (180, 'apple-touch-icon.png')]:
        Path(__file__).with_name(name).write_bytes(render(size))
