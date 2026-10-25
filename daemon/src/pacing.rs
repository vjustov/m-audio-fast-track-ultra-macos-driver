//! Stream timing logic that does not touch USB or shared memory: implicit-feedback
//! packet sizing, the playback lead controller and the device clock loop.
//!
//! `stream.rs` drives these from its transfer callbacks. Keeping them free of libusb and
//! the shared region lets the tests below replay stalls, rate patterns and starts
//! against a simulated device.

// Lead control: smoothing per playback transfer (1 ms), and the minimum number of
// transfers between corrections.
const LEAD_ALPHA: f64 = 0.05;
const LEAD_HOLD: u32 = 20;
// Smoothed lead error, in packets, that triggers a one-frame correction, and the error
// that is fixed in one jump instead.
const LEAD_BAND_PACKETS: f64 = 2.0;
const LEAD_JUMP_PACKETS: f64 = 4.0;
// Playback transfers after a start or a stall during which the smoothed error just
// follows the raw error, so one jump fixes the shift instead of a series chasing a
// lagging average. More than the 16 playback transfers in flight, since the whole queue
// may complete in the backlog after a stall.
const LEAD_SETTLE: u32 = 24;
const FIFO_LEN: usize = 4096;
// Clock loop for 1 ms updates, 0.5 Hz bandwidth: omega = 2*pi*0.5*0.001,
// b = sqrt(2)*omega, c = omega^2.
const DLL_B: f64 = 0.004443;
const DLL_C: f64 = 0.00000987;
// Callbacks in a row that must be far off before the clock re-anchors.
const RESYNC_AFTER: u32 = 3;

/// A lead jump decided by `Pacer::plan_correction`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Jump {
    /// Playback position before the jump.
    pub from: u64,
    /// Frames removed from the lead: positive skips back (replays), negative skips ahead.
    pub frames: i64,
    /// Raw and smoothed lead error at the decision.
    pub err: i64,
    pub smoothed: f64,
}

/// One playback packet from `Pacer::next_packet`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Packet {
    pub frames: usize,
    /// No capture size was queued, so the nominal size was used.
    pub fallback: bool,
    /// One-frame lead correction applied to this packet: -1, 0 or 1.
    pub correction: i64,
}

/// Capture and playback positions, the capture size FIFO that implicit feedback reads,
/// and the controller that holds playback a fixed lead ahead of capture.
pub struct Pacer {
    rate: u32,
    max_frames: usize,

    // Frame counts of completed capture packets, consumed by playback packets.
    fifo: Box<[u16; FIFO_LEN]>,
    fifo_head: usize,
    fifo_tail: usize,
    nominal_acc: f64,

    pub cap_pos: u64,
    pub play_pos: u64,
    // Playback position minus capture position that the stream should hold: the
    // prefilled playback queue. Drift away from it means the device consumes at a
    // different rate than we send, or that fallback packets were used.
    pub target_lead: i64,
    // Lead error smoothed over about 20 ms, sampled once per playback transfer. The raw
    // error swings by about one transfer either way as capture transfers complete
    // between playback transfers, so it cannot be acted on directly.
    lead_err_avg: f64,
    // Playback transfers since the last correction, and the correction (-1, 0, 1)
    // the next packet applies.
    since_correction: u32,
    correction: i64,
    // Transfers left in the settle period after a start or a stall (LEAD_SETTLE).
    settle: u32,
    // Until the first decision after start, which corrects any error exactly, so the
    // lead (and the round-trip latency) is the same after every start instead of
    // anywhere in the correction band.
    startup: bool,
}

impl Pacer {
    /// `lead_packets` is the playback queue depth in packets; `max_frames` caps a packet.
    pub fn new(rate: u32, lead_packets: usize, max_frames: usize) -> Self {
        Self {
            rate,
            max_frames,
            fifo: Box::new([0; FIFO_LEN]),
            fifo_head: 0,
            fifo_tail: 0,
            nominal_acc: 0.0,
            cap_pos: 0,
            play_pos: 0,
            target_lead: lead_packets as i64 * rate as i64 / 8000,
            lead_err_avg: 0.0,
            since_correction: 0,
            correction: 0,
            settle: LEAD_SETTLE,
            startup: true,
        }
    }

