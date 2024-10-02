// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::{
    ByteCount, Disk, ExternalIp, Generation, InstanceAutoRestartPolicy,
    InstanceCpuCount, InstanceState, Vmm, VmmState,
};
use crate::collection::DatastoreAttachTargetConfig;
use crate::schema::{disk, external_ip, instance};
use chrono::{DateTime, TimeDelta, Utc};
use db_macros::Resource;
use diesel::expression::{is_aggregate, ValidGrouping};
use diesel::pg;
use diesel::prelude::*;
use diesel::sql_types::{Bool, Nullable};
use nexus_types::external_api::params;
use omicron_uuid_kinds::{GenericUuid, InstanceUuid};
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

/// An Instance (VM).
#[derive(
    Clone,
    Debug,
    Queryable,
    Insertable,
    Selectable,
    Resource,
    Serialize,
    Deserialize,
)]
#[diesel(table_name = instance)]
pub struct Instance {
    #[diesel(embed)]
    identity: InstanceIdentity,

    /// id for the project containing this Instance
    pub project_id: Uuid,

    /// user data for instance initialization systems (e.g. cloud-init)
    pub user_data: Vec<u8>,

    /// The number of vCPUs (i.e., virtual logical processors) to allocate for
    /// this instance.
    #[diesel(column_name = ncpus)]
    pub ncpus: InstanceCpuCount,

    /// The amount of guest memory to allocate for this instance.
    #[diesel(column_name = memory)]
    pub memory: ByteCount,

    /// The instance's hostname.
    // TODO-cleanup: We use a validated wrapper type in the API, but not in
    // between the database. This is to handle existing names that do not pass
    // the new validation. We should swap this for a SQL-serializable validated
    // type.
    #[diesel(column_name = hostname)]
    pub hostname: String,

    #[diesel(embed)]
    pub auto_restart: InstanceAutoRestart,

    /// The primary boot disk for this instance.
    #[diesel(column_name = boot_disk_id)]
    pub boot_disk_id: Option<Uuid>,

    #[diesel(embed)]
    pub runtime_state: InstanceRuntimeState,

    /// A UUID identifying the saga currently holding the update lock on this
    /// instance. If this is [`None`] the instance is not locked. Otherwise, if
    /// this is [`Some`], the instance is locked by the saga owning this UUID.
    /// Note that this is not (presently) the UUID *of* the locking saga, but
    /// rather, a UUID *generated by* that saga. Therefore, it may not be
    /// useable to look up which saga holds the lock.
    ///
    /// This field is guarded by the instance's `updater_gen`
    #[diesel(column_name = updater_id)]
    pub updater_id: Option<Uuid>,

    /// The generation number for the updater lock. This is updated whenever the
    /// lock is acquired or released, and is used in attempts to set the
    /// `updater_id` field to ensure that the snapshot which indicated that the
    /// lock was not held is still valid when setting the lock ID.
    #[diesel(column_name = updater_gen)]
    pub updater_gen: Generation,
}

impl Instance {
    /// Constructs a new instance record with no VMM that will initially appear
    /// to be in the Creating state.
    pub fn new(
        instance_id: InstanceUuid,
        project_id: Uuid,
        params: &params::InstanceCreate,
    ) -> Self {
        let identity = InstanceIdentity::new(
            instance_id.into_untyped_uuid(),
            params.identity.clone(),
        );

        let runtime_state = InstanceRuntimeState::new(
            InstanceState::Creating,
            identity.time_modified,
        );

        let auto_restart = InstanceAutoRestart {
            policy: params.auto_restart_policy.map(Into::into),
            cooldown: None,
        };

        Self {
            identity,
            project_id,
            user_data: params.user_data.clone(),
            ncpus: params.ncpus.into(),
            memory: params.memory.into(),
            hostname: params.hostname.to_string(),
            auto_restart,
            // Intentionally ignore `params.boot_disk_id` here: we can't set
            // `boot_disk_id` until the referenced disk is attached.
            boot_disk_id: None,

            runtime_state,

            updater_gen: Generation::new(),
            updater_id: None,
        }
    }

    pub fn runtime(&self) -> &InstanceRuntimeState {
        &self.runtime_state
    }
}

impl DatastoreAttachTargetConfig<Disk> for Instance {
    type Id = Uuid;

    type CollectionIdColumn = instance::dsl::id;
    type CollectionTimeDeletedColumn = instance::dsl::time_deleted;

    type ResourceIdColumn = disk::dsl::id;
    type ResourceCollectionIdColumn = disk::dsl::attach_instance_id;
    type ResourceTimeDeletedColumn = disk::dsl::time_deleted;
}

impl DatastoreAttachTargetConfig<ExternalIp> for Instance {
    // TODO-cleanup ideally this would be an ExternalIpUuid, haven't quite
    // figured out how to make that work
    type Id = Uuid;

