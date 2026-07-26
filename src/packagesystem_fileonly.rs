use std::cmp::Ordering;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::prelude::*;
use serde::{Deserialize, Serialize};

use crate::model::ContentMetadata;

/// File-only module representation - independent of packagesystem::Module
#[derive(Serialize, Deserialize, Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Module {
    pub(crate) name: String,
    pub(crate) rpm_evr: String,
}

impl Module {
    pub(crate) fn rpm_evr(&self) -> &str {
        &self.rpm_evr
    }
}

impl Ord for Module {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name)
            .then_with(|| self.rpm_evr.cmp(&other.rpm_evr))
    }
}

impl PartialOrd for Module {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn sysroot_join(sysroot: &str, p: &Path) -> Result<std::path::PathBuf> {
    if p.is_absolute() {
        // strip leading '/'
        let rel = p.strip_prefix("/").unwrap_or(p);
        Ok(Path::new(sysroot).join(rel))
    } else {
        Ok(Path::new(sysroot).join(p))
    }
}

fn file_mtime_and_hash(path: &Path) -> Result<(DateTime<Utc>, String)> {
    let mut f = File::open(path).with_context(|| format!("Opening file {}", path.display()))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .with_context(|| format!("Reading file {}", path.display()))?;

    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let sysmtime = meta.modified().context("getting mtime")?;
    let dt: DateTime<Utc> = sysmtime.into();
    // sha256 prefix - use openssl's sha256 via the openssl crate
    let digest = openssl::sha::sha256(&buf);
    let hex = hex::encode(digest);
    // take first 12 hex chars to keep it short
    let prefix = if hex.len() > 12 { &hex[..12] } else { &hex[..] };
    let ver = format!("{}-{}", dt.timestamp(), prefix);
    Ok((dt, ver))
}

/// Query files under a sysroot and produce `ContentMetadata` using mtime+sha prefix.
/// File names are used as module names, versions are synthetic (<mtime>-<sha256prefix>).
pub(crate) fn query_files<T>(sysroot_path: &str, paths: impl IntoIterator<Item = T>) -> Result<ContentMetadata>
where
    T: AsRef<Path>,
{
    let mut modules: Vec<Module> = Vec::new();
    let mut latest: Option<DateTime<Utc>> = None;

    for p in paths {
        let p = p.as_ref();
        let real = sysroot_join(sysroot_path, p)?;
        if !real.exists() {
            bail!("File not found: {}", real.display());
        }
        let (dt, ver) = file_mtime_and_hash(&real)?;
        if latest.map_or(true, |l| dt > l) {
            latest = Some(dt);
        }
        let name = real
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| real.display().to_string());
        modules.push(Module {
            name,
            rpm_evr: ver,
        });
    }

    if modules.is_empty() {
        bail!("No files provided");
    }

    modules.sort_unstable();
    modules.dedup();

    let version = modules
        .iter()
        .map(|m| format!("{}-{}", m.name, m.rpm_evr))
        .collect::<Vec<_>>()
        .join(",");

    // Convert to packagesystem::Module for compatibility with ContentMetadata
    let versions_compat: Vec<crate::packagesystem::Module> = modules
        .into_iter()
        .map(|m| crate::packagesystem::Module {
            name: m.name,
            rpm_evr: m.rpm_evr,
        })
        .collect();

    Ok(ContentMetadata {
        timestamp: latest.unwrap(),
        version,
        versions: Some(versions_compat),
        #[cfg(efi_arch)]
        default_bootloader: None,
    })
}

// Comparators similar to the rpm-based module, but operate on the synthetic
// version strings lexicographically. These are provided for convenience if
// callers want to compare textual `version` fields.
pub(crate) fn parse_evr_vec(input: &str) -> Vec<Module> {
    let mut pkgs: Vec<Module> = input
        .split(',')
        .filter_map(|s| {
            if s.is_empty() {
                return None;
            }
            // Expect format "name-<mtime>-<sha>" – split at first '-' to get name
            let mut parts = s.splitn(2, '-');
            let name = parts.next().unwrap_or("");
            let evr = parts.next().unwrap_or("");
            Some(Module {
                name: name.to_string(),
                rpm_evr: evr.to_string(),
            })
        })
        .collect();
    pkgs.sort_unstable();
    pkgs.dedup();
    pkgs
}

pub(crate) fn compare_package_slices(a: &[Module], b: &[Module]) -> Ordering {
    let mut has_greater = false;
    for (pkg_a, pkg_b) in a.iter().zip(b.iter()) {
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
    use anyhow::Result;
    use tempfile::NamedTempFile;

    #[test]
    fn test_fileonly_query() -> Result<()> {
        let mut f = NamedTempFile::new()?;
        use std::io::Write;
        write!(f, "hello world")?;
        let p = f.path().to_path_buf();
        let meta = query_files("/", [p])?;
        assert!(!meta.version.is_empty());
        assert!(meta.versions.is_some());
        Ok(())
    }

    #[test]
    fn test_compare_fileonly_versions() {
        let v1 = "file1-1000-abc123,file2-2000-def456";
        let v2 = "file1-1001-abc123,file2-2000-def456";
        assert_eq!(compare_package_versions(v1, v2), Ordering::Less);
        assert_eq!(compare_package_versions(v2, v1), Ordering::Greater);
        assert_eq!(compare_package_versions(v1, v1), Ordering::Equal);
    }
}
