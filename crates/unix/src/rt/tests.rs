use super::*;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// ASIO double buffers for one in + one out channel, owned by the test.
struct Bufs {
    data: Vec<Vec<u32>>, // in0, in1, out0, out1 (u32 storage fits f32 and i32)
}

impl Bufs {
    fn new(b: usize) -> Bufs {
        Bufs { data: (0..4).map(|_| vec![0u32; b]).collect() }
    }
    fn ptr(&mut self, i: usize) -> *mut u8 {
        self.data[i].as_mut_ptr() as *mut u8
    }
}

#[derive(Clone, Copy)]
struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}

/// A fake host: copies input to output on every switch, optionally sleeping.
fn spawn_host(h: Arc<Handoff>, start: u32, ins: [SendPtr; 2], outs: [SendPtr; 2], b: usize, delay: Option<(u64, Duration)>) -> thread::JoinHandle<u64> {
    thread::spawn(move || {
        let ins = ins;
        let outs = outs;
        let mut p = abi::WaitParams { last_done: start, timeout_ms: 50, ..Default::default() };
        let mut switches = 0u64;
        loop {
            h.wait(&mut p);
            match p.result {
                abi::WAIT_SWITCH => {
                    let i = p.index as usize;
                    unsafe { std::ptr::copy_nonoverlapping(ins[i].0, outs[i].0, b * 4) };
                    if let Some((nth, d)) = delay {
                        if switches == nth {
                            thread::sleep(d);
                        }
                    }
                    switches += 1;
                    p.last_done = p.seq;
                }
                abi::WAIT_STOPPED => return switches,
                _ => {}
            }
        }
    })
}

fn run_loopback(n: usize, b: usize, cycles: usize, st: u32) -> (Vec<f32>, Vec<f32>, usize, Arc<Handoff>) {
    let h = Arc::new(Handoff::new());
    let mut bufs = Bufs::new(b);
    let ins = [SendPtr(bufs.ptr(0)), SendPtr(bufs.ptr(1))];
    let outs = [SendPtr(bufs.ptr(2)), SendPtr(bufs.ptr(3))];
    let mut s = Session::new(
        b,
        st,
        vec![Chan { port: 0, buf: [ins[0].0, ins[1].0] }],
        vec![Chan { port: 0, buf: [outs[0].0, outs[1].0] }],
        vec![1],
    );
    s.reset();
    let start = h.begin();
    let host = spawn_host(h.clone(), start, ins, outs, b, None);

    let total = n * cycles;
    let input: Vec<f32> = (0..total).map(|i| ((i % 1000) as f32 + 1.0) / 1024.0).collect();
    let mut output = vec![f32::NAN; total];
    let mut idle_port = vec![1.0f32; n];
    let mut prefill = 0;
    for c in 0..cycles {
        let cyc = Cycle { n, rate: 48000, nsec: futex::now_ns(), next_nsec: 0, freewheel: true };
        assert!(h.enter());
        unsafe {
            let inp = [input[c * n..].as_ptr(), std::ptr::null()];
            let out = [output[c * n..].as_mut_ptr(), idle_port.as_mut_ptr()];
            s.process(&h, &inp, &out, &cyc);
        }
        h.leave();
        prefill = s.prefill();
    }
    assert!(idle_port.iter().all(|&x| x == 0.0), "idle output port not silenced");
    h.halt();
    host.join().unwrap();
    (input, output, prefill, h)
}

fn check_delay(n: usize, b: usize) {
    let cycles = (8 * b / n).max(8) + 4;
    let (input, output, p, h) = run_loopback(n, b, cycles, abi::SAMPLE_FLOAT32);
    assert_eq!(p, fifo_prefill(n, b));
    for (t, &o) in output.iter().enumerate() {
        let want = if t < p { 0.0 } else { input[t - p] };
        assert_eq!(o, want, "n={n} b={b} p={p} frame {t}");
    }
    assert_eq!(h.stats.underrun_frames.load(Relaxed), 0, "n={n} b={b}");
    assert_eq!(h.stats.late.load(Relaxed), 0);
    assert_eq!(h.stats.skipped.load(Relaxed), 0);
    assert_eq!(h.stats.switches.load(Relaxed) as usize, input.len() / b);
}

#[test]
fn direct_path_has_zero_latency() {
    for (n, b) in [(256, 256), (1024, 256), (64, 16), (32, 32)] {
        check_delay(n, b);
    }
}

#[test]
fn fifo_path_uses_minimal_prefill() {
    for (n, b) in [(128, 256), (1024, 2048), (96, 64), (100, 64), (300, 256), (48, 128), (33, 16)] {
        check_delay(n, b);
    }
}

#[test]
fn prefill_formula() {
    assert_eq!(fifo_prefill(256, 256), 0);
    assert_eq!(fifo_prefill(512, 256), 0);
    assert_eq!(fifo_prefill(128, 256), 128);
    assert_eq!(fifo_prefill(96, 64), 32);
    assert_eq!(fifo_prefill(100, 64), 60);
}

