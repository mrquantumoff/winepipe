//! winepipe-test: a minimal ASIO host for exercising WinePipe under Wine.
//!
//! ```text
//! wine winepipe-test.exe [--buffer N] [--seconds S] [--sine] [--loopback]
//!                        [--output-ready] [--no-timeinfo] [--rate R] [--stall-ms M]
//! ```
//!
//! It loads the driver through COM like a DAW does, runs it, and prints the
//! callback jitter plus the driver's RT statistics. `--loopback` emits a
//! click on out_1 and times its arrival on in_1. Link the two first, e.g.
//! `pw-link WinePipe:out_1 WinePipe:in_1`, to measure the round-trip
//! latency in samples.

#![allow(non_snake_case, non_upper_case_globals)]

#[path = "../../pe/src/asio.rs"]
mod asio;

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering::*};

use asio::*;
use winepipe_abi as abi;

#[repr(C)]
#[derive(Clone, Copy)]
struct GUID {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

const CLSID_WINEPIPE: GUID = GUID { d1: 0xBE5F_BE6F, d2: 0x8E42, d3: 0x4F6E, d4: [0x89, 0xE6, 0x22, 0x25, 0x02, 0x1E, 0x59, 0x29] };

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
    fn CoCreateInstance(clsid: *const GUID, outer: *mut c_void, ctx: u32, iid: *const GUID, out: *mut *mut c_void) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(v: *mut i64) -> i32;
    fn QueryPerformanceFrequency(v: *mut i64) -> i32;
    fn Sleep(ms: u32);
}

fn qpc() -> i64 {
    let mut v = 0;
    unsafe { QueryPerformanceCounter(&mut v) };
    v
}

#[cfg(target_arch = "x86")]
macro_rules! asio_fn { ($($t:ty),* => $r:ty) => { unsafe extern "thiscall" fn(*mut c_void $(, $t)*) -> $r } }
#[cfg(not(target_arch = "x86"))]
macro_rules! asio_fn { ($($t:ty),* => $r:ty) => { unsafe extern "system" fn(*mut c_void $(, $t)*) -> $r } }

/// Thin wrapper calling IASIO through its vtable.
struct Driver(*mut c_void);

macro_rules! method {
    ($name:ident, $idx:expr, ($($a:ident: $t:ty),*) -> $r:ty) => {
        fn $name(&self $(, $a: $t)*) -> $r {
            unsafe {
                let vt = *(self.0 as *const *const usize);
                let f: asio_fn!($($t),* => $r) = std::mem::transmute(*vt.add($idx));
                f(self.0 $(, $a)*)
            }
        }
    };
}

impl Driver {
    method!(init, 3, (sys: *mut c_void) -> i32);
    method!(get_driver_name, 4, (name: *mut u8) -> ());
    method!(get_driver_version, 5, () -> i32);
    method!(get_error_message, 6, (s: *mut u8) -> ());
    method!(start, 7, () -> i32);
    method!(stop, 8, () -> i32);
    method!(get_channels, 9, (i: *mut i32, o: *mut i32) -> i32);
    method!(get_latencies, 10, (i: *mut i32, o: *mut i32) -> i32);
    method!(get_buffer_size, 11, (a: *mut i32, b: *mut i32, c: *mut i32, d: *mut i32) -> i32);
    method!(can_sample_rate, 12, (r: f64) -> i32);
    method!(get_sample_rate, 13, (r: *mut f64) -> i32);
    method!(set_sample_rate, 14, (r: f64) -> i32);
    method!(get_sample_position, 17, (p: *mut ASIOSamples, t: *mut ASIOSamples) -> i32);
    method!(get_channel_info, 18, (i: *mut ASIOChannelInfo) -> i32);
    method!(create_buffers, 19, (i: *mut ASIOBufferInfo, n: i32, s: i32, c: *mut ASIOCallbacks) -> i32);
    method!(dispose_buffers, 20, () -> i32);
    method!(control_panel, 21, () -> i32);
    method!(future, 22, (sel: i32, opt: *mut c_void) -> i32);
    method!(output_ready, 23, () -> i32);