    /// Records a completed capture packet. Returns false if the size FIFO was full and
    /// the size was dropped.
    pub fn capture_packet(&mut self, frames: usize) -> bool {
        let queued = self.fifo_head - self.fifo_tail < FIFO_LEN;
        if queued {
            self.fifo[self.fifo_head % FIFO_LEN] = frames as u16;
            self.fifo_head += 1;
        }
        self.cap_pos += frames as u64;
        queued
    }

    /// A capture callback gap long enough to have skipped playback microframes: let the
    /// smoothed error follow the raw one again before deciding.
    pub fn stalled(&mut self) {
        self.settle = LEAD_SETTLE;
    }

    /// Decides once per playback transfer whether to correct the lead. With implicit
    /// feedback the rates match, so the lead only moves after fallback packets or lost
    /// capture packets. Each correction shifts output timing by one frame, so they are
    /// limited to one per LEAD_HOLD transfers, and only when the smoothed error leaves
    /// a band of two packets. Returns a jump the caller must apply to the ring (zero the
    /// frames skipped when `frames` is negative); `play_pos` has already moved.
    pub fn plan_correction(&mut self) -> Option<Jump> {
        if self.cap_pos == 0 {
            return None;
        }
        let err = self.play_pos as i64 - self.cap_pos as i64 - self.target_lead;
        if self.settle > 0 {
            self.settle -= 1;
            self.lead_err_avg = err as f64;
            self.since_correction = 0;
            return None;
        }
        self.lead_err_avg += LEAD_ALPHA * (err as f64 - self.lead_err_avg);
        self.since_correction += 1;
        let per_packet = (self.rate / 8000) as f64;
        if self.since_correction < LEAD_HOLD {
            return None;
        }
        let startup = std::mem::take(&mut self.startup);
        if self.lead_err_avg.abs() > LEAD_JUMP_PACKETS * per_packet || (startup && self.lead_err_avg.round() != 0.0) {
            // A disturbance moved the lead by many frames at once, and the output has
            // already jumped, or the stream just started. Fix it in one step: skip ahead
            // in the playback ring (zeroing what is skipped) or step back (replaying
            // zeroed frames, a short silence). One glitch instead of one per correction.
            let frames = self.lead_err_avg.round() as i64;
            let jump = Jump { from: self.play_pos, frames, err, smoothed: self.lead_err_avg };
            self.play_pos = (self.play_pos as i64 - frames) as u64;
            self.lead_err_avg -= frames as f64;
            self.since_correction = 0;
            return Some(jump);
        }
        if self.lead_err_avg.abs() > LEAD_BAND_PACKETS * per_packet {
            self.correction = if self.lead_err_avg > 0.0 { -1 } else { 1 };
            self.since_correction = 0;
        }
        None
    }

    /// Size of the next playback packet: the size of the matching capture packet
    /// (implicit feedback), plus a pending lead correction. The caller sends that many
    /// frames from `play_pos` and then calls `played`.
    pub fn next_packet(&mut self) -> Packet {
        let fallback = self.fifo_tail == self.fifo_head;
        let mut n = if fallback {
            self.nominal_acc += self.rate as f64 / 8000.0;
            let n = self.nominal_acc as usize;
            self.nominal_acc -= n as f64;
            n
        } else {
            let n = self.fifo[self.fifo_tail % FIFO_LEN] as usize;
            self.fifo_tail += 1;
            n
        };
        let correction = std::mem::take(&mut self.correction);
        match correction {
            -1 => n = n.saturating_sub(1),
            1 => n += 1,
            _ => {}
        }
        Packet { frames: n.clamp(1, self.max_frames), fallback, correction }
    }

