// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Types helpful for diffing blueprints.

use super::blueprint_display::{
    constants::*, linear_table_modified, linear_table_unchanged,
    BpDatasetsSubtableSchema, BpDiffState, BpGeneration,
    BpOmicronZonesSubtableSchema, BpPhysicalDisksSubtableSchema,
    BpSledSubtable, BpSledSubtableColumn, BpSledSubtableData,
    BpSledSubtableRow, KvListWithHeading, KvPair,
};
use super::{zone_sort_key, CockroachDbPreserveDowngrade};
use nexus_sled_agent_shared::inventory::ZoneKind;
use omicron_common::api::external::Generation;
use omicron_common::disk::DiskIdentity;
use omicron_uuid_kinds::OmicronZoneUuid;
use omicron_uuid_kinds::SledUuid;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::deployment::{
    BlueprintDatasetConfigForDiff, BlueprintDatasetsConfig, BlueprintMetadata,
    BlueprintOrCollectionDatasetsConfig, BlueprintOrCollectionDisksConfig,
    BlueprintOrCollectionZoneConfig, BlueprintOrCollectionZonesConfig,
    BlueprintPhysicalDisksConfig, BlueprintZoneConfig,
    BlueprintZoneDisposition, BlueprintZonesConfig,
    CollectionDatasetIdentifier, DiffBeforeMetadata, ZoneSortKey,
};
use crate::external_api::views::SledState;

/// Diffs for omicron zones on a given sled with a given `BpDiffState`
#[derive(Debug)]
pub struct BpDiffZoneDetails {
    pub generation_before: Option<Generation>,
    pub generation_after: Option<Generation>,
    pub zones: Vec<BlueprintOrCollectionZoneConfig>,
}

impl BpSledSubtableData for BpDiffZoneDetails {
    fn bp_generation(&self) -> BpGeneration {
        BpGeneration::Diff {
            before: self.generation_before,
            after: self.generation_after,
        }
    }

    fn rows(
        &self,
        state: BpDiffState,
    ) -> impl Iterator<Item = BpSledSubtableRow> {
        self.zones.iter().map(move |zone| {
            BpSledSubtableRow::from_strings(
                state,
                vec![
                    zone.kind().report_str().to_string(),
                    zone.id().to_string(),
                    zone.disposition().to_string(),
                    zone.underlay_ip().to_string(),
                ],
            )
        })
    }
}

/// A modified omicron zone
///
/// A zone is considered modified if its `disposition` changes. All
/// modifications to other fields are considered errors, and will be recorded
/// as such.
#[derive(Debug)]
pub struct ModifiedZone {
    pub prior_disposition: BlueprintZoneDisposition,
    pub zone: BlueprintOrCollectionZoneConfig,
}

impl ZoneSortKey for ModifiedZone {
    fn kind(&self) -> ZoneKind {
        self.zone.kind()
    }

    fn id(&self) -> OmicronZoneUuid {
        self.zone.id()
    }
}

impl ModifiedZone {
    #[allow(clippy::result_large_err)]
    pub fn new(
        before: BlueprintOrCollectionZoneConfig,
        after: BlueprintZoneConfig,
    ) -> Result<ModifiedZone, BpDiffZoneError> {
        // Do we have any errors? If so, create a "reason" string.
        let mut reason = String::new();
        if before.kind() != after.kind() {
            let msg = format!(
                "mismatched zone kind: before: {}, after: {}\n",
                before.kind().report_str(),
                after.kind().report_str(),
            );
            reason.push_str(&msg);
        }
        if before.underlay_ip() != after.underlay_ip() {
            let msg = format!(
                "mismatched underlay IP: before: {}, after: {}\n",
                before.underlay_ip(),
                after.underlay_ip()
            );
            reason.push_str(&msg);
        }
        if !before.is_zone_type_equal(&after.zone_type) {
            let msg = format!(
                "mismatched zone type: after: {:#?}\n",
                after.zone_type
            );
            reason.push_str(&msg);
        }
        if reason.is_empty() {
            Ok(ModifiedZone {
                prior_disposition: before.disposition(),
                zone: after.into(),
            })
        } else {
            Err(BpDiffZoneError {
                zone_before: before,
                zone_after: after.into(),
                reason,
            })
        }
    }
}

/// Details of modified zones on a given sled
#[derive(Debug)]
pub struct BpDiffZonesModified {
    pub generation_before: Generation,
    pub generation_after: Generation,
    pub zones: Vec<ModifiedZone>,
}

impl BpSledSubtableData for BpDiffZonesModified {
    fn bp_generation(&self) -> BpGeneration {
        BpGeneration::Diff {
            before: Some(self.generation_before),
            after: Some(self.generation_after),
        }
    }

