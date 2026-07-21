//! `discover` — auto-discovery of a workload's **internal ports** (from the
//! entrypoint), reading the listening sockets of its *network namespace* through
//! `/proc/<pid>/net/{tcp,tcp6,udp,udp6}`.
//!
//! It is **namespace-aware without entering the netns**: `/proc/<pid>/net/*` always
//! reflects the netns of process `pid` (host-visible). So, from the API process
//! (same user, rootless), the container's socket table is read directly — no
//! `nsenter`, no going through the holder. For a **pod**, it is enough to read
//! the infra ("pause") `pid`: the members share the netns, so their ports
//! all show up in the same table.
//!
//! The result feeds the auto-mapping: each discovered port becomes a candidate
//! firewall/ingress rule in the Console, which the operator can **publish** (DNAT
//! through the ingress) with one click. Discovery is purely observational —
//! best-effort and never fails: if it can't read, it returns empty.

use serde::{Deserialize, Serialize};

/// An internal port discovered listening in a workload's netns.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPort {
    /// `tcp` or `udp`.
    pub proto: String,
    /// Local listening port (1..=65535).
    pub port: u16,
    /// Name of the listening process (C1, "if possible"): mapped from the socket inode
    /// to the process `comm` in the same netns. `None` if not resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// Reads the listening ports in the netns of process `pid` (host-visible). Merges IPv4 and
/// IPv6 (deduplicated by `(proto, port)`) and sorts by port. Best-effort: an
/// unreadable file (dead process, no permissions) is simply skipped.
pub fn discover_ports(pid: i32) -> Vec<DiscoveredPort> {
    // (proto, port) -> inode of the listening socket. Dedup by (proto, port).
    let mut seen: std::collections::BTreeMap<(String, u16), u64> =
        std::collections::BTreeMap::new();
    // (file, proto, listen-only). TCP counts only the LISTEN state (0A); UDP has
    // no "listen", so it counts bound sockets (with no remote endpoint).
    for (file, proto, listen_only) in [
        ("tcp", "tcp", true),
        ("tcp6", "tcp", true),
        ("udp", "udp", false),
        ("udp6", "udp", false),
    ] {
        let path = format!("/proc/{pid}/net/{file}");
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (port, inode) in parse_listen_ports(&content, listen_only) {
            seen.entry((proto.to_string(), port)).or_insert(inode);
        }
    }
    // inode -> process name (comm) map of processes in the SAME netns as `pid`.
    let inode_comm = inode_comm_map(pid);
    let mut out: Vec<DiscoveredPort> = seen
        .into_iter()
        .map(|((proto, port), inode)| DiscoveredPort {
            proto,
            port,
            process: inode_comm.get(&inode).cloned(),
        })
        .collect();
    out.sort_by(|a, b| a.port.cmp(&b.port).then_with(|| a.proto.cmp(&b.proto)));
    out
}

