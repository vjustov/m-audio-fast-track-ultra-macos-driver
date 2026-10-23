"""Minimal Fast Track Ultra control helper (libusb via pyusb)."""
import time
import usb.core, usb.backend.libusb1 as b

be = b.get_backend(find_library=lambda x: "/opt/homebrew/lib/libusb-1.0.dylib")
d = usb.core.find(idVendor=0x0763, idProduct=0x2080, backend=be)
OFF = -32768


def set_cur(wvalue, windex, data):
    return d.ctrl_transfer(0x21, 0x01, wvalue, windex, bytes(data), timeout=500)


def get_cur(wvalue, windex, n):
    return bytes(d.ctrl_transfer(0xA1, 0x81, wvalue, windex, n, timeout=500))


def s16(v):
    return (v & 0xFFFF).to_bytes(2, "little")


def mixer(out, src, db256):
    """Unit 5: control = output 1..8, channel = source 1..16. Value dB*256, OFF = -32768."""
    set_cur((out << 8) | src, 0x0500, s16(db256))


def meters(settle=0.0, avg=3):
    time.sleep(settle)
    acc = None
    for _ in range(avg):
        r = bytes(d.ctrl_transfer(0xA1, 0x85, 0x0100, 0x0504, 54, timeout=500))
        w = [abs(int.from_bytes(r[j:j + 2], "little", signed=True)) for j in range(0, 54, 2)]
        acc = w if acc is None else [max(a, x) for a, x in zip(acc, w)]
        time.sleep(0.15)
    return {"in": acc[0:8], "daw": acc[8:16], "out": acc[16:24], "fx": acc[24:26], "status": hex(acc[26])}
