/*
 * Copyright (C) 2020 Red Hat, Inc.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use chrono::prelude::*;
use serde_derive::{Deserialize, Serialize};

use crate::component::*;

/// The directory where updates are stored
pub(crate) const BOOTUPD_UPDATES_DIR: &str = "usr/lib/bootupd/updates";

#[derive(Serialize, Deserialize, Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct ContentMetadata {
    /// The timestamp, which is used to determine update availability
    pub(crate) timestamp: NaiveDateTime,
    /// Human readable version number, like ostree it is not ever parsed, just displayed
    pub(crate) version: Option<String>,
}

/// Our total view of the world at a point in time
#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct Status {
    pub(crate) supported_architecture: bool,
    pub(crate) components: Vec<Box<dyn Component>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct SavedComponent {
    pub(crate) component: Box<dyn Component>,
    pub(crate) metadata: ContentMetadata,
}

/// Will be serialized into /boot/bootupd-state.json
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct SavedState {
    pub(crate) components: Vec<SavedComponent>,
}

// Should be stored in /usr/lib/bootupd/edges.json
//#[derive(Serialize, Deserialize, Debug)]
// #[serde(rename_all = "kebab-case")]
// pub(crate) struct UpgradeEdge {
//     /// Set to true if we should upgrade from an unknown state
//     #[serde(default)]
//     pub(crate) from_unknown: bool,
//     /// Upgrade from content past this timestamp
//     pub(crate) from_timestamp: Option<NaiveDateTime>,
// }
