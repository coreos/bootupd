use std::path::Path;

use anyhow::{Context, Result};
use fn_error_context::context;

/// Install files required for systemd-boot
#[context("Installing systemd-boot")]
pub(crate) fn install(esp_path: &openat::Dir) -> Result<()> {
    let esp_path = esp_path.recover_path().context("ESP path is not valid")?;
    let status = std::process::Command::new("bootctl")
        .args([
            "install",
            "--esp-path",
            esp_path.to_str().context("ESP path is not valid UTF-8")?,
        ])
        .status()
        .context("Failed to execute bootctl")?;

    if !status.success() {
        anyhow::bail!(
            "bootctl install failed with status code {}",
            status.code().unwrap_or(-1)
        );
    }

    // If loader.conf is present in the bootupd configuration, replace the original config with it
    let src_loader_conf = "/usr/lib/bootupd/systemd-boot/loader.conf";
    let dst_loader_conf = Path::new(&esp_path).join("loader/loader.conf");
    if Path::new(src_loader_conf).exists() {
        std::fs::copy(src_loader_conf, &dst_loader_conf)
            .context("Failed to copy loader.conf to ESP")?;
        log::info!(
            "Copied {} to {}",
            src_loader_conf,
            dst_loader_conf.display()
        );
    } else {
        log::warn!("{} does not exist, skipping copy", src_loader_conf);
    }

    Ok(())
}
