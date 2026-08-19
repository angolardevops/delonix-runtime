//! The reclaim engine behind `system prune` and the three per-resource
//! `prune`s (`container`/`image`/`volumes`).
//!
//! # Why one module and not one implementation per group
//!
//! `system prune` already knew how to sweep everything; what it did not have
//! was a way to sweep ONE thing. An SRE who wants the disk back from images
//! alone had to run the global prune — which also removes every stopped
//! container, with no way to say no to that half — or write a shell loop.
//!
//! So the phases moved here whole and `system prune` became their first
//! caller, not a second copy of them. Duplicating a destructive sweep is how
//! two answers to the same question start to diverge, and this repo has paid
//! for that pattern more than once (`peer_uid` in four files, the DHCP lease
//! arithmetic in two).
//!
//! # What each sweep owns
//!
//! * [`sweep_containers`] — stopped containers, their rootfs directories, the
//!   orphan directories nobody registered, empty cgroups, orphan ingress refs,
//!   orphan host ports and orphan slirps. Everything whose lifetime is a
//!   container's.
//! * [`sweep_images`] — dangling (or, with `all`, every unused) image, plus the
//!   CAS blobs nobody references afterwards. The blob pass MUST follow the
//!   image pass: it is the image removal that makes the blobs unreferenced.
//! * [`sweep_volumes`] — volumes nothing references. **New**: `system prune`
//!   never touched volumes and still does not, exactly as `docker system prune`
//!   leaves them alone. Removing a volume destroys data; it takes its own verb.
//! * [`sweep_networks`] — empty auto-created `dlx-*` networks.
//! * [`sweep_vms`] — the entries of `vms/` no VM record accounts for, and only
//!   with `stopped` the VMs themselves.
//!
//! # `system prune` deliberately does NOT call `sweep_vms`
//!
//! Every other sweep here is safe to fold into the global one because what it
//! takes is disposable. A VM is not: on the host `sweep_vms` was written
//! against, all seventeen were stopped, and three directories holding 53 GiB of
//! live disks sat in `vms/` under names no VM carries. `vm prune` therefore
//! keeps its own verb and its own preview, the same way `volumes prune` does
//! and for the same reason — the sweep destroys something somebody built.
//!
//! # The measurement is a LOWER BOUND, and says so
//!
//! In rootless a container's rootfs is written by a SUBUID, so a walk from
//! outside the user namespace cannot read all of it — and an unreadable
//! directory is not an empty one (the trap this repo catalogued after reporting
//! five Postgres volumes as lost). [`Reclaimed`] therefore carries whether the
//! walk was complete, and prints `≥` when it was not, instead of a confident
//! number that is quietly too small.

use std::collections::HashSet;

use delonix_image::ImageStore;
use delonix_runtime_core::{Result, Store};
use delonix_volume::VolumeStore;

use super::po;

/// Bytes reclaimed, plus whether the walk that measured them could read
/// everything. `partial` is never cosmetic: it is the difference between "this
/// freed 40 MiB" and "this freed at least 40 MiB, and the rest was written by a
/// subuid we cannot stat from here".
#[derive(Default, Clone, Copy)]
pub(crate) struct Reclaimed {
    pub bytes: u64,
    pub partial: bool,
}

impl Reclaimed {
    pub fn add(&mut self, other: Reclaimed) {
        self.bytes += other.bytes;
        self.partial |= other.partial;
    }

    /// Human size, prefixed with `≥` when the measurement was incomplete.
    pub fn fmt(&self) -> String {
        let size = super::output::fmt_size(self.bytes);
        if self.partial {
            format!("≥ {size}")
        } else {
            size
        }
    }
}

/// Size of a tree, keeping the "was it fully readable" bit that `dir_size`
/// throws away. Same corrected walk as the volume quota and the dashboard
/// collector (allocated blocks, `(dev, ino)` dedup) — the single measurement in
/// the tree, so the three cannot drift.
fn measure(p: &std::path::Path) -> Reclaimed {
    let u = delonix_volume::measure(p);
    Reclaimed {
        bytes: u.bytes,
        partial: !u.is_complete(),
    }
}

/// **PURE** — the occupancy percentage `df` would print, from raw `statvfs`
/// counters.
///
/// The formula is `df`'s, deliberately and not by coincidence: an operator who
/// sets a 75% threshold read that 75 off `df -h`. Computing it any other way —
/// `(blocks - bfree) / blocks`, the obvious one — reports a SMALLER number,
/// because it counts the root-reserved blocks as free space that a rootless
/// engine can never use. The gap is around 5% on a default ext4, which is the
/// whole distance between this threshold and the thin-pool alert above it.
///
/// So: used = `blocks - bfree`, usable = used + `bavail`, and the percentage
/// rounds UP (`df` never reports 99% for a filesystem that has 0.4% left).
///
/// A filesystem with no usable blocks at all reports 100: as full as it gets,
/// and the reading that makes a reclaim sweep run rather than skip.
pub(crate) fn used_pct(blocks: u64, bfree: u64, bavail: u64) -> u8 {
    let used = blocks.saturating_sub(bfree);
    let usable = used.saturating_add(bavail);
    if usable == 0 {
        return 100;
    }
    // Integer ceiling, and saturating: a percentage cannot exceed 100 even if a
    // filesystem reports counters that do not add up.
    let pct = (used.saturating_mul(100)).div_ceil(usable);
    pct.min(100) as u8
}

/// Occupancy of the filesystem holding `path`, via `statvfs(3)`.
///
/// `None` when the call fails — and the caller must NOT read that as "empty".
/// Reaper rule 4 (no visibility, defer): a sweep that cannot see how full the
/// disk is has no business deciding it is full enough to start deleting.
pub(crate) fn filesystem_used_pct(path: &std::path::Path) -> Option<u8> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call,
    // and `stat` is a fully-owned, correctly-sized destination.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(used_pct(
        stat.f_blocks as u64,
        stat.f_bfree as u64,
        stat.f_bavail as u64,
    ))
}

/// Confirmation gate, shared by all four prunes.
///
/// Two different situations, deliberately not collapsed into one:
/// * **no terminal** (script/CI) — nobody can answer, so blocking forever would
///   be worse than acting; `--force` is required instead, which makes every
///   unattended prune explicit.
/// * **a terminal** — ask, and print the preview first when there is one.
///
/// Returns `Ok(false)` when the operator declined; the caller must then do
/// nothing at all.
pub(crate) fn confirm(
    force: bool,
    unattended_error: &str,
    preview: Option<String>,
    question: &str,
) -> Result<bool> {
    if force {
        return Ok(true);
    }
    let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    if !tty {
        return Err(delonix_runtime_core::Error::Invalid(
            unattended_error.into(),
        ));
    }
    if let Some(p) = preview {
        println!("{p}");
    }
    print!("{question} ");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err()
        || !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
    {
        println!("{}", po::t("aborted"));
        return Ok(false);
    }
    Ok(true)
}

/// Explains the `≥`, once, after the summary line. Without it the symbol reads
/// as decoration; with it the operator knows the real figure is higher and why
/// this engine cannot state it from outside the user namespace.
pub(crate) fn note_partial(r: Reclaimed) {
    if r.partial {
        println!(
            "  {}",
            po::t(
                "part of the tree was unreadable from outside the user namespace (rootless \
                 subuid): the freed figure is a lower bound",
            )
        );
    }
}

