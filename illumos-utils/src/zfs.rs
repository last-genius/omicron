// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utilities for poking at ZFS.

use crate::{execute, PFEXEC};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use omicron_common::api::external::ByteCount;
use omicron_common::disk::CompressionAlgorithm;
use omicron_common::disk::DiskIdentity;
use omicron_uuid_kinds::DatasetUuid;
use std::fmt;
use std::str::FromStr;

// These locations in the ramdisk must only be used by the switch zone.
//
// We need the switch zone online before we can create the U.2 drives and
// encrypt the zpools during rack initialization. Without the switch zone we
// cannot get the rack initialization request from wicketd in RSS which allows
// us to  initialize the trust quorum and derive the encryption keys needed for
// the U.2 disks.
pub const ZONE_ZFS_RAMDISK_DATASET_MOUNTPOINT: &str = "/zone";
pub const ZONE_ZFS_RAMDISK_DATASET: &str = "rpool/zone";

pub const ZFS: &str = "/usr/sbin/zfs";

/// This path is intentionally on a `tmpfs` to prevent copy-on-write behavior
/// and to ensure it goes away on power off.
///
/// We want minimize the time the key files are in memory, and so we rederive
/// the keys and recreate the files on demand when creating and mounting
/// encrypted filesystems. We then zero them and unlink them.
pub const KEYPATH_ROOT: &str = "/var/run/oxide/";

/// Error returned by [`Zfs::list_datasets`].
#[derive(thiserror::Error, Debug)]
#[error("Could not list datasets within zpool {name}: {err}")]
pub struct ListDatasetsError {
    name: String,
    #[source]
    err: crate::ExecutionError,
}

#[derive(thiserror::Error, Debug)]
pub enum DestroyDatasetErrorVariant {
    #[error("Dataset not found")]
    NotFound,
    #[error(transparent)]
    Other(crate::ExecutionError),
}

/// Error returned by [`Zfs::destroy_dataset`].
#[derive(thiserror::Error, Debug)]
#[error("Could not destroy dataset {name}: {err}")]
pub struct DestroyDatasetError {
    name: String,
    #[source]
    pub err: DestroyDatasetErrorVariant,
}

#[derive(thiserror::Error, Debug)]
enum EnsureFilesystemErrorRaw {
    #[error("ZFS execution error: {0}")]
    Execution(#[from] crate::ExecutionError),

    #[error("Filesystem does not exist, and formatting was not requested")]
    NotFoundNotFormatted,

    #[error("Unexpected output from ZFS commands: {0}")]
    Output(String),

    #[error("Failed to mount encrypted filesystem: {0}")]
    MountEncryptedFsFailed(crate::ExecutionError),

    #[error("Failed to mount overlay filesystem: {0}")]
    MountOverlayFsFailed(crate::ExecutionError),
}

/// Error returned by [`Zfs::ensure_filesystem`].
#[derive(thiserror::Error, Debug)]
#[error(
    "Failed to ensure filesystem '{name}' exists at '{mountpoint:?}': {err}"
)]
pub struct EnsureFilesystemError {
    name: String,
    mountpoint: Mountpoint,
    #[source]
    err: EnsureFilesystemErrorRaw,
}

/// Error returned by [`Zfs::set_oxide_value`]
#[derive(thiserror::Error, Debug)]
#[error(
    "Failed to set value '{name}={value}' on filesystem {filesystem}: {err}"
)]
pub struct SetValueError {
    filesystem: String,
    name: String,
    value: String,
    err: crate::ExecutionError,
}

