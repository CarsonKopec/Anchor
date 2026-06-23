fn main() {
    // Delay-load WinFsp so the tray binary links winfsp-x64.dll lazily (loaded on first
    // mount). Must run in the binary crate's build script — `cargo:rustc-link-arg` does not
    // propagate from dependency build scripts.
    #[cfg(feature = "winfsp")]
    winfsp::build::winfsp_link_delayload();
}
