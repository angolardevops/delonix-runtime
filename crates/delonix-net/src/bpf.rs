//! Optional eBPF observability datapath — per-IP flow accounting on `delonix0`.
//!
//! Two tc/clsact classifiers (`bpf/delonix_flow.bpf.c`) count bytes/packets per
//! container IP into a pinned BPF hash map, WITHOUT ever dropping or mangling
//! traffic (the nft firewall stays the only enforcer). This is pure telemetry.
//!
//! **Capability-gated, degrades silently.** Loading an eBPF program needs
//! `CAP_BPF` + `CAP_NET_ADMIN` in the init namespace (and the host must permit
//! it) — a rootless runtime has neither, so [`available`] returns false and the
//! whole module no-ops. It activates only when delonix runs privileged (root
//! install, or a helper with the caps). Nothing here is required for the SDN to
//! work: without it, `flow`/`ls` just fall back to nft/veth counters.

use std::collections::HashMap;
use std::net::Ipv4Addr;

/// The pinned map name (matches `delonix_flows` in the eBPF source).
const MAP_NAME: &str = "delonix_flows";

/// Per-IP counters read out of the BPF map.
#[derive(Debug, Clone, Copy, Default)]
pub struct Flow {
    pub rx_packets: u64,
    pub rx_bytes: u64,
    pub tx_packets: u64,
    pub tx_bytes: u64,
}

/// `true` if the eBPF datapath CAN run here: the object was built in, the tools
/// exist, and we hold `CAP_BPF` (approximated by an effective-capability check;
/// real root always qualifies). Cheap enough to call before every use.
pub fn available() -> bool {
    object_bytes().is_some() && has_cap_bpf() && tool_exists("tc") && tool_exists("bpftool")
}

/// The compiled eBPF object, embedded at build time when the toolchain was
/// present (see `build.rs`). `None` on hosts that built without clang/headers.
fn object_bytes() -> Option<&'static [u8]> {
    #[cfg(bpf_object)]
    {
        Some(include_bytes!(env!("DELONIX_BPF_OBJECT")))
    }
    #[cfg(not(bpf_object))]
    {
        None
    }
}

fn tool_exists(bin: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|p| p.join(bin).exists())
}

/// Effective root, or `CAP_BPF` (bit 39) set in the effective capability set.
fn has_cap_bpf() -> bool {
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    let status = match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => s,
        Err(_) => return false,
    };
    for line in status.lines() {
        if let Some(hex) = line.strip_prefix("CapEff:") {
            if let Ok(bits) = u64::from_str_radix(hex.trim(), 16) {
                return bits & (1 << 39) != 0; // CAP_BPF
            }
        }
    }
    false
}

/// Materialize the embedded object to a temp file so `tc`/`bpftool` can read it.
/// Returns the path (caller keeps it alive for the duration of the load).
fn stage_object() -> Option<std::path::PathBuf> {
    let bytes = object_bytes()?;
    let path = std::env::temp_dir().join("delonix_flow.bpf.o");
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

/// Attach the accounting classifiers to `iface` (e.g. `delonix0`). Idempotent:
/// re-attaching first clears the old clsact qdisc. `run` runs a command,
/// optionally inside a netns (via the caller's wrapper) — kept as a closure so
/// the ingress holder can inject `nsenter` without this crate knowing about it.
pub fn attach<F>(iface: &str, run: F) -> bool
where
    F: Fn(&[&str]) -> bool,
{
    let obj = match stage_object() {
        Some(p) => p,
        None => return false,
    };
    let obj = obj.to_string_lossy().into_owned();
    // Fresh clsact qdisc (ignore failure: may not exist yet).
    let _ = run(&["tc", "qdisc", "del", "dev", iface, "clsact"]);
    if !run(&["tc", "qdisc", "add", "dev", iface, "clsact"]) {
        return false;
    }
    let ok_tx = run(&["tc", "filter", "add", "dev", iface, "ingress", "bpf", "da", "obj", &obj, "sec", "tc/tx"]);
    let ok_rx = run(&["tc", "filter", "add", "dev", iface, "egress", "bpf", "da", "obj", &obj, "sec", "tc/rx"]);
    ok_tx && ok_rx
}

/// Remove the classifiers from `iface`. Best-effort.
pub fn detach<F>(iface: &str, run: F)
where
    F: Fn(&[&str]) -> bool,
{
    let _ = run(&["tc", "qdisc", "del", "dev", iface, "clsact"]);
}

/// Read the flow map as `IP → Flow`. Uses `bpftool map dump name <map> -j`.
/// The `run_capture` closure returns the command's stdout (so the holder can
/// run it inside the infra netns). Returns an empty map if the datapath isn't
/// loaded (map absent) — never an error.
pub fn flows<F>(run_capture: F) -> HashMap<Ipv4Addr, Flow>
where
    F: Fn(&[&str]) -> Option<String>,
{
    let mut out = HashMap::new();
    let json = match run_capture(&["bpftool", "-j", "map", "dump", "name", MAP_NAME]) {
        Some(j) => j,
        None => return out,
    };
    let val: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(_) => return out,
    };
    // `bpftool -j` renders key and value as arrays of hex-byte strings
    // (["0x0a","0xdb",…]). The key is the 4-byte IPv4 in network order; the
    // value is `struct flow` = 4 × u64 little-endian in field order
    // (rx_packets, rx_bytes, tx_packets, tx_bytes).
    if let Some(entries) = val.as_array() {
        for e in entries {
            let key = e.get("key").and_then(bytes_of);
            let value = e.get("value").and_then(bytes_of);
            if let (Some(k), Some(v)) = (key, value) {
                if k.len() < 4 || v.len() < 32 {
                    continue;
                }
                let ip = Ipv4Addr::new(k[0], k[1], k[2], k[3]);
                out.insert(
                    ip,
                    Flow {
                        rx_packets: le_u64(&v[0..8]),
                        rx_bytes: le_u64(&v[8..16]),
                        tx_packets: le_u64(&v[16..24]),
                        tx_bytes: le_u64(&v[24..32]),
                    },
                );
            }
        }
    }
    out
}

/// Parse a bpftool byte array (`["0x0a", …]`) into raw bytes.
fn bytes_of(v: &serde_json::Value) -> Option<Vec<u8>> {
    let arr = v.as_array()?;
    arr.iter()
        .map(|b| {
            let s = b.as_str()?;
            let s = s.strip_prefix("0x").unwrap_or(s);
            u8::from_str_radix(s, 16).ok()
        })
        .collect()
}

fn le_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_u32_le_bytes_is_the_dotted_quad() {
        // 172.31.9.2 stored network-order, read by bpftool as LE u32 = 34152364.
        let ip = Ipv4Addr::from((34152364u32).to_le_bytes());
        assert_eq!(ip, Ipv4Addr::new(172, 31, 9, 2));
    }
}
