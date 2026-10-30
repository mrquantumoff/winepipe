//! The IASIO COM object.
//!
//! Locking rule: `Driver::st` is never held while calling into the host
//! (hosts re-enter the driver from their callbacks), and the callback thread
//! never takes it at all; it works from `CbShared`.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex, MutexGuard};

use winepipe_abi::{self as abi, call};

use crate::asio::*;
use crate::config::Config;
use crate::win::*;
use crate::{log, unix, CLSID_WINEPIPE, OBJECTS};

pub const DRIVER_NAME: &str = "WinePipe";
const DRIVER_VERSION: i32 = 1;

/// State shared with the callback thread.
struct CbShared {
    handle: AtomicU64,
    callbacks: AtomicPtr<ASIOCallbacks>,
    time_info: AtomicBool,
    can_reset: AtomicBool,
    can_latency: AtomicBool,
    stop: AtomicBool,
    start_seq: AtomicU32,
    /// Sequence number of the block the host is currently processing.
    cur_seq: AtomicU32,
    thread_id: AtomicU32,
    sample_pos: AtomicU64,
    system_ns: AtomicU64,
    asio_time: UnsafeCell<ASIOTime>,
}

// SAFETY: asio_time is only touched by the callback thread.
unsafe impl Sync for CbShared {}
unsafe impl Send for CbShared {}

struct Buffers {
    mem: *mut c_void,
    bufsize: u32,
    /// (is_input, channel) of each active channel.
    active: Vec<(bool, u32)>,
}

struct State {
    handle: u64,
    cfg: Config,
    error: String,
    buffers: Option<Buffers>,
    thread: HANDLE,
    running: bool,
}

// SAFETY: raw handles/pointers inside are only used under the mutex.
unsafe impl Send for State {}

#[repr(C)]
pub struct Driver {
    vtbl: *const Vtbl,
    refs: AtomicU32,
    st: Mutex<State>,
    cb: Arc<CbShared>,
}

