//! What the target node's CLUSTER offers — ADR-0049 slice 3, read-only.
//!
//! The backend addresses ONE node (ADR-0008), and slice 3's rule is that it
//! never picks another: there is no implicit node selection anywhere here.
//! What this module adds is the other half of that rule — knowing what the
//! cluster around that node could do, so a capability that depends on it
//! (migration, HA, replication, Ceph) is answered from the cluster that is
//! actually there and not from a guess.
//!
//! **Every call here is a `GET`.** Five of them: `/cluster/status`,
//! `/storage`, `/cluster/ha/status/current`, `/cluster/ha/resources` and
//! `/cluster/sdn/zones`. None forks a task and none changes the cluster, which
//! is what makes it safe to point at a production cluster with an auditor's
//! token (`PVEAuditor`). A write belongs to the slices after this one, against
//! a lab.
//!
//! Measured shapes, not the schema's (PVE 9.2.2):
//! - a node that is not in a cluster answers `/cluster/status` with ONE
//!   `type: node` entry and no `type: cluster` entry — no name, no quorum;
//! - `/storage` carries `shared: 1` only on a shared storage (the flag is
//!   absent on `dir`/`lvmthin`), and `content` is a comma list;
//! - `/cluster/ha/status/current` always has a `quorum` entry, a `master`
//!   entry only while a CRM is active, and a `fencing` entry.
//!
//! The verdicts are FACTS about the cluster, not engine support: a cluster
//! that can migrate does not mean this engine migrates on it (it does not —
//! `vm.migration.*` stays what the catalogue declares). They are printed next
//! to the declared state by `delonix provider describe proxmox --probe`.

use crate::{parse, Client, Result, Wrapped};
use delonix_compute::capability::Capability;
use serde::Serialize;

/// One node of the cluster, as `/cluster/status` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClusterNode {
    /// The node's name.
    pub name: String,
    /// Whether the cluster sees it online.
    pub online: bool,
    /// Whether this is the node the backend addresses. Exactly one is, or
    /// none when the configured node is not a member — never picked.
    pub target: bool,
}

/// One storage of the cluster, as `/storage` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClusterStorage {
    /// Storage id (`local-lvm`, `ceph-vm`, …).
    pub id: String,
    /// Storage type (`dir`, `lvmthin`, `rbd`, `cephfs`, `nfs`, `zfspool`, `pbs`, …).
    pub kind: String,
    /// Whether every node sees the same content (`shared: 1`).
    pub shared: bool,
    /// Whether it may hold VM disks (`images` in `content`).
    pub images: bool,
}

/// The HA stack, as `/cluster/ha/status/current` and `/cluster/ha/resources` say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HaFacts {
    /// The `master` entry's status while a CRM is active (`"ngola (active, …)"`);
    /// `None` when no CRM is master — HA is configured on nothing.
    pub master: Option<String>,
    /// How many resources HA manages.
    pub resources: usize,
}

/// What the target's cluster is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClusterFacts {
    /// The cluster's name; `None` for a node that is not in a cluster.
    pub cluster: Option<String>,
    /// Whether the cluster is quorate; `None` for a node that is not in one.
    pub quorate: Option<bool>,
    /// Every node the cluster lists.
    pub nodes: Vec<ClusterNode>,
    /// Every storage the cluster defines.
    pub storages: Vec<ClusterStorage>,
    /// The HA stack.
    pub ha: HaFacts,
    /// How many SDN zones the cluster defines.
    pub sdn_zones: usize,
}

/// What the cluster offers for one cluster-dependent capability. Not
/// `Serialize`: the catalogue's `Capability` is not, and the caller names it
/// with `Capability::name()` when it prints JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterVerdict {
    /// The catalogue capability.
    pub capability: Capability,
    /// Whether THIS cluster offers what the capability needs.
    pub offered: bool,
    /// Why, in terms of what was read.
    pub reason: String,
}

