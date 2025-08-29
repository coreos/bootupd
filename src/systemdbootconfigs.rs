use std::path::Path;

use anyhow::{Context, Result};
use fn_error_context::context;

const CONFIG_DIR: &str = "usr/lib/bootupd/systemd-boot";

/// Install files required for systemd-boot
/// This mostly proxies the bootctl install command
#[context("Installing systemd-boot")]
pub(crate) fn install(src_root: &openat::Dir, esp_path: &openat::Dir) -> Result<()> {
    let esp_path_buf = esp_path.recover_path().context("ESP path is not valid")?;
    let esp_path_str = esp_path_buf
        .to_str()
        .context("ESP path is not valid UTF-8")?;
    let status = std::process::Command::new("bootctl")
        .args(["install", "--esp-path", esp_path_str])
        .status()
        .context("Failed to execute bootctl")?;

    if !status.success() {
        anyhow::bail!(
            "bootctl install failed with status code {}",
            status.code().unwrap_or(-1)
        );
    }

    // If loader.conf is present in the bootupd configuration, replace the original config with it
    let configdir_path = Path::new(CONFIG_DIR);
    if let Err(e) = try_copy_loader_conf(src_root, configdir_path, esp_path_str) {
        log::debug!("Optional loader.conf copy skipped: {}", e);
    }

    Ok(())
}

/// Try to copy loader.conf from configdir to ESP, returns error if not present or copy fails
fn try_copy_loader_conf(
    src_root: &openat::Dir,
    configdir_path: &Path,
    esp_path_str: &str,
) -> Result<()> {
    let configdir = src_root
        .sub_dir(configdir_path)
        .context(format!("Config directory '{}' not found", CONFIG_DIR))?;
    let dst_loader_conf = Path::new(esp_path_str).join("loader/loader.conf");
    match configdir.open_file("loader.conf") {
        Ok(mut src_file) => {
            let mut dst_file = std::fs::File::create(&dst_loader_conf)
                .context("Failed to create loader.conf in ESP")?;
            std::io::copy(&mut src_file, &mut dst_file)
                .context("Failed to copy loader.conf to ESP")?;
            log::info!("loader.conf copied to {}", dst_loader_conf.display());
            Ok(())
        }
        Err(e) => {
            log::debug!("loader.conf not found in configdir, skipping: {}", e);
            Err(anyhow::anyhow!(e))
        }
    }
}
