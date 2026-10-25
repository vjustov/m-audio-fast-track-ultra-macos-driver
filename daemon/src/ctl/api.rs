//! The XPC control API. Requests and replies are JSON strings carried in the XPC "ctl"
//! op (xpc_shim.c):
//!   {"get": "state"}                       -> the mixer state (mixer.rs)
//!   {"get": "meters"}                      -> latest meter snapshot
//!   {"get": "status"}                      -> connection, port, streaming, rate, clock, power, firmware
//!   {"clock": "internal" | "spdif"}        -> {"ok": true}; the stream restarts on the new clock
//!   {"stall_ms": n}                        -> {"ok": true}; test hook, stalls the USB event thread once
//!   {"playback_delay_ms": n}               -> {"ok": true}; test hook, playback start delay from the next start
//!   {"put": {state}}                       -> replaces the whole state (settings file restore)
//!   {"set": [["bus1.in1.level", "-6"], ..]} -> {"ok": true} or {"error": ".."}; all or nothing
//!   {"reset": true}                        -> defaults

use super::Control;
use crate::mixer::Mixer;
use crate::usb::ClockSource;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

impl Control {
    pub(super) fn handle(&self, req: &str) -> Value {
        let req: Value = match serde_json::from_str(req) {
            Ok(v) => v,
            Err(e) => return json!({ "error": format!("bad request: {e}") }),
        };
        if let Some(what) = req.get("get").and_then(Value::as_str) {
            return match what {
                "state" => serde_json::to_value(&*self.mixer.lock().unwrap()).unwrap(),
                "meters" => self.meters_json(),
                "status" => self.status_json(),
                _ => json!({ "error": format!("unknown get '{what}'") }),
            };
        }
        if let Some(list) = req.get("set").and_then(Value::as_array) {
            let mut m = self.mixer.lock().unwrap();
            let mut next = m.clone();
            for item in list {
                let pair = item.as_array().map(|a| (a.first().and_then(Value::as_str), a.get(1).and_then(Value::as_str)));
                let (Some((Some(path), Some(value))), true) = (pair, item.as_array().is_some_and(|a| a.len() == 2)) else {
                    return json!({ "error": "set expects [[path, value], ...] with string values" });
                };
                if let Err(e) = next.set(path, value) {
                    return json!({ "error": e });
                }
            }
            if next != *m {
                *m = next;
                self.changed(&m);
            }
            return json!({ "ok": true });
        }
        if let Some(state) = req.get("put") {
            let mut next: Mixer = match serde_json::from_value(state.clone()) {
                Ok(m) => m,
                Err(e) => return json!({ "error": format!("bad state: {e}") }),
            };
            next.normalize();
            let mut m = self.mixer.lock().unwrap();
            *m = next;
            self.changed(&m);
            return json!({ "ok": true });
        }
        if let Some(c) = req.get("clock").and_then(Value::as_str) {
            return match ClockSource::parse(c) {
                Some(c) => self.set_clock(c),
                None => json!({ "error": format!("unknown clock '{c}' (internal or spdif)") }),
            };
        }
        if let Some(ms) = req.get("stall_ms").and_then(Value::as_u64) {
            self.stall_ms.store(ms.min(1000) as u32, Ordering::Release);
            return json!({ "ok": true });
        }
        if let Some(ms) = req.get("playback_delay_ms").and_then(Value::as_u64) {
            self.playback_delay_ms.store(ms.min(20) as u32, Ordering::Release);
            return json!({ "ok": true });
        }
        if req.get("reset").and_then(Value::as_bool) == Some(true) {
            let mut m = self.mixer.lock().unwrap();
            *m = Mixer::default();
            self.changed(&m);
            return json!({ "ok": true });
        }
        json!({ "error": "unknown request" })
    }

    fn status_json(&self) -> Value {
        let st = self.status.lock().unwrap();
        let word = self.meters.lock().unwrap().as_ref().map(|m| m.words[26] as u16);
        json!({
            "connected": st.connected,
            "streaming": st.sample_rate.is_some(),
            "sample_rate": st.sample_rate,
            "clock": self.clock().name(),
            // Bit 12 set means the external clock is not valid (kext K:0xe52). Untested:
            // on this unit the receiver locks to its own S/PDIF output.
            "clock_locked": match (st.clock, word) {
                (Some(ClockSource::Spdif), Some(w)) => Some(w & 0x1000 == 0),
                _ => None,
            },
            // Status bit 0: 1 = bus powered, 0 = PSU (docs/protocol.md).
            "port": st.port,
            "power": word.map(|w| if w & 1 != 0 { "usb" } else { "psu" }),
            // Bit 1: the power source the device started with (vendor panel text,
            // docs/protocol.md). It tracked bit 0 on a live PSU change here.
            "startup_power": word.map(|w| if w & 2 != 0 { "usb" } else { "psu" }),
            "firmware": st.firmware,
            "driver": env!("CARGO_PKG_VERSION"),
        })
    }

    fn meters_json(&self) -> Value {
        match &*self.meters.lock().unwrap() {
            Some(mt) => {
                let w = mt.peak();
                json!({
                    "in": &w[0..8],
                    "daw": &w[8..16],
                    "out": &w[16..24],
                    "extra": &w[24..26],
                    "status": mt.words[26] as u16,
                    "age_ms": mt.at.elapsed().as_millis() as u64,
                })
            }
            None => json!({ "error": "no meters yet (device not connected?)" }),
        }
    }

