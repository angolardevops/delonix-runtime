//! Helpers shared by several command groups (`container`, `image`,
//! `build`) — state root, opening the stores, image resolution, and the
//! rootless-flat vs root-overlay logic for preparing the rootfs.

use std::path::{Path, PathBuf};

use delonix_image::{Image, ImageStore};
use delonix_runtime::{self as runtime};
use delonix_runtime_core::{Container, Error, Result, Store};

/// The runtime's state root: `$DELONIX_ROOT` or the `ImageStore` default.
pub(crate) fn state_root() -> PathBuf {
    std::env::var_os("DELONIX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(ImageStore::default_root)
}

pub(crate) fn open_stores() -> Result<(ImageStore, Store)> {
    let root = state_root();
    let images = ImageStore::open(&root)?;
    let store = Store::open(root.join("containers"))?;
    Ok((images, store))
}

/// Resolve a local image; if missing, pull it from the registry.
pub(crate) fn resolve_or_pull(images: &ImageStore, reference: &str) -> Result<Image> {
    resolve_or_pull_with_creds(images, reference, None)
}

/// Like [`resolve_or_pull`], with credentials supplied by the CALLER instead of
/// read from the machine's credential vault.
///
/// The vault (`delonix image login`) is per-MACHINE state: it works, and it is
/// exactly what a manifest cannot carry. A `kind: Image` naming a private
/// registry had no way to say which credential to use, so applying it on a
/// fresh host failed with an authentication error about a registry the manifest
/// never mentioned a credential for — the manifest was not self-contained, and
/// nothing in it said so.
///
/// `None` keeps the vault path byte-for-byte, which is what every existing
/// caller gets.
pub(crate) fn resolve_or_pull_with_creds(
    images: &ImageStore,
    reference: &str,
    creds: Option<(String, String)>,
) -> Result<Image> {
    match images.resolve(reference) {
        Ok(img) => Ok(img),
        Err(_) => {
            eprintln!("a puxar {reference}…");
            match creds {
                None => delonix_image::pull_from_registry(images, reference),
                Some(c) => delonix_image::registry::pull_from_registry_with_creds(
                    images,
                    reference,
                    Some(c),
                ),
            }
        }
    }
}

/// Like [`resolve_or_pull`], but arch-aware (`--platform`): `platform: None`
/// is byte-for-byte today's behavior (any local match wins, arch untouched).
/// `Some(arch)` only reuses a local match if its OWN recorded
/// `config.architecture` is that arch — a miss OR an arch mismatch always
/// forces a fresh registry pull for that arch. Deliberately conservative (a
/// network round-trip on any ambiguity) rather than half-implementing a
/// multi-arch-aware local cache index: a locally-tagged image silently
/// resolving to the WRONG arch under `--platform` would be a much worse
/// failure mode than an extra pull.
pub(crate) fn resolve_or_pull_platform(
    images: &ImageStore,
    reference: &str,
    platform: Option<&str>,
) -> Result<Image> {
    let Some(arch) = platform else {
        return resolve_or_pull(images, reference);
    };
    if let Ok(img) = images.resolve(reference) {
        if img.config.architecture == arch {
            return Ok(img);
        }
    }
    eprintln!("a puxar {reference} (linux/{arch})…");
    delonix_image::registry::pull_from_registry_with_creds_platform(
        images,
        reference,
        None,
        Some(arch),
    )
}

/// Effective command (pure function): ENTRYPOINT + (the user's args, otherwise the
/// image's CMD) — the same semantics as Docker/OCI (`run <cmd>` replaces the CMD, not
/// the ENTRYPOINT).
pub(crate) fn compose_command(
    entrypoint: &[String],
    cmd: &[String],
    user: &[String],
) -> Vec<String> {
    let mut v = entrypoint.to_vec();
    if user.is_empty() {
        v.extend(cmd.iter().cloned());
    } else {
        v.extend(user.iter().cloned());
    }
    v
}

/// Like [`compose_command`], but from the image's config.
pub(crate) fn effective_command(img: &Image, user: &[String]) -> Vec<String> {
    compose_command(&img.config.entrypoint, &img.config.cmd, user)
}

/// `chown -R <uid>:<uid>` of a FLAT rootfs (rootless): without this, the files
/// belong to the host's uid 0, which ends up unmapped inside the user namespace.
/// Delegates to `delonix_runtime::lchown_tree` (uses `lchown`, never follows symlinks —
/// see the security note there; don't reimplement this locally with
/// `std::os::unix::fs::chown`, which follows symlinks).
pub(crate) fn chown_tree(path: &Path, uid: u32) -> Result<()> {
    delonix_runtime::lchown_tree(path, uid, uid);
    Ok(())
}

/// Locates a container by ID prefix or by exact name.
///
/// BUG FOUND: this used to be `.find(...)` on the id-prefix/exact-name
/// predicate, returning the FIRST match from `store.list()` (created-desc
/// order) — an ambiguous short prefix silently resolved to the newest
/// matching container instead of erroring, unlike Docker/Podman ("multiple
/// IDs found"). Destructive verbs (`stop`/`rm`) all resolve through this, so
/// an ambiguous prefix could silently act on the wrong container. Fixed:
/// an exact id or exact name match wins outright (unambiguous by
/// definition); otherwise every prefix match is collected, and more than
/// one is a hard error listing the candidates.
pub(crate) fn find(store: &Store, q: &str) -> Result<Container> {
    let all = store.list()?;
    // `<namespace>/<name>` — the unambiguous form, needed since names are only
    // unique WITHIN a namespace (ADR-0011 §3). Tried first so a container whose
    // name legitimately contains no slash can never shadow it.
    if let Some((ns, name)) = q.split_once('/') {
        return all
            .into_iter()
            .find(|c| c.namespace == ns && c.name == name)
            .ok_or_else(|| Error::NotFound(format!("container: {q}")));
    }
    if let Some(c) = all.iter().find(|c| c.id == q) {
        return Ok(c.clone());
    }
    // A bare name may now match in SEVERAL namespaces — two tenants are allowed
    // to both own `db`. Picking one would be picking a tenant, and every
    // destructive verb (`stop`, `rm`) resolves through here, so this refuses and
    // names them instead. A name unique on the node — every node not using
    // namespaces — behaves exactly as before.
    let by_name: Vec<Container> = all.iter().filter(|c| c.name == q).cloned().collect();
    if by_name.len() == 1 {
        return Ok(by_name.into_iter().next().unwrap());
    }
    if by_name.len() > 1 {
        let mut opts: Vec<String> = by_name
            .iter()
            .map(|c| format!("{}/{}", c.namespace, c.name))
            .collect();
        opts.sort();
        return Err(Error::Invalid(format!(
            "{}: {q} ({})",
            super::po::t(
                "this name exists in several namespaces — qualify it as <namespace>/<name>"
            ),
            opts.join(", ")
        )));
    }
    let mut matches: Vec<Container> = all.into_iter().filter(|c| c.id.starts_with(q)).collect();
    match matches.len() {
        // BUG FOUND while classifying exit codes: this said `Error::Invalid`,
        // so EVERY container verb that resolves through here (`inspect`,
        // `stop`, `rm`, `logs`, `exec`, `describe`, `port`, `wait`, ...)
        // reported «it does not exist» as «your argument is wrong» and came
        // back as the generic exit 1 — while `volumes`/`network`/`secret`/`vm`
        // already answered `NotFound`. It also phrased the very same condition
        // differently from `Store::load`, which has always said
        // `no such container: <id>`. Both fixed by using that one wording.
        //
        // The AMBIGUOUS case below stays `Invalid` on purpose, and the
        // difference is the caller's next move: a prefix matching three
        // containers is a bad argument to fix, not a resource to create.
        0 => Err(Error::NotFound(format!("container: {q}"))),
        1 => Ok(matches.remove(0)),
        _ => {
            let ids: Vec<&str> = matches
                .iter()
                .map(|c| super::container::short_id(&c.id))
                .collect();
            Err(Error::Invalid(format!(
                "{}: {q} ({})",
                super::po::t("multiple containers match this prefix"),
                ids.join(", ")
            )))
        }
    }
}

/// Prepares a new container's rootfs from an image: an overlay over the SHARED
/// layer cache in both modes — mounted here in root mode, mounted by the
/// container's own init in rootless (see `ImageStore::prepare_overlay`). Same
/// rule used by `container run`.
///
/// Rootless used to take a FULL COPY of the image instead (`export_rootfs`), and
/// the cost was not marginal: measured on this host, 21 containers of the same
/// `kaeso-odoo:16` image held 21 separate physical copies of the same 2.1 GiB
/// tree — every file at `nlink == 1`, ~39 GiB of byte-identical duplication —
/// and every `run` paid 13 s of I/O to make one more. The layer cache under
/// `layers/<hex>/` was already shared and already had the ownership the
/// container's uid map wants; nothing pointed at it.
///
/// The `chown_tree(…, USERNS_UID_BASE)` that used to follow the copy is gone,
/// and it never did anything: `lchown` to uid 100000 from an unprivileged uid is
/// EPERM, and `lchown_tree` discards the error. Measured — both the extracted
/// layers and every flat rootfs on this host are uniformly `1000:1000`. They
/// work because the rootless map is `0 <euid> 1`, so uid 0 INSIDE the namespace
/// IS the invoking uid on the host, and the files already read as `root` to the
/// container. That is also what lets one extracted layer serve every container.
pub(crate) fn prepare_rootfs(images: &ImageStore, img: &Image, id: &str) -> Result<String> {
    let rootless = runtime::is_rootless();
    if rootless {
        // Does not mount — an unprivileged `mount(2)` on the host is EPERM. The
        // mount happens inside the clone, where we own the user namespace.
        Ok(images
            .prepare_overlay(img, id)?
            .to_string_lossy()
            .into_owned())
    } else {
        Ok(images.mount_rootfs(img, id)?.to_string_lossy().into_owned())
    }
}

/// Prepares a FLAT rootfs (full copy of the image) for a container whose tree
/// the HOST has to read and write directly. Kept for `build` alone.
///
/// A build's work container is the one case where the flat copy earns its cost:
/// `COPY` writes into the tree from the host, `FROM <stage>` clones it with
/// `cp -a`, and `commit_flat_rootfs` packs it — all from outside any namespace,
/// where an overlay mounted by the container's init is not visible. It is also
/// the case where duplication does not accumulate: the work container is
/// removed when the stage ends, unlike the long-lived containers that made 21
/// copies of the same image sit on this disk at once.
///
/// Do NOT reach for this for `run`. That is what [`prepare_rootfs`] is for, and
/// the difference between them is the 39 GiB.
pub(crate) fn prepare_rootfs_flat(images: &ImageStore, img: &Image, id: &str) -> Result<String> {
    if runtime::is_rootless() {
        let rfs = images.root().join("containers").join(id).join("rootfs");
        images.export_rootfs(img, &rfs)?;
        Ok(rfs.to_string_lossy().into_owned())
    } else {
        Ok(images.mount_rootfs(img, id)?.to_string_lossy().into_owned())
    }
}

/// The rootfs path of a container that was ALREADY prepared, across the three
/// layouts this engine has on disk, or `None` when nothing was prepared.
///
/// The order matters and is not arbitrary: the overlay marker is checked FIRST
/// because a container can carry both shapes at once — a flat `rootfs/` written
/// by a pre-overlay binary keeps living next to the `merged/` a newer `run`
/// would use, and picking the stale copy would start the container against a
/// tree that no longer receives its writes.
///
/// 1. `overlay-lowers` present → `merged/` (rootless overlay; the mount itself
///    happens inside the container's clone, so this path is an empty directory
///    out here and that is expected).
/// 2. `rootfs/` present → the legacy rootless flat copy. Kept working on
///    purpose: containers created by an older binary must survive the upgrade.
/// 3. `merged/` present → root mode, where the overlay is mounted on the host.
pub(crate) fn existing_rootfs_path(images: &ImageStore, id: &str) -> Option<PathBuf> {
    let base = images.root().join("containers").join(id);
    if base.join(ImageStore::LOWERS_FILE).exists() {
        return Some(base.join("merged"));
    }
    for cand in ["rootfs", "merged"] {
        let p = base.join(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Where a container's OWN writes live on disk — never the shared read-only
/// lower layers a `container ls -s` SIZE column must not double-count across
/// every container built from the same image. Same three-layout precedence as
/// [`existing_rootfs_path`], but pointed at the writable layer instead of the
/// (empty-on-host) mountpoint:
/// 1. `overlay-lowers` present → `upper/` (the writable layer; `merged/` is
///    only the mountpoint, shared/empty on the host).
/// 2. `rootfs/` present → the legacy flat copy IS the container's own full
///    footprint (nothing shared to exclude).
pub(crate) fn container_writable_dir(images: &ImageStore, id: &str) -> Option<PathBuf> {
    let base = images.root().join("containers").join(id);
    if base.join(ImageStore::LOWERS_FILE).exists() {
        return Some(base.join("upper"));
    }
    let p = base.join("rootfs");
    p.exists().then_some(p)
}

/// Converts a container's legacy FLAT rootfs into a shared overlay, if it has
/// one. Best-effort: any failure leaves the container exactly as it was.
///
/// # Why here, and why it is allowed to give up
///
/// Containers created before the layers were shared each carry a full private
/// copy of the image. There is no way to convert one WHILE IT RUNS — the process
/// `pivot_root`ed into that tree and holds open files in it, so swapping the
/// root is by definition recreating the process. What there IS, is the stop that
/// already happened: a `start` is the one moment the tree belongs to nobody, and
/// this costs the operator no downtime they were not already paying.
///
/// Everything here is therefore **opportunistic**. A migration that cannot run
/// is not an error to report — the container starts flat, exactly as it did
/// yesterday. What it must never do is prevent a start: that would trade a
/// container that wastes disk for one that does not come back.
///
/// The image is resolved LOCALLY (`ImageStore::resolve`) and never pulled. The
/// lower layers have to exist to migrate against, but going to a registry —
/// slowly, or failing on a node with no route out — during a `start` that would
/// otherwise succeed is not a trade this optimisation gets to make.
///
/// Runs inside the mapped userns (`reexec_mapped`) for two independent reasons:
/// whiteouts need `CAP_MKNOD`, and a flat rootfs may hold files owned by mapped
/// SUBUIDS (anything the container wrote as a non-zero uid), which the invoking
/// uid can neither move nor delete.
pub(crate) fn migrate_flat_to_overlay(images: &ImageStore, id: &str, image: &str) {
    let dir = images.root().join("containers").join(id);
    // Already shared, or nothing flat to convert. Also the fast path for every
    // ordinary start, which is why it is two `exists()` and no I/O beyond that.
    if !dir.join("rootfs").exists() || dir.join(ImageStore::LOWERS_FILE).exists() {
        return;
    }
    let Ok(img) = images.resolve(image) else {
        return; // image gone from the store — nothing to share against
    };
    let Ok(lowers) = images.lower_dirs(&img) else {
        return;
    };
    if lowers.is_empty() {
        return;
    }
    let body = lowers
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    let pending = dir.join("overlay-lowers.pending");
    if std::fs::write(&pending, body).is_err() {
        return;
    }
    let d = dir.to_string_lossy().into_owned();
    if delonix_runtime::reexec_mapped(&["__ovlmigrate", &d]) != Some(true) {
        // The helper reverts its own half; this only clears the marker it read,
        // so a later start finds the same clean starting point.
        let _ = std::fs::remove_file(&pending);
    }
}

/// Silences the engine's cgroup-delegation warning in every process spawned
/// from here on, on the grounds that it has ALREADY been printed once.
///
/// Call it only AFTER something has actually had the chance to warn. The
/// question «does this session have delegation» cannot be answered here and the
/// attempt to do so is a trap worth recording: `cgroup_limits_apply()` reports
/// `yes` in the CLI's own process while a container that re-execs into the
/// holder's userns has no delegation at all and warns — measured, the two
/// disagree on this very host. A guard that tested the parent and then silenced
/// the children would suppress a warning that is TRUE, which is strictly worse
/// than printing it three times.
///
/// So the process that knows is the one that tried: let the first child warn,
/// and silence its peers, which share its environment by construction.
///
/// SAFETY: callers are single-threaded at this point (between two container
/// spawns; a `Progress` spinner thread is joined when its step closes). The
/// value is read by re-exec'd CHILD processes, not by another thread here.
pub fn silence_cgroup_warning() {
    unsafe {
        std::env::set_var("DELONIX_NO_CGROUP_WARN", "1");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Six tests share this helper and cargo runs them in PARALLEL, in one
    /// process — so `pid` is the same for all of them and only the timestamp
    /// separates the directories. Two threads that read the clock in the same
    /// tick get the SAME path, and each test ends with `remove_dir_all`: one
    /// deletes the other's store mid-run, and the victim fails on whichever
    /// assertion it happened to reach. That is the shape of the CI flake seen
    /// on 2026-08-27 (`a_unique_bare_name_still_resolves_exactly_as_before`
    /// getting something other than `NotFound` for a name that was never
    /// there) — it passed on every local run, because losing that race needs
    /// the timing a loaded runner gives.
    ///
    /// A counter cannot tie. The clock is kept because it also separates one
    /// RUN from the next, which the counter alone would not.
    fn tmp_store() -> (Store, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "delonix-util-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        (Store::open(&dir).unwrap(), dir)
    }

    fn mk(id: &str, name: &str) -> Container {
        Container::new(
            id.to_string(),
            name.to_string(),
            "alpine".to_string(),
            vec!["sh".to_string()],
            "0".to_string(),
        )
    }

    fn mk_ns(id: &str, name: &str, ns: &str) -> Container {
        let mut c = mk(id, name);
        c.namespace = ns.to_string();
        c
    }

    #[test]
    fn a_name_in_two_namespaces_is_refused_not_guessed() {
        // Names are unique per (namespace, name), so two tenants may both own
        // `db`. Every destructive verb resolves through `find`, so picking one
        // would be picking a TENANT — `stop db` hitting the wrong team's
        // database. It refuses and names both instead.
        let (store, dir) = tmp_store();
        store.save(&mk_ns("aaa1", "db", "teamA")).unwrap();
        store.save(&mk_ns("bbb2", "db", "teamB")).unwrap();

        let err = find(&store, "db").unwrap_err().to_string();
        assert!(err.contains("teamA/db"), "must name the candidates: {err}");
        assert!(err.contains("teamB/db"), "must name the candidates: {err}");

        // The qualified form resolves each, exactly.
        assert_eq!(find(&store, "teamA/db").unwrap().id, "aaa1");
        assert_eq!(find(&store, "teamB/db").unwrap().id, "bbb2");
        // ...and the id still works, as always.
        assert_eq!(find(&store, "bbb2").unwrap().name, "db");
        // A qualified name that does not exist is NotFound, not ambiguity.
        assert!(matches!(find(&store, "teamC/db"), Err(Error::NotFound(_))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_unique_bare_name_still_resolves_exactly_as_before() {
        // Every node that does not use namespaces is this case; it must not
        // change at all.
        let (store, dir) = tmp_store();
        store.save(&mk_ns("aaa1", "web", "default")).unwrap();
        store.save(&mk_ns("bbb2", "api", "teamA")).unwrap();
        assert_eq!(find(&store, "web").unwrap().id, "aaa1");
        assert_eq!(find(&store, "api").unwrap().id, "bbb2");
        assert!(matches!(find(&store, "nope"), Err(Error::NotFound(_))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn find_prefixo_ambiguo_e_erro_nao_o_mais_recente() {
        // BUG regression guard: `find` used to silently return the FIRST
        // (newest-created) match on an ambiguous id prefix instead of
        // erroring — the exact opposite of Docker/Podman semantics.
        let (store, dir) = tmp_store();
        store.save(&mk("a1f3000000000000", "old")).unwrap();
        store.save(&mk("a1f9000000000000", "new")).unwrap();
        let err = find(&store, "a1").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("a1f300000000") || msg.contains("a1f900000000"),
            "{msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_prefixo_unico_resolve_normalmente() {
        let (store, dir) = tmp_store();
        store.save(&mk("a1f3000000000000", "old")).unwrap();
        store.save(&mk("b2000000000000000", "other")).unwrap();
        let c = find(&store, "a1f3").unwrap();
        assert_eq!(c.name, "old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_nome_exacto_ganha_mesmo_com_prefixo_de_id_ambiguo() {
        // An exact id/name match is unambiguous by definition and must win
        // outright, even if OTHER containers' ids happen to share a prefix
        // with the query string.
        let (store, dir) = tmp_store();
        store.save(&mk("a1f3000000000000", "web")).unwrap();
        store.save(&mk("a1f9000000000000", "other")).unwrap();
        let c = find(&store, "web").unwrap();
        assert_eq!(c.id, "a1f3000000000000");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