    fn rows(
        &self,
        state: BpDiffState,
    ) -> impl Iterator<Item = BpSledSubtableRow> {
        self.zones.iter().map(move |zone| {
            BpSledSubtableRow::new(
                state,
                vec![
                    BpSledSubtableColumn::value(
                        zone.zone.kind().report_str().to_string(),
                    ),
                    BpSledSubtableColumn::value(zone.zone.id().to_string()),
                    BpSledSubtableColumn::diff(
                        zone.prior_disposition.to_string(),
                        zone.zone.disposition().to_string(),
                    ),
                    BpSledSubtableColumn::value(
                        zone.zone.underlay_ip().to_string(),
                    ),
                ],
            )
        })
    }
}

#[derive(Debug)]
/// Errors arising from illegally modified zone fields
pub struct BpDiffZoneErrors {
    pub generation_before: Generation,
    pub generation_after: Generation,
    pub errors: Vec<BpDiffZoneError>,
}

#[derive(Debug)]
pub struct BpDiffZoneError {
    pub zone_before: BlueprintOrCollectionZoneConfig,
    pub zone_after: BlueprintOrCollectionZoneConfig,
    pub reason: String,
}

/// All known zones across all known sleds, their various states, and errors
#[derive(Debug, Default)]
pub struct BpDiffZones {
    pub added: BTreeMap<SledUuid, BpDiffZoneDetails>,
    pub removed: BTreeMap<SledUuid, BpDiffZoneDetails>,
    pub unchanged: BTreeMap<SledUuid, BpDiffZoneDetails>,
    pub modified: BTreeMap<SledUuid, BpDiffZonesModified>,
    pub errors: BTreeMap<SledUuid, BpDiffZoneErrors>,
}

impl BpDiffZones {
    pub fn new(
        before: BTreeMap<SledUuid, BlueprintOrCollectionZonesConfig>,
        mut after: BTreeMap<SledUuid, BlueprintZonesConfig>,
    ) -> Self {
        let mut diffs = BpDiffZones::default();
        for (sled_id, before_zones) in before {
            let before_generation = before_zones.generation();
            let mut removed = vec![];
            if let Some(after_zones) = after.remove(&sled_id) {
                let after_generation = after_zones.generation;
                let mut unchanged = vec![];
                let mut modified = vec![];
                let mut errors = vec![];
                let mut added = vec![];

                // Compare `before_zones` and `after_zones` to look
                // for additions, deletions, modifications, and errors.
                let before_by_id: BTreeMap<_, BlueprintOrCollectionZoneConfig> =
                    before_zones.zones().map(|z| (z.id(), z)).collect();
                let mut after_by_id: BTreeMap<_, BlueprintZoneConfig> =
                    after_zones.zones.into_iter().map(|z| (z.id, z)).collect();

                for (zone_id, zone_before) in before_by_id {
                    if let Some(zone_after) = after_by_id.remove(&zone_id) {
                        // Are the zones equal?
                        if zone_before == zone_after {
                            unchanged.push(zone_after.into());
                        } else {
                            // The zones are different. They are only allowed to differ in terms
                            // of `disposition`, otherwise we have an error.
                            match ModifiedZone::new(zone_before, zone_after) {
                                Ok(modified_zone) => {
                                    modified.push(modified_zone)
                                }
                                Err(error) => errors.push(error),
                            }
                        }
                    } else {
                        // This zone doesn't exist in `zone_after` so it must have
                        // been removed.
                        removed.push(zone_before);
                    }
                }
                // Any remaining zones in `after_by_id` are newly added
                for (_, zone_after) in after_by_id {
                    added.push(zone_after.into());
                }

                // Add all records to `diffs` that come from either `before` or `after`
                // for this `sled_id`.
                if !unchanged.is_empty() {
                    unchanged.sort_unstable_by_key(zone_sort_key);
                    diffs.unchanged.insert(
                        sled_id,
                        BpDiffZoneDetails {
                            generation_before: Some(before_generation),
                            generation_after: Some(after_generation),
                            zones: unchanged,
                        },
                    );
                }
                if !removed.is_empty() {
                    removed.sort_unstable_by_key(zone_sort_key);
                    diffs.removed.insert(
                        sled_id,
                        BpDiffZoneDetails {
                            generation_before: Some(before_generation),
                            generation_after: Some(after_generation),
                            zones: removed,
                        },
                    );
                }
                if !added.is_empty() {
                    added.sort_unstable_by_key(zone_sort_key);
                    diffs.added.insert(
                        sled_id,
                        BpDiffZoneDetails {
                            generation_before: Some(before_generation),
                            generation_after: Some(after_generation),
                            zones: added,
                        },
                    );
                }
                if !modified.is_empty() {
                    modified.sort_unstable_by_key(zone_sort_key);
                    diffs.modified.insert(
                        sled_id,
                        BpDiffZonesModified {
                            generation_before: before_generation,
                            generation_after: after_generation,
                            zones: modified,
                        },
                    );
                }
                if !errors.is_empty() {
                    diffs.errors.insert(
                        sled_id,
                        BpDiffZoneErrors {
                            generation_before: before_generation,
                            generation_after: after_generation,
                            errors,
                        },
                    );
                }
            } else {
                // No `after_zones` for this `sled_id`, so `before_zones` are removed
                assert!(removed.is_empty());
                for zone in before_zones.zones() {
                    removed.push(zone);
                }

                if !removed.is_empty() {
                    removed.sort_unstable_by_key(zone_sort_key);
                    diffs.removed.insert(
                        sled_id,
                        BpDiffZoneDetails {
                            generation_before: Some(before_generation),
                            generation_after: None,
                            zones: removed,
                        },
                    );
                }
            }
        }

        // Any sleds remaining in `after` have just been added, since we remove
        // sleds from `after`, that were also in `before`, in the above loop.
        for (sled_id, after_zones) in after {
            if !after_zones.zones.is_empty() {
                diffs.added.insert(
                    sled_id,
                    BpDiffZoneDetails {
                        generation_before: None,
                        generation_after: Some(after_zones.generation),
                        zones: after_zones
                            .zones
                            .into_iter()
                            .map(|z| z.into())
                            .collect(),
                    },
                );
            }
        }

        diffs
    }