    pub fn played(&mut self, frames: usize) {
        self.play_pos += frames as u64;
    }

    /// Playback position minus capture position minus the target.
    #[cfg(test)]
    pub fn lead_error(&self) -> i64 {
        self.play_pos as i64 - self.cap_pos as i64 - self.target_lead
    }
}

/// Host time per device frame, tracked with a second-order delay-locked loop. Capture
/// completions arrive every 1 ms with scheduling jitter.
pub struct Clock {
    init: bool,
    // A trusted time this far from the prediction is either a late wake-up (ignored) or,
    // if it persists for RESYNC_AFTER callbacks, a jump of sample time itself (capture
    // packets lost in a long stall): then re-anchor instead of smoothing.
    resync_ticks: f64,
    // Consecutive trusted callbacks beyond resync_ticks.
    far: u32,
    pub anchor_sample: u64,
    pub anchor_host: f64,
    pub ticks_per_frame: f64,
}

impl Clock {
    /// `ticks_per_frame` is the nominal value: host ticks per second / sample rate.
    pub fn new(rate: u32, ticks_per_frame: f64) -> Self {
        let resync_ticks = ticks_per_frame * rate as f64 * 0.002;
        Self { init: false, resync_ticks, far: 0, anchor_sample: 0, anchor_host: 0.0, ticks_per_frame }
    }

    /// Feeds the capture position at host time `now` (ticks). Returns true when the
    /// anchor changed and should be published. An untrusted `now` (a callback after a
    /// stall, or one of the backlog drained right after it) arrived late by an unknown
    /// amount: the anchor moves by the prediction alone. Feeding those times in moved
    /// the estimate by about 40 frames after a 20 ms stall and took 2 s to settle.
    pub fn update(&mut self, cap_pos: u64, now: f64, trusted: bool) -> bool {
        if !self.init {
            if cap_pos == 0 {
                return false;
            }
            self.init = true;
            self.anchor_sample = cap_pos;
            self.anchor_host = now;
            return true;
        }
        let frames = cap_pos as f64 - self.anchor_sample as f64;
        if frames <= 0.0 {
            return false;
        }
        let predicted = self.anchor_host + frames * self.ticks_per_frame;
        let mut err = if trusted { now - predicted } else { 0.0 };
        self.anchor_sample = cap_pos;
        if err.abs() > self.resync_ticks {
            self.far += 1;
            if self.far >= RESYNC_AFTER {
                self.far = 0;
                self.anchor_host = now;
                return true;
            }
            // One late wake-up says nothing about the device: ignore it. Re-anchoring on
            // it jumped timestamps by about 100 frames and made Core Audio skip IO.
            err = 0.0;
        } else {
            self.far = 0;
        }
        self.anchor_host = predicted + DLL_B * err;
        self.ticks_per_frame += DLL_C * err / frames;
        true
    }

    /// Whether a capture callback that came `gap` after the previous one carries a
    /// usable time. Completions arrive every 1 ms; over 3 ms is a stall, and under
    /// 0.5 ms is the backlog draining after one.
    pub fn trusted_gap(gap: std::time::Duration) -> bool {
        (500..=3000).contains(&gap.as_micros())
    }

    /// Host time the loop currently assigns to `sample`.
    #[cfg(test)]
    pub fn host_time_of(&self, sample: u64) -> f64 {
        self.anchor_host + (sample as f64 - self.anchor_sample as f64) * self.ticks_per_frame
    }
}

#[cfg(test)]
mod tests {
    //! A simulated device and event thread around `Pacer` and `Clock`.
    //!
    //! Time runs in 125 us microframes. The device captures a packet every microframe
    //! (sizes follow the rate's 1/8000 pattern) and plays whatever playback packet is
    //! scheduled for that microframe. Transfers of 8 packets complete at the end of their
    //! last microframe; the event thread handles completions at the next microframe
    //! boundary, or all at once when a stall ends. A playback resubmit lands right after
    //! the last queued transfer, but never sooner than 4 ms ahead, as libusb does.
    //!
    //! Round trip: a playback packet whose first frame is ring position p, played in the
    //! microframe where the device has captured c frames, comes back at capture position
    //! c plus a fixed device delay. So c - p is the round trip minus that constant, and
    //! it must not move once the lead has settled.

    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    fn clock_trusted(gap_us: f64) -> bool {
        Clock::trusted_gap(Duration::from_micros(gap_us as u64))
    }