#[derive(thiserror::Error, Debug)]
enum GetValueErrorRaw {
    #[error(transparent)]
    Execution(#[from] crate::ExecutionError),

    #[error("No value found with that name")]
    MissingValue,
}

/// Error returned by [`Zfs::get_oxide_value`] or [`Zfs::get_value`].
#[derive(thiserror::Error, Debug)]
#[error("Failed to get value '{name}' from filesystem {filesystem}")]
pub struct GetValueError {
    filesystem: String,
    name: String,
    #[source]
    err: GetValueErrorRaw,
}

#[derive(Debug, thiserror::Error)]
#[error("Failed to list snapshots: {0}")]
pub struct ListSnapshotsError(#[from] crate::ExecutionError);

#[derive(Debug, thiserror::Error)]
#[error("Failed to create snapshot '{snap_name}' from filesystem '{filesystem}': {err}")]
pub struct CreateSnapshotError {
    filesystem: String,
    snap_name: String,
    err: crate::ExecutionError,
}

#[derive(Debug, thiserror::Error)]
#[error("Failed to delete snapshot '{filesystem}@{snap_name}': {err}")]
pub struct DestroySnapshotError {
    filesystem: String,
    snap_name: String,
    err: crate::ExecutionError,
}

/// Wraps commands for interacting with ZFS.
pub struct Zfs {}

/// Describes a mountpoint for a ZFS filesystem.
#[derive(Debug, Clone)]
pub enum Mountpoint {
    #[allow(dead_code)]
    Legacy,
    Path(Utf8PathBuf),
}

impl fmt::Display for Mountpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mountpoint::Legacy => write!(f, "legacy"),
            Mountpoint::Path(p) => write!(f, "{p}"),
        }
    }
}

/// This is the path for an encryption key used by ZFS
#[derive(Debug, Clone)]
pub struct Keypath(pub Utf8PathBuf);

impl fmt::Display for Keypath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Keypath {
    /// Constructs a Keypath for the specified disk within the supplied root
    /// directory.
    ///
    /// By supplying "root", tests can override the location where these paths
    /// are stored to non-global locations.
    pub fn new<P: AsRef<Utf8Path>>(id: &DiskIdentity, root: &P) -> Keypath {
        let keypath_root = Utf8PathBuf::from(KEYPATH_ROOT);
        let mut keypath = keypath_root.as_path();
        let keypath_directory = loop {
            match keypath.strip_prefix("/") {
                Ok(stripped) => keypath = stripped,
                Err(_) => break root.as_ref().join(keypath),
            }
        };
        std::fs::create_dir_all(&keypath_directory)
            .expect("Cannot ensure directory for keys");

        let filename = format!(
            "{}-{}-{}-zfs-aes-256-gcm.key",
            id.vendor, id.serial, id.model
        );
        let path: Utf8PathBuf =
            [keypath_directory.as_str(), &filename].iter().collect();
        Keypath(path)
    }
}

#[derive(Debug)]
pub struct EncryptionDetails {
    pub keypath: Keypath,
    pub epoch: u64,
}

#[derive(Debug, Default)]
pub struct SizeDetails {
    pub quota: Option<ByteCount>,
    pub reservation: Option<ByteCount>,
    pub compression: CompressionAlgorithm,
}

#[derive(Debug)]
pub struct DatasetProperties {
    /// The Uuid of the dataset
    pub id: Option<DatasetUuid>,
    /// The full name of the dataset.
    pub name: String,
    /// Remaining space in the dataset and descendants.
    pub avail: ByteCount,
    /// Space used by dataset and descendants.
    pub used: ByteCount,
    /// Maximum space usable by dataset and descendants.
    pub quota: Option<ByteCount>,
    /// Minimum space guaranteed to dataset and descendants.
    pub reservation: Option<ByteCount>,
    /// The compression algorithm used for this dataset.
    ///
    /// This probably aligns with a value from
    /// [omicron_common::disk::CompressionAlgorithm], but is left as an untyped
    /// string so that unexpected compression formats don't prevent inventory
    /// from being collected.
    pub compression: String,
}

impl DatasetProperties {
    // care about.
    const ZFS_LIST_STR: &'static str =
        "oxide:uuid,name,avail,used,quota,reservation,compression";
}

