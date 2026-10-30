//! The unix-call ABI shared by both halves of WinePipe.
//!
//! The PE side (x86_64 *and* i686 under WoW64) and the 64-bit Unix side must
//! agree on these layouts byte-for-byte. Every struct here therefore uses only
//! fixed-width fields, pointers travel as `u64`, 8-byte fields come first, and
//! the sizes are asserted at compile time on every target that builds this
//! crate. Because the layouts are identical, the Unix side can serve WoW64
//! callers with the very same entry points.

#![no_std]

/// Highest channel count accepted per direction.
pub const MAX_CHANNELS: u32 = 64;
/// Smallest ASIO buffer size offered to hosts.
pub const MIN_BUFFER_SIZE: u32 = 16;
/// Largest ASIO buffer size offered to hosts.
pub const MAX_BUFFER_SIZE: u32 = 8192;
/// Maximum number of allowed graph rates reported through [`InfoParams`].
pub const MAX_RATES: usize = 16;

/// Unix-call function numbers (index into `__wine_unix_call_funcs`).
pub mod call {
    pub const INIT: u32 = 0;
    pub const EXIT: u32 = 1;
    pub const GET_INFO: u32 = 2;
    pub const SET_SAMPLE_RATE: u32 = 3;
    pub const CREATE_BUFFERS: u32 = 4;
    pub const DISPOSE_BUFFERS: u32 = 5;
    pub const START: u32 = 6;
    pub const STOP: u32 = 7;
    pub const WAIT: u32 = 8;
    pub const OUTPUT_READY: u32 = 9;
    pub const GET_STATS: u32 = 10;
    pub const CONTROL_PANEL: u32 = 11;
    pub const COUNT: usize = 12;
}

/// `status` values returned inside the parameter structs.
pub mod status {
    pub const OK: i32 = 0;
    pub const INVALID_HANDLE: i32 = -1;
    pub const INVALID_PARAMETER: i32 = -2;
    pub const NOT_CONNECTED: i32 = -3;
    pub const INVALID_MODE: i32 = -4;
    pub const NO_MEMORY: i32 = -5;
    pub const UNSUPPORTED: i32 = -6;
    pub const FAILED: i32 = -7;
}

/// `InitParams::flags`
pub const INIT_FLAG_AUTOCONNECT: u32 = 1 << 0;

/// ASIO-side sample formats the Unix side can convert to/from.
pub const SAMPLE_FLOAT32: u32 = 0;
pub const SAMPLE_INT32: u32 = 1;

/// Private `ASIOFuture()` selector: copies a [`StatsParams`] into `opt`.
/// Used by diagnostics tools such as `winepipe-test`.
pub const FUTURE_GET_STATS: i32 = 0x5750_0001;

/// `WaitParams::result`
pub const WAIT_SWITCH: u32 = 1;
pub const WAIT_TIMEOUT: u32 = 2;
pub const WAIT_STOPPED: u32 = 3;

/// `WaitParams::events` bits (delivered to the host from the callback thread).
pub const EVENT_RATE_CHANGED: u32 = 1 << 0;
pub const EVENT_LATENCY_CHANGED: u32 = 1 << 1;
pub const EVENT_RESET_REQUEST: u32 = 1 << 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InitParams {
    // in
    pub num_inputs: u32,
    pub num_outputs: u32,
    pub flags: u32,
    pub _pad0: u32,
    pub client_name: [u8; 64],
    pub app_name: [u8; 64],
    // out
    pub handle: u64,
    pub status: i32,
    pub _pad1: u32,
    pub error: [u8; 128],
}