    fn release(&self) -> u32 {
        unsafe {
            let vt = *(self.0 as *const *const usize);
            let f: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vt.add(2));
            f(self.0)
        }
    }

    fn error(&self) -> String {
        let mut buf = [0u8; 124];
        self.get_error_message(buf.as_mut_ptr());
        abi::get_str(&buf).to_string()
    }
}

/// State touched from the callback (single writer: the driver's thread).
struct Host {
    driver: *mut c_void,
    bufsize: usize,
    ins: Vec<[*mut f32; 2]>,
    outs: Vec<[*mut f32; 2]>,
    int32: bool,
    sine: bool,
    loopback: bool,
    output_ready: bool,
    stall_ms: u32,
    phase: f64,
    rate: f64,
    stamps: Vec<i64>,
    frames: u64,
    click_at: Option<u64>,
    last_click: u64,
    latencies: Vec<u64>,
    peak: f32,
}

struct Global(UnsafeCell<Option<Host>>);
unsafe impl Sync for Global {}
static HOST: Global = Global(UnsafeCell::new(None));
static CALLS: AtomicUsize = AtomicUsize::new(0);
static TIMEINFO: AtomicBool = AtomicBool::new(true);
static RESETS: AtomicU32 = AtomicU32::new(0);
static LAT_CHANGES: AtomicU32 = AtomicU32::new(0);
static RATE_CHANGES: AtomicU32 = AtomicU32::new(0);
static SAMPLE_POS_ERRORS: AtomicU32 = AtomicU32::new(0);
static LAST_POS: AtomicI64 = AtomicI64::new(-1);
static INLINE_RESTART: AtomicBool = AtomicBool::new(false);
static QUERY_IN_CALLBACK: AtomicBool = AtomicBool::new(false);
static IN_CALLBACK: AtomicU32 = AtomicU32::new(0);
static OVERLAPS: AtomicU32 = AtomicU32::new(0);
static RESTARTS: AtomicU32 = AtomicU32::new(0);
/// Set while the callback is busy in host code (query mode).
static IN_HOST_WORK: AtomicBool = AtomicBool::new(false);
static CB_TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

unsafe fn process(index: i32, sample_pos: Option<u64>) {
    // Two bufferSwitch calls at once would mean two live callback threads.
    if IN_CALLBACK.fetch_add(1, SeqCst) != 0 {
        OVERLAPS.fetch_add(1, SeqCst);
    }
    process_inner(index, sample_pos);
    IN_CALLBACK.fetch_sub(1, SeqCst);
}

