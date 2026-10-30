//! WinePipe Unix side (`winepipe.so`): loaded by Wine's ntdll on behalf of
//! `winepipe.dll` and driven through `__wine_unix_call`.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};

use winepipe_abi as abi;

mod engine;
mod futex;
mod pw;
mod rt;

use engine::{Config, Engine};

pub(crate) fn debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WINEPIPE_DEBUG").is_some_and(|v| !v.is_empty() && v != "0"))
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => {
        if $crate::debug_enabled() {
            eprintln!("winepipe: {}", format_args!($($t)*));
        }
    };
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { eprintln!("winepipe: warning: {}", format_args!($($t)*)) };
}

type NtStatus = u32;
const STATUS_SUCCESS: NtStatus = 0;
const STATUS_INVALID_PARAMETER: NtStatus = 0xC000_000D;
const STATUS_INTERNAL_ERROR: NtStatus = 0xC000_00E5;

type Entry = unsafe extern "C" fn(*mut c_void) -> NtStatus;

/// Run `f` on the parameter struct, turning panics into an NTSTATUS instead
/// of unwinding into Wine.
unsafe fn with<T>(args: *mut c_void, f: impl FnOnce(&mut T)) -> NtStatus {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let p = &mut *(args as *mut T);
    match catch_unwind(AssertUnwindSafe(|| f(p))) {
        Ok(()) => STATUS_SUCCESS,
        Err(_) => STATUS_INTERNAL_ERROR,
    }
}

/// # Safety
/// `h` must be 0 or a handle returned by `init` that has not been exited.
unsafe fn engine<'a>(h: u64) -> Option<&'a Engine> {
    (h as usize as *const Engine).as_ref()
}

unsafe extern "C" fn init(args: *mut c_void) -> NtStatus {
    with(args, |p: &mut abi::InitParams| {
        let name = abi::get_str(&p.client_name);
        let cfg = Config {
            inputs: p.num_inputs.min(abi::MAX_CHANNELS),
            outputs: p.num_outputs.min(abi::MAX_CHANNELS),
            autoconnect: p.flags & abi::INIT_FLAG_AUTOCONNECT != 0,
            client_name: if name.is_empty() { "WinePipe".into() } else { name.into() },
            app_name: abi::get_str(&p.app_name).into(),
        };
        log!("init: {} in / {} out, client '{}', app '{}'", cfg.inputs, cfg.outputs, cfg.client_name, cfg.app_name);
        match Engine::new(cfg) {
            Ok(e) => {
                p.handle = Box::into_raw(e) as usize as u64;
                p.status = abi::status::OK;
            }
            Err(msg) => {
                warn!("init failed: {msg}");
                abi::put_str(&mut p.error, &msg);
                p.handle = 0;
                p.status = abi::status::NOT_CONNECTED;
            }
        }
    })
}

unsafe extern "C" fn exit(args: *mut c_void) -> NtStatus {
    with(args, |p: &mut abi::HandleParams| {
        if p.handle != 0 {
            drop(Box::from_raw(p.handle as usize as *mut Engine));
            p.handle = 0;
        }
        p.status = abi::status::OK;
    })
}

macro_rules! entry {
    ($name:ident, $ty:ty, |$e:ident, $p:ident| $body:expr) => {
        unsafe extern "C" fn $name(args: *mut c_void) -> NtStatus {
            with(args, |$p: &mut $ty| match engine($p.handle) {
                Some($e) => $body,
                None => $p.status = abi::status::INVALID_HANDLE,
            })
        }
    };
}

entry!(get_info, abi::InfoParams, |e, p| e.get_info(p));
entry!(set_sample_rate, abi::SetRateParams, |e, p| p.status = e.set_sample_rate(p.rate));
entry!(create_buffers, abi::CreateBuffersParams, |e, p| p.status = e.create_buffers(p));
entry!(dispose_buffers, abi::HandleParams, |e, p| p.status = e.dispose_buffers());
entry!(start, abi::StartParams, |e, p| p.status = e.start(p));
entry!(stop, abi::HandleParams, |e, p| p.status = e.stop());
entry!(output_ready, abi::OutputReadyParams, |e, p| {
    e.output_ready(p.seq);
    p.status = abi::status::OK;
});
entry!(get_stats, abi::StatsParams, |e, p| e.stats(p));