impl Client {
    /// Reads what the target's cluster is: five `GET`s, nothing written.
    pub fn cluster_facts(&self) -> Result<ClusterFacts> {
        let status: Wrapped<serde_json::Value> =
            parse(&self.get("/cluster/status")?, "cluster status")?;
        let storage: Wrapped<serde_json::Value> = parse(&self.get("/storage")?, "storage")?;
        let ha: Wrapped<serde_json::Value> =
            parse(&self.get("/cluster/ha/status/current")?, "ha status")?;
        let ha_resources: Wrapped<serde_json::Value> =
            parse(&self.get("/cluster/ha/resources")?, "ha resources")?;
        let zones: Wrapped<serde_json::Value> =
            parse(&self.get("/cluster/sdn/zones")?, "sdn zones")?;
        Ok(facts_from(
            &status.data,
            &storage.data,
            &ha.data,
            &ha_resources.data,
            &zones.data,
            &self.node,
        ))
    }
}

fn items(v: &serde_json::Value) -> &[serde_json::Value] {
    v.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn text<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|s| s.as_str())
}

/// `1`/`true` as the node sends a flag; absent or anything else is `false`.
fn flag(v: &serde_json::Value, key: &str) -> bool {
    match v.get(key) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_u64() == Some(1),
        Some(serde_json::Value::String(s)) => s == "1",
        _ => false,
    }
}

/// Builds [`ClusterFacts`] from the five answers. Pure, so the measured shapes
/// are tests and not assumptions.
pub fn facts_from(
    status: &serde_json::Value,
    storage: &serde_json::Value,
    ha: &serde_json::Value,
    ha_resources: &serde_json::Value,
    zones: &serde_json::Value,
    target_node: &str,
) -> ClusterFacts {
    let entries = items(status);
    let cluster_entry = entries.iter().find(|e| text(e, "type") == Some("cluster"));
    let nodes = entries
        .iter()
        .filter(|e| text(e, "type") == Some("node"))
        .filter_map(|e| {
            let name = text(e, "name")?.to_string();
            Some(ClusterNode {
                online: flag(e, "online"),
                target: name == target_node,
                name,
            })
        })
        .collect();
    let storages = items(storage)
        .iter()
        .filter_map(|s| {
            Some(ClusterStorage {
                id: text(s, "storage")?.to_string(),
                kind: text(s, "type").unwrap_or_default().to_string(),
                shared: flag(s, "shared"),
                images: text(s, "content")
                    .unwrap_or_default()
                    .split(',')
                    .any(|c| c.trim() == "images"),
            })
        })
        .collect();
    let master = items(ha)
        .iter()
        .find(|e| text(e, "type") == Some("master"))
        .and_then(|e| text(e, "status"))
        .map(str::to_string);
    ClusterFacts {
        cluster: cluster_entry
            .and_then(|e| text(e, "name"))
            .map(str::to_string),
        quorate: cluster_entry.map(|e| flag(e, "quorate")),
        nodes,
        storages,
        ha: HaFacts {
            master,
            resources: items(ha_resources).len(),
        },
        sdn_zones: items(zones).len(),
    }
}