// An inner parsing function, so that the FromStr implementation can always emit
// the string 's' that failed to parse in the error message.
fn dataset_properties_parse(
    s: &str,
) -> Result<DatasetProperties, anyhow::Error> {
    let mut iter = s.split_whitespace();

    let id = match iter.next().context("Missing UUID")? {
        "-" => None,
        anything_else => Some(anything_else.parse::<DatasetUuid>()?),
    };

    let name = iter.next().context("Missing 'name'")?.to_string();
    let avail =
        iter.next().context("Missing 'avail'")?.parse::<u64>()?.try_into()?;
    let used =
        iter.next().context("Missing 'used'")?.parse::<u64>()?.try_into()?;
    let quota = match iter.next().context("Missing 'quota'")?.parse::<u64>()? {
        0 => None,
        q => Some(q.try_into()?),
    };
    let reservation =
        match iter.next().context("Missing 'reservation'")?.parse::<u64>()? {
            0 => None,
            r => Some(r.try_into()?),
        };
    let compression = iter.next().context("Missing 'compression'")?.to_string();

    Ok(DatasetProperties {
        id,
        name,
        avail,
        used,
        quota,
        reservation,
        compression,
    })
}

impl FromStr for DatasetProperties {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        dataset_properties_parse(s)
            .with_context(|| format!("Failed to parse: {s}"))
    }
}

#[derive(Debug, Copy, Clone)]
pub enum PropertySource {
    Local,
    Default,
    Inherited,
    Temporary,
    None,
}

impl fmt::Display for PropertySource {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ps = match self {
            PropertySource::Local => "local",
            PropertySource::Default => "default",
            PropertySource::Inherited => "inherited",
            PropertySource::Temporary => "temporary",
            PropertySource::None => "none",
        };
        write!(f, "{ps}")
    }
}

#[cfg_attr(any(test, feature = "testing"), mockall::automock, allow(dead_code))]
impl Zfs {
    /// Lists all datasets within a pool or existing dataset.
    ///
    /// Strips the input `name` from the output dataset names.
    pub fn list_datasets(name: &str) -> Result<Vec<String>, ListDatasetsError> {
        let mut command = std::process::Command::new(ZFS);
        let cmd = command.args(&["list", "-d", "1", "-rHpo", "name", name]);

        let output = execute(cmd)
            .map_err(|err| ListDatasetsError { name: name.to_string(), err })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let filesystems: Vec<String> = stdout
            .trim()
            .split('\n')
            .filter(|n| *n != name)
            .map(|s| {
                String::from(s.strip_prefix(&format!("{}/", name)).unwrap())
            })
            .collect();
        Ok(filesystems)
    }

    /// Get information about datasets within a list of zpools / datasets.
    ///
    /// This function is similar to [Zfs::list_datasets], but provides a more
    /// substantial results about the datasets found.
    ///
    /// Sorts results and de-duplicates them by name.
    pub fn get_dataset_properties(
        datasets: &[String],
    ) -> Result<Vec<DatasetProperties>, anyhow::Error> {
        let mut command = std::process::Command::new(ZFS);
        let cmd = command.args(&["list", "-d", "1", "-rHpo"]);

        // Note: this is tightly coupled with the layout of DatasetProperties
        cmd.arg(DatasetProperties::ZFS_LIST_STR);
        cmd.args(datasets);

        let output = execute(cmd).with_context(|| {
            format!("Failed to get dataset properties for {datasets:?}")
        })?;
        let stdout = String::from_utf8(output.stdout)?;
        let mut datasets = stdout
            .trim()
            .split('\n')
            .map(|row| row.parse::<DatasetProperties>())
            .collect::<Result<Vec<_>, _>>()?;

        datasets.sort_by(|d1, d2| d1.name.partial_cmp(&d2.name).unwrap());
        datasets.dedup_by(|d1, d2| d1.name.eq(&d2.name));

        Ok(datasets)
    }

