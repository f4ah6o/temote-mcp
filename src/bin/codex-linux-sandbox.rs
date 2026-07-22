fn main() -> ! {
    #[cfg(target_os = "linux")]
    codex_linux_sandbox::run_main();

    #[cfg(not(target_os = "linux"))]
    panic!("codex-linux-sandbox is only supported on Linux");
}
