/*
 * Copyright (C) 2020 Red Hat, Inc.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use chrono::prelude::*;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::packagesystem::*;

/// The directory where updates are stored
pub(crate) const BOOTUPD_UPDATES_DIR: &str = "usr/lib/bootupd/updates";

#[derive(Serialize, Deserialize, Clone, Debug, Hash, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct ContentMetadata {
    /// The timestamp, which is used to determine update availability
    pub(crate) timestamp: DateTime<Utc>,
    /// Human readable version number, like ostree it is not ever parsed, just displayed
    pub(crate) version: String,
    /// Transfer version into Module struct list
    pub(crate) versions: Option<Vec<Module>>,
}

impl ContentMetadata {
    pub(crate) fn can_upgrade_to(&self, target: &Self) -> Ordering {
        if let (Some(self_versions), Some(target_versions)) = (&self.versions, &target.versions) {
            compare_package_slices(self_versions, target_versions)
        } else {
            compare_package_versions(&self.version, &target.version)
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct InstalledContent {
    /// Associated metadata
    pub(crate) meta: ContentMetadata,
    /// Human readable version number, like ostree it is not ever parsed, just displayed
    pub(crate) filetree: Option<crate::filetree::FileTree>,
    /// The version this was originally adopted from
    pub(crate) adopted_from: Option<ContentMetadata>,
}

/// Will be serialized into /boot/bootupd-state.json
#[derive(Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedState {
    /// Maps a component name to its currently installed version
    pub(crate) installed: BTreeMap<String, InstalledContent>,
    /// Maps a component name to an in progress update
    pub(crate) pending: Option<BTreeMap<String, ContentMetadata>>,
    /// If static bootloader configs are enabled, this contains the version
    pub(crate) static_configs: Option<ContentMetadata>,
}

/// The status of an individual component.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ComponentUpdatable {
    NoUpdateAvailable,
    AtLatestVersion,
    Upgradable,
    WouldDowngrade,
}

impl ComponentUpdatable {
    pub(crate) fn from_metadata(from: &ContentMetadata, to: Option<&ContentMetadata>) -> Self {
        match to {
            Some(to) => {
                match from.can_upgrade_to(to) {
                    Ordering::Equal => return ComponentUpdatable::AtLatestVersion, // from == to
                    Ordering::Less => return ComponentUpdatable::Upgradable,       // from < to
                    Ordering::Greater => return ComponentUpdatable::WouldDowngrade, // from > to
                }
            }
            None => ComponentUpdatable::NoUpdateAvailable,
        }
    }
}

/// The status of an individual component.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct ComponentStatus {
    /// Currently installed version
    pub(crate) installed: ContentMetadata,
    /// In progress update that was interrupted
    pub(crate) interrupted: Option<ContentMetadata>,
    /// Update in the deployed filesystem tree
    pub(crate) update: Option<ContentMetadata>,
    /// Is true if the version in `update` is different from `installed`
    pub(crate) updatable: ComponentUpdatable,
    /// Originally adopted version
    pub(crate) adopted_from: Option<ContentMetadata>,
}

/// Information on a component that can be adopted
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct Adoptable {
    /// A synthetic version
    pub(crate) version: ContentMetadata,
    /// True if we are likely to be able to reliably update this system
    pub(crate) confident: bool,
}

/// Representation of bootupd's worldview at a point in time.
/// This is intended to be a stable format that is output by `bootupctl status --json`
/// and parsed by higher level management tools.  Transitively then
/// everything referenced from here should also be stable.
#[derive(Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub(crate) struct Status {
    /// Maps a component name to status
    pub(crate) components: BTreeMap<String, ComponentStatus>,
    /// Components that appear to be installed, not via bootupd
    pub(crate) adoptable: BTreeMap<String, Adoptable>,
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::Result;
    use chrono::Duration;

    #[test]
    fn test_meta_compare() {
        let t = Utc::now();
        let a = ContentMetadata {
            timestamp: t,
            version: "grub2-efi-ia32-1:2.12-21.fc41.x86_64,grub2-efi-x64-1:2.12-21.fc41.x86_64,shim-ia32-15.8-3.x86_64,shim-x64-15.8-3.x86_64".into(),
            versions: None,
        };
        let b = ContentMetadata {
            timestamp: t + Duration::try_seconds(1).unwrap(),
            version: "grub2-efi-ia32-1:2.12-28.fc41.x86_64,grub2-efi-x64-1:2.12-28.fc41.x86_64,shim-ia32-15.8-3.x86_64,shim-x64-15.8-3.x86_64".into(),
            versions: None,
        };
        assert_eq!(a.can_upgrade_to(&b), Ordering::Less); // means upgradable
        assert_eq!(b.can_upgrade_to(&a), Ordering::Greater);

        // Compare versions if it is not none
        let a = ContentMetadata {
            timestamp: t,
            version: "test".into(),
            versions: Some(vec![
                Module {
                    name: "grub2".into(),
                    rpm_evr: "1:2.12-21.fc41".into(),
                },
                Module {
                    name: "shim".into(),
                    rpm_evr: "15.8-3".into(),
                },
            ]),
        };
        let b = ContentMetadata {
            timestamp: t + Duration::try_seconds(1).unwrap(),
            version: "test".into(),
            versions: Some(vec![
                Module {
                    name: "grub2".into(),
                    rpm_evr: "1:2.12-28.fc41".into(),
                },
                Module {
                    name: "shim".into(),
                    rpm_evr: "15.8-3".into(),
                },
            ]),
        };
        assert_eq!(a.can_upgrade_to(&b), Ordering::Less); // means upgradable
        assert_eq!(b.can_upgrade_to(&a), Ordering::Greater);
    }

    /// Validate we're not breaking the serialized format of /boot/bootupd-state.json
    #[test]
    fn test_deserialize_state() -> Result<()> {
        let data = include_str!("../tests/fixtures/example-state-v0.json");
        let state: SavedState = serde_json::from_str(data)?;
        let efi = state.installed.get("EFI").expect("EFI");
        assert_eq!(
            efi.meta.version,
            "grub2-efi-x64-1:2.04-23.fc32.x86_64,shim-x64-15-8.x86_64"
        );
        assert_eq!(efi.meta.versions, None);

        // Test the new versions
        let data = include_str!("../tests/fixtures/example-state-versions-v0.json");
        let state: SavedState = serde_json::from_str(data)?;
        let efi = state.installed.get("EFI").expect("EFI");
        assert_eq!(efi.meta.version, "grub2-1:2.12-41.fc44,shim-15.8-4");
        assert_eq!(
            efi.meta.versions,
            Some(vec![
                Module {
                    name: "grub2".into(),
                    rpm_evr: "1:2.12-41.fc44".into(),
                },
                Module {
                    name: "shim".into(),
                    rpm_evr: "15.8-4".into(),
                },
            ])
        );
        Ok(())
    }

    /// Validate we're not breaking the serialized format of `bootupctl status --json`
    #[test]
    fn test_deserialize_status() -> Result<()> {
        let data = include_str!("../tests/fixtures/example-status-v0.json");
        let status: Status = serde_json::from_str(data)?;
        let efi = status.components.get("EFI").expect("EFI");
        assert_eq!(
            efi.installed.version,
            "grub2-efi-x64-1:2.04-23.fc32.x86_64,shim-x64-15-8.x86_64"
        );
        assert_eq!(efi.installed.versions, None);
        Ok(())
    }
}
