fn main() {
    if cfg!(target_os = "windows") {
        // The GPUI history view exceeds Windows' default 1 MiB main-thread stack.
        println!("cargo:rustc-link-arg-bin=deving-panel=/STACK:8388608");
        println!("cargo:rerun-if-changed=.github/packaging/deving-panel.rc");
        println!("cargo:rerun-if-changed=.github/packaging/deving-panel.ico");
        embed_resource::compile_for(
            ".github/packaging/deving-panel.rc",
            ["deving-panel"],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("Failed to embed the Windows application icon");
    }
}