    type CollectionIdColumn = instance::dsl::id;
    type CollectionTimeDeletedColumn = instance::dsl::time_deleted;

    type ResourceIdColumn = external_ip::dsl::id;
    type ResourceCollectionIdColumn = external_ip::dsl::parent_id;
    type ResourceTimeDeletedColumn = external_ip::dsl::time_deleted;
}

/// Runtime state of the Instance, including the actual running state and minimal
/// metadata
///
/// This state is owned by the sled agent running that Instance.
#[derive(
    Clone,
    Debug,
    AsChangeset,
    Selectable,
    Insertable,
    Queryable,
    Serialize,
    Deserialize,
)]
// N.B. Setting `treat_none_as_null` is required for these fields to be cleared
//      properly during live migrations. See the documentation for
//      `diesel::prelude::AsChangeset`.
#[diesel(table_name = instance, treat_none_as_null = true)]
pub struct InstanceRuntimeState {
    /// The time at which the runtime state was last updated. This is distinct
    /// from the time the record was last modified, because some updates don't
    /// modify the runtime state.
    #[diesel(column_name = time_state_updated)]
    pub time_updated: DateTime<Utc>,

    /// The generation number for the information stored in this structure,
    /// including the fallback state, the instance's active Propolis ID, and its
    /// migration IDs.
    #[diesel(column_name = state_generation)]
    pub gen: Generation,

    /// The ID of the Propolis server hosting the current incarnation of this
    /// instance, or None if the instance has no active VMM.
    ///
    /// This field is guarded by the instance's `gen`.
    #[diesel(column_name = active_propolis_id)]
    pub propolis_id: Option<Uuid>,

    /// If a migration is in progress, the ID of the Propolis server that is
    ///
    /// This field is guarded by the instance's `gen`.
    #[diesel(column_name = target_propolis_id)]
    pub dst_propolis_id: Option<Uuid>,

    /// If a migration is in progress, a UUID identifying that migration. This
    /// can be used to provide mutual exclusion between multiple attempts to
    /// migrate and between an attempt to migrate an attempt to mutate an
    /// instance in a way that's incompatible with migration.
    ///
    /// This field is guarded by the instance's `gen`.
    #[diesel(column_name = migration_id)]
    pub migration_id: Option<Uuid>,

    /// The "internal" state of this instance. The instance's externally-visible
    /// state may be delegated to the instance's active VMM, if it has one.
    ///
    /// This field is guarded by the instance's `gen` field.
    #[diesel(column_name = state)]
    pub nexus_state: InstanceState,

    /// The timestamp of the most recent auto-restart attempt, or `None` if this
    /// instance has never been auto-restarted by the control plane.
    ///
    /// This field is guarded by the instance's `gen`.
    #[diesel(column_name = time_last_auto_restarted)]
    pub time_last_auto_restarted: Option<DateTime<Utc>>,
}

impl InstanceRuntimeState {
    fn new(initial_state: InstanceState, creation_time: DateTime<Utc>) -> Self {
        Self {
            nexus_state: initial_state,
            time_updated: creation_time,
            propolis_id: None,
            dst_propolis_id: None,
            migration_id: None,
            gen: Generation::new(),
            time_last_auto_restarted: None,
        }
    }
}

/// Configuration for automatic instance restarts.
#[derive(
    Clone,
    Debug,
    Default,
    AsChangeset,
    Selectable,
    Insertable,
    Queryable,
    Serialize,
    Deserialize,
)]
#[diesel(table_name = instance)]
pub struct InstanceAutoRestart {
    /// The auto-restart policy for this instance.
    ///
    /// This indicates whether the instance should be automatically restarted by
    /// the control plane on failure. If this is `NULL`, no auto-restart policy
    /// has been configured for this instance by the user.
    #[diesel(column_name = auto_restart_policy)]
    #[serde(default)]
    pub policy: Option<InstanceAutoRestartPolicy>,
    /// The cooldown period that must elapse between automatic restarts of this
    /// instance.
    ///
    /// If this is `NULL`, no explicit cooldown period has been configured for
    /// this instance, and the default cooldown period should be used instead.
    #[diesel(column_name = auto_restart_cooldown)]
    #[serde(default, with = "optional_time_delta")]
    pub cooldown: Option<TimeDelta>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InstanceKarmicStatus {
    /// Whether the instance is permitted to reincarnate if
    /// `needs_reincarnation` is `true`.
    pub can_reincarnate: Reincarnatability,
    /// `true` if the instance is in a state in which it could reincarnate if
    /// `can_reincarnate` would permit it to do so.
    pub needs_reincarnation: bool,
}

impl InstanceKarmicStatus {
    /// Returns `true` if this instance is in a state that requires
    /// reincarnation, and is permitted to reincarnate immediately.
    pub fn should_reincarnate(&self) -> bool {
        self.needs_reincarnation
            && self.can_reincarnate == Reincarnatability::WillReincarnate
    }
}

/// Describes whether or not an instance can reincarnate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Reincarnatability {
    /// The instance remains bound to the cycle of saṃsāra and can return in the
    /// next life.
    WillReincarnate,
    /// The instance cannot reincarnate again until the specified time.
    CoolingDown(TimeDelta),
    /// The instance's auto-restart policy indicates that it has attained
    /// nirvāṇa and will not reincarnate.
    Nirvana,
}