/// `socket inode -> comm` map of the processes sharing `pid`'s NETNS (the
/// workload's processes). Best-effort: reads the target ns/net, scans `/proc/<p>` with the
/// same netns and maps the `socket:[inode]` fds → `comm`. Empty `BTreeMap` if nothing resolves.
fn inode_comm_map(pid: i32) -> std::collections::BTreeMap<u64, String> {
    let mut map = std::collections::BTreeMap::new();
    let target = match std::fs::read_link(format!("/proc/{pid}/ns/net")) {
        Ok(t) => t,
        Err(_) => return map,
    };
    let rd = match std::fs::read_dir("/proc") {
        Ok(r) => r,
        Err(_) => return map,
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let p: i32 = match name.to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        // only processes in the SAME netns as the workload.
        if std::fs::read_link(format!("/proc/{p}/ns/net"))
            .ok()
            .as_deref()
            != Some(target.as_path())
        {
            continue;
        }
        let comm = std::fs::read_to_string(format!("/proc/{p}/comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if comm.is_empty() {
            continue;
        }
        if let Ok(fds) = std::fs::read_dir(format!("/proc/{p}/fd")) {
            for fd in fds.flatten() {
                if let Ok(link) = std::fs::read_link(fd.path()) {
                    let s = link.to_string_lossy();
                    // format "socket:[12345]"
                    if let Some(num) = s.strip_prefix("socket:[").and_then(|x| x.strip_suffix("]"))
                    {
                        if let Ok(inode) = num.parse::<u64>() {
                            map.entry(inode).or_insert_with(|| comm.clone());
                        }
                    }
                }
            }
        }
    }
    map
}

/// PURE parser of a `/proc/net/{tcp,tcp6,udp,udp6}` table: returns the local
/// listening ports. Format (after the header):
/// `sl  local_address rem_address st tx_queue ...` — where `local_address` is
/// `HEXIP:HEXPORT` and `st` is the state (`0A` = TCP_LISTEN).
///
/// - `listen_only = true`  (TCP): only counts sockets in state `0A` (LISTEN).
/// - `listen_only = false` (UDP): counts sockets bound to a port with an empty
///   remote endpoint (`rem_port == 0`) — i.e. servers, not connected clients.
fn parse_listen_ports(table: &str, listen_only: bool) -> Vec<(u16, u64)> {
    let mut ports: Vec<(u16, u64)> = Vec::new();
    for line in table.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        let Some((_, lport_hex)) = cols[1].rsplit_once(':') else {
            continue;
        };
        let Ok(lport) = u16::from_str_radix(lport_hex, 16) else {
            continue;
        };
        if lport == 0 {
            continue;
        }
        if listen_only {
            if cols[3] != "0A" {
                continue;
            }
        } else {
            let rem_port = cols[2]
                .rsplit_once(':')
                .and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
                .unwrap_or(0);
            if rem_port != 0 {
                continue;
            }
        }
        // socket inode = 10th column (index 9) in /proc/net/{tcp,udp}.
        let inode = cols.get(9).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        if !ports.iter().any(|(p, _)| *p == lport) {
            ports.push((lport, inode));
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real sample of /proc/net/tcp: a server LISTENing on :8080 (1F90), an
    // ESTABLISHED (01) connection on :8080 that should NOT count as a new port, and a
    // socket LISTENing on :22 (0016).
    const TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000 100 0 0 10 0
   1: 0100007F:1F90 0200000A:D7F2 01 00000000:00000000 00:00000000 00000000  1000        0 23456 1 0000 20 4 30 10 -1
   2: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 34567 1 0000 100 0 0 10 0
";

    // UDP: a server bound on :53 (0035, no remote) and a connected client
    // on :C001 (remote port != 0) that should NOT count.
    const UDP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  10: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 45678 2 0000 0
  11: 0100007F:C001 0200000A:0035 01 00000000:00000000 00:00000000 00000000  1000        0 56789 2 0000 0
";

    fn ports_only(v: Vec<(u16, u64)>) -> Vec<u16> {
        v.into_iter().map(|(p, _)| p).collect()
    }

    #[test]
    fn tcp_lists_only_listen_sockets() {
        let mut p = ports_only(parse_listen_ports(TCP, true));
        p.sort_unstable();
        assert_eq!(p, vec![22, 8080]); // 0016, 1F90 — the ESTABLISHED connection is ignored
    }

    #[test]
    fn udp_lists_only_bound_servers() {
        let p = ports_only(parse_listen_ports(UDP, false));
        assert_eq!(p, vec![53]); // 0035; the connected client (rem != 0) is ignored
    }

    #[test]
    fn parses_socket_inode() {
        // the 1st TCP LISTEN line has inode 12345 (col 9).
        let v = parse_listen_ports(TCP, true);
        assert!(v.iter().any(|(p, ino)| *p == 8080 && *ino == 12345));
    }

    #[test]
    fn empty_or_header_only_yields_nothing() {
        assert!(parse_listen_ports("", true).is_empty());
        assert!(parse_listen_ports("  sl  local_address rem_address   st\n", true).is_empty());
    }

    #[test]
    fn ignores_port_zero_and_malformed_lines() {
        let t = "\
header
   0: 00000000:0000 00000000:0000 0A x
   1: garbage
   2: 00000000:1F90 00000000:0000 0A x
";
        assert_eq!(ports_only(parse_listen_ports(t, true)), vec![8080]);
    }
}