    /// Return the name of a dataset for a ZFS object.
    ///
    /// The object can either be a dataset name, or a path, in which case it
    /// will be resolved to the _mounted_ ZFS dataset containing that path.
    pub fn get_dataset_name(object: &str) -> Result<String, ListDatasetsError> {
        let mut command = std::process::Command::new(ZFS);
        let cmd = command.args(&["get", "-Hpo", "value", "name", object]);
        execute(cmd)
            .map(|output| {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            })
            .map_err(|err| ListDatasetsError { name: object.to_string(), err })
    }

    /// Destroys a dataset.
    pub fn destroy_dataset(name: &str) -> Result<(), DestroyDatasetError> {
        let mut command = std::process::Command::new(PFEXEC);
        let cmd = command.args(&[ZFS, "destroy", "-r", name]);
        execute(cmd).map_err(|err| {
            let variant = match err {
                crate::ExecutionError::CommandFailure(info)
                    if info.stderr.contains("does not exist") =>
                {
                    DestroyDatasetErrorVariant::NotFound
                }
                _ => DestroyDatasetErrorVariant::Other(err),
            };
            DestroyDatasetError { name: name.to_string(), err: variant }
        })?;
        Ok(())
    }

    /// Creates a new ZFS filesystem unless one already exists.
    ///
    /// - `name`: the full path to the zfs dataset
    /// - `mountpoint`: The expected mountpoint of this filesystem.
    /// If the filesystem already exists, and is not mounted here, and error is
    /// returned.
    /// - `zoned`: identifies whether or not this filesystem should be
    /// used in a zone. Only used when creating a new filesystem - ignored
    /// if the filesystem already exists.
    /// - `do_format`: if "false", prevents a new filesystem from being created,
    /// and returns an error if it is not found.
    /// - `encryption_details`: Ensures a filesystem as an encryption root.
    /// For new filesystems, this supplies the key, and all datasets within this
    /// root are implicitly encrypted. For existing filesystems, ensures that
    /// they are mounted (and that keys are loaded), but does not verify the
    /// input details.
    /// - `size_details`: If supplied, sets size-related information. These
    /// values are set on both new filesystem creation as well as when loading
    /// existing filesystems.
    /// - `additional_options`: Additional ZFS options, which are only set when
    /// creating new filesystems.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure_filesystem(
        name: &str,
        mountpoint: Mountpoint,
        zoned: bool,
        do_format: bool,
        encryption_details: Option<EncryptionDetails>,
        size_details: Option<SizeDetails>,
        additional_options: Option<Vec<String>>,
    ) -> Result<(), EnsureFilesystemError> {
        let (exists, mounted) = Self::dataset_exists(name, &mountpoint)?;
        if exists {
            if let Some(SizeDetails { quota, reservation, compression }) =
                size_details
            {
                // apply quota and compression mode (in case they've changed across
                // sled-agent versions since creation)
                Self::apply_properties(
                    name,
                    &mountpoint,
                    quota,
                    reservation,
                    compression,
                )?;
            }

            if encryption_details.is_none() {
                // If the dataset exists, we're done. Unencrypted datasets are
                // automatically mounted.
                return Ok(());
            } else {
                if mounted {
                    // The dataset exists and is mounted
                    return Ok(());
                }
                // We need to load the encryption key and mount the filesystem
                return Self::mount_encrypted_dataset(name, &mountpoint);
            }
        }

        if !do_format {
            return Err(EnsureFilesystemError {
                name: name.to_string(),
                mountpoint,
                err: EnsureFilesystemErrorRaw::NotFoundNotFormatted,
            });
        }

        // If it doesn't exist, make it.
        let mut command = std::process::Command::new(PFEXEC);
        let cmd = command.args(&[ZFS, "create"]);
        if zoned {
            cmd.args(&["-o", "zoned=on"]);
        }
        if let Some(details) = encryption_details {
            let keyloc = format!("keylocation=file://{}", details.keypath);
            let epoch = format!("oxide:epoch={}", details.epoch);
            cmd.args(&[
                "-o",
                "encryption=aes-256-gcm",
                "-o",
                "keyformat=raw",
                "-o",
                &keyloc,
                "-o",
                &epoch,
            ]);
        }

        if let Some(opts) = additional_options {
            for o in &opts {
                cmd.args(&["-o", &o]);
            }
        }

        cmd.args(&["-o", &format!("mountpoint={}", mountpoint), name]);

        execute(cmd).map_err(|err| EnsureFilesystemError {
            name: name.to_string(),
            mountpoint: mountpoint.clone(),
            err: err.into(),
        })?;

        // We ensure that the currently running process has the ability to
        // act on the underlying mountpoint.
        if !zoned {
            let mut command = std::process::Command::new(PFEXEC);
            let user = whoami::username();
            let mount = format!("{mountpoint}");
            let cmd = command.args(["chown", "-R", &user, &mount]);
            execute(cmd).map_err(|err| EnsureFilesystemError {
                name: name.to_string(),
                mountpoint: mountpoint.clone(),
                err: err.into(),
            })?;
        }

        if let Some(SizeDetails { quota, reservation, compression }) =
            size_details
        {
            // Apply any quota and compression mode.
            Self::apply_properties(
                name,
                &mountpoint,
                quota,
                reservation,
                compression,
            )?;
        }

        Ok(())
    }

    /// Applies the following properties to the filesystem.
    ///
    /// If any of the options are not supplied, a default "none" or "off"
    /// value is supplied.
    fn apply_properties(
        name: &str,
        mountpoint: &Mountpoint,
        quota: Option<ByteCount>,
        reservation: Option<ByteCount>,
        compression: CompressionAlgorithm,
    ) -> Result<(), EnsureFilesystemError> {
        let quota = quota
            .map(|q| q.to_bytes().to_string())
            .unwrap_or_else(|| String::from("none"));
        let reservation = reservation
            .map(|r| r.to_bytes().to_string())
            .unwrap_or_else(|| String::from("none"));
        let compression = compression.to_string();

        if let Err(err) = Self::set_value(name, "quota", &quota) {
            return Err(EnsureFilesystemError {
                name: name.to_string(),
                mountpoint: mountpoint.clone(),
                // Take the execution error from the SetValueError
                err: err.err.into(),
            });
        }
        if let Err(err) = Self::set_value(name, "reservation", &reservation) {
            return Err(EnsureFilesystemError {
                name: name.to_string(),
                mountpoint: mountpoint.clone(),
                // Take the execution error from the SetValueError
                err: err.err.into(),
            });
        }
        if let Err(err) = Self::set_value(name, "compression", &compression) {
            return Err(EnsureFilesystemError {
                name: name.to_string(),
                mountpoint: mountpoint.clone(),
                // Take the execution error from the SetValueError
                err: err.err.into(),
            });
        }
        Ok(())
    }

    fn mount_encrypted_dataset(
        name: &str,
        mountpoint: &Mountpoint,
    ) -> Result<(), EnsureFilesystemError> {
        let mut command = std::process::Command::new(PFEXEC);
        let cmd = command.args(&[ZFS, "mount", "-l", name]);
        execute(cmd).map_err(|err| EnsureFilesystemError {
            name: name.to_string(),
            mountpoint: mountpoint.clone(),
            err: EnsureFilesystemErrorRaw::MountEncryptedFsFailed(err),
        })?;
        Ok(())
    }

    pub fn mount_overlay_dataset(
        name: &str,
        mountpoint: &Mountpoint,
    ) -> Result<(), EnsureFilesystemError> {
        let mut command = std::process::Command::new(PFEXEC);
        let cmd = command.args(&[ZFS, "mount", "-O", name]);
        execute(cmd).map_err(|err| EnsureFilesystemError {
            name: name.to_string(),
            mountpoint: mountpoint.clone(),
            err: EnsureFilesystemErrorRaw::MountOverlayFsFailed(err),
        })?;
        Ok(())
    }

    // Return (true, mounted) if the dataset exists, (false, false) otherwise,
    // where mounted is if the dataset is mounted.
    fn dataset_exists(
        name: &str,
        mountpoint: &Mountpoint,
    ) -> Result<(bool, bool), EnsureFilesystemError> {
        let mut command = std::process::Command::new(ZFS);
        let cmd = command.args(&[
            "list",
            "-Hpo",
            "name,type,mountpoint,mounted",
            name,
        ]);
        // If the list command returns any valid output, validate it.
        if let Ok(output) = execute(cmd) {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let values: Vec<&str> = stdout.trim().split('\t').collect();
            if &values[..3] != &[name, "filesystem", &mountpoint.to_string()] {
                return Err(EnsureFilesystemError {
                    name: name.to_string(),
                    mountpoint: mountpoint.clone(),
                    err: EnsureFilesystemErrorRaw::Output(stdout.to_string()),
                });
            }
            let mounted = values[3] == "yes";
            Ok((true, mounted))
        } else {
            Ok((false, false))
        }
    }

    /// Set the value of an Oxide-managed ZFS property.
    pub fn set_oxide_value(
        filesystem_name: &str,
        name: &str,
        value: &str,
    ) -> Result<(), SetValueError> {
        Zfs::set_value(filesystem_name, &format!("oxide:{}", name), value)
    }

    fn set_value(
        filesystem_name: &str,
        name: &str,
        value: &str,
    ) -> Result<(), SetValueError> {
        let mut command = std::process::Command::new(PFEXEC);
        let value_arg = format!("{}={}", name, value);
        let cmd = command.args(&[ZFS, "set", &value_arg, filesystem_name]);
        execute(cmd).map_err(|err| SetValueError {
            filesystem: filesystem_name.to_string(),
            name: name.to_string(),
            value: value.to_string(),
            err,
        })?;
        Ok(())
    }

    /// Get the value of an Oxide-managed ZFS property.
    pub fn get_oxide_value(
        filesystem_name: &str,
        name: &str,
    ) -> Result<String, GetValueError> {
        let property = format!("oxide:{name}");
        let [value] = Self::get_values(
            filesystem_name,
            &[&property],
            Some(PropertySource::Local),
        )?;
        Ok(value)
    }

    /// Calls "zfs get" with a single value
    pub fn get_value(
        filesystem_name: &str,
        name: &str,
    ) -> Result<String, GetValueError> {
        let [value] = Self::get_values(filesystem_name, &[name], None)?;
        Ok(value)
    }

    /// List all extant snapshots.
    pub fn list_snapshots() -> Result<Vec<Snapshot>, ListSnapshotsError> {
        let mut command = std::process::Command::new(ZFS);
        let cmd = command.args(&["list", "-H", "-o", "name", "-t", "snapshot"]);
        execute(cmd)
            .map(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .trim()
                    .lines()
                    .map(|line| {
                        let (filesystem, snap_name) =
                            line.split_once('@').unwrap();
                        Snapshot {
                            filesystem: filesystem.to_string(),
                            snap_name: snap_name.to_string(),
                        }
                    })
                    .collect()
            })
            .map_err(ListSnapshotsError::from)
    }

    /// Create a snapshot of a filesystem.
    ///
    /// A list of properties, as name-value tuples, may be passed to this
    /// method, for creating properties directly on the snapshots.
    pub fn create_snapshot<'a>(
        filesystem: &'a str,
        snap_name: &'a str,
        properties: &'a [(&'a str, &'a str)],
    ) -> Result<(), CreateSnapshotError> {
        let mut command = std::process::Command::new(ZFS);
        let mut cmd = command.arg("snapshot");
        for (name, value) in properties.iter() {
            cmd = cmd.arg("-o").arg(&format!("{name}={value}"));
        }
        cmd.arg(&format!("{filesystem}@{snap_name}"));
        execute(cmd).map(|_| ()).map_err(|err| CreateSnapshotError {
            filesystem: filesystem.to_string(),
            snap_name: snap_name.to_string(),
            err,
        })
    }

    /// Destroy a named snapshot of a filesystem.
    pub fn destroy_snapshot(
        filesystem: &str,
        snap_name: &str,
    ) -> Result<(), DestroySnapshotError> {
        let mut command = std::process::Command::new(ZFS);
        let path = format!("{filesystem}@{snap_name}");
        let cmd = command.args(&["destroy", &path]);
        execute(cmd).map(|_| ()).map_err(|err| DestroySnapshotError {
            filesystem: filesystem.to_string(),
            snap_name: snap_name.to_string(),
            err,
        })
    }
}

