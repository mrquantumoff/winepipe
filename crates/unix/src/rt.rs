//! The realtime core: the RT <-> callback-thread handshake and the per-cycle
//! audio routing between PipeWire DSP buffers and ASIO double buffers.
//!
//! Nothing in here allocates, locks or logs on the RT path. It is independent
//! of PipeWire so it can be unit-tested natively.

use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering::*};

use winepipe_abi as abi;

use crate::futex;

pub const STATE_IDLE: u32 = 0;
pub const STATE_RUNNING: u32 = 1;
pub const STATE_STOPPING: u32 = 2;

/// Largest chunk the FIFO path processes at once (PipeWire's hard quantum
/// limit). Larger cycles are split.
pub const MAX_CHUNK: usize = 8192;

#[derive(Default)]
pub struct Stats {
    pub cycles: AtomicU64,
    pub switches: AtomicU64,
    pub late: AtomicU64,
    pub skipped: AtomicU64,
    pub underrun_frames: AtomicU64,
    pub handoff_max_ns: AtomicU64,
    pub handoff_last_ns: AtomicU64,
    pub handoff_total_ns: AtomicU64,
    pub graph_xruns: AtomicU64,
    pub quantum: AtomicU32,
    pub fifo_mode: AtomicU32,
    pub fifo_latency: AtomicU32,
    pub rt_priority: AtomicI32,
}

impl Stats {
    pub fn reset_counters(&self) {
        for c in [
            &self.cycles,
            &self.switches,
            &self.late,
            &self.skipped,
            &self.underrun_frames,
            &self.handoff_max_ns,
            &self.handoff_last_ns,
            &self.handoff_total_ns,
            &self.graph_xruns,
        ] {
            c.store(0, Relaxed);
        }
    }
}

/// Outcome of handing one block to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The host finished before the deadline; its output is valid.
    Done,
    /// The deadline passed. The host keeps the block; its output is dropped.
    Late,
    /// The driver is stopping.
    Aborted,
}

/// Shared state of the handshake.
///
/// `req_seq` is advanced only by the RT thread, `done_seq` only by the
/// callback thread. Sleepers never wait on those values directly: each side
/// has a *doorbell* futex word that every waker increments before waking.
/// A sleeper reads its bell before checking its conditions, so a wake that
/// happens in between changes the bell and the futex wait returns at once.
/// No wakeup can be lost, including the ones from `raise` and `halt`, which
/// change neither sequence number.
pub struct Handoff {
    pub state: AtomicU32,
    busy: AtomicU32,
    req_seq: AtomicU32,
    done_seq: AtomicU32,
    /// Doorbell of the callback thread (new block, event, stop).
    host_bell: AtomicU32,
    /// Doorbell of the RT thread (block done, stop).
    rt_bell: AtomicU32,
    /// Set once the callback thread waits for blocks; until then the RT side
    /// stays silent instead of issuing blocks nobody can take.
    armed: AtomicU32,
    events: AtomicU32,
    sw_index: AtomicU32,
    sw_rate: AtomicU32,
    sw_pos: AtomicU64,
    sw_nsec: AtomicU64,
    pub stats: Stats,
}

impl Default for Handoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Handoff {
    pub fn new() -> Handoff {
        Handoff {
            state: AtomicU32::new(STATE_IDLE),
            busy: AtomicU32::new(0),
            req_seq: AtomicU32::new(0),
            done_seq: AtomicU32::new(0),
            host_bell: AtomicU32::new(0),
            rt_bell: AtomicU32::new(0),
            armed: AtomicU32::new(0),
            events: AtomicU32::new(0),
            sw_index: AtomicU32::new(0),
            sw_rate: AtomicU32::new(0),
            sw_pos: AtomicU64::new(0),
            sw_nsec: AtomicU64::new(0),
            stats: Stats { rt_priority: AtomicI32::new(-libc::ENOSYS), ..Default::default() },
        }
    }

    pub fn running(&self) -> bool {
        self.state.load(Acquire) == STATE_RUNNING
    }

