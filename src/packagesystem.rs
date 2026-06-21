use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::prelude::*;
use serde::{Deserialize, Serialize};
use uapi_version::Version;

use crate::model::*;

//Manifest file is a alternative to rpm structure, its compatiable with rpm -q --qwertyformat structure. This file need to be generated first and it looks like this:
// grub2-1:2.12-28.fc42,1710000000 shim-15.8-3,170000000
const MANIFEST_PATH: &str = "usr/lib/bootupd/manifest";

//If any package starts with grub** shim**, this will make in one name in ANY distros
const CANONICAL_NAMES: &[&str] = &["grub", "shim"];

fn normalize_package_name(name: &str) -> &str {
    for canonical in CANONICAL_NAMES {
        if name == *canonical {
            return canonical;
        }

        // if package name is grub-efi -> grub, grub2-efi -> grub
        if let Some(rest) = name.strip_prefix(canonical) {
            let next = rest.chars().next();
            match next {
                Some(c) if c.is_ascii_digit() || !c.is_ascii_alphabetic() => return canonical,
                _ => {}
            }
        }
    }
    name
}

#[derive(Serialize, Deserialize, Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Module {
    pub(crate) name: String,
    pub(crate) rpm_evr: String,
}

impl Module {
    pub(crate) fn rpm_evr(&self) -> Version {
        Version::from(&self.rpm_evr)
    }

    fn canonical_name(&self) -> &str {
        normalize_package_name(&self.name)
    }
}

impl Ord for Module {
    fn cmp(&self, other: &Self) -> Ordering {
        self.canonical_name()
            .cmp(&other.canonical_name())
            .then_with(|| self.rpm_evr().cmp(&other.rpm_evr()))
    }
}

impl PartialOrd for Module {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_manifest(data: &[u8]) -> Result<ContentMetadata> {
    let pkgs = std::str::from_utf8(data)
        .context("Manifest is not valid UTF-8")?
        .split_whitespace()
        .map(|s| -> Result<_> {
            let mut parts = s.splitn(2, ',');
            let name = parts
                .next()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Missing package name in entry: {}", s))?;
            let ts_str = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("Missing buildtime in entry: {}", s))?;
            let ts = DateTime::parse_from_str(ts_str, "%s")
                .with_context(|| format!("Invalid buildtime in entry: {}", s))?
                .with_timezone(&Utc);
            Ok((name, ts))
        })
        .collect::<Result<BTreeMap<&str, DateTime<Utc>>>>()?;

    if pkgs.is_empty() {
        bail!("Manifest contains no entries");
    }

    let largest_timestamp = pkgs
        .values()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .last()
        .expect("pkgs is non-empty");

    let version = pkgs.keys().cloned().collect::<Vec<_>>().join(",");

    let mut modules: Vec<Module> = pkgs.keys().map(|s| parse_evr(s)).collect();
    modules.sort_unstable();
    modules.dedup();

    Ok(ContentMetadata {
        timestamp: *largest_timestamp,
        version,
        versions: Some(modules),
    })
}

pub(crate) fn query_files<T>(
    sysroot_path: &str,
    _paths: impl IntoIterator<Item = T>,
) -> Result<ContentMetadata>
where
    T: AsRef<Path>,
{
    let manifest_path = Path::new(sysroot_path).join(MANIFEST_PATH);
    let data = std::fs::read(&manifest_path)
        .with_context(|| format!("Failed to read manifest: {}", manifest_path.display()))?;
    parse_manifest(&data)
}

fn split_name_version(input: &str) -> Option<(String, String)> {
    let main = input.rsplit_once('.')?.0;
    let mut parts = main.rsplitn(3, '-');
    let release = parts.next()?;
    let version = parts.next()?;
    let name = parts.next()?;
    Some((name.to_string(), format!("{version}-{release}")))
}

//In this function if it using rpm use rpm_rs
fn parse_evr(pkg: &str) -> Module {
    if !pkg.ends_with(std::env::consts::ARCH) {
        let (name, evr) = pkg.split_once('-').unwrap_or((pkg, ""));
        return Module {
            name: name.to_string(),
            rpm_evr: evr.to_string(),
        };
    }

    let (name_str, rpm_evr) = {
        #[cfg(not(feature = "rpm"))]
        {
            split_name_version(pkg).unwrap()
        }
        #[cfg(feature = "rpm")]
        {
            let nevra = rpm_rs::Nevra::parse(pkg);
            (nevra.name().to_string(), nevra.evr().to_string())
        }
    };

    let (name, _) = name_str.split_once('-').unwrap_or((&name_str, ""));
    Module {
        name: name.to_string(),
        rpm_evr,
    }
}

