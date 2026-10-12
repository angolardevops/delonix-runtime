//! The port a storage-pool driver implements (ADR-0067 D1), and its types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::allowlist::Entry;
use crate::ownership::Owner;
use crate::Result;

/// A pool as a driver sees it: its name and the administrator's entry. A
/// driver never receives anything a manifest wrote beyond a volume's name
/// and size.
#[derive(Debug, Clone, Copy)]
pub struct PoolRef<'a> {
    pub name: &'a str,
    pub entry: &'a Entry,
}

/// What a probe found. **Three values**: a listing obtained without the
/// privilege to list is `Undetermined`, never `Available` with nothing in it
/// (ADR-0067 D3.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PoolProbe {
    Available,
    /// The pool cannot be used, and the driver knows why.
    Unavailable {
        missing: String,
        remedy: String,
    },
    /// The driver could not find out.
    Undetermined {
        reason: String,
        remedy: String,
    },
}

impl PoolProbe {
    /// The word a listing shows.
    pub fn label(&self) -> &'static str {
        match self {
            PoolProbe::Available => "AVAILABLE",
            PoolProbe::Unavailable { .. } => "UNAVAILABLE",
            PoolProbe::Undetermined { .. } => "UNKNOWN",
        }
    }
}

/// What an operation needs on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// The engine's own process does it.
    Unprivileged,
    /// A delegation the administrator grants once (`zfs allow`), named here.
    Delegated(&'static str),
    /// It crosses into the privileged helper.
    Helper,
}

/// The operations of the port, for [`StoragePoolDriver::required`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolOp {
    Probe,
    Allocate(VolumeShape),
    Release,
    Usage,
}

/// The two things a pool hands out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeShape {
    /// A mounted directory — a container volume.
    Filesystem,
    /// A raw device or disk file — a VM disk.
    Block,
}

impl VolumeShape {
    pub fn label(self) -> &'static str {
        match self {
            VolumeShape::Filesystem => "filesystem",
            VolumeShape::Block => "block",
        }
    }

    /// The shape a label names.
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "filesystem" => Some(VolumeShape::Filesystem),
            "block" => Some(VolumeShape::Block),
            _ => None,
        }
    }
}

/// Is `name` a volume name a pool can hold? ASCII letters, digits, `.`, `_`
/// and `-`, starting with a letter or digit, no `..`, at most 63 bytes.
///
/// A pool volume's name becomes one path component under the pool, an LVM
/// logical volume name and a ZFS dataset component, so it is held to what all
/// three accept — and a leading `-` can never be read as an option by a tool.
pub fn valid_volume_name(name: &str) -> bool {
    let Some(first) = name.chars().next() else {
        return false;
    };
    name.len() <= 63
        && first.is_ascii_alphanumeric()
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// One volume asked of a pool.
#[derive(Debug, Clone)]
pub struct VolumeRequest {
    pub name: String,
    pub shape: VolumeShape,
    pub size_bytes: u64,
}

/// What the driver handed out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allocation {
    pub pool: String,
    pub volume: String,
    pub shape: VolumeShape,
    /// `Filesystem`: the directory a container mounts. `Block`: the device or file.
    pub path: PathBuf,
    /// The driver found the object already there, stamped for this owner, and
    /// took it back instead of making a second (ADR-0067 D5).
    pub adopted: bool,
}

/// How full the pool is. `None` is «not measured», never zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolUsage {
    pub capacity_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    /// A thin pool's metadata, which fills separately from its data.
    pub metadata_pct: Option<u8>,
}

impl PoolUsage {
    /// Used space as a percentage of capacity, rounded up.
    pub fn used_pct(&self) -> Option<u8> {
        let (cap, used) = (self.capacity_bytes?, self.used_bytes?);
        if cap == 0 {
            return None;
        }
        Some((used.saturating_mul(100).div_ceil(cap)).min(100) as u8)
    }
}

/// A storage-pool backend.
///
/// Pool creation and destruction are **not** here: they are the
/// administrator's, done at a terminal, never by the engine on a manifest's
/// word (ADR-0067 D3.3, D5).
pub trait StoragePoolDriver: Send + Sync {
    /// `dir`, `btrfs`, `zfs`, `lvm-thin`.
    fn id(&self) -> &'static str;

    /// Tools, module, privilege and health. Never writes.
    fn probe(&self, pool: &PoolRef) -> PoolProbe;

    /// What `op` needs on this host.
    fn required(&self, op: PoolOp) -> Privilege;

    /// Makes the volume, or takes back one already stamped for `owner`. An
    /// object with that name and no stamp of this owner is refused, never
    /// adopted by name. A shape the driver does not serve is refused by name.
    fn allocate(&self, pool: &PoolRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation>;

    /// Destroys a volume this owner's stamp is on: the data first, the stamp
    /// last. One that is already gone is `Ok`; one without the stamp is refused.
    fn release(&self, pool: &PoolRef, volume: &str, owner: &Owner) -> Result<()>;

    /// Capacity and use.
    fn usage(&self, pool: &PoolRef) -> PoolUsage;
}

#[cfg(test)]
mod tests {
    use super::{valid_volume_name, PoolUsage, VolumeShape};

    #[test]
    fn a_volume_name_is_one_safe_component_for_every_backend() {
        for ok in ["db", "Db-1", "a.b_c", "0"] {
            assert!(valid_volume_name(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            ".x",
            "_x",
            "a/b",
            "a..b",
            "a b",
            "a\0",
            &"x".repeat(64),
        ] {
            assert!(!valid_volume_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_shape_reads_back_its_label() {
        for s in [VolumeShape::Filesystem, VolumeShape::Block] {
            assert_eq!(VolumeShape::parse(s.label()), Some(s));
        }
        assert_eq!(VolumeShape::parse("raw"), None);
    }

    #[test]
    fn used_pct_rounds_up_and_refuses_to_guess() {
        let u = |cap, used| PoolUsage {
            capacity_bytes: cap,
            used_bytes: used,
            metadata_pct: None,
        };
        assert_eq!(u(Some(1000), Some(949)).used_pct(), Some(95));
        assert_eq!(u(Some(1000), Some(0)).used_pct(), Some(0));
        assert_eq!(u(Some(1000), Some(5000)).used_pct(), Some(100));
        assert_eq!(u(None, Some(1)).used_pct(), None);
        assert_eq!(u(Some(0), Some(0)).used_pct(), None);
    }
}
