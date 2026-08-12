//! `infra` — manager of the rootless ingress's **infrastructure netns** (Phase 1).
//!
//! Eventually replaces the `1 slirp4netns per container` model with a **single
//! ingress**: a shared infra netns, with the `delonix0` bridge inside it, ONE
//! `slirp4netns` as a host↔infra bridge, and the NAT/DNAT in `nft` INSIDE the netns.
//! Containers attach by `veth` to `delonix0` (Phase 3) and ports are published
//! via `add_hostfwd` + DNAT (Phase 4). This phase delivers only the **manager**: bring up,
//! observe and tear down the infra, with a lifecycle *ref-count*.
//!
//! **Why it's rootless:** a non-root is root INSIDE its own user+network
//! namespace → it has `CAP_NET_ADMIN` there and can create a bridge and `nft` rules. The netns
//! lives as long as the *holder* process lives; it's discovered by PID (host-visible).
//!
//! **Known gotcha:** you CANNOT `nsenter --user --net` from the host
//! (it gives `setgroups: Operation not permitted`). So all the configuration INSIDE the
//! netns is done by the holder itself (already root in the userns) — hence the re-exec of the
//! binary to the holder entry point (`netns holder`/`netns pin`, intercepted in
//! the bin's `main()` before clap parses anything).

use crate::{run, run_ok, SLIRP_IP};
use delonix_runtime_core::peer_cred::peer_uid;
use delonix_runtime_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Bridge inside the infra netns (same name as the root model; doesn't collide
/// because it's in another netns).
pub const INFRA_BRIDGE: &str = "delonix0";
/// Gateway/IP of the bridge in the infra netns.
pub const INFRA_GATEWAY: &str = "10.200.0.1";
/// CIDR of the bridge in the infra netns (containers land in `10.200.x/16`).
pub const INFRA_CIDR: &str = "10.200.0.1/16";
/// `/16` prefix of the infra subnet (to validate container IPs).
pub const INFRA_PREFIX: &str = "10.200";
/// Subnet of the single slirp's `tap0` (its host↔infra side), target of the masquerade.
pub const INFRA_TAP_SUBNET: &str = "10.0.2.0/24";
/// The ingress's `nft` table, LIVES INSIDE the infra netns (distinct from the root
/// mode's `delonix`, which lives in the host's netns).
pub const INGRESS_TABLE: &str = "dlxing";

// ---- artifact locations (pidfiles, socket, status, refcount) ----------------

/// Delonix data root, WITHOUT depending on `geteuid()` when `DELONIX_ROOT`
/// is defined — crucial because the holder runs with uid mapped to 0 in the userns
/// (otherwise it would resolve to `/var/lib/delonix` instead of the user's store). The
/// parent always passes `DELONIX_ROOT` to the holder so the paths line up.
pub(crate) fn base_root() -> PathBuf {
    if let Some(root) = std::env::var_os("DELONIX_ROOT") {
        return PathBuf::from(root);
    }
    // SAFETY: geteuid() has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."));
        return base.join("delonix");
    }
    PathBuf::from("/var/lib/delonix")
}

/// Directory `<base>/ingress/` with the infra's state.
fn ingress_dir() -> PathBuf {
    base_root().join("ingress")
}
/// The **pin**'s pid — the process that owns the userns/netns/mountns and does
/// nothing else.
///
/// Deliberately keeps the historic file name and the `holder_*` naming across the
/// codebase: this is the pid every `nsenter -t <holder>` in the tree targets
/// (`join_argv`, `infra_join_argv`, `disable_ipv6_live`, …), and after the
/// pin/control split it is the pid that NEVER changes. Renaming it would have
/// meant touching every consumer to say the same thing.
fn holder_pid_path() -> PathBuf {
    ingress_dir().join("holder.pid")
}
/// The **control** process's pid — the restartable half (control socket, DNS, RA,
/// DHCP). Killing it does not touch a single wire.
fn control_pid_path() -> PathBuf {
    ingress_dir().join("control.pid")
}
/// Where the control plane's stderr goes. Lives next to the pidfiles, in the
/// PERSISTENT state dir and not the ephemeral socket dir: a control plane that
/// died is exactly the case where the file has to outlive the process.
fn control_log_path() -> PathBuf {
    ingress_dir().join("control.log")
}
fn slirp_pid_path() -> PathBuf {
    ingress_dir().join("slirp.pid")
}
/// The single slirp's api-socket (where the `add_hostfwd`s are requested in Phase 4).
pub fn slirp_sock_path() -> PathBuf {
    runtime_dir().join("slirp.sock")
}
/// The holder's control socket (netns/veth factory): the host requests attach/detach.
fn control_sock_path() -> PathBuf {
    runtime_dir().join("control.sock")
}

/// Where the control socket lived BEFORE v0.34.2 (directly under `ingress_dir()`,
/// i.e. derived from `DELONIX_ROOT`) — see `runtime_dir` for why it moved. Kept
/// for ONE purpose only: a holder started by a pre-v0.34.2 binary is still bound
/// HERE, so finding this file lets [`stale_holder_message`] name the cause
/// (in-place upgrade) instead of leaving the operator with a bare `ENOENT`.
/// Never bound or connected to — diagnosis only.
fn legacy_control_sock_path() -> PathBuf {
    ingress_dir().join("control.sock")
}

/// Waits (up to ~2s, the same budget as [`control_query`]'s retry loop) for the
/// control socket to appear. Returns immediately on the happy path — the file is
/// already there — so this costs one `stat` when everything is fine. The wait
/// covers the legitimate startup race: another process spawned the holder
/// microseconds ago and it hasn't `bind`ed yet.
/// Is something actually LISTENING on the control socket?
///
/// BUG FIXED HERE: this used to be `path.exists()`. A unix socket file outlives
/// the process that bound it, so a control plane that had died left a file that
/// answered "yes" forever — the third appearance in this codebase of the same
/// mistake, after `status()` reading pidfiles ("`holder_pid.is_some()` is not
/// «the holder is reachable»") and `container.userns`. It mattered the moment
/// the pin/control split gave the control plane a way to die on its own: with
/// the stale file passing, `ensure_up` returned a cheerful `ingress UP` over a
/// node with NO control plane at all — dataplane fine (that is the point of the
/// split), but no attach, no publish, no DNS, and not a word about it.
///
/// A connect is the only question worth asking: a leftover file gives
/// `ECONNREFUSED`, a live listener accepts.
fn control_reachable() -> bool {
    std::os::unix::net::UnixStream::connect(control_sock_path()).is_ok()
}

/// The actionable message for "holder ALIVE but its control socket is absent" —
/// the in-place-upgrade trap. [`status`] only reads pidfiles, so an old holder
/// left running by an upgrade looks perfectly `up` while every control command
/// hits `ENOENT` on a path that build never knew about. Reported live: a holder
/// from a pre-v0.34.2 binary made `cluster create` fail at "Preparing nodes"
/// with nothing but `control socket: No such file or directory`.
///
/// Deliberately does NOT auto-restart the infra: killing a live holder frees its
/// netns, dropping the network of every container attached to the SDN. That is the
/// operator's call, so the message says exactly what to run instead. PURE.
/// The actionable message for "this root has no pin, but the user's shared
/// control socket answers" — two `DELONIX_ROOT`s on one uid.
///
/// Pure so the wording is testable: it is the whole value of the branch, and the
/// branch itself cannot be exercised without two live holders.
fn foreign_holder_message(sock: &Path, ours: &Path) -> String {
    format!(
        "another delonix state root on this user already owns the network infra: `{}` has a \
         live listener, but there is no pidfile under `{}`. The sockets are per-USER while the \
         pidfiles are per-ROOT, so rebuilding from here would delete that infra and unplug \
         every workload on it. Either use that root (unset/point `DELONIX_ROOT` at it), or stop \
         it deliberately with `delonix net netns down` from the root that owns it.",
        sock.display(),
        ours.display()
    )
}

fn stale_holder_message(holder_pid: i32, sock: &Path, legacy: Option<&Path>) -> String {
    let cause = match legacy {
        Some(old) => format!(
            "it is bound to `{}` instead — the path the control socket used BEFORE v0.34.2, \
             so this holder was started by an older delonix build (in-place upgrade)",
            old.display()
        ),
        None => "it was very likely started by a different delonix build (in-place upgrade), \
                 or its runtime directory was removed while it ran"
            .to_string(),
    };
    format!(
        "ingress holder (pid {holder_pid}) is alive but `{}` does not exist: {cause}. \
         Restart the infra to recover: `delonix net netns down` (kills holder + slirp by \
         pidfile, so it works whatever build started them; the next command respawns both), \
         then `delonix container restart <name>` for each container on the SDN — they keep \
         running but lose their veth along with the old netns.",
        sock.display()
    )
}

/// Env var the PARENT passes explicitly to the holder — same reason as
/// `DELONIX_ROOT` above: the holder's uid is mapped to 0 INSIDE its userns,
/// so a `geteuid()`-based computation done independently in each process
/// would DIVERGE (parent sees its real uid, holder sees 0) — the two MUST
/// agree on this path, or the holder binds `control.sock` in a directory the
/// parent never created and never looks in.
const RUNTIME_DIR_ENV: &str = "DELONIX_NET_RUNTIME_DIR";

/// Directory for **ephemeral runtime state** — today only the two AF_UNIX
/// sockets (`slirp.sock`/`control.sock`) — kept DELIBERATELY separate from
/// `base_root()`/`DELONIX_ROOT`. `DELONIX_ROOT` holds regular files (VMs,
/// containers, images) with no length limit beyond the kernel's `PATH_MAX`
/// (~4096 bytes); a bound AF_UNIX socket's `sun_path`, however, is capped at
/// 108 bytes on Linux (`SUN_LEN`) — nesting sockets under an arbitrarily deep
/// `DELONIX_ROOT` hits that limit in practice (confirmed live: `bind` failing
/// with "path must be shorter than SUN_LEN" under a long test/tmp root).
/// Mirrors the convention every other container/VM engine already follows —
/// Docker's `/run/docker.sock`, Podman's `/run/podman/podman.sock`,
/// containerd's `/run/containerd/containerd.sock` — none of which nest their
/// control socket under the (potentially deep) data/storage root.
fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(RUNTIME_DIR_ENV) {
        return PathBuf::from(dir);
    }
    // SAFETY: geteuid() has no preconditions.
    let uid = unsafe { libc::geteuid() };
    // NOT `uid != 0`. That was the same mistake `is_rootless` made: uid 0
    // inside a NESTED user namespace is not the host's root, and this branch
    // then resolved `/run/delonix-net` — a directory it cannot create — so
    // every socket operation failed with a bare `Permission denied`. Found by
    // running the whole engine under `unshare --user --map-root-user`, which is
    // a legitimate rootless environment, not an exotic one.
    if !delonix_runtime_core::in_initial_userns() || uid != 0 {
        // NOT `$XDG_RUNTIME_DIR`/`/run/user/<uid>`, despite being the more
        // conventional choice for this kind of ephemeral state (systemd-
        // logind guarantees it's short) — BUG FOUND live trying exactly
        // that: `setup_infra_netns()` (below) does `mount -t tmpfs none
        // /run` INSIDE the holder's own (already `--make-rprivate`) mount
        // namespace, to give containers a private `/run/netns`. Anything
        // created under `/run` by the PARENT before that remount becomes
        // INVISIBLE to the holder afterwards — confirmed live: `control.sock`
        // (bound by `holder_main`, i.e. AFTER the remount) got ENOENT on a
        // directory that demonstrably existed on the host's real `/run`.
        // `slirp.sock` (bound by `slirp4netns`, spawned by the PARENT, never
        // enters this remounted view) would have been fine under `/run` —
        // but splitting the two sockets across different directories for a
        // reason future maintainers can't see by reading THIS file isn't
        // worth it. `/tmp` is a separate mount, untouched by that remount,
        // always short, and always writable; scoped by uid so two users
        // sharing a host never collide (the control socket is ALSO
        // protected by SO_PEERCRED + 0600 — this is defense in depth only).
        return std::env::temp_dir().join(format!("delonix-net-{uid}"));
    }
    // Real root — the INITIAL namespace's root — never reaches this module's
    // holder at all (see `infra.rs`'s
    // top doc comment + `delonix-runtime/CLAUDE.md`'s net gotcha: a real-root
    // process already has host `CAP_NET_ADMIN` and uses the OTHER mechanism,
    // `Net::` in `lib.rs` — no rootless holder, no netns re-exec, no `/run`
    // remount). Kept for symmetry/future reuse of this function, not because
    // `control_sock_path`/`slirp_sock_path` exercise it today.
    PathBuf::from("/run/delonix-net")
}

/// The `(env var, value)` pair that PINS `runtime_dir` for a child that will run
/// with a different **uid view** than ours: the holder (`start_holder`) and the
/// `--net <custom>` re-exec passes (`nsenter -U … ip netns exec`, see
/// `cmd::container::reexec_into_netns`). Inside the holder's userns `geteuid()` is
/// **0**, so a child left to compute `runtime_dir` on its own resolves
/// `/run/delonix-net` — a directory that does not exist — and every socket
/// operation there fails with a bare `ENOENT`.
///
/// **Reported live (v0.34.1 regression):** `container run/start --net <custom> -p
/// <port>` failed with ``slirp api-socket … No such file or directory`` because the
/// re-exec passed `DELONIX_ROOT` but not this. Until v0.34.1 the sockets lived under
/// `ingress_dir()` (i.e. `DELONIX_ROOT`-derived), so pinning the root alone happened
/// to be enough; moving them to `runtime_dir` made this a second, independent
/// thing to pin. Returned as ONE pair precisely so no caller can pass a var/value
/// mismatch, and so `grep runtime_dir_env` finds every child that needs it.
pub fn runtime_dir_env() -> (&'static str, PathBuf) {
    (RUNTIME_DIR_ENV, runtime_dir())
}

/// Creates `runtime_dir` with restrictive permissions (`0700`) — defense in
/// depth alongside the control socket's own `0600`+`SO_PEERCRED` guard.
fn ensure_runtime_dir() -> Result<()> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir).map_err(|e| Error::Runtime {
        context: "runtime dir",
        message: e.to_string(),
    })?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}
fn status_path() -> PathBuf {
    ingress_dir().join("status")
}
fn refcount_path() -> PathBuf {
    ingress_dir().join("refcount")
}
fn lock_path() -> PathBuf {
    ingress_dir().join("lock")
}

// ---- process/pid helpers -----------------------------------------------------

/// `true` if the process `pid` still exists (via `/proc/<pid>`).
fn pid_alive(pid: i32) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
}

/// Sends `SIGTERM` to a pid and removes its pidfile.
fn kill_pidfile(path: &Path) {
    if let Some(pid) = read_pid(path) {
        if pid_alive(pid) {
            // SAFETY: kill() with a valid pid; we ignore the result (best-effort).
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
    let _ = std::fs::remove_file(path);
}

// ---- ingress nft (inside the infra netns) -----------------------------------

/// The ingress's BASE `nft` *ruleset*: `pre` chain (DNAT of published ports),
/// `post` (tap0 masquerade) and `fwd` (forward FILTER — the ONLY place of the
/// parameterizable firewall, with per-container chains called by jump). PURE.
pub fn ingress_table_ruleset() -> String {
    // DEFAULT-DENY on the forward (Group B). The dynamic DROPS (anti-spoof, isolation,
    // egress, egress-net, l4guard, per-container fw) live in the `fwdeny` chain
    // (priority -10, runs BEFORE) — so a specific `drop`/`accept` always wins
    // over the default. The `forward` (priority 0) allows returns + egress +
    // inbound + **same network** (intra-bridge `delonix0`); the rest falls into the `policy drop`.
    //
    // WHERE THE PER-CONTAINER FIREWALL IS DISPATCHED: its own base chain `fwcont`
    // (priority -5), between `fwdeny` (-10) and `forward` (0). Deliberately NOT in
    // `fwdeny`: the dispatch rules would be appended among the network-wide egress
    // rules, and their relative order would then depend on the ORDER OF EVENTS (which
    // command ran first), not on intent. Placing it in its own chain makes precedence
    // a property of the design — network-level egress policy is evaluated first and
    // stays authoritative, per-container rules apply within it. An `accept` in
    // `fwdeny` is not terminal across base chains, so a network-level accept never
    // bypasses the container's own firewall.
    //
    // INTRA-NETWORK: with `br_netfilter` (bridge-nf-call-iptables=1) the traffic between
    // containers on the SAME bridge traverses the forward and would fall into the drop → apps
    // wouldn't reach their services/addons on the same network. We accept `delonix0↔delonix0`
    // (Docker user-network/k8s model: same network communicates; crossing networks is dropped by
    // the inter-bridge `fwdeny`). Intra-network micro-segmentation is done with `kind:NetworkPolicy`
    // (P12), whose rules go into the `fwdeny` (run first, pre-empt this accept).
    // Instant rollback: DELONIX_FORWARD_POLICY=accept → back to default-allow.
    let policy = if std::env::var("DELONIX_FORWARD_POLICY").ok().as_deref() == Some("accept") {
        // NET-03: the opt-out reverts the default-deny — don't leave this silent.
        tracing::warn!(
            "SECURITY WARNING — DELONIX_FORWARD_POLICY=accept: the ingress netns forward \
             reverts to default-ALLOW (no `policy drop`). For debugging only — do NOT use in production."
        );
        ""
    } else {
        " policy drop;"
    };
    // RF-NET-02 — destinations that are denied REGARDLESS of the declared policy, in
    // their own base chain at priority -20, so they are evaluated before `fwdeny`
    // (-10), before the per-container chains (-5) and before the default policy (0).
    // A user rule cannot reach in front of them; only the opt-in below removes them.
    //
    //   169.254.0.0/16 — cloud metadata (`169.254.169.254`). On a cloud host this is
    //     the instance's credentials endpoint, one HTTP GET away from any container.
    //     Measured on this workstation: nothing answers, because nothing is listening
    //     — NOT because anything blocked it. There was no denial anywhere in the tree.
    //   127.0.0.0/8 — the host's own loopback. Already unreachable in practice because
    //     `slirp4netns` runs with `--disable-host-loopback` (verified live: `Network is
    //     unreachable`), but that is one flag in one spawn path away from regressing,
    //     and this costs one rule.
    //
    // IPv6 link-local (`fe80::/10`) is covered by the v6 refusal instead — there are no
    // v6 addresses to filter, see `ipv6_sdn_enabled`.
    //
    // NOT covered here, and deliberately so: the management sockets. `serve api`,
    // `serve cri` and `serve docker-api` all default to UNIX sockets, not TCP
    // (`unix:///run/delonix-*.sock`), so there is no address for a container to reach.
    //
    // The services the holder itself exposes ARE covered now, by `dlxinput` below —
    // this comment used to record that as a follow-up, and the follow-up turned out to be
    // an exploitable bypass (measured, see `docs/discovery/46_GAPS_ENCONTRADOS.md` §4.2).
    let guard = if std::env::var("DELONIX_ALLOW_LINK_LOCAL").ok().as_deref() == Some("1") {
        tracing::warn!(
            "SECURITY WARNING — DELONIX_ALLOW_LINK_LOCAL=1: containers may reach \
             169.254.0.0/16 (cloud metadata / instance credentials) and the host loopback. \
             For debugging only — do NOT use in production."
        );
        String::new()
    } else {
        "\x20\x20 ip daddr 169.254.0.0/16 counter drop\n\
         \x20\x20 ip daddr 127.0.0.0/8 counter drop\n"
            .to_string()
    };
    // `dlxinput` — the holder's OWN services are not reachable from a container.
    //
    // Every policy chain in this table hangs off `forward`, and traffic addressed TO the
    // holder never goes through `forward` — it goes through `input`, which had no chain at
    // all. That is a whole side of the node with no policy on it, and it was exploitable:
    // the L7 proxy listens in this netns, so any container could reach it on its bridge
    // gateway and be relayed to ANY registered backend — across namespaces, and past a
    // `ingress policy deny` on the backend (both measured; the proxy→backend leg originates
    // here, so it never meets `fwcont` either).
    //
    // The allowlist is what a container legitimately needs FROM the holder, and nothing
    // else: the internal DNS, DHCP (the VM leases), ICMP for diagnostics, and the return
    // traffic of anything the holder itself opened. `tap0` is the host→slirp→holder path,
    // which is the whole point of an ingress, so it stays open.
    //
    // Deliberately keyed on what is ALLOWED, not on the proxy's ports: the listeners are
    // dynamic (`kind: HTTPRoute` declares its own) and this chain is built once at
    // `ensure_up`, long before any proxy exists. Enumerating ports would have to be kept in
    // sync with a moving target — and would leave every FUTURE holder-resident listener
    // exposed by default, which is the same shape of gap being closed here.
    let holder_input = if std::env::var("DELONIX_ALLOW_HOLDER_INGRESS")
        .ok()
        .as_deref()
        == Some("1")
    {
        tracing::warn!(
            "SECURITY WARNING — DELONIX_ALLOW_HOLDER_INGRESS=1: containers may reach the \
             services the holder exposes (the L7 proxy, the internal DNS). Through the proxy \
             a container reaches ANY registered backend, in ANY namespace, past that \
             backend's own ingress policy. For debugging only — do NOT use in production."
        );
        String::new()
    } else {
        "\x20\x20 ct state new counter drop\n".to_string()
    };
    format!(
        "table ip {INGRESS_TABLE} {{\n\
         \x20 set {DLXALL_SET} {{ type ipv4_addr; }}\n\
         \x20 map {FWMAP} {{ type ipv4_addr : verdict; }}\n\
         \x20 chain fwguard {{ type filter hook forward priority -20;\n\
         {guard}\
         \x20 }}\n\
         \x20 chain fwcont {{ type filter hook forward priority -5;\n\
         \x20\x20 ip daddr vmap @{FWMAP}\n\
         \x20\x20 ip saddr vmap @{FWMAP}\n\
         \x20 }}\n\
         \x20 chain pre {{ type nat hook prerouting priority -100; }}\n\
         \x20 chain post {{ type nat hook postrouting priority 100; oifname \"tap0\" masquerade; }}\n\
         \x20 chain fwdeny {{ type filter hook forward priority -10; }}\n\
         \x20 chain dlxinput {{ type filter hook input priority 0;\n\
         \x20\x20 ct state established,related accept\n\
         \x20\x20 iifname \"lo\" accept\n\
         \x20\x20 iifname \"tap0\" accept\n\
         \x20\x20 udp dport {{ 53, 67, 68 }} accept\n\
         \x20\x20 tcp dport 53 accept\n\
         \x20\x20 meta l4proto icmp accept\n\
         {holder_input}\
         \x20 }}\n\
         \x20 chain forward {{ type filter hook forward priority 0;{policy}\n\
         \x20\x20 ct state established,related accept\n\
         \x20\x20 ct state invalid drop\n\
         \x20\x20 oifname \"tap0\" accept\n\
         \x20\x20 iifname \"tap0\" accept\n\
         \x20\x20 iifname \"delonix0\" oifname \"delonix0\" accept\n\
         \x20 }}\n\
         }}\n"
    )
}

// ---- ref-count (lifecycle shared by the containers, Phase 3) ----------------
//
// SET model (not an integer counter). Each container/pod that enters the
// ingress infra leaves a MARKER (a file in `<ingress>/refs/`, whose
// name is the hex of the id); the "ref-count" is the CARDINALITY of the set. Why a
// set and not an `i64`:
//   - `release` becomes IDEMPOTENT per-id — removing a marker that no longer exists
//     is a no-op, so a `stop` followed by a `rm` (two detaches for the same
//     id) does NOT tear down the infra too early, and a container killed abruptly that
//     is only reaped later doesn't count double.
//   - it enables a DETERMINISTIC REAPER: cross the markers with the LIVE ids
//     (Store + CRI pods) and free only the orphans (marker with no live owner). A
//     blind counter would never know WHICH ones to free.
// Closes the "16 refs with 3 live containers" leak: each exit path (normal
// rm, dead container, error midway) removes ITS id's marker, and whatever
// escapes (abrupt death without `rm`) is caught by the reaper.

/// Directory with one marker per container/pod attached to the ingress infra.
fn refs_dir() -> PathBuf {
    ingress_dir().join("refs")
}

/// Marker filename for an id — hex of the id, reversible and always safe
/// on disk. Unlike [`sanitize`] it does NOT truncate: the id has to survive the
/// round-trip so the reaper can cross it with the Store without collisions.
fn ref_marker_name(id: &str) -> String {
    hex_encode(id.as_bytes())
}

/// Registers `id`'s marker in `dir` (idempotent). Testable core: takes the
/// dir explicitly, touches neither the global path nor the kernel.
fn ref_add_in(dir: &Path, id: &str) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(ref_marker_name(id)), id.as_bytes());
}

/// Removes `id`'s marker in `dir` (idempotent). Testable core.
fn ref_remove_in(dir: &Path, id: &str) {
    let _ = std::fs::remove_file(dir.join(ref_marker_name(id)));
}

/// Reads the ATTACHED ids from the markers in `dir` (decodes the hex of the
/// name). Testable core.
fn refs_in(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            hex_decode(&name).and_then(|b| String::from_utf8(b).ok())
        })
        .collect()
}

/// **PURE** — which of the ATTACHED ids no longer have a live owner (reap candidates).
/// It's the heart of the ref-count's deterministic reaper: a marker whose id is not
/// in `live` (running containers + CRI pods, assembled by the caller) has lost
/// its owner and should be freed. Touches neither disk nor kernel — dry-testable.
pub fn orphan_refs(attached: &[String], live: &std::collections::HashSet<String>) -> Vec<String> {
    attached
        .iter()
        .filter(|id| !live.contains(*id))
        .cloned()
        .collect()
}

/// Ids currently attached to the ingress infra (for the caller — e.g.: `system
/// prune` — to preserve the ones it knows are alive when assembling the reaper's `live`).
pub fn attached_refs() -> Vec<String> {
    refs_in(&refs_dir())
}

/// Number of containers using the infra (cardinality of the marker set).
fn read_refcount() -> i64 {
    refs_in(&refs_dir()).len() as i64
}

/// Exclusive file lock (`flock`) around the ref-count operations, so that
/// concurrent `acquire`/`release` (several `run` in parallel) don't run on
/// top of each other. Returns the fd; `Drop` releases it.
struct FileLock(i32);
impl FileLock {
    fn acquire() -> FileLock {
        let _ = std::fs::create_dir_all(ingress_dir());
        let path = lock_path();
        let c = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes().to_vec())
            .unwrap_or_else(|_| std::ffi::CString::new("/tmp/dlxlock").unwrap());
        // SAFETY: open/flock with a valid path; -1 on failure is handled next.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
        if fd >= 0 {
            unsafe { libc::flock(fd, libc::LOCK_EX) };
        }
        FileLock(fd)
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        if self.0 >= 0 {
            // SAFETY: own fd, opened in acquire().
            unsafe {
                libc::flock(self.0, libc::LOCK_UN);
                libc::close(self.0);
            }
        }
    }
}

/// Increments the ref-count and ensures the infra is up on the 1st user. Call once
/// per container/pod that enters the ingress network (Phase 3). `id` = the
/// container/pod's id — the SAME key that `release`/the reaper use to cross it with the
/// Store; idempotent (attaching the same id twice doesn't count double).
pub fn acquire(id: &str) -> Result<()> {
    let _lock = FileLock::acquire();
    ensure_up()?; // idempotent — robust even with stale markers
    ref_add_in(&refs_dir(), id);
    Ok(())
}

/// Decrements the ref-count (removes `id`'s marker, **idempotent**) and tears down
/// the infra when the LAST user leaves. Safe on any exit path:
/// `stop` and then `rm` of the same container don't tear down the infra twice.
pub fn release(id: &str) {
    let _lock = FileLock::acquire();
    ref_remove_in(&refs_dir(), id);
    if refs_in(&refs_dir()).is_empty() {
        teardown();
    }
}

/// **Deterministic ref-count reaper**: frees the markers whose id is NOT
/// among the live ones (`live` = ids of running containers + CRI pods, assembled
/// by the caller — like `reap_orphan_hostfwds` receives the `live_ports`). Returns
/// how many it freed; tears down the infra if it runs out of markers. **Never touches a
/// live id.** Closes the leak of markers left by abrupt deaths that never
/// went through `detach_container`.
/// A freshly-`acquire`d marker is spared from reaping for this long, no
/// matter what `live` says. Closes a real TOCTOU: `attach_container` writes
/// the ref-marker BEFORE the container's Store record is saved (the network
/// has to exist before the container does), so a `system prune` racing that
/// exact window sees an id with a marker but no Store entry, calls it an
/// orphan, and — if it's the last marker — tears down the ENTIRE holder/
/// slirp/nft state out from under the in-flight `run`, and drops every
/// other container's veths/nft rules too (the holder netns is shared).
/// Container creation is not expected to take anywhere near this long
/// between `acquire` and `store.save`; generous on purpose since the cost
/// of skipping a genuinely-orphaned marker for a few extra seconds is
/// nothing, while the cost of the race is total ingress teardown.
const REF_MARKER_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// Marker `mtime`-based grace check, factored out for testing without
/// needing a real clock race: `now` and `grace` are parameters, not
/// `SystemTime::now()`/the constant above, baked in.
fn marker_within_grace(
    mtime: std::time::SystemTime,
    now: std::time::SystemTime,
    grace: std::time::Duration,
) -> bool {
    now.duration_since(mtime)
        .map(|age| age < grace)
        .unwrap_or(true) // clock went backwards → don't reap
}

pub fn reap_orphan_refs(live: &std::collections::HashSet<String>) -> usize {
    let _lock = FileLock::acquire();
    let dir = refs_dir();
    let now = std::time::SystemTime::now();
    let candidates = orphan_refs(&refs_in(&dir), live);
    let orphans: Vec<String> = candidates
        .into_iter()
        .filter(|id| {
            std::fs::metadata(dir.join(ref_marker_name(id)))
                .and_then(|m| m.modified())
                .map(|mtime| !marker_within_grace(mtime, now, REF_MARKER_GRACE))
                .unwrap_or(true) // marker vanished/unreadable — nothing to spare
        })
        .collect();
    for id in &orphans {
        ref_remove_in(&dir, id);
    }
    if refs_in(&dir).is_empty() {
        teardown();
    }
    orphans.len()
}

// ---- state / observation ----------------------------------------------------

/// Observable state of the ingress infra (for `ingress status` and the Console).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct InfraStatus {
    /// Host-visible PID of the **pin** — the process that owns the namespaces and
    /// does nothing else. Alive for the whole life of the infra; if it dies, every
    /// wire dies with it.
    pub holder_pid: Option<i32>,
    /// PID of the **control plane** (control socket, DNS, RA, DHCP). Restartable:
    /// it can be absent for a moment without any workload noticing, so it is
    /// reported separately rather than folded into `up`.
    ///
    /// `None` does NOT mean "no control plane" — see `control_reachable`. A
    /// PRE-SPLIT holder (a single process doing both jobs, which is what an
    /// in-place upgrade leaves running) serves the socket perfectly while having
    /// no control pidfile at all.
    pub control_pid: Option<i32>,
    /// Is something actually LISTENING on the control socket? Decided by a
    /// connect, not by a pidfile or a file's existence — the two states that look
    /// identical without it ("no separate control process because the holder
    /// predates the split" and "the control plane is dead") are the two that most
    /// need telling apart when diagnosing a node.
    pub control_reachable: bool,
    /// PID of the single `slirp4netns` (the host↔infra bridge).
    pub slirp_pid: Option<i32>,
    /// `true` if holder AND slirp are alive.
    pub up: bool,
    pub bridge: String,
    pub gateway: String,
    /// Counter of containers using the infra (ref-count).
    pub refcount: i64,
}

/// Reads the current state from the pidfiles (without touching the kernel).
pub fn status() -> InfraStatus {
    let holder = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p));
    let control = read_pid(&control_pid_path()).filter(|&p| pid_alive(p));
    let slirp = read_pid(&slirp_pid_path()).filter(|&p| pid_alive(p));
    InfraStatus {
        up: holder.is_some() && slirp.is_some(),
        holder_pid: holder,
        control_pid: control,
        control_reachable: control_reachable(),
        slirp_pid: slirp,
        bridge: INFRA_BRIDGE.to_string(),
        gateway: INFRA_GATEWAY.to_string(),
        refcount: read_refcount(),
    }
}

// ---- bring up / tear down ---------------------------------------------------

/// Ensures the infra is up (holder + bridge + single slirp). **Idempotent**: if
/// everything is already alive, does nothing. It's the manager's entry point.
pub fn ensure_up() -> Result<()> {
    // The pin is alive: the namespaces, and everything plugged into them, are
    // intact. The only question is whether the CONTROL plane is there.
    if let Some(pin) = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p)) {
        if control_reachable() {
            return Ok(());
        }
        // An in-place upgrade over a PRE-split build: that holder is a single
        // process serving the legacy socket path, and its presence on disk is the
        // proof. Deliberately NOT auto-healed — killing it frees the netns and
        // drops the network of every workload on it. The operator's call.
        let legacy = legacy_control_sock_path();
        if legacy.exists() {
            return Err(Error::Runtime {
                context: "control socket",
                message: stale_holder_message(pin, &control_sock_path(), Some(legacy.as_path())),
            });
        }
        // THE CASE THIS SPLIT EXISTS FOR: the pin is alive — so the netns, every
        // veth, every tap, the nft ruleset and the slirp uplink are all still
        // there — and only the control plane died (a crash, a kill, an in-place
        // upgrade of the control half). Restart it INSIDE the surviving
        // namespaces and not a single wire moves. Before the split this path did
        // not exist: a dead holder meant a brand-new netns and every workload on
        // the node permanently unplugged.
        std::fs::create_dir_all(ingress_dir()).map_err(|e| Error::Runtime {
            context: "ingress dir",
            message: e.to_string(),
        })?;
        ensure_runtime_dir()?;
        let _ = std::fs::remove_file(control_sock_path());
        start_control(pin)?;
        // The slirp is the uplink and belongs to the pin, not to the control — it
        // only needs restarting if it, too, is gone.
        if read_pid(&slirp_pid_path())
            .filter(|&p| pid_alive(p))
            .is_none()
        {
            start_slirp(pin)?;
        }
        return Ok(());
    }

    // The pin is gone *for this root* — and that is NOT the same as "gone for
    // this user".
    //
    // The pidfiles live under `DELONIX_ROOT/ingress/`, per-ROOT; the control
    // socket and the slirp live in `/tmp/delonix-net-<uid>`, per-UID and
    // therefore SHARED by every root the same user runs. Two roots on one user
    // — a `delonix-cri` with its own state dir next to the normal CLI, which is
    // exactly how this repo runs its own conformance suite — each read their own
    // (absent) pidfile, each conclude "no infra", and the second one calls
    // `teardown()`: it deletes the FIRST one's sockets and unplugs every
    // workload on that netns, in silence.
    //
    // Measured 2026-08-10: a conformance run and a normal session collided this
    // way after a reboot cleared `/tmp`. The suite's holder came up at 07:01:57,
    // the session's at 07:04:23 and took the socket; every `RunPodSandbox`
    // afterwards hung for the full 600s spec timeout, and the run reported 79
    // failures that had nothing to do with conformance.
    //
    // So: ask the RESOURCE, not our bookkeeping. A live listener on the shared
    // socket means somebody owns this user's infra, and destroying it is the
    // operator's call, never a side effect of a command that wanted a network.
    if control_reachable() {
        return Err(Error::Runtime {
            context: "control socket",
            message: foreign_holder_message(&control_sock_path(), &ingress_dir()),
        });
    }
    teardown();
    std::fs::create_dir_all(ingress_dir()).map_err(|e| Error::Runtime {
        context: "ingress dir",
        message: e.to_string(),
    })?;
    ensure_runtime_dir()?;
    let pin_pid = start_pin()?;
    if let Err(e) = start_control(pin_pid) {
        teardown();
        return Err(e);
    }
    if let Err(e) = start_slirp(pin_pid) {
        // if the slirp fails, we don't leave an orphan pin.
        teardown();
        return Err(e);
    }
    Ok(())
}

