//! `infra` — gestor do **netns de infra-estrutura** do ingress rootless (Fase 1).
//!
//! Substitui (a prazo) o modelo `1 slirp4netns por container` por um **ingress
//! único**: um netns de infra partilhado, com a bridge `delonix0` lá dentro, UM
//! só `slirp4netns` como ponte host↔infra, e o NAT/DNAT em `nft` DENTRO do netns.
//! Os containers ligam-se por `veth` à `delonix0` (Fase 3) e as portas publicam-se
//! via `add_hostfwd` + DNAT (Fase 4). Esta fase entrega só o **gestor**: arrancar,
//! observar e derrubar a infra, com *ref-count* de ciclo de vida.
//!
//! **Porque é rootless:** um não-root é root DENTRO do seu próprio user+network
//! namespace → tem `CAP_NET_ADMIN` lá e pode criar bridge e regras `nft`. O netns
//! vive enquanto o processo *holder* viver; descobre-se pelo PID (host-visível).
//!
//! **Gotcha conhecido:** NÃO se consegue `nsenter --user --net` a partir do host
//! (dá `setgroups: Operation not permitted`). Logo toda a configuração DENTRO do
//! netns é feita pelo próprio holder (já é root no userns) — daí o re-exec do
//! binário para [`holder_main`].

use crate::{run, run_ok, SLIRP_IP};
use delonix_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Bridge dentro do netns de infra (mesmo nome do modelo root; não colide por
/// estar noutro netns).
pub const INFRA_BRIDGE: &str = "delonix0";
/// Gateway/IP da bridge no netns de infra.
pub const INFRA_GATEWAY: &str = "10.200.0.1";
/// CIDR da bridge no netns de infra (os containers ficam em `10.200.x/16`).
pub const INFRA_CIDR: &str = "10.200.0.1/16";
/// Prefixo `/16` da subnet de infra (para validar IPs de container).
pub const INFRA_PREFIX: &str = "10.200";
/// Subnet do `tap0` do slirp único (o seu lado host↔infra), alvo do masquerade.
pub const INFRA_TAP_SUBNET: &str = "10.0.2.0/24";
/// Tabela `nft` do ingress, VIVE DENTRO do netns de infra (distinta da `delonix`
/// do modo root, que vive no netns do host).
pub const INGRESS_TABLE: &str = "dlxing";

// ---- localização dos artefactos (pidfiles, socket, status, refcount) --------

/// Raiz de dados do Delonix, SEM depender de `geteuid()` quando `DELONIX_ROOT`
/// está definido — crucial porque o holder corre com uid mapeado a 0 no userns
/// (senão resolveria para `/var/lib/delonix` em vez do armazém do utilizador). O
/// pai passa sempre `DELONIX_ROOT` ao holder para os caminhos baterem certo.
fn base_root() -> PathBuf {
    if let Some(root) = std::env::var_os("DELONIX_ROOT") {
        return PathBuf::from(root);
    }
    // SAFETY: geteuid() não tem pré-condições.
    if unsafe { libc::geteuid() } != 0 {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."));
        return base.join("delonix");
    }
    PathBuf::from("/var/lib/delonix")
}

/// Diretório `<base>/ingress/` com o estado da infra.
fn ingress_dir() -> PathBuf {
    base_root().join("ingress")
}
fn holder_pid_path() -> PathBuf {
    ingress_dir().join("holder.pid")
}
fn slirp_pid_path() -> PathBuf {
    ingress_dir().join("slirp.pid")
}
/// O api-socket do slirp único (onde se pedem os `add_hostfwd` na Fase 4).
pub fn slirp_sock_path() -> PathBuf {
    ingress_dir().join("slirp.sock")
}
/// Socket de controlo do holder (fábrica de netns/veth): o host pede attach/detach.
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

// ---- helpers de processo/pid -------------------------------------------------

/// `true` se o processo `pid` ainda existe (via `/proc/<pid>`).
fn pid_alive(pid: i32) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse::<i32>().ok()
}

/// Envia `SIGTERM` a um pid e remove o seu pidfile.
fn kill_pidfile(path: &Path) {
    if let Some(pid) = read_pid(path) {
        if pid_alive(pid) {
            // SAFETY: kill() com um pid válido; ignoramos o resultado (best-effort).
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
    let _ = std::fs::remove_file(path);
}

// ---- nft do ingress (dentro do netns de infra) ------------------------------

/// O *ruleset* `nft` BASE do ingress: cadeia `pre` (DNAT das portas publicadas),
/// `post` (masquerade do tap0) e `fwd` (FILTRO de forward — o ÚNICO sítio do
/// firewall parametrizável, com chains por-container chamadas por jump). PURA.
pub fn ingress_table_ruleset() -> String {
    format!(
        "table ip {INGRESS_TABLE} {{\n\
         \x20 chain pre {{ type nat hook prerouting priority -100; }}\n\
         \x20 chain post {{ type nat hook postrouting priority 100; oifname \"tap0\" masquerade; }}\n\
         \x20 chain forward {{ type filter hook forward priority 0; }}\n\
         }}\n"
    )
}

// ---- ref-count (ciclo de vida partilhado pelos containers, Fase 3) ----------

/// Próximo valor do contador dado o atual e o passo (`+1` no acquire, `-1` no
/// release), nunca abaixo de 0. PURA — o coração testável do *ref-count*.
pub fn next_refcount(current: i64, delta: i64) -> i64 {
    (current + delta).max(0)
}

fn read_refcount() -> i64 {
    std::fs::read_to_string(refcount_path())
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

fn write_refcount(n: i64) {
    let _ = std::fs::create_dir_all(ingress_dir());
    let _ = std::fs::write(refcount_path(), n.to_string());
}

/// Trinco exclusivo de ficheiro (`flock`) à volta das operações de ref-count, para
/// `acquire`/`release` concorrentes (vários `run` em paralelo) não correrem em
/// cima um do outro. Devolve o fd; o `Drop` liberta-o.
struct FileLock(i32);
impl FileLock {
    fn acquire() -> FileLock {
        let _ = std::fs::create_dir_all(ingress_dir());
        let path = lock_path();
        let c = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes().to_vec())
            .unwrap_or_else(|_| std::ffi::CString::new("/tmp/dlxlock").unwrap());
        // SAFETY: open/flock com caminho válido; -1 em falha trata-se a seguir.
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
            // SAFETY: fd próprio, aberto em acquire().
            unsafe {
                libc::flock(self.0, libc::LOCK_UN);
                libc::close(self.0);
            }
        }
    }
}

/// Incrementa o ref-count e garante a infra de pé no 1.º utilizador. Chamar uma
/// vez por container que entra na rede de ingress (Fase 3).
pub fn acquire() -> Result<()> {
    let _lock = FileLock::acquire();
    ensure_up()?; // idempotente — robusto mesmo com ref-count stale
    write_refcount(next_refcount(read_refcount(), 1));
    Ok(())
}

/// Decrementa o ref-count e derruba a infra quando o último utilizador sai.
pub fn release() {
    let _lock = FileLock::acquire();
    let n = next_refcount(read_refcount(), -1);
    write_refcount(n);
    if n == 0 {
        teardown();
    }
}

// ---- estado / observação ----------------------------------------------------

/// Estado observável da infra de ingress (para `ingress status` e a Console).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct InfraStatus {
    /// PID host-visível do holder do netns (vivo enquanto a infra existe).
    pub holder_pid: Option<i32>,
    /// PID do `slirp4netns` único (a ponte host↔infra).
    pub slirp_pid: Option<i32>,
    /// `true` se holder E slirp estão vivos.
    pub up: bool,
    pub bridge: String,
    pub gateway: String,
    /// Contador de containers a usar a infra (ref-count).
    pub refcount: i64,
}

/// Lê o estado atual a partir dos pidfiles (sem tocar no kernel).
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

// ---- arranque / derrube -----------------------------------------------------

