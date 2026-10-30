fn main() {
    // i686: Rust emits stdcall exports as `Name@N`; let ld export them
    // undecorated (what COM/regsvr32 look up) without warning about it.
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,--enable-stdcall-fixup");
    }
}
