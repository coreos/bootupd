fn main() {
    if std::env::var("CARGO_FEATURE_SYSTEMD_BOOT").is_ok() {
        if let Ok(arch) = std::env::var("CARGO_CFG_TARGET_ARCH") {
            if arch.starts_with("riscv") {
                panic!("The systemd-boot feature is not supported on RISC-V.");
            }
        }
    }
}