/// Garante a infra de pé (holder + bridge + slirp único). **Idempotente**: se já
/// estiver tudo vivo, não faz nada. É o ponto de entrada do gestor.
pub fn ensure_up() -> Result<()> {
    let st = status();
    if st.up {
        return Ok(());
    }
    // estado parcial (ex.: holder morto) → limpa antes de recriar.
    teardown();
    std::fs::create_dir_all(ingress_dir()).map_err(|e| Error::Runtime {
        context: "ingress dir",
        message: e.to_string(),
    })?;
    let holder_pid = start_holder()?;
    if let Err(e) = start_slirp(holder_pid) {
        // se o slirp falha, não deixamos um holder órfão.
        teardown();
        return Err(e);
    }
    Ok(())
}

/// Derruba a infra: mata o slirp e o holder (o que liberta o netns) e limpa os
/// artefactos. Best-effort e idempotente.
pub fn teardown() {
    // os servidores DHCP/DNS/RA são threads do holder — morrem ao matá-lo.
    kill_pidfile(&slirp_pid_path());
    kill_pidfile(&holder_pid_path());
    let _ = std::fs::remove_file(slirp_sock_path());
    let _ = std::fs::remove_file(control_sock_path());
    let _ = std::fs::remove_file(status_path());
    write_refcount(0); // estado limpo — evita ref-count stale a saltar o ensure_up
}

/// Arranca o **holder**: re-exec do próprio binário dentro de `unshare
/// --user --map-root-user --net --mount`, que corre [`holder_main`] (root no
/// userns) para montar a `delonix0` + `nft` e depois bloquear. Espera o ficheiro
/// de estado "ready" antes de devolver o PID host-visível.
fn start_holder() -> Result<i32> {
    let exe = std::env::current_exe().map_err(|e| Error::Runtime {
        context: "current_exe",
        message: e.to_string(),
    })?;
    let _ = std::fs::remove_file(status_path());
    // `--map-auto` mapeia TODA a gama subuid/subgid do utilizador (/etc/subuid),
    // não só o root: imagens reais (nginx uid 101, postgres, …) precisam de chown
    // para uids != 0 DENTRO do container, que assim ficam mapeáveis. `--map-root-user`
    // mapeia o uid 0 do userns → o uid do utilizador no host.
    let child = Command::new("unshare")
        .args(["--user", "--map-auto", "--map-root-user", "--net", "--mount", "--"])
        .arg(&exe)
        .args(["netns", "holder"])
        // o holder corre com uid->0 no userns; força os caminhos para a base real.
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
    // o holder segue vivo durante toda a vida da infra — não lhe fazemos wait.
    std::mem::forget(child);

    // espera o holder sinalizar "ready" (ou erro) no ficheiro de estado (~5s).
    for _ in 0..100 {
        if !pid_alive(pid) {
            teardown();
            return Err(Error::Runtime {
                context: "ingress holder",
                message: "o holder do netns morreu ao arrancar".into(),
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
        message: "timeout à espera do holder do netns".into(),
    })
}

/// Arranca o **slirp único** ligado ao netns do holder (`tap0`), com api-socket
/// para os `add_hostfwd` da Fase 4. Espera o `--ready-fd` antes de devolver.
fn start_slirp(holder_pid: i32) -> Result<()> {
    let sock = slirp_sock_path();
    let _ = std::fs::remove_file(&sock);
    let mut fds = [0i32; 2];
    // SAFETY: pipe() preenche 2 fds; -1 em falha trata-se a seguir.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(Error::Runtime { context: "pipe", message: "slirp ready-fd".into() });
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
    // SAFETY: o pai fecha a sua cópia de escrita; só o slirp a mantém aberta.
    unsafe { libc::close(wr) };
    match spawned {
        Ok(child) => {
            let mut b = [0u8; 1];
            // SAFETY: lê 1 byte do read-end; bloqueia até o slirp sinalizar pronto.
            unsafe {
                libc::read(rd, b.as_mut_ptr() as *mut libc::c_void, 1);
                libc::close(rd);
            }
            let _ = std::fs::write(slirp_pid_path(), (child.id() as i32).to_string());
            // o slirp vive durante toda a vida da infra — não lhe fazemos wait.
            std::mem::forget(child);
            Ok(())
        }
        Err(e) => {
            // SAFETY: fecha o read-end no erro.
            unsafe { libc::close(rd) };
            Err(Error::Runtime { context: "slirp4netns", message: e.to_string() })
        }
    }
}

// ---- corpo do holder (corre DENTRO do user+net+mount namespace) -------------

/// Ponto de entrada do **holder** (invocado por `delonix netns holder`, oculto).
/// Corre como root no userns/netns recém-criados: monta a `delonix0`, liga o
/// `ip_forward`, instala a tabela `nft` de ingress, ABRE o socket de controlo,
/// escreve "ready" e **serve** pedidos de attach/detach de containers (a fábrica
/// de netns/veth). O netns vive enquanto este processo viver; SIGTERM (teardown)
/// mata-o → o kernel liberta o netns. Em falha de arranque escreve `err:<msg>`.
pub fn holder_main() -> ! {
    let started = setup_infra_netns().and_then(|_| {
        let _ = std::fs::remove_file(control_sock_path());
        std::os::unix::net::UnixListener::bind(control_sock_path()).map_err(|e| Error::Runtime {
            context: "control socket",
            message: e.to_string(),
        })
    });
    match started {
        Ok(listener) => {
            // servidor DNS do ingress numa thread (resolve nomes de containers/VMs).
            std::thread::spawn(dns_server_main);
            // emissor de Router Advertisements (SLAAC IPv6 para VMs/containers).
            std::thread::spawn(ra_sender_main);
            // só agora sinalizamos pronto — o socket de controlo já aceita ligações.
            write_status("ready");
            control_loop(listener); // nunca retorna (até SIGTERM)
        }
        Err(e) => {
            write_status(&format!("err: {e}"));
            std::process::exit(1);
        }
    }
}

/// Aceita ligações no socket de controlo e serve um comando por ligação (a fábrica
/// de netns/veth). Corre DENTRO do holder, logo as operações `ip`/`ip netns` ficam
/// no netns de infra sem `nsenter`. Síncrono (um attach de cada vez — suficiente).
fn control_loop(listener: std::os::unix::net::UnixListener) -> ! {
    use std::io::{BufRead, BufReader, Write};
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            continue;
        }
        let reply = handle_control(line.trim());
        let _ = stream.write_all(reply.as_bytes());
    }
    std::process::exit(0);
}

/// Despacha um comando de controlo (`attach <netns> <ip>`, `detach <netns>`,
/// `ping`) e devolve a resposta (`ok\n` ou `err: <msg>\n`).
fn handle_control(line: &str) -> String {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let res = match parts.as_slice() {
        ["ping"] => Ok(()),
        ["attach", netns, ip, bridge, gateway] => do_attach(netns, ip, bridge, gateway),
        ["detach", netns] => do_detach(netns),
        ["netdel", bridge] => do_netdel(bridge),
        ["vmtap", tap, bridge, gateway] => do_vmtap(tap, bridge, gateway),
        ["vmtapdel", tap] => do_vmtapdel(tap),
        ["publish", proto, host_port, cip, cport] => do_publish(proto, host_port, cip, cport),
        ["unpublish", host_port] => do_unpublish(host_port),
        ["firewall", _netns, ip, hex] => do_firewall(ip, hex),
        ["unfirewall", ip] => do_unfirewall(ip),
        _ => Err(Error::Invalid(format!("comando de controlo inválido: {line:?}"))),
    };
    match res {
        Ok(()) => "ok\n".to_string(),
        Err(e) => format!("err: {e}\n"),
    }
}

