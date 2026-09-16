//! Linux capabilities: the name↔number table, the default kept set, and the
//! resolution of `--cap-drop`/`--cap-add` into a mask.
//!
//! Public because it is the SINGLE source of truth for the whole workspace: the
//! CRI's capability ceiling (`delonix-cri`'s `cap_ceiling`) has to reason about
//! exactly the set this engine would grant, and a second name↔number table on
//! that side would drift the day a capability is added here — the same
//! generator-and-reader-share-the-format discipline as `fw_rule_tail` on the
//! network side.

/// The capabilities the container MAY keep (Docker's model, minus
/// `CAP_MKNOD` — without a device cgroup, this is how we prevent access to host
/// disks). Everything else is dropped.
pub const KEPT_CAPS: &[u8] = &[
    0,  // CHOWN
    1,  // DAC_OVERRIDE
    3,  // FOWNER
    4,  // FSETID
    5,  // KILL
    6,  // SETGID
    7,  // SETUID
    8,  // SETPCAP
    10, // NET_BIND_SERVICE
    11, // NET_BROADCAST
    13, // NET_RAW
    18, // SYS_CHROOT
    29, // AUDIT_WRITE
    31, // SETFCAP
];

/// Capability number from the name (`CAP_NET_ADMIN` or `NET_ADMIN`).
pub fn cap_num(name: &str) -> Option<u8> {
    let n = name.trim().to_ascii_uppercase();
    let n = n.strip_prefix("CAP_").unwrap_or(&n);
    Some(match n {
        "CHOWN" => 0,
        "DAC_OVERRIDE" => 1,
        "DAC_READ_SEARCH" => 2,
        "FOWNER" => 3,
        "FSETID" => 4,
        "KILL" => 5,
        "SETGID" => 6,
        "SETUID" => 7,
        "SETPCAP" => 8,
        "LINUX_IMMUTABLE" => 9,
        "NET_BIND_SERVICE" => 10,
        "NET_BROADCAST" => 11,
        "NET_ADMIN" => 12,
        "NET_RAW" => 13,
        "IPC_LOCK" => 14,
        "IPC_OWNER" => 15,
        "SYS_MODULE" => 16,
        "SYS_RAWIO" => 17,
        "SYS_CHROOT" => 18,
        "SYS_PTRACE" => 19,
        "SYS_PACCT" => 20,
        "SYS_ADMIN" => 21,
        "SYS_BOOT" => 22,
        "SYS_NICE" => 23,
        "SYS_RESOURCE" => 24,
        "SYS_TIME" => 25,
        "SYS_TTY_CONFIG" => 26,
        "MKNOD" => 27,
        "LEASE" => 28,
        "AUDIT_WRITE" => 29,
        "AUDIT_CONTROL" => 30,
        "SETFCAP" => 31,
        "MAC_OVERRIDE" => 32,
        "MAC_ADMIN" => 33,
        "SYSLOG" => 34,
        "WAKE_ALARM" => 35,
        "BLOCK_SUSPEND" => 36,
        "AUDIT_READ" => 37,
        "PERFMON" => 38,
        "BPF" => 39,
        "CHECKPOINT_RESTORE" => 40,
        _ => return None,
    })
}

/// Mask with ALL capabilities supported by the kernel (`--privileged`).
/// Reads `/proc/sys/kernel/cap_last_cap` so as not to pass invalid bits to `capset`
/// (which would give EINVAL). Conservative fallback: CAP_CHECKPOINT_RESTORE (40).
pub fn all_caps_mask() -> u64 {
    let last: u32 = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(40);
    let last = last.min(63);
    if last >= 63 {
        u64::MAX
    } else {
        (1u64 << (last + 1)) - 1
    }
}

/// Computes the mask of capabilities to keep: starts at [`KEPT_CAPS`], applies
/// `--cap-drop` (`ALL` → none) and then `--cap-add`.
pub fn resolve_cap_keep(cap_drop: &[String], cap_add: &[String]) -> u64 {
    let mut keep: u64 = if cap_drop.iter().any(|c| c.eq_ignore_ascii_case("all")) {
        0
    } else {
        let mut m = 0u64;
        for &c in KEPT_CAPS {
            m |= 1u64 << c;
        }
        for c in cap_drop {
            if let Some(n) = cap_num(c) {
                m &= !(1u64 << n);
            }
        }
        m
    };
    // `--cap-add ALL` (docker) / the CRI translation of `privileged` → keeps ALL
    // capabilities. Without this branch, `cap_num("ALL")` returned `None` and `ALL` was
    // silently ignored — a "privileged" container via CRI ended up without
    // CAP_SYS_ADMIN (e.g. `sethostname` gave EPERM even though CRI requested it).
    if cap_add.iter().any(|c| c.eq_ignore_ascii_case("all")) {
        return all_caps_mask();
    }
    for c in cap_add {
        if let Some(n) = cap_num(c) {
            keep |= 1u64 << n;
        }
    }
    keep
}