/// The names of the containers a prune would remove — the preview that turns a
/// blind `[y/N]` into an informed one.
pub(crate) fn doomed_containers(store: &Store) -> Result<Vec<String>> {
    Ok(store
        .list()?
        .into_iter()
        .filter(|c| !c.pid.map(delonix_runtime::is_alive).unwrap_or(false))
        .map(|c| c.name)
        .collect())
}

/// What [`sweep_containers`] reclaimed.
#[derive(Default)]
pub(crate) struct ContainerSweep {
    pub containers: usize,
    pub dirs: usize,
    pub cgroups: usize,
    pub ports: usize,
    pub refs: usize,
    pub slirps: usize,
    pub freed: Reclaimed,
}

/// Everything whose lifetime is a container's: stopped containers, their
/// directories, and the debris left by the ones that died without an `rm`.
///
/// The single biggest reclaimer is the orphan directories — measured on this
/// machine, **88 container directories on disk against 4 in the registry
/// (~36 GiB)**. They come from cluster nodes and containers killed by
/// SIGKILL/crash/closed session, so no `container rm` will ever see them: they
/// are not in the registry, and only an explicit GC like this one catches them.
pub(crate) fn sweep_containers(images: &ImageStore, store: &Store) -> Result<ContainerSweep> {
    // Orphan slirps (dead target) — the SAFE reaper, never the fail-open
    // `reap_orphan_hostfwds` (see the history of the reaper that deleted live
    // ports).
    let mut out = ContainerSweep {
        slirps: delonix_net::reap_orphan_slirp(),
        ..Default::default()
    };

    // 1) stopped containers that ARE in the registry.
    //
    // The directory is measured BEFORE the removal and counted only when
    // `remove_container_dir` reports success — in rootless it uses
    // `remove_dir_all`, which cannot delete subuid files, and whatever it
    // leaves behind is picked up (and counted) by the orphan pass below.
    // Counting it in both places would report twice the space that was freed.
    for c in store.list()? {
        if c.pid.map(delonix_runtime::is_alive).unwrap_or(false) {
            continue;
        }
        let size = measure(&images.container_path(&c.id));
        let _ = delonix_runtime::remove(store, &c, true);
        let _ = images.unmount_rootfs(&c.id);
        if images.remove_container_dir(&c.id) {
            out.freed.add(size);
        }
        out.containers += 1;
    }

    /// Is the VM behind a `vm-<nome>` ingress ref still running?
    ///
    /// Reads the record AND checks the pid, because the two can disagree: a VM whose
    /// host died keeps `status: Running` on disk until something reconciles it, and
    /// that stale record is exactly what would keep a dead ref alive. A name the
    /// store does not know is not alive either — the VM was removed and its ref
    /// outlived it.
    fn vm_esta_viva(name: &str) -> bool {
        let st: delonix_runtime_core::JsonStore<delonix_runtime_core::Vm> =
            match delonix_runtime_core::JsonStore::open(super::util::state_root().join("vms")) {
                Ok(s) => s,
                // Cannot tell → do NOT reap. Freeing the ref of a live VM cuts its
                // network; leaving a stale one costs a refcount.
                Err(_) => return true,
            };
        match st.load(name) {
            Ok(vm) => {
                matches!(vm.status, delonix_runtime_core::Status::Running)
                    && vm.pid.map(delonix_runtime::is_alive).unwrap_or(false)
            }
            Err(_) => false,
        }
    }

    // Ids still alive AFTER step 1 — the basis for deciding what is orphan.
    let live_ids: HashSet<String> = store.list()?.iter().map(|c| c.id.clone()).collect();

    // 2) orphan ingress ref markers — the "16 refs with 3 live containers"
    //    leak. A container killed without `rm` leaves its marker holding the
    //    shared infra forever. `live` = ids of running containers PLUS the CRI
    //    pods (`cri-*`) and VMs (`vm-*`), which belong to other stores and are
    //    never reaped here. The reaper frees only markers with no live owner,
    //    and tears the infra down if it ends up empty; it NEVER touches a live id.
    let mut live_refs: HashSet<String> = store
        .list()?
        .iter()
        .filter(|c| c.pid.map(delonix_runtime::is_alive).unwrap_or(false))
        .map(|c| c.id.clone())
        .collect();
    for id in delonix_net::infra::attached_refs() {
        // A `vm-<nome>` ref is checked against the VM store instead of being
        // assumed alive. Assuming made it IMMORTAL: nothing on the system ever
        // freed it, so the ref-count never reached zero, the infra was never torn
        // down, and — the part that bites — `delonix-cri` reports
        // `NetworkReady=false` whenever the infra is down while a ref remains,
        // which pins the node in NotReady forever.
        //
        // Measured on this host after a reboot: `ingress DOWN … refcount 1`, held
        // by `vm-micro`, a VM that had been stopped since before the reboot. Same
        // family as every other trap in this repo — a FILE that outlives the
        // process that created it is not a live thing.
        //
        // `cri-*` stays unconditional, and that is not an oversight: a sandbox id
        // belongs to the CRI's own bookkeeping, this command cannot tell a live
        // pod from a dead one, and guessing wrong tears the network out from
        // under a running pod. Conservative there is the right side to err on;
        // here the store gives a real answer.
        if id.starts_with("cri-") {
            live_refs.insert(id);
        } else if let Some(name) = id.strip_prefix("vm-") {
            if vm_esta_viva(name) {
                live_refs.insert(id);
            }
        }
    }
    out.refs = delonix_net::infra::reap_orphan_refs(&live_refs);

    // 3) orphan container DIRECTORIES — the big space reclaimer.
    //
    // A `<containers>/<id>/` whose `<id>` is no longer in the registry.
    // `remove_tree_mapped` and not `remove_dir_all`: the rootfs may hold SUBUID
    // files that the real user cannot delete directly.
    let containers_dir = images.root().join("containers");
    for path in orphan_container_dirs(&containers_dir, &live_ids) {
        out.freed.add(measure(&path));
        delonix_runtime::remove_tree_mapped(&path);
        out.dirs += 1;
    }

    // 4) orphan EMPTY cgroups in delonix.slice.
    let live_cg: HashSet<String> = live_ids.iter().map(|id| format!("delonix-{id}")).collect();
    if let Ok(rd) = std::fs::read_dir(delonix_runtime_core::DELONIX_SLICE) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // `remove_dir` (not `_all`): only removes if EMPTY — a cgroup with
            // processes inside refuses, and rightly so.
            if name.starts_with("delonix-")
                && !live_cg.contains(&name)
                && std::fs::remove_dir(e.path()).is_ok()
            {
                out.cgroups += 1;
            }
        }
    }

    // 5) orphan hostfwds — host ports held by containers that already died.
    //
    // ONLY from the default root, and that guard is not paranoia: the store is
    // per-`DELONIX_ROOT` while the slirp api-socket is per-UID
    // (`/tmp/delonix-net-<uid>/slirp.sock`, see `runtime_dir`). Under an
    // alternative root — a test root, a second engine on the same login — the
    // `live_ports` set is EMPTY while the socket still serves the real ingress,
    // and `AuthoritativeLivePorts` would then be asserting authority the caller
    // does not have: every published port on the machine reaped as an orphan.
    //
    // That is the exact shape of the bug that once made published ports die on
    // their own and took several sessions to pin on an external caller passing
    // its own partial list. The type exists to force the claim to be made out
    // loud; making it here is only truthful when this root IS the one that owns
    // the ingress.
    if owns_shared_ingress_at(&super::util::state_root()) {
        let live_ports: HashSet<u32> = store
            .list()?
            .iter()
            .filter(|c| c.pid.map(delonix_runtime::is_alive).unwrap_or(false))
            .flat_map(|c| c.ports.iter())
            .filter_map(|p| {
                delonix_net::parse_publish(p)
                    .ok()
                    .and_then(|(hp, _, _)| hp.parse::<u32>().ok())
            })
            .collect();
        out.ports = delonix_net::infra::reap_orphan_hostfwds(
            delonix_net::infra::AuthoritativeLivePorts::new(&live_ports),
        );
    }

    Ok(out)
}