/// Garante a BRIDGE de uma rede no netns de infra (o gateway é SEMPRE o ingress):
/// cria `<bridge>` com `<gateway>/16` se faltar, e ISOLA-a das outras bridges
/// delonix (forward drop entre redes, como o docker) — mas o egress (oifname tap0)
/// e a comunicação intra-rede mantêm-se. Idempotente.
fn ensure_net_bridge(bridge: &str, gateway: &str) -> Result<()> {
    let exists = crate::capture("ip", &["link", "show", bridge])
        .map(|o| o.contains(bridge))
        .unwrap_or(false);
    if !exists {
        run("ip", &["link", "add", bridge, "type", "bridge"])?;
        run("ip", &["addr", "add", &format!("{gateway}/16"), "dev", bridge])?;
        run("ip", &["link", "set", bridge, "up"])?;
        // IPv6 (ULA): gateway na bridge + forwarding v6 (best-effort).
        let p = prefix_of(gateway);
        run_ok("ip", &["-6", "addr", "add", &format!("{}/64", v6_gw(&p)), "dev", bridge]);
        let _ = std::fs::write("/proc/sys/net/ipv6/conf/all/forwarding", "1");
    }
    // isolamento entre redes: drop forward entre esta bridge e as outras delonix.
    let listed = crate::capture("ip", &["-o", "link", "show", "type", "bridge"]).unwrap_or_default();
    let fwd = crate::capture("nft", &["list", "chain", "ip", INGRESS_TABLE, "forward"]).unwrap_or_default();
    for line in listed.lines() {
        let other = line.split(':').nth(1).map(|s| s.trim().split('@').next().unwrap_or("").trim()).unwrap_or("");
        if other.is_empty() || other == bridge || (other != INFRA_BRIDGE && !other.starts_with("dlxn")) {
            continue; // só isolar contra delonix0 e outras redes dlxn*
        }
        for (a, b) in [(bridge, other), (other, bridge)] {
            let needle = format!("iifname \"{a}\" oifname \"{b}\" drop");
            if !fwd.contains(&needle) {
                run_ok("nft", &["add", "rule", "ip", INGRESS_TABLE, "forward", "iifname", a, "oifname", b, "drop"]);
            }
        }
    }
    // servidor DHCP da rede (para VMs/clientes que peçam IP).
    start_dhcp(bridge, &prefix_of(gateway));
    Ok(())
}

/// Bridges que já têm o servidor DHCP nativo a correr (uma thread por bridge).
static DHCP_STARTED: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Arranca o servidor DHCP **NATIVO** (Rust) da bridge de uma rede, se ainda não
/// estiver a correr. Substitui o `busybox udhcpd` — o holder fica self-contained
/// (sem dependência de binários do host). Uma thread por bridge.
fn start_dhcp(bridge: &str, prefix: &str) {
    {
        let mut s = DHCP_STARTED.lock().unwrap();
        if !s.insert(bridge.to_string()) {
            return; // já tem servidor DHCP
        }
    }
    let (b, p) = (bridge.to_string(), prefix.to_string());
    std::thread::spawn(move || dhcp_serve(b, p));
}

/// Servidor DHCPv4 nativo de uma bridge: escuta UDP `:67` (só nessa bridge, via
/// `SO_BINDTODEVICE`) e responde a DISCOVER/REQUEST com um IP do pool
/// `<prefix>.254.10–.254.250` (determinístico do MAC), **gateway/DNS = ingress**.
fn dhcp_serve(bridge: String, prefix: String) {
    use std::os::unix::io::FromRawFd;
    let oct: Vec<u8> = prefix.split('.').filter_map(|x| x.parse().ok()).collect();
    if oct.len() != 2 {
        return;
    }
    let (o0, o1) = (oct[0], oct[1]);
    let gw = [o0, o1, 0, 1]; // gateway/server/DNS = <prefix>.0.1 (o ingress)
    // SAFETY: socket UDP; setsockopt REUSEADDR/PORT/BROADCAST/BINDTODEVICE; bind :67.
    let sock = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return;
        }
        let one: libc::c_int = 1;
        let so = |n| libc::setsockopt(fd, libc::SOL_SOCKET, n, &one as *const _ as *const libc::c_void, 4);
        so(libc::SO_REUSEADDR);
        so(libc::SO_REUSEPORT);
        so(libc::SO_BROADCAST);
        let bn = bridge.as_bytes();
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_BINDTODEVICE, bn.as_ptr() as *const libc::c_void, bn.len() as u32);
        let mut a: libc::sockaddr_in = std::mem::zeroed();
        a.sin_family = libc::AF_INET as u16;
        a.sin_port = 67u16.to_be();
        a.sin_addr.s_addr = 0; // INADDR_ANY
        if libc::bind(fd, &a as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_in>() as u32) != 0 {
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
        let macs = mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
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
        r.extend_from_slice(&gw); // DNS (o nosso servidor)
        r.push(255); // end
        let _ = sock.send_to(&r, "255.255.255.255:68");
    }
}

/// Extrai o valor de uma opção DHCP (TLV) do bloco de opções.
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

// ---- IPv6 (ULA) do ingress: fd00:<2.º octeto>::/64 por rede ----------------

/// Grupo IPv6 de uma rede a partir do prefixo `/16` (`10.201` → `201`).
fn v6_group(v4prefix: &str) -> String {
    v4prefix.rsplit('.').next().unwrap_or("200").to_string()
}
/// Gateway IPv6 (= ingress) de uma rede: `fd00:<grupo>::1`.
fn v6_gw(v4prefix: &str) -> String {
    format!("fd00:{}::1", v6_group(v4prefix))
}
/// IPv6 ULA determinístico de um IP v4 do ingress: `fd00:<o2>::<o3>:<o4>`.
fn v6_of(ip4: &str) -> Option<String> {
    let o: Vec<&str> = ip4.split('.').collect();
    if o.len() != 4 {
        return None;
    }
    Some(format!("fd00:{}::{}:{}", o[1], o[2], o[3]))
}

/// Prefixo `/16` (`10.x`) a partir de um IP/gateway (`10.x.y.z`).
fn prefix_of(ip: &str) -> String {
    let o: Vec<&str> = ip.split('.').collect();
    if o.len() >= 2 {
        format!("{}.{}", o[0], o[1])
    } else {
        INFRA_PREFIX.to_string()
    }
}

/// Cria o netns de um container e liga-o à BRIDGE da sua rede por `veth`: par
/// `<vh>`↔`eth0`, `vh` na bridge, `eth0` no netns com `<ip>/16` e rota default
/// pelo `<gateway>` (= o ingress). Cria a bridge da rede se faltar. Corre no holder.
fn do_attach(netns: &str, ip: &str, bridge: &str, gateway: &str) -> Result<()> {
    let netns = sanitize(netns);
    let bridge = sanitize(bridge);
    ensure_net_bridge(&bridge, gateway)?;
    let vh = vh_name(&netns);
    // limpa restos de uma tentativa anterior (best-effort).
    run_ok("ip", &["netns", "del", &netns]);
    run_ok("ip", &["link", "del", &vh]);
    run("ip", &["netns", "add", &netns])?;
    run("ip", &["link", "add", &vh, "type", "veth", "peer", "name", "eth0"])?;
    run("ip", &["link", "set", &vh, "master", &bridge])?;
    run("ip", &["link", "set", &vh, "up"])?;
    run("ip", &["link", "set", "eth0", "netns", &netns])?;
    let cidr = format!("{ip}/16");
    for argv in [
        vec!["netns", "exec", &netns, "ip", "link", "set", "lo", "up"],
        vec!["netns", "exec", &netns, "ip", "addr", "add", &cidr, "dev", "eth0"],
        vec!["netns", "exec", &netns, "ip", "link", "set", "eth0", "up"],
        vec!["netns", "exec", &netns, "ip", "route", "add", "default", "via", gateway],
    ] {
        run("ip", &argv)?;
    }
    // IPv6 (ULA) no eth0 + rota default v6 (best-effort; o host pode ter v6 off).
    let p = prefix_of(gateway);
    let gw6 = v6_gw(&p);
    if let Some(v6) = v6_of(ip) {
        let cidr6 = format!("{v6}/64");
        run_ok("ip", &["netns", "exec", &netns, "ip", "-6", "addr", "add", &cidr6, "dev", "eth0", "nodad"]);
        run_ok("ip", &["netns", "exec", &netns, "ip", "-6", "route", "add", "default", "via", &gw6]);
    }
    Ok(())
}