    const MAX_FRAMES: usize = 13;
    const PACKETS: u64 = 8;
    const OUT_XFERS: u64 = 16;
    const IN_XFERS: u64 = 24;
    // Capture callback gap that counts as a stall (STALL_RESYNC_US in stream.rs).
    const STALL_MF: u64 = 80;
    const SUBMIT_AHEAD: u64 = 32;
    const NS_PER_MF: f64 = 125_000.0;

    enum Event {
        // Packet sizes and the microframe after the transfer's last one.
        Capture(Vec<usize>, u64),
        Playback,
    }

    struct Sim {
        rate: u32,
        pace: Pacer,
        clock: Clock,
        mf: u64,
        acc: u64,
        // Queued capture transfers (first microframe of each) and the next free slot.
        cap_sched: VecDeque<u64>,
        cap_next_free: u64,
        // Microframe of the last capture callback, for stall detection.
        last_cap_cb: u64,
        cap_count: u64,
        cap_batch: Vec<usize>,
        // Capture sizes the device sends before its normal pattern (stale packets).
        stale: VecDeque<usize>,
        events: VecDeque<Event>,
        stalled_until: u64,
        // Scheduling jitter: the event thread sometimes wakes up to 3.5 ms late, as the
        // daemon's thread does (callback gaps of 1.5 to 5.7 ms seen on 2026-10-25).
        jitter: bool,
        late_until: u64,
        rng: u64,
        // Queued playback packets: (microframe, ring position of the first frame).
        sched: VecDeque<(u64, u64)>,
        xfer_ends: VecDeque<u64>,
        next_free: u64,
        rt: Vec<(u64, i64)>,
        jumps: Vec<Jump>,
        corrections: u32,
        // Clock error in frames at each capture completion: (microframe, error).
        clock_err: Vec<(u64, f64)>,
    }

    impl Sim {
        /// Starts a stream with capture and playback transfers first scheduled at the
        /// given microframes (libusb puts them at least 4 ms ahead).
        fn start_at(rate: u32, cap_start: u64, play_start: u64) -> Self {
            let mut s = Sim {
                rate,
                pace: Pacer::new(rate, (OUT_XFERS * PACKETS) as usize, MAX_FRAMES),
                clock: Clock::new(rate, 1e9 / rate as f64),
                mf: 0,
                acc: 0,
                cap_sched: VecDeque::new(),
                cap_next_free: cap_start,
                last_cap_cb: 0,
                cap_count: 0,
                cap_batch: Vec::new(),
                stale: VecDeque::new(),
                events: VecDeque::new(),
                stalled_until: 0,
                jitter: false,
                late_until: 0,
                rng: 0x2545F4914F6CDD1D,
                sched: VecDeque::new(),
                xfer_ends: VecDeque::new(),
                next_free: play_start,
                rt: Vec::new(),
                jumps: Vec::new(),
                corrections: 0,
                clock_err: Vec::new(),
            };
            for _ in 0..OUT_XFERS {
                s.submit_playback();
            }
            for _ in 0..IN_XFERS {
                s.submit_capture();
            }
            s
        }

        fn start(rate: u32) -> Self {
            Self::start_at(rate, SUBMIT_AHEAD, SUBMIT_AHEAD)
        }

        fn submit_capture(&mut self) {
            let start = self.cap_next_free.max(self.mf + SUBMIT_AHEAD);
            self.cap_sched.push_back(start);
            self.cap_next_free = start + PACKETS;
        }