    #[inline]
    fn ring_host(&self, all: bool) {
        self.host_bell.fetch_add(1, Release);
        if all {
            futex::wake_all(&self.host_bell);
        } else {
            futex::wake(&self.host_bell, 1);
        }
    }

    #[inline]
    fn ring_rt(&self) {
        self.rt_bell.fetch_add(1, Release);
        futex::wake_all(&self.rt_bell);
    }

    /// Queue an event for the host and nudge the callback thread so it is
    /// delivered promptly. RT-safe (one futex syscall).
    pub fn raise(&self, ev: u32) {
        self.events.fetch_or(ev, AcqRel);
        self.ring_host(false);
    }

    /// True when the host has finished every block handed to it.
    #[inline]
    pub fn host_idle(&self) -> bool {
        self.done_seq.load(Acquire) == self.req_seq.load(Relaxed)
    }

    /// True once the callback thread of the current run is waiting for blocks.
    #[inline]
    pub fn armed(&self) -> bool {
        self.armed.load(Acquire) != 0
    }

    // ---- RT thread side -------------------------------------------------

    /// Mark the start of a `process()` call. Returns false (and nothing may
    /// touch the session) unless the driver is running.
    #[inline]
    pub fn enter(&self) -> bool {
        self.busy.store(1, SeqCst);
        if self.state.load(SeqCst) == STATE_RUNNING {
            true
        } else {
            self.busy.store(0, Release);
            false
        }
    }

    #[inline]
    pub fn leave(&self) {
        self.busy.store(0, Release);
    }

    /// Hand block `index` to the host and wait (until `deadline_ns`, absolute
    /// CLOCK_MONOTONIC) for it to finish. Must only be called when
    /// [`host_idle`](Self::host_idle) is true.
    pub fn issue(&self, index: u32, pos: u64, nsec: u64, rate: u32, deadline_ns: u64) -> Outcome {
        self.sw_index.store(index, Relaxed);
        self.sw_pos.store(pos, Relaxed);
        self.sw_nsec.store(nsec, Relaxed);
        self.sw_rate.store(rate, Relaxed);
        let seq = self.req_seq.load(Relaxed).wrapping_add(1);
        let t0 = futex::now_ns();
        self.req_seq.store(seq, Release);
        self.ring_host(false);
        self.stats.switches.fetch_add(1, Relaxed);

        loop {
            let bell = self.rt_bell.load(Acquire);
            if self.done_seq.load(Acquire) == seq {
                let dt = futex::now_ns().saturating_sub(t0);
                self.stats.handoff_last_ns.store(dt, Relaxed);
                self.stats.handoff_total_ns.fetch_add(dt, Relaxed);
                self.stats.handoff_max_ns.fetch_max(dt, Relaxed);
                return Outcome::Done;
            }
            if self.state.load(Acquire) != STATE_RUNNING {
                return Outcome::Aborted;
            }
            if !futex::wait(&self.rt_bell, bell, deadline_ns) && self.done_seq.load(Acquire) != seq {
                self.stats.late.fetch_add(1, Relaxed);
                return Outcome::Late;
            }
        }
    }

    // ---- callback thread side --------------------------------------------

    /// Report block `last_done` finished, then block until the next block,
    /// an event, a stop, or `timeout_ms`.
    pub fn wait(&self, p: &mut abi::WaitParams) {
        // The caller reuses its params; outputs must not carry over.
        p.events = 0;
        p.result = 0;
        self.complete(p.last_done);
        if self.armed.swap(1, AcqRel) == 0 {
            // First wait of this run: blocks may flow from now on.
            self.ring_host(false);
        }
        let deadline = futex::now_ns() + u64::from(p.timeout_ms.max(1)) * 1_000_000;
        loop {
            let bell = self.host_bell.load(Acquire);
            let req = self.req_seq.load(Acquire);
            if req != p.last_done {
                p.result = abi::WAIT_SWITCH;
                p.seq = req;
                p.index = self.sw_index.load(Relaxed);
                p.sample_position = self.sw_pos.load(Relaxed);
                p.system_nsec = self.sw_nsec.load(Relaxed);
                p.sample_rate = f64::from(self.sw_rate.load(Relaxed));
                p.events = self.events.swap(0, AcqRel);
                break;
            }
            let ev = self.events.swap(0, AcqRel);
            if ev != 0 {
                p.result = abi::WAIT_TIMEOUT;
                p.events = ev;
                break;
            }
            if self.state.load(Acquire) != STATE_RUNNING {
                p.result = abi::WAIT_STOPPED;
                break;
            }
            if !futex::wait(&self.host_bell, bell, deadline) {
                p.result = abi::WAIT_TIMEOUT;
                break;
            }
        }
        p.now_nsec = futex::now_ns();
    }

