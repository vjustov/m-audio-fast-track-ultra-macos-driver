//! `ftu-daemon test`: streaming test harness that stands in for the plugin. Writes a
//! tone into the playback ring and the capture ring to a WAV file.

use crate::rt;
use crate::shm::{ShmRegion, CHANNELS, RING_FRAMES};
use crate::stream::{Stream, StreamConfig, PLAYBACK_DELAY};
use crate::usage;
use crate::usb::{ClockSource, Device};
use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Args {
    rate: u32,
    seconds: f64,
    clock: ClockSource,
    tone_channel: usize,
    freq: f64,
    dbfs: f64,
    record: Option<String>,
    /// Stream on internal clock for 5 s first, stop, then start on `clock` on the same
    /// handle, as the service does on a clock change.
    switch: bool,
}

pub fn parse_args(mut it: impl Iterator<Item = String>) -> Args {
    let mut a = Args {
        rate: 48000,
        seconds: 10.0,
        clock: ClockSource::Internal,
        tone_channel: 1,
        freq: 997.0,
        dbfs: -20.0,
        record: None,
        switch: false,
    };
    while let Some(k) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match k.as_str() {
            "--rate" => a.rate = val().parse().unwrap_or_else(|_| usage()),
            "--seconds" => a.seconds = val().parse().unwrap_or_else(|_| usage()),
            "--spdif" => a.clock = ClockSource::Spdif,
            "--tone-channel" => a.tone_channel = val().parse().unwrap_or_else(|_| usage()),
            "--freq" => a.freq = val().parse().unwrap_or_else(|_| usage()),
            "--dbfs" => a.dbfs = val().parse().unwrap_or_else(|_| usage()),
            "--record" => a.record = Some(val()),
            "--switch" => a.switch = true,
            _ => usage(),
        }
    }
    if a.tone_channel > CHANNELS {
        usage();
    }
    a
}

struct Wav {
    file: std::io::BufWriter<std::fs::File>,
    bytes: u32,
    rate: u32,
}

impl Wav {
    fn create(path: &str, rate: u32) -> std::io::Result<Self> {
        let mut w = Wav { file: std::io::BufWriter::new(std::fs::File::create(path)?), bytes: 0, rate };
        w.header()?;
        Ok(w)
    }

    fn header(&mut self) -> std::io::Result<()> {
        let fb = (CHANNELS * 3) as u32;
        let f = &mut self.file;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(b"RIFF")?;
        f.write_all(&(36 + self.bytes).to_le_bytes())?;
        f.write_all(b"WAVEfmt ")?;
        f.write_all(&16u32.to_le_bytes())?;
        f.write_all(&1u16.to_le_bytes())?;
        f.write_all(&(CHANNELS as u16).to_le_bytes())?;
        f.write_all(&self.rate.to_le_bytes())?;
        f.write_all(&(self.rate * fb).to_le_bytes())?;
        f.write_all(&(fb as u16).to_le_bytes())?;
        f.write_all(&24u16.to_le_bytes())?;
        f.write_all(b"data")?;
        f.write_all(&self.bytes.to_le_bytes())?;
        Ok(())
    }

    fn frame(&mut self, samples: &[f32]) -> std::io::Result<()> {
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
            self.file.write_all(&v.to_le_bytes()[..3])?;
        }
        self.bytes += (samples.len() * 3) as u32;
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<()> {
        self.header()?;
        self.file.flush()
    }
}