/// Launch the native settings UI for the calling application's prefix.
unsafe extern "C" fn control_panel(args: *mut c_void) -> NtStatus {
    with(args, |p: &mut abi::HandleParams| {
        p.status = match launch_settings() {
            Ok(()) => abi::status::OK,
            Err(e) => {
                log!("control panel: {e}");
                abi::status::UNSUPPORTED
            }
        }
    })
}

/// The `bin/wine` of the Wine installation running this process, derived
/// from the loader executable (`<root>/bin/wine` or
/// `<root>/lib*/wine/<arch>-unix/wine`).
fn wine_binary() -> Option<std::path::PathBuf> {
    let exe = std::fs::read_link("/proc/self/exe").ok()?;
    if !exe.file_name()?.to_str()?.starts_with("wine") {
        return None;
    }
    let dir = exe.parent()?;
    let root = if dir.file_name()? == "bin" { dir.parent()? } else { dir.parent()?.parent()?.parent()? };
    let wine = root.join("bin/wine");
    wine.is_file().then_some(wine)
}

/// Locate an executable file, resolved here in the parent: a failed exec in
/// a child forked from a Wine process cannot be reported or cleaned up
/// reliably.
fn find_settings_binary() -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    let runnable = |p: &Path| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if let Some(explicit) = std::env::var_os("WINEPIPE_SETTINGS") {
        let p = PathBuf::from(explicit);
        return runnable(&p).then_some(p);
    }
    let name = "winepipe-settings";
    let from_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(name)).collect::<Vec<_>>())
        .unwrap_or_default();
    // Apps started from a desktop entry often lack ~/.local/bin in PATH.
    let extra = std::env::var_os("HOME")
        .map(|h| Path::new(&h).join(".local/bin").join(name))
        .into_iter()
        .chain(["/usr/local/bin", "/usr/bin"].map(|d| Path::new(d).join(name)));
    from_path.into_iter().chain(extra).find(|p| runnable(p))
}

fn launch_settings() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = find_settings_binary()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "winepipe-settings not found"))?;
    let prefix = std::env::var_os("WINEPREFIX")
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".wine").into_os_string()));
    let mut cmd = Command::new(&exe);
    if let Some(p) = prefix {
        cmd.arg("--prefix").arg(p);
    }
    if let Some(w) = std::env::var_os("WINELOADER").map(std::path::PathBuf::from).or_else(wine_binary) {
        cmd.arg("--wine").arg(w);
    }
    // No pre_exec hook: that would force fork() and run Rust code in a child
    // of the (heavily multithreaded, signal-handling) Wine process. With
    // only a process group set, std uses posix_spawn, which execs directly
    // with a clean signal mask and default handlers.
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    let mut child = cmd.spawn()?;
    log!("control panel: started {} (pid {})", exe.display(), child.id());
    // reap it so it never lingers as a zombie of the Wine process
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

unsafe extern "C" fn wait(args: *mut c_void) -> NtStatus {
    with(args, |p: &mut abi::WaitParams| match engine(p.handle) {
        Some(e) => e.wait(p),
        None => p.result = abi::WAIT_STOPPED,
    })
}

const FUNCS: [Entry; abi::call::COUNT] = [
    init,
    exit,
    get_info,
    set_sample_rate,
    create_buffers,
    dispose_buffers,
    start,
    stop,
    wait,
    output_ready,
    get_stats,
    control_panel,
];

/// Entry points for 64-bit callers.
#[no_mangle]
pub static __wine_unix_call_funcs: [Entry; abi::call::COUNT] = FUNCS;

/// Entry points for 32-bit (WoW64) callers. The ABI structs have identical
/// layouts on i686 and x86_64 and carry pointers as zero-extended u64s, so no
/// thunking is needed.
#[no_mangle]
pub static __wine_unix_call_wow64_funcs: [Entry; abi::call::COUNT] = FUNCS;

const _: () = {
    assert!(abi::call::INIT == 0 && abi::call::EXIT == 1 && abi::call::GET_INFO == 2);
    assert!(abi::call::SET_SAMPLE_RATE == 3 && abi::call::CREATE_BUFFERS == 4);
    assert!(abi::call::DISPOSE_BUFFERS == 5 && abi::call::START == 6 && abi::call::STOP == 7);
    assert!(abi::call::WAIT == 8 && abi::call::OUTPUT_READY == 9 && abi::call::GET_STATS == 10);
    assert!(abi::call::CONTROL_PANEL == 11);
};