/// Remove o netns de um container (e, com ele, o `eth0`; o `vh` órfão é limpo a
/// seguir). Best-effort.
fn do_detach(netns: &str) -> Result<()> {
    let netns = sanitize(netns);
    let vh = vh_name(&netns);
    run_ok("ip", &["netns", "del", &netns]);
    run_ok("ip", &["link", "del", &vh]);
    Ok(())
}

/// Cria um `tap` para uma VM, ligado à BRIDGE da sua rede (cria a bridge + DHCP se
/// faltar). O QEMU (a correr no netns de infra) usa este tap; o guest obtém IP do
/// udhcpd da rede (gateway = ingress). Corre no holder.
fn do_vmtap(tap: &str, bridge: &str, gateway: &str) -> Result<()> {
    let tap = sanitize(tap);
    let bridge = sanitize(bridge);
    ensure_net_bridge(&bridge, gateway)?;
    run_ok("ip", &["link", "del", &tap]); // limpa restos
    run("ip", &["tuntap", "add", "dev", &tap, "mode", "tap"])?;
    run("ip", &["link", "set", &tap, "master", &bridge])?;
    run("ip", &["link", "set", &tap, "up"])?;
    Ok(())
}

/// Remove o `tap` de uma VM (no `vm rm`/stop). Best-effort.
fn do_vmtapdel(tap: &str) -> Result<()> {
    run_ok("ip", &["link", "del", &sanitize(tap)]);
    Ok(())
}

/// Remove a bridge de uma rede privada do netns de infra (no `network rm`).
fn do_netdel(bridge: &str) -> Result<()> {
    let bridge = sanitize(bridge);
    if bridge == INFRA_BRIDGE {
        return Err(Error::Invalid("a bridge default do ingress não se remove".into()));
    }
    run_ok("ip", &["link", "del", &bridge]);
    Ok(())
}

/// Instala o DNAT de uma porta publicada na chain `pre` do `dlxing` (corre no
/// holder): tráfego que chegou pelo slirp (`daddr` do tap) na `host_port` é
/// reescrito para `<cip>:<cport>`. Validações defensivas contra injeção no `nft`.
fn do_publish(proto: &str, host_port: &str, cip: &str, cport: &str) -> Result<()> {
    validate_publish(proto, host_port, cip, cport)?;
    run("nft", &[
        "add", "rule", "ip", INGRESS_TABLE, "pre",
        "ip", "daddr", SLIRP_IP, proto, "dport", host_port,
        "dnat", "to", &format!("{cip}:{cport}"),
    ])
}

/// Remove o DNAT de uma `host_port` (por handle) da chain `pre`. Best-effort.
fn do_unpublish(host_port: &str) -> Result<()> {
    if !is_port(host_port) {
        return Err(Error::Invalid(format!("porta inválida: {host_port}")));
    }
    // lista a chain com handles e apaga a(s) regra(s) que casam a dport.
    let listed = crate::capture("nft", &["-a", "list", "chain", "ip", INGRESS_TABLE, "pre"]).unwrap_or_default();
    let needle = format!("dport {host_port} ");
    for line in listed.lines() {
        if line.contains(&needle) {
            if let Some(handle) = line.rsplit("# handle ").nth(0).and_then(|h| h.trim().parse::<u32>().ok()) {
                run_ok("nft", &["delete", "rule", "ip", INGRESS_TABLE, "pre", "handle", &handle.to_string()]);
            }
        }
    }
    Ok(())
}

/// Valida os campos de um publish antes de os meter num comando `nft` (defesa
/// contra injeção): protocolo `tcp`/`udp`, portas numéricas, IP na subnet de infra.
fn validate_publish(proto: &str, host_port: &str, cip: &str, cport: &str) -> Result<()> {
    if proto != "tcp" && proto != "udp" {
        return Err(Error::Invalid(format!("protocolo inválido: {proto}")));
    }
    if !is_port(host_port) || !is_port(cport) {
        return Err(Error::Invalid("porta inválida (1..65535)".into()));
    }
    if !is_ingress_ip(cip) {
        return Err(Error::Invalid(format!("IP {cip} fora do espaço de ingress (10.200-254.x)")));
    }
    Ok(())
}

fn is_port(p: &str) -> bool {
    p.parse::<u16>().map(|n| n >= 1).unwrap_or(false)
}

/// `true` se `ip` é um endereço válido do ESPAÇO de ingress (`10.{200..=254}.x.x`,
/// unicast): a rede default (10.200) ou uma rede privada (10.201+). Defesa
/// anti-injeção sem fixar um único `/16`.
fn is_ingress_ip(ip: &str) -> bool {
    let o: Vec<&str> = ip.split('.').collect();
    if o.len() != 4 {
        return false;
    }
    let n: Vec<u16> = match o.iter().map(|x| x.parse::<u16>()).collect::<std::result::Result<_, _>>() {
        Ok(v) => v,
        Err(_) => return false,
    };
    if n.iter().any(|&x| x > 255) {
        return false;
    }
    n[0] == 10 && (200..=254).contains(&n[1]) && (n[2], n[3]) != (0, 0) && (n[2], n[3]) != (255, 255)
}

/// Nome do `veth` do lado da bridge para um netns (determinístico, <= 15 chars).
fn vh_name(netns: &str) -> String {
    format!("vh{:08x}", crate::fnv32(netns))
}

// ---- firewall PARAMETRIZÁVEL do ingress (o ÚNICO sítio — princípio do utilizador) ----

/// Nome da chain de firewall por-container no `dlxing` (derivado do IP).
fn fw_chain_name(ip: &str) -> String {
    format!("fw{:08x}", crate::fnv32(ip))
}

/// Gera o CORPO da chain de firewall de um container (regras L4 + política default),
/// no netns de infra. PURA — mesma semântica do modelo root (`apply_container_firewall`),
/// mas aplicada no ingress. `in` = tráfego PARA o container (daddr==ip); `out` = DELE
/// (saddr==ip); `src` casa o outro extremo (peer). Testável sem kernel.
pub fn fw_chain_body(ip: &str, fw: &delonix_core::ContainerFw) -> String {
    let mut body = String::new();
    if fw.enabled {
        for r in &fw.rules {
            let (self_dir, peer_dir) = if r.dir == "out" { ("saddr", "daddr") } else { ("daddr", "saddr") };
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
            line.push_str(if r.action == "allow" { " accept" } else { " drop" });
            body.push_str(&format!("\t\t{line}\n"));
        }
        if fw.policy_in == "deny" {
            body.push_str(&format!("\t\tip daddr {ip} drop\n"));
        }
        if fw.policy_out == "deny" {
            body.push_str(&format!("\t\tip saddr {ip} drop\n"));
        }
    }
    body
}