// These methods don't work with mockall, so they exist in a separate impl block
impl Zfs {
    /// Calls "zfs get" to acquire multiple values
    ///
    /// - `names`: The properties being acquired
    /// - `source`: The optioanl property source (origin of the property)
    /// Defaults to "all sources" when unspecified.
    pub fn get_values<const N: usize>(
        filesystem_name: &str,
        names: &[&str; N],
        source: Option<PropertySource>,
    ) -> Result<[String; N], GetValueError> {
        let mut cmd = std::process::Command::new(PFEXEC);
        let all_names =
            names.into_iter().map(|n| *n).collect::<Vec<&str>>().join(",");

        cmd.args(&[ZFS, "get", "-Ho", "value", &all_names]);
        if let Some(source) = source {
            cmd.args(&["-s", &source.to_string()]);
        }
        cmd.arg(filesystem_name);
        let output = execute(&mut cmd).map_err(|err| GetValueError {
            filesystem: filesystem_name.to_string(),
            name: format!("{:?}", names),
            err: err.into(),
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let values = stdout.trim();

        const EMPTY_STRING: String = String::new();
        let mut result: [String; N] = [EMPTY_STRING; N];

        for (i, value) in values.lines().enumerate() {
            let value = value.trim();
            if value == "-" {
                return Err(GetValueError {
                    filesystem: filesystem_name.to_string(),
                    name: names[i].to_string(),
                    err: GetValueErrorRaw::MissingValue,
                });
            }
            result[i] = value.to_string();
        }
        Ok(result)
    }
}

/// A read-only snapshot of a ZFS filesystem.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub filesystem: String,
    pub snap_name: String,
}

impl Snapshot {
    /// Return the full path to the snapshot directory within the filesystem.
    pub fn full_path(&self) -> Result<Utf8PathBuf, GetValueError> {
        let mountpoint = Zfs::get_value(&self.filesystem, "mountpoint")?;
        Ok(Utf8PathBuf::from(mountpoint)
            .join(format!(".zfs/snapshot/{}", self.snap_name)))
    }
}

impl fmt::Display for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}@{}", self.filesystem, self.snap_name)
    }
}