/// Starts the control plane inside the pin's namespaces (`nsenter -t <pin> -U -m
/// -n`) and waits for it to signal `ready` on the state file.
///
/// `-m` as well as `-U -n`: the control needs the pin's MOUNT namespace, where
/// `/run/netns` lives — that is where every named netns of every pod and
/// `--net <custom>` container is pinned.
fn start_control(pin: i32) -> Result<i32> {
    let exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    let _ = std::fs::remove_file(status_path());
    let child = Command::new("nsenter")
        .args([
            "-t",
            &pin.to_string(),
            "-U",
            "-m",
            "-n",
            "--preserve-credentials",
            "--",
        ])
        .arg(&exe)
        .args(["netns", "control"])
        .env("DELONIX_ROOT", base_root())
        .env(runtime_dir_env().0, runtime_dir_env().1)
        .env("DELONIX_INTERNAL", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // NOT `/dev/null`, and the difference is diagnosability. The control
        // process runs the DNS server, the RA emitter and the DHCP threads; a
        // panic in any of them used to go to the void, so the service degraded
        // with no line anywhere — measured during the DNS review, where it was
        // the single biggest reason a diagnosis took as long as it did.
        // Inherit is wrong here (unlike the pin): the control plane is
        // RESTARTABLE and long-lived, so it would hold the stderr of whichever
        // short-lived CLI happened to start it. A file is the thing that
        // survives. Appends, because the reason a control plane DIED is worth
        // more than the log of the one that replaced it.
        .stderr(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(control_log_path())
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null()),
        )
        .spawn()
        .map_err(|e| Error::Runtime {
            context: "spawn nsenter (control)",
            message: e.to_string(),
        })?;
    let pid = child.id() as i32;
    let _ = std::fs::write(control_pid_path(), pid.to_string());
    std::mem::forget(child);
    for _ in 0..100 {
        if !pid_alive(pid) {
            return Err(Error::Runtime {
                context: "ingress control",
                message: "the control plane died during startup".into(),
            });
        }
        match std::fs::read_to_string(status_path()) {
            Ok(s) if s.trim() == "ready" => return Ok(pid),
            Ok(s) if s.trim_start().starts_with("err:") => {
                return Err(Error::Runtime {
                    context: "ingress control",
                    message: s.trim().trim_start_matches("err:").trim().to_string(),
                });
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    Err(Error::Runtime {
        context: "ingress control",
        message: "timeout waiting for the control plane".into(),
    })
}

/// Tears down the infra: kills the slirp and the holder (which frees the netns) and cleans up the
/// artifacts. Best-effort and idempotent.
pub fn teardown() {
    // the DHCP/DNS/RA servers are threads of the holder — they die when it's killed.
    kill_pidfile(&slirp_pid_path());
    // Control BEFORE pin: the control lives inside the pin's namespaces, and
    // killing the pin first would leave it running in a netns nobody can name.
    kill_pidfile(&control_pid_path());
    kill_pidfile(&holder_pid_path());
    let _ = std::fs::remove_file(slirp_sock_path());
    let _ = std::fs::remove_file(control_sock_path());
    // Also the PRE-v0.34.2 locations: this is the command that recovers a host from
    // an in-place upgrade (see `stale_holder_message`), so it has to leave no socket
    // behind from the build it just killed — a leftover legacy file would make a
    // LATER diagnosis blame an old binary that is no longer running.
    let _ = std::fs::remove_file(legacy_control_sock_path());
    let _ = std::fs::remove_file(ingress_dir().join("slirp.sock"));
    let _ = std::fs::remove_file(status_path());
    // Clean state — no stale markers holding the infra up in the next cycle.
    let _ = std::fs::remove_dir_all(refs_dir());
    let _ = std::fs::remove_file(refcount_path()); // legacy (old integer counter)
}

/// Starts the **holder**: re-exec of the binary itself inside `unshare
/// --user --map-root-user --net --mount`, which runs [`holder_main`] (root in the
/// userns) to set up `delonix0` + `nft` and then block. Waits for the
/// "ready" state file before returning the host-visible PID.
/// Waits for `fd` to become readable, capped at `timeout_ms`. `true` = readable (or
/// EOF, which is also an event and unblocks the `read`); `false` = timed out.
///
/// It exists so there are no more bare `read`s on fds that depend on an external
/// process signaling: if that process never signals AND never closes the fd (a
/// grandchild inheriting it is enough), the `read` hangs forever — that's how a
/// `run` got stuck 1h in `skb_wait_for_more_packets` with no log or exit.
/// `poll` doesn't need to touch the fd's flags (no `O_NONBLOCK` leaking
/// to whoever inherits it).
pub(crate) fn wait_readable(fd: i32, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` is valid and lives for the duration of the call; poll doesn't retain the pointer.
    // EINTR (signal) returns -1 → we treat it as "not ready", the caller warns.
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 }
}

fn start_pin() -> Result<i32> {
    let exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    let _ = std::fs::remove_file(status_path());
    // `--map-auto` maps the user's ENTIRE subuid/subgid range (/etc/subuid),
    // not just root: real images (nginx uid 101, postgres, …) need chown
    // to uids != 0 INSIDE the container, which thus become mappable. `--map-root-user`
    // maps the userns's uid 0 → the user's uid on the host.
    // `--map-auto` needs `newuidmap`, which validates the requested range
    // against `/etc/subuid` for the REAL uid — and inside a NESTED user
    // namespace that check fails ("uid range not allowed") however the outer
    // namespace was set up. The holder then never came up and the caller got
    // `timeout waiting for the netns holder`: a symptom five seconds and one
    // layer away from the cause.
    //
    // Nested → map only uid 0. Containers that need a non-root uid INSIDE
    // (nginx's 101, postgres) lose that there, which is a real reduction — but
    // a documented one, and strictly better than an engine that hangs.
    let nested = !delonix_runtime_core::in_initial_userns();
    let mut unshare_args: Vec<&str> = vec!["--user"];
    if !nested {
        unshare_args.push("--map-auto");
    }
    unshare_args.extend(["--map-root-user", "--net", "--mount", "--"]);
    let child = Command::new("unshare")
        .args(&unshare_args)
        .arg(&exe)
        .args(["netns", "pin"])
        // the holder runs with uid->0 in the userns; forces the paths to the real base.
        .env("DELONIX_ROOT", base_root())
        // same reason, same fix: forces the SHORT socket dir too (see `runtime_dir_env`).
        .env(runtime_dir_env().0, runtime_dir_env().1)
        .env("DELONIX_INTERNAL", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Kept out of `/dev/null`, deliberately. The holder failing to start is
        // reported to the caller as a bare timeout, and the reason — an
        // `unshare`/`newuidmap` refusal, printed here and nowhere else — used to
        // be discarded. Inheriting stderr costs nothing (the process is
        // detached) and turns a five-second silence into a sentence.
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| Error::Runtime {
            context: "spawn unshare",
            message: e.to_string(),
        })?;
    let pid = child.id() as i32;
    let _ = std::fs::write(holder_pid_path(), pid.to_string());
    // the holder stays alive for the entire life of the infra — we don't wait on it.
    std::mem::forget(child);

    // waits for the holder to signal "ready" (or error) in the state file (~5s).
    for _ in 0..100 {
        if !pid_alive(pid) {
            teardown();
            return Err(Error::Runtime {
                context: "ingress holder",
                message: "the netns holder died during startup".into(),
            });
        }
        match std::fs::read_to_string(status_path()) {
            Ok(s) if s.trim() == "pinned" => return Ok(pid),
            Ok(s) if s.trim_start().starts_with("err:") => {
                teardown();
                return Err(Error::Runtime {
                    context: "ingress holder",
                    message: s.trim().trim_start_matches("err:").trim().to_string(),
                });
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    teardown();
    Err(Error::Runtime {
        context: "ingress holder",
        message: "timeout waiting for the netns holder".into(),
    })
}

/// Starts the **single slirp** attached to the holder's netns (`tap0`), with an api-socket
/// for the Phase 4 `add_hostfwd`s. Waits for the `--ready-fd` before returning.
fn start_slirp(holder_pid: i32) -> Result<()> {
    let sock = slirp_sock_path();
    let _ = std::fs::remove_file(&sock);
    let mut fds = [0i32; 2];
    // SAFETY: pipe() fills 2 fds; -1 on failure is handled next.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(Error::Runtime {
            context: "pipe",
            message: "slirp ready-fd".into(),
        });
    }
    let (rd, wr) = (fds[0], fds[1]);
    let spawned = Command::new("slirp4netns")
        .args([
            "--configure",
            "--mtu=65520",
            "--disable-host-loopback",
            &format!("--ready-fd={wr}"),
            &format!("--api-socket={}", sock.display()),
            &holder_pid.to_string(),
            "tap0",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // SAFETY: the parent closes its write copy; only the slirp keeps it open.
    unsafe { libc::close(wr) };
    match spawned {
        Ok(child) => {
            // CAPPED WAIT. A bare `read` here could hang FOREVER: the EOF
            // only arrives if ALL copies of the write-end close, and a single grandchild of the
            // slirp inheriting the fd is enough for that to never happen. And the stakes rose: the
            // `slirp_attach` now runs BEFORE releasing the container (the network
            // has to be ready before the entrypoint), so a slirp that doesn't
            // signal hangs the entire `run`, with no log and no exit — the same class
            // as the console's `recv_fd` deadlock. 10s is more than enough (the slirp
            // signals in ms); after that we move on and the error surfaces downstream,
            // with a message, instead of a process hung forever.
            // BUG FOUND: this used to `read` unconditionally after the poll,
            // even on timeout — defeating the whole point of `wait_readable`.
            // With no data and no EOF (the exact condition the poll timed
            // out on), that bare `read` blocks INDEFINITELY, reintroducing
            // the deadlock this capped wait exists to prevent. Only read
            // when the fd is actually known-readable; always close it either way.
            if wait_readable(rd, 10_000) {
                let mut b = [0u8; 1];
                // SAFETY: reads 1 byte from a read-end already confirmed readable.
                unsafe {
                    libc::read(rd, b.as_mut_ptr() as *mut libc::c_void, 1);
                }
            } else {
                tracing::warn!("slirp4netns did not signal ready within 10s; the container network may not be operational");
            }
            // SAFETY: rd is a valid fd owned by this function either way.
            unsafe {
                libc::close(rd);
            }
            let _ = std::fs::write(slirp_pid_path(), (child.id() as i32).to_string());
            // the slirp lives for the entire life of the infra — we don't wait on it.
            std::mem::forget(child);
            Ok(())
        }
        Err(e) => {
            // SAFETY: closes the read-end on error.
            unsafe { libc::close(rd) };
            Err(Error::Runtime {
                context: "slirp4netns",
                message: e.to_string(),
            })
        }
    }
}

// ---- holder body (runs INSIDE the user+net+mount namespace) -----------------

/// Entry point of the **holder** (invoked by `delonix netns holder`, hidden).
/// Runs as root in the freshly-created userns/netns: sets up `delonix0`, enables
/// `ip_forward`, installs the ingress `nft` table, OPENS the control socket,
/// writes "ready" and **serves** container attach/detach requests (the netns/veth
/// factory). The netns lives as long as this process lives; SIGTERM (teardown)
/// kills it → the kernel frees the netns. On startup failure it writes `err:<msg>`.
/// The **pin**: owns the userns/netns/mountns and does nothing else, forever.
///
/// This is the whole point of the pin/control split. Before it, ONE process both
/// owned the namespaces and ran the control plane, so restarting the control
/// plane — an in-place upgrade, a crash, a `kill` — destroyed the netns and with
/// it every wire in it. Measured on a live VM: kill the old single-process
/// holder and the netns actually SURVIVES (the VM process keeps it alive, bridge
/// and taps and nft ruleset all intact) — what killed connectivity was the next
/// `ensure_up` throwing that netns away and building a fresh one.
///
/// So the pin never does anything that can fail after startup: no sockets, no
/// threads, no state. The only way it dies is a kill or the machine going down.
/// The control plane runs INSIDE it via `nsenter` and is free to come and go.
///
/// It also removes a whole class of upgrade hazard: the pin has no
/// version-specific behaviour, so a pin from an older build plus a control from a
/// newer one is safe by construction (see `stale_holder_message` for what that
/// used to cost).
pub fn pin_main() -> ! {
    write_status("pinned");
    // Nothing to serve, nothing to poll — just stay alive holding the namespaces.
    loop {
        // SAFETY: `pause()` only returns on a signal; no arguments, no state.
        unsafe {
            libc::pause();
        }
    }
}

/// The **control plane**, running inside the pin's namespaces: control socket,
/// DNS, Router Advertisements and the per-bridge DHCP servers.
///
/// Restartable by design. On a FRESH netns it builds the infra; on one that is
/// already configured (the pin survived, only this process restarted) it
/// reattaches instead — see `setup_infra_netns` for why re-running the build
/// would be actively destructive.
pub fn control_main() -> ! {
    let started = reattach_or_setup_infra_netns().and_then(|_| {
        let _ = std::fs::remove_file(control_sock_path());
        let listener =
            std::os::unix::net::UnixListener::bind(control_sock_path()).map_err(|e| {
                Error::Runtime {
                    context: "control socket",
                    message: e.to_string(),
                }
            })?;
        // only the engine's uid can talk to the holder: 0600 + SO_PEERCRED (control_loop).
        use std::os::unix::fs::PermissionsExt;
        let _ =
            std::fs::set_permissions(control_sock_path(), std::fs::Permissions::from_mode(0o600));
        Ok(listener)
    });
    match started {
        Ok(listener) => {
            // ingress DNS server on a thread (resolves container/VM names).
            std::thread::spawn(dns_server_main);
            // Router Advertisements emitter (SLAAC IPv6 for VMs/containers).
            std::thread::spawn(ra_sender_main);
            // only now do we signal ready — the control socket already accepts connections.
            write_status("ready");
            control_loop(listener); // never returns (until SIGTERM)
        }
        Err(e) => {
            write_status(&format!("err: {e}"));
            std::process::exit(1);
        }
    }
}

/// How long the holder waits for a client to finish sending its command line,
/// and for it to drain the reply.
///
/// BUG FIXED HERE: `control_loop` serves ONE connection at a time (deliberately
/// — it is the netns/veth/nft factory and those operations must not interleave),
/// and it used to `read_line` with **no deadline at all**. A client that
/// connected and never completed a line therefore blocked the holder *forever*,
/// and with it the control plane of every container on the node: no attach, no
/// detach, no publish, no firewall, no `cni-add`. Nothing ever recovered it —
/// there was no timeout to expire and no second thread to make progress.
///
/// It does not take a malicious peer (`SO_PEERCRED` already restricts callers to
/// the engine's own uid). The reachable trigger is ordinary: `control_query`
/// does `connect` then `write`, so any CLI descheduled, `SIGSTOP`ped, or
/// OOM-throttled in that window wedges the node. This is the same class of hang
/// that `recv_fd` already grew an `SO_RCVTIMEO` for; the holder simply never got
/// the same treatment.
///
/// Generous on purpose: the client has already written its command before the
/// holder is scheduled in the normal case, so this only ever fires on a peer
/// that is genuinely stuck. It matches the client's own 5s read timeout in
/// `control_query`.
const CONTROL_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Reads ONE command line from a control connection, bounded by
/// [`CONTROL_IO_TIMEOUT`]. `None` when the peer sent nothing in time, hung up,
/// or the deadline could not be armed — in every case the caller drops the
/// connection and moves on to the next one instead of blocking the holder.
///
/// Extracted from `control_loop` so the deadline is exercisable by a test
/// without standing up a real holder (which would need a userns/netns and would
/// run `nft` for real).
/// How long a caller waits for the holder's reply. Generous on purpose: the
/// holder serializes every mutating command, so under a burst the wait is the
/// queue ahead of you, not a hang. It is bounded anyway — a caller that waits
/// forever on a wedged holder is the failure this whole pair of timeouts exists
/// to avoid.
const CONTROL_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn read_control_line(stream: &std::os::unix::net::UnixStream) -> Option<String> {
    use std::io::{BufRead, BufReader};
    // Fail CLOSED: if the deadline cannot be armed we refuse the connection
    // rather than fall back to the unbounded read this exists to prevent.
    stream.set_read_timeout(Some(CONTROL_IO_TIMEOUT)).ok()?;
    let mut line = String::new();
    match BufReader::new(stream).read_line(&mut line) {
        Ok(0) => None, // peer hung up without sending anything
        Ok(_) => Some(line),
        Err(_) => None, // includes WouldBlock/TimedOut once the deadline fires
    }
}

/// Accepts connections on the control socket and serves one command per connection (the netns/veth
/// factory). Runs INSIDE the holder, so the `ip`/`ip netns` operations stay
/// in the infra netns without `nsenter`. Synchronous (one attach at a time — sufficient).
///
/// Both halves of the exchange are bounded by [`CONTROL_IO_TIMEOUT`] — see there
/// for why. **Residual, deliberately not closed here**: a `handle_control` that
/// itself hangs (an `nft`/`ip` blocked on a netlink lock) still stalls the loop.
/// Fixing that needs the dispatch to move off this thread, which would break the
/// serialization the factory depends on — a separate change with its own design,
/// not a timeout.
/// How long a caller waits for a MUTATING command before being told the factory
/// is busy. Generous: a legitimate `cni-add` runs external plugins, and an
/// `attach` runs several `ip` invocations.
const CONTROL_WORK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Verbs that only READ holder state and mutate nothing.
///
/// These are the ones worth serving off the serialized worker: they cannot
/// corrupt the netns/veth/nft factory by interleaving, and keeping them
/// answerable is what lets an operator still SEE the node while a mutating
/// command is stuck. Anything not listed here is treated as mutating —
/// fail-closed, so a verb added later is serialized by default rather than
/// silently racing the factory.
fn is_readonly_verb(line: &str) -> bool {
    matches!(
        line.split_whitespace().next().unwrap_or(""),
        "ping" | "has-netns" | "fwstats" | "egress-show"
    )
}

/// Accepts connections on the control socket and serves one command per connection (the netns/veth
/// factory). Runs INSIDE the holder, so the `ip`/`ip netns` operations stay
/// in the infra netns without `nsenter`.
///
/// **Mutating commands are still strictly serialized** — they are the netns/veth/
/// nft factory and interleaving them would corrupt state. What changed is WHERE
/// that serialization lives: a single dedicated worker owns it, instead of it
/// being an accidental property of running everything on the accept thread.
///
/// That distinction buys two things a hung `nft`/`ip` used to take away:
///
///  * **the accept loop keeps accepting**, so a caller gets a clear
///    `holder busy` after [`CONTROL_WORK_TIMEOUT`] instead of hanging forever
///    on a socket that will never answer;
///  * **read-only verbs keep being served** ([`is_readonly_verb`]) — `ping`,
///    `has-netns`, `fwstats`, `egress-show` — so the node stays observable, and
///    `net netns up`'s reconciliation can still ask which containers are served,
///    while a mutation is wedged.
///
/// **What it does NOT do, deliberately stated**: it cannot make a hung
/// `handle_control` harmless. The worker is single by design, so a command
/// stuck in a netlink lock still blocks every LATER mutation — they now fail
/// with a bounded, diagnosable error rather than never returning. Turning that
/// into real progress needs the factory itself to become interruptible, which
/// is a different piece of work.
fn control_loop(listener: std::os::unix::net::UnixListener) -> ! {
    use std::io::Write;
    use std::sync::mpsc;

    // Bounded: a flood of mutating commands must not grow memory without limit
    // while the worker is busy. Callers beyond the queue get the same busy
    // error as a timeout, which is the truth.
    let (tx, rx) = mpsc::sync_channel::<(String, mpsc::SyncSender<String>)>(64);
    std::thread::spawn(move || {
        // THE serialization point. Every mutating command in the holder runs
        // here, one at a time, in arrival order — exactly the invariant the
        // single-threaded accept loop used to provide by accident.
        for (line, reply_to) in rx {
            let _ = reply_to.try_send(handle_control(line.trim()));
        }
    });

    // SAFETY: geteuid() has no preconditions.
    let own_uid = unsafe { libc::geteuid() };
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        // SO_PEERCRED: only accepts commands from the engine's own uid — prevents a
        // non-privileged local user from driving the holder / injecting nft (CAP_NET_ADMIN).
        if peer_uid(&stream) != Some(own_uid) {
            continue;
        }
        let Some(line) = read_control_line(&stream) else {
            // Say it instead of hanging up mute. A silent close reaches the caller
            // as an EMPTY reply, and an empty reply used to print an error with
            // nothing after the colon. The peer that never wrote is usually one
            // that lost the CPU race under load, not an attacker — it deserves a
            // sentence it can act on. (The `SO_PEERCRED` mismatch above stays
            // silent on purpose: that one IS the hostile case, and it gets no
            // oracle.)
            let _ = stream.set_write_timeout(Some(CONTROL_IO_TIMEOUT));
            let _ = stream.write_all(
                b"err: no command received within the read deadline - the caller was too slow, or hung up\n",
            );
            continue;
        };
        let line = line.trim().to_string();

        let reply = if is_readonly_verb(&line) {
            // Cheap and non-mutating: answer here so the node stays observable
            // even while the factory is busy.
            handle_control(&line)
        } else {
            let (rtx, rrx) = mpsc::sync_channel::<String>(1);
            match tx.try_send((line, rtx)) {
                Ok(()) => rrx.recv_timeout(CONTROL_WORK_TIMEOUT).unwrap_or_else(|_| {
                    format!(
                        "err: holder busy — a network operation has been running for over {}s; \
                         inspect with `delonix net netns status` and see if an `nft`/`ip` is stuck\n",
                        CONTROL_WORK_TIMEOUT.as_secs()
                    )
                }),
                Err(_) => "err: holder busy — too many network operations queued\n".to_string(),
            }
        };

        // The reply is a single short line, but a peer that never reads it would
        // otherwise be able to block the holder in `write_all` once the socket
        // buffer filled — the same wedge from the other direction.
        let _ = stream.set_write_timeout(Some(CONTROL_IO_TIMEOUT));
        let _ = stream.write_all(reply.as_bytes());
    }
    std::process::exit(0);
}

/// Dispatches a control command (`attach <netns> <ip>`, `detach <netns>`,
/// `ping`) and returns the reply (`ok\n` or `err: <msg>\n`).
fn handle_control(line: &str) -> String {
    let parts: Vec<&str> = line.split_whitespace().collect();
    // CNI (rootless): the plugin runs HERE, in the holder — mapped-root and owner of the netns
    // (the host, the user's uid, wouldn't have CAP_NET_ADMIN in it). `cni-add` returns
    // the assigned IP in the reply body (`ok <cidr>`), for the host to register.
    if let ["cni-add", netns, id, ifname, hex] = parts.as_slice() {
        return match do_cni_add(netns, id, ifname, hex) {
            Ok(ip) => format!("ok {ip}\n"),
            Err(e) => format!("err: {e}\n"),
        };
    }
    // Query: FQDN IPs currently learned (in the nft set) for the bridge — for
    // `egress show`. Runs in the holder (owner of the netns where the set lives).
    if let ["egress-show", bridge] = parts.as_slice() {
        return format!("ok {}\n", egress_set_members(bridge).join(","));
    }
    // Query: the container's firewall chain WITH its counters, for `ingress/egress ls`.
    // Hex-encoded because the reply is a single line and an nft listing is not — the
    // same encoding the `firewall` command already uses in the other direction.
    if let ["fwstats", ip] = parts.as_slice() {
        if !is_ingress_ip(ip) {
            return "err: IP outside the ingress space\n".to_string();
        }
        let listing = crate::capture(
            "nft",
            &["list", "chain", "ip", INGRESS_TABLE, &fw_chain_name(ip)],
        )
        .unwrap_or_default();
        return format!("ok {}\n", hex_encode(listing.as_bytes()));
    }
    let res = match parts.as_slice() {
        ["ping"] => Ok(()),
        // 5 tokens = `default` namespace (compat with the old client); 6 = namespaced.
        ["attach", netns, ip, bridge, gateway] => do_attach(netns, ip, bridge, gateway, "default"),
        ["attach", netns, ip, bridge, gateway, ns] => do_attach(netns, ip, bridge, gateway, ns),
        // Reconciliation after a holder respawn: adopt the netns of a container
        // that is STILL RUNNING (by pid) instead of creating a fresh one. See
        // `do_reattach`. A holder from an older binary does not know this verb
        // and answers `err:` — the caller reports it rather than pretending the
        // container was recovered.
        // Does this holder already serve that netns? Lets the host side skip
        // containers that are healthy, so reconciliation is idempotent and an
        // explicit `net netns up` on a working system does not tear down and
        // rebuild every container's wire for nothing.
        ["has-netns", netns] => {
            let name = sanitize(netns);
            return if std::path::Path::new(&format!("/run/netns/{name}")).exists() {
                "ok yes\n".to_string()
            } else {
                "ok no\n".to_string()
            };
        }
        ["detach", netns] => do_detach(netns),
        // Takes the IP out of `@dlxall`/`@dlxns_*`. Its OWN line, and not part of `detach`,
        // for two reasons: `detach` carries no address, and `unfirewall` (which does) is
        // also sent by `clear_firewall` for a container that is still very much alive —
        // hanging the removal there would evict a live peer from the namespace sets.
        // Additive and sent best-effort, so an older holder just refuses it and behaves
        // exactly as before (it leaks, as it always did) instead of failing the teardown.
        ["nsleave", ip] => {
            ns_set_leave(ip);
            Ok(())
        }
        ["cni-del", netns, id, ifname, hex] => do_cni_del(netns, id, ifname, hex),
        // live multi-homing (rootless): connects/disconnects an ADDITIONAL network to a
        // container already running (extra veth to the private network's bridge).
        // 6 tokens = `default` namespace (compat with an older client, same shape as
        // `attach` above); 7 = namespaced.
        ["attach-extra", netns, ifname, ip, bridge, gateway] => {
            do_attach_extra(netns, ifname, ip, bridge, gateway, "default")
        }
        ["attach-extra", netns, ifname, ip, bridge, gateway, ns] => {
            do_attach_extra(netns, ifname, ip, bridge, gateway, ns)
        }
        ["detach-extra", netns, ifname] => do_detach_extra(netns, ifname),
        // live bandwidth limit (rootless): shaping on the infra-side veth
        // (download via tbf at the root, upload via ingress police).
        ["netrate", vh, rate, burst] => do_netrate(vh, rate, burst),
        ["netrate-clear", vh] => {
            do_netrate_clear(vh);
            Ok(())
        }
        ["netdel", bridge] => do_netdel(bridge),
        ["vmtap", tap, bridge, gateway] => do_vmtap(tap, bridge, gateway, None, None),
        // Namespaced form. Same compat idiom `attach`/`attach-extra` already use:
        // the short line keeps working against an older holder, only the
        // namespaced one needs the newer binary.
        ["vmtap", tap, bridge, gateway, ip, ns] => {
            do_vmtap(tap, bridge, gateway, Some(ip), Some(ns))
        }
        ["vmtapdel", tap] => do_vmtapdel(tap),
        ["publish", proto, host_port, cip, cport] => do_publish(proto, host_port, cip, cport),
        // 2 tokens = every proto on the port (teardown); 3 = only that proto.
        ["unpublish", host_port] => do_unpublish(host_port, None),
        ["unpublish", host_port, proto] => do_unpublish(host_port, Some(proto)),
        ["firewall", _netns, ip, hex] => do_firewall(ip, hex),
        ["unfirewall", ip] => do_unfirewall(ip),
        ["egress", policy] => do_egress(policy),
        ["egress-net", bridge, policy] => do_egress_net(bridge, policy),
        ["egress-host", bridge, suffix] => do_egress_host(bridge, suffix),
        ["l4guard", rate, max] => {
            do_l4guard(rate.parse().unwrap_or(50), max.parse().unwrap_or(200))
        }
        ["l4guard-clear"] => {
            clear_l4guard();
            Ok(())
        }
        // WireGuard over the overlay (req #6): the interface lives in the infra netns.
        ["wg-up", iface, port, priv_key, addr] => {
            crate::wg::ensure_iface(iface, priv_key, port.parse().unwrap_or(51820), addr)
        }
        ["wg-peer", iface, pub_key, endpoint, allowed] => crate::wg::set_peer(
            iface,
            &crate::wg::Peer {
                public: pub_key.to_string(),
                endpoint: endpoint.to_string(),
                allowed_ips: allowed.split(',').map(str::to_string).collect(),
            },
        ),
        // VXLAN uplink of an overlay network (the L2 shared between nodes). `dsts` = the
        // FDB destinations (`wg_ip` if encrypted, otherwise `node_ip`; `-` = no peers).
        ["vxlan", dev, vni, bridge, gateway, dsts] => do_vxlan(dev, vni, bridge, gateway, dsts),
        _ => Err(Error::Invalid(format!("invalid control command: {line:?}"))),
    };
    match res {
        Ok(()) => "ok\n".to_string(),
        Err(e) => format!("err: {e}\n"),
    }
}

/// Ensures a network's BRIDGE in the infra netns (the gateway is ALWAYS the ingress):
/// creates `<bridge>` with `<gateway>/16` if missing, and ISOLATES it from the other
/// delonix bridges (forward drop between networks, like docker) — but egress (oifname tap0)
/// and intra-network communication remain. Idempotent.
fn ensure_net_bridge(bridge: &str, gateway: &str) -> Result<()> {
    let exists = crate::capture("ip", &["link", "show", bridge])
        .map(|o| o.contains(bridge))
        .unwrap_or(false);
    if !exists {
        run("ip", &["link", "add", bridge, "type", "bridge"])?;
        run(
            "ip",
            &["addr", "add", &format!("{gateway}/16"), "dev", bridge],
        )?;
        run("ip", &["link", "set", bridge, "up"])?;
        // IPv6 (ULA): gateway on the bridge + v6 forwarding (best-effort).
        let p = prefix_of(gateway);
        run_ok(
            "ip",
            &[
                "-6",
                "addr",
                "add",
                &format!("{}/64", v6_gw(&p)),
                "dev",
                bridge,
            ],
        );
        let _ = std::fs::write("/proc/sys/net/ipv6/conf/all/forwarding", "1");
    }
    // INTRA-network connectivity: containers on the SAME bridge talk to each other (Docker/
    // user-network model, like `delonix0`). Without this rule, the `forward`'s `policy drop`
    // cut ALL intra-bridge traffic of the created networks (`dlxn*`) —
    // services on the same network (incl. within a tenant) couldn't reach each other. The
    // fine micro-segmentation is done later with `kind:NetworkPolicy`. Idempotent.
    let fchain = crate::capture("nft", &["list", "chain", "ip", INGRESS_TABLE, "forward"])
        .unwrap_or_default();
    let self_accept = format!("iifname \"{bridge}\" oifname \"{bridge}\" accept");
    if !fchain.contains(&self_accept) {
        run_ok(
            "nft",
            &[
                "add",
                "rule",
                "ip",
                INGRESS_TABLE,
                "forward",
                "iifname",
                bridge,
                "oifname",
                bridge,
                "accept",
            ],
        );
    }
    // inter-network isolation: forward drop between this bridge and the other delonix ones.
    let listed =
        crate::capture("ip", &["-o", "link", "show", "type", "bridge"]).unwrap_or_default();
    let fwd = crate::capture("nft", &["list", "chain", "ip", INGRESS_TABLE, "fwdeny"])
        .unwrap_or_default();
    for line in listed.lines() {
        let other = line
            .split(':')
            .nth(1)
            .map(|s| s.trim().split('@').next().unwrap_or("").trim())
            .unwrap_or("");
        if other.is_empty()
            || other == bridge
            || (other != INFRA_BRIDGE && !other.starts_with("dlxn"))
        {
            continue; // only isolate against delonix0 and other dlxn* networks
        }
        for (a, b) in [(bridge, other), (other, bridge)] {
            let needle = format!("iifname \"{a}\" oifname \"{b}\" drop");
            if !fwd.contains(&needle) {
                run_ok(
                    "nft",
                    &[
                        "add",
                        "rule",
                        "ip",
                        INGRESS_TABLE,
                        "fwdeny",
                        "iifname",
                        a,
                        "oifname",
                        b,
                        "drop",
                    ],
                );
            }
        }
    }
    // the network's DHCP server (for VMs/clients that request an IP).
    start_dhcp(bridge, &prefix_of(gateway));
    // Re-applies the PERSISTED egress intent when the bridge is (re)created — it's what
    // makes it survive the holder's respawn (the nft and the FQDN registry live in the
    // ephemeral netns). Only on `!exists` (new bridge): idempotent and cheap.
    if !exists {
        if let Some(def) = network_list().into_iter().find(|d| d.bridge == bridge) {
            if def.egress.policy.is_some() || !def.egress.hosts.is_empty() {
                let _ = apply_egress_from_state(bridge, &def.egress);
            }
        }
    }
    Ok(())
}

/// Bridges that already have the native DHCP server running (one thread per bridge).
static DHCP_STARTED: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Starts a network bridge's **NATIVE** (Rust) DHCP server, if it isn't already
/// running. Replaces `busybox udhcpd` — the holder becomes self-contained
/// (no dependency on host binaries). One thread per bridge.
fn start_dhcp(bridge: &str, prefix: &str) {
    {
        let mut s = DHCP_STARTED.lock().unwrap();
        if !s.insert(bridge.to_string()) {
            return; // already has a DHCP server
        }
    }
    let (b, p) = (bridge.to_string(), prefix.to_string());
    std::thread::spawn(move || dhcp_serve(b, p));
}

/// The IPv4 address the holder's native DHCP server will hand to `mac` on a
/// bridge whose `prefix` is `<o0>.<o1>` — pool `<prefix>.254.10–.254.249`.
///
/// Deterministic from the MAC, and deliberately so: it is the ONLY reason the
/// HOST side can know a VM's address before the guest has even booted, which is
/// what lets `vm_attach` place that address under namespace isolation at attach
/// time instead of waiting for a lease it has no way to observe (the DHCP
/// exchange happens inside the holder, minutes later, and for a guest that may
/// never come up at all).
///
/// Shared by the server (`dhcp_serve`) and the attach path on purpose. Two
/// copies of this arithmetic would diverge the day the pool changes, and the
/// symptom would be the worst kind: a VM firewalled at an address nobody uses,
/// reported as isolated.
pub fn dhcp_lease_ip(prefix: &str, mac: &str) -> Option<String> {
    let oct: Vec<u8> = prefix.split('.').filter_map(|x| x.parse().ok()).collect();
    if oct.len() != 2 {
        return None;
    }
    // The server hashes the MAC as it renders it off the wire: lowercase,
    // `:`-separated. Normalizing here (and not at each call site) is what stops
    // an upper-case MAC from a record producing a different, unused address.
    let host = 10 + (crate::fnv32(&mac.to_lowercase()) % 240) as u8; // pool .254.10–.254.249
    Some(format!("{}.{}.254.{}", oct[0], oct[1], host))
}

/// Native DHCPv4 server of a bridge: listens on UDP `:67` (only on that bridge, via
/// `SO_BINDTODEVICE`) and responds to DISCOVER/REQUEST with an IP from the pool
/// `<prefix>.254.10–.254.250` (deterministic from the MAC), **gateway/DNS = ingress**.
fn dhcp_serve(bridge: String, prefix: String) {
    use std::os::unix::io::FromRawFd;
    let oct: Vec<u8> = prefix.split('.').filter_map(|x| x.parse().ok()).collect();
    if oct.len() != 2 {
        return;
    }
    let (o0, o1) = (oct[0], oct[1]);
    let gw = [o0, o1, 0, 1]; // gateway/server/DNS = <prefix>.0.1 (the ingress)
                             // SAFETY: UDP socket; setsockopt REUSEADDR/PORT/BROADCAST/BINDTODEVICE; bind :67.
    let sock = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return;
        }
        let one: libc::c_int = 1;
        let so = |n| {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                n,
                &one as *const _ as *const libc::c_void,
                4,
            )
        };
        so(libc::SO_REUSEADDR);
        so(libc::SO_REUSEPORT);
        so(libc::SO_BROADCAST);
        let bn = bridge.as_bytes();
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            bn.as_ptr() as *const libc::c_void,
            bn.len() as u32,
        );
        let mut a: libc::sockaddr_in = std::mem::zeroed();
        a.sin_family = libc::AF_INET as u16;
        a.sin_port = 67u16.to_be();
        a.sin_addr.s_addr = 0; // INADDR_ANY
        if libc::bind(
            fd,
            &a as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as u32,
        ) != 0
        {
            libc::close(fd);
            return;
        }
        std::net::UdpSocket::from_raw_fd(fd)
    };
    let mut buf = [0u8; 1024];
    loop {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if n < 240 || buf[236..240] != [99, 130, 83, 99] {
            continue; // BOOTP + magic cookie
        }
        let reply_type = match dhcp_opt(&buf[240..n], 53).and_then(|v| v.first().copied()) {
            Some(1) => 2u8, // DISCOVER → OFFER
            Some(3) => 5u8, // REQUEST → ACK
            _ => continue,
        };
        let mac = &buf[28..34];
        let macs = mac
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":");
        // Same arithmetic the host side used at attach time — see `dhcp_lease_ip`.
        let host = match dhcp_lease_ip(&prefix, &macs)
            .and_then(|ip| ip.rsplit('.').next().and_then(|h| h.parse::<u8>().ok()))
        {
            Some(h) => h,
            None => continue,
        };
        let yi = [o0, o1, 254, host];
        let mut r = vec![0u8; 240];
        r[0] = 2; // BOOTREPLY
        r[1] = 1; // htype ethernet
        r[2] = 6; // hlen
        r[4..8].copy_from_slice(&buf[4..8]); // xid
        r[10..12].copy_from_slice(&buf[10..12]); // flags
        r[16..20].copy_from_slice(&yi); // yiaddr
        r[20..24].copy_from_slice(&gw); // siaddr (server)
        r[28..34].copy_from_slice(mac); // chaddr
        r[236..240].copy_from_slice(&[99, 130, 83, 99]); // magic
        r.extend_from_slice(&[53, 1, reply_type]); // message type
        r.extend_from_slice(&[54, 4]);
        r.extend_from_slice(&gw); // server id
        r.extend_from_slice(&[51, 4]);
        r.extend_from_slice(&3600u32.to_be_bytes()); // lease time
        r.extend_from_slice(&[1, 4, 255, 255, 0, 0]); // subnet mask /16
        r.extend_from_slice(&[3, 4]);
        r.extend_from_slice(&gw); // router
        r.extend_from_slice(&[6, 4]);
        r.extend_from_slice(&gw); // DNS (our server)
        r.push(255); // end
        let _ = sock.send_to(&r, "255.255.255.255:68");
    }
}

/// Extracts the value of a DHCP option (TLV) from the options block.
fn dhcp_opt(opts: &[u8], want: u8) -> Option<Vec<u8>> {
    let mut i = 0;
    while i < opts.len() {
        let code = opts[i];
        if code == 255 {
            break;
        }
        if code == 0 {
            i += 1;
            continue;
        }
        if i + 1 >= opts.len() {
            break;
        }
        let len = opts[i + 1] as usize;
        if i + 2 + len > opts.len() {
            break;
        }
        if code == want {
            return Some(opts[i + 2..i + 2 + len].to_vec());
        }
        i += 2 + len;
    }
    None
}

// ---- ingress IPv6 (ULA): fd00:<2nd octet>::/64 per network -----------------
//
// **IPv6 IS REFUSED BY DEFAULT — and this is a security fix, not a feature removal.**
//
// The SDN used to hand every container an IPv6 ULA derived from its v4 address
// (`fd00:<o2>::<o3>:<o4>`) while the ENTIRE firewall lives in `table ip` — v4 only.
// That is a second, completely unfiltered data path to every container. Reproduced
// live: with the firewall dropping on IPv4, the same target answered on port 80 over
// its ULA. It defeats `ingress`/`egress` rules, `policy deny`, namespace isolation,
// `kind: Dependency` and the L4 guard alike, because all of them are `table ip`.
// Discovery is a single `ping -6 ff02::1%eth0`, which enumerates every neighbour on
// the bridge, and the address is derivable from the v4 one anyway.
//
// Two independent layers close it, deliberately not one:
//   1. `disable_ipv6` inside the container's netns — no ULA, no link-local, no
//      addresses at all. Does not depend on any host setting.
//   2. `table ip6 dlxing` with `forward policy drop` in the holder — catches whatever
//      still routes v6, e.g. a PRIVILEGED container that remounts `/proc/sys` rw and
//      turns v6 back on. Depends on `bridge-nf-call-ip6tables`, which is why it is the
//      SECOND layer and not the only one.
//
// Nothing was lost: there is no v6 uplink (`slirp4netns` runs without
// `--enable-ipv6`, so v6 to the Internet was always `Network is unreachable`) and the
// internal resolver only ever answered A records. The ULA served east-west traffic
// that no policy could govern — which is precisely the problem.
//
// `DELONIX_ENABLE_IPV6=1` restores the old behaviour, loudly, for whoever needs the
// v6 SDN and accepts that no firewall rule applies to it. Same escape-hatch shape as
// `DELONIX_FORWARD_POLICY=accept`. Real dual-stack — `table inet` plus a v6 IPAM and
// v6 sets — is separate work; this is the explicit refusal that must exist until then.

/// Is the (unfiltered) IPv6 SDN explicitly opted back in?
pub fn ipv6_sdn_enabled() -> bool {
    let on = std::env::var("DELONIX_ENABLE_IPV6").ok().as_deref() == Some("1");
    if on {
        tracing::warn!(
            "SECURITY WARNING — DELONIX_ENABLE_IPV6=1: containers get IPv6 addresses that \
             NO firewall rule governs (ingress/egress, policy deny, namespace isolation and \
             Dependency are all IPv4-only). Any container can reach any other over IPv6, \
             bypassing every policy. For debugging only — do NOT use in production."
        );
    }
    on
}

/// The v6 refusal table: forwarding of IPv6 dies in the holder's netns. Second layer
/// of the fix above — [`disable_ipv6_argv`] is the first. Its own table so it is as
/// isolated and identifiable as `dlxing`, and so tearing one down never touches the
/// other.
pub fn ingress_v6_refusal_ruleset() -> String {
    format!(
        "table ip6 {INGRESS_TABLE} {{\n\
         \x20 chain forward {{ type filter hook forward priority -10; policy drop;\n\
         \x20\x20 counter\n\
         \x20 }}\n\
         }}\n"
    )
}

/// Turns IPv6 off in EVERY netns the holder currently owns — the already-running
/// containers, without restarting a single one.
///
/// This exists because the refusal is applied at `attach` time, which closes the hole
/// for containers created from now on and does NOTHING for the ones already up. Telling
/// the operator to restart them would be the wrong answer in this engine: hot
/// reconfiguration is the whole point (`container update` changes ports, volumes and
/// networks with the PID unchanged), and the dataplane does not belong to the
/// container's process lifecycle. A netns is entered and its sysctls written whenever
/// the holder feels like it — the container never notices.
///
/// Idempotent and best-effort per netns: one that disappears mid-sweep (a container
/// stopping concurrently) must not abort the rest. Returns how many were hardened.
///
/// Deliberately NOT a control-socket verb, which was the first shape tried. The case
/// this exists for is an in-place upgrade: the new binary is installed and the OLD
/// holder is still running (see `stale_holder_message`) with every container attached
/// to it. An old holder does not know a verb added today, so a control command would
/// fail in exactly the scenario that matters. Entering its namespaces from the host
/// works whatever binary the holder came from — the same `nsenter` the L7 proxy
/// already uses (`infra_join_argv`), and the same one that was used by hand to prove
/// this is possible at all.
pub fn disable_ipv6_live() -> Result<usize> {
    let Some(holder) = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p)) else {
        return Ok(0); // holder down: nothing is running to protect
    };
    // `-m` as well as `-U -n`: `ip netns exec` reads `/run/netns`, which lives in the
    // holder's MOUNT namespace.
    let join = |args: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = [
            "-t",
            &holder.to_string(),
            "-U",
            "-m",
            "-n",
            "--preserve-credentials",
            "--",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        v.extend(args.iter().map(|s| s.to_string()));
        v
    };
    let listed = {
        let a = join(&["ip", "netns", "list"]);
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        crate::capture("nsenter", &refs).unwrap_or_default()
    };
    let mut n = 0;
    for line in listed.lines() {
        // `ip netns list` prints `<name>` or `<name> (id: N)`.
        let Some(ns) = line.split_whitespace().next().filter(|s| !s.is_empty()) else {
            continue;
        };
        let ns = sanitize(ns);
        if ns.is_empty() {
            continue;
        }
        for argv in disable_ipv6_argv(&ns) {
            let mut a = vec!["ip".to_string()];
            a.extend(argv);
            let full = join(&a.iter().map(String::as_str).collect::<Vec<_>>());
            let refs: Vec<&str> = full.iter().map(String::as_str).collect();
            run_ok("nsenter", &refs);
        }
        n += 1;
    }
    Ok(n)
}

/// `ip` args that turn IPv6 off inside a container's netns. Pure, so the exact
/// invocation is testable without a kernel.
///
/// `all` alone is not enough: the per-interface knob wins for an interface that
/// already exists, and `eth0` is moved in before this runs. Setting `default` too
/// means an interface created later (an additional network) starts off as well.
pub fn disable_ipv6_argv(netns: &str) -> Vec<Vec<String>> {
    ["all", "default", "eth0"]
        .iter()
        .map(|k| {
            vec![
                "netns".to_string(),
                "exec".to_string(),
                netns.to_string(),
                "sysctl".to_string(),
                "-q".to_string(),
                "-w".to_string(),
                format!("net.ipv6.conf.{k}.disable_ipv6=1"),
            ]
        })
        .collect()
}

/// A network's IPv6 group from the `/16` prefix (`10.201` → `201`).
fn v6_group(v4prefix: &str) -> String {
    v4prefix.rsplit('.').next().unwrap_or("200").to_string()
}
/// A network's IPv6 gateway (= ingress): `fd00:<group>::1`.
fn v6_gw(v4prefix: &str) -> String {
    format!("fd00:{}::1", v6_group(v4prefix))
}
/// Deterministic IPv6 ULA of an ingress v4 IP: `fd00:<o2>::<o3>:<o4>`.
fn v6_of(ip4: &str) -> Option<String> {
    let o: Vec<&str> = ip4.split('.').collect();
    if o.len() != 4 {
        return None;
    }
    Some(format!("fd00:{}::{}:{}", o[1], o[2], o[3]))
}

/// `/16` prefix (`10.x`) from an IP/gateway (`10.x.y.z`).
fn prefix_of(ip: &str) -> String {
    let o: Vec<&str> = ip.split('.').collect();
    if o.len() >= 2 {
        format!("{}.{}", o[0], o[1])
    } else {
        INFRA_PREFIX.to_string()
    }
}

/// Rootless CNI (holder): creates an EMPTY netns and delegates its configuration to the
/// CNI plugins (`crate::cni::add`) — the bridge/veth/IPAM are the plugin's, not the native
/// SDN's. Runs in the holder (mapped-root, owner of the netns → CAP_NET_ADMIN). Returns the
/// IP (CIDR) assigned by the CNI's IPAM. `hex` = the conflist JSON in hex.
fn do_cni_add(netns: &str, id: &str, ifname: &str, hex: &str) -> Result<String> {
    let netns = sanitize(netns);
    let bytes = hex_decode(hex).ok_or_else(|| Error::Invalid("invalid conflist hex".into()))?;
    let conf = crate::cni::parse_config(&String::from_utf8_lossy(&bytes))?;
    // empty netns (the plugin moves the veth there); clears leftovers of attempts.
    run_ok("ip", &["netns", "del", &netns]);
    run("ip", &["netns", "add", &netns])?;
    let path = format!("/run/netns/{netns}");
    match crate::cni::add(&conf, &crate::cni::plugin_dirs(), id, &path, ifname) {
        Ok(r) => Ok(r.ips.first().map(|i| i.address.clone()).unwrap_or_default()),
        Err(e) => {
            // rollback: doesn't leave the netns orphan if the plugin failed.
            run_ok("ip", &["netns", "del", &netns]);
            Err(e)
        }
    }
}

/// Rootless CNI (holder): runs the plugins' `DEL` and removes the netns. Best-effort.
fn do_cni_del(netns: &str, id: &str, ifname: &str, hex: &str) -> Result<()> {
    let netns = sanitize(netns);
    if let Some(bytes) = hex_decode(hex) {
        if let Ok(conf) = crate::cni::parse_config(&String::from_utf8_lossy(&bytes)) {
            let path = format!("/run/netns/{netns}");
            let _ = crate::cni::del(&conf, &crate::cni::plugin_dirs(), id, &path, ifname);
        }
    }
    run_ok("ip", &["netns", "del", &netns]);
    Ok(())
}

/// Creates a container's netns and attaches it to its network's BRIDGE via `veth`: pair
/// `<vh>`↔`eth0`, `vh` on the bridge, `eth0` in the netns with `<ip>/16` and default route
/// through `<gateway>` (= the ingress). Creates the network's bridge if missing. Runs in the holder.
/// Registers a container's IP in the namespace sets: `@dlxall` (all container
/// IPs) + `@dlxns_<ns>` (the container's namespace). Beforehand, **removes it from
/// any previous `@dlxns_*`** — so a re-attach (or namespace change)
/// stays correct without needing cleanup on detach. Best-effort/idempotent.
/// Removes `elem` from EVERY `@dlxns_*` set (the set name is the 2nd token of `set X {`).
/// Shared by join (which moves an IP between namespaces) and leave (which takes it out for
/// good) — two copies of this scan would drift the day the naming changes.
fn drop_from_every_ns_set(elem: &str) {
    let sets = crate::capture("nft", &["list", "sets", "ip", INGRESS_TABLE]).unwrap_or_default();
    for line in sets.lines() {
        if let Some(name) = line.split_whitespace().nth(1) {
            if name.starts_with("dlxns") {
                run_ok(
                    "nft",
                    &["delete", "element", "ip", INGRESS_TABLE, name, elem],
                );
            }
        }
    }
}

/// Takes an IP OUT of the namespace sets, on detach. The counterpart `ns_set_join` existed
/// from the start and this did not, so `@dlxall` only ever grew: measured at **49 elements
/// for 8 live veths and 5 registered containers** on a development host
/// (`docs/discovery/46_GAPS_ENCONTRADOS.md` §4.3).
///
/// Not a policy leak — `@dlxall` is only ever read to *drop*
/// (`ip saddr @dlxall ct state new drop`), so a stale element never opens anything. It is
/// unbounded kernel state and, worse for whoever is debugging, a set that cannot answer the
/// question it exists to answer: which addresses on this node belong to containers.
fn ns_set_leave(ip: &str) {
    if !is_ingress_ip(ip) {
        return; // only SDN IPs
    }
    let elem = format!("{{ {ip} }}");
    run_ok(
        "nft",
        &["delete", "element", "ip", INGRESS_TABLE, DLXALL_SET, &elem],
    );
    drop_from_every_ns_set(&elem);
}

fn ns_set_join(ip: &str, ns: &str) {
    if !is_ingress_ip(ip) {
        return; // only SDN IPs
    }
    let elem = format!("{{ {ip} }}");
    run_ok(
        "nft",
        &["add", "element", "ip", INGRESS_TABLE, DLXALL_SET, &elem],
    );
    // takes the IP out of any previous namespace (set name = 2nd token of "set X {").
    drop_from_every_ns_set(&elem);
    let nsset = dlxns_set(ns);
    run_ok(
        "nft",
        &[
            "add",
            "set",
            "ip",
            INGRESS_TABLE,
            &nsset,
            "{ type ipv4_addr; }",
        ],
    );
    run_ok(
        "nft",
        &["add", "element", "ip", INGRESS_TABLE, &nsset, &elem],
    );
}

fn do_attach(netns: &str, ip: &str, bridge: &str, gateway: &str, namespace: &str) -> Result<()> {
    let netns = sanitize(netns);
    let bridge = sanitize(bridge);
    ensure_net_bridge(&bridge, gateway)?;
    let vh = vh_name(&netns);
    // clears leftovers of a previous attempt (best-effort).
    run_ok("ip", &["netns", "del", &netns]);
    run_ok("ip", &["link", "del", &vh]);
    run("ip", &["netns", "add", &netns])?;
    run(
        "ip",
        &["link", "add", &vh, "type", "veth", "peer", "name", "eth0"],
    )?;
    run("ip", &["link", "set", &vh, "master", &bridge])?;
    run("ip", &["link", "set", &vh, "up"])?;
    run("ip", &["link", "set", "eth0", "netns", &netns])?;
    let cidr = format!("{ip}/16");
    for argv in [
        vec!["netns", "exec", &netns, "ip", "link", "set", "lo", "up"],
        vec![
            "netns", "exec", &netns, "ip", "addr", "add", &cidr, "dev", "eth0",
        ],
        vec!["netns", "exec", &netns, "ip", "link", "set", "eth0", "up"],
        vec![
            "netns", "exec", &netns, "ip", "route", "add", "default", "via", gateway,
        ],
    ] {
        run("ip", &argv)?;
    }
    // IPv6: REFUSED by default — see the long note on `ipv6_sdn_enabled`. Turning it
    // off here, in the container's own netns, removes the ULA *and* the kernel's
    // link-local, so there is no v6 address left to reach anything with. Opting back
    // in restores the ULA + v6 default route exactly as before.
    if ipv6_sdn_enabled() {
        let p = prefix_of(gateway);
        let gw6 = v6_gw(&p);
        if let Some(v6) = v6_of(ip) {
            let cidr6 = format!("{v6}/64");
            run_ok(
                "ip",
                &[
                    "netns", "exec", &netns, "ip", "-6", "addr", "add", &cidr6, "dev", "eth0",
                    "nodad",
                ],
            );
            run_ok(
                "ip",
                &[
                    "netns", "exec", &netns, "ip", "-6", "route", "add", "default", "via", &gw6,
                ],
            );
        }
    } else {
        for argv in disable_ipv6_argv(&netns) {
            let args: Vec<&str> = argv.iter().map(String::as_str).collect();
            run_ok("ip", &args);
        }
    }
    // ANTI-SPOOFING: traffic entering from this veth MUST have the assigned IP as
    // source — otherwise a container could forge the source-IP and bypass the per-IP
    // firewall / the isolation / the flow assignment. `insert` puts the rule at the top
    // of the `forward`, before the per-container jumps. Idempotent (clears first).
    //
    // NET-06 (known limitation): for a PRIVILEGED Kind node, pod→pod traffic
    // stays inside the node's netns (never crosses this veth) and pod→outside leaves with
    // `saddr`=node-IP (kindnet masquerades), so single-node works. A
    // MULTI-NODE scenario with pod-CIDR routing (10.244/16) BETWEEN nodes would be DROPped
    // here (pod's saddr ≠ node-IP). While multi-node isn't supported, this is
    // latent; the fix will be an anti-spoof exception for the pod-CIDR when the
    // container is a cluster node (alongside the inter-node routing work).
    clear_antispoof(&vh);
    run_ok("nft", &antispoof_rule_args(&vh, ip));
    // Namespace isolation: registers the IP in @dlxall + @dlxns_<ns> (the
    // container's fw_chain_body references these sets). Behavior unchanged
    // for everything in `default` (the same namespace contains all = open SDN).
    ns_set_join(ip, namespace);
    Ok(())
}

/// Removes a container's netns (and, with it, the `eth0`; the orphan `vh` is cleaned up
/// next). Best-effort.
fn do_detach(netns: &str) -> Result<()> {
    let netns = sanitize(netns);
    let vh = vh_name(&netns);
    clear_antispoof(&vh);
    run_ok("ip", &["netns", "del", &netns]);
    run_ok("ip", &["link", "del", &vh]);
    Ok(())
}

/// Attaches an ADDITIONAL network to an ALREADY-RUNNING container (live multi-homing): a
/// second `veth` from the existing netns to the private network's bridge. Does not create the
/// netns (it already exists) and does NOT touch the default route (the primary network keeps it).
fn do_attach_extra(
    netns: &str,
    ifname: &str,
    ip: &str,
    bridge: &str,
    gateway: &str,
    namespace: &str,
) -> Result<()> {
    let netns = sanitize(netns);
    let ifname = sanitize(ifname);
    let bridge = sanitize(bridge);
    ensure_net_bridge(&bridge, gateway)?;
    let vh = vh_name_extra(&netns, &ifname);
    run_ok("ip", &["link", "del", &vh]); // clears leftovers
    run(
        "ip",
        &["link", "add", &vh, "type", "veth", "peer", "name", &ifname],
    )?;
    run("ip", &["link", "set", &vh, "master", &bridge])?;
    run("ip", &["link", "set", &vh, "up"])?;
    run("ip", &["link", "set", &ifname, "netns", &netns])?;
    let cidr = format!("{ip}/16");
    for argv in [
        vec![
            "netns", "exec", &netns, "ip", "addr", "add", &cidr, "dev", &ifname,
        ],
        vec!["netns", "exec", &netns, "ip", "link", "set", &ifname, "up"],
    ] {
        run("ip", &argv)?;
    }
    // IPv6: REFUSED by default, same as the primary interface. An additional network
    // is exactly where this mattered most — a multi-homed container carried a second
    // unfiltered v6 address, on a second bridge, and the firewall governed neither.
    // The `default` knob set on the primary attach covers an interface created later,
    // but this is not left to that: `--net-connect` can land on a container that
    // predates the fix.
    if ipv6_sdn_enabled() {
        if let Some(v6) = v6_of(ip) {
            let cidr6 = format!("{v6}/64");
            run_ok(
                "ip",
                &[
                    "netns", "exec", &netns, "ip", "-6", "addr", "add", &cidr6, "dev", &ifname,
                    "nodad",
                ],
            );
        }
    } else {
        run_ok(
            "ip",
            &[
                "netns",
                "exec",
                &netns,
                "sysctl",
                "-q",
                "-w",
                &format!("net.ipv6.conf.{ifname}.disable_ipv6=1"),
            ],
        );
    }
    // ANTI-SPOOFING also on the additional interface (same per-IP guarantee as eth0).
    clear_antispoof(&vh);
    run_ok("nft", &antispoof_rule_args(&vh, ip));
    // Namespace isolation on the ADDITIONAL IP too. Its absence here was a real
    // bypass, not a theoretical one: the cross-namespace drop only fires for sources in
    // `@dlxall`, so two containers in different namespaces, both connected to a shared
    // second network, reached each other freely over it — reproduced live (teamA ↔ teamB
    // blocked on the primary IPs, REACHABLE on the extra ones). `do_attach` has always
    // done this for the primary; the extra path simply never did.
    ns_set_join(ip, namespace);
    Ok(())
}

/// Detaches an additional network: removes the extra `veth` (takes the container's netns
/// `<ifname>` with it). Best-effort.
fn do_detach_extra(netns: &str, ifname: &str) -> Result<()> {
    let netns = sanitize(netns);
    let ifname = sanitize(ifname);
    let vh = vh_name_extra(&netns, &ifname);
    clear_antispoof(&vh);
    run_ok("ip", &["link", "del", &vh]);
    Ok(())
}

/// Applies bandwidth shaping on the veth `vh` (infra side), INSIDE the
/// infra netns (runs in the holder). Same rate in both directions:
/// DOWNLOAD (host→container) = tbf at the root; UPLOAD (container→host) = ingress
/// `police`+`drop`. `rate`/`burst` already come in bit/s and bytes. Idempotent.
fn do_netrate(vh: &str, rate: &str, burst: &str) -> Result<()> {
    let vh = sanitize(vh);
    let r = format!("{}bit", rate.parse::<u64>().unwrap_or(0).max(8000));
    let b = burst.to_string();
    do_netrate_clear(&vh); // clean reapplication
    run(
        "tc",
        &[
            "qdisc", "add", "dev", &vh, "root", "tbf", "rate", &r, "burst", &b, "latency", "50ms",
        ],
    )?;
    run(
        "tc",
        &["qdisc", "add", "dev", &vh, "handle", "ffff:", "ingress"],
    )?;
    run(
        "tc",
        &[
            "filter", "add", "dev", &vh, "parent", "ffff:", "protocol", "all", "prio", "1", "u32",
            "match", "u32", "0", "0", "police", "rate", &r, "burst", &b, "drop",
        ],
    )?;
    Ok(())
}

/// Removes the shaping from the veth `vh` (best-effort). Deleting the veth already takes the qdiscs;
/// we clear by hand for reapplication and orphans.
fn do_netrate_clear(vh: &str) {
    let vh = sanitize(vh);
    run_ok("tc", &["qdisc", "del", "dev", &vh, "root"]);
    run_ok(
        "tc",
        &["qdisc", "del", "dev", &vh, "handle", "ffff:", "ingress"],
    );
}

/// The anti-spoofing rule for an interface pinned to a single address: anything
/// arriving on `iface` whose source is not `ip` is dropped.
///
/// ONE definition shared by all three attach paths (container veth, extra veth,
/// VM tap). They had drifted apart before — the VM tap simply never got the rule,
/// which let a guest kernel forge a source address and bypass namespace isolation
/// and `kind: Dependency` alike. Keeping the argv in a single function is also
/// what lets `clear_antispoof` stay in step with what is emitted: the same
/// generator-and-reader-share-the-format discipline `fw_rule_tail` already
/// follows.
fn antispoof_rule_args<'a>(iface: &'a str, ip: &'a str) -> [&'a str; 12] {
    [
        "insert",
        "rule",
        "ip",
        INGRESS_TABLE,
        "fwdeny",
        "iifname",
        iface,
        "ip",
        "saddr",
        "!=",
        ip,
        "drop",
    ]
}

/// Removes a veth's anti-spoofing rules from the `forward` (idempotency).
fn clear_antispoof(vh: &str) {
    let listed = crate::capture(
        "nft",
        &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"],
    )
    .unwrap_or_default();
    let needle = format!("iifname \"{vh}\"");
    for line in listed.lines() {
        if line.contains(&needle) && line.contains("saddr") && line.contains("drop") {
            if let Some(h) = line
                .rsplit("# handle ")
                .next()
                .and_then(|x| x.trim().parse::<u32>().ok())
            {
                run_ok(
                    "nft",
                    &[
                        "delete",
                        "rule",
                        "ip",
                        INGRESS_TABLE,
                        "fwdeny",
                        "handle",
                        &h.to_string(),
                    ],
                );
            }
        }
    }
}

/// Creates a `tap` for a VM, attached to its network's BRIDGE (creates the bridge + DHCP if
/// missing). QEMU (running in the infra netns) uses this tap; the guest gets an IP from the
/// network's udhcpd (gateway = ingress). Runs in the holder.
/// `ip`/`namespace` are `None` for the legacy 4-token control line (a VM in the
/// `default` namespace, and any caller running an older binary): the tap is
/// created exactly as before and nothing joins the namespace sets. With them,
/// the VM's future DHCP address is registered in `@dlxall`/`@dlxns_<ns>` — the
/// same membership `do_attach` gives a container, which is half of what makes
/// the isolation hold (the other half is the chain, installed host-side by
/// `apply_firewall`).
fn do_vmtap(
    tap: &str,
    bridge: &str,
    gateway: &str,
    ip: Option<&str>,
    namespace: Option<&str>,
) -> Result<()> {
    let tap = sanitize(tap);
    let bridge = sanitize(bridge);
    ensure_net_bridge(&bridge, gateway)?;
    run_ok("ip", &["link", "del", &tap]); // clears leftovers
    run("ip", &["tuntap", "add", "dev", &tap, "mode", "tap"])?;
    run("ip", &["link", "set", &tap, "master", &bridge])?;
    run("ip", &["link", "set", &tap, "up"])?;
    if let (Some(ip), Some(ns)) = (ip, namespace) {
        // ANTI-SPOOFING on the VM's tap, for the same reason `do_attach`/
        // `do_attach_extra` have it on a container's veth — and it matters MORE
        // here: a VM runs a guest kernel we do not control, so nothing inside it
        // stops it from putting an arbitrary source address on the wire. Every
        // policy this engine enforces keys off the source IP (the cross-namespace
        // drop matches `@dlxall`, a `kind: Dependency` allows a specific peer
        // address), so without this rule a guest forges a saddr outside `@dlxall`
        // — or one belonging to a peer of the target namespace — and walks
        // straight through the isolation. The bridge forwards on MAC and does not
        // look at the IP, so this is the only place it can be caught.
        clear_antispoof(&tap);
        run_ok("nft", &antispoof_rule_args(&tap, ip));
        ns_set_join(ip, ns);
    }
    Ok(())
}

/// Removes a VM's `tap` (on `vm rm`/stop). Best-effort.
fn do_vmtapdel(tap: &str) -> Result<()> {
    let tap = sanitize(tap);
    // Drop the anti-spoof rule with the tap, exactly as `do_detach_extra` does
    // for a veth: tap names are reused across VM restarts, and a leftover rule
    // pinned to the previous VM's address would silently blackhole the next one.
    clear_antispoof(&tap);
    run_ok("ip", &["link", "del", &tap]);
    Ok(())
}

/// An FDB destination can only be an IP (v4/v6): hex digits, `.`, `:`. Rejects
/// everything else BEFORE passing it to `bridge`/`ip`. It goes via argv (not shell), but
/// we keep the audit's `valid_*` discipline — a destination with a space/`;`/`|`
/// never reaches a command. (An empty value was already filtered by the caller.)
fn valid_fdb_dst(dst: &str) -> bool {
    !dst.is_empty()
        && dst.len() <= 45 // cap of a textual IPv6
        && dst.chars().all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':')
}

/// **Brings up an overlay network's VXLAN uplink** in the infra netns (port of
/// `crate::Net::ensure_vxlan` to the rootless holder model): ensures the network's
/// `<bridge>`, creates the device `<dev>` (id `<vni>`, port 4789, `nolearning`)
/// mastering it, and seeds the FDB with a "broadcast" entry (`00:…:00`) for each
/// peer destination (`dsts_csv` = `wg_ip` if the overlay is encrypted, otherwise `node_ip`;
/// `-` = still no peers). Idempotent: only creates what's missing, only seeds new FDB.
fn do_vxlan(dev: &str, vni: &str, bridge: &str, gateway: &str, dsts_csv: &str) -> Result<()> {
    let dev = sanitize(dev);
    let bridge = sanitize(bridge);
    let vni: u32 = vni
        .parse()
        .map_err(|_| Error::Invalid(format!("invalid vni: {vni}")))?;
    // The overlay's bridge is a normal holder network bridge — the same function that
    // `attach`/`vmtap` use, so containers and VXLAN share the same L2.
    ensure_net_bridge(&bridge, gateway)?;
    let exists = crate::capture("ip", &["link", "show", &dev])
        .map(|o| o.contains(dev.as_str()))
        .unwrap_or(false);
    if !exists {
        run(
            "ip",
            &[
                "link",
                "add",
                &dev,
                "type",
                "vxlan",
                "id",
                &vni.to_string(),
                "dstport",
                crate::VXLAN_PORT,
                "nolearning",
            ],
        )?;
        run_ok("ip", &["link", "set", &dev, "master", &bridge]);
        run_ok("ip", &["link", "set", &dev, "up"]);
    }
    if dsts_csv != "-" {
        let have = crate::capture("bridge", &["fdb", "show", "dev", &dev]).unwrap_or_default();
        for dst in dsts_csv
            .split(',')
            .map(str::trim)
            .filter(|d| valid_fdb_dst(d))
        {
            // EXACT match by token (not `contains`): otherwise 10.0.0.5 would be "already
            // present" for being a substring of a 10.0.0.50 in the FDB → never seeded.
            let present = have.lines().any(|l| l.split_whitespace().any(|t| t == dst));
            if !present {
                run_ok(
                    "bridge",
                    &[
                        "fdb",
                        "append",
                        "00:00:00:00:00:00",
                        "dev",
                        &dev,
                        "dst",
                        dst,
                    ],
                );
            }
        }
    }
    Ok(())
}

/// Removes a private network's bridge from the infra netns (on `network rm`).
fn do_netdel(bridge: &str) -> Result<()> {
    let bridge = sanitize(bridge);
    if bridge == INFRA_BRIDGE {
        return Err(Error::Invalid(
            "the default ingress bridge cannot be removed".into(),
        ));
    }
    run_ok("ip", &["link", "del", &bridge]);
    Ok(())
}

/// Installs the DNAT of a published port in the `dlxing`'s `pre` chain (runs in the
/// holder): traffic that arrived via the slirp (the tap's `daddr`) on `host_port` is
/// rewritten to `<cip>:<cport>`. Defensive validations against injection in `nft`.
fn do_publish(proto: &str, host_port: &str, cip: &str, cport: &str) -> Result<()> {
    validate_publish(proto, host_port, cip, cport)?;
    run(
        "nft",
        &[
            "add",
            "rule",
            "ip",
            INGRESS_TABLE,
            "pre",
            "ip",
            "daddr",
            SLIRP_IP,
            proto,
            "dport",
            host_port,
            "dnat",
            "to",
            &format!("{cip}:{cport}"),
        ],
    )
}

/// Removes a `host_port`'s DNAT (by handle) from the `pre` chain. Best-effort.
/// `proto` narrows it to one protocol; `None` removes every proto on that port.
/// The `dport <n>` needle alone matched a `tcp dport n` rule and a `udp dport n` rule
/// alike, so unpublishing one protocol tore down the other's DNAT too.
fn do_unpublish(host_port: &str, proto: Option<&str>) -> Result<()> {
    if !is_port(host_port) {
        return Err(Error::Invalid(format!("invalid port: {host_port}")));
    }
    if let Some(p) = proto {
        if p != "tcp" && p != "udp" {
            return Err(Error::Invalid(format!("invalid proto: {p}")));
        }
    }
    // lists the chain with handles and deletes the rule(s) matching the dport.
    let listed = crate::capture("nft", &["-a", "list", "chain", "ip", INGRESS_TABLE, "pre"])
        .unwrap_or_default();
    let needle = match proto {
        Some(p) => format!("{p} dport {host_port} "),
        None => format!("dport {host_port} "),
    };
    for line in listed.lines() {
        if line.contains(&needle) {
            if let Some(handle) = line
                .rsplit("# handle ")
                .next()
                .and_then(|h| h.trim().parse::<u32>().ok())
            {
                run_ok(
                    "nft",
                    &[
                        "delete",
                        "rule",
                        "ip",
                        INGRESS_TABLE,
                        "pre",
                        "handle",
                        &handle.to_string(),
                    ],
                );
            }
        }
    }
    Ok(())
}

/// GLOBAL egress policy of the single ingress (runs INSIDE the infra netns,
/// where the holder has CAP_NET_ADMIN). `deny` adds `forward oifname tap0 drop`
/// (blocks all egress to the Internet); `allow` removes it. The per-workload
/// firewall rules (accept) that appear BEFORE in the `forward` chain still
/// open specific exceptions — so this is the BASE egress policy.
/// Whether an `nft -a list chain` line is the ONE global blanket egress rule
/// (`oifname "tap0" drop`, no `iifname`) that `do_egress` is allowed to
/// delete before (re)applying its new policy.
///
/// BUG FOUND, fixed here: the original check was just `contains("oifname
/// \"tap0\"") && contains("drop")`, with no `iifname` exclusion. Every
/// PER-NETWORK egress rule installed by `apply_egress_from_state`/
/// `egress_specs` has the shape `iifname "<bridge>" oifname "tap0" ...
/// drop` — it ALSO contains both substrings. A later, unrelated global
/// `egress allow`/`deny` silently deleted a network's own `deny`/
/// `allowlist` terminal drop rule, reopening full Internet egress for that
/// network with no error and no indication the per-network policy was
/// wiped — a data-exfiltration path reopened by an unrelated command.
/// Excluding any line that also contains `iifname` is exactly "the global
/// rule has no iifname" — every `apply_egress_from_state` rule always
/// starts with one (see `egress_specs`'s `base()` helper).
fn is_global_egress_drop_line(line: &str) -> bool {
    line.contains("oifname \"tap0\"") && line.contains("drop") && !line.contains("iifname")
}

fn do_egress(policy: &str) -> Result<()> {
    let listed = crate::capture(
        "nft",
        &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"],
    )
    .unwrap_or_default();
    for line in listed.lines() {
        if is_global_egress_drop_line(line) {
            if let Some(handle) = line
                .rsplit("# handle ")
                .next()
                .and_then(|h| h.trim().parse::<u32>().ok())
            {
                run_ok(
                    "nft",
                    &[
                        "delete",
                        "rule",
                        "ip",
                        INGRESS_TABLE,
                        "fwdeny",
                        "handle",
                        &handle.to_string(),
                    ],
                );
            }
        }
    }
    match policy {
        "deny" => run(
            "nft",
            &[
                "add",
                "rule",
                "ip",
                INGRESS_TABLE,
                "fwdeny",
                "oifname",
                "tap0",
                "drop",
            ],
        ),
        "allow" => Ok(()),
        _ => Err(Error::Invalid(format!("invalid egress policy: {policy}"))),
    }
}

/// PER-NETWORK egress (workspace): controls the egress→Internet of ONE bridge, without
/// affecting the others. Idempotent (removes that bridge's old rules first).
/// Supports `deny`/`allow`/`allowlist:<cidrs>` (NET-A).
fn do_egress_net(bridge: &str, policy: &str) -> Result<()> {
    if !(policy == "allow" || policy == "deny" || policy.starts_with("allowlist:")) {
        return Err(Error::Invalid(format!("invalid egress policy: {policy}")));
    }
    let norm = (policy != "allow").then(|| policy.to_string());
    let bridge = sanitize(bridge);
    // Persists the new policy and re-applies the COMPLETE chain (policy + existing
    // FQDN hosts) — so `egress net` and `egress host` compose.
    let state = update_netdef_egress(&bridge, |e| e.policy = norm.clone()).unwrap_or(EgressState {
        policy: norm,
        hosts: Vec::new(),
    });
    apply_egress_from_state(&bridge, &state)
}

// ---- egress by HOSTNAME (FQDN allowlist via DNS-snooping) -------------------
//
// nft only knows about IPs; to allow "egress only to *.github.com" the holder sees the
// DNS responses it already forwards (the ingress's resolver) and injects the A-records
// of the allowed hostnames into a per-bridge nft `set` that the egress accepts. It's the
// Cilium FQDN-policy, but 100% rootless (nft + DNS in the holder, no eBPF).

/// FQDN allowlist shared between the control thread (registers in `egress-host`)
/// and the DNS thread (populates the set with the A-records). Tuples `(bridge, set, suffix)`.
/// The suffix `github.com` matches `github.com` AND `*.github.com`.
static FQDN_ALLOW: std::sync::Mutex<Vec<(String, String, String)>> =
    std::sync::Mutex::new(Vec::new());

/// Name (short, <= nft's limit) of a bridge's FQDN set.
fn fqdn_set(bridge: &str) -> String {
    format!("dlxfq{:08x}", crate::fnv32(bridge))
}

/// Registers an allowed hostname for a bridge's egress: creates the nft set (with
/// `flags timeout` so entries expire with the TTL), reprograms the bridge's egress
/// to `DNS + @set + drop`, and memorizes the suffix for the DNS to populate.
fn do_egress_host(bridge: &str, suffix: &str) -> Result<()> {
    let bridge = sanitize(bridge);
    let suffix = suffix
        .trim()
        .trim_start_matches("*.")
        .trim_matches('.')
        .to_lowercase();
    // Anti-injection: a hostname is [a-z0-9.-], with at least one dot, <= 253.
    if suffix.is_empty()
        || suffix.len() > 253
        || !suffix.contains('.')
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(Error::Invalid(format!("invalid hostname: {suffix:?}")));
    }
    // Persists the hostname and re-applies the COMPLETE chain (composes with the CIDR
    // policy if any). `apply_egress_from_state` creates the set and registers in FQDN_ALLOW.
    let state = update_netdef_egress(&bridge, |e| {
        if !e.hosts.contains(&suffix) {
            e.hosts.push(suffix.clone());
        }
    })
    .unwrap_or(EgressState {
        policy: None,
        hosts: vec![suffix],
    });
    apply_egress_from_state(&bridge, &state)
}

/// Extracts the IPv4s from the A-records of a DNS response (bounds-checked; tolerates
/// name compression by skipping via RDLENGTH). PURE — testable without a network.
fn parse_a_records(resp: &[u8]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    if resp.len() < 12 {
        return out;
    }
    let qd = u16::from_be_bytes([resp[4], resp[5]]) as usize;
    let an = u16::from_be_bytes([resp[6], resp[7]]) as usize;
    let mut i = 12usize;
    // skip the QDCOUNT questions (name + QTYPE + QCLASS)
    for _ in 0..qd {
        i = skip_name(resp, i);
        i += 4;
        if i > resp.len() {
            return out;
        }
    }
    // read ANCOUNT answers
    for _ in 0..an {
        i = skip_name(resp, i);
        if i + 10 > resp.len() {
            break;
        }
        let rtype = u16::from_be_bytes([resp[i], resp[i + 1]]);
        let rdlen = u16::from_be_bytes([resp[i + 8], resp[i + 9]]) as usize;
        i += 10;
        if i + rdlen > resp.len() {
            break;
        }
        if rtype == 1 && rdlen == 4 {
            out.push([resp[i], resp[i + 1], resp[i + 2], resp[i + 3]]);
        }
        i += rdlen;
    }
    out
}

/// Advances the offset past a DNS name (labels or 0xC0 compression pointer).
fn skip_name(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        let len = b[i] as usize;
        if len == 0 {
            return i + 1;
        }
        if len & 0xc0 == 0xc0 {
            return i + 2; // compression pointer: 2 bytes, end of the name
        }
        i += 1 + len;
    }
    i
}

/// If `name` matches an allowed suffix, injects `resp`'s A-records into the corresponding
/// nft set(s), with timeout (renews on each resolution). Best-effort.
fn snoop_fqdn(name: &str, resp: &[u8]) {
    let n = name.trim_end_matches('.').to_lowercase();
    let sets: Vec<String> = match FQDN_ALLOW.lock() {
        Ok(g) => g
            .iter()
            .filter(|(_, _, suf)| n == *suf || n.ends_with(&format!(".{suf}")))
            .map(|(_, set, _)| set.clone())
            .collect(),
        Err(_) => return,
    };
    if sets.is_empty() {
        return;
    }
    for ip in parse_a_records(resp) {
        let ips = format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        for set in &sets {
            run_ok(
                "nft",
                &[
                    "add",
                    "element",
                    "ip",
                    INGRESS_TABLE,
                    set,
                    &format!("{{ {ips} timeout 1h }}"),
                ],
            );
        }
    }
}

/// Pre-flight of an `nft` ruleset (`nft -c -f -`): returns `true` if it's ACCEPTED,
/// WITHOUT applying it. It's the "golden rule" of the L4 protection — we only apply after the
/// kernel confirms it supports the syntax (e.g.: `meter`/`ct count`).
fn nft_check(script: &str) -> bool {
    use std::io::Write;
    let mut child = match Command::new("nft")
        .args(["-c", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(script.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// L4 DDoS protection (req #5): PER-SOURCE rate-limit + ct-count of NEW inbound
/// connections (via tap0), in the dlxing's `forward`. Not global (each source has
/// its own bucket → it's not self-DoS). `counter drop` makes the excesses OBSERVABLE
/// (detection). best-effort + `nft -c` pre-flight: if the kernel doesn't support `meter`,
/// it DEGRADES (doesn't apply, doesn't break the ruleset). Idempotent (clears first).
fn do_l4guard(conn_rate: u32, conn_max: u32) -> Result<()> {
    clear_l4guard();
    let rate = conn_rate.clamp(1, 100_000);
    let burst = rate.saturating_mul(2).max(1);
    let max = conn_max.clamp(1, 1_000_000);
    let script = format!(
        "add rule ip {t} forward iifname \"tap0\" ct state new meter dlx_conn_rate \
            {{ ip saddr limit rate over {rate}/second burst {burst} packets }} counter drop\n\
         add rule ip {t} forward iifname \"tap0\" ct state new meter dlx_conn_count \
            {{ ip saddr ct count over {max} }} counter drop\n",
        t = INGRESS_TABLE,
    );
    // GOLDEN RULE: only applies if the kernel accepts the syntax (otherwise degrades).
    if !nft_check(&script) {
        return Ok(());
    }
    let _ = apply_nft_stdin(&script);
    Ok(())
}

/// Removes the L4 guard rules from the `forward` (and, with them, the dynamic meters —
/// a meter with no rules referencing it is freed). Idempotent.
fn clear_l4guard() {
    let listed = crate::capture(
        "nft",
        &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"],
    )
    .unwrap_or_default();
    for line in listed.lines() {
        if line.contains("dlx_conn_rate") || line.contains("dlx_conn_count") {
            if let Some(h) = line
                .rsplit("# handle ")
                .next()
                .and_then(|x| x.trim().parse::<u32>().ok())
            {
                run_ok(
                    "nft",
                    &[
                        "delete",
                        "rule",
                        "ip",
                        INGRESS_TABLE,
                        "fwdeny",
                        "handle",
                        &h.to_string(),
                    ],
                );
            }
        }
    }
}

/// Validates a publish's fields before putting them into an `nft` command (defense
/// against injection): `tcp`/`udp` protocol, numeric ports, IP in the infra subnet.
fn validate_publish(proto: &str, host_port: &str, cip: &str, cport: &str) -> Result<()> {
    if proto != "tcp" && proto != "udp" {
        return Err(Error::Invalid(format!("invalid protocol: {proto}")));
    }
    if !is_port(host_port) || !is_port(cport) {
        return Err(Error::Invalid("invalid port (1..65535)".into()));
    }
    if !is_ingress_ip(cip) {
        return Err(Error::Invalid(format!(
            "IP {cip} outside the ingress space (10.200-254.x)"
        )));
    }
    Ok(())
}

fn is_port(p: &str) -> bool {
    p.parse::<u16>().map(|n| n >= 1).unwrap_or(false)
}

/// `true` if `ip` is a valid address of the ingress SPACE (`10.{200..=254}.x.x`,
/// unicast): the default network (10.200) or a private network (10.201+). Anti-injection
/// defense without fixing a single `/16`.
/// Workload space (`10.200.0.0`–`10.254.255.255`, see
/// `delonix_runtime_core::workload_net` — shared with `delonix-tunnel`, which uses the
/// SAME range for the tunnel's "no-bypass" guard), except each /16's
/// network/broadcast addresses (`.0.0` and `.255.255`), which here are not usable
/// workload IPs.
fn is_ingress_ip(ip: &str) -> bool {
    let o: Vec<&str> = ip.split('.').collect();
    if o.len() != 4 {
        return false;
    }
    let n: Vec<u8> = match o
        .iter()
        .map(|x| x.parse::<u8>())
        .collect::<std::result::Result<_, _>>()
    {
        Ok(v) => v,
        Err(_) => return false,
    };
    let addr = std::net::Ipv4Addr::new(n[0], n[1], n[2], n[3]);
    delonix_runtime_core::workload_net::is_workload_ipv4(addr)
        && (n[2], n[3]) != (0, 0)
        && (n[2], n[3]) != (255, 255)
}

/// Name of the bridge-side `veth` for a netns (deterministic, <= 15 chars).
fn vh_name(netns: &str) -> String {
    format!("vh{:08x}", crate::fnv32(netns))
}

/// Name of the host-side `veth` of an ADDITIONAL network (multi-homing): distinct per
/// (netns, interface) so as not to collide with the primary nor between extra networks.
fn vh_name_extra(netns: &str, ifname: &str) -> String {
    format!("vx{:08x}", crate::fnv32(&format!("{netns}/{ifname}")))
}

// ---- PARAMETERIZABLE ingress firewall (the ONLY place — user's principle) ----

/// Name of the per-container firewall chain in `dlxing` (derived from the IP).
fn fw_chain_name(ip: &str) -> String {
    format!("fw{:08x}", crate::fnv32(ip))
}

/// Generates the BODY of a container's firewall chain (L4 rules + default policy),
/// in the infra netns. PURE — same semantics as the root model (`apply_container_firewall`),
/// but applied at the ingress. `in` = traffic TO the container (daddr==ip); `out` = FROM it
/// (saddr==ip); `src` matches the other end (peer). Testable without a kernel.
/// nft map that dispatches a packet straight to its container's chain, keyed by
/// address. Replaces the two `ip {daddr,saddr} <ip> jump fw…` rules PER CONTAINER
/// that used to pile up in a base chain: with 50 containers every packet walked
/// ~100 rules before reaching its own. A verdict map is one hashed lookup,
/// independent of how many containers exist.
pub const FWMAP: &str = "fwmap";

/// Head of a container's firewall chain, emitted ONCE per chain (not per IP,
/// unlike [`fw_chain_body`] — conntrack state is a property of the flow, not of
/// which address it entered by).
///
/// This is the standard stateful-firewall shape, and it is what makes a default
/// policy USABLE. Without it, `policy_in: deny` emitted a bare `ip daddr <ip>
/// drop` in a chain hooked at forward priority -10 — BEFORE the `forward`
/// chain's own `ct state established,related accept` at priority 0. So the
/// container's own outbound traffic died on the REPLY: `ingress policy deny`
/// killed DNS and every outbound connection, and the symmetric `egress policy
/// deny` dropped the SYN-ACK of an inbound connection, making a published
/// service unreachable. Both reproduced live before this fix. Between them they
/// made "default-deny, then allow exactly what is needed" — the whole point of
/// the subsystem — impossible to express.
///
/// Consequence worth knowing: an explicit `deny` no longer tears down a flow
/// that is ALREADY established, it only stops new ones. That is what iptables/
/// nftables/Kubernetes NetworkPolicy all do, and it is why `conntrack -D` exists
/// (the CLI already ships conntrack for exactly this kind of cleanup).
pub fn fw_chain_prologue(fw: &delonix_runtime_core::ContainerFw) -> String {
    if !fw.enabled {
        return String::new();
    }
    "\t\tct state invalid counter drop\n\
     \t\tct state established,related counter accept\n"
        .to_string()
}

/// Everything in a rule's nft line AFTER the `ip {daddr,saddr} <own-ip>` anchor —
/// the peer match, the L4 match, the counter and the verdict. `None` for a rule whose
/// fields are not safe to interpolate.
///
/// Shared on purpose between the GENERATOR ([`fw_chain_body`]) and the counter READER
/// (`ingress ls`): the tail is identical on every address a multi-homed container
/// holds, which is exactly what makes it usable as the key to sum a rule's counters
/// back across networks. If the two had separate copies of this formatting, the
/// reader would silently stop matching the day the generator changed a space.
pub fn fw_rule_tail(r: &delonix_runtime_core::FwRule) -> Option<String> {
    // Defense against nft injection: refuses rules with unsafe fields
    // (src/proto/port are interpolated into the ruleset fed to `nft -f`).
    if !r.nft_safe() {
        return None;
    }
    let peer_dir = if r.dir == "out" { "daddr" } else { "saddr" };
    let mut tail = String::new();
    if !r.src.is_empty() && r.src != "0.0.0.0/0" && r.src != "*" {
        // `/32` is dropped: the kernel renders a single-host prefix as a bare address,
        // so emitting it would make the generated text and the LISTED text differ — and
        // the listed text is what the counter lookup matches on. Caught live: an
        // `--from 172.16.31.103/32` rule with real traffic on it (`packets 1 bytes 44`
        // in the chain) showed `-` in `ingress ls`. Same rule either way for nft.
        let src = r.src.strip_suffix("/32").unwrap_or(&r.src);
        tail.push_str(&format!("ip {peer_dir} {src} "));
    }
    // The PORT has to survive `proto: any` (`allow <c> 8080`, the shape the CLI
    // produces when the user writes a bare port). Emitting the port only inside the
    // `proto != any` branch silently WIDENED the rule to the whole container: an
    // `allow <c> 9999` under `policy deny` opened every port, and a `deny <c> 9999`
    // dropped every port — the opposite of what the command says, in the one place
    // where being wrong is a security hole. nft can't put a `dport` on a rule with
    // no L4 proto selected, so `any` + port becomes `meta l4proto { tcp, udp } th
    // dport <port>` (`th` = transport header, valid for both, ranges included).
    let has_port = !r.port.is_empty() && r.port != "*";
    if !r.proto.is_empty() && r.proto != "any" {
        tail.push_str(&r.proto);
        if has_port {
            tail.push_str(&format!(" dport {}", r.port));
        }
        tail.push(' ');
    } else if has_port {
        tail.push_str(&format!("meta l4proto {{ tcp, udp }} th dport {} ", r.port));
    }
    // `counter` on EVERY rule: without it there is no way to answer "did this rule
    // ever match?" — the question a firewall exists to answer. Read back by
    // `ingress/egress ls` (PACKETS/BYTES columns). Cost is one counter per rule, the
    // same thing iptables has always done unconditionally.
    tail.push_str(if r.action == "allow" {
        "counter accept"
    } else {
        "counter drop"
    });
    Some(tail)
}

/// Rule lines of an `nft list chain` listing, as `(text with the counter VALUES
/// removed, packets, bytes)`. The kernel renders a counter as `counter packets N
/// bytes M`, so stripping the two numbers turns the listed line back into the exact
/// text [`fw_rule_tail`] produces — which is what lets a rule find its own counters
/// without depending on rule ORDER (fragile: the body repeats per address, with the
/// namespace and policy lines interleaved between repetitions).
pub fn parse_fw_counters(listing: &str) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for line in listing.lines() {
        let line = line.trim();
        let Some((before, after)) = line.split_once("counter packets ") else {
            continue;
        };
        let mut it = after.split_whitespace();
        let Some(packets) = it.next().and_then(|p| p.parse::<u64>().ok()) else {
            continue;
        };
        if it.next() != Some("bytes") {
            continue;
        }
        let Some(bytes) = it.next().and_then(|b| b.parse::<u64>().ok()) else {
            continue;
        };
        let rest: Vec<&str> = it.collect();
        out.push((
            format!("{before}counter {}", rest.join(" "))
                .trim()
                .to_string(),
            packets,
            bytes,
        ));
    }
    out
}

pub fn fw_chain_body(ip: &str, fw: &delonix_runtime_core::ContainerFw) -> String {
    let mut body = String::new();
    if !fw.enabled {
        return body; // empty chain = open (behavior prior to fw/namespace)
    }
    for r in &fw.rules {
        // Defense against nft injection: skips rules with unsafe fields
        // (src/proto/port are interpolated into the ruleset fed to `nft -f`).
        if !r.nft_safe() {
            continue;
        }
        let self_dir = if r.dir == "out" { "saddr" } else { "daddr" };
        let Some(tail) = fw_rule_tail(r) else {
            continue;
        };
        body.push_str(&format!("\t\tip {self_dir} {ip} {tail}\n"));
    }
    // NAMESPACE isolation on INGRESS — only when there is NO explicit inbound
    // policy (a Dependency/Ingress is authoritative and replaces this): accepts the
    // same namespace and drops NEW connections from containers of ANOTHER namespace. The
    // `ct state new` exempts the return (established/related), and the `@dlxall` limits the
    // drop to sources that ARE SDN containers (lets gateway/DNS/internet through).
    // The EXPLICIT rules above take precedence (first-match terminal in the chain).
    let has_explicit_in = fw.policy_in == "deny" || fw.rules.iter().any(|r| r.dir == "in");
    if !has_explicit_in {
        let nsset = dlxns_set(&fw.namespace);
        body.push_str(&format!(
            "\t\tip daddr {ip} ip saddr @{nsset} counter accept\n"
        ));
        body.push_str(&format!(
            "\t\tip daddr {ip} ip saddr @{DLXALL_SET} ct state new counter drop\n"
        ));
    }
    // The default policy is reached only by NEW flows — the prologue already let
    // established/related through. See `fw_chain_prologue` for why that matters.
    if fw.policy_in == "deny" {
        body.push_str(&format!("\t\tip daddr {ip} counter drop\n"));
    }
    if fw.policy_out == "deny" {
        body.push_str(&format!("\t\tip saddr {ip} counter drop\n"));
    }
    body
}

/// nft set with ALL the SDN container IPs (so namespace isolation
/// only affects container↔container traffic, not gateway/DNS/internet).
pub const DLXALL_SET: &str = "dlxall";

/// Name (short, ≤ nft's limit) of the IP set of a logical namespace.
pub fn dlxns_set(ns: &str) -> String {
    format!("dlxns{:08x}", crate::fnv32(ns))
}

/// Parses `nft list map ip dlxing fwmap` into `(address, chain)` pairs. Text and
/// not `-j`: the JSON shape of a verdict map is markedly more code to walk, for a
/// listing whose text form is two tokens around a `:`. Pure and tested.
///
/// The `elements = { … }` block wraps over several lines and the last entry has no
/// trailing comma, so splitting on `,` alone loses entries — parse per `jump`.
pub fn parse_fwmap_elements(listing: &str) -> Vec<(String, String)> {
    let Some(rest) = listing.split_once("elements = {").map(|(_, r)| r) else {
        return Vec::new();
    };
    let body = rest.split_once('}').map(|(b, _)| b).unwrap_or(rest);
    body.split(',')
        .filter_map(|entry| {
            let (addr, verdict) = entry.split_once(':')?;
            let chain = verdict.trim().strip_prefix("jump ")?.trim();
            let addr = addr.trim();
            (!addr.is_empty() && !chain.is_empty()).then(|| (addr.to_string(), chain.to_string()))
        })
        .collect()
}

/// Applies a container's firewall in `dlxing` (runs in the holder): ensures the chain
/// `fw<hash>`, points every one of the container's addresses at it through the `fwmap`
/// verdict map, and rebuilds the body. `hex` is the `ContainerFw` JSON in hexadecimal
/// (the control channel is line-based).
///
/// **ONE nft transaction.** This used to be a sequence of separate `nft` invocations —
/// `add chain`, a `list`, one `delete rule` per stale jump, one `add rule` per jump, and
/// only then the flush+body as its own script. Each is a separate kernel transaction, so
/// between deleting the old jumps and adding the new ones the container was, briefly,
/// governed by NOTHING. Any `ingress deny`/`--net-connect` opened that window. A single
/// `nft -f` script is applied atomically by the kernel: the ruleset goes from fully-old
/// to fully-new with no observable state in between, and a syntax error anywhere leaves
/// the previous ruleset untouched instead of half-applied.
///
/// `ips` is EVERY IP the container holds — the primary first, then one per additional
/// network (multi-homing). It used to be a single IP, which meant a container connected
/// to a second network was reachable on that second IP with NO firewall at all: an
/// `ingress policy deny` blocked the primary and the container answered fine on the
/// extra (reproduced live). Every emitted rule is anchored to a specific IP
/// (`ip daddr <ip>` / `ip saddr <ip>`), so concatenating one body per IP is
/// correct under the chain's first-match-terminal semantics — a packet for IP-B
/// simply matches none of IP-A's lines.
fn do_firewall(ips: &str, hex: &str) -> Result<()> {
    let ips: Vec<&str> = ips.split(',').filter(|s| !s.is_empty()).collect();
    if ips.is_empty() {
        return Err(Error::Invalid("firewall: no IP given".into()));
    }
    for ip in &ips {
        if !is_ingress_ip(ip) {
            return Err(Error::Invalid(format!(
                "IP {ip} outside the ingress space (10.200-254.x)"
            )));
        }
    }
    let bytes = hex_decode(hex).ok_or_else(|| Error::Invalid("invalid hex".into()))?;
    let fw: delonix_runtime_core::ContainerFw = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Invalid(format!("firewall JSON: {e}")))?;
    // The chain is named after the PRIMARY IP so it stays stable as extra networks
    // come and go (`do_unfirewall` finds it by the same name).
    let chain = fw_chain_name(ips[0]);
    // Which map entries have to go before the new ones land. Two groups, and BOTH
    // are needed: (a) every address still pointing at THIS chain — a container that
    // left an additional network would otherwise keep an entry for the released
    // address, and IPAM hands that address to someone else later, silently giving the
    // next tenant this container's firewall; (b) any address we are about to claim
    // that is currently mapped elsewhere — `add element` on an existing key is an
    // error, which would abort the whole transaction and leave the container
    // unprotected. Reading is outside the transaction, which is harmless: a stale
    // read can only leave an entry that the next apply removes.
    let listing =
        crate::capture("nft", &["list", "map", "ip", INGRESS_TABLE, FWMAP]).unwrap_or_default();
    let mut stale: Vec<String> = parse_fwmap_elements(&listing)
        .into_iter()
        .filter(|(addr, c)| c == &chain || ips.contains(&addr.as_str()))
        .map(|(addr, _)| addr)
        .collect();
    stale.sort();
    stale.dedup();

    // One body per IP: the rules, the namespace isolation and the default policy are
    // all anchored to a concrete address, so the container is governed identically on
    // every network it is attached to. The prologue (conntrack fast-path) is emitted
    // once for the whole chain — state belongs to the flow, not to an address.
    let body: String = std::iter::once(fw_chain_prologue(&fw))
        .chain(ips.iter().map(|ip| fw_chain_body(ip, &fw)))
        .collect();
    let mut script = String::new();
    // Idempotent re-declarations: they let a table created by an older holder grow
    // the map/chain instead of failing, and cost nothing when they already exist.
    script.push_str(&format!(
        "add map ip {INGRESS_TABLE} {FWMAP} {{ type ipv4_addr : verdict; }}\n\
         add chain ip {INGRESS_TABLE} {chain}\n\
         flush chain ip {INGRESS_TABLE} {chain}\n"
    ));
    for addr in &stale {
        script.push_str(&format!(
            "delete element ip {INGRESS_TABLE} {FWMAP} {{ {addr} }}\n"
        ));
    }
    for ip in &ips {
        script.push_str(&format!(
            "add element ip {INGRESS_TABLE} {FWMAP} {{ {ip} : jump {chain} }}\n"
        ));
    }
    script.push_str(&format!(
        "table ip {INGRESS_TABLE} {{\n\tchain {chain} {{\n{body}\t}}\n}}\n"
    ));
    apply_nft_stdin(&script)
}

/// Removes a container's firewall from `dlxing`: drops every `fwmap` entry pointing at
/// its chain, then the chain itself — one transaction, same reasoning as
/// [`do_firewall`]. Best-effort: a teardown that half-fails must not block the rest of
/// the container's cleanup.
fn do_unfirewall(ip: &str) -> Result<()> {
    let chain = fw_chain_name(ip);
    let listing =
        crate::capture("nft", &["list", "map", "ip", INGRESS_TABLE, FWMAP]).unwrap_or_default();
    let mut script = String::new();
    for (addr, c) in parse_fwmap_elements(&listing) {
        if c == chain {
            script.push_str(&format!(
                "delete element ip {INGRESS_TABLE} {FWMAP} {{ {addr} }}\n"
            ));
        }
    }
    script.push_str(&format!("delete chain ip {INGRESS_TABLE} {chain}\n"));
    let _ = apply_nft_stdin(&script);
    Ok(())
}

/// Hex-encode (lowercase) — to pass the firewall JSON through the line channel.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Hex-decode; `None` if the length is odd or there are invalid digits.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Sanitizes a netns/interface name (only `[a-z0-9_-]`, <= 12 chars) — defense
/// against injection in `ip netns` and the IFNAMSIZ.
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    cleaned.chars().take(12).collect()
}

// ---- ingress private networks (F6): bridge per network, gateway = ingress ----

/// Definition of an ingress private network: name, bridge (in the infra netns) and
/// `/16` prefix. The **gateway is ALWAYS the ingress** (`<prefix>.0.1` on the bridge), through
/// which the network egresses/receives (egress via the single slirp) and where the firewall lives.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NetDef {
    pub name: String,
    pub bridge: String,
    pub prefix: String, // e.g.: "10.201"
    /// The network's egress intent, PERSISTED to survive the holder's
    /// respawn (the nft and the FQDN registry live in an ephemeral netns). Re-applied in
    /// `ensure_net_bridge` when the bridge is recreated.
    #[serde(default)]
    pub egress: EgressState,
}

/// A network's egress policy, stored in the [`NetDef`].
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct EgressState {
    /// `deny` | `allow` | `allowlist:<cidrs>`. `None` = default (allow).
    #[serde(default)]
    pub policy: Option<String>,
    /// Allowed FQDN suffixes (`egress host`).
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// Updates (and persists) the egress intent of the network whose bridge is `bridge`,
/// returning the resulting state. `None` if no `NetDef` matches (e.g.:
/// the default bridge `delonix0`, which is not persisted).
fn update_netdef_egress(
    bridge: &str,
    mutate: impl FnOnce(&mut EgressState),
) -> Option<EgressState> {
    for mut def in network_list() {
        if def.bridge == bridge {
            mutate(&mut def.egress);
            if let Ok(json) = serde_json::to_vec_pretty(&def) {
                let _ = std::fs::write(netdef_path(&def.name), json);
            }
            return Some(def.egress);
        }
    }
    None
}

/// Builds a bridge's COMPLETE egress chain from the combined state
/// (CIDR policy + FQDN hosts), so that `egress net allowlist` and `egress host`
/// COMPOSE instead of one reprogramming over the other. Removes the bridge's old
/// rules and reinserts in the right order: DNS → CIDRs → @set FQDN → drop. `allow`
/// with no hosts = default-allow (nothing). `deny` with no hosts = total drop. Any host
/// forces allowlist mode (the hosts are explicit allows).
fn apply_egress_from_state(bridge: &str, state: &EgressState) -> Result<()> {
    let bridge = sanitize(bridge);
    // Removes all this bridge's old egress rules (drop + accepts).
    let needle_if = format!("iifname \"{bridge}\"");
    let listed = crate::capture(
        "nft",
        &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"],
    )
    .unwrap_or_default();
    for line in listed.lines() {
        if line.contains(&needle_if)
            && line.contains("oifname \"tap0\"")
            && (line.contains("drop") || line.contains("accept"))
        {
            if let Some(h) = line
                .rsplit("# handle ")
                .next()
                .and_then(|x| x.trim().parse::<u32>().ok())
            {
                run_ok(
                    "nft",
                    &[
                        "delete",
                        "rule",
                        "ip",
                        INGRESS_TABLE,
                        "fwdeny",
                        "handle",
                        &h.to_string(),
                    ],
                );
            }
        }
    }
    // Creates the FQDN set + registers the suffixes BEFORE inserting the `@set` rule.
    if !state.hosts.is_empty() {
        let set = fqdn_set(&bridge);
        run_ok(
            "nft",
            &[
                "add",
                "set",
                "ip",
                INGRESS_TABLE,
                &set,
                "{ type ipv4_addr; flags timeout; }",
            ],
        );
        fqdn_register(&bridge, &set, &state.hosts);
    }
    // `insert` prepends → insert in REVERSE order so the top→bottom comes out right.
    for spec in egress_specs(&bridge, state).iter().rev() {
        run("nft", &spec.iter().map(|s| s.as_str()).collect::<Vec<_>>())?;
    }
    Ok(())
}

/// Builds the `nft insert rule …` arg-vectors for a bridge's egress from the
/// combined state (CIDR policy + FQDN hosts), in top→bottom order. **PURE**
/// (no I/O — testable): DNS → allowlist CIDRs → `@set` FQDN → drop. `allow`
/// with no hosts → empty (default-allow); `deny` with no hosts → only drop. `bridge` comes
/// already sanitized.
fn egress_specs(bridge: &str, state: &EgressState) -> Vec<Vec<String>> {
    let policy = state.policy.as_deref().unwrap_or("allow");
    let has_hosts = !state.hosts.is_empty();
    let base = |extra: &[&str]| -> Vec<String> {
        let mut v = vec![
            "insert".into(),
            "rule".into(),
            "ip".into(),
            INGRESS_TABLE.into(),
            "fwdeny".into(),
            "iifname".into(),
            bridge.to_string(),
            "oifname".into(),
            "tap0".into(),
        ];
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    if policy == "allow" && !has_hosts {
        return Vec::new();
    }
    if policy == "deny" && !has_hosts {
        return vec![base(&["drop"])];
    }
    let mut specs = vec![
        base(&["udp", "dport", "53", "accept"]),
        base(&["tcp", "dport", "53", "accept"]),
    ];
    if let Some(cidrs) = policy.strip_prefix("allowlist:") {
        for cidr in cidrs.split(',').map(|c| c.trim()).filter(|c| !c.is_empty()) {
            if delonix_runtime_core::fw_src_ok(cidr) {
                specs.push(base(&["ip", "daddr", cidr, "accept"]));
            } else {
                tracing::warn!(cidr = ?cidr, "egress allowlist — invalid CIDR skipped");
            }
        }
    }
    if has_hosts {
        specs.push(base(&[
            "ip",
            "daddr",
            &format!("@{}", fqdn_set(bridge)),
            "accept",
        ]));
    }
    specs.push(base(&["drop"])); // default-deny of the rest (stays LAST)
    specs
}

/// IPs currently in a bridge's FQDN set (learned from the DNS responses).
/// Runs INSIDE the holder (the set lives in the infra netns). Extracts the IPv4s from the dump.
fn egress_set_members(bridge: &str) -> Vec<String> {
    let set = fqdn_set(&sanitize(bridge));
    let dump =
        crate::capture("nft", &["list", "set", "ip", INGRESS_TABLE, &set]).unwrap_or_default();
    let mut ips = Vec::new();
    for tok in dump.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        if tok.split('.').filter(|o| !o.is_empty()).count() == 4
            && tok.parse::<std::net::Ipv4Addr>().is_ok()
        {
            ips.push(tok.to_string());
        }
    }
    ips.sort();
    ips.dedup();
    ips
}

/// Live counters of a container's firewall rules, keyed by the rule tail
/// ([`fw_rule_tail`]) and SUMMED across every address the container holds — a rule
/// on a multi-homed container has one counter per network, and the user asked about
/// the rule, not about the network.
///
/// Empty map when the holder is down or the chain does not exist yet: a firewall
/// listing must still print its rules when the dataplane is not up, showing no
/// traffic rather than refusing to answer.
pub fn fw_counters(ip: &str) -> std::collections::HashMap<String, (u64, u64)> {
    let mut out = std::collections::HashMap::new();
    let Ok(body) = control_query(&format!("fwstats {ip}")) else {
        return out;
    };
    let Some(listing) = hex_decode(body.trim()).and_then(|b| String::from_utf8(b).ok()) else {
        return out;
    };
    for (text, packets, bytes) in parse_fw_counters(&listing) {
        let entry = out.entry(strip_fw_anchor(&text)).or_insert((0, 0));
        entry.0 += packets;
        entry.1 += bytes;
    }
    out
}

/// Drops the leading `ip {daddr,saddr} <address>` anchor from a listed rule, leaving
/// the tail — the address is what varies between a container's networks, the tail is
/// what identifies the rule. Lines without an anchor (the conntrack prologue) come
/// back unchanged; they simply never match a rule tail.
fn strip_fw_anchor(text: &str) -> String {
    let mut it = text.split_whitespace();
    match (it.next(), it.next()) {
        (Some("ip"), Some("daddr" | "saddr")) => {
            it.next(); // the address itself
            it.collect::<Vec<_>>().join(" ")
        }
        _ => text.to_string(),
    }
}

/// FQDN IPs learned live for a bridge — asks the holder (`egress show`
/// on the CLI side). Empty if the holder is down.
pub fn egress_members(bridge: &str) -> Vec<String> {
    // `control_query` already returns the body (without the `ok ` prefix).
    match control_query(&format!("egress-show {bridge}")) {
        Ok(body) => body
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Registers (without duplicating) a bridge's FQDN suffixes in [`FQDN_ALLOW`] for the
/// DNS thread to snoop them. Called on apply and on the post-respawn re-application.
fn fqdn_register(bridge: &str, set: &str, hosts: &[String]) {
    if let Ok(mut g) = FQDN_ALLOW.lock() {
        for h in hosts {
            if !g.iter().any(|(b, _, s)| b == bridge && s == h) {
                g.push((bridge.to_string(), set.to_string(), h.clone()));
            }
        }
    }
}

fn networks_dir() -> PathBuf {
    ingress_dir().join("networks")
}
fn netdef_path(name: &str) -> PathBuf {
    networks_dir().join(format!("{}.json", sanitize(name)))
}

/// Gateway (= ingress) of a `/16` prefix.
fn gateway_of(prefix: &str) -> String {
    format!("{prefix}.0.1")
}

/// Resolves a network to `(bridge, prefix, gateway)`. `ingress`/empty = the
/// default network (delonix0/10.200); otherwise loads the private network's `NetDef`.
pub fn resolve_net(name: &str) -> Result<(String, String, String)> {
    if name.is_empty() || name == "ingress" {
        return Ok((
            INFRA_BRIDGE.to_string(),
            INFRA_PREFIX.to_string(),
            INFRA_GATEWAY.to_string(),
        ));
    }
    let def = network_get(name).ok_or_else(|| {
        Error::NotFound(format!(
            "ingress network '{name}' does not exist — create it with `delonix network create {name}`, or use the default network"
        ))
    })?;
    let gw = gateway_of(&def.prefix);
    Ok((def.bridge, def.prefix, gw))
}

/// Reads a private network's `NetDef` (if it exists).
pub fn network_get(name: &str) -> Option<NetDef> {
    serde_json::from_slice(&std::fs::read(netdef_path(name)).ok()?).ok()
}

/// Lists the defined ingress private networks.
pub fn network_list() -> Vec<NetDef> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(networks_dir()) {
        for e in rd.flatten() {
            if let Ok(def) =
                serde_json::from_slice::<NetDef>(&std::fs::read(e.path()).unwrap_or_default())
            {
                v.push(def);
            }
        }
    }
    v
}

/// **Creates an ingress private network**: allocates a free `/16` prefix (10.201+,
/// avoiding 10.200 and the ones already used) and a bridge, and persists the `NetDef`. The bridge is
/// created (lazily) in the infra netns on the 1st `attach`. Idempotent by name.
pub fn network_create(name: &str) -> Result<NetDef> {
    if let Some(def) = network_get(name) {
        return Ok(def);
    }
    let used: std::collections::HashSet<String> =
        network_list().into_iter().map(|d| d.prefix).collect();
    let prefix = (201..=254)
        .map(|o| format!("10.{o}"))
        .find(|p| !used.contains(p))
        .ok_or_else(|| Error::Invalid("no free /16 prefixes for ingress networks".into()))?;
    let def = NetDef {
        name: name.to_string(),
        bridge: format!("dlxn{:08x}", crate::fnv32(name)),
        prefix,
        egress: EgressState::default(),
    };
    std::fs::create_dir_all(networks_dir()).map_err(|e| Error::Runtime {
        context: "networks dir",
        message: e.to_string(),
    })?;
    std::fs::write(
        netdef_path(name),
        serde_json::to_vec_pretty(&def).unwrap_or_default(),
    )?;
    Ok(def)
}

/// Like [`network_create`], but with an **explicit `/16` prefix** (e.g.: `"10.50"`).
/// Used to ALIGN the VMs' network plan to the prefix decided by the
/// `NetworkStore` (the source of truth), so the same network has the SAME subnet
/// in containers and VMs. Idempotent by name.
pub fn network_create_with(name: &str, prefix: &str) -> Result<NetDef> {
    if let Some(def) = network_get(name) {
        return Ok(def);
    }
    let def = NetDef {
        name: name.to_string(),
        bridge: format!("dlxn{:08x}", crate::fnv32(name)),
        prefix: prefix.to_string(),
        egress: EgressState::default(),
    };
    std::fs::create_dir_all(networks_dir()).map_err(|e| Error::Runtime {
        context: "networks dir",
        message: e.to_string(),
    })?;
    std::fs::write(
        netdef_path(name),
        serde_json::to_vec_pretty(&def).unwrap_or_default(),
    )?;
    Ok(def)
}

/// **Removes an ingress private network**: deletes the bridge (if the infra is
/// up) and the `NetDef`. Best-effort.
pub fn network_remove(name: &str) {
    if let Some(def) = network_get(name) {
        // `control_send` fails right away if the holder is down (network with no workloads) —
        // the bridge never lived in a netns, nothing to delete. Best-effort.
        let _ = control_send(&format!("netdel {}", def.bridge));
    }
    let _ = std::fs::remove_file(netdef_path(name));
}

// ---- host-side API: container factory + lifecycle (ref-count) ---------------

/// Deterministic IP of a container on an ingress network (`<prefix>.A.B`),
/// derived from the id — stable across invocations.
pub fn container_ip_on(prefix: &str, id: &str) -> String {
    crate::alloc_ip_in(prefix, id)
}

/// The container's IP on the default ingress network (`10.200.A.B`).
pub fn container_ip(id: &str) -> String {
    container_ip_on(INFRA_PREFIX, id)
}

/// **Attaches a container via CNI (rootless)**: ensures the infra is up (ref-count++) and
/// asks the holder to run the CNI plugins (`conf_json` = conflist) in the container's
/// netns. Returns `(netns, ip_cidr)`. The IP comes from the plugin's IPAM. On failure
/// it undoes the ref-count. Preserves rootless-first: the plugin runs in the holder (owner
/// of the netns), not on the host without privilege.
pub fn cni_attach_container(id: &str, conf_json: &str) -> Result<(String, String)> {
    acquire(id)?; // ensure_up + ref marker for `id`
    let netns = sanitize(id);
    let hex = hex_encode(conf_json.as_bytes());
    let cmd = format!(
        "cni-add {netns} {netns} {} {hex}",
        crate::cni::DEFAULT_IFNAME
    );
    match control_query(&cmd) {
        Ok(ip) => Ok((netns, ip)),
        Err(e) => {
            release(id);
            Err(e)
        }
    }
}

/// **Detaches a CNI container (rootless)**: asks the holder for the plugins' `DEL` +
/// netns removal, and frees the ref-count. Best-effort.
pub fn cni_detach_container(id: &str, conf_json: &str) -> Result<()> {
    let netns = sanitize(id);
    let hex = hex_encode(conf_json.as_bytes());
    let _ = control_send(&format!(
        "cni-del {netns} {netns} {} {hex}",
        crate::cni::DEFAULT_IFNAME
    ));
    release(id);
    Ok(())
}

/// `true` when the live holder already serves this container's netns.
///
/// The idempotence guard for reconciliation: without it, an explicit
/// `net netns up` on a perfectly healthy system would rebuild every container's
/// wire and cause the outage it exists to prevent. A holder too old to know the
/// verb answers `err:` — treated as "already served", i.e. the conservative
/// answer that changes nothing.
pub fn holder_serves_netns(id: &str) -> bool {
    match control_query(&format!("has-netns {}", sanitize(id))) {
        Ok(body) => body.trim() != "no",
        Err(_) => true,
    }
}

/// **Attaches a container to an ingress network** (`net`=`ingress` or a private
/// network name): ensures the infra is up (ref-count++), resolves the bridge/gateway and asks
/// the holder for the netns + `veth` + IP. Returns `(netns, ip)`. On failure it undoes the ref-count.
pub fn attach_container(id: &str, net: &str, namespace: &str) -> Result<(String, String)> {
    let (bridge, prefix, gateway) = resolve_net(net)?;
    let ip = crate::ipam::allocate(&prefix, id)?; // unique lease (anti-collision), stable per id
    acquire(id)?; // ensure_up + ref marker for `id`
    let netns = sanitize(id);
    // `namespace` sanitized (goes to a control-line token): no spaces/garbage.
    let ns = sanitize(if namespace.is_empty() {
        "default"
    } else {
        namespace
    });
    // Upgrade compat: `default` keeps the 5-token form (an OLD holder,
    // pre-namespaces, still accepts it); only namespaced attaches carry the 6th token and
    // require the new holder. Minimizes breakage on an in-place binary upgrade.
    let cmd = if ns == "default" {
        format!("attach {netns} {ip} {bridge} {gateway}")
    } else {
        format!("attach {netns} {ip} {bridge} {gateway} {ns}")
    };
    match control_send(&cmd) {
        Ok(()) => Ok((netns, ip)),
        Err(e) => {
            release(id); // undoes the ref marker if the attach failed
            Err(e)
        }
    }
}

/// **Attaches a RUNNING container to an ADDITIONAL network** (live multi-homing,
/// rootless): resolves the network's bridge/gateway/IP and asks the holder for the extra `veth`
/// on the interface `eth<idx>`. No new ref-count (the primary attach already holds the infra).
/// Returns `(ifname, ip)`.
pub fn attach_extra_container(
    id: &str,
    idx: u32,
    net: &str,
    namespace: &str,
) -> Result<(String, String)> {
    let (bridge, prefix, gateway) = resolve_net(net)?;
    let ip = crate::ipam::allocate(&prefix, id)?; // unique lease on the additional network
    let ifname = format!("eth{idx}");
    let netns = sanitize(id);
    // `default` keeps the 6-token form an older holder understands (same compat rule
    // `attach_container` follows); only a namespaced attach needs the newer holder.
    let line = if namespace.is_empty() || namespace == "default" {
        format!("attach-extra {netns} {ifname} {ip} {bridge} {gateway}")
    } else {
        format!("attach-extra {netns} {ifname} {ip} {bridge} {gateway} {namespace}")
    };
    control_send(&line)?;
    Ok((ifname, ip))
}

/// **Limits the bandwidth of a RUNNING container** (rootless, live):
/// asks the holder for the shaping on the infra-side veth (`vh<fnv>`). `rate_bit` in
/// bit/s, `burst_bytes` in bytes. Idempotent.
pub fn set_net_rate(id: &str, rate_bit: u64, burst_bytes: u64) -> Result<()> {
    let vh = vh_name(&sanitize(id));
    control_send(&format!("netrate {vh} {rate_bit} {burst_bytes}"))
}

/// **Removes the bandwidth limit** of a container (rootless). Best-effort.
pub fn clear_net_rate(id: &str) {
    let vh = vh_name(&sanitize(id));
    let _ = control_send(&format!("netrate-clear {vh}"));
}

/// **Detaches a container from an additional network** (live multi-homing): asks the
/// holder to remove the extra `veth` and frees the IP lease on that network. `ip` is the
/// container's IP on the additional network (from the `ExtraNet` record). Best-effort.
pub fn detach_extra_container(id: &str, idx: u32, ip: &str) {
    let netns = sanitize(id);
    let ifname = format!("eth{idx}");
    let _ = control_send(&format!("detach-extra {netns} {ifname}"));
    let _ = control_send(&format!("nsleave {ip}"));
    crate::ipam::release(&crate::ipam::prefix_of(ip), id); // frees the extra network's lease
}

/// **Detaches a container from the ingress**: clears the firewall (on its `ip`), asks the
/// holder for the `detach` and lowers the ref-count (tears down the infra on the last). Best-effort.
pub fn detach_container(id: &str, ip: &str) {
    let netns = sanitize(id);
    let _ = control_send(&format!("unfirewall {ip}"));
    let _ = control_send(&format!("detach {netns}"));
    let _ = control_send(&format!("nsleave {ip}"));
    crate::ipam::release(&crate::ipam::prefix_of(ip), id); // frees the IP lease
    release(id); // removes the ref marker (teardown when it becomes empty)
}

/// **Applies a container's parameterizable firewall AT THE INGRESS** (the only place,
/// via the bind): translates the `ContainerFw` (the same one persisted in the record, v0.1.93) to
/// the `dlxing`'s `fw<hash>` chain, keyed by the container's `ip` on its network.
///
/// Covers only the PRIMARY IP — see [`apply_firewall_all`] for a multi-homed container.
/// Kept because most callers have a single IP in hand and the wire line stays identical.
pub fn apply_firewall(id: &str, ip: &str, fw: &delonix_runtime_core::ContainerFw) -> Result<()> {
    apply_firewall_all(id, std::slice::from_ref(&ip), fw)
}

/// Like [`apply_firewall`], but governs EVERY IP the container holds (primary first, then
/// one per additional network). A multi-homed container used to be firewalled on its
/// primary address only, so `ingress`/`egress`/`Dependency`/namespace rules were all
/// bypassable by talking to it over a second network — reproduced live before the fix.
///
/// With a single IP the control line is byte-identical to the old one, so an older
/// holder keeps working for the (overwhelmingly common) single-homed case; only the
/// comma-separated multi-IP form needs the newer holder.
pub fn apply_firewall_all(
    id: &str,
    ips: &[&str],
    fw: &delonix_runtime_core::ContainerFw,
) -> Result<()> {
    if ips.is_empty() {
        return Err(Error::Invalid("apply_firewall: no IP given".into()));
    }
    let json = serde_json::to_vec(fw).map_err(|e| Error::Invalid(e.to_string()))?;
    control_send(&format!(
        "firewall {} {} {}",
        sanitize(id),
        ips.join(","),
        hex_encode(&json)
    ))
}

/// Sets the GLOBAL egress policy of the single ingress (via the holder, in the infra
/// netns). `deny` blocks all egress to the Internet; `allow` restores the default
/// (egress allowed). Idempotent.
pub fn set_egress_policy(deny: bool) -> Result<()> {
    control_send(&format!("egress {}", if deny { "deny" } else { "allow" }))
}

/// Like [`set_egress_policy`], but ONLY for the bridge `<bridge>` (per-network /
/// per-workspace egress). Doesn't affect the other networks.
pub fn set_egress_policy_net(bridge: &str, deny: bool) -> Result<()> {
    control_send(&format!(
        "egress-net {} {}",
        bridge,
        if deny { "deny" } else { "allow" }
    ))
}

/// NET-A — ALLOWLIST-mode egress for the bridge `<bridge>`: denies all egress→
/// Internet EXCEPT DNS (53) and the given `cidrs` (comma-separated list,
/// no spaces). It's the "deny everything except X" that was missing (`set_egress_policy_net`
/// is only a denylist). The CIDRs are validated (`fw_src_ok`) in the holder — anti-injection.
pub fn set_egress_policy_net_allowlist(bridge: &str, cidrs: &[&str]) -> Result<()> {
    control_send(&format!(
        "egress-net {} allowlist:{}",
        bridge,
        cidrs.join(",")
    ))
}

/// Egress by HOSTNAME: only lets the bridge egress to the IPs that resolve to
/// `<suffix>` (or `*.<suffix>`), learned live from the DNS responses. Denies the
/// rest (except DNS). Calling more than once adds hostnames to the allowlist.
pub fn set_egress_host(bridge: &str, suffix: &str) -> Result<()> {
    control_send(&format!("egress-host {bridge} {suffix}"))
}

/// Enables/updates the L4 DDoS protection (per-source rate-limit + ct-count). `conn_rate`
/// = new connections/second per IP; `conn_max` = concurrent connections per IP.
/// best-effort in the holder (degrades if the kernel doesn't support it). See `do_l4guard`.
pub fn set_l4_guard(conn_rate: u32, conn_max: u32) -> Result<()> {
    control_send(&format!("l4guard {conn_rate} {conn_max}"))
}

/// Removes the L4 DDoS protection (idempotent).
pub fn clear_l4_guard() -> Result<()> {
    control_send("l4guard-clear")
}

/// Brings up the WireGuard interface `<iface>` in the infra netns (req #6) with the node's
/// private key and the listen port. The private key goes via the control socket (0600 + SO_PEERCRED
/// = only the engine's uid). See [`crate::wg`].
pub fn set_wg_iface(
    iface: &str,
    private_key: &str,
    listen_port: u16,
    addr_cidr: &str,
) -> Result<()> {
    control_send(&format!(
        "wg-up {iface} {listen_port} {private_key} {addr_cidr}"
    ))
}

/// Adds a WireGuard peer (another node) to the overlay interface.
pub fn set_wg_peer(
    iface: &str,
    public_key: &str,
    endpoint: &str,
    allowed_ips: &[String],
) -> Result<()> {
    control_send(&format!(
        "wg-peer {iface} {public_key} {endpoint} {}",
        allowed_ips.join(",")
    ))
}

/// **Realizes an overlay network's VXLAN uplink** in the infra netns: bridge +
/// VXLAN device (`<dev>`/`<vni>`) + peers' FDB (`dsts` = `wg_ip` if encrypted,
/// otherwise `node_ip`). The gateway aligns the subnet to the one decided by the `NetworkStore`.
/// Requires the holder up (`ensure_up` first). Idempotent. See `do_vxlan`.
pub fn set_vxlan(dev: &str, vni: u32, bridge: &str, gateway: &str, dsts: &[String]) -> Result<()> {
    // Validates the destinations HERE, BEFORE interpolating them into the control-socket line
    // (the audit's valid_* discipline — validate before the `format!`/socket, not only
    // holder-side): a dst with a space/newline would malform the line or attempt
    // smuggling a 2nd command. `do_vxlan` re-validates, but this is the boundary.
    if let Some(bad) = dsts.iter().find(|d| !valid_fdb_dst(d)) {
        return Err(Error::Invalid(format!(
            "invalid overlay peer destination: {bad:?} (IPs only)"
        )));
    }
    // CSV in a single token (the control-loop does `split_whitespace`); `-` = no peers.
    let csv = if dsts.is_empty() {
        "-".to_string()
    } else {
        dsts.join(",")
    };
    control_send(&format!("vxlan {dev} {vni} {bridge} {gateway} {csv}"))
}

/// Removes a container's firewall from the ingress (best-effort).
pub fn clear_firewall(ip: &str) {
    let _ = control_send(&format!("unfirewall {ip}"));
}

// ---- VMs on the ingress (QEMU/KVM) ------------------------------------------

/// Name of a VM's `tap` (deterministic, <= 15 chars).
pub fn vm_tap_name(vm: &str) -> String {
    format!("vt{:08x}", crate::fnv32(vm))
}

/// FNV-1a hash of a name (to derive a deterministic MAC, etc.).
pub fn name_hash(s: &str) -> u32 {
    crate::fnv32(s)
}

/// **Attaches a VM to the ingress**: ensures the infra is up (ref-count++), resolves the network
/// and asks the holder for a `tap` on that network's bridge (with DHCP). Returns the tap name
/// (which QEMU uses). The guest gets an IP via DHCP (the network's pool; gateway = ingress).
/// The `vmtap` control line for a VM attach.
///
/// The 4-token form is emitted whenever there is nothing to isolate (the
/// `default` namespace, or a network whose lease could not be derived), so an
/// older holder keeps serving the overwhelmingly common case unchanged. Only a
/// genuinely namespaced VM needs the 6-token form — the same compatibility
/// idiom `attach` and `attach-extra` already use.
fn vmtap_line(tap: &str, bridge: &str, gateway: &str, ip: Option<&str>, namespace: &str) -> String {
    match (namespace, ip) {
        ("default", _) | (_, None) => format!("vmtap {tap} {bridge} {gateway}"),
        (ns, Some(ip)) => format!("vmtap {tap} {bridge} {gateway} {ip} {}", sanitize(ns)),
    }
}

/// `mac` is the guest's MAC (deterministic from the VM name — see
/// `delonix_vm::mac_for`) and `namespace` its logical isolation namespace.
/// Together they are what makes a VM a first-class citizen of the namespace
/// model: [`dhcp_lease_ip`] turns them into the address the guest WILL get, so
/// the membership (in the holder) and the chain (here) can both be installed
/// now rather than after a lease nobody watches for.
pub fn vm_attach(vm: &str, net: &str, mac: &str, namespace: &str) -> Result<String> {
    let (bridge, prefix, gateway) = resolve_net(net)?;
    // Ref key `vm-<name>` — its own namespace, distinct from the container ids
    // and the `cri-*` pods; the `prune` reaper preserves the `vm-*` (managed by
    // another store) just like the `cri-*`.
    acquire(&format!("vm-{vm}"))?;
    let tap = vm_tap_name(vm);
    let lease = dhcp_lease_ip(&prefix, mac);
    let line = vmtap_line(&tap, &bridge, &gateway, lease.as_deref(), namespace);
    if let Err(e) = control_send(&line) {
        release(&format!("vm-{vm}"));
        return Err(e);
    }
    // The chain is what actually DROPS cross-namespace traffic; the set
    // membership above only makes the VM visible to everyone else's rules. A
    // VM with membership and no chain is exactly the half-wired state pods were
    // found in — reachable from another namespace while looking isolated.
    if namespace != "default" {
        if let Some(ip) = &lease {
            let fw = delonix_runtime_core::ContainerFw {
                enabled: true,
                namespace: namespace.to_string(),
                ..Default::default()
            };
            apply_firewall(&format!("vm-{vm}"), ip, &fw)?;
        }
    }
    Ok(tap)
}

/// **Detaches a VM from the ingress**: removes the `tap`, drops its firewall chain
/// and lowers the ref-count. Best-effort.
///
/// `ip` is the VM's SDN address when the caller knows it (from the record, or
/// recomputed with [`dhcp_lease_ip`]); `None` skips the firewall teardown, which
/// is right for the orphan-cleanup path where there is no record to trust.
pub fn vm_detach(vm: &str, ip: Option<&str>) {
    if let Some(ip) = ip {
        clear_firewall(ip);
    }
    let _ = control_send(&format!("vmtapdel {}", vm_tap_name(vm)));
    release(&format!("vm-{vm}"));
}

/// `argv` to run a process (QEMU) INSIDE the holder's infra netns
/// (where the bridges and taps live). `None` if the infra isn't up.
pub fn infra_join_argv() -> Option<Vec<String>> {
    let holder = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p))?;
    Some(vec![
        "nsenter".into(),
        "-t".into(),
        holder.to_string(),
        "-U".into(),
        "-m".into(),
        "-n".into(),
        "--preserve-credentials".into(),
        "--".into(),
    ])
}

/// Like [`infra_join_argv`] but enters ONLY the net namespace (`-n`), keeping the
/// caller's user namespace and its init-ns capabilities. This is what a
/// privileged caller (root/`CAP_BPF`) needs to load an eBPF program into the
/// infra netns: entering the holder's userns (`-U`) would namespace the caps
/// away and the `bpf()` syscall would be refused. `None` if the holder is down.
pub fn infra_netns_argv() -> Option<Vec<String>> {
    let holder = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p))?;
    Some(vec![
        "nsenter".into(),
        "-t".into(),
        holder.to_string(),
        "-n".into(),
        "--".into(),
    ])
}

/// Discovers the IP of a MAC on the infra network — via the `neigh` (ARP) table INSIDE the
/// holder's netns (immediate, unlike the udhcpd's leasefile that is only written
/// periodically). Used to report the IP that DHCP assigned to a VM/client.
/// `_net` kept for signature compatibility. `None` if the MAC hasn't yet
/// appeared in the table (guest booting).
pub fn dhcp_ip_for_mac(net: &str, mac: &str) -> Option<String> {
    // A VM's IP is DETERMINISTIC from the MAC: the native DHCP server
    // (`dhcp_serve`) assigns `<prefix>.254.<10 + fnv32(mac)%240>`. It's computed
    // directly with the SAME formula, instead of reading `ip neigh` — which only shows
    // the IP after recent ARP and gave `<none>` for a live but silent VM
    // (the real reported case). This is the IP the VM gets from DHCP, available
    // as soon as it exists, and the right one for SSH.
    let (_bridge, prefix, _gw) = resolve_net(net).ok()?;
    dhcp_lease_ip(&prefix, mac)
}

/// **Publishes a port through the ingress** (the container's bind): `add_hostfwd` on the
/// single slirp (host → tap0) + DNAT on the `pre` chain (tap0 → container). `spec` is
/// `hostPort:contPort[/tcp|udp]`. This is WHERE the ingress firewall's parameterizable
/// rules live (next increment: allow/deny per port/CIDR on the same
/// surface).
pub fn publish_port(cip: &str, spec: &str) -> Result<()> {
    let (host_addr, host_port, cont_port, proto) = crate::parse_publish_addr(spec)?;
    // host → tap0:host_port (the single slirp; guest_port == host_port).
    crate::slirp_add_hostfwd(
        &slirp_sock_path(),
        &host_port,
        &host_port,
        &proto,
        host_addr.as_deref(),
    )?;
    // tap0:host_port → container:cont_port (DNAT in the infra netns, via the holder).
    control_send(&format!("publish {proto} {host_port} {cip} {cont_port}"))
}

// REMOVED: `publish_port_allow` / the `publish-allow` control verb — a pre-DNAT
// source allowlist for a published port.
//
// Removed as REDUNDANT, not as impossible. The per-container chain already filters a
// published port by source and does it correctly (`ingress allow <c> <port> --from
// <cidr>`, validated end to end against a real remote client), because the client
// address survives the hostfwd for every routable source — see `crate::SLIRP_GW`. A
// second, parallel mechanism for the same job, sitting in a different chain with
// different precedence, is how two answers to one question start disagreeing.
//
// It also had zero callers since the day it was written: the trap this codebase has
// been bitten by three times (`mount_live`, `set_net_rate`, `update_limits`) — public,
// dead, mutating shared state, with the latent bug waiting for the first real caller.

/// Removes a `host_port`'s publication: takes the `add_hostfwd` out of the slirp and the DNAT
/// out of the `pre` chain. Best-effort.
pub fn unpublish_port(host_port: &str) {
    unpublish_port_proto(host_port, None)
}

/// Like [`unpublish_port`], but for ONE protocol — `-p 53:53/tcp` and `-p 53:53/udp`
/// are two independent publications and removing one must not take the other with it.
/// `None` removes every proto (teardown paths, where the container is going away).
pub fn unpublish_port_proto(host_port: &str, proto: Option<&str>) {
    trace_unpublish("unpublish_port", host_port);
    let _ = slirp_remove_hostfwd_proto(&slirp_sock_path(), host_port, proto);
    // The 2-token form stays byte-identical for the teardown case, so an older holder
    // keeps working there; only the per-proto form needs the newer one.
    let line = match proto {
        Some(p) => format!("unpublish {host_port} {p}"),
        None => format!("unpublish {host_port}"),
    };
    let _ = control_send(&line);
}

/// Records who unpublished a port, when `DELONIX_TRACE_UNPUBLISH` is
/// set (points to a file; otherwise goes to stderr).
///
/// It's not debug left in the code by accident: there's an open bug where hostfwds of
/// LIVE containers disappear without `stop`/`rm`, and the question that closes it is
/// "who removed them?". A long-running binary (holder, `--restart` supervisor,
/// log shim) keeps running the code from when it was BORN, so
/// the answer isn't obtained by reading the repo — only by instrumenting and reproducing.
/// Zero cost when the env var isn't set.
pub fn trace_unpublish(func: &str, host_port: &str) {
    let Ok(dest) = std::env::var("DELONIX_TRACE_UNPUBLISH") else {
        return;
    };
    let pid = std::process::id();
    let exe = std::fs::read_link("/proc/self/exe")
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ppid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("PPid:"))
                .map(|l| l.trim_start_matches("PPid:").trim().to_string())
        })
        .unwrap_or_default();
    let bt = std::backtrace::Backtrace::force_capture();
    let line = format!(
        "[trace_unpublish] {func}(port={host_port}) pid={pid} ppid={ppid} exe={exe}\n{bt}\n"
    );
    if dest == "1" || dest == "stderr" {
        eprint!("{line}");
    } else if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&dest)
    {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

/// Proof, from the caller, that a set of host ports is the COMPLETE node-wide
/// picture of what is published on the shared ingress — and not just the subset
/// that one component happens to know about.
///
/// This type exists because of a real outage. `reap_orphan_hostfwds` treats
/// "not in the set" as "orphan, delete it", so a caller that passes a PARTIAL
/// list silently deletes everyone else's published ports — and an empty list
/// deletes them all. That is exactly what happened when a separate product's
/// engine called this with only its own containers: ports published through the
/// CLI died on their own, seconds after being created, and the cause took
/// several sessions to find because the deletion looked like it came from
/// nowhere.
///
/// Constructing this is therefore a deliberate act: whoever writes
/// `AuthoritativeLivePorts::new(...)` is asserting they own the whole ingress.
/// If you cannot honestly assert that, do not call the reaper.
pub struct AuthoritativeLivePorts<'a>(&'a std::collections::HashSet<u32>);