#[test]
fn int32_conversion_roundtrips() {
    let (input, output, _, _) = run_loopback(256, 256, 4, abi::SAMPLE_INT32);
    for (a, b) in input.iter().zip(&output) {
        assert!((a - b).abs() < 1e-6, "{a} vs {b}");
    }
    assert_eq!(f32_to_i32(1.0), i32::MAX);
    assert_eq!(f32_to_i32(-1.0), i32::MIN);
    assert_eq!(f32_to_i32(2.0), i32::MAX);
    assert_eq!(f32_to_i32(f32::NAN), 0);
}

/// A host that stalls must cost silence, never a stuck RT thread, and the
/// stream must recover by itself.
#[test]
fn late_host_is_dropped_and_recovers() {
    let n = 256;
    let b = 256;
    let h = Arc::new(Handoff::new());
    let mut bufs = Bufs::new(b);
    let ins = [SendPtr(bufs.ptr(0)), SendPtr(bufs.ptr(1))];
    let outs = [SendPtr(bufs.ptr(2)), SendPtr(bufs.ptr(3))];
    let mut s = Session::new(b, abi::SAMPLE_FLOAT32, vec![Chan { port: 0, buf: [ins[0].0, ins[1].0] }], vec![Chan { port: 0, buf: [outs[0].0, outs[1].0] }], vec![]);
    s.reset();
    let start = h.begin();
    // stall for 30 ms on the 5th switch; period is 5.3 ms
    let host = spawn_host(h.clone(), start, ins, outs, b, Some((5, Duration::from_millis(30))));

    let input = vec![0.5f32; n];
    let mut output = vec![0.0f32; n];
    let mut worst = 0u64;
    let mut good_after = 0;
    for c in 0..40 {
        let now = futex::now_ns();
        let period = n as u64 * 1_000_000_000 / 48000;
        let cyc = Cycle { n, rate: 48000, nsec: now, next_nsec: now + period, freewheel: false };
        assert!(h.enter());
        unsafe { s.process(&h, &[input.as_ptr()], &[output.as_mut_ptr()], &cyc) };
        h.leave();
        let spent = futex::now_ns() - now;
        worst = worst.max(spent);
        if c > 20 && output[0] == 0.5 {
            good_after += 1;
        }
        // emulate the rest of the period
        thread::sleep(Duration::from_nanos(period.saturating_sub(spent)));
    }
    h.halt();
    host.join().unwrap();
    let period = n as u64 * 1_000_000_000 / 48000;
    assert!(worst <= period + 2_000_000, "RT thread blocked {worst} ns (period {period})");
    assert!(h.stats.late.load(Relaxed) >= 1);
    assert!(h.stats.skipped.load(Relaxed) >= 1);
    assert!(good_after >= 15, "stream did not recover ({good_after})");
}

/// halt() must return promptly even while the host sits inside bufferSwitch.
#[test]
fn halt_while_host_busy() {
    let b = 64;
    let h = Arc::new(Handoff::new());
    let mut bufs = Bufs::new(b);
    let ins = [SendPtr(bufs.ptr(0)), SendPtr(bufs.ptr(1))];
    let outs = [SendPtr(bufs.ptr(2)), SendPtr(bufs.ptr(3))];
    let mut s = Session::new(b, abi::SAMPLE_FLOAT32, vec![Chan { port: 0, buf: [ins[0].0, ins[1].0] }], vec![Chan { port: 0, buf: [outs[0].0, outs[1].0] }], vec![]);
    s.reset();
    let start = h.begin();
    let host = spawn_host(h.clone(), start, ins, outs, b, Some((0, Duration::from_millis(200))));
    let h2 = h.clone();
    let rt = thread::spawn(move || {
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];
        let cyc = Cycle { n: 64, rate: 48000, nsec: futex::now_ns(), next_nsec: 0, freewheel: true };
        if h2.enter() {
            unsafe { s.process(&h2, &[input.as_ptr()], &[output.as_mut_ptr()], &cyc) };
            h2.leave();
        }
    });
    thread::sleep(Duration::from_millis(20));
    let t0 = futex::now_ns();
    h.halt();
    assert!(futex::now_ns() - t0 < 50_000_000, "halt took too long");
    assert!(!h.enter());
    rt.join().unwrap();
    host.join().unwrap();
}

#[test]
fn events_wake_the_callback_thread() {
    let h = Handoff::new();
    let start = h.begin();
    h.raise(abi::EVENT_LATENCY_CHANGED);
    let mut p = abi::WaitParams { last_done: start, timeout_ms: 1000, ..Default::default() };
    let t0 = futex::now_ns();
    h.wait(&mut p);
    assert!(futex::now_ns() - t0 < 100_000_000);
    assert_eq!(p.result, abi::WAIT_TIMEOUT);
    assert_eq!(p.events, abi::EVENT_LATENCY_CHANGED);
    // an event is delivered exactly once, even though `p` is reused
    p.timeout_ms = 20;
    h.wait(&mut p);
    assert_eq!(p.result, abi::WAIT_TIMEOUT);
    assert_eq!(p.events, 0);
    h.halt();
    h.wait(&mut p);
    assert_eq!(p.result, abi::WAIT_STOPPED);
}
