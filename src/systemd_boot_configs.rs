use std::path::Path;

use anyhow::{bail, Context, Result};
use fn_error_context::context;
use log::warn;

/// Install the systemd-boot entry files
#[context("Installing systemd-boot entries")]
pub(crate) fn install(esp_path: &openat::Dir, _write_uuid: bool) -> Result<()> {
    let esp_path = esp_path.recover_path().context("ESP path is not valid")?;
    let status = std::process::Command::new("bootctl")
        .args([
            "install",
            "--esp-path",
            esp_path.to_str().context("ESP path is not valid UTF-8")?,
        ])
        .status()
        .context("running  install")?;
    warn!("bootctl install status: {}", status);
    if !status.success() {
        bail!("bootctl install failed with status: {}", status);
    }

    // If loader.conf is present in /usr/lib/bootupd/systemd-boot/loader.conf, copy it over
    let src_loader_conf = "/usr/lib/bootupd/systemd-boot/loader.conf";
    let dst_loader_conf = Path::new(&esp_path).join("loader/loader.conf");
    if Path::new(src_loader_conf).exists() {
        std::fs::copy(src_loader_conf, &dst_loader_conf).context(format!(
            "Copying {} to {}",
            src_loader_conf,
            dst_loader_conf.display()
        ))?;
        warn!(
            "Copied \"{}\" to \"{}\".",
            src_loader_conf,
            dst_loader_conf.display()
        );
    }

    Ok(())
}