impl<'a> AuthoritativeLivePorts<'a> {
    /// Asserts that `ports` lists EVERY host port in use on this node's ingress,
    /// so that anything else really is an orphan. An empty set means "nothing is
    /// published", not "I don't know".
    pub fn new(ports: &'a std::collections::HashSet<u32>) -> Self {
        Self(ports)
    }
}

/// Reconciles the SINGLE ingress slirp's `hostfwd`s against the ports ACTUALLY in
/// use by live containers: removes the orphan entries (from containers already removed,
/// or that died without cleaning up) that would otherwise block the reuse of the host
/// port. Part of reaper #1 (port-leak). Returns how many it removed. Cheap (1
/// query to the api-socket).
///
/// See [`AuthoritativeLivePorts`] for why the argument is not a plain set.
pub fn reap_orphan_hostfwds(live_ports: AuthoritativeLivePorts<'_>) -> usize {
    let live_ports = live_ports.0;
    let sock = slirp_sock_path();
    if !sock.exists() {
        return 0;
    }
    let listed = match slirp_api(&sock, r#"{"execute":"list_hostfwd"}"#) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let v: serde_json::Value = serde_json::from_str(&listed).unwrap_or(serde_json::Value::Null);
    // The response comes as {"entries":[…]} or {"return":{"entries":[…]}} depending on the version.
    let entries = v
        .get("return")
        .and_then(|r| r.get("entries"))
        .and_then(|e| e.as_array())
        .or_else(|| v.get("entries").and_then(|e| e.as_array()));
    let mut removed = 0;
    if let Some(entries) = entries {
        for e in entries {
            let hp = e.get("host_port").and_then(|p| p.as_u64()).unwrap_or(0) as u32;
            if hp == 0 || live_ports.contains(&hp) {
                continue;
            }
            if let Some(id) = e.get("id").and_then(|i| i.as_u64()) {
                let cmd = format!(r#"{{"execute":"remove_hostfwd","arguments":{{"id":{id}}}}}"#);
                let _ = slirp_api(&sock, &cmd);
                removed += 1;
            }
        }
    }
    removed
}

/// Sends a JSON command to the single slirp's api-socket and returns the response.
fn slirp_api(sock: &Path, json: &str) -> Result<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    // Chokepoint for ALL commands to the slirp — including the
    // `remove_hostfwd`s that `reap_orphan_hostfwds` sends directly, without
    // going through `slirp_remove_hostfwd`. Instrumenting only the named functions
    // left that path invisible.
    if !json.contains("list_hostfwd") {
        trace_unpublish("slirp_api", json);
    }
    let mut s = UnixStream::connect(sock).map_err(|e| Error::Runtime {
        context: "slirp api",
        message: e.to_string(),
    })?;
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    // The `\n` is MANDATORY: slirp4netns only PARSES the command (and responds) upon seeing
    // a newline OR the client's EOF. Since here the client stays READING the response
    // (`read_to_string`) without closing the write side, without the `\n` the slirp never parsed
    // and `list_hostfwd` came back EMPTY at the end of the timeout — so
    // `slirp_remove_hostfwd` didn't find the `id` and removed NOTHING. Effect: the port
    // of a deleted cluster/container stayed stuck in the ingress (seen on the 6443 of a
    // `cluster delete`). `add_hostfwd` got away with it by parsing on EOF (fire-and-
    // forget), which hid the bug.
    let line = if json.ends_with('\n') {
        json.to_string()
    } else {
        format!("{json}\n")
    };
    s.write_all(line.as_bytes()).map_err(|e| Error::Runtime {
        context: "slirp api write",
        message: e.to_string(),
    })?;
    // Same class as the control socket's (see `control_send`): a read that FAILS
    // is not an empty reply. Here the caller that hurts is
    // `slirp_remove_hostfwd`, which parses this as JSON — an empty string parses
    // to `Null`, `hostfwd_entries` finds nothing, and the unpublish reports
    // success having removed NOTHING, leaving the host port held by an entry the
    // record no longer knows about.
    let mut resp = String::new();
    s.read_to_string(&mut resp).map_err(|e| Error::Runtime {
        context: "slirp api read",
        message: format!("no reply from the slirp api-socket: {e}"),
    })?;
    Ok(resp)
}

/// Removes a `hostfwd` from ONE slirp (the single ingress one, or a container's on the
/// slirp-per-container path): finds the `id` of the entry with that
/// `host_port` (via `list_hostfwd`) and removes it.
///
/// `pub` because `container update` needs to hot-unpublish a port
/// of a container's OWN slirp (socket `delonix-slirp-<pid>.sock`), and not
/// just the single ingress slirp — which is what [`unpublish_port`] assumes.
/// The entries of a `list_hostfwd`, tolerant of the response's SHAPE.
///
/// slirp4netns 1.2.1 responds `{"entries":[…]}` — without the `return` wrapper that
/// envelops other responses (`remove_hostfwd` gives `{"return":{}}`). The old
/// parser looked ONLY at `return.entries` and so found nothing and never
/// removed — the other half of the port-leak bug (the 1st was the missing `\n` in
/// `slirp_api`). Accepts both shapes so as not to break again across versions.
fn hostfwd_entries(v: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    v.get("entries")
        .or_else(|| v.get("return").and_then(|r| r.get("entries")))
        .and_then(|e| e.as_array())
}

/// `proto` selects WHICH publication to remove: `Some("tcp")`/`Some("udp")` removes only
/// that one, `None` removes every proto on the port.
///
/// Publishing has always been proto-aware (`-p 53:53/tcp` and `-p 53:53/udp` coexist as
/// two distinct hostfwds) but removal was not: matching on `host_port` alone tore down
/// BOTH. Reproduced live — a `--publish-rm 18100` on a container publishing tcp+udp left
/// the record still claiming `18100:53/tcp` while nothing was bound and `curl` got
/// nothing. Removal now mirrors publication.
pub fn slirp_remove_hostfwd_proto(sock: &Path, host_port: &str, proto: Option<&str>) -> Result<()> {
    trace_unpublish("slirp_remove_hostfwd", host_port);
    let hp: u32 = host_port
        .parse()
        .map_err(|_| Error::Invalid("invalid port".into()))?;
    let listed = slirp_api(sock, r#"{"execute":"list_hostfwd"}"#)?;
    let v: serde_json::Value = serde_json::from_str(&listed).unwrap_or(serde_json::Value::Null);
    if let Some(entries) = hostfwd_entries(&v) {
        for e in entries {
            if e.get("host_port").and_then(|p| p.as_u64()) != Some(hp as u64) {
                continue;
            }
            // An entry whose proto the slirp doesn't report is NOT skipped when a proto
            // was asked for — better to remove a publication we can't disambiguate than
            // to leave the host port held by something the record no longer knows about.
            if let (Some(want), Some(have)) = (proto, e.get("proto").and_then(|p| p.as_str())) {
                if !have.eq_ignore_ascii_case(want) {
                    continue;
                }
            }
            if let Some(id) = e.get("id").and_then(|i| i.as_u64()) {
                let cmd = format!(r#"{{"execute":"remove_hostfwd","arguments":{{"id":{id}}}}}"#);
                let _ = slirp_api(sock, &cmd);
            }
        }
    }
    Ok(())
}

/// [`slirp_remove_hostfwd_proto`] for every proto on the port (teardown paths:
/// `stop`/`rm`, where the whole container is going away).
pub fn slirp_remove_hostfwd(sock: &Path, host_port: &str) -> Result<()> {
    slirp_remove_hostfwd_proto(sock, host_port, None)
}

/// The `argv` prefix to RUN a process inside the netns of a container managed
/// by the holder: enters the holder's userns+mountns (`--preserve-credentials` avoids
/// the `setgroups` error) and does `ip netns exec <netns>`. The runtime prefixes this to the
/// container's command. `None` if the infra isn't up.
pub fn join_argv(id: &str) -> Option<Vec<String>> {
    let holder = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p))?;
    let netns = sanitize(id);
    Some(vec![
        "nsenter".into(),
        "-t".into(),
        holder.to_string(),
        "-U".into(),
        "-m".into(),
        "-n".into(),
        "--preserve-credentials".into(),
        "--".into(),
        "ip".into(),
        "netns".into(),
        "exec".into(),
        netns,
    ])
}

/// The `eth0` rx/tx bytes of a rootless container, read FROM INSIDE its netns
/// (via `join_argv`). From the container's point of view, `rx`=download and `tx`=upload
/// (without the swap of the root model, where the host-side veth is read). Returns
/// `(download, upload)` or `None` if the infra/container isn't up.
/// Sums `(rx_bytes, tx_bytes)` over every non-loopback interface in a
/// `/proc/net/dev` dump. `None` when the text has no usable interface line at
/// all (an empty/unparseable read must not masquerade as "0 bytes of traffic").
///
/// Column layout, fixed since forever: `iface: rx_bytes rx_packets rx_errs
/// rx_drop rx_fifo rx_frame rx_compressed rx_multicast tx_bytes …` — so rx is
/// field 0 and tx is field 8 after the colon.
///
/// `lo` is excluded: container-local loopback traffic is not network usage, and
/// on a busy container it dwarfs the real numbers (this host's own `lo` shows
/// 487 GB against 13 GB on the actual NIC).
fn parse_proc_net_dev(text: &str) -> Option<(u64, u64)> {
    let mut rx_total = 0u64;
    let mut tx_total = 0u64;
    let mut seen = false;
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue; // the two header lines have no colon in the name position
        };
        let name = name.trim();
        if name.is_empty() || name == "lo" {
            continue;
        }
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() < 9 {
            continue;
        }
        let (Ok(rx), Ok(tx)) = (f[0].parse::<u64>(), f[8].parse::<u64>()) else {
            continue;
        };
        rx_total = rx_total.saturating_add(rx);
        tx_total = tx_total.saturating_add(tx);
        seen = true;
    }
    seen.then_some((rx_total, tx_total))
}