    fn set_clock(&self, c: ClockSource) -> Value {
        if c == ClockSource::Spdif && self.status.lock().unwrap().port == Some("USB 1.1") {
            return json!({ "error": "S/PDIF clock needs a USB 2.0 port" });
        }
        let mut cur = self.clock.lock().unwrap();
        if *cur == c {
            return json!({ "ok": true });
        }
        *cur = c;
        self.store.save_clock(c);
        self.clock_changed.store(true, Ordering::Release);
        self.wake.notify_all();
        json!({ "ok": true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctl::Store;

    fn control(name: &str) -> (Control, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("ftu-api-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        (Control::new(Store::at(&d)), d)
    }

    #[test]
    fn set_changes_state_saves_it_and_marks_it_for_the_device() {
        let (c, d) = control("set");
        *c.dirty.lock().unwrap() = false;
        assert_eq!(c.handle(r#"{"set": [["bus1.in1.level", "-6"]]}"#), json!({ "ok": true }));
        assert!(*c.dirty.lock().unwrap());
        assert_eq!(c.handle(r#"{"get": "state"}"#), serde_json::to_value(Store::at(&d).load_mixer()).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_bad_set_changes_nothing() {
        let (c, d) = control("bad-set");
        let before = c.handle(r#"{"get": "state"}"#);
        let r = c.handle(r#"{"set": [["bus1.in1.level", "-6"], ["bus9.in1.level", "0"]]}"#);
        assert!(r.get("error").is_some(), "{r}");
        assert_eq!(c.handle(r#"{"get": "state"}"#), before);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn clock_change_is_saved_and_signalled_once() {
        let (c, d) = control("clock");
        assert_eq!(c.handle(r#"{"clock": "spdif"}"#), json!({ "ok": true }));
        assert_eq!(c.clock(), ClockSource::Spdif);
        assert!(c.take_clock_change());
        assert!(!c.take_clock_change());
        assert_eq!(Store::at(&d).load_clock(), ClockSource::Spdif);
        // Same clock again: nothing to restart.
        c.handle(r#"{"clock": "spdif"}"#);
        assert!(!c.take_clock_change());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn spdif_clock_is_refused_on_usb_1_1() {
        let (c, d) = control("usb11");
        c.status.lock().unwrap().port = Some("USB 1.1");
        assert!(c.handle(r#"{"clock": "spdif"}"#).get("error").is_some());
        assert_eq!(c.clock(), ClockSource::Internal);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unknown_and_malformed_requests_are_errors() {
        let (c, d) = control("errors");
        for req in ["not json", r#"{"get": "nope"}"#, r#"{"frobnicate": 1}"#, r#"{"set": [["a"]]}"#] {
            assert!(c.handle(req).get("error").is_some(), "{req}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The replies the panel decodes (panel/Sources/FastTrackUltraPanel/Model.swift),
    /// checked against `shared/fixtures/`. The panel's tests decode the same files, so a
    /// change on either side that breaks the other fails a test. After an intended
    /// change: `UPDATE_FIXTURES=1 cargo test contract`, then run the panel's tests.
    #[test]
    fn contract_fixtures_match() {
        let (c, d) = control("contract");
        // A state with every kind of value, not only defaults (null levels, zero pans).
        let set = r#"{"set": [["bus1.in1.level", "-6"], ["bus1.in1.pan", "0.25"], ["bus2.daw3.mute", "on"], ["link.in1", "on"],
            ["fx.program", "delay"], ["fx.send.in2", "-10"], ["fx.return.1", "-3"], ["fx.bypass.2", "on"], ["out1.level", "-1.5"]]}"#;
        assert_eq!(c.handle(set), json!({ "ok": true }));
        {
            let mut st = c.status.lock().unwrap();
            st.connected = true;
            st.sample_rate = Some(48000);
            st.firmware = Some("1.51".into());
            st.port = Some("USB 2.0");
            st.clock = Some(ClockSource::Internal);
        }
        let mut recent = [0u16; 26];
        recent[0] = 3276;
        *c.meters.lock().unwrap() = Some(super::super::Meters {
            words: std::array::from_fn(|i| if i == 26 { 0x0100 } else { recent.get(i).map_or(0, |&v| v as i16) }),
            recent: vec![recent],
            at: std::time::Instant::now(),
        });
        let mut status = c.handle(r#"{"get": "status"}"#);
        status["driver"] = json!("0.0.0");
        let mut meters = c.handle(r#"{"get": "meters"}"#);
        meters["age_ms"] = json!(0);
        let replies = [("mixer-state.json", c.handle(r#"{"get": "state"}"#)), ("status.json", status), ("meters.json", meters)];

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../shared/fixtures");
        for (name, v) in replies {
            let path = dir.join(name);
            let text = serde_json::to_string_pretty(&v).unwrap() + "\n";
            if std::env::var_os("UPDATE_FIXTURES").is_some() {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &text).unwrap();
                continue;
            }
            let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e} (run with UPDATE_FIXTURES=1)", path.display()));
            assert_eq!(text, want, "{name} changed: update the fixture and the panel (see the comment above)");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
