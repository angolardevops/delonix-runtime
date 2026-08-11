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
        if id.starts_with("cri-") || id.starts_with("vm-") {
            live_refs.insert(id);
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
    pub mountpoint: String,
    pub driver: String,
    /// Carries `delonix.io/provisioned-by`: the local record is the ONLY thing
    /// that says which dataset on which appliance belongs to this volume.
    pub provisioned: bool,
    /// A CONTAINER references it, running or stopped (see [`volume_facts`]).
    /// Being a share's parent is NOT this — that is derived, and reported.
    pub referenced: bool,
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
        let mine = std::path::Path::new(&v.mountpoint);
        // Component-wise, never a string prefix: `/vol/data2` is not inside
        // `/vol/data`, and `starts_with` on `str` would say it is.
        let parent = vols
            .iter()
            .find(|o| o.name != v.name && mine.starts_with(std::path::Path::new(&o.mountpoint)));
        let children: Vec<String> = vols
            .iter()
            .filter(|o| o.name != v.name && std::path::Path::new(&o.mountpoint).starts_with(mine))
            .map(|o| o.name.clone())
            .collect();
        if v.referenced {
            keep.push((v.clone(), Keep::InUse));
        } else if let Some(p) = parent {
            keep.push((v.clone(), Keep::ShareOf(p.name.clone())));
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
pub(crate) fn volume_facts(store: &VolumeStore) -> Result<Vec<VolumeFacts>> {
    let mut out = Vec::new();
    for v in store.list()? {
        // CONTAINERS only. `volume_refs` also reports the shares carved out of
        // this volume, and folding those in here would send a share's parent
        // down the silent `InUse` path — see `classify_volumes`.
        let referenced = !super::volume::volume_refs(store, &v.name)
            .containers
            .is_empty();
        out.push(VolumeFacts {
            name: v.name.clone(),
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
        match store.remove_with(&v.name, Some(&delonix_runtime::remove_tree_mapped)) {
            Ok(()) => {
                out.freed.add(size);
                out.removed.push(v.name.clone());
                delonix_runtime_core::events::emit(
                    &super::util::state_root(),
                    "volume",
                    "remove",
                    &v.name,
                    &v.name,
                    Some("prune"),
                );
            }
            Err(e) => {
                // Never silent: a volume that survived the sweep has to say so,
                // or the count reads as "everything unreferenced is gone".
                eprintln!(
                    "{}",
                    po::tf(
                        "volume '{name}' could not be removed: {err}",
                        &[("name", &v.name), ("err", &e.to_string())],
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

#[cfg(test)]
mod tests {
    use super::*;

    fn v(name: &str, mount: &str) -> VolumeFacts {
        VolumeFacts {
            name: name.into(),
            mountpoint: mount.into(),
            driver: "local".into(),
            provisioned: false,
            referenced: false,
        }
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
