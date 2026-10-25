//! Saved settings: the mixer state in `mixer.json` and the clock source in
//! `device.json`, both in `~/Library/Application Support/FastTrackUltra/` for the
//! daemon's user.

use crate::mixer::Mixer;
use crate::usb::ClockSource;
use serde_json::{json, Value};
use std::ffi::CStr;
use std::path::{Path, PathBuf};

pub struct Store {
    // None when the user's home cannot be found: nothing is loaded or saved.
    dir: Option<PathBuf>,
}

impl Store {
    /// The store in the daemon user's home.
    pub fn user() -> Self {
        // SAFETY: getpwuid returns a pointer to static storage or null; read immediately.
        let home = unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if pw.is_null() {
                return Self { dir: None };
            }
            CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned()
        };
        Self::at(PathBuf::from(home).join("Library/Application Support/FastTrackUltra"))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: Some(dir.into()) }
    }

    fn file(&self, name: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(name))
    }

    /// The saved mixer state, or defaults. An unreadable file is moved aside to
    /// `mixer.json.bad` so it is not overwritten.
    pub fn load_mixer(&self) -> Mixer {
        let Some(p) = self.file("mixer.json") else { return Mixer::default() };
        match std::fs::read_to_string(&p) {
            Ok(s) => match serde_json::from_str::<Mixer>(&s) {
                Ok(mut m) => {
                    m.normalize();
                    eprintln!("ftu-daemon: mixer state loaded from {}", p.display());
                    m
                }
                Err(e) => {
                    let bad = p.with_extension("json.bad");
                    let _ = std::fs::rename(&p, &bad);
                    eprintln!("ftu-daemon: {} unreadable ({e}), moved to {}, using defaults", p.display(), bad.display());
                    Mixer::default()
                }
            },
            Err(_) => Mixer::default(),
        }
    }

    pub fn load_clock(&self) -> ClockSource {
        self.file("device.json")
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("clock").and_then(Value::as_str).and_then(ClockSource::parse))
            .unwrap_or(ClockSource::Internal)
    }

    /// Writes through a temporary file, so a crash never leaves a half-written state.
    pub fn save_mixer(&self, m: &Mixer) {
        let Some(p) = self.file("mixer.json") else { return };
        let write = || -> std::io::Result<()> {
            create_parent(&p)?;
            let tmp = p.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
            std::fs::rename(&tmp, &p)
        };
        if let Err(e) = write() {
            eprintln!("ftu-daemon: saving {} failed: {e}", p.display());
        }
    }

    pub fn save_clock(&self, c: ClockSource) {
        let Some(p) = self.file("device.json") else { return };
        let _ = create_parent(&p);
        if let Err(e) = std::fs::write(&p, json!({ "clock": c.name() }).to_string()) {
            eprintln!("ftu-daemon: saving {} failed: {e}", p.display());
        }
    }
}

fn create_parent(p: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(p.parent().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ftu-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn saves_and_loads_mixer_and_clock() {
        let d = temp_dir("roundtrip");
        let s = Store::at(&d);
        assert_eq!(s.load_mixer(), Mixer::default());
        assert_eq!(s.load_clock(), ClockSource::Internal);
        let mut m = Mixer::default();
        m.set("bus1.in1.level", "-6").unwrap();
        s.save_mixer(&m);
        s.save_clock(ClockSource::Spdif);
        let s = Store::at(&d);
        assert_eq!(s.load_mixer(), m);
        assert_eq!(s.load_clock(), ClockSource::Spdif);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unreadable_mixer_file_is_moved_aside() {
        let d = temp_dir("bad");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("mixer.json"), "{not json").unwrap();
        assert_eq!(Store::at(&d).load_mixer(), Mixer::default());
        assert!(d.join("mixer.json.bad").exists());
        assert!(!d.join("mixer.json").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