        fn submit_playback(&mut self) {
            if let Some(j) = self.pace.plan_correction() {
                self.jumps.push(j);
            }
            let start = self.next_free.max(self.mf + SUBMIT_AHEAD);
            for k in 0..PACKETS {
                let p = self.pace.next_packet();
                if p.correction != 0 {
                    self.corrections += 1;
                }
                self.sched.push_back((start + k, self.pace.play_pos));
                self.pace.played(p.frames);
            }
            self.xfer_ends.push_back(start + PACKETS - 1);
            self.next_free = start + PACKETS;
        }

        fn step(&mut self) {
            let m = self.mf;
            let n = match self.stale.pop_front() {
                Some(n) => n,
                None => {
                    self.acc += self.rate as u64;
                    let n = (self.acc / 8000) as usize;
                    self.acc %= 8000;
                    n
                }
            };
            if self.sched.front().is_some_and(|&(at, _)| at == m) {
                let (_, p) = self.sched.pop_front().unwrap();
                self.rt.push((m, self.cap_count as i64 - p as i64));
            }
            // A microframe no capture transfer covers is lost: the host never sees it.
            if self.cap_sched.front().is_some_and(|&at| at <= m) {
                self.cap_count += n as u64;
                self.cap_batch.push(n);
                if m + 1 == self.cap_sched[0] + PACKETS {
                    self.cap_sched.pop_front();
                    self.events.push_back(Event::Capture(std::mem::take(&mut self.cap_batch), m + 1));
                }
            }
            if self.xfer_ends.front() == Some(&m) {
                self.xfer_ends.pop_front();
                self.events.push_back(Event::Playback);
            }
            self.mf += 1;
            if self.jitter && self.mf >= self.late_until {
                self.rng ^= self.rng << 13;
                self.rng ^= self.rng >> 7;
                self.rng ^= self.rng << 17;
                if self.rng % 40 == 0 {
                    self.late_until = self.mf + 1 + self.rng / 40 % 28;
                }
            }
            if self.mf >= self.stalled_until && self.mf >= self.late_until {
                while let Some(e) = self.events.pop_front() {
                    match e {
                        Event::Capture(sizes, end) => {
                            let gap = self.mf - self.last_cap_cb;
                            if self.last_cap_cb != 0 && gap > STALL_MF {
                                self.pace.stalled();
                            }
                            self.last_cap_cb = self.mf;
                            let trusted = clock_trusted(gap as f64 * NS_PER_MF / 1000.0);
                            for n in sizes {
                                self.pace.capture_packet(n);
                            }
                            let now = self.mf as f64 * NS_PER_MF;
                            self.clock.update(self.pace.cap_pos, now, trusted);
                            let truth = end as f64 * NS_PER_MF;
                            let err = (self.clock.host_time_of(self.pace.cap_pos) - truth) / (1e9 / self.rate as f64);
                            self.clock_err.push((self.mf, err));
                            self.submit_capture();
                        }
                        Event::Playback => self.submit_playback(),
                    }
                }
            }
        }

        fn run_ms(&mut self, ms: u64) {
            for _ in 0..ms * 8 {
                self.step();
            }
        }

        fn stall_ms(&mut self, ms: u64) {
            self.stalled_until = self.mf + ms * 8;
        }

        /// Distinct round-trip values seen in microframes [from, to).
        fn rt_values(&self, from: u64, to: u64) -> Vec<i64> {
            let mut v: Vec<i64> = self.rt.iter().filter(|&&(m, _)| m >= from && m < to).map(|&(_, r)| r).collect();
            v.sort();
            v.dedup();
            v
        }

        fn ms(&self) -> u64 {
            self.mf / 8
        }
    }

    const RATES: [u32; 4] = [44100, 48000, 88200, 96000];

