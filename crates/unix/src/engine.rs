//! The PipeWire side of one driver instance.
//!
//! Threading model:
//! * Control calls (init/start/stop/...) arrive on arbitrary host threads and
//!   take the `pw_thread_loop` lock for everything that touches PipeWire.
//! * PipeWire events (core/registry/metadata/filter state) run on the
//!   thread-loop thread, which holds that same lock while dispatching. Mutable
//!   control state therefore lives in `Cell`/`RefCell`s that are only touched
//!   under the lock.
//! * `process()` runs on PipeWire's RT data thread and only touches
//!   [`Shared`], whose session is guarded by the [`Handoff`] protocol.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering::*};

use winepipe_abi as abi;

use crate::pw::{self, *};
use crate::rt::{Chan, Cycle, Handoff, Session};
use crate::{log, warn};

pub struct Config {
    pub inputs: u32,
    pub outputs: u32,
    pub autoconnect: bool,
    pub client_name: String,
    pub app_name: String,
}

/// State reachable from the RT thread.
pub struct Shared {
    pub h: Handoff,
    session: UnsafeCell<Option<Box<Session>>>,
    in_ports: UnsafeCell<Vec<*mut c_void>>,
    out_ports: UnsafeCell<Vec<*mut c_void>>,
    in_bufs: UnsafeCell<Vec<*const f32>>,
    out_bufs: UnsafeCell<Vec<*mut f32>>,
    /// Graph rate seen by the last process() call.
    graph_rate: AtomicU32,
    /// Rate the host was last told about.
    reported_rate: AtomicU32,
    /// SCHED_FIFO priority of PipeWire's data thread (0 = not RT / unknown).
    data_prio: AtomicI32,
    prio_probed: AtomicU32,
}

// SAFETY: the UnsafeCells are written only before the filter is activated
// (ports) or while the handoff state is IDLE and no process() call is in
// flight (session); see Handoff::enter/halt.
unsafe impl Sync for Shared {}
unsafe impl Send for Shared {}

#[derive(Default)]
struct NodeInfo {
    name: String,
    class: String,
    priority: i32,
}

#[derive(Default)]
struct PortInfo {
    node: u32,
    output: bool,
    name: String,
    index: u32,
    monitor: bool,
    audio: bool,
}

#[derive(Default)]
struct Graph {
    nodes: HashMap<u32, NodeInfo>,
    ports: HashMap<u32, PortInfo>,
    default_sink: Option<String>,
    default_source: Option<String>,
    rate: u32,
    force_rate: u32,
    quantum: u32,
    force_quantum: u32,
    allowed_rates: Vec<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetaKind {
    Default,
    Settings,
}

struct Meta {
    engine: *const Engine,
    kind: MetaKind,
    proxy: *mut c_void,
    hook: Box<spa_hook>,
}

pub struct Engine {
    cfg: Config,
    tl: *mut pw_thread_loop,
    context: Cell<*mut pw_context>,
    core: Cell<*mut pw_core>,
    registry: Cell<*mut pw_registry>,
    filter: Cell<*mut pw_filter>,
    core_hook: Box<spa_hook>,
    registry_hook: Box<spa_hook>,
    filter_hook: Box<spa_hook>,
    // Boxed: PipeWire holds pointers to each Meta (listener data + hook).
    #[allow(clippy::vec_box)]
    meta: RefCell<Vec<Box<Meta>>>,
    links: RefCell<Vec<*mut pw_proxy>>,
    graph: RefCell<Graph>,
    sync_seq: Cell<c_int>,
    sync_done: Cell<bool>,
    filter_state: Cell<c_int>,
    connected: Cell<bool>,
    error: RefCell<String>,
    autoconnected: Cell<bool>,
    reset_requested: Cell<bool>,
    requested_rate: Cell<u32>,
    bufsize: Cell<u32>,
    pub shared: Box<Shared>,
}

struct Locked<'a>(&'a Engine);

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: paired with the lock taken in Engine::lock.
        unsafe { pw_thread_loop_unlock(self.0.tl) };
    }
}

static FILTER_EVENTS: pw_filter_events = pw_filter_events {
    version: PW_VERSION_FILTER_EVENTS,
    destroy: None,
    state_changed: Some(on_filter_state),
    io_changed: None,
    param_changed: None,
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_process),
    drained: None,
    command: None,
};

static CORE_EVENTS: pw_core_events = pw_core_events {
    version: PW_VERSION_CORE_EVENTS,
    info: None,
    done: Some(on_core_done),
    ping: None,
    error: Some(on_core_error),
    remove_id: None,
    bound_id: None,
    add_mem: None,
    remove_mem: None,
    bound_props: None,
};

static REGISTRY_EVENTS: pw_registry_events = pw_registry_events {
    version: PW_VERSION_REGISTRY_EVENTS,
    global: Some(on_global),
    global_remove: Some(on_global_remove),
};