/// Returns all datasets managed by Omicron
pub fn get_all_omicron_datasets_for_delete() -> anyhow::Result<Vec<String>> {
    let mut datasets = vec![];

    // Collect all datasets within Oxide zpools.
    //
    // This includes cockroachdb, clickhouse, and crucible datasets.
    let zpools = crate::zpool::Zpool::list()?;
    for pool in &zpools {
        let internal =
            pool.kind() == omicron_common::zpool_name::ZpoolKind::Internal;
        let pool = pool.to_string();
        for dataset in &Zfs::list_datasets(&pool)? {
            // Avoid erasing crashdump, backing data and swap datasets on
            // internal pools. The swap device may be in use.
            if internal
                && (["crash", "backing", "swap"].contains(&dataset.as_str())
                    || dataset.starts_with("backing/"))
            {
                continue;
            }

            datasets.push(format!("{pool}/{dataset}"));
        }
    }

    // Collect all datasets for ramdisk-based Oxide zones, if any exist.
    if let Ok(ramdisk_datasets) = Zfs::list_datasets(&ZONE_ZFS_RAMDISK_DATASET)
    {
        for dataset in &ramdisk_datasets {
            datasets.push(format!("{}/{dataset}", ZONE_ZFS_RAMDISK_DATASET));
        }
    };

    Ok(datasets)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parse_dataset_props() {
        let input =
            "-       dataset_name        1234   5678   0       0       off";
        let props = DatasetProperties::from_str(&input)
            .expect("Should have parsed data");

        assert_eq!(props.id, None);
        assert_eq!(props.name, "dataset_name");
        assert_eq!(props.avail.to_bytes(), 1234);
        assert_eq!(props.used.to_bytes(), 5678);
        assert_eq!(props.quota, None);
        assert_eq!(props.reservation, None);
        assert_eq!(props.compression, "off");
    }

    #[test]
    fn parse_dataset_props_with_optionals() {
        let input = "d4e1e554-7b98-4413-809e-4a42561c3d0c       dataset_name        1234   5678   111       222       off";
        let props = DatasetProperties::from_str(&input)
            .expect("Should have parsed data");

        assert_eq!(
            props.id,
            Some("d4e1e554-7b98-4413-809e-4a42561c3d0c".parse().unwrap())
        );
        assert_eq!(props.name, "dataset_name");
        assert_eq!(props.avail.to_bytes(), 1234);
        assert_eq!(props.used.to_bytes(), 5678);
        assert_eq!(props.quota.map(|q| q.to_bytes()), Some(111));
        assert_eq!(props.reservation.map(|r| r.to_bytes()), Some(222));
        assert_eq!(props.compression, "off");
    }

    #[test]
    fn parse_dataset_bad_uuid() {
        let input = "bad       dataset_name        1234   5678   111       222       off";
        let err = DatasetProperties::from_str(&input)
            .expect_err("Should have failed to parse");
        assert!(
            format!("{err:#}").contains("error parsing UUID (dataset)"),
            "{err}"
        );
    }

    #[test]
    fn parse_dataset_bad_avail() {
        let input = "-       dataset_name        BADAVAIL   5678   111       222       off";
        let err = DatasetProperties::from_str(&input)
            .expect_err("Should have failed to parse");
        assert!(
            format!("{err:#}").contains("invalid digit found in string"),
            "{err}"
        );
    }

    #[test]
    fn parse_dataset_bad_usage() {
        let input = "-       dataset_name        1234   BADUSAGE   111       222       off";
        let err = DatasetProperties::from_str(&input)
            .expect_err("Should have failed to parse");
        assert!(
            format!("{err:#}").contains("invalid digit found in string"),
            "{err}"
        );
    }

    #[test]
    fn parse_dataset_bad_quota() {
        let input = "-       dataset_name        1234   5678   BADQUOTA      222       off";
        let err = DatasetProperties::from_str(&input)
            .expect_err("Should have failed to parse");
        assert!(
            format!("{err:#}").contains("invalid digit found in string"),
            "{err}"
        );
    }

    #[test]
    fn parse_dataset_bad_reservation() {
        let input = "-       dataset_name        1234   5678   111      BADRES       off";
        let err = DatasetProperties::from_str(&input)
            .expect_err("Should have failed to parse");
        assert!(
            format!("{err:#}").contains("invalid digit found in string"),
            "{err}"
        );
    }

    #[test]
    fn parse_dataset_missing_fields() {
        let expect_missing = |input: &str, what: &str| {
            let err = DatasetProperties::from_str(input)
                .expect_err("Should have failed to parse");
            let err = format!("{err:#}");
            assert!(err.contains(&format!("Missing {what}")), "{err}");
        };

        expect_missing(
            "-       dataset_name        1234   5678   111      222",
            "'compression'",
        );
        expect_missing(
            "-       dataset_name        1234   5678   111",
            "'reservation'",
        );
        expect_missing("-       dataset_name        1234   5678", "'quota'");
        expect_missing("-       dataset_name        1234", "'used'");
        expect_missing("-       dataset_name", "'avail'");
        expect_missing("-", "'name'");
        expect_missing("", "UUID");
    }
}