unsafe fn process_inner(index: i32, sample_pos: Option<u64>) {
    let Some(h) = (*HOST.0.get()).as_mut() else { return };
    let n = CALLS.fetch_add(1, Relaxed);
    if QUERY_IN_CALLBACK.load(Relaxed) {
        // Hosts call back into the driver from bufferSwitch all the time.
        // Spend a while in the callback first so a stop() from the main
        // thread is likely to land while we are in here.
        let t0 = qpc();
        IN_HOST_WORK.store(true, SeqCst);
        Sleep(1);
        let d = Driver(h.driver);
        let mut r = 0.0;
        d.get_sample_rate(&mut r);
        let (mut il, mut ol) = (0, 0);
        d.get_latencies(&mut il, &mut ol);
        IN_HOST_WORK.store(false, SeqCst);
        CB_TICKS.fetch_add((qpc() - t0) as u64, Relaxed);
    }
    if INLINE_RESTART.load(Relaxed) && n == 100 {
        // A host resetting its stream from inside the callback.
        let d = Driver(h.driver);
        d.stop();
        d.start();
        RESTARTS.fetch_add(1, Relaxed);
    }
    if n < h.stamps.capacity() {
        h.stamps.push(qpc());
    }
    if let Some(p) = sample_pos {
        let last = LAST_POS.swap(p as i64, Relaxed);
        // Forward jumps by whole blocks are skipped (late) blocks; a position
        // that repeats, goes back, or leaves the block grid is a bug (e.g. a
        // block delivered twice).
        let d = p as i64 - last;
        if last >= 0 && (d <= 0 || d % h.bufsize as i64 != 0) {
            SAMPLE_POS_ERRORS.fetch_add(1, Relaxed);
        }
    }
    let i = index as usize & 1;
    let b = h.bufsize;
    let mut out = vec![0.0f32; b];
    if h.sine {
        let w = 2.0 * std::f64::consts::PI * 440.0 / h.rate;
        for s in out.iter_mut() {
            *s = (h.phase.sin() * 0.25) as f32;
            h.phase += w;
        }
        h.phase %= 2.0 * std::f64::consts::PI;
    }
    if let Some(inp) = h.ins.first() {
        for k in 0..b {
            let v = if h.int32 { *(inp[i] as *const i32).add(k) as f32 / 2.147_483_6e9 } else { *inp[i].add(k) };
            h.peak = h.peak.max(v.abs());
        }
    }
    if h.loopback {
        // Detect the click on in_1.
        if let (Some(at), Some(inp)) = (h.click_at, h.ins.first()) {
            for k in 0..b {
                let v = if h.int32 { *(inp[i] as *const i32).add(k) as f32 / 2.147_483_6e9 } else { *inp[i].add(k) };
                if v > 0.5 {
                    h.latencies.push(h.frames + k as u64 - at);
                    h.click_at = None;
                    break;
                }
            }
        }
        // Emit a click every 0.25 s.
        if h.click_at.is_none() && h.frames >= h.last_click + (h.rate as u64 / 4) {
            out[0] = 1.0;
            h.click_at = Some(h.frames);
            h.last_click = h.frames;
        }
    }
    for (k, o) in h.outs.iter().enumerate() {
        for (j, &s) in out.iter().enumerate() {
            let s = if k < 2 || h.loopback { s } else { 0.0 };
            if h.int32 {
                *(o[i] as *mut i32).add(j) = (s as f64 * 2_147_483_647.0) as i32;
            } else {
                *o[i].add(j) = s;
            }
        }
    }
    h.frames += b as u64;
    if h.stall_ms > 0 && n == 200 {
        Sleep(h.stall_ms);
    }
    if h.output_ready {
        Driver(h.driver).output_ready();
    }
}

unsafe extern "C" fn cb_buffer_switch(index: i32, _direct: i32) {
    let h = (*HOST.0.get()).as_ref();
    let pos = h.map(|h| {
        let mut p = ASIOSamples::default();
        let mut t = ASIOSamples::default();
        Driver(h.driver).get_sample_position(&mut p, &mut t);
        p.to_u64()
    });
    process(index, pos);
}

unsafe extern "C" fn cb_buffer_switch_time_info(t: *mut ASIOTime, index: i32, _direct: i32) -> *mut ASIOTime {
    let pos = t.as_ref().map(|t| t.timeInfo.samplePosition.to_u64());
    process(index, pos);
    t
}

unsafe extern "C" fn cb_rate_changed(rate: f64) {
    println!("  [host] sampleRateDidChange({rate})");
    RATE_CHANGES.fetch_add(1, Relaxed);
}

unsafe extern "C" fn cb_message(sel: i32, value: i32, _msg: *mut c_void, _opt: *mut f64) -> i32 {
    match sel {
        kAsioSelectorSupported => matches!(value, kAsioResetRequest | kAsioLatenciesChanged | kAsioSupportsTimeInfo | kAsioEngineVersion) as i32,
        kAsioEngineVersion => 2,
        kAsioSupportsTimeInfo => TIMEINFO.load(Relaxed) as i32,
        kAsioResetRequest => {
            println!("  [host] kAsioResetRequest");
            RESETS.fetch_add(1, Relaxed);
            1
        }
        kAsioLatenciesChanged => {
            println!("  [host] kAsioLatenciesChanged");
            LAT_CHANGES.fetch_add(1, Relaxed);
            1
        }
        _ => 0,
    }
}