/// **PURE** — is `root` the one the per-UID network infra belongs to?
///
/// The ingress (slirp api-socket, control socket) is keyed by UID; the container
/// registry is keyed by `DELONIX_ROOT`. They agree only at the default root, and
/// every sweep that reasons about "who owns this port" is truthful only there.
pub(crate) fn owns_shared_ingress_at(root: &std::path::Path) -> bool {
    root == ImageStore::default_root()
}

/// What [`sweep_images`] reclaimed.
#[derive(Default)]
pub(crate) struct ImageSweep {
    pub images: usize,
    pub blobs: usize,
    pub freed: Reclaimed,
}

/// Dangling images — or, with `all`, every image no container uses — followed
/// by the CAS blobs nobody references any more.
///
/// The order is not a preference: the blobs only become unreferenced once the
/// images that named them are gone.
pub(crate) fn sweep_images(images: &ImageStore, store: &Store, all: bool) -> Result<ImageSweep> {
    let mut out = ImageSweep::default();

    let in_use: HashSet<String> = store.list()?.iter().map(|c| c.image.clone()).collect();
    for img in images.list()? {
        let dangling =
            img.repo_tags.is_empty() || img.repo_tags.iter().all(|t| t.contains("<none>"));
        let used = in_use.contains(&img.id) || img.repo_tags.iter().any(|t| in_use.contains(t));
        if (dangling || all) && !used {
            if img.repo_tags.is_empty() {
                let _ = images.remove(&img.id);
            } else {
                for t in &img.repo_tags {
                    let _ = images.remove(t);
                }
            }
            out.images += 1;
        }
    }

    let mut referenced: HashSet<String> = HashSet::new();
    for img in images.list()? {
        referenced.insert(delonix_image::cas::strip(&img.id).to_string());
        for l in &img.layers {
            referenced.insert(delonix_image::cas::strip(l).to_string());
        }
    }
    let blobs_dir = images.root().join("blobs").join("sha256");
    if let Ok(rd) = std::fs::read_dir(&blobs_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || referenced.contains(&name) {
                continue;
            }
            out.freed.bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = std::fs::remove_file(e.path());
            out.blobs += 1;
        }
    }

    Ok(out)
}

