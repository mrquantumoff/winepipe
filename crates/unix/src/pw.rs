//! Minimal hand-written bindings for the parts of libpipewire-0.3 / SPA that
//! WinePipe uses. Layouts follow the PipeWire 1.x headers; the inline helpers
//! of the C API (`pw_core_sync`, `pw_registry_bind`, ...) are re-implemented
//! on top of the `spa_interface` method tables they dispatch through.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_void};

pub enum pw_thread_loop {}
pub enum pw_loop {}
pub enum pw_context {}
pub enum pw_core {}
pub enum pw_registry {}
pub enum pw_proxy {}
pub enum pw_properties {}
pub enum pw_filter {}
pub enum pw_core_info {}
pub enum spa_pod {}
pub enum spa_command {}
pub enum spa_thread {}

#[repr(C)]
pub struct spa_list {
    pub next: *mut spa_list,
    pub prev: *mut spa_list,
}

#[repr(C)]
pub struct spa_callbacks {
    pub funcs: *const c_void,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct spa_interface {
    pub type_: *const c_char,
    pub version: u32,
    pub cb: spa_callbacks,
}

#[repr(C)]
pub struct spa_hook {
    pub link: spa_list,
    pub cb: spa_callbacks,
    pub removed: Option<unsafe extern "C" fn(*mut spa_hook)>,
    pub priv_: *mut c_void,
}

impl spa_hook {
    pub fn zeroed() -> Box<spa_hook> {
        Box::new(spa_hook {
            link: spa_list { next: std::ptr::null_mut(), prev: std::ptr::null_mut() },
            cb: spa_callbacks { funcs: std::ptr::null(), data: std::ptr::null_mut() },
            removed: None,
            priv_: std::ptr::null_mut(),
        })
    }
}

#[repr(C)]
pub struct spa_dict_item {
    pub key: *const c_char,
    pub value: *const c_char,
}

#[repr(C)]
pub struct spa_dict {
    pub flags: u32,
    pub n_items: u32,
    pub items: *const spa_dict_item,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct spa_fraction {
    pub num: u32,
    pub denom: u32,
}

/// Leading part of `struct spa_io_clock`; only read through a pointer.
#[repr(C)]
pub struct spa_io_clock {
    pub flags: u32,
    pub id: u32,
    pub name: [c_char; 64],
    pub nsec: u64,
    pub rate: spa_fraction,
    pub position: u64,
    pub duration: u64,
    pub delay: i64,
    pub rate_diff: f64,
    pub next_nsec: u64,
    pub target_rate: spa_fraction,
    pub target_duration: u64,
    pub target_seq: u32,
    pub cycle: u32,
    pub xrun: u64,
}

/// `struct spa_io_position` starts with the driver clock; that is all we read.
#[repr(C)]
pub struct spa_io_position {
    pub clock: spa_io_clock,
}

pub const SPA_IO_CLOCK_FLAG_FREEWHEEL: u32 = 1 << 0;
pub const SPA_IO_CLOCK_FLAG_XRUN_RECOVER: u32 = 1 << 1;

pub const PW_ID_CORE: u32 = 0;

pub const PW_DIRECTION_INPUT: c_int = 0;
pub const PW_DIRECTION_OUTPUT: c_int = 1;

pub const PW_FILTER_FLAG_INACTIVE: c_int = 1 << 0;
pub const PW_FILTER_FLAG_RT_PROCESS: c_int = 1 << 2;
pub const PW_FILTER_PORT_FLAG_MAP_BUFFERS: c_int = 1 << 0;

pub const PW_FILTER_STATE_ERROR: c_int = -1;
pub const PW_FILTER_STATE_UNCONNECTED: c_int = 0;
pub const PW_FILTER_STATE_CONNECTING: c_int = 1;
pub const PW_FILTER_STATE_PAUSED: c_int = 2;
pub const PW_FILTER_STATE_STREAMING: c_int = 3;

pub const TYPE_NODE: &str = "PipeWire:Interface:Node";
pub const TYPE_PORT: &str = "PipeWire:Interface:Port";
pub const TYPE_METADATA: &[u8] = b"PipeWire:Interface:Metadata\0";
pub const TYPE_LINK: &[u8] = b"PipeWire:Interface:Link\0";
pub const TYPE_THREAD_UTILS: &[u8] = b"Spa:Pointer:Interface:ThreadUtils\0";
pub const PW_VERSION_METADATA: u32 = 3;
pub const PW_VERSION_LINK: u32 = 3;
pub const PW_VERSION_REGISTRY: u32 = 3;

#[repr(C)]
pub struct pw_filter_events {
    pub version: u32,
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    pub state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    pub io_changed: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, u32)>,
    pub param_changed: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *const spa_pod)>,
    pub add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void)>,
    pub remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void)>,
    pub process: Option<unsafe extern "C" fn(*mut c_void, *mut spa_io_position)>,
    pub drained: Option<unsafe extern "C" fn(*mut c_void)>,
    pub command: Option<unsafe extern "C" fn(*mut c_void, *const spa_command)>,
}
pub const PW_VERSION_FILTER_EVENTS: u32 = 1;

