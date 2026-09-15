use std::cmp::Ordering;
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

fn parse_module_vec(input: &str) -> Result<Vec<Module>> {
    let mut pkgs: Vec<Module> = input
        .split(',')
        .map(|pkg| parse_module(pkg)) // parse_module returns owned Module
        .collect::<Result<Vec<_>>>()?;
    // Sort packages to ensure a consistent order for comparison, which is
    // required by `compare_package_slices`.
    pkgs.sort_unstable();
    // Now that it's sorted, we can efficiently remove duplicates.
    pkgs.dedup();
    Ok(pkgs)
}

pub(crate) fn compare_package_slices(a: &[Module], b: &[Module]) -> Ordering {
    let mut has_greater = false;

    // Assume it is in order
    for (pkg_a, pkg_b) in a.iter().zip(b.iter()) {
        match pkg_a.cmp(pkg_b) {
            Ordering::Less => return Ordering::Less, // upgradable
            Ordering::Greater => has_greater = true, // downgrade
            Ordering::Equal => {}
        }
    }

    // If all compared equal, longer slice wins
    if a.len() < b.len() {
        return Ordering::Less; // extra packages in b → upgrade
    }
    if a.len() > b.len() {
        return Ordering::Greater; // extra packages in a → downgrade
    }

    if has_greater {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

// Compare package versions:
// If any package is Ordering::Less, return Ordering::Less, means upgradable,
// Else if any package is Ordering::Greater, return Ordering::Greater,
// Else (all equal), return Ordering::Equal.
pub(crate) fn compare_package_versions(a: &str, b: &str) -> Ordering {
    // Fast path: if the two values are equal, skip detailed comparison
    if a == b {
        return Ordering::Equal;
    }
    let pkg_a = parse_module_vec(a).unwrap_or_default();
    let pkg_b = parse_module_vec(b).unwrap_or_default();
    compare_package_slices(&pkg_a, &pkg_b)
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
    fn test_compare_package_slices() {
        let a = vec![
            Module {
                name: "grub2".into(),
                evr: "1:2.12-21.fc41".into(),
            },
            Module {
                name: "shim".into(),
                evr: "15.8-3".into(),
            },
        ];
        let b = vec![
            Module {
                name: "grub2".into(),
                evr: "1:2.12-28.fc41".into(),
            },
            Module {
                name: "shim".into(),
                evr: "15.8-3".into(),
            },
        ];
        let ord = compare_package_slices(&a, &b);
        assert_eq!(ord, Ordering::Less);

        let ord = compare_package_slices(&b, &a);
        assert_eq!(ord, Ordering::Greater);

        let ord = compare_package_slices(&a, &a);
        assert_eq!(ord, Ordering::Equal);
    }

    #[test]
    fn test_compare_legacy_and_new_metadata() {
        let legacy = "grub2-efi-ia32-1:2.12-21.fc41.x86_64,\
         grub2-efi-x64-1:2.12-21.fc41.x86_64,\
         shim-ia32-15.8-3.x86_64,\
         shim-x64-15.8-3.x86_64";

        let new = "grub2-efi-ia32 1:2.12-28.fc41,\
         grub2-efi-x64 1:2.12-28.fc41,\
         shim-ia32 15.8-3,\
         shim-x64 15.8-3";

        assert_eq!(compare_package_versions(legacy, new), Ordering::Less);
    }

    #[test]
    fn test_compare_legacy_and_new_equal_metadata() {
        let legacy = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";

        let new = "grub2-efi-x64 1:2.12-28.fc42,shim-x64 15.8-3";

        assert_eq!(compare_package_versions(legacy, new), Ordering::Equal);
    }
    #[test]
    fn test_compare_package_versions() {
        // Test 1: Same packages, different versions
        let current = "grub2-efi-x64 1:2.12-28.fc42,shim-x64 15.8-3";
        let target = "grub2-efi-x64 1:2.12-29.fc42,shim-x64 15.8-3";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less); // current < target

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        // Test 2: Different package names but same version comparison logic
        let current = "grub2 1:2.12-28.fc42,shim 15.8-3";
        let target = "grub2 1:2.12-28.fc42,shim 15.8-4";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less); // current < target

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        // Test 3: Target includes new package, should upgrade
        let current = "grub2-efi-x64 1:2.12-28.fc42,shim-x64 15.8-3";
        let target = "grub2-efi-x64 1:2.12-28.fc42,shim-x64 15.8-3,test 1.0";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less);

        // The target missed some package
        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        // Test 4: Mixed comparison (different ordering)
        {
            let current = "grub2 1:2.12-28.fc42,shim 15.8-3";
            let target = "grub2 1:2.12-27.fc42,shim 15.8-4";
            let ord = compare_package_versions(current, target);
            assert_eq!(ord, Ordering::Less);

            let ord = compare_package_versions(target, current);
            assert_eq!(ord, Ordering::Less);
        }

        // Test 5: Equal versions
        {
            let current = "grub2-efi-x64 1:2.12-28.fc42,shim-x64 15.8-3";
            let target = "grub2-efi-x64 1:2.12-28.fc42,shim-x64 15.8-3";
            let ord = compare_package_versions(current, target);
            assert_eq!(ord, Ordering::Equal);

            let current = "grub2 1:2.12-28.fc42,shim 15.8-3";
            let target = "grub2 1:2.12-28.fc42,shim 15.8-3";
            let ord = compare_package_versions(current, target);
            assert_eq!(ord, Ordering::Equal);
        }

        // Test 6: Single package comparison
        let current = "grub2-tools 1:2.06-86.el9_4.3";
        let target = "grub2-tools 1:2.06-110.el9";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less);

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        // Test 7: Multiple packages with different names
        let current = "grub2-efi-ia32 1:2.12-21.fc41,grub2-efi-x64 1:2.12-21.fc41,shim-ia32 15.8-3,shim-x64 15.8-3";
        let target = "grub2-efi-ia32 1:2.12-28.fc42,grub2-efi-x64 1:2.12-28.fc42,shim-ia32 15.8-3,shim-x64 15.8-3";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less);

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);
    }
}
