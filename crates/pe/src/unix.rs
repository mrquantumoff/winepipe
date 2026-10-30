//! Calling into `winepipe.so` through Wine's unix-call dispatcher.
//!
//! This does what winecrt0's `__wine_init_unix_call` does, but at runtime:
//! the dispatcher pointer is exported by ntdll as a data symbol, and
//! `NtQueryVirtualMemory(MemoryWineLoadUnixLib)` loads the Unix library that
//! belongs to this module and returns its handle. No Wine import libraries
//! are needed at build time, and the same code works under WoW64.

use std::ffi::c_void;
use std::sync::OnceLock;

use crate::win::*;

const MEMORY_WINE_LOAD_UNIX_LIB: u32 = 1000;

type Dispatcher = unsafe extern "system" fn(handle: u64, code: u32, args: *mut c_void) -> u32;
type NtQueryVirtualMemory =
    unsafe extern "system" fn(HANDLE, *const c_void, u32, *mut c_void, usize, *mut usize) -> i32;

struct UnixLib {
    handle: u64,
    dispatch: Dispatcher,
}

static LIB: OnceLock<Result<UnixLib, String>> = OnceLock::new();

/// Load the Unix half (once). Returns a description of the failure if Wine
/// could not find or load it.
pub fn load() -> Result<(), String> {
    LIB.get_or_init(|| unsafe { try_load() }).as_ref().map(|_| ()).map_err(Clone::clone)
}

unsafe fn try_load() -> Result<UnixLib, String> {
    let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
    if ntdll.is_null() {
        return Err("ntdll.dll not found".into());
    }
    let disp = GetProcAddress(ntdll, b"__wine_unix_call_dispatcher\0".as_ptr()) as *const Option<Dispatcher>;
    let Some(dispatch) = disp.as_ref().copied().flatten() else {
        return Err("not running under Wine (no __wine_unix_call_dispatcher)".into());
    };
    let q = GetProcAddress(ntdll, b"NtQueryVirtualMemory\0".as_ptr());
    if q.is_null() {
        return Err("NtQueryVirtualMemory not found".into());
    }
    let query: NtQueryVirtualMemory = std::mem::transmute(q);
    let mut handle = 0u64;
    let status = query(
        GetCurrentProcess(),
        crate::module_handle() as *const c_void,
        MEMORY_WINE_LOAD_UNIX_LIB,
        &mut handle as *mut u64 as *mut c_void,
        std::mem::size_of::<u64>(),
        std::ptr::null_mut(),
    );
    if status != 0 || handle == 0 {
        return Err(format!(
            "could not load winepipe.so (status {:#010x}); is it installed next to winepipe.dll \
             in the matching *-unix directory, and is libpipewire-0.3 available?",
            status as u32
        ));
    }
    Ok(UnixLib { handle, dispatch })
}

/// Invoke unix function `code` with parameter struct `p`. Returns the NTSTATUS.
pub fn call<T>(code: u32, p: &mut T) -> u32 {
    match LIB.get() {
        // SAFETY: `p` is one of the ABI structs matching `code`.
        Some(Ok(lib)) => unsafe { (lib.dispatch)(lib.handle, code, p as *mut T as *mut c_void) },
        _ => 0xC000_0135, // STATUS_DLL_NOT_FOUND
    }
}