/// Empty auto-created `dlx-*` networks (a cluster that has been deleted). A
/// user network, without the prefix, is NEVER touched here.
pub(crate) fn sweep_networks(store: &Store) -> Result<usize> {
    let attached: HashSet<String> = store
        .list()?
        .iter()
        .filter_map(|c| c.network.clone())
        .collect();
    let mut n = 0usize;
    if let Ok(nstore) = delonix_net::NetworkStore::open(super::util::state_root()) {
        if let Ok(nets) = nstore.list() {
            for net in nets {
                if net.name.starts_with("dlx-") && !attached.contains(&net.name) {
                    let _ = nstore.remove(&net.name);
                    delonix_net::infra::network_remove(&net.name);
                    n += 1;
                }
            }
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

/// Everything the prune decision needs to know about one volume, read off the
/// store by [`volume_facts`]. A plain struct on purpose: it is what makes
/// [`classify_volumes`] — the part that decides whether data lives or dies —
/// testable with no store, no user namespace and not a byte on disk.
#[derive(Clone, Debug)]
pub(crate) struct VolumeFacts {
    pub name: String,
    /// The namespace that owns it; `None` = the unscoped root (no owner).
    ///
    /// It is NOT read from the record — a volume's owner is where it lives on
    /// disk (`volumes/.ns/<ns>/<name>`), so it is carried here from the walk
    /// that found it rather than re-derived from the path by each reader.
    pub namespace: Option<String>,
    pub mountpoint: String,
    pub driver: String,
    /// Carries `delonix.io/provisioned-by`: the local record is the ONLY thing
    /// that says which dataset on which appliance belongs to this volume.
    pub provisioned: bool,
    /// A CONTAINER references it, running or stopped (see [`volume_facts`]).
    /// Being a share's parent is NOT this — that is derived, and reported.
    pub referenced: bool,
    /// Whether the sweep's [`Scope`] allows TAKING this one.
    ///
    /// Out-of-scope volumes are still collected, because safety is derived from
    /// the whole store: a `kind: ShareVolume` is registered in its tenant's
    /// sub-tree while its parent `Storage` stays unscoped node infrastructure
    /// (`sharevolume::apply_one` says so, and puts the share's data INSIDE the
    /// parent's tree). Hand `classify_volumes` only one namespace and that
    /// parent is invisible, the share looks like a free-standing volume, and
    /// `--namespace <tenant>` destroys data on the NAS. So the scope decides
    /// what may be taken; it never decides what is looked at.
    pub in_scope: bool,
}

impl VolumeFacts {
    /// The owner as an operator names it (`<none>` for the unscoped root).
    pub fn owner(&self) -> &str {
        self.namespace
            .as_deref()
            .unwrap_or(delonix_volume::OwnedVolume::NO_OWNER)
    }

    /// How this volume is named in a report that may span several owners.
    ///
    /// Unqualified inside one namespace would be ambiguous the moment two
    /// tenants use the same obvious name — and `data`, `pgdata` and `config`
    /// are exactly the names every tenant picks.
    pub fn qualified(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{ns}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// Which owners a volume sweep is allowed to touch.
///
/// A destructive sweep gets its blast radius from an explicit value, never from
/// a default that happens to be whatever the caller had in hand. This is rule 1
/// of the reaper rules (`prefix or owner, and refuse the rest`): the wrong
/// filter IS the problem, so the filter is a type, and adding an owner is a
/// change to it rather than a wider `if`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The unscoped root only — volumes with no owner. What `volumes prune`
    /// has always swept, and still sweeps when nobody says otherwise.
    Unowned,
    /// One namespace's sub-tree, and nothing else. The primitive a tenant
    /// teardown calls: `volumes prune --namespace <ns> --force`.
    Namespace(String),
    /// The root AND every namespace.
    Everything,
}

impl Scope {
    /// Whether a volume with this owner falls inside the scope.
    pub fn covers(&self, namespace: Option<&str>) -> bool {
        match self {
            Scope::Unowned => namespace.is_none(),
            Scope::Namespace(ns) => namespace == Some(ns.as_str()),
            Scope::Everything => true,
        }
    }
}

/// Why a volume was kept. Every variant is a concrete failure mode that
/// removing it would have caused, not a precaution in general.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Keep {
    /// Some container still points at it.
    InUse,
    /// Its data lives under another volume — it is a `kind: ShareVolume` carved
    /// out of that parent, and deleting it deletes that tenant's data.
    ShareOf(String),
    /// It is the PARENT of shares: its tree holds other volumes' data.
    HoldsShares(Vec<String>),
    /// Provisioned on a remote NAS: dropping the record orphans a dataset on
    /// another machine with nothing left anywhere pointing at it.
    Provisioned,
    /// A network share (`nfs`/`cifs`/`webdav`) — deliberate infrastructure an
    /// operator declared, not leftover debris, and the data is not even local.
    NetworkDriver(String),
}

/// **PURE** — splits the volumes into the ones prune may take and the ones it
/// must keep, with the reason for each.
///
/// The order of the checks is the order of the danger. A volume is prunable
/// only when it is unreferenced AND local AND not a share of another volume AND
/// carries no remote provisioning stamp; anything else is kept and named. This
/// is deliberately more conservative than `docker volume prune`, because three
/// of the four exclusions are paths this engine has already been burned by:
/// destroying a live container's data, destroying a parent `Storage` out from
/// under every share carved into it, and orphaning a remote dataset by deleting
/// the only record that pointed at it.
///
/// It never returns a referenced volume, a share, or a share's parent.
///
/// **The parent case was found live, not reasoned about.** In the first run the
/// parent of a share vanished from the report with no line at all: `volume_refs`
/// counts a share as a reference, so it came back `InUse` — the one variant that
/// prints nothing, because listing every attached volume would bury the ones
/// that look prunable and are not. A volume that is local, unreferenced by any
/// container and still not taken has to say why, so `referenced` now means
/// CONTAINERS only and the parent relationship is derived here, where both sides
/// of it are already in hand.
pub(crate) fn classify_volumes(
    vols: &[VolumeFacts],
) -> (Vec<VolumeFacts>, Vec<(VolumeFacts, Keep)>) {
    let mut take = Vec::new();
    let mut keep = Vec::new();
    for v in vols {
        // Out of scope: it took part in the derivation above as a possible
        // parent or child, and that is ALL it is here for. It is neither taken
        // nor reported as kept — a listing of every other tenant's volumes is
        // noise, and worse, it reads as if they had been considered.
        if !v.in_scope {
            continue;
        }
        let mine = std::path::Path::new(&v.mountpoint);
        // Identity is the MOUNTPOINT, not the name. Once a sweep spans several
        // namespaces the name stops being unique — two tenants both call it
        // `data` — and `o.name != v.name` would then read one tenant's volume
        // as "myself" and drop it from the parent/child derivation of the
        // other. The mountpoint is what is actually unique on disk, and
        // comparing it also excludes self exactly (`starts_with` is true for
        // equal paths).
        let parent = vols.iter().find(|o| {
            o.mountpoint != v.mountpoint && mine.starts_with(std::path::Path::new(&o.mountpoint))
        });
        let children: Vec<String> = vols
            .iter()
            .filter(|o| {
                o.mountpoint != v.mountpoint
                    && std::path::Path::new(&o.mountpoint).starts_with(mine)
            })
            .map(|o| o.qualified())
            .collect();
        if v.referenced {
            keep.push((v.clone(), Keep::InUse));
        } else if let Some(p) = parent {
            keep.push((v.clone(), Keep::ShareOf(p.qualified())));
        } else if !children.is_empty() {
            keep.push((v.clone(), Keep::HoldsShares(children)));
        } else if v.provisioned {
            keep.push((v.clone(), Keep::Provisioned));
        } else if delonix_volume::is_network_driver(&v.driver) {
            keep.push((v.clone(), Keep::NetworkDriver(v.driver.clone())));
        } else {
            take.push(v.clone());
        }
    }
    (take, keep)
}

/// Reads the store and the container registry into the facts
/// [`classify_volumes`] decides on.
///
/// **A STOPPED container counts as a reference.** `volume_refs` already made
/// that call for `volumes rm` and it is the only defensible one here: a stopped
/// container is one `start` away from needing its data, and prune is a sweeper
/// of leftovers, not a way to discover that yesterday's database is gone.
pub(crate) fn volume_facts(store: &VolumeStore, scope: &Scope) -> Result<Vec<VolumeFacts>> {
    let mut out = Vec::new();
    for owned in store.list_all()? {
        let in_scope = scope.covers(owned.namespace.as_deref());
        let v = &owned.volume;
        // The reference test has to run against the store the volume ACTUALLY
        // lives in: `volume_refs` starts with `store.inspect(name)`, and an
        // unscoped store cannot inspect a namespaced volume. It would return
        // `NotFound`, hence no references, hence "unreferenced" — and this
        // function's answer is what decides whether data is destroyed. An
        // owner's volume must be inspected through its owner's store.
        let scoped;
        let home = match &owned.namespace {
            Some(ns) if in_scope => {
                scoped = store.scoped(ns)?;
                &scoped
            }
            _ => store,
        };
        // CONTAINERS only. `volume_refs` also reports the shares carved out of
        // this volume, and folding those in here would send a share's parent
        // down the silent `InUse` path — see `classify_volumes`. The container
        // scan behind it is global and matches by RESOLVED mountpoint, so a
        // container in any namespace still counts as a reference — which is
        // the point: ownership decides what a sweep may consider, never
        // whether someone else's running workload gets its data deleted.
        //
        // Only for what the scope may actually take: an out-of-scope volume is
        // carried for its PATH alone (parent/child derivation), and the
        // container scan behind this is the expensive part of the sweep.
        let referenced = in_scope
            && !super::volume::volume_refs(home, &v.name)
                .containers
                .is_empty();
        out.push(VolumeFacts {
            name: v.name.clone(),
            namespace: owned.namespace.clone(),
            in_scope,
            mountpoint: v.mountpoint.clone(),
            driver: v.driver.clone(),
            provisioned: v
                .annotations
                .contains_key(super::provision::PROVENANCE_ANNOTATION),
            referenced,
        });
    }
    Ok(out)
}

/// The owners that hold at least one volume, for the report that tells an
/// operator what a default `volumes prune` did NOT look at.
///
/// Rule 1 of the reaper rules cuts both ways: a sweep must refuse what is not
/// in scope, AND it must say what it refused. A prune that silently walks past
/// every tenant's data reads as "the store is clean" — which is exactly how a
/// tenant's volumes survived every prune ever run on the host that eventually
/// gave up 141 GB by hand, 46 of them volumes.
pub(crate) fn volume_owners(store: &VolumeStore) -> Result<Vec<String>> {
    // Derived from the volumes themselves, NOT from `namespaces()`: that one
    // lists DIRECTORIES, and a namespace's directory outlives the last volume
    // in it (a sweep removes the volume, not the empty sub-tree). Built on it,
    // this would keep naming a tenant as an owner of volumes after its last one
    // was reclaimed — the report would be false in exactly the direction that
    // sends someone looking for disk that is already free.
    let mut out: Vec<String> = store
        .list_all()?
        .into_iter()
        .filter_map(|o| o.namespace)
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// The already-translated reason a volume was kept, for the line printed next
/// to it. `InUse` never reaches here: every attached volume is kept, and
/// listing them all would bury the three that look prunable and are not.
pub(crate) fn keep_reason(k: &Keep) -> Option<String> {
    match k {
        Keep::InUse => None,
        Keep::ShareOf(parent) => Some(po::tf(
            "a share carved out of volume '{parent}' — remove the parent, or use `volumes rm`",
            &[("parent", parent)],
        )),
        Keep::HoldsShares(children) => Some(po::tf(
            "its tree holds the share volume(s) {list} — remove those first",
            &[("list", &children.join(", "))],
        )),
        Keep::Provisioned => Some(
            po::t(
                "provisioned on a remote NAS — use `volumes rm [--destroy-remote]` so the dataset \
                 is not left orphaned",
            )
            .to_string(),
        ),
        Keep::NetworkDriver(d) => Some(po::tf(
            "network driver '{driver}' — declared infrastructure, remove it with `volumes rm`",
            &[("driver", d)],
        )),
    }
}

/// What [`sweep_volumes`] reclaimed.
#[derive(Default)]
pub(crate) struct VolumeSweep {
    pub removed: Vec<String>,
    pub freed: Reclaimed,
}

/// Destroys the volumes [`classify_volumes`] cleared, and nothing else.
///
/// Order inside the loop: measure, then destroy the data, then the record —
/// the accounting goes LAST. Taking the record down first and then failing on
/// the data is how the v0.37.0 audit found volumes that had vanished from `ls`
/// while their bytes stayed on disk, ready to be handed to whoever created the
/// next volume of the same name.
pub(crate) fn sweep_volumes(store: &VolumeStore, take: &[VolumeFacts]) -> VolumeSweep {
    let mut out = VolumeSweep::default();
    for v in take {
        let size = measure(std::path::Path::new(&v.mountpoint));
        // Removal goes through the OWNER's store, for the same reason the
        // reference test did: `remove_with` resolves `<root>/<name>`, and the
        // unscoped root does not contain a namespaced volume. Called with the
        // wrong store it returns `NotFound` — the volume survives while the
        // report says it was swept.
        let scoped;
        let home = match &v.namespace {
            Some(ns) => match store.scoped(ns) {
                Ok(s) => {
                    scoped = s;
                    &scoped
                }
                Err(e) => {
                    eprintln!(
                        "{}",
                        po::tf(
                            "volume '{name}' could not be removed: {err}",
                            &[("name", &v.qualified()), ("err", &e.to_string())],
                        )
                    );
                    continue;
                }
            },
            None => store,
        };
        match home.remove_with(&v.name, Some(&delonix_runtime::remove_tree_mapped)) {
            Ok(()) => {
                out.freed.add(size);
                out.removed.push(v.qualified());
                // The owner goes in the trail (reaper rule 5: leave a trace).
                // "volume `data` removed by prune" is not answerable a week
                // later; "whose `data`" is the whole question.
                delonix_runtime_core::events::emit(
                    &super::util::state_root(),
                    "volume",
                    "remove",
                    &v.name,
                    &v.name,
                    Some(&format!("prune owner={}", v.owner())),
                );
            }
            Err(e) => {
                // Never silent: a volume that survived the sweep has to say so,
                // or the count reads as "everything unreferenced is gone".
                eprintln!(
                    "{}",
                    po::tf(
                        "volume '{name}' could not be removed: {err}",
                        &[("name", &v.qualified()), ("err", &e.to_string())],
                    )
                );
            }
        }
    }
    out
}

/// **PURE** — subdirectories (name = id) of `containers_dir` whose id is NOT in
/// `live` (registered containers): the orphans to reap. Isolated from
/// `remove_tree_mapped` (which needs subuid) so it can be tested dry, without
/// privilege. Only directories count — registry entries are `<id>.json` files
/// and never enter here. **It never returns a live id.**
pub(crate) fn orphan_container_dirs(
    containers_dir: &std::path::Path,
    live: &HashSet<String>,
) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(containers_dir) {
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = e.file_name().to_string_lossy().into_owned();
            if !live.contains(&id) {
                out.push(e.path());
            }
        }
    }
    out
}

/// The suffixes a VM's state carries in `vms/`, longest first.
///
/// Order is load-bearing: `micro.sock.lock` has to yield `micro`, and a
/// shortest-first scan would strip `.lock` and leave `micro.sock`.
const VM_STATE_SUFFIXES: &[&str] = &[
    ".sock.lock",
    ".console",
    ".qcow2",
    ".json",
    ".sock",
    ".xml",
    ".pid",
    ".log",
];

/// **PURE** — the VM name an entry of `vms/` belongs to, and whether the shape
/// is one prune is allowed to reason about at all.
///
/// `None` means "not a recognised piece of VM state": a bare directory, or
/// anything else that landed here. Those are never doomed by name — see
/// [`classify_vm_debris`].
pub(crate) fn vm_state_owner(entry: &str) -> Option<&str> {
    // `.NAME.lock` — the create lock, the one shape with the name in the middle.
    if let Some(rest) = entry.strip_prefix('.') {
        // A non-empty stem, or nothing: `..lock` would otherwise yield `""`,
        // and an empty needle makes every `refs.contains` test true.
        return rest.strip_suffix(".lock").filter(|stem| !stem.is_empty());
    }
    VM_STATE_SUFFIXES
        .iter()
        .find_map(|suf| entry.strip_suffix(suf))
        .filter(|stem| !stem.is_empty())
}

/// **PURE** — splits the entries of the VM state directory into the ones prune
/// may take and the ones it must leave alone.
///
/// Three independent tests have to agree before an entry is doomed, and the
/// reason there are three is a measurement, not caution in the abstract. On the
/// host this was written against, `vms/` held 63 entries against 17 VMs; a
/// name-based sweep would have called `hadata`, `labdata` and `pbs` orphans and
/// destroyed **53 GiB of live data** — three ZFS disks belonging to the
/// `pve-ha-*` cluster, a NAS disk, and the substrate's backup server. Not one
/// of them is named after a VM, because none of them IS a VM: they are extra
/// disks that live records point INTO.
///
/// So:
/// 1. the entry's owning name must not be a VM in the registry;
/// 2. its name must not appear anywhere in a live VM's record (`refs`), which
///    is what catches a directory whose name looks like nothing in particular;
/// 3. its shape must be recognised VM state — [`vm_state_owner`] — or, for a
///    plain directory, a `NAME.json`/`NAME.qcow2` sibling must prove that a VM
///    called `NAME` once existed. A lone directory is NEVER swept.
///
/// Test 2 alone would be enough if every record parsed and every path were
/// spelled the same way. Test 3 is there for the day one does not.
pub(crate) fn classify_vm_debris(
    entries: &[String],
    live: &HashSet<String>,
    refs: &str,
    stems_with_record: &HashSet<String>,
) -> Vec<String> {
    let mut doomed: Vec<String> = entries
        .iter()
        .filter(|e| {
            let owner = vm_state_owner(e);
            // A plain directory is only ever debris when a sibling record names
            // the same VM; on its own it is somebody's data.
            let name = match owner {
                Some(o) => o,
                None => {
                    if stems_with_record.contains(*e) {
                        e.as_str()
                    } else {
                        return false;
                    }
                }
            };
            !live.contains(name) && !refs.contains(name)
        })
        .cloned()
        .collect();
    doomed.sort();
    doomed
}

/// The entries of `vms/` a prune would remove — the preview that turns a blind
/// `[y/N]` into an informed one, and the single place the decision is made.
///
/// `sweep_vms` calls exactly this: a preview computed by a second code path is
/// a preview that can lie about what the sweep is about to do.
pub(crate) fn doomed_vm_entries(base: &std::path::Path) -> Result<Vec<String>> {
    let dir = base.join("vms");
    let live: HashSet<String> = delonix_vm::list(base)?
        .into_iter()
        .map(|v| v.name)
        .collect();

    // Every live record, concatenated. A candidate whose name appears anywhere
    // in here is pointed at by a VM that still exists — an extra disk, a seed
    // ISO, a console log — and is not ours to take.
    let mut refs = String::new();
    for name in &live {
        for ext in ["json", "xml"] {
            if let Ok(t) = std::fs::read_to_string(dir.join(format!("{name}.{ext}"))) {
                refs.push_str(&t);
                refs.push('\n');
            }
        }
    }

    let entries: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let stems_with_record: HashSet<String> = entries
        .iter()
        .filter(|e| e.ends_with(".json") || e.ends_with(".qcow2"))
        .filter_map(|e| vm_state_owner(e).map(str::to_string))
        .collect();

    Ok(classify_vm_debris(
        &entries,
        &live,
        &refs,
        &stems_with_record,
    ))
}

/// What [`sweep_vms`] reclaimed.
#[derive(Default)]
pub(crate) struct VmSweep {
    pub entries: usize,
    pub vms: usize,
    pub freed: Reclaimed,
}

/// Everything in `vms/` that no VM record accounts for — and, with `stopped`,
/// the stopped VMs themselves.
///
/// The default is deliberately NOT `container prune`'s. A stopped container is
/// a finished process; a stopped VM is the normal resting state of a machine
/// somebody built, and on the host this was written against **every one of the
/// 17 VMs was stopped**. Sweeping them by default would have been a `rm -rf` of
/// the whole lab wearing the name of a cleanup command. `--stopped` is
/// therefore opt-in, and even then goes through `vm rm`'s own removal path so
/// the backend gets its cleanup.
pub(crate) fn sweep_vms(base: &std::path::Path, stopped: bool) -> Result<VmSweep> {
    let mut out = VmSweep::default();
    let dir = base.join("vms");
    let vms = delonix_vm::list(base)?;

    for entry in doomed_vm_entries(base)? {
        let p = dir.join(&entry);
        let sz = measure(&p);
        let gone = if p.is_dir() {
            std::fs::remove_dir_all(&p).is_ok()
        } else {
            std::fs::remove_file(&p).is_ok()
        };
        if gone {
            out.entries += 1;
            out.freed.add(sz);
        }
    }

    if stopped {
        for vm in vms {
            if !matches!(vm.status, delonix_runtime_core::Status::Running) {
                let sz = measure(&dir.join(format!("{}.qcow2", vm.name)));
                if delonix_vm::remove(base, &vm.name).is_ok() {
                    out.vms += 1;
                    out.freed.add(sz);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {

    fn set(items: &[&str]) -> std::collections::HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn o_dono_de_um_ficheiro_de_estado_tira_o_sufixo_mais_longo_primeiro() {
        assert_eq!(super::vm_state_owner("micro.sock.lock"), Some("micro"));
        assert_eq!(super::vm_state_owner("micro.sock"), Some("micro"));
        assert_eq!(super::vm_state_owner("lab-dns.qcow2"), Some("lab-dns"));
        assert_eq!(super::vm_state_owner(".pve2.lock"), Some("pve2"));
        // A bare directory is not VM state by shape — it needs a sibling record.
        assert_eq!(super::vm_state_owner("hadata"), None);
        // `.lock`/`..lock` name no VM — an empty stem is not a name.
        assert_eq!(super::vm_state_owner(".lock"), None);
        assert_eq!(super::vm_state_owner("..lock"), None);
    }

    /// REGRESSION GUARD, and the reason this classifier has three tests instead
    /// of one. On the host `vm prune` was written against, `vms/` held three
    /// directories — `hadata` (28 GiB of `pve-ha-*` ZFS disks), `labdata`
    /// (a NAS disk) and `pbs` (the substrate's backup server) — none named
    /// after a VM. A name-based sweep calls all three orphans and deletes
    /// 53 GiB of live data.
    #[test]
    fn uma_pasta_de_dados_citada_por_um_registo_vivo_nunca_e_podada() {
        let entries: Vec<String> = ["hadata", "labdata", "pve-ha-1.json", ".morta.lock"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let live = set(&["pve-ha-1"]);
        let refs = "<disk file='/home/w/.local/share/delonix/vms/hadata/pve-ha-1-zfs.qcow2'/>";
        // `labdata` is in NO record here: it survives on shape alone, because a
        // lone directory with no sibling record is never swept.
        let doomed = super::classify_vm_debris(&entries, &live, refs, &set(&["pve-ha-1"]));
        assert_eq!(doomed, vec![".morta.lock".to_string()]);
    }

    #[test]
    fn uma_vm_viva_guarda_todo_o_seu_proprio_estado() {
        let entries: Vec<String> = ["micro.sock", "micro.pid", "micro.log", ".micro.lock"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let doomed = super::classify_vm_debris(&entries, &set(&["micro"]), "", &set(&[]));
        assert!(doomed.is_empty(), "{doomed:?}");
    }

    #[test]
    fn o_overlay_e_o_registo_de_uma_vm_morta_saem_juntos() {
        let entries: Vec<String> = ["morta.qcow2", "morta.json", "morta", "viva.qcow2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let doomed = super::classify_vm_debris(
            &entries,
            &set(&["viva"]),
            "",
            // `morta` has a record sibling, so its directory is sweepable too.
            &set(&["morta", "viva"]),
        );
        assert_eq!(doomed, vec!["morta", "morta.json", "morta.qcow2"]);
    }
    use super::*;

    fn v(name: &str, mount: &str) -> VolumeFacts {
        VolumeFacts {
            name: name.into(),
            namespace: None,
            mountpoint: mount.into(),
            driver: "local".into(),
            provisioned: false,
            referenced: false,
            in_scope: true,
        }
    }

    /// O mesmo, mas pertencente a um inquilino.
    fn vns(ns: &str, name: &str, mount: &str) -> VolumeFacts {
        let mut f = v(name, mount);
        f.namespace = Some(ns.into());
        f
    }

    #[test]
    fn um_volume_local_sem_referencias_e_podavel() {
        let (take, keep) = classify_volumes(&[v("solto", "/root/volumes/solto/_data")]);
        assert_eq!(take.len(), 1);
        assert_eq!(take[0].name, "solto");
        assert!(keep.is_empty());
    }

    /// O caso que este comando existe para NÃO fazer: um container parado
    /// continua a um `start` de precisar dos dados.
    #[test]
    fn um_volume_referenciado_nunca_e_podado() {
        let mut a = v("dados", "/root/volumes/dados/_data");
        a.referenced = true;
        let (take, keep) = classify_volumes(&[a]);
        assert!(take.is_empty());
        assert_eq!(keep[0].1, Keep::InUse);
        // In-use não imprime linha — senão todos os volumes ligados enchiam o ecrã.
        assert!(keep_reason(&keep[0].1).is_none());
    }

    /// Um `kind: ShareVolume` é um subdirectório REAL do Storage pai, e NENHUM
    /// dos dois lados é podável: podar a filha apaga os dados de um inquilino,
    /// podar o pai apaga a árvore que os contém.
    ///
    /// A asserção que interessa é a do PAI: antes desta correcção ele voltava
    /// como `InUse` (porque o `volume_refs` conta as shares) e essa é a variante
    /// que não imprime linha nenhuma — desaparecia do relatório em silêncio.
    #[test]
    fn nem_a_share_nem_o_seu_pai_sao_podados_e_os_dois_dizem_porque() {
        let pai = v("nas", "/root/volumes/nas/_data");
        let filha = v("nas-teamA", "/root/volumes/nas/_data/teamA");
        let (take, keep) = classify_volumes(&[pai, filha]);
        assert!(take.is_empty(), "nem o pai nem a filha podem ser podados");
        assert_eq!(keep.len(), 2);
        let pai = keep.iter().find(|(v, _)| v.name == "nas").unwrap();
        assert_eq!(pai.1, Keep::HoldsShares(vec!["nas-teamA".into()]));
        let filha = keep.iter().find(|(v, _)| v.name == "nas-teamA").unwrap();
        assert_eq!(filha.1, Keep::ShareOf("nas".into()));
        // Os DOIS têm de imprimir razão: um volume local e sem containers que
        // não é levado, sem uma linha a explicar, lê-se como um prune partido.
        assert!(keep_reason(&pai.1).is_some());
        assert!(keep_reason(&filha.1).is_some());
    }

    /// `/vol/data2` NÃO está dentro de `/vol/data`. Com `str::starts_with`
    /// estaria, e um volume perfeitamente podável seria guardado para sempre
    /// como se fosse filho de outro.
    #[test]
    fn o_prefixo_de_caminho_e_por_componente_e_nao_por_texto() {
        let (take, keep) = classify_volumes(&[v("data", "/vol/data"), v("data2", "/vol/data2")]);
        assert_eq!(take.len(), 2);
        assert!(keep.is_empty());
    }

    /// O registo local é a ÚNICA coisa que diz qual dataset em qual appliance
    /// pertence a este volume: apagá-lo deixa um dataset órfão do outro lado.
    #[test]
    fn um_volume_provisionado_numa_nas_nunca_e_podado() {
        let mut a = v("nas-dados", "/root/volumes/nas-dados/_data");
        a.provisioned = true;
        let (take, keep) = classify_volumes(&[a]);
        assert!(take.is_empty());
        assert_eq!(keep[0].1, Keep::Provisioned);
    }

    /// A fórmula é a do `df`, e a diferença NÃO é cosmética: os blocos
    /// reservados ao root contam como ocupados para quem não é root. Um motor
    /// rootless nunca lhes toca, portanto contá-los como espaço livre reporta
    /// um número mais BAIXO do que o operador vê — e o limiar dele foi lido do
    /// `df`.
    #[test]
    fn a_ocupacao_e_a_que_o_df_reporta_nao_a_ingenua() {
        // 1000 blocos, 100 livres, 50 disponíveis a quem não é root.
        // df: usados=900, utilizáveis=950 → 95%. A ingénua daria 90%.
        assert_eq!(used_pct(1000, 100, 50), 95);
    }

    /// Arredonda para CIMA: um disco com 0,4% livre não se lê como 99%.
    #[test]
    fn a_percentagem_arredonda_para_cima() {
        assert_eq!(used_pct(1000, 4, 4), 100);
        assert_eq!(used_pct(1000, 996, 996), 1, "1 bloco usado já não é 0%");
    }

    #[test]
    fn os_extremos_nao_saem_da_gama() {
        assert_eq!(used_pct(1000, 1000, 1000), 0, "vazio");
        assert_eq!(used_pct(1000, 0, 0), 100, "cheio");
        // Um sistema de ficheiros sem blocos utilizáveis lê-se como CHEIO, que
        // é a leitura que faz uma varredura correr em vez de saltar.
        assert_eq!(used_pct(0, 0, 0), 100);
        // Contadores que não batem certo não podem produzir mais de 100 nem
        // dar a volta: `bfree > blocks` satura em ZERO usados, e com blocos
        // ainda disponíveis isso lê-se como um disco vazio.
        assert_eq!(used_pct(10, 100, 50), 0);
        // Mas sem NADA disponível ganha o ramo de cima — cheio, não vazio: é a
        // leitura que faz a varredura correr em vez de saltar.
        assert_eq!(used_pct(10, 100, 0), 100);
    }

    /// O âmbito é o filtro, e um filtro que não distingue «sem dono» de
    /// «do inquilino `default`» apaga os dados errados. São dois sítios
    /// diferentes no disco e têm de continuar a ser duas respostas diferentes.
    #[test]
    fn o_ambito_distingue_a_raiz_sem_dono_do_inquilino_default() {
        assert!(Scope::Unowned.covers(None));
        assert!(!Scope::Unowned.covers(Some("default")));
        assert!(!Scope::Unowned.covers(Some("acme")));

        let acme = Scope::Namespace("acme".into());
        assert!(acme.covers(Some("acme")));
        assert!(!acme.covers(Some("acme2")), "nem por prefixo");
        assert!(!acme.covers(None), "a raiz não é de ninguém");

        assert!(Scope::Everything.covers(None));
        assert!(Scope::Everything.covers(Some("acme")));
    }

    /// O buraco que esta feature fecha: o volume de um inquilino ENTRA na
    /// classificação, com o dono agarrado, em vez de ficar invisível.
    #[test]
    fn o_volume_de_um_inquilino_e_podavel_e_diz_de_quem_era() {
        let (take, keep) =
            classify_volumes(&[vns("acme", "pgdata", "/root/volumes/.ns/acme/pgdata/_data")]);
        assert_eq!(take.len(), 1);
        assert!(keep.is_empty());
        assert_eq!(take[0].owner(), "acme");
        // Num relatório que atravessa donos, o nome nu é ambíguo.
        assert_eq!(take[0].qualified(), "acme/pgdata");
    }

    /// A identidade é o MOUNTPOINT, não o nome. Dois inquilinos escolhem
    /// `data` — comparar por nome faria cada um passar por «eu próprio» na
    /// derivação pai/filho do outro, e um `Storage` partilhado deixaria de ser
    /// reconhecido como pai de uma share do vizinho.
    #[test]
    fn dois_inquilinos_com_o_mesmo_nome_nao_se_confundem() {
        let a = vns("acme", "data", "/root/volumes/.ns/acme/data/_data");
        let b = vns("globex", "data", "/root/volumes/.ns/globex/data/_data");
        let (take, keep) = classify_volumes(&[a, b]);
        assert_eq!(take.len(), 2, "nenhum é filho do outro");
        assert!(keep.is_empty());
        let mut donos: Vec<&str> = take.iter().map(|v| v.owner()).collect();
        donos.sort();
        assert_eq!(donos, ["acme", "globex"]);
    }

    /// E a prova de que a correcção da identidade não foi só cosmética: com o
    /// nome como identidade, a share do `acme` NÃO seria vista como filha do
    /// `Storage` homónimo do `globex`... mas a do MESMO nome dentro da sua
    /// própria árvore passaria despercebida. Aqui o pai e a filha chamam-se
    /// ambos `nas` e a relação tem de ser detectada na mesma.
    #[test]
    fn pai_e_filha_com_o_mesmo_nome_em_donos_diferentes_sao_detectados() {
        let pai = vns("acme", "nas", "/root/volumes/.ns/acme/nas/_data");
        let filha = vns("acme", "nas", "/root/volumes/.ns/acme/nas/_data/teamA");
        let (take, keep) = classify_volumes(&[pai, filha]);
        assert!(take.is_empty(), "nem o pai nem a filha podem ser podados");
        assert_eq!(keep.len(), 2);
        assert!(keep.iter().any(|(_, k)| matches!(k, Keep::HoldsShares(_))));
        assert!(keep.iter().any(|(_, k)| matches!(k, Keep::ShareOf(_))));
    }

    /// **O caminho de perda de dados que o âmbito por dono quase abriu.**
    ///
    /// Um `kind: ShareVolume` é registado na sub-árvore do SEU inquilino, mas o
    /// `Storage` pai fica na raiz sem dono — é o mount da NAS, infraestrutura
    /// do nó (`sharevolume::apply_one` diz isto por escrito) — e os dados da
    /// share vivem DENTRO da árvore do pai.
    ///
    /// Se o âmbito filtrasse a lista ANTES da derivação, `--namespace acme`
    /// veria a share sozinha, não lhe encontraria pai nenhum, e apagaria dados
    /// na NAS. O âmbito decide o que se LEVA; nunca o que se OLHA.
    #[test]
    fn uma_share_do_inquilino_nao_perde_o_pai_por_ele_estar_fora_do_ambito() {
        let mut pai = v("nas", "/root/volumes/nas/_data");
        pai.in_scope = false; // a raiz não está no âmbito de `--namespace acme`
        let filha = vns("acme", "db", "/root/volumes/nas/_data/shares/acme/db");

        let (take, keep) = classify_volumes(&[pai, filha]);
        assert!(
            take.is_empty(),
            "a share foi levada — os dados na NAS seriam destruídos"
        );
        assert_eq!(keep.len(), 1, "o pai fora do âmbito não se reporta");
        assert_eq!(keep[0].0.qualified(), "acme/db");
        assert_eq!(keep[0].1, Keep::ShareOf("nas".into()));
    }

    /// E o simétrico: um volume fora do âmbito não aparece no relatório como
    /// «mantido». Listar os volumes dos outros inquilinos é ruído, e pior —
    /// lê-se como se tivessem sido considerados.
    #[test]
    fn o_que_esta_fora_do_ambito_nao_entra_no_relatorio() {
        let mut outro = vns("globex", "pgdata", "/root/volumes/.ns/globex/pgdata/_data");
        outro.in_scope = false;
        let meu = vns("acme", "pgdata", "/root/volumes/.ns/acme/pgdata/_data");

        let (take, keep) = classify_volumes(&[outro, meu]);
        assert_eq!(take.len(), 1);
        assert_eq!(take[0].qualified(), "acme/pgdata");
        assert!(keep.is_empty());
    }

    /// Um container de OUTRO inquilino a montar este volume conta como
    /// referência. A posse decide o que a varredura pode CONSIDERAR; nunca
    /// autoriza apagar dados debaixo de uma carga viva de outrem.
    #[test]
    fn a_referencia_de_outro_inquilino_protege_o_volume() {
        let mut a = vns("acme", "dados", "/root/volumes/.ns/acme/dados/_data");
        a.referenced = true;
        let (take, keep) = classify_volumes(&[a]);
        assert!(take.is_empty());
        assert_eq!(keep[0].1, Keep::InUse);
    }

    #[test]
    fn um_volume_de_rede_e_infraestrutura_declarada_e_fica() {
        let mut a = v("export", "/root/volumes/export/_data");
        a.driver = "nfs".into();
        let (take, keep) = classify_volumes(&[a]);
        assert!(take.is_empty());
        assert_eq!(keep[0].1, Keep::NetworkDriver("nfs".into()));
    }

    /// A medição incompleta não pode passar por número exacto: em rootless o
    /// rootfs é de um subuid e um `du` de fora lê zero onde há gigabytes.
    #[test]
    fn uma_medicao_incompleta_imprime_se_como_limite_inferior() {
        let exacta = Reclaimed {
            bytes: 1024,
            partial: false,
        };
        let parcial = Reclaimed {
            bytes: 1024,
            partial: true,
        };
        assert!(!exacta.fmt().starts_with('≥'));
        assert!(parcial.fmt().starts_with('≥'));
    }

    #[test]
    fn a_soma_de_uma_medicao_parcial_contamina_o_total() {
        let mut total = Reclaimed::default();
        total.add(Reclaimed {
            bytes: 10,
            partial: false,
        });
        total.add(Reclaimed {
            bytes: 5,
            partial: true,
        });
        assert_eq!(total.bytes, 15);
        assert!(total.partial);
    }

    /// A root that is not the default NEVER gets to claim the shared ingress.
    ///
    /// The store is per-`DELONIX_ROOT` and the slirp api-socket is per-UID: from
    /// a test root the live-ports set is empty while the socket still serves the
    /// real one, so asserting authority there would reap every published port on
    /// the machine as an orphan. Fails the moment the guard becomes a constant.
    #[test]
    fn so_o_root_default_pode_reclamar_o_ingress_partilhado() {
        assert!(owns_shared_ingress_at(&ImageStore::default_root()));
        assert!(!owns_shared_ingress_at(std::path::Path::new(
            "/tmp/um-root-de-teste"
        )));
    }

    /// Unique temp dir (without depending on the `tempfile` crate).
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        // SAFETY: getpid() has no preconditions.
        let uniq = format!(
            "delonix-prune-{tag}-{}-{}",
            unsafe { libc::getpid() },
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let d = std::env::temp_dir().join(uniq);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// STRESS test of the orphan-rootfs reaper: create→destroy of N container
    /// directories at the disk level, crossed with the "Store" (set of live
    /// ids). Asserts that the reaper catches ALL the orphans (containers killed
    /// without `rm`), preserves the live ones, and that after deleting them ZERO
    /// orphans remain. Runs without privilege — it tests the DECISION
    /// (`orphan_container_dirs`), not `remove_tree_mapped` (which needs subuid).
    #[test]
    fn stress_reaper_rootfs_orfaos_deixa_zero() {
        const N: usize = 300;
        let root = tmp_dir("rootfs");
        let containers = root.join("containers");
        std::fs::create_dir_all(&containers).unwrap();

        // N dead container directories + M live ones, and some `<id>.json`
        // files (registry entries) that are NOT directories and must be
        // ignored by the reaper.
        for i in 0..N {
            std::fs::create_dir_all(containers.join(format!("dead{i}"))).unwrap();
        }
        let live: HashSet<String> = (0..5).map(|i| format!("alive{i}")).collect();
        for id in &live {
            let d = containers.join(id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("rootfs-marker"), b"x").unwrap();
        }
        std::fs::write(containers.join("alive0.json"), b"{}").unwrap();
        std::fs::write(containers.join("dead0.json"), b"{}").unwrap();

        // The reaper sees exactly the N orphans (none live, no files).
        let orphans = orphan_container_dirs(&containers, &live);
        assert_eq!(
            orphans.len(),
            N,
            "todos os `dead*` são órfãos, ficheiros ignorados"
        );
        for id in &live {
            let p = containers.join(id);
            assert!(!orphans.contains(&p), "container vivo NUNCA é reapado");
        }

        // Delete them and reconfirm: ZERO orphans remain, the live ones intact.
        for p in &orphans {
            std::fs::remove_dir_all(p).unwrap();
        }
        assert!(
            orphan_container_dirs(&containers, &live).is_empty(),
            "após o reap, zero directórios órfãos"
        );
        for id in &live {
            assert!(containers.join(id).is_dir(), "vivo preservado no disco");
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