    #[test]
    fn lead_settles_with_one_exact_jump_after_start() {
        for rate in RATES {
            let mut s = Sim::start(rate);
            s.run_ms(2000);
            assert_eq!(s.jumps.len(), 1, "{rate} Hz: {:?}", s.jumps);
            assert_eq!(s.corrections, 0, "{rate} Hz");
            // Sampled once per transfer the error sits within a transfer of the target.
            assert!(s.pace.lead_error().abs() <= (rate / 1000) as i64 + 1, "{rate} Hz: {}", s.pace.lead_error());
        }
    }

    #[test]
    fn small_start_error_is_corrected_exactly() {
        // An error inside the correction band would otherwise stay, and the round trip
        // would land anywhere in the band after each start (2026-10-25, before the fix).
        for err in [-10i64, -3, 5, 11] {
            let mut p = Pacer::new(48000, 128, MAX_FRAMES);
            let mut jumps = Vec::new();
            for _ in 0..100 {
                // Capture and playback advance together, one transfer per call.
                for _ in 0..8 {
                    p.capture_packet(6);
                }
                if p.play_pos == 0 {
                    p.played((p.target_lead + p.cap_pos as i64 + err) as usize);
                }
                jumps.extend(p.plan_correction());
                for _ in 0..8 {
                    let n = p.next_packet().frames;
                    p.played(n);
                }
            }
            assert_eq!(jumps.len(), 1, "error {err}: {jumps:?}");
            assert_eq!(jumps[0].frames, err, "error {err}");
            assert_eq!(p.lead_error() % 48, 0, "error {err}: lead error {}", p.lead_error());
        }
    }

    #[test]
    fn round_trip_is_steady_after_start() {
        for rate in RATES {
            let mut s = Sim::start(rate);
            s.run_ms(3000);
            let v = s.rt_values(200 * 8, 3000 * 8);
            // 44.1 and 88.2 kHz packets alternate in size, so the value may move by one
            // frame with the pattern, never more.
            assert!(v.len() <= 2 && v[v.len() - 1] - v[0] <= 1, "{rate} Hz: {v:?}");
        }
    }

    #[test]
    fn round_trip_does_not_depend_on_start_phase() {
        // libusb on macOS schedules isochronous transfers in whole 1 ms frames, so the
        // two endpoints start an integer number of milliseconds apart. Within a
        // millisecond the lead measurement is biased by the capture transfer boundary
        // (up to one transfer); that case does not occur on this platform.
        for rate in RATES {
            let mut seen = Vec::new();
            for cap_ms in 0..4 {
                // 10: playback started after capture, as the daemon does (PLAYBACK_DELAY).
                for play_ms in [0, 1, 2, 3, 10, 13] {
                    let mut s = Sim::start_at(rate, SUBMIT_AHEAD + 8 * cap_ms, SUBMIT_AHEAD + 8 * play_ms);
                    s.run_ms(1000);
                    seen.push(s.rt_values(500 * 8, 1000 * 8)[0]);
                }
            }
            let (lo, hi) = (*seen.iter().min().unwrap(), *seen.iter().max().unwrap());
            assert!(hi - lo <= 1, "{rate} Hz: round trip over start phases {seen:?}");
        }
    }

    #[test]
    fn short_stall_changes_nothing() {
        let mut s = Sim::start(48000);
        s.run_ms(1000);
        let before = s.rt_values(500 * 8, 1000 * 8);
        let jumps = s.jumps.len();
        s.stall_ms(10);
        s.run_ms(1000);
        assert_eq!(s.jumps.len(), jumps);
        assert_eq!(s.corrections, 0);
        assert_eq!(s.rt_values(1000 * 8, 2000 * 8), before);
    }

    #[test]
    fn long_stall_recovers_with_one_jump() {
        for stall in [15, 20, 50, 100] {
            let mut s = Sim::start(48000);
            s.run_ms(1000);
            let before = s.rt_values(500 * 8, 1000 * 8);
            let jumps = s.jumps.len();
            s.stall_ms(stall);
            s.run_ms(2000);
            assert_eq!(s.jumps.len(), jumps + 1, "{stall} ms: {:?}", &s.jumps[jumps..]);
            assert_eq!(s.corrections, 0, "{stall} ms");
            let after = s.rt_values((s.ms() - 1000) * 8, s.ms() * 8);
            assert_eq!(after, before, "{stall} ms");
        }
    }