/// What the cluster offers for each capability that depends on it. Pure.
///
/// - **migration** (cold and live): at least two ONLINE nodes, a quorate
///   cluster, and a SHARED storage that holds VM disks — without one the disk
///   has to be copied, which is the local-disk migration the node refuses for
///   a running VM;
/// - **HA**: a quorate cluster of at least two nodes with a CRM master;
/// - **replication**: a `zfspool` storage on a cluster of at least two nodes
///   (Proxmox replicates ZFS volumes only);
/// - **Ceph**: an `rbd` or `cephfs` storage.
pub fn cluster_verdicts(f: &ClusterFacts) -> Vec<ClusterVerdict> {
    let online = f.nodes.iter().filter(|n| n.online).count();
    let clustered = f.cluster.is_some();
    let quorate = f.quorate == Some(true);
    let shared_images: Vec<&str> = f
        .storages
        .iter()
        .filter(|s| s.shared && s.images)
        .map(|s| s.id.as_str())
        .collect();
    let not_clustered = "the target node is not in a cluster".to_string();
    let migration = if !clustered {
        (false, not_clustered.clone())
    } else if !quorate {
        (false, "the cluster is not quorate".to_string())
    } else if online < 2 {
        (
            false,
            format!("{online} node(s) online — a migration needs two"),
        )
    } else if shared_images.is_empty() {
        (
            false,
            format!(
                "{online} nodes online, but no shared storage holds VM disks — every disk would \
                 have to be copied"
            ),
        )
    } else {
        (
            true,
            format!(
                "{online} nodes online, quorate, shared storage for VM disks: {}",
                shared_images.join(", ")
            ),
        )
    };
    let ha = if !clustered {
        (false, not_clustered.clone())
    } else if !quorate || online < 2 {
        (
            false,
            format!("quorate: {quorate}, {online} node(s) online — HA needs a quorate cluster of two or more"),
        )
    } else {
        match &f.ha.master {
            Some(m) => (
                true,
                format!("CRM master {m}; {} resource(s) under HA", f.ha.resources),
            ),
            None => (
                false,
                "no CRM master — HA manages nothing on this cluster".to_string(),
            ),
        }
    };
    let zfs: Vec<&str> = f
        .storages
        .iter()
        .filter(|s| s.kind == "zfspool")
        .map(|s| s.id.as_str())
        .collect();
    let replication = if !clustered || f.nodes.len() < 2 {
        (
            false,
            "replication needs a cluster of two or more nodes".to_string(),
        )
    } else if zfs.is_empty() {
        (
            false,
            "no zfspool storage — Proxmox replicates ZFS volumes only".to_string(),
        )
    } else {
        (true, format!("zfspool storage: {}", zfs.join(", ")))
    };
    let ceph: Vec<&str> = f
        .storages
        .iter()
        .filter(|s| s.kind == "rbd" || s.kind == "cephfs")
        .map(|s| s.id.as_str())
        .collect();
    let ceph = if ceph.is_empty() {
        (false, "no rbd/cephfs storage".to_string())
    } else {
        (true, format!("Ceph storage: {}", ceph.join(", ")))
    };
    let verdict = |capability, (offered, reason): (bool, String)| ClusterVerdict {
        capability,
        offered,
        reason,
    };
    vec![
        verdict(Capability::VmMigrationCold, migration.clone()),
        verdict(Capability::VmMigrationLive, migration),
        verdict(Capability::VmHighAvailability, ha),
        verdict(Capability::VmReplication, replication),
        verdict(Capability::StorageCeph, ceph),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The lab node, measured on PVE 9.2.2: no cluster entry, two local
    /// storages without `shared`, a quorum entry and no master.
    fn lab() -> ClusterFacts {
        facts_from(
            &json!([{"name":"pve","id":"node/pve","online":1,"nodeid":0,"ip":"10.0.2.15","local":1,"type":"node"}]),
            &json!([
                {"type":"dir","path":"/var/lib/vz","content":"iso,backup,vztmpl,images,import","storage":"local"},
                {"thinpool":"data","storage":"local-lvm","content":"images,rootdir","type":"lvmthin","vgname":"pve"}
            ]),
            &json!([
                {"type":"quorum","quorate":1,"status":"OK","id":"quorum","node":"pve"},
                {"status":"standby (CRM watchdog standby)","id":"fencing","type":"fencing","node":"pve","armed-state":"standby"}
            ]),
            &json!([]),
            &json!([]),
            "pve",
        )
    }

    #[test]
    fn a_node_outside_a_cluster_offers_none_of_the_cluster_capabilities() {
        let f = lab();
        assert_eq!(f.cluster, None);
        assert_eq!(f.quorate, None);
        assert_eq!(
            f.nodes,
            vec![ClusterNode {
                name: "pve".into(),
                online: true,
                target: true
            }]
        );
        assert!(f.storages.iter().all(|s| !s.shared));
        assert!(f.storages.iter().all(|s| s.images));
        assert_eq!(f.ha.master, None);
        let v = cluster_verdicts(&f);
        assert_eq!(v.len(), 5);
        for x in &v[..3] {
            assert!(!x.offered, "{x:?}");
            assert!(x.reason.contains("not in a cluster"), "{x:?}");
        }
        assert!(!v[3].offered && !v[4].offered);
    }

    /// A three-node cluster with Ceph and an active CRM: migration and HA are
    /// offered, the TARGET is the one configured and no other node is marked.
    #[test]
    fn a_quorate_cluster_with_shared_disks_offers_migration_and_ha() {
        let f = facts_from(
            &json!([
                {"type":"cluster","name":"ngola-lda","quorate":1,"nodes":3,"version":7},
                {"type":"node","name":"ngola","online":1},
                {"type":"node","name":"delonix02","online":1},
                {"type":"node","name":"delonix03","online":0}
            ]),
            &json!([
                {"storage":"ceph-vm","type":"rbd","content":"images,rootdir","shared":1},
                {"storage":"local-lvm","type":"lvmthin","content":"images"},
                {"storage":"pbs","type":"pbs","content":"backup","shared":1}
            ]),
            &json!([
                {"type":"quorum","quorate":1,"status":"OK"},
                {"type":"master","status":"ngola (active, Thu Sep 25 10:00:00 2026)"}
            ]),
            &json!([{"sid":"vm:101"},{"sid":"vm:102"}]),
            &json!([{"zone":"z1"}]),
            "delonix02",
        );
        assert_eq!(f.cluster.as_deref(), Some("ngola-lda"));
        assert_eq!(f.quorate, Some(true));
        let targets: Vec<&str> = f
            .nodes
            .iter()
            .filter(|n| n.target)
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(
            targets,
            ["delonix02"],
            "exactly the configured node, never picked"
        );
        assert_eq!(f.ha.resources, 2);
        assert_eq!(f.sdn_zones, 1);
        let v = cluster_verdicts(&f);
        let by = |c: Capability| v.iter().find(|x| x.capability == c).unwrap();
        assert!(by(Capability::VmMigrationLive).offered);
        assert!(by(Capability::VmMigrationLive).reason.contains("ceph-vm"));
        assert!(
            !by(Capability::VmMigrationLive).reason.contains("pbs"),
            "a shared BACKUP store is not a disk store"
        );
        assert!(by(Capability::VmHighAvailability).offered);
        assert!(!by(Capability::VmReplication).offered, "no zfspool");
        assert!(by(Capability::StorageCeph).offered);
    }

    #[test]
    fn a_cluster_without_shared_disks_or_quorum_is_said_so() {
        let status = json!([
            {"type":"cluster","name":"c","quorate":0},
            {"type":"node","name":"a","online":1},
            {"type":"node","name":"b","online":1}
        ]);
        let local = json!([{"storage":"local-lvm","type":"lvmthin","content":"images"}]);
        let f = facts_from(&status, &local, &json!([]), &json!([]), &json!([]), "a");
        let v = cluster_verdicts(&f);
        assert!(
            !v[0].offered && v[0].reason.contains("not quorate"),
            "{:?}",
            v[0]
        );
        let status = json!([
            {"type":"cluster","name":"c","quorate":1},
            {"type":"node","name":"a","online":1},
            {"type":"node","name":"b","online":1}
        ]);
        let f = facts_from(&status, &local, &json!([]), &json!([]), &json!([]), "zz");
        assert!(
            f.nodes.iter().all(|n| !n.target),
            "an unknown target is marked nowhere"
        );
        let v = cluster_verdicts(&f);
        assert!(
            !v[0].offered && v[0].reason.contains("no shared storage"),
            "{:?}",
            v[0]
        );
        assert!(
            !v[2].offered && v[2].reason.contains("no CRM master"),
            "{:?}",
            v[2]
        );
    }
}