    /// Return a [`BpSledSubtable`] for the given `sled_id`
    ///
    /// We collate all the data from each category to produce a single table.
    /// The order is:
    ///
    /// 1. Unchanged
    /// 2. Removed
    /// 3. Modified
    /// 4. Added
    ///
    /// The idea behind the order is to (a) group all changes together
    /// and (b) put changes towards the bottom, so people have to scroll
    /// back less.
    ///
    /// Errors are printed in a more freeform manner after the table is
    /// displayed.
    pub fn to_bp_sled_subtable(
        &self,
        sled_id: &SledUuid,
    ) -> Option<BpSledSubtable> {
        let mut generation = BpGeneration::Diff { before: None, after: None };
        let mut rows = vec![];
        if let Some(diff) = self.unchanged.get(sled_id) {
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Unchanged));
        }
        if let Some(diff) = self.removed.get(sled_id) {
            // Generations never vary for the same sled, so this is harmless
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Removed));
        }

        if let Some(diff) = self.modified.get(sled_id) {
            // Generations never vary for the same sled, so this is harmless
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Modified));
        }

        if let Some(diff) = self.added.get(sled_id) {
            // Generations never vary for the same sled, so this is harmless
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Added));
        }

        if rows.is_empty() {
            None
        } else {
            Some(BpSledSubtable::new(
                BpOmicronZonesSubtableSchema {},
                generation,
                rows,
            ))
        }
    }
}

#[derive(Debug)]
pub struct DiffPhysicalDisksDetails {
    // Disks that come from inventory don't have generation numbers
    pub before_generation: Option<Generation>,

    // Disks that are removed don't have "after" generation numbers
    pub after_generation: Option<Generation>,

    // Disks added, removed, or unmodified
    pub disks: BTreeSet<DiskIdentity>,
}

impl BpSledSubtableData for DiffPhysicalDisksDetails {
    fn bp_generation(&self) -> BpGeneration {
        BpGeneration::Diff {
            before: self.before_generation,
            after: self.after_generation,
        }
    }

    fn rows(
        &self,
        state: BpDiffState,
    ) -> impl Iterator<Item = BpSledSubtableRow> {
        self.disks.iter().map(move |d| {
            BpSledSubtableRow::from_strings(
                state,
                vec![d.vendor.clone(), d.model.clone(), d.serial.clone()],
            )
        })
    }
}

#[derive(Debug, Default)]
pub struct BpDiffPhysicalDisks {
    pub added: BTreeMap<SledUuid, DiffPhysicalDisksDetails>,
    pub removed: BTreeMap<SledUuid, DiffPhysicalDisksDetails>,
    pub unchanged: BTreeMap<SledUuid, DiffPhysicalDisksDetails>,
}