fn parse_evr_vec(input: &str) -> Vec<Module> {
    let mut pkgs: Vec<Module> = input.split(',').map(|pkg| parse_evr(pkg)).collect();
    pkgs.sort_unstable();
    pkgs.dedup();
    pkgs
}

pub(crate) fn compare_package_slices(a: &[Module], b: &[Module]) -> Ordering {
    let mut has_greater = false;

    for (pkg_a, pkg_b) in a.iter().zip(b.iter()) {
        // Compare only versions - names are already normalized via canonical_name()
        // in Ord so sort order is consistent across distros.
        match pkg_a.cmp(pkg_b) {
            Ordering::Less => return Ordering::Less,
            Ordering::Greater => has_greater = true,
            Ordering::Equal => {}
        }
    }

    if a.len() < b.len() {
        return Ordering::Less;
    }
    if a.len() > b.len() {
        return Ordering::Greater;
    }

    if has_greater {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

pub(crate) fn compare_package_versions(a: &str, b: &str) -> Ordering {
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
    use tempfile::TempDir;

    fn write_manifest(dir: &TempDir, content: &str) {
        let manifest_dir = dir.path().join("usr/lib/bootupd");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::write(manifest_dir.join("manifest"), content).unwrap();
    }

    #[test]
    fn test_normalize_package_name() {
        // Canonic names without change
        assert_eq!(normalize_package_name("grub"), "grub");
        assert_eq!(normalize_package_name("shim"), "shim");
        // Grub variants
        assert_eq!(normalize_package_name("grub2"), "grub");
        assert_eq!(normalize_package_name("grub2-tools"), "grub");
        assert_eq!(normalize_package_name("grub2-efi-x64"), "grub");
        assert_eq!(normalize_package_name("grub2-efi-ia32"), "grub");
        assert_eq!(normalize_package_name("grub2-common"), "grub");
        assert_eq!(normalize_package_name("grub-efi-amd64"), "grub");
        assert_eq!(normalize_package_name("grub-efi-arm64"), "grub");
        assert_eq!(normalize_package_name("grub-pc"), "grub");
        // Shim Variants
        assert_eq!(normalize_package_name("shim-x64"), "shim");
        assert_eq!(normalize_package_name("shim-ia32"), "shim");
        assert_eq!(normalize_package_name("shim-signed"), "shim");
        assert_eq!(normalize_package_name("shim-unsigned"), "shim");
        // Should not (this is not grub, or shim)
        assert_eq!(normalize_package_name("grubby"), "grubby");
        assert_eq!(normalize_package_name("shimmer"), "shimmer");
        // Unkown packages, no changes
        assert_eq!(normalize_package_name("unknown-pkg"), "unknown-pkg");
    }

    #[test]
    fn test_parse_manifest() {
        let data =
            b"grub2-efi-x64-1:2.06-95.fc38.x86_64,1681321788 shim-x64-15.6-2.x86_64,1657222566 ";
        let parsed = parse_manifest(data).unwrap();
        assert_eq!(
            parsed.version,
            "grub2-efi-x64-1:2.06-95.fc38.x86_64,shim-x64-15.6-2.x86_64"
        );
        let modules = parsed.versions.unwrap();
        assert_eq!(modules[0].name, "grub2");
        assert_eq!(modules[0].rpm_evr, "1:2.06-95.fc38");
        assert_eq!(modules[1].name, "shim");
        assert_eq!(modules[1].rpm_evr, "15.6-2");
    }

    #[test]
    fn test_query_files_reads_manifest() {
        let dir = TempDir::new().unwrap();
        write_manifest(
            &dir,
            "grub2-1:2.12-28.fc42,1710000000 shim-15.8-3,1700000000 ",
        );
        let meta = query_files(dir.path().to_str().unwrap(), std::iter::empty::<&Path>()).unwrap();
        let modules = meta.versions.unwrap();
        assert_eq!(modules[0].name, "grub2");
        assert_eq!(modules[1].name, "shim");
    }

    #[test]
    fn test_query_files_missing_manifest() {
        let dir = TempDir::new().unwrap();
        let result = query_files(dir.path().to_str().unwrap(), std::iter::empty::<&Path>());
        assert!(result.is_err());
    }

    #[test]
    fn test_compare_cross_distro() {
        // grub2-efi-x64 (Fedora) vs grub (Arch) - that same version
        let fedora = vec![
            Module {
                name: "grub2-efi-x64".into(),
                rpm_evr: "1:2.12-28.fc42".into(),
            },
            Module {
                name: "shim-x64".into(),
                rpm_evr: "15.8-3".into(),
            },
        ];
        let arch = vec![
            Module {
                name: "grub".into(),
                rpm_evr: "1:2.12-28.fc42".into(),
            },
            Module {
                name: "shim-signed".into(),
                rpm_evr: "15.8-3".into(),
            },
        ];
        assert_eq!(compare_package_slices(&fedora, &arch), Ordering::Equal);

        // grub2-tools (fedora) vs Arch (grub)
        let rhel = vec![Module {
            name: "grub2-tools".into(),
            rpm_evr: "1:2.06-86.el9".into(),
        }];
        let arch_newer = vec![Module {
            name: "grub".into(),
            rpm_evr: "1:2.12-28.fc42".into(),
        }];
        assert_eq!(compare_package_slices(&rhel, &arch_newer), Ordering::Less);
    }

    #[test]
    fn test_compare_package_slices() {
        let a = vec![
            Module {
                name: "grub2".into(),
                rpm_evr: "1:2.12-21.fc41".into(),
            },
            Module {
                name: "shim".into(),
                rpm_evr: "15.8-3".into(),
            },
        ];
        let b = vec![
            Module {
                name: "grub2".into(),
                rpm_evr: "1:2.12-28.fc41".into(),
            },
            Module {
                name: "shim".into(),
                rpm_evr: "15.8-3".into(),
            },
        ];
        assert_eq!(compare_package_slices(&a, &b), Ordering::Less);
        assert_eq!(compare_package_slices(&b, &a), Ordering::Greater);
        assert_eq!(compare_package_slices(&a, &a), Ordering::Equal);
    }

    #[test]
    fn test_compare_package_versions() {
        let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-efi-x64-1:2.12-29.fc42.x86_64,shim-x64-15.8-3.x86_64";
        assert_eq!(compare_package_versions(current, target), Ordering::Less);
        assert_eq!(compare_package_versions(target, current), Ordering::Greater);

        let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-1:2.12-29.fc42,shim-15.8-3";
        assert_eq!(compare_package_versions(current, target), Ordering::Less);
        assert_eq!(compare_package_versions(target, current), Ordering::Greater);

        let current = "grub2-1:2.12-28.fc42,shim-15.8-3";
        let target = "grub2-1:2.12-28.fc42,shim-15.8-4";
        assert_eq!(compare_package_versions(current, target), Ordering::Less);
        assert_eq!(compare_package_versions(target, current), Ordering::Greater);

        let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64,test";
        assert_eq!(compare_package_versions(current, target), Ordering::Less);
        assert_eq!(compare_package_versions(target, current), Ordering::Greater);

        {
            let current = "grub2-1:2.12-28.fc42,shim-15.8-3";
            let target = "grub2-1:2.12-27.fc42,shim-15.8-4";
            assert_eq!(compare_package_versions(current, target), Ordering::Less);
            assert_eq!(compare_package_versions(target, current), Ordering::Less);
        }

        {
            let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
            let target = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
            assert_eq!(compare_package_versions(current, target), Ordering::Equal);

            let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
            let target = "grub2-1:2.12-28.fc42,shim-15.8-3";
            assert_eq!(compare_package_versions(current, target), Ordering::Equal);

            let current = "grub2-1:2.12-28.fc42,shim-15.8-3";
            let target = "grub2-1:2.12-28.fc42,shim-15.8-3";
            assert_eq!(compare_package_versions(current, target), Ordering::Equal);
        }

        let current = "grub2-tools-1:2.06-86.el9_4.3.x86_64";
        let target = "grub2-tools-1:2.06-110.el9.x86_64";
        assert_eq!(compare_package_versions(current, target), Ordering::Less);
        assert_eq!(compare_package_versions(target, current), Ordering::Greater);

        let current = "grub2-efi-ia32-1:2.12-21.fc41.x86_64,grub2-efi-x64-1:2.12-21.fc41.x86_64,shim-ia32-15.8-3.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-1:2.12-28.fc42,shim-15.8-3";
        assert_eq!(compare_package_versions(current, target), Ordering::Less);
        assert_eq!(compare_package_versions(target, current), Ordering::Greater);
    }
}