#[repr(C)]
pub struct pw_core_events {
    pub version: u32,
    pub info: Option<unsafe extern "C" fn(*mut c_void, *const pw_core_info)>,
    pub done: Option<unsafe extern "C" fn(*mut c_void, u32, c_int)>,
    pub ping: Option<unsafe extern "C" fn(*mut c_void, u32, c_int)>,
    pub error: Option<unsafe extern "C" fn(*mut c_void, u32, c_int, c_int, *const c_char)>,
    pub remove_id: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    pub bound_id: Option<unsafe extern "C" fn(*mut c_void, u32, u32)>,
    pub add_mem: Option<unsafe extern "C" fn(*mut c_void, u32, u32, c_int, u32)>,
    pub remove_mem: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    pub bound_props: Option<unsafe extern "C" fn(*mut c_void, u32, u32, *const spa_dict)>,
}
pub const PW_VERSION_CORE_EVENTS: u32 = 1;

#[repr(C)]
pub struct pw_core_methods {
    pub version: u32,
    pub add_listener: unsafe extern "C" fn(*mut c_void, *mut spa_hook, *const pw_core_events, *mut c_void) -> c_int,
    pub hello: unsafe extern "C" fn(*mut c_void, u32) -> c_int,
    pub sync: unsafe extern "C" fn(*mut c_void, u32, c_int) -> c_int,
    pub pong: unsafe extern "C" fn(*mut c_void, u32, c_int) -> c_int,
    pub error: unsafe extern "C" fn(*mut c_void, u32, c_int, c_int, *const c_char) -> c_int,
    pub get_registry: unsafe extern "C" fn(*mut c_void, u32, usize) -> *mut pw_registry,
    pub create_object:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, u32, *const spa_dict, usize) -> *mut c_void,
    pub destroy: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
}

#[repr(C)]
pub struct pw_registry_events {
    pub version: u32,
    pub global: Option<unsafe extern "C" fn(*mut c_void, u32, u32, *const c_char, u32, *const spa_dict)>,
    pub global_remove: Option<unsafe extern "C" fn(*mut c_void, u32)>,
}
pub const PW_VERSION_REGISTRY_EVENTS: u32 = 0;

#[repr(C)]
pub struct pw_registry_methods {
    pub version: u32,
    pub add_listener: unsafe extern "C" fn(*mut c_void, *mut spa_hook, *const pw_registry_events, *mut c_void) -> c_int,
    pub bind: unsafe extern "C" fn(*mut c_void, u32, *const c_char, u32, usize) -> *mut c_void,
    pub destroy: unsafe extern "C" fn(*mut c_void, u32) -> c_int,
}

#[repr(C)]
pub struct pw_metadata_events {
    pub version: u32,
    pub property: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_char, *const c_char, *const c_char) -> c_int>,
}
pub const PW_VERSION_METADATA_EVENTS: u32 = 0;

#[repr(C)]
pub struct pw_metadata_methods {
    pub version: u32,
    pub add_listener: unsafe extern "C" fn(*mut c_void, *mut spa_hook, *const pw_metadata_events, *mut c_void) -> c_int,
    pub set_property: unsafe extern "C" fn(*mut c_void, u32, *const c_char, *const c_char, *const c_char) -> c_int,
    pub clear: unsafe extern "C" fn(*mut c_void) -> c_int,
}

#[repr(C)]
pub struct spa_thread_utils_methods {
    pub version: u32,
    pub create: unsafe extern "C" fn(*mut c_void, *const spa_dict, extern "C" fn(*mut c_void) -> *mut c_void, *mut c_void) -> *mut spa_thread,
    pub join: unsafe extern "C" fn(*mut c_void, *mut spa_thread, *mut *mut c_void) -> c_int,
    pub get_rt_range: unsafe extern "C" fn(*mut c_void, *const spa_dict, *mut c_int, *mut c_int) -> c_int,
    pub acquire_rt: unsafe extern "C" fn(*mut c_void, *mut spa_thread, c_int) -> c_int,
    pub drop_rt: unsafe extern "C" fn(*mut c_void, *mut spa_thread) -> c_int,
}