impl BpDiffPhysicalDisks {
    pub fn new(
        before: BTreeMap<SledUuid, BlueprintOrCollectionDisksConfig>,
        mut after: BTreeMap<SledUuid, BlueprintPhysicalDisksConfig>,
    ) -> Self {
        let mut diffs = BpDiffPhysicalDisks::default();
        for (sled_id, before_disks) in before {
            let before_generation = before_disks.generation();
            if let Some(after_disks) = after.remove(&sled_id) {
                let after_generation = Some(after_disks.generation);
                let a: BTreeSet<DiskIdentity> =
                    after_disks.disks.into_iter().map(|d| d.identity).collect();
                let b = before_disks.disks();
                let added: BTreeSet<_> = a.difference(&b).cloned().collect();
                let removed: BTreeSet<_> = b.difference(&a).cloned().collect();
                let unchanged: BTreeSet<_> =
                    a.intersection(&b).cloned().collect();
                if !added.is_empty() {
                    diffs.added.insert(
                        sled_id,
                        DiffPhysicalDisksDetails {
                            before_generation,
                            after_generation,
                            disks: added,
                        },
                    );
                }
                if !removed.is_empty() {
                    diffs.removed.insert(
                        sled_id,
                        DiffPhysicalDisksDetails {
                            before_generation,
                            after_generation,
                            disks: removed,
                        },
                    );
                }
                if !unchanged.is_empty() {
                    diffs.unchanged.insert(
                        sled_id,
                        DiffPhysicalDisksDetails {
                            before_generation,
                            after_generation,
                            disks: unchanged,
                        },
                    );
                }
            } else {
                diffs.removed.insert(
                    sled_id,
                    DiffPhysicalDisksDetails {
                        before_generation,
                        after_generation: None,
                        disks: before_disks.disks().into_iter().collect(),
                    },
                );
            }
        }

        // Any sleds remaining in `after` have just been added, since we remove
        // sleds from `after`, that were also in `before`, in the above loop.
        for (sled_id, after_disks) in after {
            let added: BTreeSet<DiskIdentity> =
                after_disks.disks.into_iter().map(|d| d.identity).collect();
            if !added.is_empty() {
                diffs.added.insert(
                    sled_id,
                    DiffPhysicalDisksDetails {
                        before_generation: None,
                        after_generation: Some(after_disks.generation),
                        disks: added,
                    },
                );
            }
        }

        diffs
    }

    /// Return a [`BpSledSubtable`] for the given `sled_id`
    pub fn to_bp_sled_subtable(
        &self,
        sled_id: &SledUuid,
    ) -> Option<BpSledSubtable> {
        let mut generation = BpGeneration::Diff { before: None, after: None };
        let mut rows = vec![];
        if let Some(diff) = self.unchanged.get(sled_id) {
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Unchanged));
        }
        if let Some(diff) = self.removed.get(sled_id) {
            // Generations never vary for the same sled, so this is harmless
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Removed));
        }

        if let Some(diff) = self.added.get(sled_id) {
            // Generations never vary for the same sled, so this is harmless
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Added));
        }

        if rows.is_empty() {
            None
        } else {
            Some(BpSledSubtable::new(
                BpPhysicalDisksSubtableSchema {},
                generation,
                rows,
            ))
        }
    }
}

#[derive(Debug)]
pub struct DiffDatasetsDetails {
    // Datasets that come from inventory don't have generation numbers
    pub before_generation: Option<Generation>,

    // Datasets that are removed don't have "after" generation numbers
    pub after_generation: Option<Generation>,

    // Datasets added, removed, modified, or unmodified
    pub datasets:
        BTreeMap<CollectionDatasetIdentifier, BlueprintDatasetConfigForDiff>,
}

impl BpSledSubtableData for DiffDatasetsDetails {
    fn bp_generation(&self) -> BpGeneration {
        BpGeneration::Diff {
            before: self.before_generation,
            after: self.after_generation,
        }
    }

    fn rows(
        &self,
        state: BpDiffState,
    ) -> impl Iterator<Item = BpSledSubtableRow> {
        self.datasets.values().map(move |dataset| {
            BpSledSubtableRow::from_strings(state, dataset.as_strings())
        })
    }
}

#[derive(Debug)]
pub struct ModifiedDataset {
    pub before: BlueprintDatasetConfigForDiff,
    pub after: BlueprintDatasetConfigForDiff,
}

#[derive(Debug)]
pub struct BpDiffDatasetsModified {
    pub generation_before: Option<Generation>,
    pub generation_after: Option<Generation>,
    pub datasets: Vec<ModifiedDataset>,
}

impl BpSledSubtableData for BpDiffDatasetsModified {
    fn bp_generation(&self) -> BpGeneration {
        BpGeneration::Diff {
            before: self.generation_before,
            after: self.generation_after,
        }
    }

    fn rows(
        &self,
        state: BpDiffState,
    ) -> impl Iterator<Item = BpSledSubtableRow> {
        self.datasets.iter().map(move |dataset| {
            let before_strings = dataset.before.as_strings();
            let after_strings = dataset.after.as_strings();

            let mut columns = vec![];
            for (before, after) in std::iter::zip(before_strings, after_strings)
            {
                let column = if before != after {
                    BpSledSubtableColumn::diff(before, after)
                } else {
                    BpSledSubtableColumn::value(before)
                };
                columns.push(column);
            }

            BpSledSubtableRow::new(state, columns)
        })
    }
}

#[derive(Debug, Default)]
pub struct BpDiffDatasets {
    pub added: BTreeMap<SledUuid, DiffDatasetsDetails>,
    pub removed: BTreeMap<SledUuid, DiffDatasetsDetails>,
    pub modified: BTreeMap<SledUuid, BpDiffDatasetsModified>,
    pub unchanged: BTreeMap<SledUuid, DiffDatasetsDetails>,
}