/// Canonical name (without the `CAP_` prefix) of a capability number — the
/// inverse of [`cap_num`]. Used to render a mask back into the `--cap-add`
/// arguments the engine accepts, and to name capabilities in policy errors.
///
/// `None` for a number this build does not know (a kernel newer than the table):
/// deliberately not a synthetic `CAP_38`-style string, because such a name would
/// travel into an argv that [`cap_num`] would then silently drop.
pub fn cap_name(n: u8) -> Option<&'static str> {
    Some(match n {
        0 => "CHOWN",
        1 => "DAC_OVERRIDE",
        2 => "DAC_READ_SEARCH",
        3 => "FOWNER",
        4 => "FSETID",
        5 => "KILL",
        6 => "SETGID",
        7 => "SETUID",
        8 => "SETPCAP",
        9 => "LINUX_IMMUTABLE",
        10 => "NET_BIND_SERVICE",
        11 => "NET_BROADCAST",
        12 => "NET_ADMIN",
        13 => "NET_RAW",
        14 => "IPC_LOCK",
        15 => "IPC_OWNER",
        16 => "SYS_MODULE",
        17 => "SYS_RAWIO",
        18 => "SYS_CHROOT",
        19 => "SYS_PTRACE",
        20 => "SYS_PACCT",
        21 => "SYS_ADMIN",
        22 => "SYS_BOOT",
        23 => "SYS_NICE",
        24 => "SYS_RESOURCE",
        25 => "SYS_TIME",
        26 => "SYS_TTY_CONFIG",
        27 => "MKNOD",
        28 => "LEASE",
        29 => "AUDIT_WRITE",
        30 => "AUDIT_CONTROL",
        31 => "SETFCAP",
        32 => "MAC_OVERRIDE",
        33 => "MAC_ADMIN",
        34 => "SYSLOG",
        35 => "WAKE_ALARM",
        36 => "BLOCK_SUSPEND",
        37 => "AUDIT_READ",
        38 => "PERFMON",
        39 => "BPF",
        40 => "CHECKPOINT_RESTORE",
        _ => return None,
    })
}

/// Mask of the default kept set ([`KEPT_CAPS`]) — what a container gets when the
/// caller asks for nothing.
pub fn default_kept_mask() -> u64 {
    KEPT_CAPS.iter().fold(0u64, |m, &c| m | (1u64 << c))
}

/// The names of every capability set in `mask`, in ascending capability number
/// (stable order, so the argv this feeds is reproducible). Bits without a known
/// name are skipped — see [`cap_name`].
pub fn names_from_mask(mask: u64) -> Vec<&'static str> {
    (0u8..64)
        .filter(|c| (mask >> c) & 1 == 1)
        .filter_map(cap_name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cap_name` and `cap_num` are two hand-written tables that MUST agree: the
    /// ceiling renders a mask with one and the engine parses it back with the
    /// other, so a single mismatched entry would silently drop a capability the
    /// operator explicitly allowed.
    #[test]
    fn cap_name_e_cap_num_sao_inversas_uma_da_outra() {
        for n in 0u8..=40 {
            let name = cap_name(n).unwrap_or_else(|| panic!("cap {n} sem nome"));
            assert_eq!(cap_num(name), Some(n), "round-trip falhou para {name}");
            assert_eq!(
                cap_num(&format!("CAP_{name}")),
                Some(n),
                "prefixo CAP_ devia ser aceite para {name}"
            );
        }
        assert_eq!(cap_name(41), None, "41 ainda não está na tabela");
    }

    #[test]
    fn names_from_mask_e_ordenada_e_ignora_bits_sem_nome() {
        let mask = (1u64 << 21) | (1u64 << 0) | (1u64 << 63);
        assert_eq!(names_from_mask(mask), vec!["CHOWN", "SYS_ADMIN"]);
    }

    #[test]
    fn default_kept_mask_bate_com_kept_caps() {
        let m = default_kept_mask();
        for &c in KEPT_CAPS {
            assert_eq!((m >> c) & 1, 1);
        }
        assert_eq!((m >> 21) & 1, 0, "SYS_ADMIN nunca está no default");
    }

    /// A mask rendered into names and fed back as `--cap-drop ALL --cap-add <…>`
    /// has to resolve to the SAME mask — this is the round-trip the CRI ceiling
    /// depends on to clamp without reimplementing the resolution.
    #[test]
    fn mask_sobrevive_a_ida_e_volta_por_drop_all_mais_add() {
        let want = default_kept_mask() | (1u64 << 12); // + NET_ADMIN
        let add: Vec<String> = names_from_mask(want)
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(resolve_cap_keep(&["ALL".to_string()], &add), want);
    }
}