struct Args {
    buffer: Option<i32>,
    seconds: f64,
    sine: bool,
    loopback: bool,
    output_ready: bool,
    rate: Option<f64>,
    stall_ms: u32,
    cycles: u32,
    control_panel: bool,
}

fn parse_args() -> Args {
    let mut a = Args { buffer: None, seconds: 5.0, sine: false, loopback: false, output_ready: false, rate: None, stall_ms: 0, cycles: 1, control_panel: false };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--buffer" => a.buffer = it.next().and_then(|v| v.parse().ok()),
            "--seconds" => a.seconds = it.next().and_then(|v| v.parse().ok()).unwrap_or(5.0),
            "--rate" => a.rate = it.next().and_then(|v| v.parse().ok()),
            "--stall-ms" => a.stall_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--cycles" => a.cycles = it.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            "--sine" => a.sine = true,
            "--control-panel" => a.control_panel = true,
            "--inline-restart" => INLINE_RESTART.store(true, Relaxed),
            "--query-in-callback" => QUERY_IN_CALLBACK.store(true, Relaxed),
            "--loopback" => a.loopback = true,
            "--output-ready" => a.output_ready = true,
            "--no-timeinfo" => TIMEINFO.store(false, Relaxed),
            "-h" | "--help" => {
                println!("usage: winepipe-test [--buffer N] [--seconds S] [--sine] [--loopback] [--output-ready] [--no-timeinfo] [--rate R] [--stall-ms M] [--cycles K]");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    a
}

fn check(what: &str, r: i32) -> i32 {
    if r != ASE_OK && r != ASE_SUCCESS {
        println!("  {what} -> {r}");
    }
    r
}

fn main() {
    let args = parse_args();
    let mut freq = 0;
    unsafe { QueryPerformanceFrequency(&mut freq) };
    let freq = freq as f64;

    println!("winepipe-test ({} bit)", std::mem::size_of::<usize>() * 8);
    let mut ptr = null_mut();
    let hr = unsafe {
        CoInitializeEx(null_mut(), 2);
        CoCreateInstance(&CLSID_WINEPIPE, null_mut(), 1, &CLSID_WINEPIPE, &mut ptr)
    };
    if hr != 0 || ptr.is_null() {
        println!("FAIL: CoCreateInstance -> {:#010x} (is the driver registered?)", hr as u32);
        std::process::exit(1);
    }
    let d = Driver(ptr);
    if d.init(null_mut()) == 0 {
        println!("FAIL: init: {}", d.error());
        std::process::exit(1);
    }
    let mut name = [0u8; 32];
    d.get_driver_name(name.as_mut_ptr());
    println!("driver: {} v{}", abi::get_str(&name), d.get_driver_version());

    if args.control_panel {
        println!("controlPanel -> {}", d.control_panel());
        d.release();
        return;
    }
    let (mut ni, mut no) = (0, 0);
    check("getChannels", d.get_channels(&mut ni, &mut no));
    let (mut mn, mut mx, mut pref, mut gran) = (0, 0, 0, 0);
    check("getBufferSize", d.get_buffer_size(&mut mn, &mut mx, &mut pref, &mut gran));
    let mut rate = 0.0;
    check("getSampleRate", d.get_sample_rate(&mut rate));
    println!("channels: {ni} in / {no} out; buffer min {mn} max {mx} pref {pref} gran {gran}; rate {rate}");
    for r in [44100.0, 48000.0, 96000.0] {
        println!("  canSampleRate({r}) -> {}", d.can_sample_rate(r));
    }
    if let Some(r) = args.rate {
        println!("  setSampleRate({r}) -> {}", d.set_sample_rate(r));
        d.get_sample_rate(&mut rate);
    }

    let mut st = 0;
    for input in [1, 0] {
        for ch in 0..(if input == 1 { ni } else { no }) {
            let mut info = ASIOChannelInfo { channel: ch, isInput: input, isActive: 0, channelGroup: 0, type_: 0, name: [0; 32] };
            check("getChannelInfo", d.get_channel_info(&mut info));
            st = info.type_;
            if ch < 2 {
                println!("  {} ch{ch}: '{}' type {}", if input == 1 { "in " } else { "out" }, abi::get_str(&info.name), info.type_);
            }
        }
    }
    let int32 = st == ASIOSTInt32LSB;

    let bufsize = args.buffer.unwrap_or(pref);
    let mut fail = false;
    for cycle in 0..args.cycles.max(1) {
        let mut infos: Vec<ASIOBufferInfo> = (0..ni)
            .map(|c| ASIOBufferInfo { isInput: 1, channelNum: c, buffers: [null_mut(); 2] })
            .chain((0..no).map(|c| ASIOBufferInfo { isInput: 0, channelNum: c, buffers: [null_mut(); 2] }))
            .collect();
        let mut cbs = ASIOCallbacks {
            bufferSwitch: Some(cb_buffer_switch),
            sampleRateDidChange: Some(cb_rate_changed),
            asioMessage: Some(cb_message),
            bufferSwitchTimeInfo: Some(cb_buffer_switch_time_info),
        };
        let r = d.create_buffers(infos.as_mut_ptr(), infos.len() as i32, bufsize, &mut cbs);
        if r != ASE_OK {
            println!("FAIL: createBuffers({bufsize}) -> {r}: {}", d.error());
            std::process::exit(1);
        }
        let (mut il, mut ol) = (0, 0);
        check("getLatencies", d.get_latencies(&mut il, &mut ol));
        println!("buffers: {} x {bufsize} frames; latency in {il} out {ol} ({:.2} ms / {:.2} ms)", infos.len(), il as f64 * 1000.0 / rate, ol as f64 * 1000.0 / rate);

        let expected = (args.seconds * rate / bufsize as f64) as usize;
        unsafe {
            *HOST.0.get() = Some(Host {
                driver: ptr,
                bufsize: bufsize as usize,
                ins: infos.iter().filter(|i| i.isInput != 0).map(|i| [i.buffers[0].cast(), i.buffers[1].cast()]).collect(),
                outs: infos.iter().filter(|i| i.isInput == 0).map(|i| [i.buffers[0].cast(), i.buffers[1].cast()]).collect(),
                int32,
                sine: args.sine,
                loopback: args.loopback,
                output_ready: args.output_ready,
                stall_ms: args.stall_ms,
                phase: 0.0,
                rate,
                stamps: Vec::with_capacity(expected * 2 + 64),
                frames: 0,
                click_at: None,
                last_click: 0,
                latencies: Vec::new(),
                peak: 0.0,
            });
        }
        CALLS.store(0, Relaxed);
        LAST_POS.store(-1, Relaxed);
        if check("start", d.start()) != ASE_OK {
            println!("FAIL: start: {}", d.error());
            std::process::exit(1);
        }
        unsafe { Sleep((args.seconds * 1000.0) as u32) };
        if QUERY_IN_CALLBACK.load(Relaxed) {
            // Stop while the callback thread is inside host code that is
            // about to call back into the driver.
            let until = qpc() + freq as i64;
            while !IN_HOST_WORK.load(SeqCst) && qpc() < until {
                std::hint::spin_loop();
            }
        }
        let t_stop = qpc();
        check("stop", d.stop());
        let stop_ms = (qpc() - t_stop) as f64 * 1000.0 / freq;
        let calls = CALLS.load(Relaxed);
        unsafe { Sleep(50) };
        let after = CALLS.load(Relaxed);

        let mut stats = abi::StatsParams::default();
        let have_stats = d.future(abi::FUTURE_GET_STATS, &mut stats as *mut _ as *mut c_void) == ASE_SUCCESS;
        check("disposeBuffers", d.dispose_buffers());

        let h = unsafe { (*HOST.0.get()).take().unwrap() };
        println!("--- run {} ---", cycle + 1);
        println!("callbacks: {calls} (expected ~{expected}), after stop: {}", after - calls);
        println!(
            "stop took {stop_ms:.1} ms; overlapping callbacks {}; inline restarts {}",
            OVERLAPS.load(SeqCst),
            RESTARTS.load(Relaxed)
        );
        if QUERY_IN_CALLBACK.load(Relaxed) && calls > 0 {
            println!("time in callback: {:.0} us avg", CB_TICKS.load(Relaxed) as f64 * 1e6 / freq / calls as f64);
        }
        // An inline restart legitimately resets the position once; any
        // further jump means a block was delivered twice or skipped.
        let pos_errors_allowed = RESTARTS.load(Relaxed);
        if OVERLAPS.load(SeqCst) != 0 || stop_ms > 1000.0 || SAMPLE_POS_ERRORS.load(Relaxed) > pos_errors_allowed {
            fail = true;
        }
        if h.stamps.len() > 10 {
            let period_us = bufsize as f64 * 1e6 / rate;
            let iv: Vec<f64> = h.stamps.windows(2).skip(5).map(|w| (w[1] - w[0]) as f64 * 1e6 / freq).collect();
            let mean = iv.iter().sum::<f64>() / iv.len() as f64;
            let max = iv.iter().cloned().fold(0.0, f64::max);
            let min = iv.iter().cloned().fold(f64::MAX, f64::min);
            let sd = (iv.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / iv.len() as f64).sqrt();
            println!("interval: mean {mean:.1} us (period {period_us:.1}), min {min:.1}, max {max:.1}, stddev {sd:.1}");
        }
        if have_stats {
            let avg = stats.handoff_total_ns.checked_div(stats.switches).unwrap_or(0);
            println!(
                "driver: cycles {} switches {} late {} skipped {} underrun_frames {} graph_xruns {}",
                stats.cycles, stats.switches, stats.late, stats.skipped, stats.underrun_frames, stats.graph_xruns
            );
            println!(
                "        quantum {} fifo {} (+{} frames) rt_prio {} handoff avg {:.1} us max {:.1} us",
                stats.quantum,
                stats.fifo_mode,
                stats.fifo_latency,
                stats.rt_priority,
                avg as f64 / 1000.0,
                stats.handoff_max_ns as f64 / 1000.0
            );
        }
        println!(
            "events: resets {} latency-changes {} rate-changes {} sample-pos errors {}",
            RESETS.load(Relaxed),
            LAT_CHANGES.load(Relaxed),
            RATE_CHANGES.load(Relaxed),
            SAMPLE_POS_ERRORS.load(Relaxed)
        );
        println!("input peak (in_1): {:.4}", h.peak);
        if args.loopback {
            if h.latencies.is_empty() {
                println!("loopback: no clicks detected (link out_1 -> in_1 first)");
            } else {
                let mut l = h.latencies.clone();
                l.sort();
                println!(
                    "loopback: {} clicks, round trip min {} max {} samples ({:.2} ms)",
                    l.len(),
                    l[0],
                    l[l.len() - 1],
                    l[0] as f64 * 1000.0 / rate
                );
            }
        }
        if calls < expected * 9 / 10 || after - calls > 1 {
            fail = true;
        }
    }
    d.release();
    if fail {
        println!("RESULT: FAIL");
        std::process::exit(1);
    }
    println!("RESULT: OK");
}
