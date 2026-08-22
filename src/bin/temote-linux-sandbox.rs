fn main() -> ! {
    #[cfg(target_os = "linux")]
    temote_mcp::sandbox::linux::run_main();

    #[cfg(not(target_os = "linux"))]
    panic!("temote-linux-sandbox is only supported on Linux");
}
