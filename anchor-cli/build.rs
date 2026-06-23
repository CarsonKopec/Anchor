fn main() {
    // Delay-load WinFsp so non-mount commands (list/add/test/set-password/--version) run
    // without winfsp-x64.dll present; it's loaded lazily on the first mount. This must run in
    // the *binary* crate's build script — `cargo:rustc-link-arg` doesn't propagate from
    // dependencies (which is why anchor-fs's own build.rs isn't enough).
    #[cfg(feature = "winfsp")]
    winfsp::build::winfsp_link_delayload();
}
