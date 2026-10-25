//! ftu-daemon: helper daemon for the M-Audio Fast Track Ultra.
//!
//! Subcommands:
//!   run   Serve the HAL plugin: XPC service plus streaming, reconnecting to the device.
//!         Started by launchd (packaging/local.ftu.daemon.plist).
//!   ctl   Mixer control client: talks to the running daemon over XPC.
//!   test  Streaming test harness that stands in for the plugin: tone into the
//!         playback ring, capture ring to a WAV file.

mod ctl;
mod harness;
mod mixer;
mod pacing;
mod rt;
mod shm;
mod stream;
mod supervisor;
mod usb;

pub const SERVICE_NAME: &str = "local.ftu.daemon";

pub fn usage() -> ! {
    eprintln!(
        "usage: ftu-daemon run\n       ftu-daemon ctl get [state|meters|status] | set PATH VALUE [PATH VALUE ...] | clock internal|spdif | stall MS | playback-delay MS | reset\n       ftu-daemon test [--rate HZ] [--seconds S] [--spdif] [--tone-channel 0-8] [--freq HZ] [--dbfs DB] [--record out.wav] [--switch]\n\
         Tone channel 0 disables the tone. --switch streams 5 s on internal clock first, then\n\
         switches on the same handle, as the service does."
    );
    std::process::exit(2)
}

extern "C" {
    fn ftu_xpc_ctl_call(name: *const libc::c_char, request: *const libc::c_char) -> *mut libc::c_char;
}

/// Local time as HH:MM:SS.mmm, for log lines.
pub fn wall_clock() -> String {
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

/// `ftu-daemon ctl ...`: builds one request, prints the reply. Returns the exit code.
fn run_ctl(args: Vec<String>) -> i32 {
    let req = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["get"] | ["get", "state"] => serde_json::json!({ "get": "state" }),
        ["get", "meters"] | ["meters"] => serde_json::json!({ "get": "meters" }),
        ["get", "status"] | ["status"] => serde_json::json!({ "get": "status" }),
        ["reset"] => serde_json::json!({ "reset": true }),
        ["clock", c] => serde_json::json!({ "clock": c }),
        ["stall", ms] => serde_json::json!({ "stall_ms": ms.parse::<u64>().unwrap_or_else(|_| usage()) }),
        ["playback-delay", ms] => serde_json::json!({ "playback_delay_ms": ms.parse::<u64>().unwrap_or_else(|_| usage()) }),
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
            if let Err(e) = supervisor::run() {
                eprintln!("ftu-daemon: {e}");
                std::process::exit(1);
            }
        }
        Some("ctl") => std::process::exit(run_ctl(argv.collect())),
        Some("test") => {
            if let Err(e) = harness::run_test(harness::parse_args(argv)) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        _ => usage(),
    }
}
