use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uapi_version::Version;

use crate::model::*;
use crate::util::get_metadata_timestamp;
use log::debug;

pub(crate) const QUERY_FILE_OWNER_SCRIPT: &str = "usr/lib/bootupd/packagesystem/query-file-owner";

#[derive(Serialize, Deserialize, Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Module {
    pub(crate) name: String,

    #[serde(alias = "rpm_evr")]
    pub(crate) evr: String,
}

impl Module {
    pub(crate) fn evr(&self) -> Version {
        Version::from(&self.evr)
    }
}

impl Ord for Module {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name) // Compare names first
            .then_with(|| self.evr().cmp(&other.evr())) // If names equal, compare versions
    }
}

impl PartialOrd for Module {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Parse the output of the `query-file-owner` script.
///
/// Each line contains one package with two space-separated values: NAME and VERSION.
/// The format depends on the package manager (rpm, dpkg, pacman, etc.):
fn parse_package_metadata(stdout: &[u8]) -> Result<ContentMetadata> {
    let output =
        std::str::from_utf8(stdout).context("Failed to decode package metadata output as UTF-8")?;

    debug!("Package metadata output: {:?}", output);

    let mut packages = BTreeSet::new();

    for line in output.lines() {
        let package = line.trim();

        if package.is_empty() {
            continue;
        }

        packages.insert(package);
    }

    if packages.is_empty() {
        bail!("Failed to find any packages matching files");
    }

    let version = packages.iter().copied().collect::<Vec<_>>().join(",");

    let modules_vec: Vec<_> = packages
        .iter()
        .map(|pkg| {
            parse_module(pkg)
                .with_context(|| format!("Failed to parse package metadata for package: '{pkg}'"))
        })
        .collect::<Result<_>>()?;

    Ok(ContentMetadata {
        timestamp: get_metadata_timestamp()?,
        version,
        versions: Some(modules_vec),
        #[cfg(efi_arch)]
        default_bootloader: None,
    })
}

/// Query the package owner of the given files using `query-file-owner`.
pub(crate) fn query_files<T>(
    sysroot_path: &str,
    paths: impl IntoIterator<Item = T>,
) -> Result<ContentMetadata>
where
    T: AsRef<Path>,
{
    //Combine with sysroot
    let query_files_script_path = Path::new(sysroot_path).join(QUERY_FILE_OWNER_SCRIPT);
    if !query_files_script_path.exists() {
        bail!(
            "Query file owner script not found at {:?}",
            query_files_script_path
        );
    }

    let mut cmd = std::process::Command::new(query_files_script_path);
    for path in paths {
        cmd.arg(path.as_ref());
    }
    let output = cmd.output().context("Failed to invoke query-file-owner")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("query-file-owner failed: {}", stderr.trim());
    }

    parse_package_metadata(&output.stdout)
}

fn parse_module(pkg: &str) -> Result<Module> {
    // New format: "NAME VERSION"
    if let Some((name, evr)) = pkg.split_once(' ') {
        return Ok(Module {
            name: name.to_string(),
            evr: evr.to_string(),
        });
    }

    // Legacy RPM format: "NAME-EVR.ARCH"
    let pkg = pkg.rsplit_once('.').map(|(pkg, _arch)| pkg).unwrap_or(pkg);

    let (separator, _) = pkg
        .char_indices()
        .filter(|(_, c)| *c == '-')
        .find_map(|(idx, _)| {
            let evr = &pkg[idx + 1..];

            if evr.starts_with(|c: char| c.is_ascii_digit()) {
                Some((idx, evr))
            } else {
                None
            }
        })
        .with_context(|| format!("Invalid legacy package metadata: {pkg:?}"))?;

    Ok(Module {
        name: pkg[..separator].to_string(),
        evr: pkg[separator + 1..].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_package_metadata() {
        let testdata = "\
            grub2-efi-x64 1:2.06-95.fc38
            grub2-efi-x64 1:2.06-95.fc38
            shim-x64 15.6-2
            shim-x64 15.6-2
            shim-x64 15.6-2
        ";

        let parsed = parse_package_metadata(testdata.as_bytes()).unwrap();

        assert_eq!(
            parsed.version,
            "grub2-efi-x64 1:2.06-95.fc38,shim-x64 15.6-2"
        );

        let expected_modules = vec![
            Module {
                name: "grub2-efi-x64".to_string(),
                evr: "1:2.06-95.fc38".to_string(),
            },
            Module {
                name: "shim-x64".to_string(),
                evr: "15.6-2".to_string(),
            },
        ];

        assert_eq!(parsed.versions, Some(expected_modules));
    }

    #[test]
    fn test_parse_module() {
        // New format: "NAME EVR"
        assert_eq!(
            parse_module("grub2-efi-x64 1:2.12-28.fc42").unwrap(),
            Module {
                name: "grub2-efi-x64".into(),
                evr: "1:2.12-28.fc42".into(),
            }
        );

        // Legacy RPM format: "NAME-EVR.ARCH"
        assert_eq!(
            parse_module("shim-x64-15.8-3.x86_64").unwrap(),
            Module {
                name: "shim-x64".into(),
                evr: "15.8-3".into(),
            }
        );

        // Legacy RPM format without an architecture suffix. The version must
        // not contain dots. Everything after the last dot is treated as the
        // architecture and stripped.
        assert_eq!(
            parse_module("shim-x64-15-3").unwrap(),
            Module {
                name: "shim-x64".into(),
                evr: "15-3".into(),
            }
        );
    }
}