    /// `ASIOOutputReady()`: release the RT thread as soon as outputs are
    /// written, before the host returns from bufferSwitch.
    pub fn complete(&self, seq: u32) {
        if self.done_seq.load(Relaxed) != seq && self.req_seq.load(Acquire) == seq {
            self.done_seq.store(seq, Release);
            self.ring_rt();
        }
    }

    // ---- control side -----------------------------------------------------

    /// Switch to RUNNING. Returns the sequence number the callback thread
    /// starts from. A pending reset request survives: the host must still
    /// learn that the connection broke.
    pub fn begin(&self) -> u32 {
        let seq = self.req_seq.load(Acquire);
        self.done_seq.store(seq, Release);
        self.events.fetch_and(abi::EVENT_RESET_REQUEST, AcqRel);
        self.armed.store(0, Release);
        self.stats.reset_counters();
        self.state.store(STATE_RUNNING, SeqCst);
        seq
    }

    /// Stop handing out blocks and wait until no `process()` call can still
    /// be using the session. Bounded by one cycle deadline.
    pub fn halt(&self) {
        if self.state.swap(STATE_STOPPING, SeqCst) == STATE_IDLE {
            self.state.store(STATE_IDLE, SeqCst);
            return;
        }
        self.ring_rt();
        self.ring_host(true);
        let give_up = futex::now_ns() + 3_000_000_000;
        while self.busy.load(SeqCst) != 0 && futex::now_ns() < give_up {
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
        self.state.store(STATE_IDLE, SeqCst);
        self.armed.store(0, Release);
        self.ring_host(true);
    }
}

/// One host-visible channel: the PipeWire port it maps to and its two ASIO
/// half-buffers (allocated by the PE side, `bufsize` samples each).
#[derive(Clone, Copy)]
pub struct Chan {
    pub port: usize,
    pub buf: [*mut u8; 2],
}

/// Timing of the current PipeWire cycle.
#[derive(Clone, Copy, Debug)]
pub struct Cycle {
    pub n: usize,
    pub rate: u32,
    /// CLOCK_MONOTONIC start of the cycle.
    pub nsec: u64,
    /// Driver's estimate of the next cycle start (0 if unknown).
    pub next_nsec: u64,
    pub freewheel: bool,
}

impl Cycle {
    fn period_ns(&self) -> u64 {
        if self.rate == 0 {
            return 0;
        }
        self.n as u64 * 1_000_000_000 / u64::from(self.rate)
    }

    /// Latest time the host may finish this cycle's blocks.
    pub fn deadline(&self, now: u64) -> u64 {
        let period = self.period_ns();
        if self.freewheel || period == 0 {
            return now + 2_000_000_000;
        }
        let lo = now + period / 8;
        let hi = now + period;
        let target = if self.next_nsec > now { self.next_nsec - period / 16 } else { hi };
        target.clamp(lo, hi)
    }

