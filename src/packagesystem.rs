use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::prelude::*;
use uapi_version::Version;

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
struct Package {
    name: String,
    rpm_evr: String,
}

impl Package {
    pub(crate) fn rpm_evr(&self) -> Version {
        Version::from(&self.rpm_evr)
    }
}

impl Ord for Package {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name) // Compare names first
            .then_with(|| self.rpm_evr().cmp(&other.rpm_evr())) // If names equal, compare versions
    }
}

impl PartialOrd for Package {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// Copy from https://github.com/rpm-rs/rpm/pull/277
#[allow(dead_code)]
pub fn parse_values(nevra: &str) -> (&str, &str, &str, &str, &str) {
    // 1. Split Architecture from the right.
    // Example: "foo-1:2.3-4.x86_64" -> ("foo-1:2.3-4", "x86_64")
    let (nevr, arch) = nevra.rsplit_once('.').unwrap_or((nevra, ""));

    // 2. Split Release from the right of the remainder.
    // Example: "foo-1:2.3-4" -> ("foo-1:2.3", "4")
    let (nev, release) = nevr.rsplit_once('-').unwrap_or((nevr, ""));

    // 3. Split Version (with potential Epoch) from the right of the remainder.
    // Example: "foo-1:2.3" -> ("foo", "1:2.3")
    let (name, version_epoch) = nev.rsplit_once('-').unwrap_or((nev, ""));

    // 4. Check the version part for an Epoch.
    // The epoch is separated by a colon. If no colon exists, the epoch is empty.
    let (epoch, version) = match version_epoch.split_once(':') {
        // Example: "1:2.3" -> ("1", "2.3")
        Some((e, v)) => (e, v),
        // Example: "2.3" -> ("", "2.3")
        None => ("", version_epoch),
    };
    (name, epoch, version, release, arch)
}

fn parse_evr(pkg: &str) -> Package {
    // assume it is "grub2-1:2.12-28.fc42" (from usr/lib/efi)
    if !pkg.ends_with(std::env::consts::ARCH) {
        let (name, evr) = pkg.split_once('-').unwrap_or((pkg, ""));
        return Package {
            name: name.to_string(),
            rpm_evr: evr.to_string(),
        };
    }

    let (name_str, rpm_evr) = {
        #[cfg(not(feature = "rpm"))]
        {
            let (name, epoch, version, release, _arch) = parse_values(pkg);
            (name.to_string(), format!("{epoch}:{version}-{release}"))
        }
        #[cfg(feature = "rpm")]
        {
            let nevra = rpm_rs::Nevra::parse(pkg);
            (nevra.name().to_string(), nevra.evr().to_string())
        }
    };

    let (name, _) = name_str.split_once('-').unwrap_or((&name_str, ""));
    Package {
        name: name.to_string(),
        rpm_evr,
    }
}

fn parse_evr_vec(input: &str) -> Vec<Package> {
    let mut pkgs: Vec<Package> = input
        .split(',')
        .map(|pkg| parse_evr(pkg)) // parse_evr returns owned Package
        .collect();
    // Sort packages to ensure a consistent order for comparison, which is
    // required by `compare_package_slices`.
    pkgs.sort_unstable();
    // Now that it's sorted, we can efficiently remove duplicates.
    pkgs.dedup();
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
