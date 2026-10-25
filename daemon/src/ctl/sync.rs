//! Pushes the mixer state to the device and polls the meters, from the service loop.

use super::{Control, Meters};
use crate::mixer::Ctl;
use crate::usb::Device;
use std::collections::HashMap;
use std::time::{Duration, Instant};

// Each GET_MEM returns the snapshot taken at the previous one, so the poll period is
// also the meters' age.
const METER_PERIOD: Duration = Duration::from_millis(20);
/// Polls kept for the peak window: 3 × 20 ms covers a client reading every 33 ms
/// (the panel's 30 Hz) without losing short peaks, and no reader resets it for another.
pub(super) const PEAK_POLLS: usize = 3;

/// The device side of the control service. Owned by the service loop.
#[derive(Default)]
pub struct DeviceSync {
    /// Controls already on the device, by (unit, control, channel).
    sent: HashMap<(u8, u8, u8), Vec<u8>>,
}

impl DeviceSync {
    /// Forgets what the device holds, so the next pass sends the whole mixer. Call
    /// whenever the device may have reset (replug, power cycle, failed stream).
    pub fn clear(&mut self) {
        self.sent.clear();
    }

    /// Sends state changes that are not yet on the device, polls the meters, and waits
    /// up to `timeout` for the next change.
    pub fn service(&mut self, control: &Control, dev: &Device, timeout: Duration) {
        let poll_due = control.meters.lock().unwrap().as_ref().map_or(true, |m| m.at.elapsed() >= METER_PERIOD);
        if poll_due {
            self.poll_meters(control, dev);
        }
        let pending = std::mem::take(&mut *control.dirty.lock().unwrap());
        if pending || self.sent.is_empty() {
            let controls: Vec<Ctl> = control.mixer.lock().unwrap().controls();
            if !self.send(dev, controls) {
                // Retry on a later pass, without spinning.
                let mut dirty = control.dirty.lock().unwrap();
                *dirty = true;
                let _ = control.wake.wait_timeout(dirty, timeout);
                return;
            }
        }
        let dirty = control.dirty.lock().unwrap();
        if !*dirty {
            let _ = control.wake.wait_timeout(dirty, timeout.min(METER_PERIOD));
        }
    }

    fn poll_meters(&mut self, control: &Control, dev: &Device) {
        let Ok(words) = dev.read_meters_raw() else { return };
        let mut meters = control.meters.lock().unwrap();
        let mut recent = meters.as_ref().map_or_else(Vec::new, |m| m.recent.clone());
        let mut mag = [0u16; 26];
        for (m, w) in mag.iter_mut().zip(&words) {
            *m = w.unsigned_abs();
        }
        recent.push(mag);
        if recent.len() > PEAK_POLLS {
            recent.remove(0);
        }
        let prev = meters.replace(Meters { words, recent, at: Instant::now() });
        drop(meters);
        // Power mode change (status bits 0 and 1): the DSP restarts, so resend everything.
        if prev.is_some_and(|p| (p.words[26] ^ words[26]) & 0x3 != 0) {
            eprintln!("ftu-daemon: power mode changed (status {:#06x}), restoring mixer", words[26] as u16);
            self.sent.clear();
        }
    }

    /// Sends the controls whose value differs from what the device holds. Returns false
    /// if a transfer failed; the rest are sent on a later pass.
    fn send(&mut self, dev: &Device, controls: Vec<Ctl>) -> bool {
        let mut n = 0;
        for c in controls {
            if self.sent.get(&c.key()) == Some(&c.data) {
                continue;
            }
            if let Err(e) = dev.set_cur(c.unit, c.control, c.channel, &c.data) {
                eprintln!("ftu-daemon: unit {} control {} channel {}: {e}", c.unit, c.control, c.channel);
                log_written(n);
                return false;
            }
            self.sent.insert(c.key(), c.data);
            n += 1;
        }
        log_written(n);
        true
    }
}

fn log_written(n: usize) {
    if n > 20 {
        eprintln!("ftu-daemon: wrote {n} mixer controls");
    }
}