    fn frame_time(&self, frame: i64) -> u64 {
        if self.rate == 0 {
            return self.nsec;
        }
        let off = frame * 1_000_000_000 / i64::from(self.rate);
        (self.nsec as i64).saturating_add(off).max(0) as u64
    }
}

pub fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Extra output delay (frames) needed when the graph cycle is `n` and the
/// ASIO buffer `b`: the minimal FIFO pre-fill that can never underrun.
pub fn fifo_prefill(n: usize, b: usize) -> usize {
    if n.is_multiple_of(b) {
        0
    } else {
        b - gcd(n, b)
    }
}

/// Everything that exists between createBuffers() and disposeBuffers().
pub struct Session {
    pub bufsize: usize,
    pub sample_type: u32,
    pub ins: Vec<Chan>,
    pub outs: Vec<Chan>,
    /// Output ports with no active ASIO channel (always silent).
    pub idle_outs: Vec<usize>,
    index: u32,
    pos: u64,
    last_n: usize,
    fifo: bool,
    prefill: usize,
    /// Latency (frames) the host was last told about, per direction.
    reported_latency: usize,
    mask: usize,
    in_ring: Vec<Vec<f32>>,
    out_ring: Vec<Vec<f32>>,
    in_rd: usize,
    in_len: usize,
    out_rd: usize,
    out_len: usize,
}

// SAFETY: the raw buffer pointers are owned by the PE side for the session's
// lifetime, and the session is only touched by one thread at a time (see
// Handoff::enter/halt).
unsafe impl Send for Session {}

impl Session {
    pub fn new(bufsize: usize, sample_type: u32, ins: Vec<Chan>, outs: Vec<Chan>, idle_outs: Vec<usize>) -> Session {
        let cap = (2 * bufsize + MAX_CHUNK).next_power_of_two();
        Session {
            bufsize,
            sample_type,
            in_ring: ins.iter().map(|_| vec![0.0; cap]).collect(),
            out_ring: outs.iter().map(|_| vec![0.0; cap]).collect(),
            ins,
            outs,
            idle_outs,
            index: 0,
            pos: 0,
            last_n: 0,
            fifo: false,
            prefill: 0,
            reported_latency: bufsize,
            mask: cap - 1,
            in_rd: 0,
            in_len: 0,
            out_rd: 0,
            out_len: 0,
        }
    }

    /// Called (non-RT) before every start.
    pub fn reset(&mut self) {
        self.index = 0;
        self.pos = 0;
        self.last_n = 0;
        self.fifo = false;
        self.prefill = 0;
        // Before start the host is told `bufsize` (the quantum we force).
        self.reported_latency = self.bufsize;
        self.in_rd = 0;
        self.in_len = 0;
        self.out_rd = 0;
        self.out_len = 0;
    }

    #[allow(dead_code)]
    pub fn prefill(&self) -> usize {
        self.prefill
    }

    fn configure(&mut self, n: usize, h: &Handoff) {
        self.last_n = n;
        let p = fifo_prefill(n, self.bufsize);
        // Matches Engine::get_info: max(quantum, buffer) (+ FIFO pre-fill).
        let latency = n.max(self.bufsize) + p;
        if latency != self.reported_latency {
            self.reported_latency = latency;
            h.raise(abi::EVENT_LATENCY_CHANGED);
        }
        self.fifo = p != 0;
        self.prefill = p;
        self.in_rd = 0;
        self.in_len = 0;
        self.out_rd = 0;
        self.out_len = p;
        for r in &mut self.out_ring {
            r[..p].fill(0.0);
        }
        h.stats.quantum.store(n as u32, Relaxed);
        h.stats.fifo_mode.store(self.fifo as u32, Relaxed);
        h.stats.fifo_latency.store(p as u32, Relaxed);
    }

    /// Run one PipeWire cycle. `inp`/`out` hold one DSP buffer pointer per
    /// PipeWire port (null = unavailable), each valid for `cyc.n` samples.
    ///
    /// # Safety
    /// All non-null pointers must be valid for `cyc.n` f32s, and the ASIO
    /// buffers for `bufsize` samples, for the duration of the call.
    pub unsafe fn process(&mut self, h: &Handoff, inp: &[*const f32], out: &[*mut f32], cyc: &Cycle) {
        let n = cyc.n;
        if n == 0 {
            return;
        }
        if !h.armed() {
            // The callback thread is not up yet: issuing a block now would
            // only run into the deadline. Stay silent until it is.
            for c in &self.outs {
                zero(out.get(c.port).copied().unwrap_or(std::ptr::null_mut()), 0, n);
            }
            for &p in &self.idle_outs {
                zero(out.get(p).copied().unwrap_or(std::ptr::null_mut()), 0, n);
            }
            return;
        }
        if n != self.last_n {
            self.configure(n, h);
        }
        let deadline = cyc.deadline(futex::now_ns());
        if self.fifo {
            self.process_fifo(h, inp, out, cyc, deadline);
        } else {
            self.process_direct(h, inp, out, cyc, deadline);
        }
        for &p in &self.idle_outs {
            zero(out.get(p).copied().unwrap_or(std::ptr::null_mut()), 0, n);
        }
    }