    #[test]
    fn stale_capture_sizes_after_a_rate_switch_do_not_shift_the_round_trip() {
        // A device still sending 96 kHz-sized packets for the first 2 ms after a switch
        // down to 48 kHz. Implicit feedback copies them, then the lead settles. If the
        // host side were the cause of the higher round trip seen after such switches
        // (timeline 2026-10-25), this would fail.
        let mut clean = Sim::start(48000);
        clean.run_ms(1000);
        let mut stale = Sim::start(48000);
        stale.stale.extend(std::iter::repeat(12).take(16));
        stale.run_ms(1000);
        assert_eq!(stale.rt_values(500 * 8, 1000 * 8), clean.rt_values(500 * 8, 1000 * 8));
    }

    #[test]
    fn clock_tracks_the_device_within_a_frame() {
        let mut s = Sim::start(48000);
        s.run_ms(10_000);
        let steady: Vec<f64> = s.clock_err.iter().filter(|&&(m, _)| m > 5000 * 8).map(|&(_, e)| e).collect();
        let (lo, hi) = steady.iter().fold((f64::MAX, f64::MIN), |(l, h), &e| (l.min(e), h.max(e)));
        assert!(hi - lo < 1.0, "clock error spread {lo:.3}..{hi:.3} frames");
    }

    #[test]
    fn clock_ignores_the_backlog_after_a_stall() {
        // Up to 24 ms the capture queue covers the stall and no frame is lost, so the
        // device clock did not move and neither may the estimate.
        for stall in [5, 12, 20] {
            let mut s = Sim::start(48000);
            s.run_ms(10_000);
            let base = s.clock_err.last().unwrap().1;
            s.stall_ms(stall);
            s.run_ms(5000);
            let worst = s
                .clock_err
                .iter()
                .filter(|&&(m, _)| m >= 10_000 * 8)
                .map(|&(_, e)| (e - base).abs())
                .fold(0.0, f64::max);
            assert!(worst < 1.0, "{stall} ms stall: clock moved {worst:.1} frames");
        }
    }

    #[test]
    fn clock_reanchors_when_capture_frames_were_lost() {
        // A 100 ms stall outlasts the capture queue: sample time skips the lost frames,
        // and the estimate must follow within a few callbacks.
        let mut s = Sim::start(48000);
        s.run_ms(10_000);
        let base = s.clock_err.last().unwrap().1;
        s.stall_ms(100);
        s.run_ms(5000);
        let worst = s
            .clock_err
            .iter()
            .filter(|&&(m, _)| m >= (10_000 + 150) * 8)
            .map(|&(_, e)| (e - base).abs())
            .fold(0.0, f64::max);
        assert!(worst < 1.0, "clock off by {worst:.1} frames after re-anchoring");
    }

    #[test]
    fn clock_ignores_callback_jitter() {
        // Late wake-ups of up to 3.5 ms, no frames lost: the device clock did not move,
        // so the estimate may not either. A late callback inside the trusted gap range
        // must not re-anchor the clock (that jumped timestamps by about 100 frames on
        // hardware and made Core Audio skip IO cycles, 2026-10-25).
        let mut s = Sim::start(48000);
        s.jitter = true;
        s.run_ms(20_000);
        let steady: Vec<f64> = s.clock_err.iter().filter(|&&(m, _)| m > 5000 * 8).map(|&(_, e)| e).collect();
        let (lo, hi) = steady.iter().fold((f64::MAX, f64::MIN), |(l, h), &e| (l.min(e), h.max(e)));
        // The clock before 2026-10-25 moved 17 frames here and still ran an 8-hour soak
        // clean; the re-anchor on a single late callback moved it 132.
        assert!(hi - lo < 6.0, "clock error spread {lo:.1}..{hi:.1} frames under jitter");
    }
}