/// Aplica a firewall de um container no `dlxing` (corre no holder): garante a chain
/// `fw<hash>` + jumps no `fwd` (daddr/saddr==ip), e reconstrói o corpo. `hex` é o
/// JSON da `ContainerFw` em hexadecimal (o canal de controlo é por linhas).
fn do_firewall(ip: &str, hex: &str) -> Result<()> {
    if !is_ingress_ip(ip) {
        return Err(Error::Invalid(format!("IP {ip} fora do espaço de ingress (10.200-254.x)")));
    }
    let bytes = hex_decode(hex).ok_or_else(|| Error::Invalid("hex inválido".into()))?;
    let fw: delonix_core::ContainerFw =
        serde_json::from_slice(&bytes).map_err(|e| Error::Invalid(format!("firewall JSON: {e}")))?;
    let chain = fw_chain_name(ip);
    // garante a chain (regular, só alvo de jump).
    let exists = crate::capture("nft", &["list", "chain", "ip", INGRESS_TABLE, &chain])
        .map(|o| o.contains(&chain))
        .unwrap_or(false);
    if !exists {
        run_ok("nft", &["add", "chain", "ip", INGRESS_TABLE, &chain]);
    }
    // jumps idempotentes no fwd: tráfego PARA (daddr) e DE (saddr) o IP.
    let fwd_chain = crate::capture("nft", &["list", "chain", "ip", INGRESS_TABLE, "forward"]).unwrap_or_default();
    for dir in ["daddr", "saddr"] {
        if !fwd_chain.contains(&format!("ip {dir} {ip} jump {chain}")) {
            run_ok("nft", &["add", "rule", "ip", INGRESS_TABLE, "forward", "ip", dir, ip, "jump", &chain]);
        }
    }
    // flush + reconstrução do corpo num único script (mantém a chain e os jumps).
    let body = fw_chain_body(ip, &fw);
    let script = format!(
        "flush chain ip {INGRESS_TABLE} {chain}\ntable ip {INGRESS_TABLE} {{\n\tchain {chain} {{\n{body}\t}}\n}}\n"
    );
    apply_nft_stdin(&script)
}

/// Remove a firewall de um container do `dlxing`: tira os jumps do `fwd` (por
/// handle) e apaga a chain. Best-effort.
fn do_unfirewall(ip: &str) -> Result<()> {
    let chain = fw_chain_name(ip);
    if let Ok(out) = crate::capture("nft", &["-a", "list", "chain", "ip", INGRESS_TABLE, "forward"]) {
        for line in out.lines() {
            if line.contains(&format!("jump {chain}")) {
                if let Some(h) = line.rsplit("handle ").next().map(|s| s.trim()) {
                    run_ok("nft", &["delete", "rule", "ip", INGRESS_TABLE, "forward", "handle", h]);
                }
            }
        }
    }
    run_ok("nft", &["delete", "chain", "ip", INGRESS_TABLE, &chain]);
    Ok(())
}

/// Hex-encode (minúsculas) — para passar o JSON da firewall pelo canal de linhas.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Hex-decode; `None` se o comprimento for ímpar ou houver dígitos inválidos.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Sanitiza um nome de netns/interface (só `[a-z0-9_-]`, <= 12 chars) — defesa
/// contra injeção no `ip netns` e no IFNAMSIZ.
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    cleaned.chars().take(12).collect()
}

// ---- redes privadas do ingress (F6): bridge por rede, gateway = ingress -------

/// Definição de uma rede privada do ingress: nome, bridge (no netns de infra) e
/// prefixo `/16`. O **gateway é SEMPRE o ingress** (`<prefix>.0.1` na bridge), por
/// onde a rede sai/recebe (egress via o slirp único) e onde vive o firewall.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NetDef {
    pub name: String,
    pub bridge: String,
    pub prefix: String, // ex.: "10.201"
}

fn networks_dir() -> PathBuf {
    ingress_dir().join("networks")
}
fn netdef_path(name: &str) -> PathBuf {
    networks_dir().join(format!("{}.json", sanitize(name)))
}

/// Gateway (= ingress) de um prefixo `/16`.
fn gateway_of(prefix: &str) -> String {
    format!("{prefix}.0.1")
}

/// Resolve uma rede para `(bridge, prefix, gateway)`. `ingress`/vazio = a rede
/// default (delonix0/10.200); senão carrega a `NetDef` da rede privada.
pub fn resolve_net(name: &str) -> Result<(String, String, String)> {
    if name.is_empty() || name == "ingress" {
        return Ok((INFRA_BRIDGE.to_string(), INFRA_PREFIX.to_string(), INFRA_GATEWAY.to_string()));
    }
    let def = network_get(name).ok_or_else(|| Error::NotFound(format!("rede de ingress '{name}'")))?;
    let gw = gateway_of(&def.prefix);
    Ok((def.bridge, def.prefix, gw))
}

/// Lê a `NetDef` de uma rede privada (se existir).
pub fn network_get(name: &str) -> Option<NetDef> {
    serde_json::from_slice(&std::fs::read(netdef_path(name)).ok()?).ok()
}

/// Lista as redes privadas do ingress definidas.
pub fn network_list() -> Vec<NetDef> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(networks_dir()) {
        for e in rd.flatten() {
            if let Ok(def) = serde_json::from_slice::<NetDef>(&std::fs::read(e.path()).unwrap_or_default()) {
                v.push(def);
            }
        }
    }
    v
}

/// **Cria uma rede privada do ingress**: aloca um prefixo `/16` livre (10.201+,
/// evitando 10.200 e os já usados) e uma bridge, e persiste a `NetDef`. A bridge é
/// criada (lazy) no netns de infra no 1.º `attach`. Idempotente por nome.
pub fn network_create(name: &str) -> Result<NetDef> {
    if let Some(def) = network_get(name) {
        return Ok(def);
    }
    let used: std::collections::HashSet<String> = network_list().into_iter().map(|d| d.prefix).collect();
    let prefix = (201..=254)
        .map(|o| format!("10.{o}"))
        .find(|p| !used.contains(p))
        .ok_or_else(|| Error::Invalid("sem prefixos /16 livres para redes de ingress".into()))?;
    let def = NetDef {
        name: name.to_string(),
        bridge: format!("dlxn{:08x}", crate::fnv32(name)),
        prefix,
    };
    std::fs::create_dir_all(networks_dir()).map_err(|e| Error::Runtime {
        context: "networks dir",
        message: e.to_string(),
    })?;
    std::fs::write(netdef_path(name), serde_json::to_vec_pretty(&def).unwrap_or_default())?;
    Ok(def)
}

/// Como [`network_create`], mas com um **prefixo `/16` explícito** (ex.: `"10.50"`).
/// Usado para ALINHAR o plano de rede das VMs ao prefixo decidido pelo
/// `NetworkStore` (a fonte da verdade), para que a mesma rede tenha a MESMA subnet
/// em containers e VMs. Idempotente por nome.
pub fn network_create_with(name: &str, prefix: &str) -> Result<NetDef> {
    if let Some(def) = network_get(name) {
        return Ok(def);
    }
    let def = NetDef {
        name: name.to_string(),
        bridge: format!("dlxn{:08x}", crate::fnv32(name)),
        prefix: prefix.to_string(),
    };
    std::fs::create_dir_all(networks_dir()).map_err(|e| Error::Runtime {
        context: "networks dir",
        message: e.to_string(),
    })?;
    std::fs::write(netdef_path(name), serde_json::to_vec_pretty(&def).unwrap_or_default())?;
    Ok(def)
}

/// **Remove uma rede privada do ingress**: apaga a bridge (se a infra estiver de
/// pé) e a `NetDef`. Best-effort.
pub fn network_remove(name: &str) {
    if let Some(def) = network_get(name) {
        // `control_send` falha já se o holder estiver em baixo (rede sem cargas) —
        // a bridge nunca viveu num netns, nada a apagar. Best-effort.
        let _ = control_send(&format!("netdel {}", def.bridge));
    }
    let _ = std::fs::remove_file(netdef_path(name));
}

// ---- API host-side: fábrica de containers + ciclo de vida (ref-count) -------

/// IP determinístico de um container numa rede do ingress (`<prefix>.A.B`),
/// derivado do id — estável entre invocações.
pub fn container_ip_on(prefix: &str, id: &str) -> String {
    crate::alloc_ip_in(prefix, id)
}

/// IP do container na rede default do ingress (`10.200.A.B`).
pub fn container_ip(id: &str) -> String {
    container_ip_on(INFRA_PREFIX, id)
}