/// Cumulative `(rx_bytes, tx_bytes)` of a container, summed over ALL of its
/// interfaces. `None` when the container has no netns to enter (`--net
/// host`/`--net none`) — callers must surface that as *unmeasured*, never fold
/// it in as zero.
///
/// BUG FIXED HERE: this read `/sys/class/net/**eth0**/statistics/*`, a single
/// hardcoded interface. A multi-homed container — `container update
/// --net-connect`, a first-class feature — carries its second network on
/// `eth1`, and every byte of it was invisible. Worse than invisible: the
/// function still returned `Some`, so `dashstats::collect` counted the
/// container as successfully measured and `network_unmeasured_containers`
/// stayed at zero. The gauge came out **falsely complete** — exactly the
/// failure mode that field was added to prevent for `--net host/none`.
///
/// This is the same blind spot the firewall and the namespace isolation both
/// had (see the "primary IP" lesson in CLAUDE.md): a container does not have
/// *an* interface, it has a primary one plus however many `--net-connect`
/// added. Reading `/proc/net/dev` enumerates whatever is actually there instead
/// of naming one.
///
/// Cheaper, too: ONE `nsenter`+`cat` per container instead of two (this runs
/// per running container on every expensive collection).
pub fn container_net_bytes(id: &str) -> Option<(u64, u64)> {
    let mut argv = join_argv(id)?;
    argv.push("cat".into());
    argv.push("/proc/net/dev".into());
    let out = Command::new(&argv[0]).args(&argv[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_proc_net_dev(&String::from_utf8_lossy(&out.stdout))
}

/// Sends a command to the holder's control socket and waits for `ok`. Retries
/// briefly until the socket exists (the holder creates it on startup).
fn control_send(cmd: &str) -> Result<()> {
    // Only the commands that UNDO state — the trace serves to answer "who
    // turned this off?", and a log of every attach/publish would drown out the answer.
    if cmd.starts_with("unpublish") || cmd.starts_with("detach") || cmd.starts_with("unfirewall") {
        trace_unpublish("control_send", cmd);
    }
    control_query(cmd).map(|_| ())
}

/// Like `control_send`, but returns the BODY of the response after `ok ` (empty if just
/// `ok`). Used by `cni-add`, whose response carries the IP assigned by the IPAM.
fn control_query(cmd: &str) -> Result<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    // Fast-fail if the holder is NOT alive: without it there's no one to respond, and
    // spinning 50×40ms (~2s) waiting for a socket that won't come is pure waste. The
    // SETUP paths call `ensure_up()` first (holder alive → passes); the
    // TEARDOWN ones with the holder down exit here. The retry below still covers the
    // legitimate startup race (holder ALREADY alive, socket still coming up).
    let Some(holder_pid) = status().holder_pid else {
        return Err(Error::Runtime {
            context: "control socket",
            message: "ingress holder is down".into(),
        });
    };
    let sock = control_sock_path();
    let mut last = String::from("control socket unavailable");
    for _ in 0..50 {
        match UnixStream::connect(&sock) {
            Ok(mut s) => {
                let _ = s.set_read_timeout(Some(CONTROL_REPLY_TIMEOUT));
                s.write_all(format!("{cmd}\n").as_bytes())
                    .map_err(|e| Error::Runtime {
                        context: "control write",
                        message: e.to_string(),
                    })?;
                // A read that FAILS is not an empty reply. This used to be
                // `let _ = s.read_to_string(...)`, which threw the error away and
                // left `resp` empty — so a 5s read timeout was indistinguishable
                // from a holder that answered with nothing, and both printed an
                // error with nothing after the colon.
                //
                // Measured on this host, and it is not noise — it scales with
                // concurrency, because `handle_control` is THE serialization
                // point and a queued caller waits for every attach ahead of it:
                // 10 concurrent attaches → 0 failures, 20 → 3, 30 → 15.
                let mut resp = String::new();
                if let Err(e) = s.read_to_string(&mut resp) {
                    let timed_out = matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    );
                    return Err(Error::Runtime {
                        context: "ingress control",
                        message: if timed_out {
                            format!(
                                "the control plane did not reply within {CONTROL_REPLY_TIMEOUT:?}                                  - it serializes every network operation, so a burst of                                  concurrent `run`s queues up behind itself. Retry, or start them                                  in smaller batches"
                            )
                        } else {
                            format!("reading the reply failed: {e}")
                        },
                    });
                }
                let resp = resp.trim();
                if resp == "ok" {
                    return Ok(String::new());
                }
                if let Some(body) = resp.strip_prefix("ok ") {
                    return Ok(body.trim().to_string());
                }
                // An EMPTY reply is not an empty error message. The holder closes
                // the connection without a word in two places (`read_control_line`
                // giving up, and the `SO_PEERCRED` mismatch), and both used to
                // surface as a bare `system call `ingress control` failed:` with
                // nothing after the colon. Measured under load: 16 of 30
                // concurrent attaches on this host failed exactly like that — an
                // error with no subject, which is the one thing this codebase
                // keeps having to remove.
                let body = resp.trim_start_matches("err:").trim();
                return Err(Error::Runtime {
                    context: "ingress control",
                    message: if body.is_empty() {
                        "the control plane closed the connection without replying - it is \
                         likely saturated (or died); check `delonix net netns status` and retry"
                            .into()
                    } else {
                        body.to_string()
                    },
                });
            }
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
    }
    // The retries are exhausted. A socket that never even APPEARED is not the same
    // failure as one that exists and refuses the connection — say which (the
    // teardown paths reach here too, hence the check on this side as well: they
    // don't go through `ensure_up`).
    let message = if sock.exists() {
        last
    } else {
        let legacy = legacy_control_sock_path();
        stale_holder_message(
            holder_pid,
            &sock,
            legacy.exists().then_some(legacy.as_path()),
        )
    };
    Err(Error::Runtime {
        context: "control socket",
        message,
    })
}

/// Atomic write of the state file (tmp + rename) so the parent never reads a
/// partial value.
fn write_status(s: &str) {
    let _ = std::fs::create_dir_all(ingress_dir());
    let tmp = ingress_dir().join(".status.tmp");
    if std::fs::write(&tmp, s).is_ok() {
        let _ = std::fs::rename(&tmp, status_path());
    }
}

/// Configures the infra netns (runs inside the holder). The proven recipe: lo up
/// → bridge `delonix0` 10.200.0.1/16 up → `ip_forward=1` → tmpfs at `/run/netns`
/// (for Phase 3 to create container netns) → ingress `nft` table.
/// `true` if this netns is ALREADY set up (the pin survived and only the control
/// process restarted). Probed from the kernel — the presence of the ingress
/// bridge — rather than from a flag, because a flag can be stale and the wire
/// cannot.
fn infra_netns_already_built() -> bool {
    link_exists(INFRA_BRIDGE)
}

/// Does `name` exist as a link IN THE CURRENT NETNS?
///
/// Asked over netlink (`ip link show`) and deliberately NOT via
/// `/sys/class/net/<name>`, which was the first attempt and is wrong here:
/// sysfs reports the netns of the process that MOUNTED it, not the caller's. The
/// pin never remounts `/sys`, so from inside the control plane that directory is
/// still the HOST's — it showed no `delonix0` for a netns that had one, the
/// reattach never triggered, and the control died on
/// `ip link add delonix0: File exists`. Measured, not reasoned about.
fn link_exists(name: &str) -> bool {
    // On the OUTPUT, never on the `Result`: `capture` returns `Ok(stdout)` even
    // when the command exits non-zero — it does not look at the status at all.
    // The first version of this used `.is_ok()` and was therefore ALWAYS true, so
    // the control plane took the reattach path on a virgin netns, built nothing,
    // and reported `ingress UP` over a netns with no bridge in it. `ip link show`
    // prints the link when it exists and nothing when it does not, which is the
    // signal worth reading.
    crate::capture("ip", &["link", "show", name])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Builds the infra netns, or REATTACHES to one that is already built.
///
/// Re-running the build over a live netns is not merely wasteful, it is
/// destructive, and each of these was checked rather than assumed:
///
///   * `mount -t tmpfs none /run` would mount a SECOND tmpfs over the existing
///     one, hiding `/run/netns` — i.e. every named netns of every pod and every
///     container on the node, instantly unreachable by name;
///   * `ip link add delonix0` / `ip addr add` return `File exists` and, being
///     `?`-propagated, would abort the whole control startup;
///   * re-applying the base `nft` ruleset re-appends the dispatch rules of
///     `fwcont` on every restart (the ruleset merges into the existing table —
///     it has no `flush`, which is why the container firewalls survive at all).
///
/// So on reattach only the PROCESS-LOCAL state is rebuilt: the DHCP servers,
/// which are threads and died with the previous control. `DHCP_STARTED` is a
/// process-local static, so a fresh process starts with an empty set and every
/// bridge legitimately needs one again — the default ingress plus each private
/// network's own.
fn reattach_or_setup_infra_netns() -> Result<()> {
    if !infra_netns_already_built() {
        return setup_infra_netns();
    }
    start_dhcp(INFRA_BRIDGE, INFRA_PREFIX);
    for def in network_list() {
        if link_exists(&def.bridge) {
            start_dhcp(&def.bridge, &def.prefix);
        }
    }
    Ok(())
}

fn setup_infra_netns() -> Result<()> {
    // the holder's mounts become private (don't leak to the host).
    run_ok("mount", &["--make-rprivate", "/"]);
    run("ip", &["link", "set", "lo", "up"])?;
    run("ip", &["link", "add", INFRA_BRIDGE, "type", "bridge"])?;
    run("ip", &["addr", "add", INFRA_CIDR, "dev", INFRA_BRIDGE])?;
    run("ip", &["link", "set", INFRA_BRIDGE, "up"])?;
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1").map_err(|e| Error::Runtime {
        context: "ip_forward",
        message: e.to_string(),
    })?;
    // /run/netns for the containers' `ip netns` (Phase 3); best-effort.
    run_ok("mount", &["-t", "tmpfs", "none", "/run"]);
    let _ = std::fs::create_dir_all("/run/netns");
    apply_nft_stdin(&ingress_table_ruleset())?;
    // Second layer of the IPv6 refusal (the first is `disable_ipv6` per container, in
    // `do_attach`). Best-effort ON PURPOSE and not `?`: a kernel built without
    // `nf_tables` IPv6 support would otherwise take the whole holder down with it,
    // and on such a kernel there is no v6 forwarding to protect against anyway. When
    // v6 is explicitly opted back in, the refusal is not installed at all — the whole
    // point of the opt-in.
    if !ipv6_sdn_enabled() {
        if let Err(e) = apply_nft_stdin(&ingress_v6_refusal_ruleset()) {
            tracing::warn!(
                error = %e,
                "could not install the IPv6 refusal table; containers still have no v6 \
                 addresses (disable_ipv6 per netns), but a privileged container that turns \
                 v6 back on would not be filtered"
            );
        }
    }
    // L4 DDoS protection by default (req #5): PER-SOURCE rate-limit + ct-count.
    // Conservative limits (legitimate traffic is not affected), best-effort and with
    // `nft -c` pre-flight (degrades on kernels without `meter`). Configurable via API.
    let _ = do_l4guard(50, 200);
    // DHCP for the default ingress network (delonix0).
    start_dhcp(INFRA_BRIDGE, INFRA_PREFIX);
    Ok(())
}