static METADATA_EVENTS: pw_metadata_events =
    pw_metadata_events { version: PW_VERSION_METADATA_EVENTS, property: Some(on_metadata_property) };

fn c(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

impl Engine {
    pub fn new(cfg: Config) -> Result<Box<Engine>, String> {
        static INIT: std::sync::Once = std::sync::Once::new();
        // SAFETY: pw_init accepts NULL argc/argv.
        INIT.call_once(|| unsafe { pw_init(null_mut(), null_mut()) });

        let name = c("winepipe-loop");
        // SAFETY: plain constructor call.
        let tl = unsafe { pw_thread_loop_new(name.as_ptr(), std::ptr::null()) };
        if tl.is_null() {
            return Err("pw_thread_loop_new failed".into());
        }
        let e = Box::new(Engine {
            cfg,
            tl,
            context: Cell::new(null_mut()),
            core: Cell::new(null_mut()),
            registry: Cell::new(null_mut()),
            filter: Cell::new(null_mut()),
            core_hook: spa_hook::zeroed(),
            registry_hook: spa_hook::zeroed(),
            filter_hook: spa_hook::zeroed(),
            meta: RefCell::new(Vec::new()),
            links: RefCell::new(Vec::new()),
            graph: RefCell::new(Graph::default()),
            sync_seq: Cell::new(0),
            sync_done: Cell::new(false),
            filter_state: Cell::new(PW_FILTER_STATE_UNCONNECTED),
            connected: Cell::new(false),
            error: RefCell::new(String::new()),
            autoconnected: Cell::new(false),
            reset_requested: Cell::new(false),
            requested_rate: Cell::new(0),
            bufsize: Cell::new(0),
            shared: Box::new(Shared {
                h: Handoff::new(),
                session: UnsafeCell::new(None),
                in_ports: UnsafeCell::new(Vec::new()),
                out_ports: UnsafeCell::new(Vec::new()),
                in_bufs: UnsafeCell::new(Vec::new()),
                out_bufs: UnsafeCell::new(Vec::new()),
                graph_rate: AtomicU32::new(0),
                reported_rate: AtomicU32::new(0),
                data_prio: AtomicI32::new(0),
                prio_probed: AtomicU32::new(0),
            }),
        });
        // SAFETY: `e` is boxed, so the pointers handed to PipeWire stay valid
        // until Drop tears everything down.
        unsafe { e.connect()? };
        // The node stays active (silent while stopped) for its whole life, so
        // the graph rate reported to the host is the real one. With several
        // allowed rates, `clock.rate` is only the idle default. Give the graph
        // a moment to schedule us once.
        e.wait_for_rate(0, 500);
        log!("graph rate {}", e.shared.graph_rate.load(Relaxed));
        Ok(e)
    }

    fn lock(&self) -> Locked<'_> {
        // SAFETY: tl is valid for the engine's lifetime.
        unsafe { pw_thread_loop_lock(self.tl) };
        Locked(self)
    }

    fn data(&self) -> *mut c_void {
        self as *const Engine as *mut c_void
    }

    unsafe fn connect(&self) -> Result<(), String> {
        let ctx = pw_context_new(pw_thread_loop_get_loop(self.tl), null_mut(), 0);
        if ctx.is_null() {
            return Err("pw_context_new failed".into());
        }
        self.context.set(ctx);
        if pw_thread_loop_start(self.tl) < 0 {
            return Err("pw_thread_loop_start failed".into());
        }
        let _g = self.lock();

        let app = if self.cfg.app_name.is_empty() { "WinePipe".to_string() } else { self.cfg.app_name.clone() };
        let core = pw_context_connect(ctx, properties(&[("application.name", app.clone())]), 0);
        if core.is_null() {
            return Err(format!("cannot connect to PipeWire: {}", std::io::Error::last_os_error()));
        }
        self.core.set(core);
        self.connected.set(true);
        core_add_listener(core, &*self.core_hook as *const _ as *mut _, &CORE_EVENTS, self.data());

        let reg = core_get_registry(core);
        if reg.is_null() {
            return Err("cannot get PipeWire registry".into());
        }
        self.registry.set(reg);
        registry_add_listener(reg, &*self.registry_hook as *const _ as *mut _, &REGISTRY_EVENTS, self.data());

        let desc = if self.cfg.app_name.is_empty() {
            "WinePipe ASIO".to_string()
        } else {
            format!("{} (WinePipe ASIO)", self.cfg.app_name)
        };
        let fname = c(&self.cfg.client_name);
        let props = properties(&[
            ("media.type", "Audio".into()),
            ("media.category", "Duplex".into()),
            ("media.role", "DSP".into()),
            ("media.name", desc.clone()),
            ("node.name", self.cfg.client_name.clone()),
            ("node.description", desc),
            ("application.name", app),
            ("node.always-process", "true".into()),
            ("node.want-driver", "true".into()),
            ("node.autoconnect", "false".into()),
        ]);
        let filter = pw_filter_new(core, fname.as_ptr(), props);
        if filter.is_null() {
            return Err("pw_filter_new failed".into());
        }
        self.filter.set(filter);
        pw_filter_add_listener(filter, &*self.filter_hook as *const _ as *mut _, &FILTER_EVENTS, self.data());

        let sh = &*self.shared;
        let ins = &mut *sh.in_ports.get();
        let outs = &mut *sh.out_ports.get();
        for (dir, count, prefix, list) in [
            (PW_DIRECTION_INPUT, self.cfg.inputs, "in", &mut *ins),
            (PW_DIRECTION_OUTPUT, self.cfg.outputs, "out", &mut *outs),
        ] {
            for i in 1..=count {
                let p = properties(&[
                    ("format.dsp", "32 bit float mono audio".into()),
                    ("port.name", format!("{prefix}_{i}")),
                ]);
                let port = pw_filter_add_port(filter, dir, PW_FILTER_PORT_FLAG_MAP_BUFFERS, 8, p, null_mut(), 0);
                if port.is_null() {
                    return Err(format!("cannot create port {prefix}_{i}"));
                }
                list.push(port);
            }
        }
        *sh.in_bufs.get() = vec![std::ptr::null(); ins.len()];
        *sh.out_bufs.get() = vec![null_mut(); outs.len()];

        if pw_filter_connect(filter, PW_FILTER_FLAG_RT_PROCESS | PW_FILTER_FLAG_INACTIVE, null_mut(), 0) < 0 {
            return Err("pw_filter_connect failed".into());
        }
        if !self.wait_until(5000, || {
            let s = self.filter_state.get();
            s == PW_FILTER_STATE_PAUSED || s == PW_FILTER_STATE_STREAMING || s == PW_FILTER_STATE_ERROR
        }) || self.filter_state.get() == PW_FILTER_STATE_ERROR
        {
            let err = self.error.borrow().clone();
            return Err(format!("PipeWire filter did not come up: {err}"));
        }
        // First roundtrip delivers the globals (and binds the metadata),
        // the second one their properties.
        self.roundtrip();
        self.roundtrip();
        pw_filter_set_active(filter, true);
        let g = self.graph.borrow();
        log!(
            "connected: node {} rate {} quantum {} allowed {:?} sink {:?} source {:?}",
            pw_filter_get_node_id(filter),
            g.rate,
            g.quantum,
            g.allowed_rates,
            g.default_sink,
            g.default_source
        );
        Ok(())
    }

    /// Wait (lock held) until `cond` holds or `ms` elapse.
    unsafe fn wait_until(&self, ms: i64, mut cond: impl FnMut() -> bool) -> bool {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        pw_thread_loop_get_time(self.tl, &mut ts, ms * 1_000_000);
        loop {
            if cond() {
                return true;
            }
            if !self.connected.get() {
                return false;
            }
            if pw_thread_loop_timed_wait_full(self.tl, &ts) != 0 {
                return cond();
            }
        }
    }

    /// Sync with the server (lock held) so all pending events are processed.
    unsafe fn roundtrip(&self) -> bool {
        if !self.connected.get() {
            return false;
        }
        self.sync_done.set(false);
        self.sync_seq.set(core_sync(self.core.get(), PW_ID_CORE, 0));
        self.wait_until(3000, || self.sync_done.get())
    }

    unsafe fn update_props(&self, pairs: &[(&str, String)]) {
        let f = self.filter.get();
        if f.is_null() {
            return;
        }
        let d = pw::Dict::new(pairs);
        let raw = d.raw();
        pw_filter_update_properties(f, null_mut(), &raw);
    }

    /// Ask the host (once) to tear down and re-initialise the driver.
    /// Called from the loop thread.
    fn request_reset(&self) {
        if !self.reset_requested.replace(true) {
            self.shared.h.raise(abi::EVENT_RESET_REQUEST);
        }
    }

    /// Poll (without the loop lock) until process() has seen a graph rate
    /// other than `not`, or `ms` elapse.
    fn wait_for_rate(&self, not: u32, ms: u64) -> u32 {
        let until = crate::futex::now_ns() + ms * 1_000_000;
        loop {
            let r = self.shared.graph_rate.load(Relaxed);
            if (r != 0 && r != not) || crate::futex::now_ns() >= until {
                return r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn running(&self) -> bool {
        self.shared.h.running()
    }

    fn current_rate(&self) -> u32 {
        let g = self.graph.borrow();
        let seen = self.shared.graph_rate.load(Relaxed);
        let req = self.requested_rate.get();
        if req != 0 && req == seen {
            // applied; from now on just follow the graph
            self.requested_rate.set(0);
        }
        if self.running() && seen != 0 {
            seen
        } else if req != 0 && req != seen {
            // a setSampleRate() the graph has not applied yet
            req
        } else if seen != 0 {
            seen
        } else if g.force_rate != 0 {
            g.force_rate
        } else if g.rate != 0 {
            g.rate
        } else {
            48000
        }
    }

    // ---- unix call implementations ------------------------------------

    pub fn get_info(&self, p: &mut abi::InfoParams) {
        let _g = self.lock();
        let rate = self.current_rate();
        self.shared.reported_rate.store(rate, Relaxed);
        p.sample_rate = f64::from(rate);
        let g = self.graph.borrow();
        let mut rates = g.allowed_rates.clone();
        if !rates.contains(&rate) {
            rates.insert(0, rate);
        }
        rates.truncate(abi::MAX_RATES);
        p.num_allowed_rates = rates.len() as u32;
        p.allowed_rates[..rates.len()].copy_from_slice(&rates);

        let st = &self.shared.h.stats;
        let seen_q = st.quantum.load(Relaxed);
        let b = self.bufsize.get();
        let q = if self.running() && seen_q != 0 {
            seen_q
        } else if b != 0 {
            b
        } else if g.force_quantum != 0 {
            g.force_quantum
        } else {
            g.quantum
        };
        p.quantum = q;
        let base = q.max(b);
        let extra = if self.running() { st.fifo_latency.load(Relaxed) } else { 0 };
        p.input_latency = base;
        p.output_latency = base + extra;
        p.connected = self.connected.get() as u32;
        p.status = abi::status::OK;
    }

    pub fn set_sample_rate(&self, rate: f64) -> i32 {
        let _g = self.lock();
        if !(1.0..=1_000_000.0).contains(&rate) || rate.fract() != 0.0 {
            return abi::status::INVALID_PARAMETER;
        }
        let r = rate as u32;
        if r == self.current_rate() {
            return abi::status::OK;
        }
        if !self.graph.borrow().allowed_rates.contains(&r) {
            return abi::status::UNSUPPORTED;
        }
        log!("requesting graph rate {r}");
        self.requested_rate.set(r);
        self.shared.reported_rate.store(r, Relaxed);
        // SAFETY: lock held.
        unsafe { self.update_props(&[("node.force-rate", r.to_string()), ("node.rate", format!("1/{r}"))]) };
        drop(_g);
        // Our node is active, so the graph switches within a few cycles.
        let now = self.wait_for_rate(self.shared.graph_rate.load(Relaxed), 500);
        log!("graph rate now {now}");
        abi::status::OK
    }

    pub fn create_buffers(&self, p: &abi::CreateBuffersParams) -> i32 {
        if self.running() {
            return abi::status::INVALID_MODE;
        }
        let b = p.buffer_size;
        if !(abi::MIN_BUFFER_SIZE..=abi::MAX_BUFFER_SIZE).contains(&b)
            || (p.sample_type != abi::SAMPLE_FLOAT32 && p.sample_type != abi::SAMPLE_INT32)
            || p.num_channels > self.cfg.inputs + self.cfg.outputs
            || (p.num_channels > 0 && p.channels == 0)
        {
            return abi::status::INVALID_PARAMETER;
        }
        // SAFETY: the PE side passes `num_channels` ChannelBuffers.
        let descs = if p.num_channels == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(p.channels as usize as *const abi::ChannelBuffer, p.num_channels as usize) }
        };
        let mut ins = Vec::new();
        let mut outs = Vec::new();
        let mut used_out = vec![false; self.cfg.outputs as usize];
        let mut used_in = vec![false; self.cfg.inputs as usize];
        for d in descs {
            let (limit, used, list) = if d.is_input != 0 {
                (self.cfg.inputs, &mut used_in, &mut ins)
            } else {
                (self.cfg.outputs, &mut used_out, &mut outs)
            };
            if d.channel >= limit || used[d.channel as usize] || d.buffers[0] == 0 || d.buffers[1] == 0 {
                return abi::status::INVALID_PARAMETER;
            }
            used[d.channel as usize] = true;
            list.push(Chan {
                port: d.channel as usize,
                buf: [d.buffers[0] as usize as *mut u8, d.buffers[1] as usize as *mut u8],
            });
        }
        let idle: Vec<usize> = (0..self.cfg.outputs as usize).filter(|&i| !used_out[i]).collect();
        let session = Box::new(Session::new(b as usize, p.sample_type, ins, outs, idle));

        let _g = self.lock();
        // SAFETY: not running, so no process() call can be inside the session.
        unsafe { *self.shared.session.get() = Some(session) };
        self.bufsize.set(b);
        let rate = self.current_rate();
        // SAFETY: lock held.
        unsafe {
            // Keep the graph at the rate the host was told about: applying the
            // quantum restarts the driver, and with several allowed rates
            // PipeWire would otherwise fall back to `clock.rate`.
            self.update_props(&[
                ("node.force-quantum", b.to_string()),
                ("node.latency", format!("{b}/{rate}")),
                ("node.rate", format!("1/{rate}")),
                ("node.lock-rate", "true".into()),
            ]);
        }
        log!("buffers: {} channels, {} frames, type {}", descs.len(), b, p.sample_type);
        abi::status::OK
    }

    pub fn dispose_buffers(&self) -> i32 {
        self.stop();
        let _g = self.lock();
        // SAFETY: stopped; see create_buffers.
        let had = unsafe { (*self.shared.session.get()).take().is_some() };
        self.bufsize.set(0);
        // SAFETY: lock held.
        unsafe { self.update_props(&[("node.force-quantum", "0".into()), ("node.lock-rate", "false".into())]) };
        if had {
            abi::status::OK
        } else {
            abi::status::INVALID_MODE
        }
    }

    pub fn start(&self, p: &mut abi::StartParams) -> i32 {
        if self.running() {
            return abi::status::INVALID_MODE;
        }
        let _g = self.lock();
        if !self.connected.get() || self.filter_state.get() == PW_FILTER_STATE_ERROR {
            // The host was (or is about to be) asked to reset; a "started"
            // stream that can never deliver a block would just hang it.
            self.shared.h.raise(abi::EVENT_RESET_REQUEST);
            return abi::status::NOT_CONNECTED;
        }
        // SAFETY: not running.
        let Some(session) = (unsafe { (*self.shared.session.get()).as_mut() }) else {
            return abi::status::INVALID_MODE;
        };
        session.reset();
        p.seq = self.shared.h.begin();
        // SAFETY: lock held.
        unsafe {
            pw_filter_set_active(self.filter.get(), true);
            if self.cfg.autoconnect && !self.autoconnected.get() {
                self.autoconnected.set(true);
                self.autoconnect();
            }
        }
        log!("started");
        abi::status::OK
    }

    pub fn stop(&self) -> i32 {
        if !self.running() {
            return abi::status::OK;
        }
        // The node stays active (and silent) so the rate keeps being tracked.
        self.shared.h.halt();
        log!("stopped");
        abi::status::OK
    }

    pub fn wait(&self, p: &mut abi::WaitParams) {
        thread_local!(static PROMOTED: Cell<bool> = const { Cell::new(false) });
        if !PROMOTED.with(|c| c.replace(true)) {
            self.promote_current_thread();
        }
        self.shared.h.wait(p);
    }

    pub fn output_ready(&self, seq: u32) {
        self.shared.h.complete(seq);
    }

    pub fn stats(&self, p: &mut abi::StatsParams) {
        let s = &self.shared.h.stats;
        p.cycles = s.cycles.load(Relaxed);
        p.switches = s.switches.load(Relaxed);
        p.late = s.late.load(Relaxed);
        p.skipped = s.skipped.load(Relaxed);
        p.underrun_frames = s.underrun_frames.load(Relaxed);
        p.handoff_max_ns = s.handoff_max_ns.load(Relaxed);
        p.handoff_last_ns = s.handoff_last_ns.load(Relaxed);
        p.handoff_total_ns = s.handoff_total_ns.load(Relaxed);
        p.graph_xruns = s.graph_xruns.load(Relaxed);
        p.quantum = s.quantum.load(Relaxed);
        p.rt_priority = s.rt_priority.load(Relaxed);
        p.fifo_mode = s.fifo_mode.load(Relaxed);
        p.fifo_latency = s.fifo_latency.load(Relaxed);
        p.status = abi::status::OK;
    }

    /// Give the calling (callback) thread the same SCHED_FIFO priority as
    /// PipeWire's data thread: it runs strictly in lock-step with it.
    fn promote_current_thread(&self) {
        let env_prio = std::env::var("WINEPIPE_RT_PRIO").ok().and_then(|v| v.parse::<i32>().ok());
        if env_prio == Some(0) {
            self.shared.h.stats.rt_priority.store(0, Relaxed);
            return;
        }
        let seen = self.shared.data_prio.load(Relaxed);
        let want = env_prio.unwrap_or(if seen > 0 { seen } else { -1 });
        // SAFETY: plain libc/PipeWire calls on the current thread.
        unsafe {
            let me = libc::pthread_self();
            let mut res = -libc::ENOSYS;
            let utils = {
                let _g = self.lock();
                pw_context_get_object(self.context.get(), TYPE_THREAD_UTILS.as_ptr() as *const c_char)
            };
            // Called without the loop lock: module-rt may need the context's
            // main loop (ours) to complete an RTKit request.
            if !utils.is_null() {
                res = thread_utils_acquire_rt(utils, me as *mut spa_thread, want);
            }
            if res < 0 {
                let max = libc::sched_get_priority_max(libc::SCHED_FIFO);
                let prio = if want > 0 { want } else { 83 }.min(max);
                let sp = libc::sched_param { sched_priority: prio };
                let r = libc::pthread_setschedparam(me, libc::SCHED_FIFO | libc::SCHED_RESET_ON_FORK, &sp);
                if r != 0 {
                    warn!("could not get realtime priority for the callback thread (module-rt: {res}, sched: {r})");
                    self.shared.h.stats.rt_priority.store(-r, Relaxed);
                    return;
                }
            }
            let mut policy = 0;
            let mut sp = libc::sched_param { sched_priority: 0 };
            libc::pthread_getschedparam(me, &mut policy, &mut sp);
            let rt = policy & !libc::SCHED_RESET_ON_FORK;
            let prio = if rt == libc::SCHED_FIFO || rt == libc::SCHED_RR { sp.sched_priority } else { 0 };
            self.shared.h.stats.rt_priority.store(prio, Relaxed);
            log!("callback thread priority: {prio} (data thread {seen})");
        }
    }

    /// Link outputs to the default sink and inputs from the default source.
    /// Lock held.
    unsafe fn autoconnect(&self) {
        self.roundtrip();
        let node = pw_filter_get_node_id(self.filter.get());
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        {
            let g = self.graph.borrow();
            let mine = |output: bool, prefix: &str, count: u32| -> Vec<u32> {
                (1..=count)
                    .filter_map(|i| {
                        let name = format!("{prefix}_{i}");
                        g.ports.iter().find(|(_, p)| p.node == node && p.output == output && p.name == name).map(|(id, _)| *id)
                    })
                    .collect()
            };
            let my_out = mine(true, "out", self.cfg.outputs);
            let my_in = mine(false, "in", self.cfg.inputs);

            if let Some(sink) = g.pick(g.default_sink.as_deref(), |c| c == "Audio/Sink" || c == "Audio/Duplex") {
                let targets = g.ports_of(sink, false);
                if my_out.len() == 1 && targets.len() >= 2 {
                    pairs.push((my_out[0], targets[0]));
                    pairs.push((my_out[0], targets[1]));
                } else {
                    pairs.extend(my_out.iter().copied().zip(targets.iter().copied()));
                }
                log!("autoconnect: sink {} ({} ports)", g.nodes[&sink].name, targets.len());
            }
            if let Some(src) = g.pick(g.default_source.as_deref(), |c| c.starts_with("Audio/Source") || c == "Audio/Duplex") {
                let sources = g.ports_of(src, true);
                if my_in.len() >= 2 && sources.len() == 1 {
                    pairs.push((sources[0], my_in[0]));
                    pairs.push((sources[0], my_in[1]));
                } else {
                    pairs.extend(sources.iter().copied().zip(my_in.iter().copied()));
                }
                log!("autoconnect: source {} ({} ports)", g.nodes[&src].name, sources.len());
            }
        }
        let factory = c("link-factory");
        for (out_port, in_port) in pairs {
            let d = pw::Dict::new(&[
                ("link.output.port", out_port.to_string()),
                ("link.input.port", in_port.to_string()),
                ("object.linger", "false".into()),
            ]);
            let raw = d.raw();
            let proxy = core_create_object(
                self.core.get(),
                factory.as_ptr(),
                TYPE_LINK.as_ptr() as *const c_char,
                PW_VERSION_LINK,
                &raw,
            );
            if proxy.is_null() {
                warn!("could not link port {out_port} -> {in_port}");
            } else {
                self.links.borrow_mut().push(proxy as *mut pw_proxy);
            }
        }
    }
}

impl Graph {
    /// The node named `default` if it exists and matches `class`, else the
    /// highest-priority node of that class.
    fn pick(&self, default: Option<&str>, class: impl Fn(&str) -> bool) -> Option<u32> {
        if let Some(name) = default {
            if let Some((id, _)) = self.nodes.iter().find(|(_, n)| n.name == name && class(&n.class)) {
                return Some(*id);
            }
        }
        self.nodes
            .iter()
            .filter(|(_, n)| class(&n.class))
            .max_by_key(|(id, n)| (n.priority, std::cmp::Reverse(**id)))
            .map(|(id, _)| *id)
    }

    /// Audio ports of `node` in the given direction, in port order.
    fn ports_of(&self, node: u32, output: bool) -> Vec<u32> {
        let mut v: Vec<(u32, u32)> = self
            .ports
            .iter()
            .filter(|(_, p)| p.node == node && p.output == output && !p.monitor && p.audio)
            .map(|(id, p)| (p.index, *id))
            .collect();
        v.sort();
        v.into_iter().map(|(_, id)| id).collect()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.h.halt();
        // SAFETY: tear down in reverse order of creation; the hooks and
        // listener data stay alive until the objects referencing them are gone.
        unsafe {
            {
                let _g = self.lock();
                let f = self.filter.replace(null_mut());
                if !f.is_null() {
                    pw_filter_destroy(f);
                }
                for l in self.links.borrow_mut().drain(..) {
                    pw_proxy_destroy(l);
                }
                for m in self.meta.borrow_mut().iter() {
                    pw_proxy_destroy(m.proxy as *mut pw_proxy);
                }
                let r = self.registry.replace(null_mut());
                if !r.is_null() {
                    pw_proxy_destroy(r as *mut pw_proxy);
                }
                let core = self.core.replace(null_mut());
                if !core.is_null() {
                    pw_core_disconnect(core);
                }
            }
            pw_thread_loop_stop(self.tl);
            let ctx = self.context.replace(null_mut());
            if !ctx.is_null() {
                pw_context_destroy(ctx);
            }
            pw_thread_loop_destroy(self.tl);
        }
        self.meta.borrow_mut().clear();
    }
}

// ---- PipeWire callbacks -----------------------------------------------------

unsafe extern "C" fn on_process(data: *mut c_void, pos: *mut spa_io_position) {
    let e = &*(data as *const Engine);
    let sh = &*e.shared;
    if sh.prio_probed.swap(1, Relaxed) == 0 {
        let mut policy = 0;
        let mut sp = libc::sched_param { sched_priority: 0 };
        libc::pthread_getschedparam(libc::pthread_self(), &mut policy, &mut sp);
        let rt = policy & !libc::SCHED_RESET_ON_FORK;
        if rt == libc::SCHED_FIFO || rt == libc::SCHED_RR {
            sh.data_prio.store(sp.sched_priority, Relaxed);
        }
    }
    let ins = &*sh.in_ports.get();
    let outs = &*sh.out_ports.get();
    if pos.is_null() {
        return;
    }
    let clk = &(*pos).clock;
    let n = clk.duration as usize;
    let rate = clk.rate.denom;
    if n == 0 || rate == 0 {
        return;
    }
    sh.graph_rate.store(rate, Relaxed);

    if !sh.h.enter() {
        for &p in outs {
            let b = pw_filter_get_dsp_buffer(p, n as u32) as *mut f32;
            if !b.is_null() {
                std::ptr::write_bytes(b, 0, n);
            }
        }
        return;
    }
    let st = &sh.h.stats;
    st.cycles.fetch_add(1, Relaxed);
    if clk.flags & SPA_IO_CLOCK_FLAG_XRUN_RECOVER != 0 {
        st.graph_xruns.fetch_add(1, Relaxed);
    }
    let reported = sh.reported_rate.load(Relaxed);
    if reported != 0 && reported != rate {
        sh.reported_rate.store(rate, Relaxed);
        sh.h.raise(abi::EVENT_RATE_CHANGED);
    }
    let in_bufs = &mut *sh.in_bufs.get();
    let out_bufs = &mut *sh.out_bufs.get();
    for (b, &p) in in_bufs.iter_mut().zip(ins) {
        *b = pw_filter_get_dsp_buffer(p, n as u32) as *const f32;
    }
    for (b, &p) in out_bufs.iter_mut().zip(outs) {
        *b = pw_filter_get_dsp_buffer(p, n as u32) as *mut f32;
    }
    if let Some(s) = (*sh.session.get()).as_mut() {
        let cyc = Cycle {
            n,
            rate,
            nsec: clk.nsec,
            next_nsec: clk.next_nsec,
            freewheel: clk.flags & SPA_IO_CLOCK_FLAG_FREEWHEEL != 0,
        };
        s.process(&sh.h, in_bufs, out_bufs, &cyc);
    }
    sh.h.leave();
}

unsafe extern "C" fn on_filter_state(data: *mut c_void, _old: c_int, state: c_int, error: *const c_char) {
    let e = &*(data as *const Engine);
    e.filter_state.set(state);
    if state == PW_FILTER_STATE_ERROR {
        let msg = cstr(error).unwrap_or("unknown error").to_string();
        warn!("filter error: {msg}");
        *e.error.borrow_mut() = msg;
        e.request_reset();
    }
    pw_thread_loop_signal(e.tl, false);
}

unsafe extern "C" fn on_core_done(data: *mut c_void, id: u32, seq: c_int) {
    let e = &*(data as *const Engine);
    if id == PW_ID_CORE && seq == e.sync_seq.get() {
        e.sync_done.set(true);
        pw_thread_loop_signal(e.tl, false);
    }
}

unsafe extern "C" fn on_core_error(data: *mut c_void, id: u32, _seq: c_int, res: c_int, msg: *const c_char) {
    let e = &*(data as *const Engine);
    let msg = cstr(msg).unwrap_or("").to_string();
    warn!("PipeWire error on object {id}: {msg} ({res})");
    if id == PW_ID_CORE {
        *e.error.borrow_mut() = msg;
        if res == -libc::EPIPE {
            e.connected.set(false);
            e.request_reset();
        }
    }
    pw_thread_loop_signal(e.tl, false);
}

unsafe extern "C" fn on_global(
    data: *mut c_void,
    id: u32,
    _perm: u32,
    type_: *const c_char,
    _version: u32,
    props: *const spa_dict,
) {
    let e = &*(data as *const Engine);
    let Some(ty) = cstr(type_) else { return };
    let get = |k: &str| dict_get(props, k);
    match ty {
        TYPE_NODE => {
            let n = NodeInfo {
                name: get("node.name").unwrap_or("").to_string(),
                class: get("media.class").unwrap_or("").to_string(),
                priority: get("priority.session").and_then(|v| v.parse().ok()).unwrap_or(0),
            };
            e.graph.borrow_mut().nodes.insert(id, n);
        }
        TYPE_PORT => {
            let p = PortInfo {
                node: get("node.id").and_then(|v| v.parse().ok()).unwrap_or(u32::MAX),
                output: get("port.direction") == Some("out"),
                name: get("port.name").unwrap_or("").to_string(),
                index: get("port.id").and_then(|v| v.parse().ok()).unwrap_or(u32::MAX),
                monitor: get("port.monitor") == Some("true"),
                audio: get("format.dsp").is_none_or(|f| f.contains("audio")),
            };
            e.graph.borrow_mut().ports.insert(id, p);
        }
        _ if ty.as_bytes() == &TYPE_METADATA[..TYPE_METADATA.len() - 1] => {
            let kind = match get("metadata.name") {
                Some("default") => MetaKind::Default,
                Some("settings") => MetaKind::Settings,
                _ => return,
            };
            if e.meta.borrow().iter().any(|m| m.kind == kind) {
                return;
            }
            let proxy = registry_bind(e.registry.get(), id, TYPE_METADATA.as_ptr() as *const c_char, PW_VERSION_METADATA);
            if proxy.is_null() {
                return;
            }
            let m = Box::new(Meta { engine: e, kind, proxy, hook: spa_hook::zeroed() });
            metadata_add_listener(proxy, &*m.hook as *const _ as *mut _, &METADATA_EVENTS, &*m as *const Meta as *mut c_void);
            e.meta.borrow_mut().push(m);
        }
        _ => {}
    }
}

unsafe extern "C" fn on_global_remove(data: *mut c_void, id: u32) {
    let e = &*(data as *const Engine);
    let mut g = e.graph.borrow_mut();
    g.nodes.remove(&id);
    g.ports.remove(&id);
}

unsafe extern "C" fn on_metadata_property(
    data: *mut c_void,
    subject: u32,
    key: *const c_char,
    _type: *const c_char,
    value: *const c_char,
) -> c_int {
    let m = &*(data as *const Meta);
    let e = &*m.engine;
    if subject != PW_ID_CORE {
        return 0;
    }
    let mut g = e.graph.borrow_mut();
    let value = cstr(value);
    let Some(key) = cstr(key) else {
        // NULL key clears everything
        if m.kind == MetaKind::Default {
            g.default_sink = None;
            g.default_source = None;
        }
        return 0;
    };
    let num = || value.and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(0);
    match (m.kind, key) {
        (MetaKind::Default, "default.audio.sink") => g.default_sink = value.and_then(json_name),
        (MetaKind::Default, "default.audio.source") => g.default_source = value.and_then(json_name),
        (MetaKind::Settings, "clock.rate") => g.rate = num(),
        (MetaKind::Settings, "clock.force-rate") => g.force_rate = num(),
        (MetaKind::Settings, "clock.quantum") => g.quantum = num(),
        (MetaKind::Settings, "clock.force-quantum") => g.force_quantum = num(),
        (MetaKind::Settings, "clock.allowed-rates") => {
            g.allowed_rates = value.map(parse_rate_list).unwrap_or_default();
        }
        _ => {}
    }
    0
}

/// Extract `"name"` from metadata JSON like `{ "name": "alsa_output.x" }`.
pub fn json_name(v: &str) -> Option<String> {
    let rest = &v[v.find("\"name\"")? + 6..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            c => out.push(c),
        }
    }
    None
}

/// Parse SPA JSON arrays like `[ 44100 48000 ]` or `[44100,48000]`.
pub fn parse_rate_list(v: &str) -> Vec<u32> {
    v.split(|c: char| !c.is_ascii_digit()).filter_map(|s| s.parse().ok()).filter(|&r| r > 0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metadata_values() {
        assert_eq!(json_name(r#"{ "name": "alsa_output.pci-0000_00_1f.3.analog-stereo" }"#).as_deref(), Some("alsa_output.pci-0000_00_1f.3.analog-stereo"));
        assert_eq!(json_name(r#"{"name":"a\"b"}"#).as_deref(), Some("a\"b"));
        assert_eq!(json_name("{}"), None);
        assert_eq!(parse_rate_list("[ 44100 48000 ]"), vec![44100, 48000]);
        assert_eq!(parse_rate_list("[44100,48000,96000]"), vec![44100, 48000, 96000]);
        assert!(parse_rate_list("[ ]").is_empty());
    }
}