/// **Liga um container a uma rede do ingress** (`net`=`ingress` ou nome de rede
/// privada): garante a infra de pé (ref-count++), resolve a bridge/gateway e pede
/// ao holder o netns + `veth` + IP. Devolve `(netns, ip)`. Em falha desfaz o ref-count.
pub fn attach_container(id: &str, net: &str) -> Result<(String, String)> {
    let (bridge, prefix, gateway) = resolve_net(net)?;
    let ip = container_ip_on(&prefix, id);
    acquire()?; // ensure_up + refcount++
    let netns = sanitize(id);
    match control_send(&format!("attach {netns} {ip} {bridge} {gateway}")) {
        Ok(()) => Ok((netns, ip)),
        Err(e) => {
            release(); // desfaz o ref-count se o attach falhou
            Err(e)
        }
    }
}

/// **Desliga um container do ingress**: limpa a firewall (no seu `ip`), pede o
/// `detach` ao holder e baixa o ref-count (derruba a infra no último). Best-effort.
pub fn detach_container(id: &str, ip: &str) {
    let netns = sanitize(id);
    let _ = control_send(&format!("unfirewall {ip}"));
    let _ = control_send(&format!("detach {netns}"));
    release(); // refcount-- (teardown no 0)
}

/// **Aplica a firewall parametrizável de um container NO INGRESS** (o único sítio,
/// via o bind): traduz a `ContainerFw` (a mesma persistida no record, v0.1.93) para
/// a chain `fw<hash>` do `dlxing`, chaveada pelo `ip` do container na sua rede.
pub fn apply_firewall(id: &str, ip: &str, fw: &delonix_core::ContainerFw) -> Result<()> {
    let json = serde_json::to_vec(fw).map_err(|e| Error::Invalid(e.to_string()))?;
    control_send(&format!("firewall {} {} {}", sanitize(id), ip, hex_encode(&json)))
}

/// Remove a firewall de um container do ingress (best-effort).
pub fn clear_firewall(ip: &str) {
    let _ = control_send(&format!("unfirewall {ip}"));
}

// ---- VMs no ingress (QEMU/KVM) ----------------------------------------------

/// Nome do `tap` de uma VM (determinístico, <= 15 chars).
pub fn vm_tap_name(vm: &str) -> String {
    format!("vt{:08x}", crate::fnv32(vm))
}

/// Hash FNV-1a de um nome (para derivar MAC determinístico, etc.).
pub fn name_hash(s: &str) -> u32 {
    crate::fnv32(s)
}

/// **Liga uma VM ao ingress**: garante a infra de pé (ref-count++), resolve a rede
/// e pede ao holder um `tap` na bridge dessa rede (com DHCP). Devolve o nome do tap
/// (que o QEMU usa). O guest obtém IP por DHCP (pool da rede; gateway = ingress).
pub fn vm_attach(vm: &str, net: &str) -> Result<String> {
    let (bridge, _prefix, gateway) = resolve_net(net)?;
    acquire()?;
    let tap = vm_tap_name(vm);
    match control_send(&format!("vmtap {tap} {bridge} {gateway}")) {
        Ok(()) => Ok(tap),
        Err(e) => {
            release();
            Err(e)
        }
    }
}

/// **Desliga uma VM do ingress**: remove o `tap` e baixa o ref-count. Best-effort.
pub fn vm_detach(vm: &str) {
    let _ = control_send(&format!("vmtapdel {}", vm_tap_name(vm)));
    release();
}

/// `argv` para correr um processo (o QEMU) DENTRO do netns de infra do holder
/// (onde vivem as bridges e os taps). `None` se a infra não estiver de pé.
pub fn infra_join_argv() -> Option<Vec<String>> {
    let holder = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p))?;
    Some(vec![
        "nsenter".into(), "-t".into(), holder.to_string(),
        "-U".into(), "-m".into(), "-n".into(), "--preserve-credentials".into(), "--".into(),
    ])
}

/// Descobre o IP de um MAC na rede de infra — pela tabela `neigh` (ARP) DENTRO do
/// netns do holder (imediata, ao contrário da leasefile do udhcpd que só é escrita
/// periodicamente). Usado para reportar o IP que o DHCP atribuiu a uma VM/cliente.
/// `_net` mantido por compatibilidade da assinatura. `None` se o MAC ainda não
/// apareceu na tabela (guest a arrancar).
pub fn dhcp_ip_for_mac(_net: &str, mac: &str) -> Option<String> {
    let holder = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p))?;
    let mac = mac.to_lowercase();
    let out = crate::capture(
        "nsenter",
        &["-t", &holder.to_string(), "-U", "-n", "--preserve-credentials", "ip", "-o", "neigh", "show"],
    )
    .ok()?;
    for line in out.lines() {
        // formato: "<ip> dev <br> lladdr <mac> <state>"
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

/// **Publica uma porta pelo ingress** (o bind do container): `add_hostfwd` no
/// slirp único (host → tap0) + DNAT na chain `pre` (tap0 → container). `spec` é
/// `hostPort:contPort[/tcp|udp]`. É AQUI que vivem as regras parametrizáveis do
/// firewall do ingress (próximo incremento: allow/deny por porta/CIDR na mesma
/// superfície).
pub fn publish_port(cip: &str, spec: &str) -> Result<()> {
    let (host_port, cont_port, proto) = crate::parse_publish(spec)?;
    // host → tap0:host_port (o slirp único; guest_port == host_port).
    crate::slirp_add_hostfwd(&slirp_sock_path(), &host_port, &host_port, &proto)?;
    // tap0:host_port → container:cont_port (DNAT no netns de infra, via holder).
    control_send(&format!("publish {proto} {host_port} {cip} {cont_port}"))
}

/// Remove a publicação de uma `host_port`: tira o `add_hostfwd` do slirp e o DNAT
/// da chain `pre`. Best-effort.
pub fn unpublish_port(host_port: &str) {
    let _ = slirp_remove_hostfwd(&slirp_sock_path(), host_port);
    let _ = control_send(&format!("unpublish {host_port}"));
}

/// Envia um comando JSON ao api-socket do slirp único e devolve a resposta.
fn slirp_api(sock: &Path, json: &str) -> Result<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(sock).map_err(|e| Error::Runtime {
        context: "slirp api",
        message: e.to_string(),
    })?;
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    s.write_all(json.as_bytes()).map_err(|e| Error::Runtime {
        context: "slirp api write",
        message: e.to_string(),
    })?;
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
    Ok(resp)
}