/// Applies an `nft` *ruleset* via stdin (`nft -f -`) — variant local to the holder
/// (the one in `lib.rs` is private to that module).
fn apply_nft_stdin(ruleset: &str) -> Result<()> {
    use std::io::Write;
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Runtime {
            context: "spawn nft",
            message: e.to_string(),
        })?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(ruleset.as_bytes())
        .map_err(|e| Error::Runtime {
            context: "nft stdin",
            message: e.to_string(),
        })?;
    let out = child.wait_with_output().map_err(|e| Error::Runtime {
        context: "nft wait",
        message: e.to_string(),
    })?;
    if !out.status.success() {
        return Err(Error::Runtime {
            context: "nft -f",
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// Re-exports the slirp's tap0 IP (infra side) — the destination of the `add_hostfwd`s.
pub const INFRA_SLIRP_IP: &str = SLIRP_IP;

// ---- ingress internal DNS (own responder; dnsmasq doesn't run rootless) ----

/// **Ingress DNS server** — runs in a holder thread, listens on UDP `:53` on
/// ALL bridges (`0.0.0.0` in the infra netns → responds on each gateway).
/// Resolves names of ingress **containers and VMs** (→ IPv4); forwards the rest
/// to the upstream (the slirp's DNS). It's the functional equivalent of dnsmasq (which doesn't
/// work rootless).
/// Upper bound on DNS queries handled concurrently. UDP `:53` accepts
/// unauthenticated traffic from anyone on the ingress bridges — without a cap, a
/// flood of garbage queries would spawn one thread per packet, unbounded.
const DNS_MAX_INFLIGHT: usize = 64;
static DNS_INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn dns_server_main() {
    let sock = match std::net::UdpSocket::bind("0.0.0.0:53") {
        Ok(s) => s,
        Err(_) => return,
    };
    let sock = std::sync::Arc::new(sock);
    let mut buf = [0u8; 1500];
    loop {
        let (n, peer) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(_) => continue,
        };
        if n < 12 {
            continue;
        }
        // BUG FIXED: `handle_dns` used to run INLINE here, in the single accept
        // loop. `forward_dns` can block up to ~6s (2 upstreams × 3s timeout) on a
        // slow/dead external resolver — one query for an unreachable domain
        // stalled DNS for the WHOLE node (including lookups of live
        // containers/VMs, which don't touch the network at all) for the duration
        // of that timeout. Each query now gets its own short-lived thread; a slow
        // forward blocks only its own client, not the node.
        use std::sync::atomic::Ordering;
        if DNS_INFLIGHT.load(Ordering::Relaxed) >= DNS_MAX_INFLIGHT {
            continue; // drop: the resolver on the other end retries/times out, same as an overloaded server would
        }
        DNS_INFLIGHT.fetch_add(1, Ordering::Relaxed);
        let q = buf[..n].to_vec();
        let sock2 = sock.clone();
        // The client's address decides WHAT it may resolve (ADR-0011). It was
        // available here all along and thrown away.
        let client = match peer.ip() {
            std::net::IpAddr::V4(a) => Some(a.octets()),
            std::net::IpAddr::V6(_) => None,
        };
        std::thread::spawn(move || {
            if let Some(r) = handle_dns(&q, client) {
                let _ = sock2.send_to(&r, peer);
            }
            DNS_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;
const RCODE_NOERROR: u8 = 0;
const RCODE_NXDOMAIN: u8 = 3;

/// Is this name inside the zone we are AUTHORITATIVE for? A name under
/// `.delonix.internal` is ours by construction and must NEVER be forwarded: the
/// upstream cannot know it, so forwarding only leaks every workload and
/// namespace name of every tenant to an external resolver (`SLIRP_DNS`, then
/// `1.1.1.1`) and pays that resolver's latency to be told what we already know.
///
/// `.delonix.io` is deliberately NOT here: it is a real, publicly resolvable
/// domain, and claiming authority over it would blackhole it for containers.
fn is_internal_zone(name: &str) -> bool {
    let n = name.trim_end_matches('.').to_lowercase();
    n == "delonix.internal" || n.ends_with(".delonix.internal")
}

/// What to do with a query. PURE (no I/O), so the decision table is testable on
/// its own — the part that used to be wrong was the DECISION, not the encoding.
#[derive(Debug, PartialEq, Eq)]
enum DnsAction {
    /// We know this name and the query asks for `A`.
    Answer([u8; 4]),
    /// The name exists but has no record of the requested type. An empty
    /// NOERROR — NOT an error, and the distinction is the whole point: a
    /// resolver reads NODATA as "no address of this family, carry on" and a
    /// SERVFAIL as "the lookup failed", which makes `getaddrinfo()` fail the
    /// WHOLE resolution, `A` record included.
    NoData,
    /// Our zone, name unknown. Authoritative negative — never leaves the node.
    NxDomain,
    /// Not ours: forward upstream (unchanged behaviour).
    Forward,
}

/// BUG FIXED (both halves of the same defect): only `qtype == 1` was ever
/// answered locally, and NOTHING generated a negative reply — so every `AAAA`,
/// which `getaddrinfo()` emits ALONGSIDE the `A` for essentially every real
/// client (Go, Java, Node, Python, curl, nc, wget), was forwarded to the
/// upstream. For a bare container name the upstream answers SERVFAIL, and both
/// musl and glibc treat a SERVFAIL in either half as failure of the whole
/// lookup: measured live, `nslookup -type=a weba` returned the right address
/// while `wget http://weba:8080/` died with `bad address`. Service discovery
/// was therefore broken for every client that resolves the normal way, and only
/// worked for tools asking for `A` explicitly (`ping`, `getent`) — which is why
/// it survived manual testing.
fn dns_action(qtype: u16, name: &str, resolved: Option<[u8; 4]>) -> DnsAction {
    if qtype == QTYPE_A {
        if let Some(ip) = resolved {
            return DnsAction::Answer(ip);
        }
    } else if resolved.is_some() {
        // Known name, other type (AAAA and everything else). IPv6 is disabled
        // node-wide by design (v0.37.1), so "no AAAA" is the TRUTH here, not a
        // gap — and saying it ourselves costs nothing and leaks nothing.
        return DnsAction::NoData;
    }
    if is_internal_zone(name) {
        return DnsAction::NxDomain;
    }
    DnsAction::Forward
}

/// Builds an answer-less reply (NODATA when `rcode` is 0, NXDOMAIN when 3),
/// echoing the question back as the RFC requires. `AA` is set because we really
/// are authoritative for what we answer this way; `RD` is copied from the query
/// rather than assumed, so we don't claim the client asked for recursion.
fn negative_reply(q: &[u8], qend: usize, rcode: u8) -> Vec<u8> {
    let mut r = Vec::with_capacity(qend);
    r.extend_from_slice(&q[0..2]); // original ID
    r.push(0x84 | (q[2] & 0x01)); // QR=1, AA=1, RD copied from the query
    r.push(0x80 | rcode); // RA=1 + rcode
    r.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
    r.extend_from_slice(&[0x00, 0x00]); // ANCOUNT=0
    r.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // NSCOUNT=0, ARCOUNT=0
    r.extend_from_slice(&q[12..qend]); // original question
    r
}

/// Response to a DNS query: answers AUTHORITATIVELY for names we know and for
/// our own zone (positively, or with NODATA/NXDOMAIN); forwards the rest.
/// `client` is the query's source address — it scopes what may be resolved.
fn handle_dns(q: &[u8], client: Option<[u8; 4]>) -> Option<Vec<u8>> {
    // parse the 1st question (offset 12): labels until 0x00, then QTYPE+QCLASS.
    let mut i = 12usize;
    let mut name = String::new();
    while i < q.len() {
        let len = q[i] as usize;
        if len == 0 {
            i += 1;
            break;
        }
        if len > 63 || i + 1 + len > q.len() {
            return forward_dns(q);
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&String::from_utf8_lossy(&q[i + 1..i + 1 + len]));
        i += 1 + len;
    }
    if i + 4 > q.len() {
        return forward_dns(q);
    }
    let qtype = u16::from_be_bytes([q[i], q[i + 1]]);
    let qend = i + 4; // end of the question (QTYPE+QCLASS)
                      // Only the address types (and our own zone) need the index consulted; an
                      // `MX` for an external domain must not pay for a lookup that cannot match.
    let resolved = if qtype == QTYPE_A || qtype == QTYPE_AAAA || is_internal_zone(&name) {
        dns_resolve_for(&name, client)
    } else {
        None
    };
    match dns_action(qtype, &name, resolved) {
        DnsAction::Answer(ip) => {
            let mut r = Vec::with_capacity(qend + 16);
            r.extend_from_slice(&q[0..2]); // original ID
            r.extend_from_slice(&[0x81, 0x80]); // flags: response + RA
            r.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
            r.extend_from_slice(&[0x00, 0x01]); // ANCOUNT=1
            r.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // NSCOUNT=0, ARCOUNT=0
            r.extend_from_slice(&q[12..qend]); // original question
            r.extend_from_slice(&[0xc0, 0x0c]); // pointer to the name (offset 12)
            r.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // TYPE A, CLASS IN
            r.extend_from_slice(&[0x00, 0x00, 0x00, 0x1e]); // TTL 30s
            r.extend_from_slice(&[0x00, 0x04]); // RDLENGTH 4
            r.extend_from_slice(&ip);
            Some(r)
        }
        DnsAction::NoData => Some(negative_reply(q, qend, RCODE_NOERROR)),
        DnsAction::NxDomain => Some(negative_reply(q, qend, RCODE_NXDOMAIN)),
        DnsAction::Forward => {
            // External name: forwards and, if it's on an FQDN allowlist, learns the
            // response's A-records into the egress nft set (before returning it).
            let resp = forward_dns(q)?;
            snoop_fqdn(&name, &resp);
            Some(resp)
        }
    }
}

/// Forwards the raw query to the upstream (the slirp's DNS; fallback 1.1.1.1) and
/// returns the response.
fn forward_dns(q: &[u8]) -> Option<Vec<u8>> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .ok()?;
    for up in [crate::SLIRP_DNS, "1.1.1.1"] {
        // `connect()` + `send`/`recv` instead of `send_to`/`recv_from`: the
        // socket used to accept the FIRST datagram to reach its ephemeral port
        // from ANYONE (the source was read into `_`, and neither the transaction
        // id nor the question was checked against what we sent). Any container
        // able to guess the port within the 3s window could beat the upstream
        // and have its forged answer handed straight to another container — the
        // classic off-path poisoning shape. A connected UDP socket makes the
        // KERNEL drop anything not from this upstream, which costs nothing.
        if sock.connect(format!("{up}:53")).is_ok() && sock.send(q).is_ok() {
            let mut buf = [0u8; 1500];
            if let Ok(n) = sock.recv(&mut buf) {
                return Some(buf[..n].to_vec());
            }
        }
    }
    None
}

/// Resolves an ingress name (container OR VM) → IPv4. Accepts `name` and
/// `name.delonix.io`. Reads the containers' records and the VMs' metas.
/// Splits an internal DNS name into `(container, optional_namespace)`. Accepts the
/// schemes: `<name>`, `<name>.delonix.io` (legacy, any namespace) and
/// `<name>.<namespace>.delonix.internal` (with namespace verification). PURE
/// (testable). Returns `None` if it ends up empty.
pub fn parse_internal_name(name: &str) -> Option<(String, Option<String>)> {
    let n = name.trim_end_matches('.').to_lowercase();
    // ONLY `.delonix.internal` does namespace matching (`<name>.<namespace>`) — an
    // EXTERNAL domain `foo.com` CANNOT be hijacked by a container 'foo' in the
    // 'com' namespace. Container names have no `.`, so the last segment is the
    // namespace.
    if let Some(core) = n.strip_suffix(".delonix.internal") {
        if core.is_empty() {
            return None;
        }
        return match core.rsplit_once('.') {
            Some((cname, ns)) if !cname.is_empty() && !ns.is_empty() => {
                Some((cname.to_string(), Some(ns.to_string())))
            }
            _ => Some((core.to_string(), None)),
        };
    }
    // `.delonix.io` (legacy) and SIMPLE names: match the WHOLE name, without splitting into
    // namespace (preserves the old behavior — a `foo.com` with no container 'foo.com'
    // doesn't match and forwards).
    let core = n.strip_suffix(".delonix.io").unwrap_or(&n);
    if core.is_empty() {
        return None;
    }
    Some((core.to_string(), None))
}

/// How long a built ingress-DNS index is trusted before a query pays for a
/// rebuild. BUG FIXED: `dns_resolve` used to do a full directory scan + JSON
/// parse of EVERY container/VM record (plus, per VM lacking a static IP, its own
/// `ip neigh show` subprocess exec) on EVERY single query — including queries
/// for entirely external domains, since `parse_internal_name` never returns
/// `None` for a bare hostname. On a node running many containers this made
/// every DNS lookup (even a miss forwarded upstream) pay for an O(n) scan
/// first. The index is now rebuilt at most once per `DNS_INDEX_TTL`; each query
/// does an O(1) `HashMap` lookup instead. 2s keeps a newly-created/renamed
/// container resolvable promptly — well under the 30s TTL already stamped on
/// the generated `A` responses, so the staleness window is not user-visible in
/// practice.
const DNS_INDEX_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// Namespaced key: only a query that explicitly asks for `<name>.<namespace>.
/// delonix.internal` matches this — mirrors the `continue`-past-mismatched-
/// namespace behavior the original per-query scan had (no silent fallback to a
/// same-named container in a DIFFERENT namespace).
fn dns_index_ns_key(name: &str, ns: &str) -> String {
    format!("{name}@{}", ns.to_lowercase())
}

/// VM key, distinct namespace from container keys: VMs have no namespace
/// concept (matches the original code, whose VM branch never checked
/// `want_ns`), and containers are always tried first — same priority the
/// original two-section scan had.
fn dns_index_vm_key(name: &str) -> String {
    format!("vm:{name}")
}

/// The default (shared) namespace. Reachable from every namespace by design —
/// see ADR-0011 §5 for why that stays true and why it is not the leak.
const NS_DEFAULT: &str = "default";

/// One resolvable name, with everything needed to decide WHO may resolve it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DnsEntry {
    ip: [u8; 4],
    ns: String,
    /// Inbound-allow CIDRs already persisted in the workload's firewall — i.e.
    /// what a `kind: Dependency` opened. Carried here so the resolver can mirror
    /// the dataplane instead of keeping a second, drift-prone opinion about
    /// reachability (ADR-0011 §1).
    allow_in: Vec<(u32, u32)>,
}

/// The built index: names to entries, plus the reverse map used to place the
/// CLIENT (its source address is all we get from `recv_from`).
#[derive(Default)]
struct DnsIndex {
    by_key: std::collections::HashMap<String, DnsEntry>,
    ns_of_ip: std::collections::HashMap<[u8; 4], String>,
}

/// Parses `a.b.c.d/len` into `(network, mask)`. A bare address is `/32`.
fn parse_cidr(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    if s.is_empty() || s == "*" {
        return Some((0, 0)); // any
    }
    let (addr, len) = match s.split_once('/') {
        Some((a, l)) => (a, l.parse::<u32>().ok()?),
        None => (s, 32),
    };
    if len > 32 {
        return None;
    }
    let o = parse_v4(addr)?;
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    Some((u32::from_be_bytes(o) & mask, mask))
}

fn cidr_contains(cidr: (u32, u32), ip: [u8; 4]) -> bool {
    let (net, mask) = cidr;
    u32::from_be_bytes(ip) & mask == net
}

/// The CIDR of an inbound `allow`, as far as NAME VISIBILITY is concerned —
/// `None` when the rule must not widen it.
///
/// A rule allowing the whole world (`0.0.0.0/0`, `*`, empty) is a port-level
/// decision — "this port is open" — and NOT "publish this name to every
/// tenant". Letting it through would quietly restore the global visibility
/// ADR-0011 removes, through the single most common firewall rule there is.
/// Kept here, next to the parsing, so the index and the tests cannot disagree
/// about it.
fn scoping_allow_cidr(src: &str) -> Option<(u32, u32)> {
    parse_cidr(src).filter(|(_, mask)| *mask != 0)
}

/// May a client sitting in `client_ns` (at `client_ip`) resolve this entry?
/// PURE — the whole point of ADR-0011 is a decision that mirrors the dataplane,
/// and a decision worth trusting is one that can be tested as a table.
fn dns_scope_allows(entry: &DnsEntry, client_ns: &str, client_ip: [u8; 4]) -> bool {
    // Same namespace: the dataplane accepts (`@dlxns_<ns>`).
    if entry.ns.eq_ignore_ascii_case(client_ns) {
        return true;
    }
    // The shared namespace is reachable from anywhere, by design.
    if entry.ns.eq_ignore_ascii_case(NS_DEFAULT) {
        return true;
    }
    // Explicitly opened for this client — `kind: Dependency`. Without this, a
    // dependency that crosses namespaces would work by address and not by name,
    // which is the "accepted and then ignored" shape this repo keeps removing.
    entry.allow_in.iter().any(|c| cidr_contains(*c, client_ip))
}

/// Parses `ip -o neigh show` output into `(lowercased line, ip)` pairs — same
/// substring-match semantics `neigh_ip_local` always used, just decomposed so
/// the table is fetched ONCE per index build instead of once per VM per query.
fn parse_neigh_table(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|line| {
            let ip = line.split_whitespace().next()?;
            ip.contains('.')
                .then(|| (line.to_lowercase(), ip.to_string()))
        })
        .collect()
}

fn neigh_table_lookup(table: &[(String, String)], mac: &str) -> Option<String> {
    let mac = mac.to_lowercase();
    table
        .iter()
        .find(|(line, _)| line.contains(&mac))
        .map(|(_, ip)| ip.clone())
}

/// Builds the full ingress-DNS index from disk (containers + VMs). Real
/// filesystem/subprocess I/O — called at most once per `DNS_INDEX_TTL` by
/// [`dns_index`], never directly from a query.
fn build_dns_index() -> DnsIndex {
    let mut idx = DnsIndex::default();
    // containers: <base>/containers/*.json (name + ip [+ namespace + firewall])
    if let Ok(rd) = std::fs::read_dir(base_root().join("containers")) {
        for e in rd.flatten() {
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(e.path()).unwrap_or_default(),
            ) else {
                continue;
            };
            let Some(name) = v["name"].as_str().map(|s| s.to_lowercase()) else {
                continue;
            };
            let Some(ip) = v["ip"].as_str().and_then(parse_v4) else {
                continue;
            };
            let ns = v["namespace"].as_str().unwrap_or(NS_DEFAULT).to_string();
            // What an explicit inbound `allow` opened for this workload — the
            // only thing that lets a client in ANOTHER namespace resolve it
            // (ADR-0011 §1). Read straight from the persisted firewall so there
            // is no second opinion about reachability to drift from.
            let allow_in: Vec<(u32, u32)> = v["firewall"]["rules"]
                .as_array()
                .map(|rules| {
                    rules
                        .iter()
                        .filter(|r| {
                            r["dir"].as_str().unwrap_or("in") == "in"
                                && r["action"].as_str().unwrap_or("allow") == "allow"
                        })
                        .filter_map(|r| scoping_allow_cidr(r["src"].as_str().unwrap_or_default()))
                        .collect()
                })
                .unwrap_or_default();
            let entry = DnsEntry {
                ip,
                ns: ns.clone(),
                allow_in,
            };
            // Bare key: with names now unique per (namespace, name), a bare
            // collision is possible ON PURPOSE — two tenants may both own `db`.
            // Both are indexed under the namespaced key, and the scope check is
            // what picks the right one for the asker; the bare key keeps only
            // the first as a fallback for clients we cannot place.
            idx.by_key.entry(name.clone()).or_insert(entry.clone());
            idx.by_key
                .entry(dns_index_ns_key(&name, &ns))
                .or_insert(entry.clone());
            // Reverse map: the client is identified by its source address.
            idx.ns_of_ip.entry(ip).or_insert(ns.clone());
            // A pod resolves by its OWN name too (ADR-0011 §6): every member
            // shares the address, and the pod is the thing that owns it. The
            // name comes from the label the creator writes, not from parsing
            // the netns name.
            if let Some(pod) = v["labels"]["delonix.io/pod"].as_str() {
                let pod = pod.to_lowercase();
                idx.by_key.entry(pod.clone()).or_insert(entry.clone());
                idx.by_key
                    .entry(dns_index_ns_key(&pod, &ns))
                    .or_insert(entry);
            }
        }
    }
    // VMs: <base>/vms/*.json (name + mac) → IP via the neigh table (fetched once).
    if let Ok(rd) = std::fs::read_dir(base_root().join("vms")) {
        let neigh_table: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
        for e in rd.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(e.path()).unwrap_or_default(),
            ) else {
                continue;
            };
            let Some(name) = v["name"].as_str().map(|s| s.to_lowercase()) else {
                continue;
            };
            // Recorded IP first: a libvirt nat/bridge VM lives on the HOST's
            // virbr0 — its MAC never shows up in the holder's neigh table, so
            // without this branch those VMs simply didn't resolve.
            let ip = v["ip"]
                .as_str()
                .filter(|s| !s.is_empty())
                .and_then(parse_v4)
                .or_else(|| {
                    let mac = v["mac"].as_str()?;
                    let table = neigh_table.get_or_init(|| {
                        let raw =
                            crate::capture("ip", &["-o", "neigh", "show"]).unwrap_or_default();
                        parse_neigh_table(&raw)
                    });
                    neigh_table_lookup(table, mac).as_deref().and_then(parse_v4)
                });
            if let Some(ip) = ip {
                let ns = v["namespace"].as_str().unwrap_or(NS_DEFAULT).to_string();
                idx.by_key
                    .entry(dns_index_vm_key(&name))
                    .or_insert(DnsEntry {
                        ip,
                        ns: ns.clone(),
                        allow_in: Vec::new(),
                    });
                idx.ns_of_ip.entry(ip).or_insert(ns);
            }
        }
    }
    idx
}

