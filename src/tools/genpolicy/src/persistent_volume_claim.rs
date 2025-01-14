// Copyright (c) 2023 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

// Allow K8s YAML field names.
#![allow(non_snake_case)]

use crate::obj_meta;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// See Reference / Kubernetes API / Config and Storage Resources / PersistentVolumeClaim.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistentVolumeClaim {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apiVersion: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,

    pub metadata: obj_meta::ObjectMeta,
    pub spec: PersistentVolumeClaimSpec,
}

/// See Reference / Kubernetes API / Config and Storage Resources / PersistentVolumeClaim.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistentVolumeClaimSpec {
    pub resources: ResourceRequirements,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessModes: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub storageClassName: Option<String>,
    // TODO: additional fields.
}

/// See Reference / Kubernetes API / Config and Storage Resources / PersistentVolumeClaim.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResourceRequirements {
    #[serde(skip_serializing_if = "Option::is_none")]
    requests: Option<BTreeMap<String, String>>,
}
