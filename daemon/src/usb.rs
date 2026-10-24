//! Device access: open, claim, alternate settings, and UAC1 control requests.
//! Protocol details: docs/protocol.md.

use libusb1_sys as ffi;
use std::fmt;
use std::os::raw::c_int;

pub const VID: u16 = 0x0763;
pub const PID: u16 = 0x2080;
pub const IF_PLAY: c_int = 1;
pub const IF_CAP: c_int = 2;
pub const EP_PLAY: u8 = 0x01;
pub const EP_CAP: u8 = 0x81;

#[derive(Debug)]
pub struct UsbError {
    pub op: &'static str,
    pub code: c_int,
}

impl fmt::Display for UsbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: libusb_error_name returns a static C string for any code.
        let name = unsafe { std::ffi::CStr::from_ptr(ffi::libusb_error_name(self.code)) };
        write!(f, "{} failed: {}", self.op, name.to_string_lossy())
    }
}

impl std::error::Error for UsbError {}

fn check(op: &'static str, code: c_int) -> Result<c_int, UsbError> {
    if code < 0 {
        Err(UsbError { op, code })
    } else {
        Ok(code)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockSource {
    Internal,
    Spdif,
}

impl ClockSource {
    /// Capture interface alternate setting that selects this clock (protocol.md, clock tests).
    fn capture_alt(self) -> c_int {
        match self {
            ClockSource::Internal => 1,
            ClockSource::Spdif => 2,
        }
    }
}

pub struct Device {
    ctx: *mut ffi::libusb_context,
    handle: *mut ffi::libusb_device_handle,
    claimed: bool,
}

// The handle is used from the event thread (transfers) and the control thread
// (control requests). libusb is thread-safe for these uses.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    pub fn open() -> Result<Self, UsbError> {
        let mut ctx = std::ptr::null_mut();
        // SAFETY: out-pointer initialised by libusb on success.
        check("libusb_init", unsafe { ffi::libusb_init(&mut ctx) })?;
        // libusb prints the underlying IOKit error only at error log level or above.
        // SAFETY: ctx valid; option takes one int argument.
        unsafe { ffi::libusb_set_option(ctx, ffi::constants::LIBUSB_OPTION_LOG_LEVEL, ffi::constants::LIBUSB_LOG_LEVEL_ERROR) };
        // SAFETY: ctx valid.
        let handle = unsafe { ffi::libusb_open_device_with_vid_pid(ctx, VID, PID) };
        if handle.is_null() {
            // SAFETY: ctx valid and unused elsewhere.
            unsafe { ffi::libusb_exit(ctx) };
            return Err(UsbError { op: "open 0763:2080", code: ffi::constants::LIBUSB_ERROR_NO_DEVICE });
        }
        Ok(Self { ctx, handle, claimed: false })
    }

    pub fn ctx(&self) -> *mut ffi::libusb_context {
        self.ctx
    }

    pub fn handle(&self) -> *mut ffi::libusb_device_handle {
        self.handle
    }

    pub fn claim_streaming(&mut self, clock: ClockSource) -> Result<(), UsbError> {
        // SAFETY: handle valid for self's lifetime.
        unsafe {
            check("claim interface 1", ffi::libusb_claim_interface(self.handle, IF_PLAY))?;
            check("claim interface 2", ffi::libusb_claim_interface(self.handle, IF_CAP))?;
            self.claimed = true;
            check("set interface 1 alt 1", ffi::libusb_set_interface_alt_setting(self.handle, IF_PLAY, 1))?;
            check(
                "set interface 2 alt",
                ffi::libusb_set_interface_alt_setting(self.handle, IF_CAP, clock.capture_alt()),
            )?;
        }
        Ok(())
    }

    pub fn release_streaming(&mut self) {
        if !self.claimed {
            return;
        }
        // SAFETY: handle valid; errors ignored on teardown.
        unsafe {
            ffi::libusb_set_interface_alt_setting(self.handle, IF_PLAY, 0);
            ffi::libusb_set_interface_alt_setting(self.handle, IF_CAP, 0);
            ffi::libusb_release_interface(self.handle, IF_PLAY);
            ffi::libusb_release_interface(self.handle, IF_CAP);
        }
        self.claimed = false;
    }

    fn control(&self, bm: u8, req: u8, value: u16, index: u16, data: &mut [u8]) -> Result<usize, UsbError> {
        // SAFETY: data outlives the synchronous call; length matches the buffer.
        let n = unsafe {
            ffi::libusb_control_transfer(self.handle, bm, req, value, index, data.as_mut_ptr(), data.len() as u16, 1000)
        };
        check("control transfer", n).map(|n| n as usize)
    }

    /// UAC1 endpoint SET_CUR sampling frequency. Either endpoint sets both.
    pub fn set_sample_rate(&self, hz: u32) -> Result<(), UsbError> {
        let mut d = [hz as u8, (hz >> 8) as u8, (hz >> 16) as u8];
        self.control(0x22, 0x01, 0x0100, EP_CAP as u16, &mut d).map(|_| ())
    }

    pub fn sample_rate(&self) -> Result<u32, UsbError> {
        let mut d = [0u8; 3];
        self.control(0xA2, 0x81, 0x0100, EP_CAP as u16, &mut d)?;
        Ok(d[0] as u32 | (d[1] as u32) << 8 | (d[2] as u32) << 16)
    }

    /// Firmware version from the device descriptor's bcdDevice, for example 0x0151 = 1.51.
    pub fn firmware_version(&self) -> Option<String> {
        // SAFETY: handle valid; the descriptor is plain data filled by libusb.
        let bcd = unsafe {
            let mut d = std::mem::zeroed::<ffi::libusb_device_descriptor>();
            if ffi::libusb_get_device_descriptor(ffi::libusb_get_device(self.handle), &mut d) < 0 {
                return None;
            }
            d.bcdDevice
        };
        Some(format!("{:x}.{:02x}", bcd >> 8, bcd & 0xff))
    }

    /// Negotiated link: "USB 2.0" at high speed, "USB 1.1" at full speed.
    pub fn link_speed(&self) -> &'static str {
        // SAFETY: handle valid for the lifetime of self.
        match unsafe { ffi::libusb_get_device_speed(ffi::libusb_get_device(self.handle)) } {
            3 => "USB 2.0",
            1 | 2 => "USB 1.1",
            4.. => "USB 3",
            _ => "unknown",
        }
    }

    /// UAC1 SET_CUR to a unit on the control interface. Units 5 to 9: docs/protocol.md.
    pub fn set_cur(&self, unit: u8, control: u8, channel: u8, data: &[u8]) -> Result<(), UsbError> {
        let mut d = data.to_vec();
        let value = (control as u16) << 8 | channel as u16;
        self.control(0x21, 0x01, value, (unit as u16) << 8, &mut d).map(|_| ())
    }

    /// Meters and status (GET_MEM). Returns the snapshot taken at the previous call,
    /// so callers that need current values read twice.
    pub fn read_meters_raw(&self) -> Result<[i16; 27], UsbError> {
        let mut b = [0u8; 54];
        self.control(0xA1, 0x85, 0x0100, 0x0504, &mut b)?;
        let mut w = [0i16; 27];
        for (i, v) in w.iter_mut().enumerate() {
            *v = i16::from_le_bytes([b[2 * i], b[2 * i + 1]]);
        }
        Ok(w)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.release_streaming();
        // SAFETY: handle and ctx valid and no longer used.
        unsafe {
            ffi::libusb_close(self.handle);
            ffi::libusb_exit(self.ctx);
        }
    }
}