impl BpDiffDatasets {
    pub fn new(
        before: BTreeMap<SledUuid, BlueprintOrCollectionDatasetsConfig>,
        mut after: BTreeMap<SledUuid, BlueprintDatasetsConfig>,
    ) -> Self {
        let mut diffs = BpDiffDatasets::default();

        // Observe the set of old sleds first
        for (sled_id, before_datasets) in before {
            let before_generation = before_datasets.generation();

            // If the sled exists in both the old and new set, compare
            // the set of datasets to identify which "grouping" they should
            // land in.
            if let Some(after_datasets) = after.remove(&sled_id) {
                let after_generation = Some(after_datasets.generation);

                let mut unchanged = BTreeMap::new();
                let mut modified = BTreeMap::new();
                let mut removed = BTreeMap::new();

                // Normalize the "before" and "after" data to compare individual
                // datasets.

                let b = before_datasets.datasets();
                let mut added: BTreeMap<
                    CollectionDatasetIdentifier,
                    BlueprintDatasetConfigForDiff,
                > = after_datasets
                    .datasets
                    .values()
                    .map(|d| {
                        (CollectionDatasetIdentifier::from(d), d.clone().into())
                    })
                    .collect();

                for (id, dataset_before) in b {
                    if let Some(dataset_after) = added.remove(&id) {
                        if dataset_before == dataset_after {
                            unchanged.insert(id, dataset_after);
                        } else {
                            modified
                                .insert(id, (dataset_before, dataset_after));
                        }
                    } else {
                        removed.insert(id, dataset_before);
                    }
                }

                if !added.is_empty() {
                    diffs.added.insert(
                        sled_id,
                        DiffDatasetsDetails {
                            before_generation,
                            after_generation,
                            datasets: added,
                        },
                    );
                }
                if !removed.is_empty() {
                    diffs.removed.insert(
                        sled_id,
                        DiffDatasetsDetails {
                            before_generation,
                            after_generation,
                            datasets: removed,
                        },
                    );
                }
                if !modified.is_empty() {
                    diffs.modified.insert(
                        sled_id,
                        BpDiffDatasetsModified {
                            generation_before: before_generation,
                            generation_after: after_generation,
                            datasets: modified
                                .into_values()
                                .map(|(before, after)| ModifiedDataset {
                                    before,
                                    after,
                                })
                                .collect(),
                        },
                    );
                }
                if !unchanged.is_empty() {
                    diffs.unchanged.insert(
                        sled_id,
                        DiffDatasetsDetails {
                            before_generation,
                            after_generation,
                            datasets: unchanged,
                        },
                    );
                }
            } else {
                diffs.removed.insert(
                    sled_id,
                    DiffDatasetsDetails {
                        before_generation,
                        after_generation: None,
                        datasets: before_datasets
                            .datasets()
                            .into_iter()
                            .collect(),
                    },
                );
            }
        }

        // Any sleds remaining in `after` have just been added, since we remove
        // sleds from `after`, that were also in `before`, in the above loop.
        for (sled_id, after_datasets) in after {
            let added: BTreeMap<CollectionDatasetIdentifier, _> =
                after_datasets
                    .datasets
                    .into_values()
                    .map(|d| (CollectionDatasetIdentifier::from(&d), d.into()))
                    .collect();
            if !added.is_empty() {
                diffs.added.insert(
                    sled_id,
                    DiffDatasetsDetails {
                        before_generation: None,
                        after_generation: Some(after_datasets.generation),
                        datasets: added,
                    },
                );
            }
        }

        diffs
    }

    /// Return a [`BpSledSubtable`] for the given `sled_id`
    pub fn to_bp_sled_subtable(
        &self,
        sled_id: &SledUuid,
    ) -> Option<BpSledSubtable> {
        let mut generation = BpGeneration::Diff { before: None, after: None };
        let mut rows = vec![];
        if let Some(diff) = self.unchanged.get(sled_id) {
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Unchanged));
        }
        if let Some(diff) = self.removed.get(sled_id) {
            // Generations never vary for the same sled, so this is harmless
            //
            // (Same below, where we overwrite the "generation")
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Removed));
        }
        if let Some(diff) = self.modified.get(sled_id) {
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Modified));
        }
        if let Some(diff) = self.added.get(sled_id) {
            generation = diff.bp_generation();
            rows.extend(diff.rows(BpDiffState::Added));
        }

        if rows.is_empty() {
            None
        } else {
            Some(BpSledSubtable::new(
                BpDatasetsSubtableSchema {},
                generation,
                rows,
            ))
        }
    }
}

/// Summarizes the differences between two blueprints
#[derive(Debug)]
pub struct BlueprintDiff {
    pub before_meta: DiffBeforeMetadata,
    pub after_meta: BlueprintMetadata,
    pub before_state: BTreeMap<SledUuid, SledState>,
    pub after_state: BTreeMap<SledUuid, SledState>,
    pub zones: BpDiffZones,
    pub physical_disks: BpDiffPhysicalDisks,
    pub datasets: BpDiffDatasets,
    pub sleds_added: BTreeSet<SledUuid>,
    pub sleds_removed: BTreeSet<SledUuid>,
    pub sleds_unchanged: BTreeSet<SledUuid>,
    pub sleds_modified: BTreeSet<SledUuid>,
}