impl Driver {
    pub fn create() -> *mut Driver {
        OBJECTS.fetch_add(1, SeqCst);
        Box::into_raw(Box::new(Driver {
            vtbl: &VTBL,
            refs: AtomicU32::new(1),
            st: Mutex::new(State {
                handle: 0,
                cfg: Config::default(),
                error: String::new(),
                buffers: None,
                thread: null_mut(),
                running: false,
            }),
            cb: Arc::new(CbShared {
                handle: AtomicU64::new(0),
                callbacks: AtomicPtr::new(null_mut()),
                time_info: AtomicBool::new(false),
                can_reset: AtomicBool::new(false),
                can_latency: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                start_seq: AtomicU32::new(0),
                cur_seq: AtomicU32::new(0),
                thread_id: AtomicU32::new(0),
                sample_pos: AtomicU64::new(0),
                system_ns: AtomicU64::new(0),
                asio_time: UnsafeCell::new(ASIOTime::default()),
            }),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn info(&self, handle: u64) -> Option<abi::InfoParams> {
        if handle == 0 {
            return None;
        }
        let mut p = abi::InfoParams { handle, ..Default::default() };
        (unix::call(call::GET_INFO, &mut p) == 0 && p.status == abi::status::OK).then_some(p)
    }

    // ---- IUnknown ---------------------------------------------------------

    pub(crate) unsafe fn query_interface(this: *mut Driver, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
        if ppv.is_null() || riid.is_null() {
            return E_POINTER;
        }
        // ASIO hosts pass the driver's CLSID as the interface ID.
        if *riid == IID_IUNKNOWN || *riid == CLSID_WINEPIPE {
            (*this).refs.fetch_add(1, AcqRel);
            *ppv = this.cast();
            S_OK
        } else {
            *ppv = null_mut();
            E_NOINTERFACE
        }
    }

    unsafe fn add_ref(this: *mut Driver) -> u32 {
        (*this).refs.fetch_add(1, AcqRel) + 1
    }

    pub(crate) unsafe fn release(this: *mut Driver) -> u32 {
        let n = (*this).refs.fetch_sub(1, AcqRel) - 1;
        if n == 0 {
            (*this).shutdown();
            drop(Box::from_raw(this));
            OBJECTS.fetch_sub(1, SeqCst);
        }
        n
    }

    fn shutdown(&self) {
        self.stop();
        self.dispose_buffers();
        let mut st = self.lock();
        if st.handle != 0 {
            self.cb.stop.store(true, SeqCst);
            self.cb.handle.store(0, SeqCst);
            let mut p = abi::HandleParams { handle: st.handle, ..Default::default() };
            unix::call(call::EXIT, &mut p);
            st.handle = 0;
        }
    }

    // ---- IASIO ------------------------------------------------------------

    fn init(&self, _sys: *mut c_void) -> ASIOBool {
        let mut st = self.lock();
        if st.handle != 0 {
            return 1;
        }
        st.cfg = Config::load();
        if let Err(e) = unix::load() {
            st.error = e;
            log!("init failed: {}", st.error);
            return 0;
        }
        let mut p = abi::InitParams {
            num_inputs: st.cfg.inputs,
            num_outputs: st.cfg.outputs,
            flags: if st.cfg.autoconnect { abi::INIT_FLAG_AUTOCONNECT } else { 0 },
            ..Default::default()
        };
        abi::put_str(&mut p.client_name, &st.cfg.client_name);
        let app = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        abi::put_str(&mut p.app_name, &app);
        let r = unix::call(call::INIT, &mut p);
        if r != 0 || p.status != abi::status::OK || p.handle == 0 {
            st.error = if r != 0 {
                format!("unix call failed ({r:#x})")
            } else {
                abi::get_str(&p.error).to_string()
            };
            log!("init failed: {}", st.error);
            return 0;
        }
        st.handle = p.handle;
        st.error.clear();
        self.cb.handle.store(p.handle, SeqCst);
        log!("init ok: {:?}", st.cfg);
        1
    }

    fn get_driver_name(&self, name: *mut u8) {
        if !name.is_null() {
            // SAFETY: ASIO guarantees 32 bytes.
            unsafe { abi::put_str(std::slice::from_raw_parts_mut(name, 32), DRIVER_NAME) };
        }
    }

    fn get_driver_version(&self) -> i32 {
        DRIVER_VERSION
    }

    fn get_error_message(&self, s: *mut u8) {
        if !s.is_null() {
            let st = self.lock();
            let msg = if st.error.is_empty() { "No error" } else { &st.error };
            // SAFETY: ASIO guarantees 124 bytes.
            unsafe { abi::put_str(std::slice::from_raw_parts_mut(s, 124), msg) };
        }
    }

    fn start(&self) -> ASIOError {
        let mut st = self.lock();
        if st.handle == 0 || st.buffers.is_none() || self.cb.callbacks.load(SeqCst).is_null() {
            return ASE_NotPresent;
        }
        if st.running {
            return ASE_OK;
        }
        // A previous callback thread that we could not join (stop() called
        // from inside the callback) must be gone before we start a new one.
        self.reap(&mut st, 2000);
        let mut p = abi::StartParams { handle: st.handle, ..Default::default() };
        if unix::call(call::START, &mut p) != 0 || p.status != abi::status::OK {
            st.error = format!("start failed ({})", p.status);
            return ASE_HWMalfunction;
        }
        self.cb.stop.store(false, SeqCst);
        self.cb.start_seq.store(p.seq, SeqCst);
        self.cb.cur_seq.store(p.seq, SeqCst);
        self.cb.sample_pos.store(0, SeqCst);
        self.cb.system_ns.store(qpc_ns(), SeqCst);
        let arg = Arc::into_raw(self.cb.clone()) as *mut c_void;
        let mut tid = 0;
        // SAFETY: thread_proc takes ownership of the Arc reference.
        let th = unsafe { CreateThread(null_mut(), 0, thread_proc, arg, 0, &mut tid) };
        if th.is_null() {
            // SAFETY: reclaim the reference the thread never took.
            unsafe { drop(Arc::from_raw(arg as *const CbShared)) };
            let mut s = abi::HandleParams { handle: st.handle, ..Default::default() };
            unix::call(call::STOP, &mut s);
            st.error = "could not create the callback thread".into();
            return ASE_HWMalfunction;
        }
        self.cb.thread_id.store(tid, SeqCst);
        st.thread = th;
        st.running = true;
        log!("start");
        ASE_OK
    }

    fn stop(&self) -> ASIOError {
        let mut st = self.lock();
        if !st.running {
            return ASE_OK;
        }
        self.cb.stop.store(true, SeqCst);
        let mut p = abi::HandleParams { handle: st.handle, ..Default::default() };
        unix::call(call::STOP, &mut p);
        st.running = false;
        self.reap(&mut st, 3000);
        log!("stop");
        ASE_OK
    }

    /// Join the callback thread (it has been told to stop). When called from
    /// the callback thread itself (a host calling stop()/disposeBuffers()
    /// from bufferSwitch or asioMessage) there is nothing to wait for: the
    /// thread checks `stop` as soon as the host returns and exits.
    fn reap(&self, st: &mut State, timeout_ms: DWORD) {
        if st.thread.is_null() {
            return;
        }
        // SAFETY: plain Win32 calls on a handle we own.
        unsafe {
            if GetCurrentThreadId() != self.cb.thread_id.load(SeqCst)
                && WaitForSingleObject(st.thread, timeout_ms) != WAIT_OBJECT_0
            {
                log!("callback thread did not exit within {timeout_ms} ms (host stuck in bufferSwitch?)");
            }
            CloseHandle(st.thread);
        }
        st.thread = null_mut();
    }

    fn get_channels(&self, ins: *mut i32, outs: *mut i32) -> ASIOError {
        let st = self.lock();
        if st.handle == 0 {
            return ASE_NotPresent;
        }
        // SAFETY: host-provided out pointers.
        unsafe {
            if let Some(i) = ins.as_mut() {
                *i = st.cfg.inputs as i32;
            }
            if let Some(o) = outs.as_mut() {
                *o = st.cfg.outputs as i32;
            }
        }
        ASE_OK
    }

    fn get_latencies(&self, inl: *mut i32, outl: *mut i32) -> ASIOError {
        let (handle, fallback) = {
            let st = self.lock();
            let b = st.buffers.as_ref().map_or(st.cfg.preferred_buffer_size, |b| b.bufsize);
            (st.handle, b)
        };
        let Some(info) = self.info(handle) else { return ASE_NotPresent };
        let (i, o) = if info.input_latency == 0 { (fallback, fallback) } else { (info.input_latency, info.output_latency) };
        // SAFETY: host-provided out pointers.
        unsafe {
            if let Some(p) = inl.as_mut() {
                *p = i as i32;
            }
            if let Some(p) = outl.as_mut() {
                *p = o as i32;
            }
        }
        ASE_OK
    }

    fn get_buffer_size(&self, min: *mut i32, max: *mut i32, pref: *mut i32, gran: *mut i32) -> ASIOError {
        let st = self.lock();
        if st.handle == 0 {
            return ASE_NotPresent;
        }
        let p = st.cfg.preferred_buffer_size as i32;
        let (mn, mx, g) = if st.cfg.fixed_buffer_size {
            (p, p, 0)
        } else {
            (abi::MIN_BUFFER_SIZE as i32, abi::MAX_BUFFER_SIZE as i32, -1)
        };
        // SAFETY: host-provided out pointers.
        unsafe {
            for (ptr, v) in [(min, mn), (max, mx), (pref, p), (gran, g)] {
                if let Some(r) = ptr.as_mut() {
                    *r = v;
                }
            }
        }
        ASE_OK
    }

    fn can_sample_rate(&self, rate: f64) -> ASIOError {
        let handle = self.lock().handle;
        let Some(info) = self.info(handle) else { return ASE_NotPresent };
        let rates = &info.allowed_rates[..info.num_allowed_rates as usize];
        if rate == info.sample_rate || rates.iter().any(|&r| f64::from(r) == rate) {
            ASE_OK
        } else {
            ASE_NoClock
        }
    }

    fn get_sample_rate(&self, rate: *mut f64) -> ASIOError {
        if rate.is_null() {
            return ASE_InvalidParameter;
        }
        let handle = self.lock().handle;
        let Some(info) = self.info(handle) else { return ASE_NotPresent };
        // SAFETY: checked above.
        unsafe { *rate = info.sample_rate };
        ASE_OK
    }

    fn set_sample_rate(&self, rate: f64) -> ASIOError {
        let handle = self.lock().handle;
        if handle == 0 {
            return ASE_NotPresent;
        }
        if rate == 0.0 {
            // "external clock": the PipeWire graph is our only clock.
            return ASE_OK;
        }
        let mut p = abi::SetRateParams { handle, rate, ..Default::default() };
        unix::call(call::SET_SAMPLE_RATE, &mut p);
        log!("setSampleRate({rate}) -> {}", p.status);
        match p.status {
            abi::status::OK => ASE_OK,
            abi::status::INVALID_PARAMETER => ASE_InvalidParameter,
            _ => ASE_NoClock,
        }
    }

    fn get_clock_sources(&self, clocks: *mut ASIOClockSource, num: *mut i32) -> ASIOError {
        if clocks.is_null() || num.is_null() {
            return ASE_InvalidParameter;
        }
        // SAFETY: host provides at least *num entries (we only write one).
        unsafe {
            if *num < 1 {
                *num = 0;
                return ASE_OK;
            }
            let c = &mut *clocks;
            c.index = 0;
            c.associatedChannel = -1;
            c.associatedGroup = -1;
            c.isCurrentSource = 1;
            abi::put_str(&mut c.name, "PipeWire graph");
            *num = 1;
        }
        ASE_OK
    }

    fn set_clock_source(&self, reference: i32) -> ASIOError {
        if reference == 0 {
            ASE_OK
        } else {
            ASE_InvalidParameter
        }
    }

    fn get_sample_position(&self, pos: *mut ASIOSamples, ts: *mut ASIOTimeStamp) -> ASIOError {
        if pos.is_null() || ts.is_null() {
            return ASE_InvalidParameter;
        }
        // Lock-free: hosts call this from inside bufferSwitch.
        if self.cb.handle.load(Relaxed) == 0 {
            return ASE_NotPresent;
        }
        // SAFETY: checked above.
        unsafe {
            *pos = ASIOSamples::from_u64(self.cb.sample_pos.load(Acquire));
            *ts = ASIOSamples::from_u64(self.cb.system_ns.load(Acquire));
        }
        ASE_OK
    }

    fn get_channel_info(&self, info: *mut ASIOChannelInfo) -> ASIOError {
        let Some(info) = (unsafe { info.as_mut() }) else { return ASE_InvalidParameter };
        let st = self.lock();
        if st.handle == 0 {
            return ASE_NotPresent;
        }
        let input = info.isInput != 0;
        let count = if input { st.cfg.inputs } else { st.cfg.outputs };
        if info.channel < 0 || info.channel as u32 >= count {
            return ASE_InvalidParameter;
        }
        let ch = info.channel as u32;
        info.isActive = st.buffers.as_ref().is_some_and(|b| b.active.contains(&(input, ch))) as i32;
        info.channelGroup = 0;
        info.type_ = if st.cfg.sample_type == abi::SAMPLE_INT32 { ASIOSTInt32LSB } else { ASIOSTFloat32LSB };
        abi::put_str(&mut info.name, &format!("{}_{}", if input { "in" } else { "out" }, ch + 1));
        ASE_OK
    }

    fn create_buffers(&self, infos: *mut ASIOBufferInfo, num: i32, size: i32, callbacks: *mut ASIOCallbacks) -> ASIOError {
        if self.lock().buffers.is_some() {
            // Lenient: some hosts re-create without disposing first.
            self.dispose_buffers();
        }
        let mut st = self.lock();
        if st.handle == 0 {
            return ASE_NotPresent;
        }
        if st.running {
            return ASE_InvalidMode;
        }
        if infos.is_null() || callbacks.is_null() || num < 0 || num as u32 > st.cfg.inputs + st.cfg.outputs {
            return ASE_InvalidParameter;
        }
        let cfg = &st.cfg;
        let size_ok = if cfg.fixed_buffer_size {
            size as u32 == cfg.preferred_buffer_size
        } else {
            (abi::MIN_BUFFER_SIZE as i32..=abi::MAX_BUFFER_SIZE as i32).contains(&size)
        };
        if !size_ok {
            st.error = format!("unsupported buffer size {size}");
            return ASE_InvalidMode;
        }
        // SAFETY: host passes `num` entries.
        let infos = unsafe { std::slice::from_raw_parts_mut(infos, num as usize) };
        let mut active = Vec::with_capacity(infos.len());
        for i in infos.iter() {
            let input = i.isInput != 0;
            let limit = if input { cfg.inputs } else { cfg.outputs };
            if i.channelNum < 0 || i.channelNum as u32 >= limit || active.contains(&(input, i.channelNum as u32)) {
                return ASE_InvalidParameter;
            }
            active.push((input, i.channelNum as u32));
        }

        let half = size as usize * 4;
        let total = (half * 2 * infos.len()).max(1);
        // SAFETY: plain allocation; zeroed by the OS.
        let mem = unsafe { VirtualAlloc(null_mut(), total, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        if mem.is_null() {
            return ASE_NoMemory;
        }
        let mut descs = Vec::with_capacity(infos.len());
        for (k, i) in infos.iter_mut().enumerate() {
            // SAFETY: inside the allocation.
            let b0 = unsafe { (mem as *mut u8).add(k * 2 * half) };
            let b1 = unsafe { b0.add(half) };
            i.buffers = [b0.cast(), b1.cast()];
            descs.push(abi::ChannelBuffer {
                buffers: [b0 as usize as u64, b1 as usize as u64],
                channel: i.channelNum as u32,
                is_input: (i.isInput != 0) as u32,
            });
        }
        let mut p = abi::CreateBuffersParams {
            handle: st.handle,
            channels: descs.as_ptr() as usize as u64,
            num_channels: descs.len() as u32,
            buffer_size: size as u32,
            sample_type: st.cfg.sample_type,
            status: 0,
        };
        if unix::call(call::CREATE_BUFFERS, &mut p) != 0 || p.status != abi::status::OK {
            // SAFETY: allocated above.
            unsafe { VirtualFree(mem, 0, MEM_RELEASE) };
            st.error = format!("createBuffers failed ({})", p.status);
            return if p.status == abi::status::INVALID_MODE { ASE_InvalidMode } else { ASE_InvalidParameter };
        }
        st.buffers = Some(Buffers { mem, bufsize: size as u32, active });
        drop(st);

        // Probe host capabilities without holding the lock.
        // SAFETY: host-provided callbacks struct, valid until disposeBuffers.
        let cbs = unsafe { *callbacks };
        let supported = |sel: i32| -> bool {
            cbs.asioMessage.is_some_and(|m| unsafe { m(kAsioSelectorSupported, sel, null_mut(), null_mut()) } == 1)
        };
        let time_info = cbs.bufferSwitchTimeInfo.is_some()
            && (supported(kAsioSupportsTimeInfo)
                && cbs.asioMessage.is_some_and(|m| unsafe { m(kAsioSupportsTimeInfo, 0, null_mut(), null_mut()) } == 1)
                || cbs.bufferSwitch.is_none());
        self.cb.time_info.store(time_info, SeqCst);
        self.cb.can_reset.store(supported(kAsioResetRequest), SeqCst);
        self.cb.can_latency.store(supported(kAsioLatenciesChanged), SeqCst);
        self.cb.callbacks.store(callbacks, SeqCst);
        log!("createBuffers: {num} channels x {size} frames, timeInfo={time_info}");
        ASE_OK
    }

    fn dispose_buffers(&self) -> ASIOError {
        self.stop();
        let mut st = self.lock();
        let Some(b) = st.buffers.take() else { return ASE_InvalidMode };
        let mut p = abi::HandleParams { handle: st.handle, ..Default::default() };
        unix::call(call::DISPOSE_BUFFERS, &mut p);
        self.reap(&mut st, 2000);
        // SAFETY: allocated in create_buffers; the Unix side no longer uses it.
        unsafe { VirtualFree(b.mem, 0, MEM_RELEASE) };
        self.cb.callbacks.store(null_mut(), SeqCst);
        ASE_OK
    }

    fn control_panel(&self) -> ASIOError {
        // Prefer the native settings app (winepipe-settings); fall back to a
        // summary message box when it is not installed.
        if unix::load().is_ok() {
            let mut p = abi::HandleParams { handle: self.cb.handle.load(Relaxed), ..Default::default() };
            if unix::call(call::CONTROL_PANEL, &mut p) == 0 && p.status == abi::status::OK {
                return ASE_OK;
            }
        }
        let (cfg, handle) = {
            let st = self.lock();
            (st.cfg.clone(), st.handle)
        };
        let info = self.info(handle);
        let text = format!(
            "WinePipe ASIO driver {}\n\n\
             PipeWire node: {}\n\
             Inputs / outputs: {} / {}\n\
             Sample rate: {}\n\
             Graph quantum: {}\n\
             Preferred buffer size: {}{}\n\
             Sample format: {}\n\
             Autoconnect: {}\n\n\
             Install winepipe-settings for a settings window, or edit\n\
             HKEY_CURRENT_USER\\Software\\WinePipe (`winepipe-config`).\n\
             Changes apply the next time the driver is loaded.",
            env!("CARGO_PKG_VERSION"),
            cfg.client_name,
            cfg.inputs,
            cfg.outputs,
            info.map_or("unknown".into(), |i| format!("{} Hz", i.sample_rate)),
            info.map_or("unknown".into(), |i| i.quantum.to_string()),
            cfg.preferred_buffer_size,
            if cfg.fixed_buffer_size { " (fixed)" } else { "" },
            if cfg.sample_type == abi::SAMPLE_INT32 { "Int32LSB" } else { "Float32LSB" },
            if cfg.autoconnect { "on" } else { "off" },
        );
        // SAFETY: valid strings.
        unsafe { MessageBoxW(null_mut(), wide(&text).as_ptr(), wide("WinePipe").as_ptr(), MB_OK | MB_ICONINFORMATION) };
        ASE_OK
    }

    fn future(&self, selector: i32, opt: *mut c_void) -> ASIOError {
        match selector {
            kAsioCanTimeInfo => ASE_SUCCESS,
            abi::FUTURE_GET_STATS => {
                let handle = self.cb.handle.load(Relaxed);
                if handle == 0 || opt.is_null() {
                    return ASE_InvalidParameter;
                }
                let mut p = abi::StatsParams { handle, ..Default::default() };
                unix::call(call::GET_STATS, &mut p);
                // SAFETY: caller passes a StatsParams.
                unsafe { *(opt as *mut abi::StatsParams) = p };
                ASE_SUCCESS
            }
            _ => ASE_NotPresent,
        }
    }

    fn output_ready(&self) -> ASIOError {
        let handle = self.cb.handle.load(Relaxed);
        if handle != 0 {
            let mut p = abi::OutputReadyParams { handle, seq: self.cb.cur_seq.load(Relaxed), status: 0 };
            unix::call(call::OUTPUT_READY, &mut p);
        }
        ASE_OK
    }
}

/// The ASIO callback thread: blocks in the Unix side until PipeWire's RT
/// thread hands over a block, then runs the host's bufferSwitch.
unsafe extern "system" fn thread_proc(arg: *mut c_void) -> DWORD {
    let cb = Arc::from_raw(arg as *const CbShared);
    SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    let handle = cb.handle.load(SeqCst);
    let mut p = abi::WaitParams { handle, last_done: cb.start_seq.load(SeqCst), timeout_ms: 200, ..Default::default() };
    let r = catch_unwind(AssertUnwindSafe(|| loop {
        if cb.stop.load(Acquire) {
            break;
        }
        if unix::call(call::WAIT, &mut p) != 0 {
            break;
        }
        let Some(cbs) = cb.callbacks.load(Acquire).as_ref().copied() else { break };
        if p.events != 0 {
            deliver_events(&cb, &cbs, handle, p.events);
        }
        match p.result {
            abi::WAIT_SWITCH => {
                cb.cur_seq.store(p.seq, Release);
                // Map the block's CLOCK_MONOTONIC start onto the QPC timebase.
                let age = p.now_nsec.saturating_sub(p.system_nsec);
                let sys = qpc_ns().saturating_sub(age);
                cb.sample_pos.store(p.sample_position, Release);
                cb.system_ns.store(sys, Release);
                let idx = p.index as i32;
                if cb.time_info.load(Relaxed) {
                    if let Some(f) = cbs.bufferSwitchTimeInfo {
                        let t = &mut *cb.asio_time.get();
                        t.timeInfo.speed = 1.0;
                        t.timeInfo.systemTime = ASIOSamples::from_u64(sys);
                        t.timeInfo.samplePosition = ASIOSamples::from_u64(p.sample_position);
                        t.timeInfo.sampleRate = p.sample_rate;
                        t.timeInfo.flags = kSystemTimeValid | kSamplePositionValid | kSampleRateValid | kSpeedValid;
                        f(t, idx, 1);
                    }
                } else if let Some(f) = cbs.bufferSwitch {
                    f(idx, 1);
                }
                p.last_done = p.seq;
            }
            abi::WAIT_STOPPED => break,
            _ => {}
        }
    }));
    if r.is_err() {
        log!("callback thread panicked");
    }
    0
}

unsafe fn deliver_events(cb: &CbShared, cbs: &ASIOCallbacks, handle: u64, ev: u32) {
    log!("events {ev:#x}");
    if ev & abi::EVENT_RATE_CHANGED != 0 {
        let mut p = abi::InfoParams { handle, ..Default::default() };
        if unix::call(call::GET_INFO, &mut p) == 0 {
            if let Some(f) = cbs.sampleRateDidChange {
                f(p.sample_rate);
            }
        }
    }
    if let Some(msg) = cbs.asioMessage {
        if ev & abi::EVENT_LATENCY_CHANGED != 0 && cb.can_latency.load(Relaxed) {
            msg(kAsioLatenciesChanged, 0, null_mut(), null_mut());
        }
        if ev & abi::EVENT_RESET_REQUEST != 0 && cb.can_reset.load(Relaxed) {
            msg(kAsioResetRequest, 0, null_mut(), null_mut());
        }
    }
}

// ---- vtable ------------------------------------------------------------------

trait Fallback {
    fn fallback() -> Self;
}
impl Fallback for i32 {
    fn fallback() -> Self {
        ASE_HWMalfunction
    }
}
impl Fallback for () {
    fn fallback() -> Self {}
}

fn guard<R: Fallback>(f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| R::fallback())
}

/// Generates the ABI shims. IASIO methods are C++ virtuals: `thiscall` on
/// i686 and the normal Win64 convention on x86_64. IUnknown is `stdcall`.
macro_rules! asio_shims {
    ($($shim:ident => $method:ident($($a:ident: $t:ty),*) -> $r:ty;)*) => {
        $(
            #[cfg(target_arch = "x86")]
            unsafe extern "thiscall" fn $shim(this: *mut Driver $(, $a: $t)*) -> $r {
                guard(|| (*this).$method($($a),*))
            }
            #[cfg(not(target_arch = "x86"))]
            unsafe extern "system" fn $shim(this: *mut Driver $(, $a: $t)*) -> $r {
                guard(|| (*this).$method($($a),*))
            }
        )*
    };
}

asio_shims! {
    s_get_driver_name => get_driver_name(name: *mut u8) -> ();
    s_get_driver_version => get_driver_version() -> i32;
    s_get_error_message => get_error_message(s: *mut u8) -> ();
    s_start => start() -> i32;
    s_stop => stop() -> i32;
    s_get_channels => get_channels(i: *mut i32, o: *mut i32) -> i32;
    s_get_latencies => get_latencies(i: *mut i32, o: *mut i32) -> i32;
    s_get_buffer_size => get_buffer_size(a: *mut i32, b: *mut i32, c: *mut i32, d: *mut i32) -> i32;
    s_can_sample_rate => can_sample_rate(r: f64) -> i32;
    s_get_sample_rate => get_sample_rate(r: *mut f64) -> i32;
    s_set_sample_rate => set_sample_rate(r: f64) -> i32;
    s_get_clock_sources => get_clock_sources(c: *mut ASIOClockSource, n: *mut i32) -> i32;
    s_set_clock_source => set_clock_source(r: i32) -> i32;
    s_get_sample_position => get_sample_position(p: *mut ASIOSamples, t: *mut ASIOTimeStamp) -> i32;
    s_get_channel_info => get_channel_info(i: *mut ASIOChannelInfo) -> i32;
    s_create_buffers => create_buffers(i: *mut ASIOBufferInfo, n: i32, s: i32, c: *mut ASIOCallbacks) -> i32;
    s_dispose_buffers => dispose_buffers() -> i32;
    s_control_panel => control_panel() -> i32;
    s_future => future(sel: i32, opt: *mut c_void) -> i32;
    s_output_ready => output_ready() -> i32;
}

// `init` returns ASIOBool: a panic must read as FALSE, not ASE_HWMalfunction.
#[cfg(target_arch = "x86")]
unsafe extern "thiscall" fn s_init_bool(this: *mut Driver, sys: *mut c_void) -> i32 {
    catch_unwind(AssertUnwindSafe(|| (*this).init(sys))).unwrap_or(0)
}
#[cfg(not(target_arch = "x86"))]
unsafe extern "system" fn s_init_bool(this: *mut Driver, sys: *mut c_void) -> i32 {
    catch_unwind(AssertUnwindSafe(|| (*this).init(sys))).unwrap_or(0)
}

unsafe extern "system" fn s_query_interface(this: *mut Driver, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    Driver::query_interface(this, riid, ppv)
}
unsafe extern "system" fn s_add_ref(this: *mut Driver) -> u32 {
    Driver::add_ref(this)
}
unsafe extern "system" fn s_release(this: *mut Driver) -> u32 {
    catch_unwind(AssertUnwindSafe(|| Driver::release(this))).unwrap_or(0)
}

#[repr(C)]
pub struct Vtbl([*const c_void; 24]);
// SAFETY: immutable table of function pointers.
unsafe impl Sync for Vtbl {}

#[allow(clippy::fn_to_numeric_cast_any)]
static VTBL: Vtbl = Vtbl([
    s_query_interface as *const c_void,
    s_add_ref as *const c_void,
    s_release as *const c_void,
    s_init_bool as *const c_void,
    s_get_driver_name as *const c_void,
    s_get_driver_version as *const c_void,
    s_get_error_message as *const c_void,
    s_start as *const c_void,
    s_stop as *const c_void,
    s_get_channels as *const c_void,
    s_get_latencies as *const c_void,
    s_get_buffer_size as *const c_void,
    s_can_sample_rate as *const c_void,
    s_get_sample_rate as *const c_void,
    s_set_sample_rate as *const c_void,
    s_get_clock_sources as *const c_void,
    s_set_clock_source as *const c_void,
    s_get_sample_position as *const c_void,
    s_get_channel_info as *const c_void,
    s_create_buffers as *const c_void,
    s_dispose_buffers as *const c_void,
    s_control_panel as *const c_void,
    s_future as *const c_void,
    s_output_ready as *const c_void,
]);
