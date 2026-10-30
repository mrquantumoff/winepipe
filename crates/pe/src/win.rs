//! The handful of Win32 declarations WinePipe needs.

#![allow(non_snake_case, non_camel_case_types, dead_code, clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub type BOOL = i32;
pub type DWORD = u32;
pub type HANDLE = *mut c_void;
pub type HMODULE = *mut c_void;
pub type HKEY = *mut c_void;
pub type HRESULT = i32;
pub type LSTATUS = i32;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl GUID {
    pub fn to_string_braced(&self) -> String {
        let d = &self.data4;
        format!(
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

pub const IID_IUNKNOWN: GUID = GUID { data1: 0, data2: 0, data3: 0, data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
pub const IID_ICLASSFACTORY: GUID = GUID { data1: 1, data2: 0, data3: 0, data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

pub const S_OK: HRESULT = 0;
pub const S_FALSE: HRESULT = 1;
pub const E_NOINTERFACE: HRESULT = 0x8000_4002u32 as i32;
pub const E_POINTER: HRESULT = 0x8000_4003u32 as i32;
pub const E_FAIL: HRESULT = 0x8000_4005u32 as i32;
pub const E_OUTOFMEMORY: HRESULT = 0x8007_000Eu32 as i32;
pub const CLASS_E_NOAGGREGATION: HRESULT = 0x8004_0110u32 as i32;
pub const CLASS_E_CLASSNOTAVAILABLE: HRESULT = 0x8004_0111u32 as i32;
pub const SELFREG_E_CLASS: HRESULT = 0x8004_0201u32 as i32;

pub const DLL_PROCESS_ATTACH: DWORD = 1;

pub const HKEY_CLASSES_ROOT: HKEY = 0x8000_0000u32 as i32 as isize as HKEY;
pub const HKEY_CURRENT_USER: HKEY = 0x8000_0001u32 as i32 as isize as HKEY;
pub const HKEY_LOCAL_MACHINE: HKEY = 0x8000_0002u32 as i32 as isize as HKEY;
pub const KEY_READ: DWORD = 0x20019;
pub const KEY_ALL_ACCESS: DWORD = 0xF003F;
pub const REG_SZ: DWORD = 1;
pub const REG_DWORD: DWORD = 4;
pub const ERROR_SUCCESS: LSTATUS = 0;

pub const MEM_COMMIT: DWORD = 0x1000;
pub const MEM_RESERVE: DWORD = 0x2000;
pub const MEM_RELEASE: DWORD = 0x8000;
pub const PAGE_READWRITE: DWORD = 0x04;

pub const INFINITE: DWORD = 0xFFFF_FFFF;
pub const WAIT_OBJECT_0: DWORD = 0;
pub const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;

pub const MB_OK: u32 = 0;
pub const MB_ICONINFORMATION: u32 = 0x40;

pub type ThreadProc = unsafe extern "system" fn(*mut c_void) -> DWORD;

#[link(name = "kernel32")]
extern "system" {
    pub fn GetModuleHandleW(name: *const u16) -> HMODULE;
    pub fn GetProcAddress(module: HMODULE, name: *const u8) -> *mut c_void;
    pub fn GetModuleFileNameW(module: HMODULE, buf: *mut u16, size: DWORD) -> DWORD;
    pub fn DisableThreadLibraryCalls(module: HMODULE) -> BOOL;
    pub fn GetCurrentProcess() -> HANDLE;
    pub fn GetCurrentThread() -> HANDLE;
    pub fn GetCurrentThreadId() -> DWORD;
    pub fn CreateThread(
        attrs: *mut c_void,
        stack: usize,
        start: ThreadProc,
        param: *mut c_void,
        flags: DWORD,
        tid: *mut DWORD,
    ) -> HANDLE;
    pub fn SetThreadPriority(thread: HANDLE, prio: i32) -> BOOL;
    pub fn WaitForSingleObject(h: HANDLE, ms: DWORD) -> DWORD;
    pub fn CloseHandle(h: HANDLE) -> BOOL;
    pub fn VirtualAlloc(addr: *mut c_void, size: usize, ty: DWORD, protect: DWORD) -> *mut c_void;
    pub fn VirtualFree(addr: *mut c_void, size: usize, ty: DWORD) -> BOOL;
    pub fn QueryPerformanceCounter(v: *mut i64) -> BOOL;
    pub fn QueryPerformanceFrequency(v: *mut i64) -> BOOL;
}

#[link(name = "advapi32")]
extern "system" {
    pub fn RegCreateKeyExW(
        key: HKEY,
        sub: *const u16,
        reserved: DWORD,
        class: *const u16,
        options: DWORD,
        sam: DWORD,
        sa: *mut c_void,
        out: *mut HKEY,
        disp: *mut DWORD,
    ) -> LSTATUS;
    pub fn RegOpenKeyExW(key: HKEY, sub: *const u16, options: DWORD, sam: DWORD, out: *mut HKEY) -> LSTATUS;
    pub fn RegSetValueExW(key: HKEY, name: *const u16, reserved: DWORD, ty: DWORD, data: *const u8, len: DWORD) -> LSTATUS;
    pub fn RegQueryValueExW(
        key: HKEY,
        name: *const u16,
        reserved: *mut DWORD,
        ty: *mut DWORD,
        data: *mut u8,
        len: *mut DWORD,
    ) -> LSTATUS;
    pub fn RegDeleteTreeW(key: HKEY, sub: *const u16) -> LSTATUS;
    pub fn RegCloseKey(key: HKEY) -> LSTATUS;
}

#[link(name = "user32")]
extern "system" {
    pub fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, ty: u32) -> i32;
}

/// NUL-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Nanoseconds from QueryPerformanceCounter.
pub fn qpc_ns() -> u64 {
    static FREQ: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let f = *FREQ.get_or_init(|| {
        let mut f = 0;
        // SAFETY: valid out pointer.
        unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    });
    let mut c = 0;
    // SAFETY: valid out pointer.
    unsafe { QueryPerformanceCounter(&mut c) };
    (c as u128 * 1_000_000_000 / f as u128) as u64
}

/// An open registry key, closed on drop.
pub struct Key(pub HKEY);

impl Key {
    pub fn open(root: HKEY, path: &str) -> Option<Key> {
        let mut k = std::ptr::null_mut();
        let p = wide(path);
        // SAFETY: valid strings and out pointer.
        (unsafe { RegOpenKeyExW(root, p.as_ptr(), 0, KEY_READ, &mut k) } == ERROR_SUCCESS).then_some(Key(k))
    }

    pub fn create(root: HKEY, path: &str) -> Option<Key> {
        let mut k = std::ptr::null_mut();
        let p = wide(path);
        // SAFETY: valid strings and out pointer.
        let r = unsafe {
            RegCreateKeyExW(root, p.as_ptr(), 0, std::ptr::null(), 0, KEY_ALL_ACCESS, std::ptr::null_mut(), &mut k, std::ptr::null_mut())
        };
        (r == ERROR_SUCCESS).then_some(Key(k))
    }

    pub fn set_str(&self, name: Option<&str>, value: &str) -> bool {
        let n = name.map(wide);
        let v = wide(value);
        // SAFETY: valid buffers; length in bytes including the terminator.
        unsafe {
            RegSetValueExW(
                self.0,
                n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                0,
                REG_SZ,
                v.as_ptr() as *const u8,
                (v.len() * 2) as DWORD,
            ) == ERROR_SUCCESS
        }
    }

    pub fn get_dword(&self, name: &str) -> Option<u32> {
        let n = wide(name);
        let mut ty = 0;
        let mut v = 0u32;
        let mut len = 4;
        // SAFETY: valid buffers.
        let r = unsafe { RegQueryValueExW(self.0, n.as_ptr(), std::ptr::null_mut(), &mut ty, &mut v as *mut u32 as *mut u8, &mut len) };
        (r == ERROR_SUCCESS && ty == REG_DWORD).then_some(v)
    }

    pub fn get_str(&self, name: &str) -> Option<String> {
        let n = wide(name);
        let mut ty = 0;
        let mut buf = [0u16; 256];
        let mut len = (buf.len() * 2) as DWORD;
        // SAFETY: valid buffers.
        let r = unsafe {
            RegQueryValueExW(self.0, n.as_ptr(), std::ptr::null_mut(), &mut ty, buf.as_mut_ptr() as *mut u8, &mut len)
        };
        if r != ERROR_SUCCESS || ty != REG_SZ {
            return None;
        }
        let chars = (len as usize / 2).min(buf.len());
        let s = &buf[..chars];
        let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
        Some(String::from_utf16_lossy(&s[..end]))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: we own the key.
        unsafe { RegCloseKey(self.0) };
    }
}

pub fn delete_tree(root: HKEY, path: &str) {
    let p = wide(path);
    // SAFETY: valid string.
    unsafe { RegDeleteTreeW(root, p.as_ptr()) };
}