#[link(name = "pipewire-0.3")]
extern "C" {
    pub fn pw_init(argc: *mut c_int, argv: *mut *mut *mut c_char);
    pub fn pw_get_library_version() -> *const c_char;

    pub fn pw_thread_loop_new(name: *const c_char, props: *const spa_dict) -> *mut pw_thread_loop;
    pub fn pw_thread_loop_destroy(l: *mut pw_thread_loop);
    pub fn pw_thread_loop_start(l: *mut pw_thread_loop) -> c_int;
    pub fn pw_thread_loop_stop(l: *mut pw_thread_loop);
    pub fn pw_thread_loop_lock(l: *mut pw_thread_loop);
    pub fn pw_thread_loop_unlock(l: *mut pw_thread_loop);
    pub fn pw_thread_loop_signal(l: *mut pw_thread_loop, wait_for_accept: bool);
    pub fn pw_thread_loop_get_loop(l: *mut pw_thread_loop) -> *mut pw_loop;
    pub fn pw_thread_loop_get_time(l: *mut pw_thread_loop, abstime: *mut libc::timespec, timeout: i64) -> c_int;
    pub fn pw_thread_loop_timed_wait_full(l: *mut pw_thread_loop, abstime: *const libc::timespec) -> c_int;
    pub fn pw_thread_loop_in_thread(l: *mut pw_thread_loop) -> bool;

    pub fn pw_context_new(main_loop: *mut pw_loop, props: *mut pw_properties, user_data_size: usize) -> *mut pw_context;
    pub fn pw_context_destroy(ctx: *mut pw_context);
    pub fn pw_context_connect(ctx: *mut pw_context, props: *mut pw_properties, user_data_size: usize) -> *mut pw_core;
    pub fn pw_context_get_object(ctx: *mut pw_context, type_: *const c_char) -> *mut c_void;
    pub fn pw_core_disconnect(core: *mut pw_core) -> c_int;

    pub fn pw_properties_new(key: *const c_char, ...) -> *mut pw_properties;
    pub fn pw_properties_set(props: *mut pw_properties, key: *const c_char, value: *const c_char) -> c_int;

    pub fn pw_proxy_destroy(proxy: *mut pw_proxy);

    pub fn pw_filter_new(core: *mut pw_core, name: *const c_char, props: *mut pw_properties) -> *mut pw_filter;
    pub fn pw_filter_destroy(f: *mut pw_filter);
    pub fn pw_filter_add_listener(f: *mut pw_filter, listener: *mut spa_hook, events: *const pw_filter_events, data: *mut c_void);
    pub fn pw_filter_connect(f: *mut pw_filter, flags: c_int, params: *mut *const spa_pod, n_params: u32) -> c_int;
    pub fn pw_filter_disconnect(f: *mut pw_filter) -> c_int;
    pub fn pw_filter_get_node_id(f: *mut pw_filter) -> u32;
    pub fn pw_filter_add_port(
        f: *mut pw_filter,
        direction: c_int,
        flags: c_int,
        port_data_size: usize,
        props: *mut pw_properties,
        params: *mut *const spa_pod,
        n_params: u32,
    ) -> *mut c_void;
    pub fn pw_filter_get_dsp_buffer(port_data: *mut c_void, n_samples: u32) -> *mut c_void;
    pub fn pw_filter_set_active(f: *mut pw_filter, active: bool) -> c_int;
    pub fn pw_filter_update_properties(f: *mut pw_filter, port_data: *mut c_void, dict: *const spa_dict) -> c_int;
    pub fn pw_filter_get_state(f: *mut pw_filter, error: *mut *const c_char) -> c_int;
}

/// Resolve the method table and object pointer behind a `spa_interface`.
unsafe fn methods<T>(iface: *mut c_void) -> (&'static T, *mut c_void) {
    let i = &*(iface as *const spa_interface);
    (&*(i.cb.funcs as *const T), i.cb.data)
}

pub unsafe fn core_add_listener(core: *mut pw_core, hook: *mut spa_hook, ev: *const pw_core_events, data: *mut c_void) {
    let (m, o) = methods::<pw_core_methods>(core.cast());
    (m.add_listener)(o, hook, ev, data);
}

