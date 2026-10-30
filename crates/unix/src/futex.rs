//! Thin futex wrappers used for the RT <-> callback thread handshake.

use std::sync::atomic::AtomicU32;

const NSEC_PER_SEC: u64 = 1_000_000_000;

/// CLOCK_MONOTONIC in nanoseconds (the clock PipeWire timestamps use).
#[inline]
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out pointer; CLOCK_MONOTONIC cannot fail.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * NSEC_PER_SEC + ts.tv_nsec as u64
}

/// Block while `*word == expected`, until woken or the absolute
/// CLOCK_MONOTONIC `deadline_ns` passes. Returns `false` only on timeout;
/// spurious wakeups and value changes return `true` (callers re-check).
#[inline]
pub fn wait(word: &AtomicU32, expected: u32, deadline_ns: u64) -> bool {
    let ts = libc::timespec {
        tv_sec: (deadline_ns / NSEC_PER_SEC) as libc::time_t,
        tv_nsec: (deadline_ns % NSEC_PER_SEC) as libc::c_long,
    };
    // FUTEX_WAIT_BITSET takes an absolute timeout against CLOCK_MONOTONIC.
    // SAFETY: `word` is a live, aligned u32 for the duration of the call.
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG,
            expected,
            &ts as *const libc::timespec,
            std::ptr::null::<u32>(),
            libc::FUTEX_BITSET_MATCH_ANY,
        )
    };
    if r == 0 {
        return true;
    }
    // SAFETY: errno location is thread-local and always valid.
    let err = unsafe { *libc::__errno_location() };
    err != libc::ETIMEDOUT
}

/// Wake up to `n` waiters blocked on `word`.
#[inline]
pub fn wake(word: &AtomicU32, n: i32) {
    // SAFETY: as above.
    unsafe {
        libc::syscall(libc::SYS_futex, word.as_ptr(), libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG, n);
    }
}

pub fn wake_all(word: &AtomicU32) {
    wake(word, i32::MAX);
}
