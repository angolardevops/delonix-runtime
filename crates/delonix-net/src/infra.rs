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
//! binary to [`holder_main`].

use crate::{run, run_ok, SLIRP_IP};
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
fn holder_pid_path() -> PathBuf {
    ingress_dir().join("holder.pid")
}
fn slirp_pid_path() -> PathBuf {
    ingress_dir().join("slirp.pid")
}
/// The single slirp's api-socket (where the `add_hostfwd`s are requested in Phase 4).
pub fn slirp_sock_path() -> PathBuf {
    ingress_dir().join("slirp.sock")
}
/// The holder's control socket (netns/veth factory): the host requests attach/detach.
fn control_sock_path() -> PathBuf {
    ingress_dir().join("control.sock")
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
    format!(
        "table ip {INGRESS_TABLE} {{\n\
         \x20 set {DLXALL_SET} {{ type ipv4_addr; }}\n\
         \x20 chain pre {{ type nat hook prerouting priority -100; }}\n\
         \x20 chain post {{ type nat hook postrouting priority 100; oifname \"tap0\" masquerade; }}\n\
         \x20 chain fwdeny {{ type filter hook forward priority -10; }}\n\
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
pub fn reap_orphan_refs(live: &std::collections::HashSet<String>) -> usize {
    let _lock = FileLock::acquire();
    let dir = refs_dir();
    let orphans = orphan_refs(&refs_in(&dir), live);
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
    /// Host-visible PID of the netns holder (alive as long as the infra exists).
    pub holder_pid: Option<i32>,
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
    let slirp = read_pid(&slirp_pid_path()).filter(|&p| pid_alive(p));
    InfraStatus {
        up: holder.is_some() && slirp.is_some(),
        holder_pid: holder,
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
    let st = status();
    if st.up {
        return Ok(());
    }
    // partial state (e.g.: dead holder) → clean up before recreating.
    teardown();
    std::fs::create_dir_all(ingress_dir()).map_err(|e| Error::Runtime {
        context: "ingress dir",
        message: e.to_string(),
    })?;
    let holder_pid = start_holder()?;
    if let Err(e) = start_slirp(holder_pid) {
        // if the slirp fails, we don't leave an orphan holder.
        teardown();
        return Err(e);
    }
    Ok(())
}

/// Tears down the infra: kills the slirp and the holder (which frees the netns) and cleans up the
/// artifacts. Best-effort and idempotent.
pub fn teardown() {
    // the DHCP/DNS/RA servers are threads of the holder — they die when it's killed.
    kill_pidfile(&slirp_pid_path());
    kill_pidfile(&holder_pid_path());
    let _ = std::fs::remove_file(slirp_sock_path());
    let _ = std::fs::remove_file(control_sock_path());
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
fn wait_readable(fd: i32, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` is valid and lives for the duration of the call; poll doesn't retain the pointer.
    // EINTR (signal) returns -1 → we treat it as "not ready", the caller warns.
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 }
}

fn start_holder() -> Result<i32> {
    let exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    let _ = std::fs::remove_file(status_path());
    // `--map-auto` maps the user's ENTIRE subuid/subgid range (/etc/subuid),
    // not just root: real images (nginx uid 101, postgres, …) need chown
    // to uids != 0 INSIDE the container, which thus become mappable. `--map-root-user`
    // maps the userns's uid 0 → the user's uid on the host.
    let child = Command::new("unshare")
        .args([
            "--user",
            "--map-auto",
            "--map-root-user",
            "--net",
            "--mount",
            "--",
        ])
        .arg(&exe)
        .args(["netns", "holder"])
        // the holder runs with uid->0 in the userns; forces the paths to the real base.
        .env("DELONIX_ROOT", base_root())
        .env("DELONIX_INTERNAL", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
            Ok(s) if s.trim() == "ready" => return Ok(pid),
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
            if !wait_readable(rd, 10_000) {
                tracing::warn!("slirp4netns did not signal ready within 10s; the container network may not be operational");
            }
            let mut b = [0u8; 1];
            // SAFETY: reads 1 byte from the read-end (already readable, or we gave up above).
            unsafe {
                libc::read(rd, b.as_mut_ptr() as *mut libc::c_void, 1);
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
pub fn holder_main() -> ! {
    let started = setup_infra_netns().and_then(|_| {
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

/// Accepts connections on the control socket and serves one command per connection (the netns/veth
/// factory). Runs INSIDE the holder, so the `ip`/`ip netns` operations stay
/// in the infra netns without `nsenter`. Synchronous (one attach at a time — sufficient).
/// uid of the peer of a Unix connection (via SO_PEERCRED). `None` on failure.
fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt on SO_PEERCRED with a correctly-sized ucred buffer.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if r == 0 {
        Some(cred.uid)
    } else {
        None
    }
}

fn control_loop(listener: std::os::unix::net::UnixListener) -> ! {
    use std::io::{BufRead, BufReader, Write};
    // SAFETY: geteuid() has no preconditions.
    let own_uid = unsafe { libc::geteuid() };
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        // SO_PEERCRED: only accepts commands from the engine's own uid — prevents a
        // non-privileged local user from driving the holder / injecting nft (CAP_NET_ADMIN).
        if peer_uid(&stream) != Some(own_uid) {
            continue;
        }
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            continue;
        }
        let reply = handle_control(line.trim());
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
    let res = match parts.as_slice() {
        ["ping"] => Ok(()),
        // 5 tokens = `default` namespace (compat with the old client); 6 = namespaced.
        ["attach", netns, ip, bridge, gateway] => do_attach(netns, ip, bridge, gateway, "default"),
        ["attach", netns, ip, bridge, gateway, ns] => do_attach(netns, ip, bridge, gateway, ns),
        ["detach", netns] => do_detach(netns),
        ["cni-del", netns, id, ifname, hex] => do_cni_del(netns, id, ifname, hex),
        // live multi-homing (rootless): connects/disconnects an ADDITIONAL network to a
        // container already running (extra veth to the private network's bridge).
        ["attach-extra", netns, ifname, ip, bridge, gateway] => {
            do_attach_extra(netns, ifname, ip, bridge, gateway)
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
        ["vmtap", tap, bridge, gateway] => do_vmtap(tap, bridge, gateway),
        ["vmtapdel", tap] => do_vmtapdel(tap),
        ["publish", proto, host_port, cip, cport] => do_publish(proto, host_port, cip, cport),
        ["publish-allow", proto, host_port, cip, cport, cidrs] => {
            do_publish_allow(proto, host_port, cip, cport, cidrs)
        }
        ["unpublish", host_port] => do_unpublish(host_port),
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
        let host = 10 + (crate::fnv32(&macs) % 240) as u8; // pool .254.10–.254.249
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
    let sets = crate::capture("nft", &["list", "sets", "ip", INGRESS_TABLE]).unwrap_or_default();
    for line in sets.lines() {
        if let Some(name) = line.split_whitespace().nth(1) {
            if name.starts_with("dlxns") {
                run_ok(
                    "nft",
                    &["delete", "element", "ip", INGRESS_TABLE, name, &elem],
                );
            }
        }
    }
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
    // IPv6 (ULA) on eth0 + v6 default route (best-effort; the host may have v6 off).
    let p = prefix_of(gateway);
    let gw6 = v6_gw(&p);
    if let Some(v6) = v6_of(ip) {
        let cidr6 = format!("{v6}/64");
        run_ok(
            "ip",
            &[
                "netns", "exec", &netns, "ip", "-6", "addr", "add", &cidr6, "dev", "eth0", "nodad",
            ],
        );
        run_ok(
            "ip",
            &[
                "netns", "exec", &netns, "ip", "-6", "route", "add", "default", "via", &gw6,
            ],
        );
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
    run_ok(
        "nft",
        &[
            "insert",
            "rule",
            "ip",
            INGRESS_TABLE,
            "fwdeny",
            "iifname",
            &vh,
            "ip",
            "saddr",
            "!=",
            ip,
            "drop",
        ],
    );
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
fn do_attach_extra(netns: &str, ifname: &str, ip: &str, bridge: &str, gateway: &str) -> Result<()> {
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
    // IPv6 (ULA) on the new interface (best-effort; no v6 default route — the primary keeps it).
    if let Some(v6) = v6_of(ip) {
        let cidr6 = format!("{v6}/64");
        run_ok(
            "ip",
            &[
                "netns", "exec", &netns, "ip", "-6", "addr", "add", &cidr6, "dev", &ifname, "nodad",
            ],
        );
    }
    // ANTI-SPOOFING also on the additional interface (same per-IP guarantee as eth0).
    clear_antispoof(&vh);
    run_ok(
        "nft",
        &[
            "insert",
            "rule",
            "ip",
            INGRESS_TABLE,
            "fwdeny",
            "iifname",
            &vh,
            "ip",
            "saddr",
            "!=",
            ip,
            "drop",
        ],
    );
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
fn do_vmtap(tap: &str, bridge: &str, gateway: &str) -> Result<()> {
    let tap = sanitize(tap);
    let bridge = sanitize(bridge);
    ensure_net_bridge(&bridge, gateway)?;
    run_ok("ip", &["link", "del", &tap]); // clears leftovers
    run("ip", &["tuntap", "add", "dev", &tap, "mode", "tap"])?;
    run("ip", &["link", "set", &tap, "master", &bridge])?;
    run("ip", &["link", "set", &tap, "up"])?;
    Ok(())
}

/// Removes a VM's `tap` (on `vm rm`/stop). Best-effort.
fn do_vmtapdel(tap: &str) -> Result<()> {
    run_ok("ip", &["link", "del", &sanitize(tap)]);
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

/// Like [`do_publish`], but with a **source allowlist**: only the given CIDRs
/// reach the `host_port`; the rest is dropped BEFORE the DNAT (`insert` at the top of the
/// `pre` chain). The CIDRs are validated (`fw_src_ok`) — nft anti-injection. Used
/// to expose an app's DB only to authorized IPs (firewall).
fn do_publish_allow(
    proto: &str,
    host_port: &str,
    cip: &str,
    cport: &str,
    cidrs_csv: &str,
) -> Result<()> {
    validate_publish(proto, host_port, cip, cport)?;
    let cidrs: Vec<&str> = cidrs_csv
        .split(',')
        .map(|c| c.trim())
        .filter(|c| !c.is_empty() && delonix_runtime_core::fw_src_ok(c))
        .collect();
    if cidrs.is_empty() {
        return Err(Error::Invalid("empty allowlist or no valid CIDRs".into()));
    }
    // drop at the top of `pre`: traffic to this host_port whose saddr is NOT in the
    // allowlist is discarded before reaching the DNAT rule (which comes after).
    let set = format!("{{ {} }}", cidrs.join(", "));
    run(
        "nft",
        &[
            "insert",
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
            "ip",
            "saddr",
            "!=",
            &set,
            "drop",
        ],
    )?;
    do_publish(proto, host_port, cip, cport)
}

/// Removes a `host_port`'s DNAT (by handle) from the `pre` chain. Best-effort.
fn do_unpublish(host_port: &str) -> Result<()> {
    if !is_port(host_port) {
        return Err(Error::Invalid(format!("invalid port: {host_port}")));
    }
    // lists the chain with handles and deletes the rule(s) matching the dport.
    let listed = crate::capture("nft", &["-a", "list", "chain", "ip", INGRESS_TABLE, "pre"])
        .unwrap_or_default();
    let needle = format!("dport {host_port} ");
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
fn do_egress(policy: &str) -> Result<()> {
    let listed = crate::capture(
        "nft",
        &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"],
    )
    .unwrap_or_default();
    for line in listed.lines() {
        if line.contains("oifname \"tap0\"") && line.contains("drop") {
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
        let (self_dir, peer_dir) = if r.dir == "out" {
            ("saddr", "daddr")
        } else {
            ("daddr", "saddr")
        };
        let mut line = format!("ip {self_dir} {ip}");
        if !r.src.is_empty() && r.src != "0.0.0.0/0" && r.src != "*" {
            line.push_str(&format!(" ip {peer_dir} {}", r.src));
        }
        if !r.proto.is_empty() && r.proto != "any" {
            line.push_str(&format!(" {}", r.proto));
            if !r.port.is_empty() && r.port != "*" {
                line.push_str(&format!(" dport {}", r.port));
            }
        }
        line.push_str(if r.action == "allow" {
            " accept"
        } else {
            " drop"
        });
        body.push_str(&format!("\t\t{line}\n"));
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
        body.push_str(&format!("\t\tip daddr {ip} ip saddr @{nsset} accept\n"));
        body.push_str(&format!(
            "\t\tip daddr {ip} ip saddr @{DLXALL_SET} ct state new drop\n"
        ));
    }
    if fw.policy_in == "deny" {
        body.push_str(&format!("\t\tip daddr {ip} drop\n"));
    }
    if fw.policy_out == "deny" {
        body.push_str(&format!("\t\tip saddr {ip} drop\n"));
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

/// Applies a container's firewall in `dlxing` (runs in the holder): ensures the chain
/// `fw<hash>` + jumps in the `fwd` (daddr/saddr==ip), and rebuilds the body. `hex` is the
/// `ContainerFw` JSON in hexadecimal (the control channel is line-based).
fn do_firewall(ip: &str, hex: &str) -> Result<()> {
    if !is_ingress_ip(ip) {
        return Err(Error::Invalid(format!(
            "IP {ip} outside the ingress space (10.200-254.x)"
        )));
    }
    let bytes = hex_decode(hex).ok_or_else(|| Error::Invalid("invalid hex".into()))?;
    let fw: delonix_runtime_core::ContainerFw = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Invalid(format!("firewall JSON: {e}")))?;
    let chain = fw_chain_name(ip);
    // ensures the chain (regular, only a jump target).
    let exists = crate::capture("nft", &["list", "chain", "ip", INGRESS_TABLE, &chain])
        .map(|o| o.contains(&chain))
        .unwrap_or(false);
    if !exists {
        run_ok("nft", &["add", "chain", "ip", INGRESS_TABLE, &chain]);
    }
    // idempotent jumps in the fwd: traffic TO (daddr) and FROM (saddr) the IP.
    let fwd_chain = crate::capture("nft", &["list", "chain", "ip", INGRESS_TABLE, "fwdeny"])
        .unwrap_or_default();
    for dir in ["daddr", "saddr"] {
        if !fwd_chain.contains(&format!("ip {dir} {ip} jump {chain}")) {
            run_ok(
                "nft",
                &[
                    "add",
                    "rule",
                    "ip",
                    INGRESS_TABLE,
                    "fwdeny",
                    "ip",
                    dir,
                    ip,
                    "jump",
                    &chain,
                ],
            );
        }
    }
    // flush + body rebuild in a single script (keeps the chain and the jumps).
    let body = fw_chain_body(ip, &fw);
    let script = format!(
        "flush chain ip {INGRESS_TABLE} {chain}\ntable ip {INGRESS_TABLE} {{\n\tchain {chain} {{\n{body}\t}}\n}}\n"
    );
    apply_nft_stdin(&script)
}

/// Removes a container's firewall from `dlxing`: takes the jumps out of the `fwd` (by
/// handle) and deletes the chain. Best-effort.
fn do_unfirewall(ip: &str) -> Result<()> {
    let chain = fw_chain_name(ip);
    if let Ok(out) = crate::capture(
        "nft",
        &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"],
    ) {
        for line in out.lines() {
            if line.contains(&format!("jump {chain}")) {
                if let Some(h) = line.rsplit("handle ").next().map(|s| s.trim()) {
                    run_ok(
                        "nft",
                        &["delete", "rule", "ip", INGRESS_TABLE, "fwdeny", "handle", h],
                    );
                }
            }
        }
    }
    run_ok("nft", &["delete", "chain", "ip", INGRESS_TABLE, &chain]);
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
    /// [`ensure_net_bridge`] when the bridge is recreated.
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
pub fn attach_extra_container(id: &str, idx: u32, net: &str) -> Result<(String, String)> {
    let (bridge, prefix, gateway) = resolve_net(net)?;
    let ip = crate::ipam::allocate(&prefix, id)?; // unique lease on the additional network
    let ifname = format!("eth{idx}");
    let netns = sanitize(id);
    control_send(&format!(
        "attach-extra {netns} {ifname} {ip} {bridge} {gateway}"
    ))?;
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
    crate::ipam::release(&crate::ipam::prefix_of(ip), id); // frees the extra network's lease
}

/// **Detaches a container from the ingress**: clears the firewall (on its `ip`), asks the
/// holder for the `detach` and lowers the ref-count (tears down the infra on the last). Best-effort.
pub fn detach_container(id: &str, ip: &str) {
    let netns = sanitize(id);
    let _ = control_send(&format!("unfirewall {ip}"));
    let _ = control_send(&format!("detach {netns}"));
    crate::ipam::release(&crate::ipam::prefix_of(ip), id); // frees the IP lease
    release(id); // removes the ref marker (teardown when it becomes empty)
}

/// **Applies a container's parameterizable firewall AT THE INGRESS** (the only place,
/// via the bind): translates the `ContainerFw` (the same one persisted in the record, v0.1.93) to
/// the `dlxing`'s `fw<hash>` chain, keyed by the container's `ip` on its network.
pub fn apply_firewall(id: &str, ip: &str, fw: &delonix_runtime_core::ContainerFw) -> Result<()> {
    let json = serde_json::to_vec(fw).map_err(|e| Error::Invalid(e.to_string()))?;
    control_send(&format!(
        "firewall {} {} {}",
        sanitize(id),
        ip,
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
/// best-effort in the holder (degrades if the kernel doesn't support it). See [`do_l4guard`].
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
/// Requires the holder up (`ensure_up` first). Idempotent. See [`do_vxlan`].
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
pub fn vm_attach(vm: &str, net: &str) -> Result<String> {
    let (bridge, _prefix, gateway) = resolve_net(net)?;
    // Ref key `vm-<name>` — its own namespace, distinct from the container ids
    // and the `cri-*` pods; the `prune` reaper preserves the `vm-*` (managed by
    // another store) just like the `cri-*`.
    acquire(&format!("vm-{vm}"))?;
    let tap = vm_tap_name(vm);
    match control_send(&format!("vmtap {tap} {bridge} {gateway}")) {
        Ok(()) => Ok(tap),
        Err(e) => {
            release(&format!("vm-{vm}"));
            Err(e)
        }
    }
}

/// **Detaches a VM from the ingress**: removes the `tap` and lowers the ref-count. Best-effort.
pub fn vm_detach(vm: &str) {
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
    let oct: Vec<u8> = prefix.split('.').filter_map(|x| x.parse().ok()).collect();
    if oct.len() != 2 {
        return None;
    }
    // Same string format that `dhcp_serve` puts into the `fnv32` (lowercase,
    // `:`-separated) — otherwise the hash diverges and the IP doesn't match the assigned one.
    let macs = mac.to_lowercase();
    let host = 10 + (crate::fnv32(&macs) % 240) as u8;
    Some(format!("{}.{}.254.{host}", oct[0], oct[1]))
}

/// **Publishes a port through the ingress** (the container's bind): `add_hostfwd` on the
/// single slirp (host → tap0) + DNAT on the `pre` chain (tap0 → container). `spec` is
/// `hostPort:contPort[/tcp|udp]`. This is WHERE the ingress firewall's parameterizable
/// rules live (next increment: allow/deny per port/CIDR on the same
/// surface).
pub fn publish_port(cip: &str, spec: &str) -> Result<()> {
    let (host_port, cont_port, proto) = crate::parse_publish(spec)?;
    // host → tap0:host_port (the single slirp; guest_port == host_port).
    crate::slirp_add_hostfwd(&slirp_sock_path(), &host_port, &host_port, &proto)?;
    // tap0:host_port → container:cont_port (DNAT in the infra netns, via the holder).
    control_send(&format!("publish {proto} {host_port} {cip} {cont_port}"))
}

/// Like [`publish_port`], but restricts access to the `host_port` to an **allowlist**
/// of CIDRs (inbound firewall): the rest is dropped before the DNAT. `spec` is
/// `hostPort:contPort[/proto]`; `cidrs` are validated in the holder (`fw_src_ok`).
/// Used to expose an app's DB only to authorized IPs.
pub fn publish_port_allow(cip: &str, spec: &str, cidrs: &[&str]) -> Result<()> {
    let (host_port, cont_port, proto) = crate::parse_publish(spec)?;
    crate::slirp_add_hostfwd(&slirp_sock_path(), &host_port, &host_port, &proto)?;
    let csv = cidrs.join(",");
    control_send(&format!(
        "publish-allow {proto} {host_port} {cip} {cont_port} {csv}"
    ))
}

/// Removes a `host_port`'s publication: takes the `add_hostfwd` out of the slirp and the DNAT
/// out of the `pre` chain. Best-effort.
pub fn unpublish_port(host_port: &str) {
    trace_unpublish("unpublish_port", host_port);
    let _ = slirp_remove_hostfwd(&slirp_sock_path(), host_port);
    let _ = control_send(&format!("unpublish {host_port}"));
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

/// Reconciles the SINGLE ingress slirp's `hostfwd`s against the ports ACTUALLY in
/// use by live containers: removes the orphan entries (from containers already removed,
/// or that died without cleaning up) that would otherwise block the reuse of the host
/// port. `live_ports` = host_ports published by live containers. Part of reaper
/// #1 (port-leak). Returns how many it removed. Cheap (1 query to the api-socket).
pub fn reap_orphan_hostfwds(live_ports: &std::collections::HashSet<u32>) -> usize {
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
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
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

pub fn slirp_remove_hostfwd(sock: &Path, host_port: &str) -> Result<()> {
    trace_unpublish("slirp_remove_hostfwd", host_port);
    let hp: u32 = host_port
        .parse()
        .map_err(|_| Error::Invalid("invalid port".into()))?;
    let listed = slirp_api(sock, r#"{"execute":"list_hostfwd"}"#)?;
    let v: serde_json::Value = serde_json::from_str(&listed).unwrap_or(serde_json::Value::Null);
    if let Some(entries) = hostfwd_entries(&v) {
        for e in entries {
            if e.get("host_port").and_then(|p| p.as_u64()) == Some(hp as u64) {
                if let Some(id) = e.get("id").and_then(|i| i.as_u64()) {
                    let cmd =
                        format!(r#"{{"execute":"remove_hostfwd","arguments":{{"id":{id}}}}}"#);
                    let _ = slirp_api(sock, &cmd);
                }
            }
        }
    }
    Ok(())
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
pub fn container_net_bytes(id: &str) -> Option<(u64, u64)> {
    let prefix = join_argv(id)?;
    let read = |stat: &str| -> Option<u64> {
        let mut argv = prefix.clone();
        argv.push("cat".into());
        argv.push(format!("/sys/class/net/eth0/statistics/{stat}"));
        let out = Command::new(&argv[0]).args(&argv[1..]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    };
    Some((read("rx_bytes")?, read("tx_bytes")?))
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
    if status().holder_pid.is_none() {
        return Err(Error::Runtime {
            context: "control socket",
            message: "ingress holder is down".into(),
        });
    }
    let sock = control_sock_path();
    let mut last = String::from("control socket unavailable");
    for _ in 0..50 {
        match UnixStream::connect(&sock) {
            Ok(mut s) => {
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                s.write_all(format!("{cmd}\n").as_bytes())
                    .map_err(|e| Error::Runtime {
                        context: "control write",
                        message: e.to_string(),
                    })?;
                let mut resp = String::new();
                let _ = s.read_to_string(&mut resp);
                let resp = resp.trim();
                if resp == "ok" {
                    return Ok(String::new());
                }
                if let Some(body) = resp.strip_prefix("ok ") {
                    return Ok(body.trim().to_string());
                }
                return Err(Error::Runtime {
                    context: "ingress control",
                    message: resp.trim_start_matches("err:").trim().to_string(),
                });
            }
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
    }
    Err(Error::Runtime {
        context: "control socket",
        message: last,
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
fn dns_server_main() {
    let sock = match std::net::UdpSocket::bind("0.0.0.0:53") {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut buf = [0u8; 1500];
    loop {
        let (n, peer) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(_) => continue,
        };
        if n >= 12 {
            if let Some(r) = handle_dns(&buf[..n]) {
                let _ = sock.send_to(&r, peer);
            }
        }
    }
}

/// Response to a DNS query: if it's `A` and the name is of an ingress container/VM,
/// responds with the IP; otherwise forwards to the upstream.
fn handle_dns(q: &[u8]) -> Option<Vec<u8>> {
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
    if qtype == 1 {
        if let Some(ip) = dns_resolve(&name) {
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
            return Some(r);
        }
    }
    // External name: forwards and, if it's on an FQDN allowlist, learns the
    // response's A-records into the egress nft set (before returning it).
    let resp = forward_dns(q)?;
    snoop_fqdn(&name, &resp);
    Some(resp)
}

/// Forwards the raw query to the upstream (the slirp's DNS; fallback 1.1.1.1) and
/// returns the response.
fn forward_dns(q: &[u8]) -> Option<Vec<u8>> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .ok()?;
    for up in [crate::SLIRP_DNS, "1.1.1.1"] {
        if sock.send_to(q, format!("{up}:53")).is_ok() {
            let mut buf = [0u8; 1500];
            if let Ok((n, _)) = sock.recv_from(&mut buf) {
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

fn dns_resolve(name: &str) -> Option<[u8; 4]> {
    let (cname, want_ns) = parse_internal_name(name)?;
    let n = cname; // container name to match
                   // containers: <base>/containers/*.json (name + ip [+ namespace])
    if let Ok(rd) = std::fs::read_dir(base_root().join("containers")) {
        for e in rd.flatten() {
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(e.path()).unwrap_or_default(),
            ) else {
                continue;
            };
            if v["name"].as_str().map(|s| s.to_lowercase()).as_deref() == Some(n.as_str()) {
                // Scheme with namespace (`.delonix.internal`): only resolves if the
                // container's namespace matches (isolation also in DNS).
                if let Some(want) = &want_ns {
                    let cns = v["namespace"].as_str().unwrap_or("default");
                    if cns != want {
                        continue;
                    }
                }
                if let Some(ip) = v["ip"].as_str().and_then(parse_v4) {
                    return Some(ip);
                }
            }
        }
    }
    // VMs: <base>/vms/*.json (name + mac) → IP pela tabela neigh
    if let Ok(rd) = std::fs::read_dir(base_root().join("vms")) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(e.path()).unwrap_or_default(),
            ) else {
                continue;
            };
            if v["name"].as_str().map(|s| s.to_lowercase()).as_deref() == Some(n.as_str()) {
                if let Some(mac) = v["mac"].as_str() {
                    if let Some(ip) = neigh_ip_local(mac).as_deref().and_then(parse_v4) {
                        return Some(ip);
                    }
                }
            }
        }
    }
    None
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

/// IP of a MAC via the `neigh` table — runs INSIDE the holder (already in the netns), no nsenter.
fn neigh_ip_local(mac: &str) -> Option<String> {
    let mac = mac.to_lowercase();
    let out = crate::capture("ip", &["-o", "neigh", "show"]).ok()?;
    for line in out.lines() {
        if line.to_lowercase().contains(&mac) {
            if let Some(ip) = line.split_whitespace().next() {
                if ip.contains('.') {
                    return Some(ip.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
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
            body.contains("ip daddr 10.200.0.5 ip saddr 10.200.0.0/16 tcp dport 8080 accept"),
            "{body}"
        );
        // out rule: saddr==ip, drop (proto any → no proto/dport)
        assert!(body.contains("ip saddr 10.200.0.5 drop"), "{body}");
        // policy in=deny → final drop on the daddr
        assert!(body.contains("ip daddr 10.200.0.5 drop"), "{body}");
        // EXPLICIT inbound policy (deny) → does NOT emit namespace rules.
        assert!(!body.contains("@dlxall"), "{body}");
        // disabled → empty body
        let off = delonix_runtime_core::ContainerFw {
            enabled: false,
            ..fw
        };
        assert!(fw_chain_body("10.200.0.5", &off).is_empty());
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
            body.contains(&format!("ip daddr 10.200.0.7 ip saddr @{nsset} accept")),
            "{body}"
        );
        assert!(
            body.contains("ip daddr 10.200.0.7 ip saddr @dlxall ct state new drop"),
            "{body}"
        );
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
    fn base_root_honours_explicit_root() {
        // with DELONIX_ROOT set, ingress_dir is deterministic and does NOT depend
        // on the uid (essential for the holder with uid mapped to 0).
        std::env::set_var("DELONIX_ROOT", "/tmp/dlx-test-root");
        assert_eq!(ingress_dir(), PathBuf::from("/tmp/dlx-test-root/ingress"));
        std::env::remove_var("DELONIX_ROOT");
    }
}