pub unsafe fn core_sync(core: *mut pw_core, id: u32, seq: c_int) -> c_int {
    let (m, o) = methods::<pw_core_methods>(core.cast());
    (m.sync)(o, id, seq)
}

pub unsafe fn core_get_registry(core: *mut pw_core) -> *mut pw_registry {
    let (m, o) = methods::<pw_core_methods>(core.cast());
    (m.get_registry)(o, PW_VERSION_REGISTRY, 0)
}

pub unsafe fn core_create_object(
    core: *mut pw_core,
    factory: *const c_char,
    type_: *const c_char,
    version: u32,
    props: *const spa_dict,
) -> *mut c_void {
    let (m, o) = methods::<pw_core_methods>(core.cast());
    (m.create_object)(o, factory, type_, version, props, 0)
}

pub unsafe fn registry_add_listener(
    reg: *mut pw_registry,
    hook: *mut spa_hook,
    ev: *const pw_registry_events,
    data: *mut c_void,
) {
    let (m, o) = methods::<pw_registry_methods>(reg.cast());
    (m.add_listener)(o, hook, ev, data);
}

pub unsafe fn registry_bind(reg: *mut pw_registry, id: u32, type_: *const c_char, version: u32) -> *mut c_void {
    let (m, o) = methods::<pw_registry_methods>(reg.cast());
    (m.bind)(o, id, type_, version, 0)
}

pub unsafe fn metadata_add_listener(
    md: *mut c_void,
    hook: *mut spa_hook,
    ev: *const pw_metadata_events,
    data: *mut c_void,
) {
    let (m, o) = methods::<pw_metadata_methods>(md);
    (m.add_listener)(o, hook, ev, data);
}

pub unsafe fn thread_utils_get_rt_range(utils: *mut c_void, min: &mut c_int, max: &mut c_int) -> c_int {
    let (m, o) = methods::<spa_thread_utils_methods>(utils);
    (m.get_rt_range)(o, std::ptr::null(), min, max)
}

pub unsafe fn thread_utils_acquire_rt(utils: *mut c_void, thread: *mut spa_thread, prio: c_int) -> c_int {
    let (m, o) = methods::<spa_thread_utils_methods>(utils);
    (m.acquire_rt)(o, thread, prio)
}

/// Look up `key` in a C `spa_dict`.
pub unsafe fn dict_get<'a>(dict: *const spa_dict, key: &str) -> Option<&'a str> {
    if dict.is_null() {
        return None;
    }
    let d = &*dict;
    if d.items.is_null() {
        return None;
    }
    for i in 0..d.n_items as usize {
        let it = &*d.items.add(i);
        if it.key.is_null() || it.value.is_null() {
            continue;
        }
        if std::ffi::CStr::from_ptr(it.key).to_bytes() == key.as_bytes() {
            return std::ffi::CStr::from_ptr(it.value).to_str().ok();
        }
    }
    None
}

pub unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        std::ffi::CStr::from_ptr(p).to_str().ok()
    }
}

/// An owned list of key/value pairs that can be viewed as a `spa_dict`.
pub struct Dict {
    _strings: Vec<std::ffi::CString>,
    items: Vec<spa_dict_item>,
}

impl Dict {
    pub fn new(pairs: &[(&str, String)]) -> Dict {
        let mut strings = Vec::with_capacity(pairs.len() * 2);
        let mut items = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            let k = std::ffi::CString::new(*k).unwrap_or_default();
            let v = std::ffi::CString::new(v.as_str()).unwrap_or_default();
            items.push(spa_dict_item { key: k.as_ptr(), value: v.as_ptr() });
            strings.push(k);
            strings.push(v);
        }
        Dict { _strings: strings, items }
    }

    /// The returned dict borrows `self`.
    pub fn raw(&self) -> spa_dict {
        spa_dict { flags: 0, n_items: self.items.len() as u32, items: self.items.as_ptr() }
    }
}

/// Build a `pw_properties` (ownership passes to whichever API consumes it).
pub unsafe fn properties(pairs: &[(&str, String)]) -> *mut pw_properties {
    let p = pw_properties_new(std::ptr::null::<c_char>());
    for (k, v) in pairs {
        let k = std::ffi::CString::new(*k).unwrap_or_default();
        let v = std::ffi::CString::new(v.as_str()).unwrap_or_default();
        pw_properties_set(p, k.as_ptr(), v.as_ptr());
    }
    p
}