/// Remove um `hostfwd` do slirp único: descobre o `id` da entrada com aquela
/// `host_port` (via `list_hostfwd`) e remove-o.
fn slirp_remove_hostfwd(sock: &Path, host_port: &str) -> Result<()> {
    let hp: u32 = host_port.parse().map_err(|_| Error::Invalid("porta inválida".into()))?;
    let listed = slirp_api(sock, r#"{"execute":"list_hostfwd"}"#)?;
    let v: serde_json::Value = serde_json::from_str(&listed).unwrap_or(serde_json::Value::Null);
    if let Some(entries) = v.get("return").and_then(|r| r.get("entries")).and_then(|e| e.as_array()) {
        for e in entries {
            if e.get("host_port").and_then(|p| p.as_u64()) == Some(hp as u64) {
                if let Some(id) = e.get("id").and_then(|i| i.as_u64()) {
                    let cmd = format!(r#"{{"execute":"remove_hostfwd","arguments":{{"id":{id}}}}}"#);
                    let _ = slirp_api(sock, &cmd);
                }
            }
        }
    }
    Ok(())
}

/// O prefixo `argv` para CORRER um processo dentro do netns de um container gerido
/// pelo holder: entra no userns+mountns do holder (`--preserve-credentials` evita
/// o `setgroups` error) e faz `ip netns exec <netns>`. O runtime prefixa isto ao
/// comando do container. `None` se a infra não estiver de pé.
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

/// Envia um comando ao socket de controlo do holder e espera `ok`. Tenta
/// brevemente até o socket existir (o holder cria-o ao arrancar).
fn control_send(cmd: &str) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    // Fast-fail se o holder NÃO estiver vivo: sem ele não há ninguém a responder, e
    // girar 50×40ms (~2s) à espera de um socket que não vem é puro desperdício. Os
    // caminhos de SETUP chamam `ensure_up()` antes (holder vivo → passa); os de
    // TEARDOWN com o holder em baixo saem aqui. O retry abaixo continua a cobrir a
    // corrida de arranque legítima (holder JÁ vivo, socket ainda a ligar).
    if status().holder_pid.is_none() {
        return Err(Error::Runtime {
            context: "control socket",
            message: "ingress holder em baixo".into(),
        });
    }
    let sock = control_sock_path();
    let mut last = String::from("socket de controlo indisponível");
    for _ in 0..50 {
        match UnixStream::connect(&sock) {
            Ok(mut s) => {
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                s.write_all(format!("{cmd}\n").as_bytes()).map_err(|e| Error::Runtime {
                    context: "control write",
                    message: e.to_string(),
                })?;
                let mut resp = String::new();
                let _ = s.read_to_string(&mut resp);
                let resp = resp.trim();
                if resp == "ok" {
                    return Ok(());
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
    Err(Error::Runtime { context: "control socket", message: last })
}

/// Escrita atómica do ficheiro de estado (tmp + rename) para o pai nunca ler um
/// valor parcial.
fn write_status(s: &str) {
    let _ = std::fs::create_dir_all(ingress_dir());
    let tmp = ingress_dir().join(".status.tmp");
    if std::fs::write(&tmp, s).is_ok() {
        let _ = std::fs::rename(&tmp, status_path());
    }
}

/// Configura o netns de infra (corre dentro do holder). A receita provada: lo up
/// → bridge `delonix0` 10.200.0.1/16 up → `ip_forward=1` → tmpfs em `/run/netns`
/// (para a Fase 3 criar netns de container) → tabela `nft` de ingress.
fn setup_infra_netns() -> Result<()> {
    // mounts do holder ficam privados (não vazam para o host).
    run_ok("mount", &["--make-rprivate", "/"]);
    run("ip", &["link", "set", "lo", "up"])?;
    run("ip", &["link", "add", INFRA_BRIDGE, "type", "bridge"])?;
    run("ip", &["addr", "add", INFRA_CIDR, "dev", INFRA_BRIDGE])?;
    run("ip", &["link", "set", INFRA_BRIDGE, "up"])?;
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1").map_err(|e| Error::Runtime {
        context: "ip_forward",
        message: e.to_string(),
    })?;
    // /run/netns para `ip netns` dos containers (Fase 3); best-effort.
    run_ok("mount", &["-t", "tmpfs", "none", "/run"]);
    let _ = std::fs::create_dir_all("/run/netns");
    apply_nft_stdin(&ingress_table_ruleset())?;
    // DHCP da rede default do ingress (delonix0).
    start_dhcp(INFRA_BRIDGE, INFRA_PREFIX);
    Ok(())
}

/// Aplica um *ruleset* `nft` por stdin (`nft -f -`) — variante local ao holder
/// (a do `lib.rs` é privada ao módulo).
fn apply_nft_stdin(ruleset: &str) -> Result<()> {
    use std::io::Write;
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Runtime { context: "spawn nft", message: e.to_string() })?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(ruleset.as_bytes())
        .map_err(|e| Error::Runtime { context: "nft stdin", message: e.to_string() })?;
    let out = child
        .wait_with_output()
        .map_err(|e| Error::Runtime { context: "nft wait", message: e.to_string() })?;
    if !out.status.success() {
        return Err(Error::Runtime {
            context: "nft -f",
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// Re-exporta o IP do tap0 do slirp (lado infra) — o destino dos `add_hostfwd`.
pub const INFRA_SLIRP_IP: &str = SLIRP_IP;

// ---- DNS interno do ingress (responder próprio; o dnsmasq não corre rootless) ----

/// **Servidor DNS do ingress** — corre numa thread do holder, escuta UDP `:53` em
/// TODAS as bridges (`0.0.0.0` no netns de infra → responde em cada gateway).
/// Resolve nomes de **containers e VMs** do ingress (→ IPv4); reencaminha o resto
/// para o upstream (DNS do slirp). É o equivalente funcional do dnsmasq (que não
/// funciona rootless).
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

/// Resposta a uma query DNS: se for `A` e o nome for de um container/VM do ingress,
/// responde com o IP; senão reencaminha para o upstream.
fn handle_dns(q: &[u8]) -> Option<Vec<u8>> {
    // parse da 1ª questão (offset 12): labels até 0x00, depois QTYPE+QCLASS.
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
    let qend = i + 4; // fim da questão (QTYPE+QCLASS)
    if qtype == 1 {
        if let Some(ip) = dns_resolve(&name) {
            let mut r = Vec::with_capacity(qend + 16);
            r.extend_from_slice(&q[0..2]); // ID original
            r.extend_from_slice(&[0x81, 0x80]); // flags: resposta + RA
            r.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
            r.extend_from_slice(&[0x00, 0x01]); // ANCOUNT=1
            r.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // NSCOUNT=0, ARCOUNT=0
            r.extend_from_slice(&q[12..qend]); // questão original
            r.extend_from_slice(&[0xc0, 0x0c]); // ponteiro para o nome (offset 12)
            r.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // TYPE A, CLASS IN
            r.extend_from_slice(&[0x00, 0x00, 0x00, 0x1e]); // TTL 30s
            r.extend_from_slice(&[0x00, 0x04]); // RDLENGTH 4
            r.extend_from_slice(&ip);
            return Some(r);
        }
    }
    forward_dns(q)
}

/// Reencaminha a query crua para o upstream (DNS do slirp; fallback 1.1.1.1) e
/// devolve a resposta.
fn forward_dns(q: &[u8]) -> Option<Vec<u8>> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(3))).ok()?;
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

/// Resolve um nome do ingress (container OU VM) → IPv4. Aceita `nome` e
/// `nome.delonix.io`. Lê os records dos containers e as metas das VMs.
fn dns_resolve(name: &str) -> Option<[u8; 4]> {
    let n = name.trim_end_matches('.').trim_end_matches(".delonix.io").to_lowercase();
    if n.is_empty() {
        return None;
    }
    // containers: <base>/containers/*.json (name + ip)
    if let Ok(rd) = std::fs::read_dir(base_root().join("containers")) {
        for e in rd.flatten() {
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&std::fs::read(e.path()).unwrap_or_default()) else { continue };
            if v["name"].as_str().map(|s| s.to_lowercase()).as_deref() == Some(n.as_str()) {
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
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&std::fs::read(e.path()).unwrap_or_default()) else { continue };
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

// ---- IPv6 SLAAC: emissor de Router Advertisements (sem radvd, que não há) ----

/// **Emissor de Router Advertisements** — corre numa thread do holder; a cada ~8s
/// envia um RA (ICMPv6 tipo 134) para `ff02::1` em CADA bridge do ingress, com o
/// prefixo ULA `/64` da rede (flags A+L → SLAAC). VMs e containers auto-configuram
/// um IPv6 a partir do prefixo. Substitui o radvd (inexistente/rootless-hostil).
fn ra_sender_main() {
    // SAFETY: cria um socket raw ICMPv6 (CAP_NET_RAW no netns de infra).
    let fd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_RAW, libc::IPPROTO_ICMPV6) };
    if fd < 0 {
        return;
    }
    let hops: libc::c_int = 255; // RA exige hop limit 255
    // SAFETY: setsockopt num fd válido com um inteiro.
    unsafe {
        libc::setsockopt(fd, libc::IPPROTO_IPV6, libc::IPV6_MULTICAST_HOPS, &hops as *const _ as *const libc::c_void, 4);
    }
    loop {
        for (br, prefix) in ra_bridges() {
            let cname = match std::ffi::CString::new(br.clone()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // SAFETY: if_nametoindex com um nome C válido.
            let idx = unsafe { libc::if_nametoindex(cname.as_ptr()) };
            if idx == 0 {
                continue;
            }
            // SAFETY: define a interface de saída do multicast.
            unsafe {
                libc::setsockopt(fd, libc::IPPROTO_IPV6, libc::IPV6_MULTICAST_IF, &idx as *const _ as *const libc::c_void, 4);
            }
            let pkt = build_ra(&prefix);
            // sockaddr_in6 para ff02::1 (all-nodes).
            // SAFETY: zera e preenche um sockaddr_in6 válido; sendto com tamanhos certos.
            unsafe {
                let mut dst: libc::sockaddr_in6 = std::mem::zeroed();
                dst.sin6_family = libc::AF_INET6 as u16;
                dst.sin6_addr.s6_addr = std::net::Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1).octets();
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

/// Bridges do ingress + o seu prefixo `/64` ULA (16 bytes, host a zero), lidas da
/// tabela de endereços do netns de infra.
fn ra_bridges() -> Vec<(String, [u8; 16])> {
    let mut out = Vec::new();
    let links = crate::capture("ip", &["-o", "link", "show", "type", "bridge"]).unwrap_or_default();
    for line in links.lines() {
        let name = line.split(':').nth(1).map(|s| s.trim().split('@').next().unwrap_or("").trim()).unwrap_or("");
        if name != INFRA_BRIDGE && !name.starts_with("dlxn") {
            continue;
        }
        let addrs = crate::capture("ip", &["-6", "-o", "addr", "show", "dev", name]).unwrap_or_default();
        for tok in addrs.split_whitespace() {
            if tok.starts_with("fd00:") {
                let ipstr = tok.split('/').next().unwrap_or("");
                if let Ok(v6) = ipstr.parse::<std::net::Ipv6Addr>() {
                    let mut b = v6.octets();
                    for x in b.iter_mut().skip(8) {
                        *x = 0; // só o /64
                    }
                    out.push((name.to_string(), b));
                    break;
                }
            }
        }
    }
    out
}

/// Constrói um Router Advertisement (ICMPv6 134) com uma opção Prefix Information
/// (A+L → SLAAC on-link). O checksum ICMPv6 é preenchido pelo kernel (socket raw).
fn build_ra(prefix: &[u8; 16]) -> Vec<u8> {
    let mut p = vec![134u8, 0, 0, 0]; // type=RA, code=0, checksum=0 (kernel)
    p.push(64); // cur hop limit
    p.push(0); // flags M/O = 0 (SLAAC, sem DHCPv6)
    p.extend_from_slice(&1800u16.to_be_bytes()); // router lifetime (default router)
    p.extend_from_slice(&0u32.to_be_bytes()); // reachable time
    p.extend_from_slice(&0u32.to_be_bytes()); // retrans timer
    // opção Prefix Information (type 3, len 4×8=32 bytes)
    p.push(3);
    p.push(4);
    p.push(64); // prefix length
    p.push(0xc0); // flags: L (on-link) + A (autonomous/SLAAC)
    p.extend_from_slice(&86400u32.to_be_bytes()); // valid lifetime
    p.extend_from_slice(&14400u32.to_be_bytes()); // preferred lifetime
    p.extend_from_slice(&0u32.to_be_bytes()); // reserved
    p.extend_from_slice(prefix); // 16 bytes do prefixo
    p
}

/// IPv6 ULA determinístico (estático) de um container a partir do seu IPv4. Para
/// mostrar na UI/CLI.
pub fn container_ip6(ip4: &str) -> Option<String> {
    v6_of(ip4)
}

/// IPv6 de um MAC pela tabela `neigh` v6 do netns de infra (via nsenter, do host).
/// Para mostrar o IPv6 (SLAAC) de uma VM. `None` se ainda não apareceu.
pub fn dhcp_ip6_for_mac(_net: &str, mac: &str) -> Option<String> {
    let holder = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p))?;
    let mac = mac.to_lowercase();
    let out = crate::capture(
        "nsenter",
        &["-t", &holder.to_string(), "-U", "-n", "--preserve-credentials", "ip", "-6", "neigh", "show"],
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

/// IP de um MAC pela tabela `neigh` — corre DENTRO do holder (já no netns), sem nsenter.
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
    use super::*;

    #[test]
    fn refcount_never_goes_negative() {
        assert_eq!(next_refcount(0, 1), 1);
        assert_eq!(next_refcount(3, 1), 4);
        assert_eq!(next_refcount(1, -1), 0);
        assert_eq!(next_refcount(0, -1), 0); // release a mais não passa de 0
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
        assert_eq!(a, vh_name("0123456789ab")); // determinístico
        assert!(a.starts_with("vh"));
        assert!(a.len() <= 15, "IFNAMSIZ: {a}"); // 'vh' + 8 hex = 10
        assert_ne!(a, vh_name("ffffffffffff")); // ids diferentes → nomes diferentes
    }

    #[test]
    fn sanitize_strips_unsafe_and_caps_length() {
        assert_eq!(sanitize("abc; rm -rf /"), "abcrm-rf"); // sem espaços/`;`/`/`
        assert_eq!(sanitize("0123456789abcdef").len(), 12); // <= 12
        assert_eq!(sanitize("web_1-x"), "web_1-x"); // alnum/_/- preservados
    }

    #[test]
    fn hex_roundtrip() {
        let data = br#"{"enabled":true}"#;
        assert_eq!(hex_decode(&hex_encode(data)).unwrap(), data);
        assert!(hex_decode("abc").is_none()); // ímpar
        assert!(hex_decode("zz").is_none()); // não-hex
    }

    #[test]
    fn fw_body_translates_rules_and_policy() {
        let fw = delonix_core::ContainerFw {
            enabled: true,
            policy_in: "deny".into(),
            policy_out: "allow".into(),
            rules: vec![
                delonix_core::FwRule { dir: "in".into(), proto: "tcp".into(), port: "8080".into(), src: "10.200.0.0/16".into(), action: "allow".into(), note: String::new() },
                delonix_core::FwRule { dir: "out".into(), proto: "any".into(), port: String::new(), src: String::new(), action: "deny".into(), note: String::new() },
            ],
        };
        let body = fw_chain_body("10.200.0.5", &fw);
        // regra in: daddr==ip, peer saddr==src, tcp dport 8080 accept
        assert!(body.contains("ip daddr 10.200.0.5 ip saddr 10.200.0.0/16 tcp dport 8080 accept"), "{body}");
        // regra out: saddr==ip, drop (proto any → sem proto/dport)
        assert!(body.contains("ip saddr 10.200.0.5 drop"), "{body}");
        // política in=deny → drop final no daddr
        assert!(body.contains("ip daddr 10.200.0.5 drop"), "{body}");
        // disabled → corpo vazio
        let off = delonix_core::ContainerFw { enabled: false, ..fw };
        assert!(fw_chain_body("10.200.0.5", &off).is_empty());
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
        assert!(validate_publish("tcp", "0", "10.200.0.5", "80").is_err()); // porta 0
        assert!(validate_publish("tcp", "8080", "10.99.0.5", "80").is_err()); // IP fora da subnet
        assert!(!is_port("70000") && !is_port("abc") && is_port("443"));
    }

    #[test]
    fn container_ip_in_infra_subnet() {
        let ip = container_ip("0a0b0c0d1122");
        assert!(ip.starts_with(&format!("{INFRA_PREFIX}.")), "{ip}");
        assert!(crate::valid_ip_in_subnet(INFRA_PREFIX, &ip), "{ip}");
        assert_eq!(ip, container_ip("0a0b0c0d1122")); // determinístico
    }

    #[test]
    fn base_root_honours_explicit_root() {
        // com DELONIX_ROOT definido, ingress_dir é determinístico e NÃO depende
        // do uid (essencial para o holder com uid mapeado a 0).
        std::env::set_var("DELONIX_ROOT", "/tmp/dlx-test-root");
        assert_eq!(ingress_dir(), PathBuf::from("/tmp/dlx-test-root/ingress"));
        std::env::remove_var("DELONIX_ROOT");
    }
}