/// Shortest gap between two forced rebuilds triggered by an unplaceable client.
/// A rebuild is a directory scan, so an unknown source must not be able to turn
/// DNS traffic into filesystem load; 200ms is far below the TTL and still makes
/// a container that started milliseconds ago resolvable on its FIRST lookup.
const DNS_INDEX_MIN_REBUILD: std::time::Duration = std::time::Duration::from_millis(200);

type DnsCache = std::sync::Mutex<Option<(std::time::Instant, std::sync::Arc<DnsIndex>)>>;

fn dns_index_cell() -> &'static DnsCache {
    static CACHE: std::sync::OnceLock<DnsCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

fn dns_index() -> std::sync::Arc<DnsIndex> {
    dns_index_within(DNS_INDEX_TTL)
}

/// The index, rebuilt if the cached one is older than `max_age`.
fn dns_index_within(max_age: std::time::Duration) -> std::sync::Arc<DnsIndex> {
    let mut guard = dns_index_cell().lock().unwrap();
    let fresh = guard
        .as_ref()
        .map(|(built_at, _)| built_at.elapsed() < max_age)
        .unwrap_or(false);
    if !fresh {
        *guard = Some((
            std::time::Instant::now(),
            std::sync::Arc::new(build_dns_index()),
        ));
    }
    guard.as_ref().unwrap().1.clone()
}

