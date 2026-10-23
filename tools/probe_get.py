"""Read-only probe: UAC1 GET requests against the Fast Track Ultra. Sends no SET requests."""
import usb.core, usb.backend.libusb1 as b

be = b.get_backend(find_library=lambda x: "/opt/homebrew/lib/libusb-1.0.dylib")
d = usb.core.find(idVendor=0x0763, idProduct=0x2080, backend=be)
GET = {"CUR": 0x81, "MIN": 0x82, "MAX": 0x83, "RES": 0x84}

def get(req, bm, wv, wi, n):
    try:
        return bytes(d.ctrl_transfer(bm, req, wv, wi, n, timeout=500))
    except usb.core.USBError as e:
        return f"ERR {e.errno}"

def s16(x):
    return int.from_bytes(x[:2], "little", signed=True) if isinstance(x, bytes) and len(x) >= 2 else x

def line(label, req, bm, wv, wi, n, signed=True):
    out = []
    for k in (["CUR", "MIN", "MAX", "RES"] if req is None else [req]):
        r = get(GET[k], bm, wv, wi, n)
        v = s16(r) if signed and n == 2 else (r.hex() if isinstance(r, bytes) else r)
        out.append(f"{k}={v}")
    print(f"{label:34s} " + " ".join(out))

print("== status/meters GET_MEM unit 5 ==")
for i in range(3):
    r = get(0x85, 0xA1, 0x0100, 0x0504, 54)
    if isinstance(r, bytes):
        w = [int.from_bytes(r[j:j+2], "little", signed=True) for j in range(0, len(r), 2)]
        print("meters", w[:26], "status", hex(w[26] & 0xFFFF) if len(w) > 26 else "-")
    else:
        print(r)

print("== sample rate (endpoint GET_CUR) ==")
for ep in (0x01, 0x81):
    r = get(0x81, 0xA2, 0x0100, ep, 3)
    print(hex(ep), int.from_bytes(r, "little") if isinstance(r, bytes) else r)

print("== mixer unit 5 (dB*256) ==")
for out in (1, 2, 3, 8):
    for ch in (1, 2, 9, 16):
        line(f"u5 out{out} src{ch}", None, 0xA1, (out << 8) | ch, 0x0500, 2)
for ch in (1, 2, 9):
    line(f"u5 fxsend ctl9 src{ch}", None, 0xA1, (9 << 8) | ch, 0x0500, 2)

print("== effect unit 6 ==")
line("program 0x0100 (1B)", "CUR", 0xA1, 0x0100, 0x0600, 1)
line("program 0x0100 (2B)", "CUR", 0xA1, 0x0100, 0x0600, 2, signed=False)
line("volume 0x0200", "CUR", 0xA1, 0x0200, 0x0600, 1)
line("duration 0x0300", "CUR", 0xA1, 0x0300, 0x0600, 2, signed=False)
line("feedback 0x0400", "CUR", 0xA1, 0x0400, 0x0600, 1)

print("== feature units 7/8/9 ==")
for unit in (7, 8, 9):
    for ctl in (1, 2, 7):
        for ch in (0, 1, 2):
            line(f"u{unit} ctl{ctl} ch{ch}", None, 0xA1, (ctl << 8) | ch, unit << 8, 2)