impl BlueprintDiff {
    /// Build a diff with the provided contents, verifying that the provided
    /// data is valid.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        before_meta: DiffBeforeMetadata,
        before_state: BTreeMap<SledUuid, SledState>,
        before_zones: BTreeMap<SledUuid, BlueprintOrCollectionZonesConfig>,
        before_disks: BTreeMap<SledUuid, BlueprintOrCollectionDisksConfig>,
        before_datasets: BTreeMap<
            SledUuid,
            BlueprintOrCollectionDatasetsConfig,
        >,
        after_meta: BlueprintMetadata,
        mut after_state: BTreeMap<SledUuid, SledState>,
        after_zones: BTreeMap<SledUuid, BlueprintZonesConfig>,
        after_disks: BTreeMap<SledUuid, BlueprintPhysicalDisksConfig>,
        after_datasets: BTreeMap<SledUuid, BlueprintDatasetsConfig>,
    ) -> Self {
        // Work around a quirk of sled decommissioning. If a sled has a before
        // state of `decommissioned`, it may or may not be present in
        // `after_state` (presence will depend on whether or not the sled was
        // present in the `PlanningInput`). However, we may still have entries
        // in `after_zones` or `after_disks` due to expunged zones/disks that
        // haven't been fully cleaned up yet. Without this workaround, this may
        // produce confusing results: the sled might appear to be modified only
        // because the state went from `decommissioned` to "missing entirely".
        // We'll patch this up here: if we have a decommissioned sled that has
        // no `after_state` entry but _does_ still have a corresponding zones or
        // disks entry, we'll artificially insert `decommissioned` to avoid
        // misleading output.
        for (sled_id, _) in before_state
            .iter()
            .filter(|&(_, &state)| state == SledState::Decommissioned)
        {
            if !after_state.contains_key(sled_id)
                && (after_zones.contains_key(sled_id)
                    || after_disks.contains_key(sled_id))
            {
                after_state.insert(*sled_id, SledState::Decommissioned);
            }
        }

        let before_sleds: BTreeSet<_> = before_state
            .keys()
            .chain(before_zones.keys())
            .chain(before_disks.keys())
            .chain(before_datasets.keys())
            .collect();
        let after_sleds: BTreeSet<_> = after_state
            .keys()
            .chain(after_zones.keys())
            .chain(after_disks.keys())
            .chain(after_datasets.keys())
            .collect();
        let all_sleds: BTreeSet<_> =
            before_sleds.union(&after_sleds).map(|&sled_id| *sled_id).collect();

        // All sleds that have state, zones, disks or datasets in `after_*`, but not
        // `before_*` have been added.
        let sleds_added: BTreeSet<_> = after_sleds
            .difference(&before_sleds)
            .map(|&sled_id| *sled_id)
            .collect();

        // All sleds that have state, zones, disks or datasets in `before_*`, but not
        // `after_*` have been removed.
        let sleds_removed: BTreeSet<_> = before_sleds
            .difference(&after_sleds)
            .map(|&sled_id| *sled_id)
            .collect();

        let zones = BpDiffZones::new(before_zones, after_zones);
        let physical_disks =
            BpDiffPhysicalDisks::new(before_disks, after_disks);
        let datasets = BpDiffDatasets::new(before_datasets, after_datasets);

        // Sleds that haven't been added or removed are either unchanged or
        // modified.
        let sleds_unchanged_or_modified: BTreeSet<_> = all_sleds
            .iter()
            .filter(|&sled_id| {
                !sleds_added.contains(sled_id)
                    && !sleds_removed.contains(sled_id)
            })
            .map(|s| *s)
            .collect();

        // Sleds are modified if their state changed or any zones or disks on
        // those sleds are anything other than unchanged.
        let mut sleds_modified = sleds_unchanged_or_modified.clone();
        sleds_modified.retain(|sled_id| {
            before_state.get(sled_id) != after_state.get(sled_id)
                || physical_disks.added.contains_key(sled_id)
                || physical_disks.removed.contains_key(sled_id)
                || datasets.added.contains_key(sled_id)
                || datasets.modified.contains_key(sled_id)
                || datasets.removed.contains_key(sled_id)
                || zones.added.contains_key(sled_id)
                || zones.removed.contains_key(sled_id)
                || zones.modified.contains_key(sled_id)
                || zones.errors.contains_key(sled_id)
        });

        // The rest of the sleds must be unchanged.
        let unchanged_sleds: BTreeSet<_> = sleds_unchanged_or_modified
            .difference(&sleds_modified)
            .map(|sled_id| *sled_id)
            .collect();

        BlueprintDiff {
            before_meta,
            after_meta,
            before_state,
            after_state,
            zones,
            physical_disks,
            datasets,
            sleds_added,
            sleds_removed,
            sleds_unchanged: unchanged_sleds,
            sleds_modified,
        }
    }

    /// Return a struct that can be used to display the diff.
    pub fn display(&self) -> BlueprintDiffDisplay<'_> {
        BlueprintDiffDisplay::new(self)
    }
}