impl InstanceAutoRestart {
    /// The default cooldown used when an instance has no overridden cooldown.
    pub const DEFAULT_COOLDOWN: TimeDelta = match TimeDelta::try_hours(1) {
        Some(delta) => delta,
        None => unreachable!(), // 1 hour should be representable...
    };

    /// The default policy used when an instance does not override the
    /// reincarnation policy.
    pub const DEFAULT_POLICY: InstanceAutoRestartPolicy =
        InstanceAutoRestartPolicy::BestEffort;

    /// Returns an instance's karmic status.
    pub fn status(
        &self,
        state: &InstanceRuntimeState,
        active_vmm: Option<&Vmm>,
    ) -> InstanceKarmicStatus {
        // Instances only need to be automatically restarted if they are in the
        // `Failed` state, or if their active VMM is in the `SagaUnwound` state.
        let needs_reincarnation = match (state.nexus_state, active_vmm) {
            (InstanceState::Failed, _vmm) => {
                debug_assert!(
                    _vmm.is_none(),
                    "a Failed instance will never have an active VMM!"
                );
                true
            }
            (InstanceState::Vmm, Some(ref vmm)) => {
                debug_assert_eq!(
                    state.propolis_id,
                    Some(vmm.id),
                    "don't call `InstanceAutoRestart::status with a VMM \
                     that isn't this instance's active VMM!?!?"
                );
                // Note that we *don't* reincarnate instances with `Failed` active
                // VMMs; in that case, an instance-update saga must first run to
                // move the *instance* record to the `Failed` state.
                vmm.runtime.state == VmmState::SagaUnwound
            }
            _ => false,
        };

        InstanceKarmicStatus {
            needs_reincarnation,
            can_reincarnate: self.can_reincarnate(&state),
        }
    }

    /// Returns whether or not this auto-restart configuration will permit an
    /// instance with the provided `InstanceRuntimeState` to reincarnate.
    ///
    /// This does *not* indicate that the instance  currently needs
    /// reincarnation, but instead, whether the instance will be permitted to
    /// reincarnate should it be in such a state.
    pub fn can_reincarnate(
        &self,
        state: &InstanceRuntimeState,
    ) -> Reincarnatability {
        // Check if the instance's configured auto-restart policy permits the
        // control plane to automatically restart it.
        let policy = self.policy.unwrap_or(Self::DEFAULT_POLICY);
        if policy == InstanceAutoRestartPolicy::Never {
            return Reincarnatability::Nirvana;
        }

        // If the instance is permitted to reincarnate, ensure that its last
        // reincarnation was at least one cooldown period ago.
        if let Some(last) = state.time_last_auto_restarted {
            // If no explicit cooldown is present, use the default.
            // Eventually, we may also allow a project-level default, so we will
            // need to consider that as well.
            let cooldown = self.cooldown.unwrap_or(Self::DEFAULT_COOLDOWN);
            let time_since_last = Utc::now().signed_duration_since(last);
            if time_since_last >= cooldown {
                return Reincarnatability::WillReincarnate;
            } else {
                return Reincarnatability::CoolingDown(
                    cooldown - time_since_last,
                );
            }
        }

        Reincarnatability::WillReincarnate
    }