/// Used by EXIT, DISPOSE_BUFFERS and STOP.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HandleParams {
    pub handle: u64,
    pub status: i32,
    pub _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InfoParams {
    pub handle: u64,
    // out
    pub sample_rate: f64,
    pub allowed_rates: [u32; MAX_RATES],
    pub num_allowed_rates: u32,
    /// Current graph quantum (frames per cycle), 0 if unknown.
    pub quantum: u32,
    pub input_latency: u32,
    pub output_latency: u32,
    /// 1 while the PipeWire connection is healthy.
    pub connected: u32,
    pub status: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SetRateParams {
    pub handle: u64,
    pub rate: f64,
    pub status: i32,
    pub _pad: u32,
}

/// One ASIO channel's double buffer, allocated by the PE side.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ChannelBuffer {
    pub buffers: [u64; 2],
    pub channel: u32,
    pub is_input: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CreateBuffersParams {
    pub handle: u64,
    /// Pointer to `num_channels` [`ChannelBuffer`]s.
    pub channels: u64,
    pub num_channels: u32,
    pub buffer_size: u32,
    pub sample_type: u32,
    pub status: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct StartParams {
    pub handle: u64,
    /// out: the sequence number the callback thread must start waiting from.
    pub seq: u32,
    pub status: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WaitParams {
    pub handle: u64,
    // out
    /// Sample position of the first frame of the block being switched.
    pub sample_position: u64,
    /// CLOCK_MONOTONIC time (ns) of the first frame of the block.
    pub system_nsec: u64,
    /// CLOCK_MONOTONIC time (ns) right before the call returned.
    pub now_nsec: u64,
    pub sample_rate: f64,
    // in
    /// Sequence number of the last switch the host finished.
    pub last_done: u32,
    pub timeout_ms: u32,
    // out
    pub result: u32,
    pub seq: u32,
    pub index: u32,
    pub events: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct OutputReadyParams {
    pub handle: u64,
    pub seq: u32,
    pub status: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct StatsParams {
    pub handle: u64,
    // out
    /// PipeWire cycles seen while running.
    pub cycles: u64,
    /// bufferSwitch calls issued to the host.
    pub switches: u64,
    /// Switches the host did not finish before the cycle deadline.
    pub late: u64,
    /// Blocks skipped because the host was still busy with an earlier one.
    pub skipped: u64,
    /// Output frames that had to be filled with silence (FIFO underruns).
    pub underrun_frames: u64,
    /// Round trip RT thread -> host -> RT thread, in ns.
    pub handoff_max_ns: u64,
    pub handoff_last_ns: u64,
    pub handoff_total_ns: u64,
    /// Graph xruns reported by PipeWire for this node.
    pub graph_xruns: u64,
    pub quantum: u32,
    /// SCHED_FIFO priority obtained for the callback thread, or negative errno.
    pub rt_priority: i32,
    /// 1 when the FIFO (quantum != k * buffer size) path is active.
    pub fifo_mode: u32,
    /// Extra output latency (frames) introduced by the FIFO path.
    pub fifo_latency: u32,
    pub status: i32,
    pub _pad: u32,
}

const _: () = {
    use core::mem::size_of;
    assert!(size_of::<InitParams>() == 288);
    assert!(size_of::<HandleParams>() == 16);
    assert!(size_of::<InfoParams>() == 104);
    assert!(size_of::<SetRateParams>() == 24);
    assert!(size_of::<ChannelBuffer>() == 24);
    assert!(size_of::<CreateBuffersParams>() == 32);
    assert!(size_of::<StartParams>() == 16);
    assert!(size_of::<WaitParams>() == 64);
    assert!(size_of::<OutputReadyParams>() == 16);
    assert!(size_of::<StatsParams>() == 104);
};

/// Copy `s` into a fixed, NUL-terminated byte buffer (truncating on a UTF-8
/// boundary).
pub fn put_str(dst: &mut [u8], s: &str) {
    let mut n = s.len().min(dst.len().saturating_sub(1));
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
    for b in &mut dst[n..] {
        *b = 0;
    }
}

/// Read a NUL-terminated string back out of a fixed buffer.
pub fn get_str(src: &[u8]) -> &str {
    let end = src.iter().position(|&b| b == 0).unwrap_or(src.len());
    match core::str::from_utf8(&src[..end]) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&src[..e.valid_up_to()]).unwrap_or(""),
    }
}

impl Default for InitParams {
    fn default() -> Self {
        InitParams {
            num_inputs: 0,
            num_outputs: 0,
            flags: 0,
            _pad0: 0,
            client_name: [0; 64],
            app_name: [0; 64],
            handle: 0,
            status: 0,
            _pad1: 0,
            error: [0; 128],
        }
    }
}