/// Which namespace a querying address belongs to. An address we cannot place is
/// scoped to the shared namespace — NEVER to the old unrestricted behaviour,
/// which is the leak this exists to close.
///
/// "Not in the index" is also what a container started INSIDE the TTL window
/// looks like, and the first thing a workload does is resolve; so an unplaceable
/// client buys one forced rebuild before we answer. Without it the isolation
/// would surface as start-up flakiness, and flaky isolation gets turned off.
fn dns_client_ns(client: Option<[u8; 4]>) -> String {
    let Some(ip) = client else {
        return NS_DEFAULT.to_string();
    };
    if let Some(ns) = dns_index().ns_of_ip.get(&ip) {
        return ns.clone();
    }
    if let Some(ns) = dns_index_within(DNS_INDEX_MIN_REBUILD).ns_of_ip.get(&ip) {
        return ns.clone();
    }
    NS_DEFAULT.to_string()
}

/// Resolves a name FOR A GIVEN CLIENT. `client` is the query's source address;
/// `None` means "caller could not tell", which is scoped like `default`.
///
/// BUG FIXED (ADR-0011): this used to take the name alone. The server had the
/// source address all along — `recv_from` returns it — and dropped it, so any
/// tenant could enumerate and address every workload of every other tenant by
/// name while the dataplane correctly refused the packets. Measured before the
/// fix: `client`@teamA resolved `webb`@teamB to its exact address, and the
/// resulting connection then hung, which is the worst of both outcomes.
fn dns_resolve_for(name: &str, client: Option<[u8; 4]>) -> Option<[u8; 4]> {
    let (cname, want_ns) = parse_internal_name(name)?;
    let idx = dns_index();
    let client_ns = dns_client_ns(client);
    let client_ip = client.unwrap_or([0, 0, 0, 0]);
    let visible = |e: &DnsEntry| dns_scope_allows(e, &client_ns, client_ip);

    if let Some(ns) = &want_ns {
        // Fully-qualified: the namespace is named, so it must BE the one asked
        // for — and the asker must be allowed to see it.
        return idx
            .by_key
            .get(&dns_index_ns_key(&cname, ns))
            .filter(|e| visible(e))
            .map(|e| e.ip);
    }
    // Bare name: the asker's OWN namespace first. This is what makes two tenants
    // able to both own `db` and each get their own.
    if let Some(e) = idx.by_key.get(&dns_index_ns_key(&cname, &client_ns)) {
        return Some(e.ip);
    }
    if let Some(e) = idx.by_key.get(&cname).filter(|e| visible(e)) {
        return Some(e.ip);
    }
    idx.by_key
        .get(&dns_index_vm_key(&cname))
        .filter(|e| visible(e))
        .map(|e| e.ip)
}

fn parse_v4(s: &str) -> Option<[u8; 4]> {
    let o: Vec<u8> = s.split('.').filter_map(|p| p.parse().ok()).collect();
    if o.len() == 4 {
        Some([o[0], o[1], o[2], o[3]])
    } else {
        None
    }
}

// ---- IPv6 SLAAC: Router Advertisements emitter (no radvd, which isn't there) ----

/// **Router Advertisements emitter** — runs in a holder thread; every ~8s
/// sends an RA (ICMPv6 type 134) to `ff02::1` on EACH ingress bridge, with the
/// network's ULA `/64` prefix (flags A+L → SLAAC). VMs and containers auto-configure
/// an IPv6 from the prefix. Replaces radvd (nonexistent/rootless-hostile).
fn ra_sender_main() {
    // SAFETY: creates a raw ICMPv6 socket (CAP_NET_RAW in the infra netns).
    let fd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_RAW, libc::IPPROTO_ICMPV6) };
    if fd < 0 {
        return;
    }
    let hops: libc::c_int = 255; // RA requires hop limit 255
                                 // SAFETY: setsockopt on a valid fd with an integer.
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IPV6,
            libc::IPV6_MULTICAST_HOPS,
            &hops as *const _ as *const libc::c_void,
            4,
        );
    }
    loop {
        for (br, prefix) in ra_bridges() {
            let cname = match std::ffi::CString::new(br.clone()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // SAFETY: if_nametoindex with a valid C name.
            let idx = unsafe { libc::if_nametoindex(cname.as_ptr()) };
            if idx == 0 {
                continue;
            }
            // SAFETY: sets the multicast output interface.
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_IPV6,
                    libc::IPV6_MULTICAST_IF,
                    &idx as *const _ as *const libc::c_void,
                    4,
                );
            }
            let pkt = build_ra(&prefix);
            // sockaddr_in6 for ff02::1 (all-nodes).
            // SAFETY: zeroes and fills a valid sockaddr_in6; sendto with correct sizes.
            unsafe {
                let mut dst: libc::sockaddr_in6 = std::mem::zeroed();
                dst.sin6_family = libc::AF_INET6 as u16;
                dst.sin6_addr.s6_addr =
                    std::net::Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1).octets();
                libc::sendto(
                    fd,
                    pkt.as_ptr() as *const libc::c_void,
                    pkt.len(),
                    0,
                    &dst as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as u32,
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(8));
    }
}

/// Ingress bridges + their ULA `/64` prefix (16 bytes, host zeroed), read from the
/// infra netns's address table.
fn ra_bridges() -> Vec<(String, [u8; 16])> {
    let mut out = Vec::new();
    let links = crate::capture("ip", &["-o", "link", "show", "type", "bridge"]).unwrap_or_default();
    for line in links.lines() {
        let name = line
            .split(':')
            .nth(1)
            .map(|s| s.trim().split('@').next().unwrap_or("").trim())
            .unwrap_or("");
        if name != INFRA_BRIDGE && !name.starts_with("dlxn") {
            continue;
        }
        let addrs =
            crate::capture("ip", &["-6", "-o", "addr", "show", "dev", name]).unwrap_or_default();
        for tok in addrs.split_whitespace() {
            if tok.starts_with("fd00:") {
                let ipstr = tok.split('/').next().unwrap_or("");
                if let Ok(v6) = ipstr.parse::<std::net::Ipv6Addr>() {
                    let mut b = v6.octets();
                    for x in b.iter_mut().skip(8) {
                        *x = 0; // only the /64
                    }
                    out.push((name.to_string(), b));
                    break;
                }
            }
        }
    }
    out
}

/// Builds a Router Advertisement (ICMPv6 134) with a Prefix Information option
/// (A+L → SLAAC on-link). The ICMPv6 checksum is filled in by the kernel (raw socket).
fn build_ra(prefix: &[u8; 16]) -> Vec<u8> {
    let mut p = vec![134u8, 0, 0, 0]; // type=RA, code=0, checksum=0 (kernel)
    p.push(64); // cur hop limit
    p.push(0); // flags M/O = 0 (SLAAC, no DHCPv6)
    p.extend_from_slice(&1800u16.to_be_bytes()); // router lifetime (default router)
    p.extend_from_slice(&0u32.to_be_bytes()); // reachable time
    p.extend_from_slice(&0u32.to_be_bytes()); // retrans timer
                                              // Prefix Information option (type 3, len 4×8=32 bytes)
    p.push(3);
    p.push(4);
    p.push(64); // prefix length
    p.push(0xc0); // flags: L (on-link) + A (autonomous/SLAAC)
    p.extend_from_slice(&86400u32.to_be_bytes()); // valid lifetime
    p.extend_from_slice(&14400u32.to_be_bytes()); // preferred lifetime
    p.extend_from_slice(&0u32.to_be_bytes()); // reserved
    p.extend_from_slice(prefix); // 16 bytes of the prefix
    p
}

/// Deterministic (static) IPv6 ULA of a container from its IPv4. For
/// display in the UI/CLI.
pub fn container_ip6(ip4: &str) -> Option<String> {
    v6_of(ip4)
}

