//! ftu-daemon: helper daemon for the M-Audio Fast Track Ultra.
//!
//! Subcommands:
//!   run   Serve the HAL plugin: XPC service plus streaming, reconnecting to the device.
//!         Started by launchd (packaging/local.ftu.daemon.plist).
//!   ctl   Mixer control client: talks to the running daemon over XPC.
//!   test  Streaming test harness that stands in for the plugin: tone into the
//!         playback ring, capture ring to a WAV file.

mod ctl;
mod mixer;
mod rt;
mod shm;
mod stream;
mod usb;

use shm::{ShmRegion, CHANNELS, RING_FRAMES};
use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use stream::{Stream, StreamConfig};
use usb::{ClockSource, Device};

struct Args {
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

fn parse_args(mut it: impl Iterator<Item = String>) -> Args {
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

fn usage() -> ! {
    eprintln!(
        "usage: ftu-daemon run\n       ftu-daemon ctl get [state|meters|status] | set PATH VALUE [PATH VALUE ...] | clock internal|spdif | stall MS | reset\n       ftu-daemon test [--rate HZ] [--seconds S] [--spdif] [--tone-channel 0-8] [--freq HZ] [--dbfs DB] [--record out.wav] [--switch]\n\
         Tone channel 0 disables the tone. --switch streams 5 s on internal clock first, then\n\
         switches on the same handle, as the service does."
    );
    std::process::exit(2)
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

fn run_test(a: Args) -> Result<(), Box<dyn std::error::Error>> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let _ = ctrlc_like(move || stop.store(true, Ordering::Release));
    }

    let shm = Arc::new(ShmRegion::new()?);
    let mut dev = Device::open()?;
    if a.switch {
        let first = Stream::start(&mut dev, shm.clone(), StreamConfig { sample_rate: a.rate, clock: ClockSource::Internal })?;
        std::thread::sleep(Duration::from_secs(5));
        println!("switch: internal stream stopped ({:?})", first.stop(&mut dev));
    }
    let stream = Stream::start(&mut dev, shm.clone(), StreamConfig { sample_rate: a.rate, clock: a.clock })?;
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

const SERVICE_NAME: &str = "local.ftu.daemon";
const DEFAULT_RATE: u32 = 48000;

extern "C" {
    fn ftu_xpc_listen(name: *const libc::c_char, addr: *mut libc::c_void, len: usize) -> libc::c_int;
    fn ftu_xpc_take_requested_rate() -> u32;
    fn ftu_xpc_ctl_call(name: *const libc::c_char, request: *const libc::c_char) -> *mut libc::c_char;
}

/// Runs forever: holds the shared memory, serves XPC, and keeps the device streaming.
fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    let shm = Arc::new(ShmRegion::new()?);
    let control = ctl::Control::init();
    let name = std::ffi::CString::new(SERVICE_NAME)?;
    // SAFETY: the region lives until process exit (shm is never dropped in this loop).
    if unsafe { ftu_xpc_listen(name.as_ptr(), shm.as_ptr() as *mut libc::c_void, shm.len()) } != 0 {
        return Err("XPC listener failed (is the service registered with launchd?)".into());
    }
    eprintln!("ftu-daemon: serving {SERVICE_NAME}, shared memory {} bytes", shm.len());

    let mut rate = DEFAULT_RATE;
    // Controls already on the device. Cleared whenever the device may have reset.
    let mut sent = HashMap::new();
    loop {
        let mut dev = match Device::open() {
            Ok(d) => d,
            Err(_) => {
                control.device_lost();
                sent.clear();
                // Keep honouring rate requests while the device is away.
                // SAFETY: plain C function, no arguments.
                let r = unsafe { ftu_xpc_take_requested_rate() };
                if r != 0 {
                    rate = r;
                }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        control.device_opened(&dev);
        // Restore the mixer before audio starts.
        control.service(&dev, &mut sent, Duration::ZERO);
        control.take_clock_change();
        let clock = if dev.link_speed() == "USB 2.0" { control.clock() } else { ClockSource::Internal };
        let mut cfg = StreamConfig { sample_rate: rate, clock };
        if clock == ClockSource::Spdif {
            // Select alternate setting 2 first, so the status word reports the S/PDIF rate.
            if let Err(e) = dev.claim_streaming(clock) {
                eprintln!("ftu-daemon: selecting S/PDIF clock failed: {e}");
            }
            std::thread::sleep(Duration::from_millis(100));
            cfg.sample_rate = device_rate(&dev).unwrap_or(rate);
        }
        match Stream::start(&mut dev, shm.clone(), cfg) {
            Ok(stream) => {
                let streaming_rate = stream.shm().sample_rate.load(Ordering::Acquire);
                eprintln!("ftu-daemon: streaming at {streaming_rate} Hz, clock {}", clock.name());
                control.set_streaming(Some(streaming_rate), Some(clock));
                let mut new_rate = None;
                let mut clock_change = false;
                // On S/PDIF clock: when the device started reporting another rate.
                let mut rate_mismatch_since: Option<Instant> = None;
                let mut last_report = Instant::now();
                let mut reported = [0u64; 15];
                while !stream.failed() {
                    // Log the error counters when any changes, at most once a second, with
                    // the wall-clock time so events can be matched with a client's log.
                    if last_report.elapsed() >= Duration::from_secs(1) {
                        last_report = Instant::now();
                        let s = stream.shm();
                        let mut now = [
                            &s.play_underruns,
                            &s.usb_errors,
                            &s.plugin_late_writes,
                            &s.plugin_short_reads,
                            &s.plugin_input_jumps,
                            &s.plugin_output_jumps,
                            &s.lead_frames_added,
                            &s.lead_frames_dropped,
                            &s.lead_jumps,
                            &s.fallback_packets,
                            &s.capture_errors,
                            &s.playback_errors,
                            &s.fifo_overflows,
                            &s.callback_stalls,
                            &s.max_callback_gap_us,
                        ]
                        .map(|c| c.load(Ordering::Relaxed));
                        // The max gap is since the previous line, so it does not trigger one.
                        if now[..14] != reported[..14] {
                            now[14] = s.max_callback_gap_us.swap(0, Ordering::Relaxed);
                            eprintln!(
                                "ftu-daemon: {} playback underruns {}, usb errors {}, plugin late writes {}, short reads {}, input jumps {}, output jumps {}, lead frames added {}, dropped {}, lead jumps {}, fallback packets {}, capture errors {}, playback errors {}, fifo overflows {}, stalls {}, max gap {} us, lead {} frames",
                                wall_clock(),
                                now[0],
                                now[1],
                                now[2],
                                now[3],
                                now[4],
                                now[5],
                                now[6],
                                now[7],
                                now[8],
                                now[9],
                                now[10],
                                now[11],
                                now[12],
                                now[13],
                                now[14],
                                s.play_read_pos.load(Ordering::Relaxed) as i64 - s.cap_write_pos.load(Ordering::Relaxed) as i64
                            );
                            reported = now;
                        }
                    }
                    // SAFETY: as above.
                    let r = unsafe { ftu_xpc_take_requested_rate() };
                    // On S/PDIF clock the source sets the rate, so requests are dropped.
                    if r != 0 && r != rate && clock == ClockSource::Internal {
                        new_rate = Some(r);
                        break;
                    }
                    let stall = control.take_stall();
                    if stall > 0 {
                        stream.inject_stall(stall);
                    }
                    if control.take_clock_change() {
                        clock_change = true;
                        break;
                    }
                    // Follow the source's rate. The status word can lag a restart, so a
                    // mismatch must last 500 ms.
                    if clock == ClockSource::Spdif {
                        let reported = control.status_word().map(ctl::status_rate);
                        if reported.is_some_and(|r| r != streaming_rate) {
                            let since = *rate_mismatch_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= Duration::from_millis(500) {
                                new_rate = reported;
                                break;
                            }
                        } else {
                            rate_mismatch_since = None;
                        }
                    }
                    control.service(&dev, &mut sent, Duration::from_millis(50));
                }
                control.set_streaming(None, None);
                let stats = stream.stop(&mut dev);
                if clock_change {
                    eprintln!("ftu-daemon: switching to {} clock", control.clock().name());
                    // Keep the current rate, so leaving S/PDIF does not jump.
                    rate = streaming_rate;
                    continue;
                }
                match new_rate {
                    Some(r) => {
                        eprintln!("ftu-daemon: switching to {r} Hz");
                        rate = r;
                        continue; // reuse the open device, restart immediately
                    }
                    None => {
                        eprintln!("ftu-daemon: stream stopped ({stats:?}), restarting");
                        sent.clear(); // a replug or power cycle also stops the stream
                        continue; // device handle still open; restart right away
                    }
                }
            }
            Err(e) => {
                eprintln!("ftu-daemon: start failed: {e}");
                sent.clear();
            }
        }
        drop(dev);
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Local time as HH:MM:SS.mmm, for log lines.
fn wall_clock() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    // SAFETY: localtime_r writes into the tm we own.
    let tm = unsafe {
        let mut tm = std::mem::zeroed::<libc::tm>();
        libc::localtime_r(&secs, &mut tm);
        tm
    };
    format!("{:02}:{:02}:{:02}.{:03}", tm.tm_hour, tm.tm_min, tm.tm_sec, now.subsec_millis())
}

/// Rate from the device status word. The meter read returns the previous snapshot, so
/// read twice.
fn device_rate(dev: &Device) -> Option<u32> {
    dev.read_meters_raw().ok()?;
    dev.read_meters_raw().ok().map(|w| ctl::status_rate(w[26] as u16))
}

/// `ftu-daemon ctl ...`: builds one request, prints the reply. Returns the exit code.
fn run_ctl(args: Vec<String>) -> i32 {
    let req = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["get"] | ["get", "state"] => serde_json::json!({ "get": "state" }),
        ["get", "meters"] | ["meters"] => serde_json::json!({ "get": "meters" }),
        ["get", "status"] | ["status"] => serde_json::json!({ "get": "status" }),
        ["reset"] => serde_json::json!({ "reset": true }),
        ["clock", c] => serde_json::json!({ "clock": c }),
        ["stall", ms] => serde_json::json!({ "stall_ms": ms.parse::<u64>().unwrap_or_else(|_| usage()) }),
        ["set", rest @ ..] if !rest.is_empty() && rest.len() % 2 == 0 => {
            serde_json::json!({ "set": rest.chunks(2).map(|p| [p[0], p[1]]).collect::<Vec<_>>() })
        }
        _ => usage(),
    };
    let name = std::ffi::CString::new(SERVICE_NAME).unwrap();
    let body = std::ffi::CString::new(req.to_string()).unwrap();
    // SAFETY: both strings are valid for the call; the reply is malloc'd by the shim.
    let reply = unsafe {
        let p = ftu_xpc_ctl_call(name.as_ptr(), body.as_ptr());
        if p.is_null() {
            eprintln!("ftu-daemon ctl: no reply from {SERVICE_NAME} (is the daemon running?)");
            return 1;
        }
        let s = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
        libc::free(p as *mut libc::c_void);
        s
    };
    let v: serde_json::Value = serde_json::from_str(&reply).unwrap_or(serde_json::Value::String(reply));
    if let Some(e) = v.get("error") {
        eprintln!("ftu-daemon ctl: {}", e.as_str().unwrap_or_default());
        return 1;
    }
    if req.get("get").and_then(|g| g.as_str()) == Some("meters") {
        print_meters(&v);
    } else if v.get("ok").is_none() {
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    }
    0
}

fn print_meters(v: &serde_json::Value) {
    let row = |key: &str| {
        let cells: Vec<String> = v[key]
            .as_array()
            .map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0).unsigned_abs()).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .map(|w| if w == 0 { "   -inf".into() } else { format!("{:7.1}", 20.0 * (w as f64 / 32768.0).log10()) })
            .collect();
        println!("{key:<6}{}", cells.join(""));
    };
    println!("dBFS  {}", (1..=8).map(|i| format!("{i:>7}")).collect::<String>());
    row("in");
    row("daw");
    row("out");
    println!("status {:#06x}, age {} ms", v["status"].as_u64().unwrap_or(0), v["age_ms"].as_u64().unwrap_or(0));
}

fn main() {
    let mut argv = std::env::args().skip(1);
    match argv.next().as_deref() {
        Some("run") => {
            if let Err(e) = run_service() {
                eprintln!("ftu-daemon: {e}");
                std::process::exit(1);
            }
        }
        Some("ctl") => std::process::exit(run_ctl(argv.collect())),
        Some("test") => {
            if let Err(e) = run_test(parse_args(argv)) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        _ => usage(),
    }
}
