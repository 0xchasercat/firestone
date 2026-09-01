//! The `firestone-init` executable: one call into the crate's PID-1 runtime.
//!
//! Everything of substance lives in the library half of this crate so the pure
//! logic is unit-testable on any host (SPEC §10.5).

#[cfg(target_os = "linux")]
fn main() -> ! {
    firestone_init::runtime::run()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    // The guest payload is built for `x86_64-unknown-linux-musl`. Other hosts
    // still compile and test the pure modules; the binary itself has nothing to
    // do there and says so rather than pretending to boot something.
    let mut console = firestone_init::console::Console::open();
    console.line("firestone-init runs as PID 1 inside a Firestone OCI machine on Linux only");
    std::process::ExitCode::FAILURE
}
