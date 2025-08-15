use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::prelude::*;
use rpm::Evr;

use crate::model::*;
use crate::ostreeutil;

/// Parse the output of `rpm -q`
fn rpm_parse_metadata(stdout: &[u8]) -> Result<ContentMetadata> {
    let pkgs = std::str::from_utf8(stdout)?
        .split_whitespace()
        .map(|s| -> Result<_> {
            let parts: Vec<_> = s.splitn(2, ',').collect();
            let name = parts[0];
            if let Some(ts) = parts.get(1) {
                let nt = DateTime::parse_from_str(ts, "%s")
                    .context("Failed to parse rpm buildtime")?
                    .with_timezone(&chrono::Utc);
                Ok((name, nt))
            } else {
                bail!("Failed to parse: {}", s);
            }
        })
        .collect::<Result<BTreeMap<&str, DateTime<Utc>>>>()?;
    if pkgs.is_empty() {
        bail!("Failed to find any RPM packages matching files in source efidir");
    }
    let timestamps: BTreeSet<&DateTime<Utc>> = pkgs.values().collect();
    // Unwrap safety: We validated pkgs has at least one value above
    let largest_timestamp = timestamps.iter().last().unwrap();
    let version = pkgs.keys().fold("".to_string(), |mut s, n| {
        if !s.is_empty() {
            s.push(',');
        }
        s.push_str(n);
        s
    });
    Ok(ContentMetadata {
        timestamp: **largest_timestamp,
        version,
    })
}

/// Query the rpm database and list the package and build times.
pub(crate) fn query_files<T>(
    sysroot_path: &str,
    paths: impl IntoIterator<Item = T>,
) -> Result<ContentMetadata>
where
    T: AsRef<Path>,
{
    let mut c = ostreeutil::rpm_cmd(sysroot_path)?;
    c.args(["-q", "--queryformat", "%{nevra},%{buildtime} ", "-f"]);
    for arg in paths {
        c.arg(arg.as_ref());
    }

    let rpmout = c.output()?;
    if !rpmout.status.success() {
        std::io::stderr().write_all(&rpmout.stderr)?;
        bail!("Failed to invoke rpm -qf");
    }

    rpm_parse_metadata(&rpmout.stdout)
}

#[derive(Debug, Eq, PartialEq)]
struct Package<'a> {
    name: String,
    version: Evr<'a>,
}

impl<'a> Ord for Package<'a> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name) // Compare names first
            .then_with(|| self.version.cmp(&other.version)) // If names equal, compare versions
    }
}

impl<'a> PartialOrd for Package<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_evr(pkg: &str) -> Package {
    // assume it is "grub2-1:2.12-28.fc42" with no suffix .ARCH
    if !pkg.ends_with(std::env::consts::ARCH) {
        let (name, version_release) = pkg.split_once('-').unwrap_or((pkg, ""));
        return Package {
            name: name.to_string(),
            version: Evr::parse(version_release),
        };
    }

    // assume it is "grub2-efi-x64-1:2.12-28.fc42.x86_64"
    let nevra = rpm::Nevra::parse(pkg);
    let _name = nevra.name();
    // get name as "grub2" to match the usr/lib/efi path
    let (name, _) = _name.split_once('-').unwrap_or((_name, ""));
    Package {
        name: name.to_string(),
        version: Evr::new(
            nevra.epoch().to_string(),
            nevra.version().to_string(),
            nevra.release().to_string(),
        ),
    }
}

fn parse_evr_vec(input: &str) -> Vec<Package> {
    let mut pkgs: Vec<Package> = input
        .split(',')
        .map(|pkg| parse_evr(pkg)) // parse_evr returns owned Package
        .collect();
    // Remove duplicates while preserving order
    let mut seen = BTreeSet::new();
    pkgs.retain(|pkg| seen.insert((pkg.name.clone(), pkg.version.clone())));
    pkgs
}

fn compare_package_slices(a: &[Package], b: &[Package]) -> Ordering {
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
    let pkg_a = parse_evr_vec(a);
    let pkg_b = parse_evr_vec(b);
    compare_package_slices(&pkg_a, &pkg_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rpmout() {
        let testdata = "grub2-efi-x64-1:2.06-95.fc38.x86_64,1681321788 grub2-efi-x64-1:2.06-95.fc38.x86_64,1681321788 shim-x64-15.6-2.x86_64,1657222566 shim-x64-15.6-2.x86_64,1657222566 shim-x64-15.6-2.x86_64,1657222566";
        let parsed = rpm_parse_metadata(testdata.as_bytes()).unwrap();
        assert_eq!(
            parsed.version,
            "grub2-efi-x64-1:2.06-95.fc38.x86_64,shim-x64-15.6-2.x86_64"
        );
    }

    #[test]
    fn test_compare_package_versions() {
        let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-efi-x64-1:2.12-29.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less); // current < target

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-1:2.12-29.fc42,shim-15.8-3";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less); // current < target

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        let current = "grub2-1:2.12-28.fc42,shim-15.8-3";
        let target = "grub2-1:2.12-28.fc42,shim-15.8-4";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less); // current < target

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        // The target includes new package, should upgrade
        let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64,test";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less);

        // The target missed some package
        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        // Not sure if this would happen
        // current_grub2 > target_grub2
        // current_shim < target_shim
        // In this case there is Ordering::Less, return Ordering::Less
        {
            let current = "grub2-1:2.12-28.fc42,shim-15.8-3";
            let target = "grub2-1:2.12-27.fc42,shim-15.8-4";
            let ord = compare_package_versions(current, target);
            assert_eq!(ord, Ordering::Less);

            let ord = compare_package_versions(target, current);
            assert_eq!(ord, Ordering::Less);
        }

        // Test Equal
        {
            let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
            let target = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
            let ord = compare_package_versions(current, target);
            assert_eq!(ord, Ordering::Equal);

            let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
            let target = "grub2-1:2.12-28.fc42,shim-15.8-3";
            let ord = compare_package_versions(current, target);
            assert_eq!(ord, Ordering::Equal);

            let current = "grub2-1:2.12-28.fc42,shim-15.8-3";
            let target = "grub2-1:2.12-28.fc42,shim-15.8-3";
            let ord = compare_package_versions(current, target);
            assert_eq!(ord, Ordering::Equal);
        }

        // Test only grub2
        let current = "grub2-1:2.12-28.fc42";
        let target = "grub2-1:2.12-29.fc42";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less);

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        let current = "grub2-efi-ia32-1:2.12-21.fc41.x86_64,grub2-efi-x64-1:2.12-21.fc41.x86_64,shim-ia32-15.8-3.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-1:2.12-28.fc42,shim-15.8-3";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less);

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);
    }
}