pub fn run_test(a: Args) -> Result<(), Box<dyn std::error::Error>> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let _ = ctrlc_like(move || stop.store(true, Ordering::Release));
    }

    let shm = Arc::new(ShmRegion::new()?);
    let mut dev = Device::open()?;
    if a.switch {
        let first = Stream::start(&mut dev, shm.clone(), StreamConfig { sample_rate: a.rate, clock: ClockSource::Internal, playback_delay: PLAYBACK_DELAY })?;
        std::thread::sleep(Duration::from_secs(5));
        println!("switch: internal stream stopped ({:?})", first.stop(&mut dev));
    }
    let stream = Stream::start(&mut dev, shm.clone(), StreamConfig { sample_rate: a.rate, clock: a.clock, playback_delay: PLAYBACK_DELAY })?;
    let s = stream.shm();
    let rate = s.sample_rate.load(Ordering::Acquire);
    println!("streaming at {rate} Hz, clock {:?}, shared memory {} bytes", a.clock, shm.len());

    let mut wav = match &a.record {
        Some(p) => Some(Wav::create(p, rate)?),
        None => None,
    };
    let amp = 10f64.powf(a.dbfs / 20.0) as f32;
    let w = 2.0 * std::f64::consts::PI * a.freq / rate as f64;
    // Write this far ahead of the daemon's playback read position.
    let lead = (rate / 20) as u64; // 50 ms
    let mut rec_pos = 0u64;
    let mut overruns = 0u64;
    let start = Instant::now();
    let mut last_report = start;
    let ns_per_tick = rt::ns_per_tick();

    while !stop.load(Ordering::Acquire) && start.elapsed().as_secs_f64() < a.seconds && !stream.failed() {
        // Playback writer (stands in for the plugin).
        let read = s.play_read_pos.load(Ordering::Acquire);
        let mut wpos = s.play_write_pos.load(Ordering::Acquire).max(read);
        while wpos < read + lead {
            let fr = s.playback_frame(wpos);
            let v = if a.tone_channel > 0 { amp * ((w * wpos as f64).sin() as f32) } else { 0.0 };
            for c in 0..CHANNELS {
                // SAFETY: frame pointer covers CHANNELS samples; frames ahead of the
                // daemon's read position are owned by the writer.
                unsafe { *fr.add(c) = if c + 1 == a.tone_channel { v } else { 0.0 } };
            }
            wpos += 1;
        }
        s.play_write_pos.store(wpos, Ordering::Release);

        // Capture reader.
        let cap = s.cap_write_pos.load(Ordering::Acquire);
        if cap - rec_pos > RING_FRAMES as u64 {
            overruns += 1;
            rec_pos = cap - RING_FRAMES as u64 / 2;
        }
        while rec_pos < cap {
            let fr = s.capture_frame(rec_pos);
            // SAFETY: frames below cap_write_pos are complete (acquire above).
            let frame = unsafe { std::slice::from_raw_parts(fr, CHANNELS) };
            if let Some(wv) = wav.as_mut() {
                wv.frame(frame)?;
            }
            rec_pos += 1;
        }

        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            let clock = s.read_clock();
            let est = clock.map(|c| 1e9 / (c.host_ticks_per_frame * ns_per_tick)).unwrap_or(0.0);
            println!(
                "t={:5.1}s cap={} play_read={} lead={} underruns={} usb_errors={} clock={:.2} Hz",
                start.elapsed().as_secs_f64(),
                cap,
                read,
                wpos as i64 - read as i64,
                s.play_underruns.load(Ordering::Relaxed),
                s.usb_errors.load(Ordering::Relaxed),
                est
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let failed = stream.failed();
    let elapsed = start.elapsed().as_secs_f64();
    let stats = stream.stop(&mut dev);
    if let Some(wv) = wav {
        wv.finish()?;
    }
    println!(
        "\nelapsed {elapsed:.2}s, capture {} frames ({:.1} Hz), playback {} frames, packet errors {}, fallback packets {}, capture overruns {overruns}",
        stats.cap_frames,
        stats.cap_frames as f64 / elapsed,
        stats.play_frames,
        stats.packet_errors,
        stats.fallback_packets
    );
    if failed {
        return Err("stream failed".into());
    }
    Ok(())
}

/// Minimal SIGINT hook without extra dependencies.
fn ctrlc_like<F: Fn() + Send + Sync + 'static>(f: F) -> std::io::Result<()> {
    static HANDLER: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();
    let _ = HANDLER.set(Box::new(f));
    extern "C" fn on_sigint(_: libc::c_int) {
        if let Some(h) = HANDLER.get() {
            h();
        }
    }
    // SAFETY: installing a handler that only stores to an atomic.
    unsafe { libc::signal(libc::SIGINT, on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    Ok(())
}