/// Wrapper to allow a [`BlueprintDiff`] to be displayed.
///
/// Returned by [`BlueprintDiff::display()`].
#[derive(Clone, Debug)]
#[must_use = "this struct does nothing unless displayed"]
pub struct BlueprintDiffDisplay<'diff> {
    pub diff: &'diff BlueprintDiff,
    // TODO: add colorization with a stylesheet
}

impl<'diff> BlueprintDiffDisplay<'diff> {
    #[inline]
    fn new(diff: &'diff BlueprintDiff) -> Self {
        Self { diff }
    }

    pub fn make_metadata_diff_tables(
        &self,
    ) -> impl IntoIterator<Item = KvListWithHeading> {
        macro_rules! diff_row {
            ($member:ident, $label:expr) => {
                diff_row!($member, $label, std::convert::identity)
            };

            ($member:ident, $label:expr, $display:expr) => {
                match &self.diff.before_meta {
                    DiffBeforeMetadata::Collection { .. } => {
                        // Collections have no metadata, so this is new
                        KvPair::new(
                            BpDiffState::Added,
                            $label,
                            linear_table_modified(
                                &NOT_PRESENT_IN_COLLECTION_PARENS,
                                &$display(&self.diff.after_meta.$member),
                            ),
                        )
                    }
                    DiffBeforeMetadata::Blueprint(before) => {
                        if before.$member == self.diff.after_meta.$member {
                            KvPair::new(
                                BpDiffState::Unchanged,
                                $label,
                                linear_table_unchanged(&$display(
                                    &self.diff.after_meta.$member,
                                )),
                            )
                        } else {
                            KvPair::new(
                                BpDiffState::Modified,
                                $label,
                                linear_table_modified(
                                    &$display(&before.$member),
                                    &$display(&self.diff.after_meta.$member),
                                ),
                            )
                        }
                    }
                }
            };
        }

        [
            KvListWithHeading::new(
                COCKROACHDB_HEADING,
                vec![
                    diff_row!(
                        cockroachdb_fingerprint,
                        COCKROACHDB_FINGERPRINT,
                        display_none_if_empty
                    ),
                    diff_row!(
                        cockroachdb_setting_preserve_downgrade,
                        COCKROACHDB_PRESERVE_DOWNGRADE,
                        display_optional_preserve_downgrade
                    ),
                ],
            ),
            KvListWithHeading::new(
                METADATA_HEADING,
                vec![
                    diff_row!(internal_dns_version, INTERNAL_DNS_VERSION),
                    diff_row!(external_dns_version, EXTERNAL_DNS_VERSION),
                ],
            ),
        ]
    }

    /// Write out physical disk and zone tables for a given `sled_id`
    fn write_tables(
        &self,
        f: &mut fmt::Formatter<'_>,
        sled_id: &SledUuid,
    ) -> fmt::Result {
        // Write the physical disks table if it exists
        if let Some(table) =
            self.diff.physical_disks.to_bp_sled_subtable(sled_id)
        {
            writeln!(f, "{table}\n")?;
        }

        // Write the datasets table if it exists
        if let Some(table) = self.diff.datasets.to_bp_sled_subtable(sled_id) {
            writeln!(f, "{table}\n")?;
        }

        // Write the zones table if it exists
        if let Some(table) = self.diff.zones.to_bp_sled_subtable(sled_id) {
            writeln!(f, "{table}\n")?;
        }

        Ok(())
    }