    /// Filters a database query to include only instances whose auto-restart
    /// configs permit them to reincarnate.
    ///
    /// Yes, this should probably be in `nexus-db-queries`, but it seemed nice
    /// for it to be defined on the same struct as the in-memory logic
    /// (`can_reincarnate`).
    pub fn filter_reincarnatable(
    ) -> impl diesel::query_builder::QueryFragment<pg::Pg>
           + diesel::query_builder::QueryId
           // All elements in this expression appear on the `instance` table, so
           // it's a valid `filter` for that table, and the expression evaluates
           // to a bool (or NULL), making it a valid WHERE clause.
           + AppearsOnTable<instance::table, SqlType = Nullable<Bool>>
           // I think this trait tells diesel that the query fragment has no
           // GROUP BY clause, so that it knows it can be used as a WHERE clause
           + ValidGrouping<(), IsAggregate = is_aggregate::No> {
        use instance::dsl;

        let now = diesel::dsl::now.into_sql::<pg::sql_types::Timestamptz>();

        // The instance's auto-restart policy must allow the control plane
        // to restart it automatically.
        //
        // N.B. that this may become more complex in the future if we grow
        // additional auto-restart policies that require additional logic
        // (such as restart limits...)
        (dsl::auto_restart_policy
            .eq(InstanceAutoRestartPolicy::BestEffort)
            // If the auto-restart policy is null, then it should
            // default to "best effort".
            .or(dsl::auto_restart_policy.is_null()))
        // An instance whose last reincarnation was within the cooldown
        // interval from now must remain in _bardo_ --- the liminal
        // state between death and rebirth --- before its next
        // reincarnation.
        .and(
            // If the instance has never previously been reincarnated, then
            // it's allowed to reincarnate.
            dsl::time_last_auto_restarted
                .is_null()
                // Or, if it has an overridden cooldown period, has that elapsed?
                .or(dsl::auto_restart_cooldown.is_not_null().and(
                    dsl::time_last_auto_restarted
                        .le(now.nullable() - dsl::auto_restart_cooldown),
                ))
                // Or, finally, if it does not have an overridden cooldown
                // period, has the default cooldown period elapsed?
                .or(dsl::auto_restart_cooldown.is_null().and(
                    dsl::time_last_auto_restarted
                        .le((now - Self::DEFAULT_COOLDOWN).nullable()),
                )),
        )
        // Deleted instances may not be reincarnated.
        .and(dsl::time_deleted.is_null())
        // If the instance is currently in the process of being updated,
        // let's not mess with it for now and try to restart it on another
        // pass.
        .and(dsl::updater_id.is_null())
    }
}

/// It's just a type with the same representation as a `TimeDelta` that
/// implements `Serialize` and `Deserialize`, because `chrono`'s `Deserialize`
/// implementation for this type is not actually for `TimeDelta`, but for the
/// `rkyv::Archived` wrapper type (see [here]). While `chrono` *does* provide a
/// `Serialize` implementation that we could use with this type, it's preferable
/// to provide our own `Serialize` as well as `Deserialize`, since a future
/// semver-compatible change in `chrono` could change the struct's internal
/// representation, quietly breaking our ability to round-trip it. So, let's
/// just derive both traits for this thing, which we control.
///
/// If you feel like this is unfortunate...yeah, I do too.
///
/// [here]: https://docs.rs/chrono/latest/chrono/struct.TimeDelta.html#impl-Deserialize%3CTimeDelta,+__D%3E-for-%3CTimeDelta+as+Archive%3E::Archived
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
struct SerdeTimeDelta {
    secs: i64,
    nanos: i32,
}

impl From<TimeDelta> for SerdeTimeDelta {
    fn from(delta: TimeDelta) -> Self {
        Self { secs: delta.num_seconds(), nanos: delta.subsec_nanos() }
    }
}

impl TryFrom<SerdeTimeDelta> for TimeDelta {
    type Error = &'static str;
    fn try_from(
        SerdeTimeDelta { secs, nanos }: SerdeTimeDelta,
    ) -> Result<Self, Self::Error> {
        // This is a bit weird: `chrono::TimeDelta`'s getter for
        // nanoseconds (`TimeDelta::subsec_nanos`) returns them as an i32,
        // with the sign coming from the seconds part, but when constructing
        // a `TimeDelta`, it takes them as a `u32` and panics if they're too
        // big. So, we take the absolute value here, because what the serialize
        // impl saw may have had its sign bit set, but the constructor will get
        // mad if we give it something with that bit set. Hopefully that made
        // sense?
        let nanos = nanos.unsigned_abs();
        TimeDelta::new(secs, nanos).ok_or("time delta out of range")
    }
}
mod optional_time_delta {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub(super) fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<Option<TimeDelta>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = Option::<SerdeTimeDelta>::deserialize(deserializer)?;
        match val {
            None => return Ok(None),
            Some(delta) => delta
                .try_into()
                .map_err(|e| {
                    <D::Error as serde::de::Error>::custom(format!(
                        "{e}: {val:?}"
                    ))
                })
                .map(Some),
        }
    }

    pub(super) fn serialize<S>(
        td: &Option<TimeDelta>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        td.as_ref()
            .map(|&delta| SerdeTimeDelta::from(delta))
            .serialize(serializer)
    }
}

/// The parts of an Instance that can be directly updated after creation.
#[derive(Clone, Debug, AsChangeset, Serialize, Deserialize)]
#[diesel(table_name = instance, treat_none_as_null = true)]
pub struct InstanceUpdate {
    #[diesel(column_name = boot_disk_id)]
    pub boot_disk_id: Option<Uuid>,
}