/// IPv6 of a MAC via the infra netns's v6 `neigh` table (via nsenter, from the host).
/// To display a VM's (SLAAC) IPv6. `None` if it hasn't appeared yet.
pub fn dhcp_ip6_for_mac(_net: &str, mac: &str) -> Option<String> {
    let holder = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p))?;
    let mac = mac.to_lowercase();
    let out = crate::capture(
        "nsenter",
        &[
            "-t",
            &holder.to_string(),
            "-U",
            "-n",
            "--preserve-credentials",
            "ip",
            "-6",
            "neigh",
            "show",
        ],
    )
    .ok()?;
    for line in out.lines() {
        if line.to_lowercase().contains(&mac) {
            if let Some(ip) = line.split_whitespace().next() {
                if ip.starts_with("fd00") {
                    return Some(ip.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    /// Os verbos read-only são o que mantém o nó OBSERVÁVEL enquanto uma
    /// mutação está presa: podem ser servidos fora do worker serializado porque
    /// não tocam na fábrica de netns/veth/nft.
    ///
    /// Fail-closed de propósito: um verbo acrescentado amanhã é tratado como
    /// mutante até alguém decidir o contrário, em vez de correr em paralelo com
    /// a fábrica por omissão.
    #[test]
    fn so_os_verbos_de_leitura_saem_do_worker_serializado() {
        for ro in [
            "ping",
            "has-netns abc123",
            "fwstats 10.0.0.5",
            "egress-show delonix0",
        ] {
            assert!(super::is_readonly_verb(ro), "{ro:?} devia ser read-only");
        }
        // Tudo o que muda estado TEM de passar pelo worker.
        for mutating in [
            "attach ns 10.0.0.5 br gw",
            "attach-extra ns eth1 10.0.0.6 br gw",
            "detach ns",
            "publish 10.0.0.5 8080:80",
            "unpublish 8080",
            "firewall id 10.0.0.5 aabb",
            "cni-add ns id eth0 aabb",
            "cni-del ns id eth0 aabb",
        ] {
            assert!(
                !super::is_readonly_verb(mutating),
                "{mutating:?} NÃO pode escapar à serialização"
            );
        }
        // Um verbo desconhecido (futuro) é serializado por omissão.
        assert!(!super::is_readonly_verb("verbo-novo-qualquer x y"));
        assert!(!super::is_readonly_verb(""));
        // Um prefixo parecido não engana o matcher.
        assert!(!super::is_readonly_verb("pingx"));
        assert!(!super::is_readonly_verb("has-netns-evil"));
    }

    /// REGRESSION (availability): a control connection that never completes its
    /// command line must NOT block the holder forever.
    ///
    /// `control_loop` serves one connection at a time by design, so an unbounded
    /// `read_line` there takes down the control plane of every container on the
    /// node — no attach/detach/publish/firewall — with nothing to ever recover
    /// it. Reverting `read_control_line`'s `set_read_timeout` makes this test
    /// fail.
    ///
    /// The read runs on its own thread with a `recv_timeout` well above
    /// `CONTROL_IO_TIMEOUT`, so a regression FAILS CLEANLY (assert on a closed
    /// channel) instead of hanging `cargo test` — the whole point being that the
    /// unfixed code never returns at all.
    #[test]
    fn read_control_line_desiste_de_um_par_que_nunca_escreve() {
        use std::io::Write;
        use std::os::unix::net::{UnixListener, UnixStream};

        let sock = std::env::temp_dir().join(format!(
            "dlx-ctl-timeout-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        // A peer that connects and sends NOTHING — held open for the whole test,
        // which is exactly the wedge: the fd stays valid, so there is no EOF to
        // unblock the read.
        let silent = UnixStream::connect(&sock).unwrap();
        let (server_side, _) = listener.accept().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let got = super::read_control_line(&server_side);
            let _ = tx.send((got, started.elapsed()));
        });

        let grace = super::CONTROL_IO_TIMEOUT * 3;
        let (got, elapsed) = rx.recv_timeout(grace).expect(
            "read_control_line NUNCA devolveu — o holder fica preso para sempre \
             num par que não escreve (é este o bug)",
        );
        assert!(
            got.is_none(),
            "um par que nada enviou não pode produzir um comando: {got:?}"
        );
        assert!(
            elapsed < grace,
            "devolveu, mas só depois de {elapsed:?} — o prazo não está a ser aplicado"
        );

        // Não regride o caminho feliz: um comando completo continua a ser lido.
        let mut client = UnixStream::connect(&sock).unwrap();
        let (server_side2, _) = listener.accept().unwrap();
        client.write_all(b"ping\n").unwrap();
        assert_eq!(
            super::read_control_line(&server_side2).as_deref(),
            Some("ping\n"),
            "um comando completo tem de continuar a ser servido"
        );

        drop(silent);
        let _ = std::fs::remove_file(&sock);
    }

    /// REGRESSION: the traffic total must cover EVERY interface, not just
    /// `eth0`.
    ///
    /// A multi-homed container (`container update --net-connect`) carries its
    /// second network on `eth1`; the old implementation read a hardcoded
    /// `/sys/class/net/eth0/...` and silently reported only half the traffic —
    /// while still returning `Some`, so the collector counted it as fully
    /// measured. Header lines and `lo` must be excluded, and an unusable dump
    /// must be `None` (unmeasured), never `Some(0)`.
    ///
    /// The header/column layout below is verbatim from a real `/proc/net/dev`
    /// on this host — parsing a shape invented for the test would prove nothing.
    #[test]
    fn container_net_bytes_soma_todas_as_interfaces_nao_so_eth0() {
        let dump = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 487295143 1281691    0    0    0     0          0         0 487295143 1281691    0    0    0     0       0          0
  eth0: 13058207  101175    0    0    0     0          0         0  3728721   31476    0   31    0     0       0          0
  eth1:  5000000   40000    0    0    0     0          0         0  1000000   10000    0    0    0     0       0          0
";
        let (rx, tx) = super::parse_proc_net_dev(dump).expect("dump válido tem de medir");
        assert_eq!(
            rx,
            13_058_207 + 5_000_000,
            "o rx da 2.ª rede (eth1) tem de entrar na soma"
        );
        assert_eq!(tx, 3_728_721 + 1_000_000, "idem para o tx");

        // `lo` fica de fora: tráfego de loopback não é uso de rede, e neste
        // host mede 487 GB contra 13 GB da interface real — somá-lo tornaria
        // a métrica inútil.
        assert!(rx < 487_295_143, "o loopback não pode entrar na soma");

        // Uma leitura sem interfaces nenhumas é DESCONHECIDA, não "zero
        // bytes" — a mesma distinção que `network_unmeasured_containers` faz.
        assert_eq!(super::parse_proc_net_dev(""), None);
        assert_eq!(
            super::parse_proc_net_dev("Inter-|   Receive\n face |bytes\n"),
            None,
            "só cabeçalhos não é uma medição"
        );
        // Um container só com loopback também não tem tráfego de rede medível.
        assert_eq!(
            super::parse_proc_net_dev("    lo: 100 1 0 0 0 0 0 0 100 1 0 0 0 0 0 0\n"),
            None
        );
    }

    /// The IP computed by `dhcp_ip_for_mac` MUST match what
    /// `dhcp_serve` assigns — same formula. If one of the two changes without the other,
    /// VMs show an IP they don't respond to. Locks the shared formula.
    #[test]
    fn dhcp_ip_matches_server_formula() {
        let mac = "52:54:00:ab:cd:ef";
        // The server's formula (dhcp_serve): host = 10 + fnv32(mac)%240.
        let host = 10 + (crate::fnv32(mac) % 240) as u8;
        let expected = format!("10.200.254.{host}");
        // The default (delonix0/10.200) resolves without a holder — uses the fixed prefix.
        // (resolve_net("ingress") returns INFRA_PREFIX without touching disk.)
        let (_b, prefix, _g) = super::resolve_net("ingress").unwrap();
        let oct: Vec<u8> = prefix.split('.').filter_map(|x| x.parse().ok()).collect();
        let got = format!("{}.{}.254.{host}", oct[0], oct[1]);
        assert_eq!(got, expected);
        assert!((10..=249).contains(&host), "fora do pool .10-.249");
    }

    #[test]
    fn parse_a_records_extracts_ipv4_answers() {
        // DNS response for `example.com` with two A-records (name compression in the
        // answer via 0xc00c pointer), plus an AAAA that should be ignored.
        let resp: Vec<u8> = vec![
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00,
            0x00, // header: QD=1 AN=3
            7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00,
            0x01, // Q
            0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1e, 0x00, 0x04, 93, 184, 216,
            34, // A
            0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1e, 0x00, 0x04, 1, 2, 3,
            4, // A
            0xc0, 0x0c, 0x00, 0x1c, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1e, 0x00,
            0x10, // AAAA (16 bytes rdata)
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ];
        let ips = super::parse_a_records(&resp);
        assert_eq!(ips, vec![[93, 184, 216, 34], [1, 2, 3, 4]]);
    }

    #[test]
    fn hostfwd_entries_aceita_as_duas_formas() {
        // slirp4netns 1.2.1: {"entries":[…]} (no wrapper). Other versions may
        // wrap it in {"return":{"entries":[…]}}. Both have to work, otherwise the
        // remove never finds the id → stuck port.
        let a: serde_json::Value =
            serde_json::from_str(r#"{"entries":[{"id":1,"host_port":6443}]}"#).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(r#"{"return":{"entries":[{"id":2,"host_port":80}]}}"#).unwrap();
        assert_eq!(super::hostfwd_entries(&a).map(|e| e.len()), Some(1));
        assert_eq!(super::hostfwd_entries(&b).map(|e| e.len()), Some(1));
        let empty: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert!(super::hostfwd_entries(&empty).is_none());
    }

    use super::*;

    /// Unique temporary dir (without depending on the `tempfile` crate) — the test runs
    /// WITHOUT privilege: it only touches marker files, never namespaces.
    fn tmp_refs_dir(tag: &str) -> PathBuf {
        // SAFETY: getpid()/gettid() have no preconditions.
        let uniq = format!(
            "delonix-refs-{tag}-{}-{}",
            unsafe { libc::getpid() },
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let dir = std::env::temp_dir().join(uniq).join("refs");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        dir
    }

    /// STRESS test of the ref-count (set model): create→destroy of N resources at
    /// the level of the infra markers, without privilege. Asserts that the "refcount"
    /// (set cardinality) ALWAYS returns to 0 and that the deterministic reaper
    /// catches the orphans left by abrupt deaths, preserving the live ones.
    #[test]
    fn stress_refcount_volta_a_zero_e_reaper_apanha_orfaos() {
        use std::collections::HashSet;
        const N: usize = 500;
        let dir = tmp_refs_dir("stress");

        // 1) Balanced cycle: each id attaches and detaches — refcount returns to 0.
        for i in 0..N {
            ref_add_in(&dir, &format!("c{i}"));
        }
        assert_eq!(refs_in(&dir).len(), N, "N attaches → N marcadores");
        for i in 0..N {
            ref_remove_in(&dir, &format!("c{i}"));
        }
        assert_eq!(refs_in(&dir).len(), 0, "N detaches balanceados → 0");

        // 2) Idempotency: attaching/detaching double (stop+rm of the same id) doesn't
        //    misalign the counter nor tear down the infra too early.
        ref_add_in(&dir, "x");
        ref_add_in(&dir, "x");
        assert_eq!(refs_in(&dir).len(), 1, "atachar 2x o mesmo id conta 1");
        ref_remove_in(&dir, "x");
        ref_remove_in(&dir, "x"); // 2nd detach is a no-op
        assert_eq!(refs_in(&dir).len(), 0, "detach idempotente");

        // 3) Abrupt deaths: N attach and NONE detaches (the `pid` went to None without
        //    going through `stop`/`rm`). The reaper crosses with the live ones and frees only the
        //    orphans. `alive` and the CRI pod `cri-pod1` have to survive.
        for i in 0..N {
            ref_add_in(&dir, &format!("dead{i}"));
        }
        ref_add_in(&dir, "alive");
        ref_add_in(&dir, "cri-pod1");
        let live: HashSet<String> = ["alive".to_string(), "cri-pod1".to_string()]
            .into_iter()
            .collect();
        let orphans = orphan_refs(&refs_in(&dir), &live);
        assert_eq!(orphans.len(), N, "todos os `dead*` são órfãos");
        for id in &orphans {
            ref_remove_in(&dir, id);
        }
        let remaining: HashSet<String> = refs_in(&dir).into_iter().collect();
        assert_eq!(remaining.len(), 2, "só os vivos ficam");
        assert!(remaining.contains("alive"), "container vivo preservado");
        assert!(remaining.contains("cri-pod1"), "pod CRI vivo preservado");

        // 4) Round-trip of the id via the marker's hex (long ids/with `-` don't collide
        //    nor get truncated — the reaper needs the EXACT id to cross).
        let long = "cri-9f8e7d6c5b4a39281706abcdef0123456789";
        ref_add_in(&dir, long);
        assert!(
            refs_in(&dir).iter().any(|s| s == long),
            "id sobrevive round-trip"
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn marker_within_grace_poupa_marcadores_recentes() {
        // BUG regression guard: `system prune` racing `attach_container`'s
        // acquire-before-store-save window used to reap (and, if it was the
        // last marker, TEAR DOWN THE WHOLE HOLDER) a container that was
        // still mid-creation. A grace period on the marker's own mtime
        // closes it without needing to reorder container creation.
        let now = std::time::SystemTime::now();
        let grace = std::time::Duration::from_secs(15);
        // Just written (in-flight creation) → spared regardless of `live`.
        assert!(super::marker_within_grace(now, now, grace));
        assert!(super::marker_within_grace(
            now - std::time::Duration::from_secs(5),
            now,
            grace
        ));
        // Old enough → no longer spared, a genuine orphan gets reaped.
        assert!(!super::marker_within_grace(
            now - std::time::Duration::from_secs(30),
            now,
            grace
        ));
        // Clock skew (mtime "in the future" relative to `now`) fails closed —
        // never reap on an unreliable duration_since.
        assert!(super::marker_within_grace(
            now + std::time::Duration::from_secs(5),
            now,
            grace
        ));
    }

    #[test]
    fn ruleset_has_pre_and_post_chains() {
        let rs = ingress_table_ruleset();
        assert!(rs.contains(&format!("table ip {INGRESS_TABLE}")));
        assert!(rs.contains("chain pre"));
        assert!(rs.contains("hook prerouting"));
        assert!(rs.contains("chain post"));
        assert!(rs.contains("oifname \"tap0\" masquerade"));
    }

    #[test]
    fn vh_name_is_short_and_deterministic() {
        let a = vh_name("0123456789ab");
        assert_eq!(a, vh_name("0123456789ab")); // deterministic
        assert!(a.starts_with("vh"));
        assert!(a.len() <= 15, "IFNAMSIZ: {a}"); // 'vh' + 8 hex = 10
        assert_ne!(a, vh_name("ffffffffffff")); // different ids → different names
    }

    #[test]
    fn egress_specs_compoem_cidrs_e_fqdn() {
        use super::EgressState;
        let st = |policy: Option<&str>, hosts: &[&str]| EgressState {
            policy: policy.map(String::from),
            hosts: hosts.iter().map(|s| s.to_string()).collect(),
        };
        // allow, no hosts → no rules (default-allow).
        assert!(super::egress_specs("dlx1", &st(None, &[])).is_empty());
        // deny, no hosts → a single drop.
        let d = super::egress_specs("dlx1", &st(Some("deny"), &[]));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].last().unwrap(), "drop");
        // allowlist + host COMPOSE: 2xDNS + 1 valid CIDR + @set + drop (bad CIDR skipped).
        let a = super::egress_specs(
            "dlx1",
            &st(Some("allowlist:1.1.1.0/24,lixo;rm"), &["github.com"]),
        );
        assert_eq!(a.len(), 5, "2xDNS + 1 CIDR + @set FQDN + drop");
        assert!(a[2].contains(&"1.1.1.0/24".to_string()));
        assert!(
            a[3].iter().any(|x| x.starts_with("@dlxfq")),
            "a regra @set do FQDN está presente"
        );
        assert_eq!(a[4].last().unwrap(), "drop");
        // host only (no CIDR policy) → 2xDNS + @set + drop.
        let h = super::egress_specs("dlx1", &st(None, &["example.com"]));
        assert_eq!(h.len(), 4);
        assert!(h[2].iter().any(|x| x.starts_with("@dlxfq")));
    }

    #[test]
    fn is_global_egress_drop_line_nao_apanha_regras_por_rede() {
        // BUG regression guard: a global `egress deny`/`allow` used to also
        // match and delete PER-NETWORK egress rules (real `nft -a list
        // chain` line shapes below), silently reopening a network's egress
        // that had been explicitly denied/allowlisted.
        assert!(super::is_global_egress_drop_line(
            "\t\toifname \"tap0\" drop # handle 12"
        ));
        assert!(!super::is_global_egress_drop_line(
            "\t\tiifname \"dlxn1a2b\" oifname \"tap0\" drop # handle 7"
        ));
        assert!(!super::is_global_egress_drop_line(
            "\t\tiifname \"dlxn1a2b\" oifname \"tap0\" ip daddr @dlxfq1a2b accept # handle 5"
        ));
        // A line with neither substring, or only one, is never a match either way.
        assert!(!super::is_global_egress_drop_line(
            "\t\tiifname \"dlxn1a2b\" oifname \"tap0\" udp dport 53 accept # handle 3"
        ));
        assert!(!super::is_global_egress_drop_line(
            "\t\tcounter packets 0 bytes 0"
        ));
    }

    #[test]
    fn sanitize_strips_unsafe_and_caps_length() {
        assert_eq!(sanitize("abc; rm -rf /"), "abcrm-rf"); // no spaces/`;`/`/`
        assert_eq!(sanitize("0123456789abcdef").len(), 12); // <= 12
        assert_eq!(sanitize("web_1-x"), "web_1-x"); // alnum/_/- preserved
    }

    #[test]
    fn hex_roundtrip() {
        let data = br#"{"enabled":true}"#;
        assert_eq!(hex_decode(&hex_encode(data)).unwrap(), data);
        assert!(hex_decode("abc").is_none()); // odd
        assert!(hex_decode("zz").is_none()); // non-hex
    }

    #[test]
    fn fw_body_translates_rules_and_policy() {
        let fw = delonix_runtime_core::ContainerFw {
            enabled: true,
            policy_in: "deny".into(),
            policy_out: "allow".into(),
            rules: vec![
                delonix_runtime_core::FwRule {
                    dir: "in".into(),
                    proto: "tcp".into(),
                    port: "8080".into(),
                    src: "10.200.0.0/16".into(),
                    action: "allow".into(),
                    note: String::new(),
                },
                delonix_runtime_core::FwRule {
                    dir: "out".into(),
                    proto: "any".into(),
                    port: String::new(),
                    src: String::new(),
                    action: "deny".into(),
                    note: String::new(),
                },
            ],
            namespace: "default".into(),
        };
        let body = fw_chain_body("10.200.0.5", &fw);
        // in rule: daddr==ip, peer saddr==src, tcp dport 8080 accept
        assert!(
            body.contains(
                "ip daddr 10.200.0.5 ip saddr 10.200.0.0/16 tcp dport 8080 counter accept"
            ),
            "{body}"
        );
        // out rule: saddr==ip, drop (proto any → no proto/dport)
        assert!(body.contains("ip saddr 10.200.0.5 counter drop"), "{body}");
        // policy in=deny → final drop on the daddr
        assert!(body.contains("ip daddr 10.200.0.5 counter drop"), "{body}");
        // EXPLICIT inbound policy (deny) → does NOT emit namespace rules.
        assert!(!body.contains("@dlxall"), "{body}");
        // disabled → empty body
        let off = delonix_runtime_core::ContainerFw {
            enabled: false,
            ..fw
        };
        assert!(fw_chain_body("10.200.0.5", &off).is_empty());
    }

    /// Regression: a rule with `proto: any` AND a port used to drop the port from the
    /// emitted nft line, widening the rule to the WHOLE container — `allow <c> 9999`
    /// opened every port under `policy deny`, `deny <c> 9999` closed every port.
    /// Reproduced live against a real published port before the fix.
    #[test]
    fn fw_body_keeps_the_port_when_proto_is_any() {
        let rule = |port: &str, action: &str| delonix_runtime_core::FwRule {
            dir: "in".into(),
            proto: "any".into(),
            port: port.into(),
            src: String::new(),
            action: action.into(),
            note: String::new(),
        };
        let fw = delonix_runtime_core::ContainerFw {
            enabled: true,
            policy_in: "deny".into(),
            policy_out: "allow".into(),
            rules: vec![rule("9999", "allow"), rule("100-200", "deny")],
            namespace: "default".into(),
        };
        let body = fw_chain_body("10.200.0.5", &fw);
        assert!(
            body.contains(
                "ip daddr 10.200.0.5 meta l4proto { tcp, udp } th dport 9999 counter accept"
            ),
            "{body}"
        );
        // ranges survive too (`fw_port_ok` already validates `n-m`).
        assert!(
            body.contains(
                "ip daddr 10.200.0.5 meta l4proto { tcp, udp } th dport 100-200 counter drop"
            ),
            "{body}"
        );
        // the whole-container form must NOT appear for a rule that named a port.
        assert!(
            !body.contains("ip daddr 10.200.0.5 counter accept"),
            "{body}"
        );
        // `proto: any` with NO port stays the whole-container rule it always was.
        let wide = delonix_runtime_core::ContainerFw {
            enabled: true,
            policy_in: "deny".into(),
            policy_out: "allow".into(),
            rules: vec![rule("", "allow")],
            namespace: "default".into(),
        };
        assert!(
            fw_chain_body("10.200.0.5", &wide).contains("ip daddr 10.200.0.5 counter accept"),
            "a portless `any` rule is still container-wide"
        );
    }

    #[test]
    fn fw_body_emits_namespace_isolation_when_no_explicit_ingress() {
        // enabled, no inbound rules and policy_in != deny → namespace isolation.
        let fw = delonix_runtime_core::ContainerFw {
            enabled: true,
            namespace: "web".into(),
            ..Default::default()
        };
        let body = fw_chain_body("10.200.0.7", &fw);
        let nsset = dlxns_set("web");
        // same-ns accept + cross-ns (container) NEW drop, com ct state new.
        assert!(
            body.contains(&format!(
                "ip daddr 10.200.0.7 ip saddr @{nsset} counter accept"
            )),
            "{body}"
        );
        assert!(
            body.contains("ip daddr 10.200.0.7 ip saddr @dlxall ct state new counter drop"),
            "{body}"
        );
    }

    /// REGRESSION (reproduced live, both directions): the default policy used to be a
    /// bare `drop` in a chain hooked BEFORE the `forward` chain's own
    /// `ct state established,related accept`, so `ingress policy deny` killed the
    /// container's outbound traffic on the reply (DNS included) and `egress policy
    /// deny` dropped the SYN-ACK of an inbound connection, making a published service
    /// unreachable. The conntrack fast-path is what makes a default-deny posture
    /// expressible at all; without it the whole subsystem is decorative.
    #[test]
    fn prologo_deixa_passar_o_trafego_ja_estabelecido() {
        let fw = delonix_runtime_core::ContainerFw {
            enabled: true,
            policy_in: "deny".into(),
            policy_out: "deny".into(),
            ..Default::default()
        };
        let head = fw_chain_prologue(&fw);
        assert!(
            head.contains("ct state established,related counter accept"),
            "{head}"
        );
        assert!(head.contains("ct state invalid counter drop"), "{head}");
        // The fast-path has to come BEFORE the policy drops it exists to survive.
        let full = format!("{head}{}", fw_chain_body("10.200.0.5", &fw));
        let accept = full.find("established,related").expect("prologue present");
        let deny = full
            .find("ip daddr 10.200.0.5 counter drop")
            .expect("policy present");
        assert!(
            accept < deny,
            "the conntrack accept must precede the policy drop:\n{full}"
        );
        // A firewall that is off stays a completely empty chain.
        let off = delonix_runtime_core::ContainerFw {
            enabled: false,
            ..fw
        };
        assert_eq!(fw_chain_prologue(&off), "");
    }

    /// The tail is the key a rule's counters are looked up by, so generator and reader
    /// must agree byte for byte. Reading a real `nft list chain` line back through
    /// `parse_fw_counters` has to land exactly on what `fw_rule_tail` produced —
    /// otherwise `ingress ls` silently shows `-` on every rule.
    #[test]
    fn counters_voltam_a_casar_com_a_regra_que_os_gerou() {
        let r = delonix_runtime_core::FwRule {
            dir: "in".into(),
            proto: "tcp".into(),
            port: "5432".into(),
            src: "10.200.0.0/16".into(),
            action: "allow".into(),
            note: String::new(),
        };
        let tail = fw_rule_tail(&r).expect("safe rule");
        // A single-host source must NOT carry `/32`: the kernel prints it as a bare
        // address, and the listed text is what the counter lookup matches on. Caught
        // live — an `--from <ip>/32` rule with real traffic showed `-` in `ingress ls`.
        let host = delonix_runtime_core::FwRule {
            src: "172.16.31.103/32".into(),
            ..r.clone()
        };
        let host_tail = fw_rule_tail(&host).expect("safe rule");
        assert!(host_tail.contains("ip saddr 172.16.31.103 "), "{host_tail}");
        assert!(!host_tail.contains("/32"), "{host_tail}");
        // A real prefix is left exactly as it is.
        let net = delonix_runtime_core::FwRule {
            src: "10.200.0.0/16".into(),
            ..r.clone()
        };
        assert!(fw_rule_tail(&net).unwrap().contains("10.200.0.0/16"));
        // Exactly how the kernel renders that rule once it has seen traffic.
        let listing = format!(
            "table ip dlxing {{\n\tchain fwdeadbeef {{\n\
             \t\tct state established,related counter packets 9 bytes 900 accept\n\
             \t\tip daddr 10.201.0.5 {} accept\n\
             \t\tip daddr 10.202.0.9 {} accept\n\t}}\n}}\n",
            tail.replace("counter accept", "counter packets 4 bytes 400"),
            tail.replace("counter accept", "counter packets 6 bytes 620"),
        );
        let parsed = parse_fw_counters(&listing);
        let summed: (u64, u64) = parsed
            .iter()
            .filter(|(text, _, _)| strip_fw_anchor(text) == tail)
            .fold((0, 0), |acc, (_, p, b)| (acc.0 + p, acc.1 + b));
        // Both addresses of a multi-homed container add up into the one rule.
        assert_eq!(summed, (10, 1020), "parsed: {parsed:?}\ntail: {tail}");
    }

    /// The verdict map is what a container's teardown and re-apply are keyed on, so a
    /// listing that spans lines (the shape nft actually prints) must not lose entries.
    #[test]
    fn parse_fwmap_le_todas_as_entradas_multilinha() {
        let listing = "table ip dlxing {\n\tmap fwmap {\n\t\ttype ipv4_addr : verdict\n\
                       \t\telements = { 10.201.0.5 : jump fwaaaaaaaa,\n\
                       \t\t             10.202.0.9 : jump fwaaaaaaaa,\n\
                       \t\t             10.203.0.7 : jump fwbbbbbbbb }\n\t}\n}\n";
        let els = parse_fwmap_elements(listing);
        assert_eq!(els.len(), 3, "{els:?}");
        // The LAST entry has no trailing comma — the one a naive split drops.
        assert!(
            els.contains(&("10.203.0.7".into(), "fwbbbbbbbb".into())),
            "{els:?}"
        );
        let mine: Vec<_> = els.iter().filter(|(_, c)| c == "fwaaaaaaaa").collect();
        assert_eq!(mine.len(), 2, "both addresses of the multi-homed container");
        assert!(parse_fwmap_elements("map fwmap { type ipv4_addr : verdict }").is_empty());
    }

    /// RF-NET-11 — the IPv6 bypass, reproduced live before this fix: with the firewall
    /// dropping on IPv4, the same container answered on port 80 over its ULA, because
    /// every rule the engine writes lives in `table ip`. Both layers of the refusal are
    /// asserted here; the live reproduction is in the release notes.
    #[test]
    fn ipv6_e_recusado_nas_duas_camadas() {
        // Layer 2 — forwarding of v6 dies in the holder, in its own table.
        let v6 = ingress_v6_refusal_ruleset();
        assert!(v6.contains("table ip6"), "{v6}");
        assert!(v6.contains("hook forward"), "{v6}");
        assert!(v6.contains("policy drop"), "{v6}");
        // Layer 1 — no v6 addresses at all inside the container's netns. `all` alone
        // is not enough (the per-interface knob wins for an interface that already
        // exists) and `default` is what covers an interface created later.
        let argv = disable_ipv6_argv("dlx-abc");
        let joined: Vec<String> = argv.iter().map(|a| a.join(" ")).collect();
        for key in ["all", "default", "eth0"] {
            assert!(
                joined
                    .iter()
                    .any(|c| c.contains(&format!("net.ipv6.conf.{key}.disable_ipv6=1"))),
                "missing the `{key}` knob: {joined:?}"
            );
        }
        assert!(joined.iter().all(|c| c.starts_with("netns exec dlx-abc ")));
    }

    /// The holder's own services are not reachable from a container.
    ///
    /// Regression for the measured bypass of §4.2: every policy chain hangs off `forward`,
    /// and traffic addressed to the holder goes through `input` — so with no `input` chain
    /// a container reached the L7 proxy on its bridge gateway and was relayed to any
    /// backend, across namespaces and past the backend's `ingress policy deny`.
    ///
    /// Asserts the ORDER, not just the presence: the terminal drop has to be LAST. An
    /// allowlist entry emitted after it would be dead rule, and the failure mode of that
    /// mistake is silent (DNS stops working for every container on the node).
    #[test]
    fn os_servicos_do_holder_nao_sao_alcancaveis_de_um_container() {
        let rs = ingress_table_ruleset();
        // Extracted LINE BY LINE, not by splitting on braces: a `split_once("}")` cuts at
        // the closing brace of the `{ 53, 67, 68 }` set, not at the end of the chain — the
        // first version of this test did exactly that and failed on a rule that was there
        // all along. The chain ends at the first line whose whole content is `}`.
        let lines: Vec<&str> = rs.lines().collect();
        let start = lines
            .iter()
            .position(|l| l.contains("chain dlxinput"))
            .expect("the input chain has to exist");
        let end = start
            + 1
            + lines[start + 1..]
                .iter()
                .position(|l| l.trim() == "}")
                .expect("the input chain has to close");
        let body_lines: Vec<&str> = lines[start..end]
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let body = body_lines.join("\n");
        let body = body.as_str();
        assert!(body.contains("hook input"), "{body}");
        // What a container legitimately needs FROM the holder — and the host→slirp path.
        for allowed in [
            "ct state established,related accept",
            "iifname \"lo\" accept",
            "iifname \"tap0\" accept",
            "udp dport { 53, 67, 68 } accept",
            "tcp dport 53 accept",
            "meta l4proto icmp accept",
        ] {
            assert!(body.contains(allowed), "missing `{allowed}`: {body}");
        }
        // Everything else that is NEW gets dropped, and that rule is the last one.
        let drop = body
            .find("ct state new counter drop")
            .expect("the terminal drop has to exist");
        assert!(
            body_lines
                .last()
                .expect("a non-empty chain body")
                .contains("ct state new counter drop"),
            "the drop must be the LAST rule, or the allowlist after it is dead: {body}"
        );
        for allowed in ["dport 53", "iifname \"tap0\""] {
            assert!(
                body.find(allowed).expect("allowlist entry") < drop,
                "`{allowed}` must be evaluated BEFORE the drop: {body}"
            );
        }
    }

    /// RF-NET-02 — the denials that no user rule can get in front of. The PRIORITY is
    /// the requirement, not just the presence of the rules: `fwguard` has to be
    /// evaluated before `fwdeny` (-10), before the per-container dispatch (-5) and
    /// before the default policy (0). A rule in a later chain cannot pre-empt it.
    #[test]
    fn destinos_sensiveis_sao_negados_antes_de_qualquer_regra() {
        let rs = ingress_table_ruleset();
        assert!(rs.contains("chain fwguard"), "{rs}");
        assert!(rs.contains("priority -20"), "{rs}");
        assert!(rs.contains("ip daddr 169.254.0.0/16 counter drop"), "{rs}");
        assert!(rs.contains("ip daddr 127.0.0.0/8 counter drop"), "{rs}");
        // The ordering claim, asserted rather than assumed — and asserted on the
        // VALUES, not on where the chains happen to appear in the text (the ruleset
        // declares `fwcont` before `fwdeny`, so textual order says nothing).
        // Only the FORWARD hook: priority orders chains within one hook, so comparing
        // against the nat chains (-100 prerouting, 100 postrouting) would be
        // meaningless — that is what the first version of this test got wrong.
        let priorities: Vec<i32> = rs
            .match_indices("hook forward priority ")
            .filter_map(|(i, m)| {
                rs[i + m.len()..]
                    .split(|c: char| c == ';' || c.is_whitespace())
                    .find(|t| !t.is_empty())?
                    .parse()
                    .ok()
            })
            .collect();
        assert!(
            priorities.len() >= 4,
            "expected every forward chain: {priorities:?}"
        );
        let guard = rs[rs.find("chain fwguard").unwrap()..]
            .split_once("priority ")
            .and_then(|(_, r)| r.split(';').next()?.trim().parse::<i32>().ok())
            .expect("fwguard declares a priority");
        assert!(
            priorities.iter().all(|p| guard <= *p),
            "fwguard ({guard}) must run before every other hook: {priorities:?}"
        );
        assert!(priorities.iter().filter(|p| **p == guard).count() == 1);
    }

    #[test]
    fn fw_chain_name_is_deterministic() {
        assert_eq!(fw_chain_name("10.200.0.5"), fw_chain_name("10.200.0.5"));
        assert!(fw_chain_name("10.200.0.5").starts_with("fw"));
        assert_ne!(fw_chain_name("10.200.0.5"), fw_chain_name("10.200.0.6"));
    }

    #[test]
    fn ruleset_has_forward_filter_chain() {
        assert!(ingress_table_ruleset().contains("chain forward"));
        assert!(ingress_table_ruleset().contains("hook forward"));
    }

    #[test]
    fn validate_publish_guards_inputs() {
        assert!(validate_publish("tcp", "8080", "10.200.0.5", "80").is_ok());
        assert!(validate_publish("udp", "53", "10.200.1.9", "53").is_ok());
        assert!(validate_publish("sctp", "80", "10.200.0.5", "80").is_err()); // proto
        assert!(validate_publish("tcp", "0", "10.200.0.5", "80").is_err()); // port 0
        assert!(validate_publish("tcp", "8080", "10.99.0.5", "80").is_err()); // IP outside the subnet
        assert!(!is_port("70000") && !is_port("abc") && is_port("443"));
    }

    #[test]
    fn container_ip_in_infra_subnet() {
        let ip = container_ip("0a0b0c0d1122");
        assert!(ip.starts_with(&format!("{INFRA_PREFIX}.")), "{ip}");
        assert!(crate::valid_ip_in_subnet(INFRA_PREFIX, &ip), "{ip}");
        assert_eq!(ip, container_ip("0a0b0c0d1122")); // deterministic
    }

    #[test]
    fn valid_fdb_dst_accepts_only_ips() {
        // textual IPv4/IPv6 — accepted.
        assert!(valid_fdb_dst("10.0.0.1"));
        assert!(valid_fdb_dst("192.168.1.254"));
        assert!(valid_fdb_dst("fd00::1"));
        assert!(valid_fdb_dst("2001:db8::a2f"));
        // Injection / garbage — refused (the dst goes to argv of `bridge fdb`, but we keep
        // the audit's valid_* discipline: nothing with a space/`;`/`|`/`$` passes).
        assert!(!valid_fdb_dst(""));
        assert!(!valid_fdb_dst("10.0.0.1; rm -rf /"));
        assert!(!valid_fdb_dst("$(curl evil)"));
        assert!(!valid_fdb_dst("10.0.0.1 dev eth0"));
        assert!(!valid_fdb_dst(&"a".repeat(46))); // above the textual IPv6 cap
    }

    #[test]
    fn parse_internal_name_handles_all_schemes() {
        // simple <name> → no namespace (any)
        assert_eq!(parse_internal_name("web"), Some(("web".into(), None)));
        // legacy .delonix.io → WHOLE name, no namespace
        assert_eq!(
            parse_internal_name("web.delonix.io"),
            Some(("web".into(), None))
        );
        // internal FQDN with namespace → verifies
        assert_eq!(
            parse_internal_name("web.data.delonix.internal"),
            Some(("web".into(), Some("data".into())))
        );
        // trailing dot + uppercase normalized
        assert_eq!(
            parse_internal_name("API.PROD.delonix.internal."),
            Some(("api".into(), Some("prod".into())))
        );
        // ANTI-HIJACK: an external domain with a dot is NOT split into namespace
        // (stays as a whole name; matches no container 'foo.com' → forwards).
        assert_eq!(
            parse_internal_name("foo.com"),
            Some(("foo.com".into(), None))
        );
        assert_eq!(
            parse_internal_name("api.github.com"),
            Some(("api.github.com".into(), None))
        );
        // only the suffix → None
        assert_eq!(parse_internal_name(".delonix.internal"), None);
        assert_eq!(parse_internal_name(""), None);
    }

    const IP: [u8; 4] = [10, 250, 0, 9];

    #[test]
    fn a_known_name_answers_nodata_not_servfail_for_aaaa() {
        // THE regression this file exists to prevent. `getaddrinfo()` asks for A
        // and AAAA together; when the AAAA half was forwarded, the upstream said
        // SERVFAIL for a bare container name and musl/glibc failed the WHOLE
        // lookup — `wget http://weba:8080/` died with `bad address` while the A
        // record resolved perfectly. NoData is what keeps the A half usable.
        assert_eq!(
            dns_action(QTYPE_AAAA, "weba", Some(IP)),
            DnsAction::NoData,
            "AAAA for a name we know must be answered locally, never forwarded"
        );
        assert_eq!(dns_action(QTYPE_A, "weba", Some(IP)), DnsAction::Answer(IP));
        // Same for a fully-qualified internal name, and for any other type.
        assert_eq!(
            dns_action(QTYPE_AAAA, "weba.teamA.delonix.internal", Some(IP)),
            DnsAction::NoData
        );
        assert_eq!(dns_action(15, "weba", Some(IP)), DnsAction::NoData); // MX
    }

    #[test]
    fn our_zone_never_leaves_the_node() {
        // An unknown name under our own zone is OURS to refuse. Forwarding it
        // leaked every workload and namespace name to an external resolver and
        // paid its latency (measured: 9.03s per query with the upstream down)
        // to be told what we already knew.
        for name in [
            "naoexiste.teamA.delonix.internal",
            "weba.teamB.delonix.internal", // right name, WRONG namespace
            "delonix.internal",
            "WEBA.TEAMB.DELONIX.INTERNAL.", // case + trailing dot
        ] {
            assert_eq!(
                dns_action(QTYPE_A, name, None),
                DnsAction::NxDomain,
                "{name} is in our zone: must be an authoritative NXDOMAIN"
            );
            assert_eq!(dns_action(QTYPE_AAAA, name, None), DnsAction::NxDomain);
        }
    }

    #[test]
    fn external_names_are_still_forwarded() {
        // The other half of the contract: claiming authority too widely would
        // blackhole the internet for every container. `.delonix.io` is a REAL
        // public domain and is deliberately NOT ours.
        for name in [
            "google.com",
            "api.github.com",
            "delonix.io",
            "web.delonix.io",
        ] {
            assert_eq!(dns_action(QTYPE_A, name, None), DnsAction::Forward);
            assert_eq!(dns_action(QTYPE_AAAA, name, None), DnsAction::Forward);
        }
        assert!(!is_internal_zone("notdelonix.internal.example.com"));
        // ...but a legacy `.delonix.io` name we DO know still answers locally.
        assert_eq!(
            dns_action(QTYPE_A, "web.delonix.io", Some(IP)),
            DnsAction::Answer(IP)
        );
    }

    fn entry(ns: &str, allow: &[&str]) -> DnsEntry {
        DnsEntry {
            ip: IP,
            ns: ns.into(),
            allow_in: allow.iter().filter_map(|c| scoping_allow_cidr(c)).collect(),
        }
    }

    #[test]
    fn a_tenant_cannot_resolve_another_tenants_workload() {
        // THE leak (measured before the fix): `client`@teamA resolved
        // `webb`@teamB to its exact address while the dataplane correctly
        // dropped the packets — a name that resolves and a connection that
        // hangs, which is the worst of both outcomes.
        let webb = entry("teamB", &[]);
        assert!(!dns_scope_allows(&webb, "teamA", [10, 250, 0, 5]));
        assert!(dns_scope_allows(&webb, "teamB", [10, 250, 0, 5]));
        // Case must not be a way around it.
        assert!(dns_scope_allows(&webb, "TEAMB", [10, 250, 0, 5]));
    }

    #[test]
    fn the_shared_namespace_stays_reachable_from_everywhere() {
        // ADR-0011 §5: `default` is the shared space you get by not naming one.
        // It is reachable from any namespace on the dataplane, so the resolver
        // says so too — and that is NOT the leak, because teamA still cannot
        // see teamB (asserted above).
        let shared = entry("default", &[]);
        for asker in ["default", "teamA", "teamB"] {
            assert!(dns_scope_allows(&shared, asker, [10, 250, 0, 5]));
        }
    }

    #[test]
    fn an_explicit_dependency_makes_the_name_resolvable_across_namespaces() {
        // A `kind: Dependency` opens the boundary in one direction. Without
        // this, it would work by address and not by name — the "accepted and
        // then ignored" shape this repo keeps removing.
        let db = entry("teamB", &["10.250.0.5/32"]);
        assert!(dns_scope_allows(&db, "teamA", [10, 250, 0, 5]));
        // ...and ONLY for the address the dependency actually named.
        assert!(!dns_scope_allows(&db, "teamA", [10, 250, 0, 6]));
        // A subnet-wide allow works the same way.
        let wide = entry("teamB", &["10.250.0.0/24"]);
        assert!(dns_scope_allows(&wide, "teamA", [10, 250, 0, 200]));
        assert!(!dns_scope_allows(&wide, "teamA", [10, 250, 1, 1]));
    }

    #[test]
    fn an_allow_from_anywhere_does_not_republish_the_name_to_every_tenant() {
        // `0.0.0.0/0` is a port-level decision ("this port is open to the
        // world"), not "publish this name to all tenants". Letting it through
        // here would quietly restore the global visibility the ADR removes, via
        // the single most common firewall rule there is.
        let e = entry("teamB", &["0.0.0.0/0", "*", ""]);
        assert!(
            e.allow_in.is_empty(),
            "world-wide allows must not be indexed"
        );
        assert!(!dns_scope_allows(&e, "teamA", [10, 250, 0, 5]));
    }

    #[test]
    fn parse_cidr_handles_the_forms_the_firewall_actually_stores() {
        assert_eq!(
            parse_cidr("10.0.0.1"),
            Some((u32::from_be_bytes([10, 0, 0, 1]), u32::MAX))
        );
        assert_eq!(parse_cidr("10.0.0.0/8").map(|c| c.1), Some(0xff00_0000));
        assert_eq!(parse_cidr("0.0.0.0/0"), Some((0, 0)));
        assert_eq!(parse_cidr("*"), Some((0, 0)));
        assert_eq!(parse_cidr("10.0.0.0/33"), None);
        assert_eq!(parse_cidr("nonsense"), None);
        // /24 boundary, both sides
        let c = parse_cidr("192.168.1.0/24").unwrap();
        assert!(cidr_contains(c, [192, 168, 1, 255]));
        assert!(!cidr_contains(c, [192, 168, 2, 0]));
    }

    #[test]
    fn negative_reply_is_a_wellformed_answerless_response() {
        // question: "weba" A IN, RD set
        let mut q = vec![0xab, 0xcd, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        q.extend_from_slice(&[4, b'w', b'e', b'b', b'a', 0]);
        q.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);
        let qend = q.len();
        let r = negative_reply(&q, qend, RCODE_NXDOMAIN);
        assert_eq!(&r[0..2], &[0xab, 0xcd], "the ID must be echoed back");
        assert_eq!(r[2] & 0x80, 0x80, "QR");
        assert_eq!(r[2] & 0x04, 0x04, "AA: we are authoritative for this");
        assert_eq!(r[2] & 0x01, 0x01, "RD copied from the query, not assumed");
        assert_eq!(r[3] & 0x0f, RCODE_NXDOMAIN);
        assert_eq!(&r[4..6], &[0, 1], "QDCOUNT=1");
        assert_eq!(&r[6..8], &[0, 0], "ANCOUNT=0");
        assert_eq!(&r[12..], &q[12..qend], "the question is echoed verbatim");
        // NODATA differs from NXDOMAIN ONLY in the rcode — that one nibble is
        // what tells a resolver "carry on" instead of "the lookup failed".
        let nd = negative_reply(&q, qend, RCODE_NOERROR);
        assert_eq!(nd[3] & 0x0f, 0);
        assert_eq!(nd[..3], r[..3]);
    }

    #[test]
    fn fdb_presence_is_exact_token_not_substring() {
        // The real output of `bridge fdb show`: each destination is an isolated token.
        let have = "00:00:00:00:00:00 dst 10.0.0.50 self permanent\n\
                    1a:2b:3c:4d:5e:6f master br0 permanent";
        let present = |dst: &str| have.lines().any(|l| l.split_whitespace().any(|t| t == dst));
        assert!(present("10.0.0.50")); // actually present
        assert!(!present("10.0.0.5")); // NOT present — even though it's a substring of 10.0.0.50
    }

    #[test]
    fn set_vxlan_empty_peers_uses_sentinel_token() {
        // With no peers, the CSV would collapse to nothing and the control-loop (split_whitespace)
        // would see 5 tokens instead of 6 — the `-` sentinel keeps the arity. (Doesn't touch
        // the holder: we only validate the command's shape, building it by hand like the wrapper.)
        let dsts: Vec<String> = Vec::new();
        let csv = if dsts.is_empty() {
            "-".to_string()
        } else {
            dsts.join(",")
        };
        assert_eq!(csv, "-");
        let cmd = format!("vxlan dlxvx0042 66 dlxn0000002a 10.201.0.1 {csv}");
        assert_eq!(cmd.split_whitespace().count(), 6);
        // With peers, a single CSV token (no spaces) preserves the arity.
        let csv2 = ["10.0.0.2".to_string(), "10.0.0.3".to_string()].join(",");
        let cmd2 = format!("vxlan dlxvx0042 66 dlxn0000002a 10.201.0.1 {csv2}");
        assert_eq!(cmd2.split_whitespace().count(), 6);
    }

    #[test]
    fn base_root_e_runtime_dir_honram_env_vars_explicitas() {
        // with DELONIX_ROOT set, ingress_dir is deterministic and does NOT depend
        // on the uid (essential for the holder with uid mapped to 0).
        std::env::set_var("DELONIX_ROOT", "/tmp/dlx-test-root");
        assert_eq!(ingress_dir(), PathBuf::from("/tmp/dlx-test-root/ingress"));

        // BUG FOUND live (a genuinely long DELONIX_ROOT, e.g. a deep test/tmp
        // path): `slirp_sock_path`/`control_sock_path` used to nest directly
        // under `ingress_dir()` (== DELONIX_ROOT-derived), and Linux's
        // AF_UNIX `sun_path` is capped at 108 bytes — `bind()` failed with
        // "path must be shorter than SUN_LEN" even though DELONIX_ROOT itself
        // (a regular directory, PATH_MAX-limited) was completely valid.
        // `runtime_dir()` MUST stay independent of DELONIX_ROOT's length,
        // proven here with a DELONIX_ROOT deliberately deep enough that a
        // socket nested under it would exceed SUN_LEN.
        std::env::set_var(
            "DELONIX_ROOT",
            "/tmp/a/very/deeply/nested/delonix/root/that/would/exceed/SUN_LEN/if/a/socket/lived/under/it/like/before",
        );
        std::env::set_var(RUNTIME_DIR_ENV, "/tmp/dlx-rt-test");
        assert_eq!(runtime_dir(), PathBuf::from("/tmp/dlx-rt-test"));
        assert_eq!(
            slirp_sock_path(),
            PathBuf::from("/tmp/dlx-rt-test/slirp.sock")
        );
        assert_eq!(
            control_sock_path(),
            PathBuf::from("/tmp/dlx-rt-test/control.sock")
        );
        std::env::remove_var(RUNTIME_DIR_ENV);

        // Rootless fallback (no explicit override, no root): `/tmp`, uid-scoped
        // — NOT `/run`-based (see `runtime_dir`'s doc comment for why: the
        // holder remounts `/run` as an empty tmpfs for its own `/run/netns`,
        // which would hide anything the parent created there first).
        assert_eq!(
            runtime_dir(),
            std::env::temp_dir().join(format!("delonix-net-{}", unsafe { libc::geteuid() }))
        );

        std::env::remove_var("DELONIX_ROOT");
    }

    /// BUG FIXED: DNS resolution used to do a full directory scan + JSON parse
    /// PER QUERY. These lock in the pieces extracted so that scan can be done
    /// once per refresh interval (`build_dns_index`) instead of once per query
    /// (`dns_resolve`/`dns_index`) — see the `DNS_INDEX_TTL` doc comment.
    #[test]
    fn dns_index_keys_separam_namespace_e_vms_de_containers() {
        // Bare vs namespaced keys never collide with each other or with the VM
        // namespace — a container and a VM sharing a name must not cross-resolve.
        assert_eq!(dns_index_ns_key("db", "prod"), "db@prod");
        // Case-insensitive on the namespace, matching the original `cns != want`
        // comparison being done on values already read verbatim from JSON.
        assert_eq!(dns_index_ns_key("db", "PROD"), "db@prod");
        assert_eq!(dns_index_vm_key("db"), "vm:db");
        // A VM key can never collide with a bare container key (distinct prefix).
        assert_ne!(dns_index_vm_key("db"), "db");
    }

    #[test]
    fn parse_neigh_table_extrai_so_linhas_com_ipv4() {
        let raw = "10.200.254.11 dev delonix0 lladdr 52:54:00:aa:bb:cc REACHABLE\n\
                    fe80::1 dev delonix0 lladdr 52:54:00:aa:bb:cc STALE\n\
                    10.200.254.12 dev delonix0 lladdr 52:54:00:dd:ee:ff STALE\n";
        let table = parse_neigh_table(raw);
        // The IPv6 line is dropped (its 1st whitespace token has no '.').
        assert_eq!(table.len(), 2);
        assert_eq!(
            neigh_table_lookup(&table, "52:54:00:AA:BB:CC").as_deref(),
            Some("10.200.254.11")
        );
        assert_eq!(
            neigh_table_lookup(&table, "52:54:00:dd:ee:ff").as_deref(),
            Some("10.200.254.12")
        );
        assert_eq!(neigh_table_lookup(&table, "52:54:00:00:00:00"), None);
    }

    #[test]
    fn stale_holder_message_diz_o_pid_o_socket_e_como_recuperar() {
        let sock = PathBuf::from("/tmp/delonix-net-1000/control.sock");
        let msg = stale_holder_message(17552, &sock, None);
        // The three things the operator needs and the bare `ENOENT` never gave:
        // WHICH holder, WHICH path is missing, and the exact recovery.
        assert!(msg.contains("17552"), "{msg}");
        assert!(msg.contains("/tmp/delonix-net-1000/control.sock"), "{msg}");
        assert!(msg.contains("net netns down"), "{msg}");
        assert!(msg.contains("container restart"), "{msg}");
        // Without the legacy socket on disk the cause stays a hypothesis — it must
        // not claim the pre-v0.34.2 path as fact.
        assert!(!msg.contains("v0.34.2"), "{msg}");
    }

    #[test]
    fn stale_holder_message_nomeia_o_socket_legado_quando_ele_existe() {
        let sock = PathBuf::from("/tmp/delonix-net-1000/control.sock");
        let legacy = PathBuf::from("/home/w/.local/share/delonix/ingress/control.sock");
        let msg = stale_holder_message(17552, &sock, Some(&legacy));
        // The legacy socket being present IS the proof of an in-place upgrade —
        // the message says so, and names both paths.
        assert!(
            msg.contains("/home/w/.local/share/delonix/ingress/control.sock"),
            "{msg}"
        );
        assert!(msg.contains("v0.34.2"), "{msg}");
        assert!(msg.contains("older delonix build"), "{msg}");
        assert!(msg.contains("net netns down"), "{msg}");
    }

    // ---- VMs inside namespace isolation -------------------------------------

    /// The whole VM half of namespace isolation rests on ONE claim: the host can
    /// know a VM's address before the guest boots, because the holder's DHCP is
    /// deterministic from the MAC. If that stops holding, VMs get firewalled at
    /// an address nobody uses — and, worse, report as isolated.
    #[test]
    fn dhcp_lease_ip_e_deterministico_e_cai_no_pool() {
        let a = dhcp_lease_ip("10.200", "52:54:00:ab:cd:ef").unwrap();
        assert_eq!(a, dhcp_lease_ip("10.200", "52:54:00:ab:cd:ef").unwrap());
        assert_ne!(a, dhcp_lease_ip("10.200", "52:54:00:ab:cd:ee").unwrap());
        // Different network, same MAC → same host byte on the network's own /16.
        assert_eq!(
            a.rsplit('.').next(),
            dhcp_lease_ip("10.240", "52:54:00:ab:cd:ef")
                .unwrap()
                .rsplit('.')
                .next()
        );
        // The documented pool is `<prefix>.254.10-.254.249` — a lease outside it
        // would collide with the IPAM range the containers draw from.
        for i in 0..300u32 {
            let ip = dhcp_lease_ip("10.200", &format!("52:54:00:00:00:{i:02x}")).unwrap();
            let o: Vec<&str> = ip.split('.').collect();
            assert_eq!(&o[..3], &["10", "200", "254"], "{ip}");
            let host: u16 = o[3].parse().unwrap();
            assert!((10..=249).contains(&host), "{ip} out of pool");
        }
    }

    #[test]
    fn dhcp_lease_ip_recusa_prefixo_invalido() {
        assert!(dhcp_lease_ip("10", "52:54:00:ab:cd:ef").is_none());
        assert!(dhcp_lease_ip("10.200.0", "52:54:00:ab:cd:ef").is_none());
        assert!(dhcp_lease_ip("", "52:54:00:ab:cd:ef").is_none());
    }

    /// Achado de auditoria (MÉDIO): o `tap` de uma VM não levava regra
    /// anti-spoofing, ao contrário do veth de um container. O kernel do
    /// convidado não é nosso, logo pode pôr no fio o endereço de origem que
    /// quiser — e TODA a política deste motor (isolamento cross-namespace,
    /// `kind: Dependency`) decide pelo IP de origem. A regra tem de ser
    /// exactamente a mesma do veth, senão a fronteira vale para uns e não
    /// para outros.
    #[test]
    fn a_regra_antispoof_e_a_mesma_para_veth_e_para_tap_de_vm() {
        let veth = super::antispoof_rule_args("dlxn1a2b", "10.200.0.7");
        let tap = super::antispoof_rule_args("vt01", "10.200.254.42");

        // Mesma FORMA nos dois (só interface e endereço mudam).
        assert_eq!(veth.len(), tap.len());
        assert_eq!(veth[5], "iifname");
        assert_eq!(tap[5], "iifname");
        assert_eq!(tap[6], "vt01");
        assert_eq!(tap[10], "10.200.254.42");
        // O verdicto tem de ser `drop` sobre "origem != o endereço atribuído".
        assert_eq!(&tap[7..10], &["ip", "saddr", "!="]);
        assert_eq!(tap[11], "drop");
        // Vai para a chain que o `clear_antispoof` também varre, senão a
        // remoção nunca encontraria a regra que a criação emitiu.
        assert_eq!(tap[4], "fwdeny");
        assert_eq!(tap[3], super::INGRESS_TABLE);
    }

    /// A VM with nothing to isolate must keep emitting the OLD line, so a holder
    /// from a previous build goes on serving it. Only the namespaced VM — which
    /// genuinely needs the new behaviour — requires the new holder.
    #[test]
    fn vmtap_line_mantem_a_forma_curta_sem_namespace() {
        assert_eq!(
            vmtap_line(
                "vt01",
                "delonix0",
                "10.200.0.1",
                Some("10.200.254.42"),
                "default"
            ),
            "vmtap vt01 delonix0 10.200.0.1"
        );
        // No derivable lease → nothing to register, so the short form again
        // rather than a line with a hole in it.
        assert_eq!(
            vmtap_line("vt01", "delonix0", "10.200.0.1", None, "teamA"),
            "vmtap vt01 delonix0 10.200.0.1"
        );
        assert_eq!(
            vmtap_line(
                "vt01",
                "delonix0",
                "10.200.0.1",
                Some("10.200.254.42"),
                "teamA"
            ),
            "vmtap vt01 delonix0 10.200.0.1 10.200.254.42 teamA"
        );
    }

    /// A DHCP lease has to be recognized as an SDN address, or `ns_set_join`
    /// silently drops it and the VM never joins a namespace set. The pool sits at
    /// `.254.x`, well away from the container IPAM range, so this is not obvious.
    #[test]
    fn lease_de_vm_conta_como_ip_da_sdn() {
        assert!(is_ingress_ip(
            &dhcp_lease_ip("10.200", "52:54:00:ab:cd:ef").unwrap()
        ));
        assert!(is_ingress_ip(
            &dhcp_lease_ip("10.240", "52:54:00:11:22:33").unwrap()
        ));
    }

    /// Dois `DELONIX_ROOT` no mesmo uid: o pidfile é por-ROOT, o socket é
    /// por-USER, e antes desta correcção o segundo apagava a infra do primeiro.
    ///
    /// A mensagem é o valor todo do ramo — o ramo em si não se exercita sem dois
    /// holders vivos —, por isso é ela que leva o teste: tem de nomear os DOIS
    /// caminhos (senão quem a lê não sabe qual dos roots é o dono) e tem de
    /// oferecer as duas saídas reais, usar aquele root ou pará-lo de propósito.
    #[test]
    fn a_mensagem_de_root_alheio_nomeia_os_dois_caminhos_e_as_duas_saidas() {
        let m = super::foreign_holder_message(
            std::path::Path::new("/tmp/delonix-net-1000/control.sock"),
            std::path::Path::new("/home/w/cri/state/ingress"),
        );
        assert!(m.contains("/tmp/delonix-net-1000/control.sock"));
        assert!(m.contains("/home/w/cri/state/ingress"));
        assert!(m.contains("DELONIX_ROOT"));
        assert!(m.contains("delonix net netns down"));
        // Nunca sugerir a reconstrução: é isso que destrói a infra alheia.
        assert!(!m.contains("rebuild it"));
    }
}