    /// n == k * bufsize: k back-to-back switches inside this cycle; no
    /// buffering, no added latency.
    unsafe fn process_direct(&mut self, h: &Handoff, inp: &[*const f32], out: &[*mut f32], cyc: &Cycle, deadline: u64) {
        let b = self.bufsize;
        let st = self.sample_type;
        for blk in 0..cyc.n / b {
            let off = blk * b;
            let mut ok = false;
            if h.host_idle() {
                let idx = self.index as usize;
                for c in &self.ins {
                    let src = inp.get(c.port).copied().unwrap_or(std::ptr::null());
                    let src = if src.is_null() { None } else { Some(std::slice::from_raw_parts(src.add(off), b)) };
                    to_asio(c.buf[idx], 0, src, b, st);
                }
                let t = cyc.frame_time(off as i64);
                let r = h.issue(self.index, self.pos, t, cyc.rate, deadline);
                self.index ^= 1;
                if r == Outcome::Done {
                    for c in &self.outs {
                        let dst = out.get(c.port).copied().unwrap_or(std::ptr::null_mut());
                        if !dst.is_null() {
                            from_asio(c.buf[idx], 0, std::slice::from_raw_parts_mut(dst.add(off), b), st);
                        }
                    }
                    ok = true;
                }
            } else {
                h.stats.skipped.fetch_add(1, Relaxed);
            }
            if !ok {
                for c in &self.outs {
                    zero(out.get(c.port).copied().unwrap_or(std::ptr::null_mut()), off, b);
                }
            }
            self.pos += b as u64;
        }
    }

