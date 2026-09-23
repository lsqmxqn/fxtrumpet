"""Inspect the Windows resources baked into an .exe, and dump its icon.

Written as a throwaway verification tool for the M6 packaging work: it proves
that `rc.exe` actually landed an icon group and a version block in the binary,
and lets a human look at the artwork instead of trusting a green build.
"""

import struct
import sys
import zlib

RT_NAMES = {3: "RT_ICON", 14: "RT_GROUP_ICON", 16: "RT_VERSION", 24: "RT_MANIFEST"}


class Pe:
    def __init__(self, path):
        self.data = open(path, "rb").read()
        data = self.data
        pe = struct.unpack_from("<I", data, 0x3C)[0]
        assert data[pe:pe + 4] == b"PE\0\0", "not a PE file"
        coff = pe + 4
        self.sections_count = struct.unpack_from("<H", data, coff + 2)[0]
        opt_size = struct.unpack_from("<H", data, coff + 16)[0]
        opt = coff + 20
        magic = struct.unpack_from("<H", data, opt)[0]
        dd = opt + (96 if magic == 0x10B else 112)
        res_rva, res_size = struct.unpack_from("<II", data, dd + 2 * 8)

        self.sections = []
        for index in range(self.sections_count):
            at = opt + opt_size + index * 40
            # VirtualSize comes first, then VirtualAddress.
            vsize, va = struct.unpack_from("<II", data, at + 8)
            raw_size, raw_ptr = struct.unpack_from("<II", data, at + 16)
            self.sections.append((va, vsize, raw_ptr, raw_size))

        self.res_offset = self.rva_to_offset(res_rva)
        self.res_size = res_size
        self.header = (magic, res_rva, res_size)

    def rva_to_offset(self, rva):
        for va, vsize, ptr, raw_size in self.sections:
            if va <= rva < va + max(vsize, raw_size):
                return rva - va + ptr
        raise ValueError(f"rva {rva:#x} is not in any section")

    def walk(self):
        """Yields (type, name, language, data_rva, size) for every resource."""
        base = self.res_offset

        def entries(offset):
            named, ids = struct.unpack_from("<HH", self.data, offset + 12)
            print(f"  [dir @ {offset:#x}] named={named} ids={ids}")
            out = []
            for index in range(named + ids):
                at = offset + 16 + index * 8
                name, child = struct.unpack_from("<II", self.data, at)
                out.append((name, child))
            return out

        for type_id, type_child in entries(base):
            if not type_child & 0x80000000:
                continue
            for name_id, name_child in entries(base + (type_child & 0x7FFFFFFF)):
                if not name_child & 0x80000000:
                    continue
                for lang_id, lang_child in entries(base + (name_child & 0x7FFFFFFF)):
                    if lang_child & 0x80000000:
                        continue
                    at = base + lang_child
                    rva, size = struct.unpack_from("<II", self.data, at)
                    yield type_id, name_id, lang_id, rva, size


def bmp_to_rgba(bmp):
    """Expands an .ico entry's 32 bpp BGRA bitmap into top-down RGBA."""
    width, doubled = struct.unpack_from("<ii", bmp, 4)
    height = doubled // 2
    pixels = 40
    rgba = bytearray(width * height * 4)
    for row in range(height):
        src = pixels + (height - 1 - row) * width * 4
        for column in range(width):
            b, g, r, a = bmp[src + column * 4:src + column * 4 + 4]
            at = (row * width + column) * 4
            rgba[at:at + 4] = bytes((r, g, b, a))
    return width, height, bytes(rgba)


def write_png(path, width, height, rgba):
    def chunk(tag, payload):
        body = tag + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))

    raw = b"".join(
        b"\x00" + rgba[row * width * 4:(row + 1) * width * 4] for row in range(height)
    )
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 9))
    png += chunk(b"IEND", b"")
    open(path, "wb").write(png)


def main():
    exe, png_out = sys.argv[1], sys.argv[2]
    pe = Pe(exe)

    icons, groups, others = {}, {}, []
    for type_id, name_id, lang_id, rva, size in pe.walk():
        label = RT_NAMES.get(type_id, f"type {type_id}")
        if type_id == 3:
            icons[name_id] = pe.data[pe.rva_to_offset(rva):pe.rva_to_offset(rva) + size]
        elif type_id == 14:
            groups[name_id] = pe.data[pe.rva_to_offset(rva):pe.rva_to_offset(rva) + size]
        else:
            others.append((label, name_id, lang_id, size))

    print(f"file            : {exe}")
    print(f"icon images     : {sorted(icons)}")
    print(f"icon groups     : {sorted(groups)}")
    for label, name, lang, size in sorted(others):
        print(f"other resource  : {label} id={name} lang={lang} {size} bytes")

    if not groups:
        print("RESULT: no icon group -- the executable has no icon")
        return 1

    group = groups[min(groups)]
    count = struct.unpack_from("<H", group, 4)[0]
    print(f"group entries   : {count}")
    best = None
    for index in range(count):
        at = 6 + index * 14
        width, height, colours, reserved, planes, bpp, size, icon_id = struct.unpack_from(
            "<BBBBHHIH", group, at
        )
        edge = width or 256
        print(f"  {edge:>3}px id={icon_id} bpp={bpp} bytes={size}")
        if best is None or edge > best[0]:
            best = (edge, icon_id)

    w, h, rgba = bmp_to_rgba(icons[best[1]])
    write_png(png_out, w, h, rgba)
    opaque = sum(1 for i in range(0, len(rgba), 4) if rgba[i + 3] > 128)
    print(f"largest entry   : {w}x{h}, {opaque} opaque pixels -> {png_out}")
    print(f"centre pixel    : {rgba[(h // 2 * w + w // 2) * 4:][:4].hex()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