    /// Helper methods to stringify sled states. These are separated by
    /// diff section because each section has different expectations for what
    /// before and after should be that can only be wrong if we have a bug
    /// constructing the diff.
    fn sled_state_unchanged(&self, sled_id: &SledUuid) -> String {
        let before = self.diff.before_state.get(sled_id);
        let after = self.diff.after_state.get(sled_id);
        if before == after {
            after
                .map(|s| s.to_string())
                .unwrap_or_else(|| "state unknown".to_string())
        } else {
            format!(
                "blueprint diff error: unchanged sled changed state from \
                 {before:?} to {after:?}"
            )
        }
    }
    fn sled_state_added(&self, sled_id: &SledUuid) -> String {
        let before = self.diff.before_state.get(sled_id);
        let after = self.diff.after_state.get(sled_id);
        if before.is_none() {
            after
                .map(|s| format!("{s}"))
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            format!(
                "blueprint diff error: added sled has old state \
                 {before:?} (now {after:?})"
            )
        }
    }
    fn sled_state_removed(&self, sled_id: &SledUuid) -> String {
        let before = self.diff.before_state.get(sled_id);
        let after = self.diff.after_state.get(sled_id);
        if after.is_none() {
            before
                .map(|s| format!("was {s}"))
                .unwrap_or_else(|| "state was unknown".to_string())
        } else {
            format!(
                "blueprint diff error: removed sled has new state \
                 {after:?} (was {before:?})"
            )
        }
    }
    fn sled_state_modified(&self, sled_id: &SledUuid) -> String {
        let before = self.diff.before_state.get(sled_id);
        let after = self.diff.after_state.get(sled_id);
        match (before, after) {
            (Some(before), Some(after)) if before != after => {
                format!("{before} -> {after}")
            }
            (Some(state), Some(_)) => {
                // states are equal; the sled was presumably modified some other
                // way
                format!("{state}")
            }
            (None, Some(after)) => format!("unknown -> {after}"),
            (Some(before), None) => format!("{before} -> unknown"),
            (None, None) => "unknown".to_string(),
        }
    }
}

impl<'diff> fmt::Display for BlueprintDiffDisplay<'diff> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let diff = self.diff;

        // Print things differently based on whether the diff is between a
        // collection and a blueprint, or a blueprint and a blueprint.
        match &diff.before_meta {
            DiffBeforeMetadata::Collection { id } => {
                writeln!(
                    f,
                    "from: collection {}\n\
                     to:   blueprint  {}",
                    id, diff.after_meta.id,
                )?;
            }
            DiffBeforeMetadata::Blueprint(before) => {
                writeln!(
                    f,
                    "from: blueprint {}\n\
                     to:   blueprint {}\n",
                    before.id, diff.after_meta.id
                )?;
            }
        }

        // Write out sled information
        //
        // The order is:
        //
        // 1. Unchanged
        // 2. Removed
        // 3. Modified
        // 4. Added
        // 5. Errors
        //
        // The idea behind the order is to (a) group all changes together
        // and (b) put changes towards the bottom, so people have to scroll
        // back less.
        //
        // We put errors at the bottom to ensure they are seen immediately.

        // Write out tables for unchanged sleds
        if !diff.sleds_unchanged.is_empty() {
            writeln!(f, " UNCHANGED SLEDS:\n")?;
            for sled_id in &diff.sleds_unchanged {
                writeln!(
                    f,
                    "  sled {sled_id} ({}):\n",
                    self.sled_state_unchanged(sled_id)
                )?;
                self.write_tables(f, sled_id)?;
            }
        }

        // Write out tables for removed sleds
        if !diff.sleds_removed.is_empty() {
            writeln!(f, " REMOVED SLEDS:\n")?;
            for sled_id in &diff.sleds_removed {
                writeln!(
                    f,
                    "  sled {sled_id} ({}):\n",
                    self.sled_state_removed(sled_id)
                )?;
                self.write_tables(f, sled_id)?;
            }
        }

        // Write out tables for modified sleds
        if !diff.sleds_modified.is_empty() {
            writeln!(f, " MODIFIED SLEDS:\n")?;
            for sled_id in &diff.sleds_modified {
                writeln!(
                    f,
                    "  sled {sled_id} ({}):\n",
                    self.sled_state_modified(sled_id)
                )?;
                self.write_tables(f, sled_id)?;
            }
        }

        // Write out tables for added sleds
        if !diff.sleds_added.is_empty() {
            writeln!(f, " ADDED SLEDS:\n")?;
            for sled_id in &diff.sleds_added {
                writeln!(
                    f,
                    "  sled {sled_id} ({}):\n",
                    self.sled_state_added(sled_id)
                )?;
                self.write_tables(f, sled_id)?;
            }
        }

        // Write out zone errors.
        if !diff.zones.errors.is_empty() {
            writeln!(f, "ERRORS:")?;
            for (sled_id, errors) in &diff.zones.errors {
                writeln!(f, "\n  sled {sled_id}\n")?;
                writeln!(
                    f,
                    "    zone diff errors: before gen {}, after gen {}\n",
                    errors.generation_before, errors.generation_after
                )?;

                for err in &errors.errors {
                    writeln!(f, "      zone id: {}", err.zone_before.id())?;
                    writeln!(f, "      reason: {}", err.reason)?;
                }
            }
        }

        // Write out metadata diff table
        for table in self.make_metadata_diff_tables() {
            writeln!(f, "{}", table)?;
        }

        Ok(())
    }
}

fn display_none_if_empty(value: &str) -> &str {
    if value.is_empty() {
        NONE_PARENS
    } else {
        value
    }
}

fn display_optional_preserve_downgrade(
    value: &Option<CockroachDbPreserveDowngrade>,
) -> String {
    match value {
        Some(v) => v.to_string(),
        None => INVALID_VALUE_PARENS.to_string(),
    }
}
