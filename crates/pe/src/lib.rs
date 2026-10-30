//! WinePipe PE side (`winepipe.dll`): an in-process COM server implementing
//! the ASIO driver interface on top of `winepipe.so`.

#![allow(clippy::missing_safety_doc, non_upper_case_globals)]

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering::*};

pub mod asio;
mod config;
mod driver;
mod unix;
mod win;

use driver::{Driver, DRIVER_NAME};
use win::*;

/// {BE5FBE6F-8E42-4F6E-89E6-2225021E5929}
pub const CLSID_WINEPIPE: GUID =
    GUID { data1: 0xBE5F_BE6F, data2: 0x8E42, data3: 0x4F6E, data4: [0x89, 0xE6, 0x22, 0x25, 0x02, 0x1E, 0x59, 0x29] };

/// Live driver objects plus server locks, for DllCanUnloadNow.
pub(crate) static OBJECTS: AtomicU32 = AtomicU32::new(0);
static MODULE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

pub(crate) fn module_handle() -> HMODULE {
    MODULE.load(Relaxed)
}

pub(crate) fn debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WINEPIPE_DEBUG").is_some_and(|v| !v.is_empty() && v != "0"))
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => {
        if $crate::debug_enabled() {
            eprintln!("winepipe.dll: {}", format_args!($($t)*));
        }
    };
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(module: HMODULE, reason: DWORD, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module, Relaxed);
        DisableThreadLibraryCalls(module);
    }
    1
}

// ---- class factory -------------------------------------------------------------

#[repr(C)]
struct Factory {
    vtbl: *const FactoryVtbl,
}

#[repr(C)]
struct FactoryVtbl {
    query_interface: unsafe extern "system" fn(*mut Factory, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut Factory) -> u32,
    release: unsafe extern "system" fn(*mut Factory) -> u32,
    create_instance: unsafe extern "system" fn(*mut Factory, *mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    lock_server: unsafe extern "system" fn(*mut Factory, BOOL) -> HRESULT,
}

struct SyncFactory(Factory);
// SAFETY: stateless, immutable object.
unsafe impl Sync for SyncFactory {}

static FACTORY_VTBL: FactoryVtbl = FactoryVtbl {
    query_interface: cf_query_interface,
    add_ref: cf_add_ref,
    release: cf_release,
    create_instance: cf_create_instance,
    lock_server: cf_lock_server,
};
static FACTORY: SyncFactory = SyncFactory(Factory { vtbl: &FACTORY_VTBL });

unsafe extern "system" fn cf_query_interface(this: *mut Factory, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    if *riid == IID_IUNKNOWN || *riid == IID_ICLASSFACTORY {
        *ppv = this.cast();
        S_OK
    } else {
        *ppv = null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn cf_add_ref(_: *mut Factory) -> u32 {
    2
}

unsafe extern "system" fn cf_release(_: *mut Factory) -> u32 {
    1
}

unsafe extern "system" fn cf_create_instance(
    _: *mut Factory,
    outer: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = null_mut();
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    let d = Driver::create();
    // QI adds a reference; drop the creation reference afterwards.
    let hr = Driver::query_interface(d, riid, ppv);
    Driver::release(d);
    hr
}

unsafe extern "system" fn cf_lock_server(_: *mut Factory, lock: BOOL) -> HRESULT {
    if lock != 0 {
        OBJECTS.fetch_add(1, SeqCst);
    } else {
        OBJECTS.fetch_sub(1, SeqCst);
    }
    S_OK
}

// ---- exports ------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if clsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    *ppv = null_mut();
    if *clsid != CLSID_WINEPIPE {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    cf_query_interface(&FACTORY.0 as *const Factory as *mut Factory, riid, ppv)
}

#[no_mangle]
pub unsafe extern "system" fn DllCanUnloadNow() -> HRESULT {
    if OBJECTS.load(SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

fn module_path() -> String {
    let mut buf = [0u16; 1024];
    // SAFETY: valid buffer.
    let n = unsafe { GetModuleFileNameW(module_handle(), buf.as_mut_ptr(), buf.len() as DWORD) } as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

fn clsid_key() -> String {
    format!("CLSID\\{}", CLSID_WINEPIPE.to_string_braced())
}

fn asio_key() -> String {
    format!("Software\\ASIO\\{DRIVER_NAME}")
}

#[no_mangle]
pub unsafe extern "system" fn DllRegisterServer() -> HRESULT {
    let clsid = CLSID_WINEPIPE.to_string_braced();
    let path = module_path();
    log!("registering {clsid} at {path}");
    let ok = (|| {
        let k = Key::create(HKEY_CLASSES_ROOT, &clsid_key())?;
        k.set_str(None, "WinePipe ASIO Driver").then_some(())?;
        let inproc = Key::create(HKEY_CLASSES_ROOT, &format!("{}\\InprocServer32", clsid_key()))?;
        inproc.set_str(None, &path).then_some(())?;
        inproc.set_str(Some("ThreadingModel"), "Apartment").then_some(())?;
        let a = Key::create(HKEY_LOCAL_MACHINE, &asio_key())?;
        a.set_str(Some("CLSID"), &clsid).then_some(())?;
        a.set_str(Some("Description"), DRIVER_NAME).then_some(())?;
        Some(())
    })();
    if ok.is_some() {
        S_OK
    } else {
        SELFREG_E_CLASS
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllUnregisterServer() -> HRESULT {
    delete_tree(HKEY_LOCAL_MACHINE, &asio_key());
    delete_tree(HKEY_CLASSES_ROOT, &clsid_key());
    S_OK
}
