fn main() {
    // WinFsp is delay-loaded: the import library is linked lazily so the binary can be
    // built and even started on a machine without WinFsp, failing only when a mount is
    // actually attempted. This link directive is only required when the `winfsp` feature
    // is enabled; the default build pulls in none of the WinFsp toolchain.
    #[cfg(feature = "winfsp")]
    winfsp::build::winfsp_link_delayload();
}