    /// Mismatched quantum: ring buffers with a minimal output pre-fill.
    unsafe fn process_fifo(&mut self, h: &Handoff, inp: &[*const f32], out: &[*mut f32], cyc: &Cycle, deadline: u64) {
        let b = self.bufsize;
        let st = self.sample_type;
        let mask = self.mask;
        let mut done = 0;
        while done < cyc.n {
            let chunk = (cyc.n - done).min(MAX_CHUNK);

            let wr = (self.in_rd + self.in_len) & mask;
            for (k, c) in self.ins.iter().enumerate() {
                let src = inp.get(c.port).copied().unwrap_or(std::ptr::null());
                let src = if src.is_null() { None } else { Some(std::slice::from_raw_parts(src.add(done), chunk)) };
                ring_put(&mut self.in_ring[k], wr, src, chunk);
            }
            self.in_len += chunk;

            while self.in_len >= b {
                // frame offset (within this cycle) of the block's first frame
                let first = (done + chunk) as i64 - self.in_len as i64;
                let wr = (self.out_rd + self.out_len) & mask;
                let mut ok = false;
                if h.host_idle() {
                    let idx = self.index as usize;
                    for (k, c) in self.ins.iter().enumerate() {
                        ring_to_asio(&self.in_ring[k], self.in_rd, c.buf[idx], b, st);
                    }
                    let r = h.issue(self.index, self.pos, cyc.frame_time(first), cyc.rate, deadline);
                    self.index ^= 1;
                    if r == Outcome::Done {
                        for (k, c) in self.outs.iter().enumerate() {
                            asio_to_ring(c.buf[idx], &mut self.out_ring[k], wr, b, st);
                        }
                        ok = true;
                    }
                } else {
                    h.stats.skipped.fetch_add(1, Relaxed);
                }
                if !ok {
                    for r in &mut self.out_ring {
                        ring_put(r, wr, None, b);
                    }
                }
                self.in_rd = (self.in_rd + b) & mask;
                self.in_len -= b;
                self.out_len += b;
                self.pos += b as u64;
            }

            let take = chunk.min(self.out_len);
            for (k, c) in self.outs.iter().enumerate() {
                let dst = out.get(c.port).copied().unwrap_or(std::ptr::null_mut());
                if dst.is_null() {
                    continue;
                }
                ring_get(&self.out_ring[k], self.out_rd, std::slice::from_raw_parts_mut(dst.add(done), take));
                zero(dst, done + take, chunk - take);
            }
            if take < chunk {
                h.stats.underrun_frames.fetch_add((chunk - take) as u64, Relaxed);
            }
            self.out_rd = (self.out_rd + take) & mask;
            self.out_len -= take;
            done += chunk;
        }
    }
}

unsafe fn zero(p: *mut f32, off: usize, len: usize) {
    if !p.is_null() && len > 0 {
        std::ptr::write_bytes(p.add(off), 0, len);
    }
}

#[inline]
fn f32_to_i32(x: f32) -> i32 {
    // `as` saturates; 2^31 * 1.0 lands on i32::MAX.
    (x * 2_147_483_648.0) as i32
}

#[inline]
fn i32_to_f32(x: i32) -> f32 {
    x as f32 * (1.0 / 2_147_483_648.0)
}

/// Write `len` frames (or silence) into an ASIO buffer at frame `off`.
unsafe fn to_asio(dst: *mut u8, off: usize, src: Option<&[f32]>, len: usize, st: u32) {
    if st == abi::SAMPLE_INT32 {
        let d = std::slice::from_raw_parts_mut((dst as *mut i32).add(off), len);
        match src {
            Some(s) => d.iter_mut().zip(s).for_each(|(d, &s)| *d = f32_to_i32(s)),
            None => d.fill(0),
        }
    } else {
        let d = std::slice::from_raw_parts_mut((dst as *mut f32).add(off), len);
        match src {
            Some(s) => d.copy_from_slice(&s[..len]),
            None => d.fill(0.0),
        }
    }
}

/// Read `dst.len()` frames out of an ASIO buffer starting at frame `off`.
unsafe fn from_asio(src: *const u8, off: usize, dst: &mut [f32], st: u32) {
    let len = dst.len();
    if st == abi::SAMPLE_INT32 {
        let s = std::slice::from_raw_parts((src as *const i32).add(off), len);
        dst.iter_mut().zip(s).for_each(|(d, &s)| *d = i32_to_f32(s));
    } else {
        dst.copy_from_slice(std::slice::from_raw_parts((src as *const f32).add(off), len));
    }
}

fn ring_put(ring: &mut [f32], at: usize, src: Option<&[f32]>, len: usize) {
    let first = len.min(ring.len() - at);
    match src {
        Some(s) => {
            ring[at..at + first].copy_from_slice(&s[..first]);
            ring[..len - first].copy_from_slice(&s[first..len]);
        }
        None => {
            ring[at..at + first].fill(0.0);
            ring[..len - first].fill(0.0);
        }
    }
}

fn ring_get(ring: &[f32], at: usize, dst: &mut [f32]) {
    let len = dst.len();
    let first = len.min(ring.len() - at);
    dst[..first].copy_from_slice(&ring[at..at + first]);
    dst[first..].copy_from_slice(&ring[..len - first]);
}

unsafe fn ring_to_asio(ring: &[f32], at: usize, dst: *mut u8, len: usize, st: u32) {
    let first = len.min(ring.len() - at);
    to_asio(dst, 0, Some(&ring[at..at + first]), first, st);
    to_asio(dst, first, Some(&ring[..len - first]), len - first, st);
}

unsafe fn asio_to_ring(src: *const u8, ring: &mut [f32], at: usize, len: usize, st: u32) {
    let first = len.min(ring.len() - at);
    from_asio(src, 0, &mut ring[at..at + first], st);
    from_asio(src, first, &mut ring[..len - first], st);
}

#[cfg(test)]
mod tests;
