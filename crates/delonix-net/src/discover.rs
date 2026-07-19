//! `discover` — auto-descoberta das **portas internas** (do entrypoint) de uma
//! carga, lendo os sockets em escuta do seu *network namespace* através de
//! `/proc/<pid>/net/{tcp,tcp6,udp,udp6}`.
//!
//! É **namespace-aware sem entrar no netns**: `/proc/<pid>/net/*` reflecte sempre
//! o netns do processo `pid` (host-visível). Logo, a partir do processo da API
//! (mesmo utilizador, rootless) lê-se directamente a tabela de sockets do
//! container — sem `nsenter`, sem passar pelo holder. Para um **pod**, basta ler
//! o `pid` do infra ("pause"): os membros partilham o netns, portanto as suas
//! portas aparecem todas na mesma tabela.
//!
//! O resultado alimenta o auto-mapping: cada porta descoberta vira um candidato a
//! regra de firewall/ingress na Console, que o operador pode **publicar** (DNAT
//! pelo ingress) com um clique. A descoberta é puramente observacional —
//! best-effort e nunca falha: se não conseguir ler, devolve vazio.

use serde::{Deserialize, Serialize};

/// Uma porta interna descoberta em escuta no netns de uma carga.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPort {
    /// `tcp` ou `udp`.
    pub proto: String,
    /// Porta local em escuta (1..=65535).
    pub port: u16,
    /// Nome do processo a escutar (C1, "se possível"): mapeado do inode do socket
    /// para o `comm` do processo na mesma netns. `None` se não resolvido.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// Lê as portas em escuta no netns do processo `pid` (host-visível). Junta IPv4 e
/// IPv6 (deduplicado por `(proto, porta)`) e ordena por porta. Best-effort: um
/// ficheiro ilegível (processo morto, sem permissões) é simplesmente saltado.
pub fn discover_ports(pid: i32) -> Vec<DiscoveredPort> {
    // (proto, porta) -> inode do socket em escuta. Dedup por (proto, porta).
    let mut seen: std::collections::BTreeMap<(String, u16), u64> =
        std::collections::BTreeMap::new();
    // (ficheiro, proto, só-LISTEN). TCP conta apenas o estado LISTEN (0A); UDP não
    // tem "listen", por isso conta os sockets ligados (sem extremo remoto).
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
    // mapa inode -> nome do processo (comm) dos processos na MESMA netns que `pid`.
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

/// Mapa `inode do socket -> comm` dos processos que partilham a NETNS de `pid` (os
/// processos da carga). Best-effort: lê o ns/net alvo, varre `/proc/<p>` com a mesma
/// netns e mapeia os fds `socket:[inode]` → `comm`. `BTreeMap` vazio se nada resolver.
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
        // só processos na MESMA netns que a carga.
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
                    // formato "socket:[12345]"
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

/// Parser PURO de uma tabela `/proc/net/{tcp,tcp6,udp,udp6}`: devolve as portas
/// locais em escuta. Formato (após o cabeçalho):
/// `sl  local_address rem_address st tx_queue ...` — onde `local_address` é
/// `HEXIP:HEXPORT` e `st` é o estado (`0A` = TCP_LISTEN).
///
/// - `listen_only = true`  (TCP): só conta sockets em estado `0A` (LISTEN).
/// - `listen_only = false` (UDP): conta sockets ligados a uma porta com extremo
///   remoto vazio (`rem_port == 0`) — i.e. servidores, não clientes conectados.
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
        // inode do socket = 10.ª coluna (índice 9) em /proc/net/{tcp,udp}.
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

    // Amostra real de /proc/net/tcp: um servidor em LISTEN na :8080 (1F90), uma
    // ligação ESTABLISHED (01) na :8080 que NÃO deve contar como nova porta, e um
    // socket em LISTEN na :22 (0016).
    const TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000 100 0 0 10 0
   1: 0100007F:1F90 0200000A:D7F2 01 00000000:00000000 00:00000000 00000000  1000        0 23456 1 0000 20 4 30 10 -1
   2: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 34567 1 0000 100 0 0 10 0
";

    // UDP: um servidor ligado na :53 (0035, sem remoto) e um cliente conectado
    // na :C001 (porta remota != 0) que NÃO deve contar.
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
        assert_eq!(p, vec![22, 8080]); // 0016, 1F90 — a ligação ESTABLISHED é ignorada
    }

    #[test]
    fn udp_lists_only_bound_servers() {
        let p = ports_only(parse_listen_ports(UDP, false));
        assert_eq!(p, vec![53]); // 0035; o cliente conectado (rem != 0) é ignorado
    }

    #[test]
    fn parses_socket_inode() {
        // a 1.ª linha LISTEN do TCP tem inode 12345 (col 9).
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
