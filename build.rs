fn main() {
    if cfg!(target_os = "windows") {
        // The GPUI history view exceeds Windows' default 1 MiB main-thread stack.
        println!("cargo:rustc-link-arg-bin=deving-panel=/STACK:8388608");
    }
}
