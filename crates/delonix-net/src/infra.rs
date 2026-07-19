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
use delonix_runtime_core::{Error, Result};
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
    // DEFAULT-DENY no forward (Grupo B). Os DROPS dinâmicos (anti-spoof, isolamento,
    // egress, egress-net, l4guard, fw por-container) vivem na chain `fwdeny`
    // (prioridade -10, corre ANTES) — assim um `drop`/`accept` específico ganha
    // sempre ao default. O `forward` (prioridade 0) permite returns + egress +
    // inbound + **mesma rede** (intra-bridge `delonix0`); o resto cai no `policy drop`.
    //
    // INTRA-REDE: com `br_netfilter` (bridge-nf-call-iptables=1) o tráfego entre
    // containers da MESMA bridge atravessa o forward e cairia no drop → apps não
    // alcançariam os seus serviços/addons na mesma rede. Aceitamos `delonix0↔delonix0`
    // (modelo Docker user-network/k8s: mesma rede comunica; cruzar redes é dropado pelo
    // `fwdeny` inter-bridge). A micro-segmentação intra-rede faz-se com `kind:NetworkPolicy`
    // (P12), cujas regras entram no `fwdeny` (correm antes, pré-emptem este accept).
    // Rollback instantâneo: DELONIX_FORWARD_POLICY=accept → volta ao default-allow.
    let policy = if std::env::var("DELONIX_FORWARD_POLICY").ok().as_deref() == Some("accept") {
        // NET-03: o opt-out reverte o default-deny — não deixar isto silencioso.
        eprintln!(
            "delonix: AVISO DE SEGURANÇA — DELONIX_FORWARD_POLICY=accept: o forward do netns\n\
             \x20        de ingress volta a default-ALLOW (sem `policy drop`). Só para\n\
             \x20        depuração — NÃO usar em produção."
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
/// Espera que `fd` fique legível, com teto de `timeout_ms`. `true` = legível (ou
/// EOF, que também é um evento e desbloqueia o `read`); `false` = esgotou o tempo.
///
/// Existe para não haver mais `read` nus em fds que dependem de um processo
/// externo sinalizar: se esse processo nunca sinalizar E nunca fechar o fd (basta
/// um neto herdá-lo), o `read` fica pendurado para sempre — foi assim que um
/// `run` ficou preso 1h em `skb_wait_for_more_packets` sem log nem exit.
/// `poll` não precisa de mexer nas flags do fd (nada de `O_NONBLOCK` a vazar
/// para quem o herdar).
fn wait_readable(fd: i32, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: `pfd` é válido e vive durante a chamada; poll não retém o ponteiro.
    // EINTR (sinal) devolve -1 → tratamos como "não ficou pronto", o chamador avisa.
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 }
}

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
            // ESPERA COM TETO. Um `read` nu aqui podia pendurar PARA SEMPRE: o EOF
            // só chega se TODAS as cópias do write-end fecharem, e basta um neto do
            // slirp herdar o fd para isso nunca acontecer. E o preço subiu: o
            // `slirp_attach` passou a correr ANTES de libertar o container (a rede
            // tem de estar pronta antes do entrypoint), por isso um slirp que não
            // sinalize pendura o `run` inteiro, sem log e sem exit — a mesma classe
            // do deadlock do `recv_fd` do console. 10s chegam de sobra (o slirp
            // sinaliza em ms); ao fim disso seguimos e o erro aparece a jusante,
            // com mensagem, em vez de um processo pendurado para sempre.
            if !wait_readable(rd, 10_000) {
                eprintln!("delonix: aviso — slirp4netns não sinalizou pronto em 10s; a rede do container pode não estar operacional");
            }
            let mut b = [0u8; 1];
            // SAFETY: lê 1 byte do read-end (já legível, ou desistimos acima).
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
        let listener =
            std::os::unix::net::UnixListener::bind(control_sock_path()).map_err(|e| Error::Runtime {
                context: "control socket",
                message: e.to_string(),
            })?;
        // só o uid do engine pode falar com o holder: 0600 + SO_PEERCRED (control_loop).
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(control_sock_path(), std::fs::Permissions::from_mode(0o600));
        Ok(listener)
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
/// uid do peer de uma ligação Unix (via SO_PEERCRED). `None` em falha.
fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt em SO_PEERCRED com um buffer ucred do tamanho correto.
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
    // SAFETY: geteuid() não tem pré-condições.
    let own_uid = unsafe { libc::geteuid() };
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        // SO_PEERCRED: só aceita comandos do próprio uid do engine — impede que um
        // utilizador local não-privilegiado conduza o holder / injete nft (CAP_NET_ADMIN).
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

/// Despacha um comando de controlo (`attach <netns> <ip>`, `detach <netns>`,
/// `ping`) e devolve a resposta (`ok\n` ou `err: <msg>\n`).
fn handle_control(line: &str) -> String {
    let parts: Vec<&str> = line.split_whitespace().collect();
    // CNI (rootless): o plugin corre AQUI, no holder — mapped-root e dono da netns
    // (o host, uid do utilizador, não teria CAP_NET_ADMIN nela). `cni-add` devolve
    // o IP atribuído no corpo da resposta (`ok <cidr>`), para o host o registar.
    if let ["cni-add", netns, id, ifname, hex] = parts.as_slice() {
        return match do_cni_add(netns, id, ifname, hex) {
            Ok(ip) => format!("ok {ip}\n"),
            Err(e) => format!("err: {e}\n"),
        };
    }
    // Query: IPs FQDN actualmente aprendidos (no set nft) da bridge — para o
    // `egress show`. Corre no holder (dono do netns onde o set vive).
    if let ["egress-show", bridge] = parts.as_slice() {
        return format!("ok {}\n", egress_set_members(bridge).join(","));
    }
    let res = match parts.as_slice() {
        ["ping"] => Ok(()),
        ["attach", netns, ip, bridge, gateway] => do_attach(netns, ip, bridge, gateway),
        ["detach", netns] => do_detach(netns),
        ["cni-del", netns, id, ifname, hex] => do_cni_del(netns, id, ifname, hex),
        // multi-homing ao vivo (rootless): liga/desliga uma rede ADICIONAL a um
        // container já a correr (veth extra para a bridge da rede privada).
        ["attach-extra", netns, ifname, ip, bridge, gateway] => do_attach_extra(netns, ifname, ip, bridge, gateway),
        ["detach-extra", netns, ifname] => do_detach_extra(netns, ifname),
        // limite de largura de banda ao vivo (rootless): shaping no veth do lado
        // do infra (download via tbf na raiz, upload via ingress police).
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
        ["l4guard", rate, max] => do_l4guard(rate.parse().unwrap_or(50), max.parse().unwrap_or(200)),
        ["l4guard-clear"] => {
            clear_l4guard();
            Ok(())
        }
        // WireGuard sobre o overlay (req #6): a interface vive no netns de infra.
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
        // Uplink VXLAN de uma rede overlay (o L2 partilhado entre nós). `dsts` = os
        // destinos do FDB (`wg_ip` se cifrado, senão `node_ip`; `-` = sem pares).
        ["vxlan", dev, vni, bridge, gateway, dsts] => do_vxlan(dev, vni, bridge, gateway, dsts),
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
    // Conectividade INTRA-rede: os containers da MESMA bridge falam-se (modelo
    // Docker/user-network, como o `delonix0`). Sem esta regra, o `policy drop` do
    // `forward` cortava TODO o tráfego intra-bridge das redes criadas (`dlxn*`) —
    // serviços na mesma rede (incl. dentro de um tenant) não se alcançavam. A
    // micro-segmentação fina faz-se depois com `kind:NetworkPolicy`. Idempotente.
    let fchain = crate::capture("nft", &["list", "chain", "ip", INGRESS_TABLE, "forward"]).unwrap_or_default();
    let self_accept = format!("iifname \"{bridge}\" oifname \"{bridge}\" accept");
    if !fchain.contains(&self_accept) {
        run_ok("nft", &["add", "rule", "ip", INGRESS_TABLE, "forward", "iifname", bridge, "oifname", bridge, "accept"]);
    }
    // isolamento entre redes: drop forward entre esta bridge e as outras delonix.
    let listed = crate::capture("ip", &["-o", "link", "show", "type", "bridge"]).unwrap_or_default();
    let fwd = crate::capture("nft", &["list", "chain", "ip", INGRESS_TABLE, "fwdeny"]).unwrap_or_default();
    for line in listed.lines() {
        let other = line.split(':').nth(1).map(|s| s.trim().split('@').next().unwrap_or("").trim()).unwrap_or("");
        if other.is_empty() || other == bridge || (other != INFRA_BRIDGE && !other.starts_with("dlxn")) {
            continue; // só isolar contra delonix0 e outras redes dlxn*
        }
        for (a, b) in [(bridge, other), (other, bridge)] {
            let needle = format!("iifname \"{a}\" oifname \"{b}\" drop");
            if !fwd.contains(&needle) {
                run_ok("nft", &["add", "rule", "ip", INGRESS_TABLE, "fwdeny", "iifname", a, "oifname", b, "drop"]);
            }
        }
    }
    // servidor DHCP da rede (para VMs/clientes que peçam IP).
    start_dhcp(bridge, &prefix_of(gateway));
    // Re-aplica a intenção de egress PERSISTIDA quando a bridge é (re)criada — é o
    // que a faz sobreviver ao respawn do holder (o nft e o registry FQDN vivem no
    // netns efémero). Só no `!exists` (bridge nova): idempotente e barato.
    if !exists {
        if let Some(def) = network_list().into_iter().find(|d| d.bridge == bridge) {
            if def.egress.policy.is_some() || !def.egress.hosts.is_empty() {
                let _ = apply_egress_from_state(bridge, &def.egress);
            }
        }
    }
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

/// CNI rootless (holder): cria uma netns VAZIA e delega a sua configuração aos
/// plugins CNI (`crate::cni::add`) — a bridge/veth/IPAM são do plugin, não do SDN
/// nativo. Corre no holder (mapped-root, dono da netns → CAP_NET_ADMIN). Devolve o
/// IP (CIDR) atribuído pelo IPAM do CNI. `hex` = a conflist JSON em hex.
fn do_cni_add(netns: &str, id: &str, ifname: &str, hex: &str) -> Result<String> {
    let netns = sanitize(netns);
    let bytes = hex_decode(hex).ok_or_else(|| Error::Invalid("conflist hex inválida".into()))?;
    let conf = crate::cni::parse_config(&String::from_utf8_lossy(&bytes))?;
    // netns vazia (o plugin move o veth para lá); limpa restos de tentativas.
    run_ok("ip", &["netns", "del", &netns]);
    run("ip", &["netns", "add", &netns])?;
    let path = format!("/run/netns/{netns}");
    match crate::cni::add(&conf, &crate::cni::plugin_dirs(), id, &path, ifname) {
        Ok(r) => Ok(r.ips.first().map(|i| i.address.clone()).unwrap_or_default()),
        Err(e) => {
            // rollback: não deixa a netns órfã se o plugin falhou.
            run_ok("ip", &["netns", "del", &netns]);
            Err(e)
        }
    }
}

/// CNI rootless (holder): corre `DEL` dos plugins e remove a netns. Best-effort.
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
    // ANTI-SPOOFING: o tráfego que entra deste veth TEM de ter o IP atribuído como
    // origem — senão um container podia falsificar o source-IP e furar a firewall
    // por-IP / o isolamento / a atribuição de fluxos. `insert` põe a regra no topo
    // do `forward`, antes dos jumps por-container. Idempotente (limpa antes).
    //
    // NET-06 (limitação conhecida): para um nó Kind PRIVILEGIADO, o tráfego pod→pod
    // fica dentro do netns do nó (nunca cruza este veth) e o pod→exterior sai com
    // `saddr`=IP-do-nó (kindnet faz masquerade), por isso nó-único funciona. Um
    // cenário MULTI-NÓ com routing de pod-CIDR (10.244/16) ENTRE nós seria DROPado
    // aqui (saddr do pod ≠ IP-do-nó). Enquanto o multi-nó não for suportado, isto é
    // latente; a correcção será uma excepção de anti-spoof para o pod-CIDR quando o
    // container é um nó de cluster (a par do trabalho de routing inter-nó).
    clear_antispoof(&vh);
    run_ok(
        "nft",
        &["insert", "rule", "ip", INGRESS_TABLE, "fwdeny", "iifname", &vh, "ip", "saddr", "!=", ip, "drop"],
    );
    Ok(())
}

/// Remove o netns de um container (e, com ele, o `eth0`; o `vh` órfão é limpo a
/// seguir). Best-effort.
fn do_detach(netns: &str) -> Result<()> {
    let netns = sanitize(netns);
    let vh = vh_name(&netns);
    clear_antispoof(&vh);
    run_ok("ip", &["netns", "del", &netns]);
    run_ok("ip", &["link", "del", &vh]);
    Ok(())
}

/// Liga uma rede ADICIONAL a um container JÁ A CORRER (multi-homing ao vivo): um
/// segundo `veth` do netns existente para a bridge da rede privada. Não cria o
/// netns (já existe) e NÃO mexe na rota default (a rede primária mantém-na).
fn do_attach_extra(netns: &str, ifname: &str, ip: &str, bridge: &str, gateway: &str) -> Result<()> {
    let netns = sanitize(netns);
    let ifname = sanitize(ifname);
    let bridge = sanitize(bridge);
    ensure_net_bridge(&bridge, gateway)?;
    let vh = vh_name_extra(&netns, &ifname);
    run_ok("ip", &["link", "del", &vh]); // limpa restos
    run("ip", &["link", "add", &vh, "type", "veth", "peer", "name", &ifname])?;
    run("ip", &["link", "set", &vh, "master", &bridge])?;
    run("ip", &["link", "set", &vh, "up"])?;
    run("ip", &["link", "set", &ifname, "netns", &netns])?;
    let cidr = format!("{ip}/16");
    for argv in [
        vec!["netns", "exec", &netns, "ip", "addr", "add", &cidr, "dev", &ifname],
        vec!["netns", "exec", &netns, "ip", "link", "set", &ifname, "up"],
    ] {
        run("ip", &argv)?;
    }
    // IPv6 (ULA) na nova interface (best-effort; sem rota default v6 — primária mantém).
    if let Some(v6) = v6_of(ip) {
        let cidr6 = format!("{v6}/64");
        run_ok("ip", &["netns", "exec", &netns, "ip", "-6", "addr", "add", &cidr6, "dev", &ifname, "nodad"]);
    }
    // ANTI-SPOOFING também na interface adicional (mesma garantia por-IP do eth0).
    clear_antispoof(&vh);
    run_ok(
        "nft",
        &["insert", "rule", "ip", INGRESS_TABLE, "fwdeny", "iifname", &vh, "ip", "saddr", "!=", ip, "drop"],
    );
    Ok(())
}

/// Desliga uma rede adicional: remove o `veth` extra (leva o `<ifname>` do netns
/// do container atrás). Best-effort.
fn do_detach_extra(netns: &str, ifname: &str) -> Result<()> {
    let netns = sanitize(netns);
    let ifname = sanitize(ifname);
    let vh = vh_name_extra(&netns, &ifname);
    clear_antispoof(&vh);
    run_ok("ip", &["link", "del", &vh]);
    Ok(())
}

/// Aplica shaping de largura de banda no veth `vh` (lado do infra), DENTRO do
/// netns de infra (corre no holder). Mesmo caudal nos dois sentidos:
/// DOWNLOAD (host→container) = tbf na raiz; UPLOAD (container→host) = ingress
/// `police`+`drop`. `rate`/`burst` já vêm em bit/s e bytes. Idempotente.
fn do_netrate(vh: &str, rate: &str, burst: &str) -> Result<()> {
    let vh = sanitize(vh);
    let r = format!("{}bit", rate.parse::<u64>().unwrap_or(0).max(8000));
    let b = burst.to_string();
    do_netrate_clear(&vh); // reaplicação limpa
    run("tc", &["qdisc", "add", "dev", &vh, "root", "tbf", "rate", &r, "burst", &b, "latency", "50ms"])?;
    run("tc", &["qdisc", "add", "dev", &vh, "handle", "ffff:", "ingress"])?;
    run(
        "tc",
        &[
            "filter", "add", "dev", &vh, "parent", "ffff:", "protocol", "all", "prio", "1",
            "u32", "match", "u32", "0", "0", "police", "rate", &r, "burst", &b, "drop",
        ],
    )?;
    Ok(())
}

/// Remove o shaping do veth `vh` (best-effort). Apagar o veth já leva os qdiscs;
/// limpa-se à mão para reaplicação e órfãos.
fn do_netrate_clear(vh: &str) {
    let vh = sanitize(vh);
    run_ok("tc", &["qdisc", "del", "dev", &vh, "root"]);
    run_ok("tc", &["qdisc", "del", "dev", &vh, "handle", "ffff:", "ingress"]);
}

/// Remove as regras anti-spoofing de um veth no `forward` (idempotência).
fn clear_antispoof(vh: &str) {
    let listed = crate::capture("nft", &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"]).unwrap_or_default();
    let needle = format!("iifname \"{vh}\"");
    for line in listed.lines() {
        if line.contains(&needle) && line.contains("saddr") && line.contains("drop") {
            if let Some(h) = line.rsplit("# handle ").next().and_then(|x| x.trim().parse::<u32>().ok()) {
                run_ok("nft", &["delete", "rule", "ip", INGRESS_TABLE, "fwdeny", "handle", &h.to_string()]);
            }
        }
    }
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

/// Um destino de FDB só pode ser um IP (v4/v6): dígitos hex, `.`, `:`. Rejeita
/// tudo o resto ANTES de o passar ao `bridge`/`ip`. Vai por argv (não shell), mas
/// mantemos a disciplina `valid_*` da auditoria — um destino com espaço/`;`/`|`
/// nunca chega a um comando. (Um valor vazio já foi filtrado pelo chamador.)
fn valid_fdb_dst(dst: &str) -> bool {
    !dst.is_empty()
        && dst.len() <= 45 // teto de um IPv6 textual
        && dst.chars().all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':')
}

/// **Sobe o uplink VXLAN de uma rede overlay** no netns de infra (porta de
/// `crate::Net::ensure_vxlan` para o modelo holder rootless): garante a `<bridge>`
/// da rede, cria o device `<dev>` (id `<vni>`, porta 4789, `nolearning`) a
/// masterizá-la, e semeia o FDB com uma entrada "broadcast" (`00:…:00`) por cada
/// destino dos pares (`dsts_csv` = `wg_ip` se o overlay é cifrado, senão `node_ip`;
/// `-` = ainda sem pares). Idempotente: só cria o que falta, só semeia FDB novo.
fn do_vxlan(dev: &str, vni: &str, bridge: &str, gateway: &str, dsts_csv: &str) -> Result<()> {
    let dev = sanitize(dev);
    let bridge = sanitize(bridge);
    let vni: u32 = vni.parse().map_err(|_| Error::Invalid(format!("vni inválido: {vni}")))?;
    // A bridge da overlay é uma bridge de rede normal do holder — mesma função que
    // o `attach`/`vmtap` usam, para containers e VXLAN partilharem o mesmo L2.
    ensure_net_bridge(&bridge, gateway)?;
    let exists = crate::capture("ip", &["link", "show", &dev])
        .map(|o| o.contains(dev.as_str()))
        .unwrap_or(false);
    if !exists {
        run("ip", &[
            "link", "add", &dev, "type", "vxlan", "id", &vni.to_string(),
            "dstport", crate::VXLAN_PORT, "nolearning",
        ])?;
        run_ok("ip", &["link", "set", &dev, "master", &bridge]);
        run_ok("ip", &["link", "set", &dev, "up"]);
    }
    if dsts_csv != "-" {
        let have = crate::capture("bridge", &["fdb", "show", "dev", &dev]).unwrap_or_default();
        for dst in dsts_csv.split(',').map(str::trim).filter(|d| valid_fdb_dst(d)) {
            // Match EXACTO por token (não `contains`): senão 10.0.0.5 seria "já
            // presente" por ser substring de um 10.0.0.50 no FDB → nunca semeado.
            let present = have.lines().any(|l| l.split_whitespace().any(|t| t == dst));
            if !present {
                run_ok("bridge", &["fdb", "append", "00:00:00:00:00:00", "dev", &dev, "dst", dst]);
            }
        }
    }
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

/// Como [`do_publish`], mas com uma **allowlist de origem**: só os CIDRs dados
/// alcançam a `host_port`; o resto é dropado ANTES do DNAT (`insert` no topo da
/// chain `pre`). Os CIDRs são validados (`fw_src_ok`) — anti-injeção nft. Usado
/// para expor a DB de uma app só a IPs autorizados (firewall).
fn do_publish_allow(proto: &str, host_port: &str, cip: &str, cport: &str, cidrs_csv: &str) -> Result<()> {
    validate_publish(proto, host_port, cip, cport)?;
    let cidrs: Vec<&str> = cidrs_csv
        .split(',')
        .map(|c| c.trim())
        .filter(|c| !c.is_empty() && delonix_runtime_core::fw_src_ok(c))
        .collect();
    if cidrs.is_empty() {
        return Err(Error::Invalid("allowlist vazia ou sem CIDRs válidos".into()));
    }
    // drop no topo da `pre`: tráfego para esta host_port cujo saddr NÃO está na
    // allowlist é descartado antes de chegar à regra de DNAT (que vem depois).
    let set = format!("{{ {} }}", cidrs.join(", "));
    run("nft", &[
        "insert", "rule", "ip", INGRESS_TABLE, "pre",
        "ip", "daddr", SLIRP_IP, proto, "dport", host_port,
        "ip", "saddr", "!=", &set, "drop",
    ])?;
    do_publish(proto, host_port, cip, cport)
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

/// Política GLOBAL de egress do ingress único (corre DENTRO do netns de infra,
/// onde o holder tem CAP_NET_ADMIN). `deny` adiciona `forward oifname tap0 drop`
/// (bloqueia toda a saída para a Internet); `allow` remove-o. As regras de
/// firewall por-carga (accept) que apareçam ANTES na chain `forward` continuam a
/// abrir excepções pontuais — portanto isto é a política de BASE do egress.
fn do_egress(policy: &str) -> Result<()> {
    let listed = crate::capture("nft", &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"]).unwrap_or_default();
    for line in listed.lines() {
        if line.contains("oifname \"tap0\"") && line.contains("drop") {
            if let Some(handle) = line.rsplit("# handle ").next().and_then(|h| h.trim().parse::<u32>().ok()) {
                run_ok("nft", &["delete", "rule", "ip", INGRESS_TABLE, "fwdeny", "handle", &handle.to_string()]);
            }
        }
    }
    match policy {
        "deny" => run("nft", &["add", "rule", "ip", INGRESS_TABLE, "fwdeny", "oifname", "tap0", "drop"]),
        "allow" => Ok(()),
        _ => Err(Error::Invalid(format!("política de egress inválida: {policy}"))),
    }
}

/// Egress POR-REDE (workspace): controla a saída→Internet de UMA bridge, sem
/// afetar as outras. Idempotente (remove as regras antigas dessa bridge antes).
/// Suporta `deny`/`allow`/`allowlist:<cidrs>` (NET-A).
fn do_egress_net(bridge: &str, policy: &str) -> Result<()> {
    if !(policy == "allow" || policy == "deny" || policy.starts_with("allowlist:")) {
        return Err(Error::Invalid(format!("política de egress inválida: {policy}")));
    }
    let norm = (policy != "allow").then(|| policy.to_string());
    let bridge = sanitize(bridge);
    // Persiste a nova política e re-aplica a chain COMPLETA (política + hosts
    // FQDN existentes) — para `egress net` e `egress host` comporem.
    let state = update_netdef_egress(&bridge, |e| e.policy = norm.clone())
        .unwrap_or(EgressState { policy: norm, hosts: Vec::new() });
    apply_egress_from_state(&bridge, &state)
}

// ---- egress por HOSTNAME (FQDN allowlist via DNS-snooping) -------------------
//
// nft só sabe de IPs; para permitir "sai só para *.github.com" o holder vê as
// respostas DNS que já reencaminha (o resolver do ingress) e injecta os A-records
// dos hostnames permitidos num `set` nft por-bridge que o egress aceita. É a
// FQDN-policy do Cilium, mas 100% rootless (nft + DNS no holder, sem eBPF).

/// Allowlist FQDN partilhada entre a thread de controlo (regista em `egress-host`)
/// e a thread de DNS (popula o set com os A-records). Tuplos `(bridge, set, sufixo)`.
/// O sufixo `github.com` casa `github.com` E `*.github.com`.
static FQDN_ALLOW: std::sync::Mutex<Vec<(String, String, String)>> = std::sync::Mutex::new(Vec::new());

/// Nome (curto, <= limite do nft) do set FQDN de uma bridge.
fn fqdn_set(bridge: &str) -> String {
    format!("dlxfq{:08x}", crate::fnv32(bridge))
}

/// Regista um hostname permitido para a saída de uma bridge: cria o set nft (com
/// `flags timeout` para as entradas expirarem com o TTL), reprograma o egress da
/// bridge para `DNS + @set + drop`, e memoriza o sufixo para o DNS o popular.
fn do_egress_host(bridge: &str, suffix: &str) -> Result<()> {
    let bridge = sanitize(bridge);
    let suffix = suffix.trim().trim_start_matches("*.").trim_matches('.').to_lowercase();
    // Anti-injeção: um hostname é [a-z0-9.-], com pelo menos um ponto, <= 253.
    if suffix.is_empty() || suffix.len() > 253 || !suffix.contains('.') || !suffix.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
        return Err(Error::Invalid(format!("hostname inválido: {suffix:?}")));
    }
    // Persiste o hostname e re-aplica a chain COMPLETA (compõe com a política CIDR
    // se houver). `apply_egress_from_state` cria o set e regista no FQDN_ALLOW.
    let state = update_netdef_egress(&bridge, |e| {
        if !e.hosts.contains(&suffix) {
            e.hosts.push(suffix.clone());
        }
    })
    .unwrap_or(EgressState { policy: None, hosts: vec![suffix] });
    apply_egress_from_state(&bridge, &state)
}

/// Extrai os IPv4 dos A-records de uma resposta DNS (bounds-checked; tolera
/// compressão de nomes por saltar via RDLENGTH). PURA — testável sem rede.
fn parse_a_records(resp: &[u8]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    if resp.len() < 12 {
        return out;
    }
    let qd = u16::from_be_bytes([resp[4], resp[5]]) as usize;
    let an = u16::from_be_bytes([resp[6], resp[7]]) as usize;
    let mut i = 12usize;
    // saltar as QDCOUNT questões (nome + QTYPE + QCLASS)
    for _ in 0..qd {
        i = skip_name(resp, i);
        i += 4;
        if i > resp.len() {
            return out;
        }
    }
    // ler ANCOUNT respostas
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

/// Avança o offset para lá de um nome DNS (labels ou ponteiro de compressão 0xC0).
fn skip_name(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        let len = b[i] as usize;
        if len == 0 {
            return i + 1;
        }
        if len & 0xc0 == 0xc0 {
            return i + 2; // ponteiro de compressão: 2 bytes, fim do nome
        }
        i += 1 + len;
    }
    i
}

/// Se `name` casa um sufixo permitido, injecta os A-records de `resp` no(s) set(s)
/// nft correspondente(s), com timeout (renova a cada resolução). Best-effort.
fn snoop_fqdn(name: &str, resp: &[u8]) {
    let n = name.trim_end_matches('.').to_lowercase();
    let sets: Vec<String> = match FQDN_ALLOW.lock() {
        Ok(g) => g.iter().filter(|(_, _, suf)| n == *suf || n.ends_with(&format!(".{suf}"))).map(|(_, set, _)| set.clone()).collect(),
        Err(_) => return,
    };
    if sets.is_empty() {
        return;
    }
    for ip in parse_a_records(resp) {
        let ips = format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        for set in &sets {
            run_ok("nft", &["add", "element", "ip", INGRESS_TABLE, set, &format!("{{ {ips} timeout 1h }}")]);
        }
    }
}

/// Pré-flight de um ruleset `nft` (`nft -c -f -`): devolve `true` se for ACEITE,
/// SEM o aplicar. É a "regra de ouro" da proteção L4 — só aplicamos depois de o
/// kernel confirmar que suporta a sintaxe (ex.: `meter`/`ct count`).
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

/// Proteção DDoS L4 (req #5): rate-limit + ct-count POR-ORIGEM das NOVAS ligações
/// de entrada (via tap0), no `forward` do dlxing. Não é global (cada origem tem o
/// seu balde → não é self-DoS). `counter drop` torna os excessos OBSERVÁVEIS
/// (deteção). best-effort + pré-flight `nft -c`: se o kernel não suportar `meter`,
/// DEGRADA (não aplica, não parte o ruleset). Idempotente (limpa antes).
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
    // REGRA DE OURO: só aplica se o kernel aceitar a sintaxe (senão degrada).
    if !nft_check(&script) {
        return Ok(());
    }
    let _ = apply_nft_stdin(&script);
    Ok(())
}

/// Remove as regras de L4 guard do `forward` (e, com elas, os meters dinâmicos —
/// um meter sem regras que o referenciem é libertado). Idempotente.
fn clear_l4guard() {
    let listed = crate::capture("nft", &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"]).unwrap_or_default();
    for line in listed.lines() {
        if line.contains("dlx_conn_rate") || line.contains("dlx_conn_count") {
            if let Some(h) = line.rsplit("# handle ").next().and_then(|x| x.trim().parse::<u32>().ok()) {
                run_ok("nft", &["delete", "rule", "ip", INGRESS_TABLE, "fwdeny", "handle", &h.to_string()]);
            }
        }
    }
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
/// Espaço de workloads (`10.200.0.0`–`10.254.255.255`, ver
/// `delonix_runtime_core::workload_net` — partilhado com `delonix-tunnel`, que usa o
/// MESMO range para o guard "no-bypass" do túnel), excepto os endereços de
/// rede/broadcast de cada /16 (`.0.0` e `.255.255`), que aqui não são IPs de
/// workload utilizáveis.
fn is_ingress_ip(ip: &str) -> bool {
    let o: Vec<&str> = ip.split('.').collect();
    if o.len() != 4 {
        return false;
    }
    let n: Vec<u8> = match o.iter().map(|x| x.parse::<u8>()).collect::<std::result::Result<_, _>>() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let addr = std::net::Ipv4Addr::new(n[0], n[1], n[2], n[3]);
    delonix_runtime_core::workload_net::is_workload_ipv4(addr) && (n[2], n[3]) != (0, 0) && (n[2], n[3]) != (255, 255)
}

/// Nome do `veth` do lado da bridge para um netns (determinístico, <= 15 chars).
fn vh_name(netns: &str) -> String {
    format!("vh{:08x}", crate::fnv32(netns))
}

/// Nome do `veth` host-side de uma rede ADICIONAL (multi-homing): distinto por
/// (netns, interface) para não colidir com o primário nem entre redes extra.
fn vh_name_extra(netns: &str, ifname: &str) -> String {
    format!("vx{:08x}", crate::fnv32(&format!("{netns}/{ifname}")))
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
pub fn fw_chain_body(ip: &str, fw: &delonix_runtime_core::ContainerFw) -> String {
    let mut body = String::new();
    if !fw.enabled {
        return body; // chain vazia = aberto (comportamento anterior a fw/namespace)
    }
    for r in &fw.rules {
        // Defesa contra injeção nft: salta regras com campos inseguros
        // (src/proto/port são interpolados na ruleset alimentada a `nft -f`).
        if !r.nft_safe() {
            continue;
        }
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
    // Isolamento de NAMESPACE na ENTRADA — só quando NÃO há política de entrada
    // explícita (uma Dependency/Ingress é autoritativa e substitui isto): aceita a
    // mesma namespace e dropa NOVAS ligações de containers de OUTRA namespace. O
    // `ct state new` isenta o retorno (established/related), e o `@dlxall` limita o
    // drop a fontes que SÃO containers da SDN (deixa passar gateway/DNS/internet).
    // As regras EXPLÍCITAS acima têm precedência (first-match terminal na chain).
    let has_explicit_in = fw.policy_in == "deny" || fw.rules.iter().any(|r| r.dir == "in");
    if !has_explicit_in {
        let nsset = dlxns_set(&fw.namespace);
        body.push_str(&format!("\t\tip daddr {ip} ip saddr @{nsset} accept\n"));
        body.push_str(&format!("\t\tip daddr {ip} ip saddr @{DLXALL_SET} ct state new drop\n"));
    }
    if fw.policy_in == "deny" {
        body.push_str(&format!("\t\tip daddr {ip} drop\n"));
    }
    if fw.policy_out == "deny" {
        body.push_str(&format!("\t\tip saddr {ip} drop\n"));
    }
    body
}

/// Set nft com TODOS os IPs de containers da SDN (para o isolamento de namespace
/// só afetar tráfego container↔container, não gateway/DNS/internet).
pub const DLXALL_SET: &str = "dlxall";

/// Nome (curto, ≤ limite do nft) do set de IPs de uma namespace lógica.
pub fn dlxns_set(ns: &str) -> String {
    format!("dlxns{:08x}", crate::fnv32(ns))
}

/// Aplica a firewall de um container no `dlxing` (corre no holder): garante a chain
/// `fw<hash>` + jumps no `fwd` (daddr/saddr==ip), e reconstrói o corpo. `hex` é o
/// JSON da `ContainerFw` em hexadecimal (o canal de controlo é por linhas).
fn do_firewall(ip: &str, hex: &str) -> Result<()> {
    if !is_ingress_ip(ip) {
        return Err(Error::Invalid(format!("IP {ip} fora do espaço de ingress (10.200-254.x)")));
    }
    let bytes = hex_decode(hex).ok_or_else(|| Error::Invalid("hex inválido".into()))?;
    let fw: delonix_runtime_core::ContainerFw =
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
    let fwd_chain = crate::capture("nft", &["list", "chain", "ip", INGRESS_TABLE, "fwdeny"]).unwrap_or_default();
    for dir in ["daddr", "saddr"] {
        if !fwd_chain.contains(&format!("ip {dir} {ip} jump {chain}")) {
            run_ok("nft", &["add", "rule", "ip", INGRESS_TABLE, "fwdeny", "ip", dir, ip, "jump", &chain]);
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
    if let Ok(out) = crate::capture("nft", &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"]) {
        for line in out.lines() {
            if line.contains(&format!("jump {chain}")) {
                if let Some(h) = line.rsplit("handle ").next().map(|s| s.trim()) {
                    run_ok("nft", &["delete", "rule", "ip", INGRESS_TABLE, "fwdeny", "handle", h]);
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
    /// Intenção de egress da rede, PERSISTIDA para sobreviver ao respawn do
    /// holder (o nft e o registry FQDN vivem num netns efémero). Re-aplicada em
    /// [`ensure_net_bridge`] quando a bridge é recriada.
    #[serde(default)]
    pub egress: EgressState,
}

/// Política de egress de uma rede, guardada na [`NetDef`].
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct EgressState {
    /// `deny` | `allow` | `allowlist:<cidrs>`. `None` = default (allow).
    #[serde(default)]
    pub policy: Option<String>,
    /// Sufixos FQDN permitidos (`egress host`).
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// Actualiza (e persiste) a intenção de egress da rede cuja bridge é `bridge`,
/// devolvendo o estado resultante. `None` se nenhuma `NetDef` corresponder (ex.:
/// a bridge default `delonix0`, que não é persistida).
fn update_netdef_egress(bridge: &str, mutate: impl FnOnce(&mut EgressState)) -> Option<EgressState> {
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

/// Constrói a chain de egress COMPLETA de uma bridge a partir do estado combinado
/// (política CIDR + hosts FQDN), para que `egress net allowlist` e `egress host`
/// COMPONHAM em vez de um reprogramar por cima do outro. Remove as regras antigas
/// da bridge e reinsere na ordem certa: DNS → CIDRs → @set FQDN → drop. `allow`
/// sem hosts = default-allow (nada). `deny` sem hosts = drop total. Qualquer host
/// força modo allowlist (os hosts são allows explícitos).
fn apply_egress_from_state(bridge: &str, state: &EgressState) -> Result<()> {
    let bridge = sanitize(bridge);
    // Remove todas as regras de egress antigas desta bridge (drop + accepts).
    let needle_if = format!("iifname \"{bridge}\"");
    let listed = crate::capture("nft", &["-a", "list", "chain", "ip", INGRESS_TABLE, "fwdeny"]).unwrap_or_default();
    for line in listed.lines() {
        if line.contains(&needle_if) && line.contains("oifname \"tap0\"") && (line.contains("drop") || line.contains("accept")) {
            if let Some(h) = line.rsplit("# handle ").next().and_then(|x| x.trim().parse::<u32>().ok()) {
                run_ok("nft", &["delete", "rule", "ip", INGRESS_TABLE, "fwdeny", "handle", &h.to_string()]);
            }
        }
    }
    // Cria o set FQDN + regista os sufixos ANTES de inserir a regra `@set`.
    if !state.hosts.is_empty() {
        let set = fqdn_set(&bridge);
        run_ok("nft", &["add", "set", "ip", INGRESS_TABLE, &set, "{ type ipv4_addr; flags timeout; }"]);
        fqdn_register(&bridge, &set, &state.hosts);
    }
    // `insert` prepende → inserir em ordem INVERSA para o topo→fundo ficar certo.
    for spec in egress_specs(&bridge, state).iter().rev() {
        run("nft", &spec.iter().map(|s| s.as_str()).collect::<Vec<_>>())?;
    }
    Ok(())
}

/// Constrói os arg-vectors `nft insert rule …` do egress de uma bridge a partir do
/// estado combinado (política CIDR + hosts FQDN), na ordem topo→fundo. **PURA**
/// (sem I/O — testável): DNS → CIDRs da allowlist → `@set` FQDN → drop. `allow`
/// sem hosts → vazio (default-allow); `deny` sem hosts → só drop. `bridge` já vem
/// sanitizado.
fn egress_specs(bridge: &str, state: &EgressState) -> Vec<Vec<String>> {
    let policy = state.policy.as_deref().unwrap_or("allow");
    let has_hosts = !state.hosts.is_empty();
    let base = |extra: &[&str]| -> Vec<String> {
        let mut v = vec!["insert".into(), "rule".into(), "ip".into(), INGRESS_TABLE.into(), "fwdeny".into(), "iifname".into(), bridge.to_string(), "oifname".into(), "tap0".into()];
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    if policy == "allow" && !has_hosts {
        return Vec::new();
    }
    if policy == "deny" && !has_hosts {
        return vec![base(&["drop"])];
    }
    let mut specs = vec![base(&["udp", "dport", "53", "accept"]), base(&["tcp", "dport", "53", "accept"])];
    if let Some(cidrs) = policy.strip_prefix("allowlist:") {
        for cidr in cidrs.split(',').map(|c| c.trim()).filter(|c| !c.is_empty()) {
            if delonix_runtime_core::fw_src_ok(cidr) {
                specs.push(base(&["ip", "daddr", cidr, "accept"]));
            } else {
                eprintln!("delonix: egress allowlist — CIDR inválido saltado: {cidr:?}");
            }
        }
    }
    if has_hosts {
        specs.push(base(&["ip", "daddr", &format!("@{}", fqdn_set(bridge)), "accept"]));
    }
    specs.push(base(&["drop"])); // default-deny do resto (fica em ÚLTIMO)
    specs
}

/// IPs actualmente no set FQDN de uma bridge (aprendidos das respostas DNS).
/// Corre DENTRO do holder (o set vive no netns de infra). Extrai os IPv4 do dump.
fn egress_set_members(bridge: &str) -> Vec<String> {
    let set = fqdn_set(&sanitize(bridge));
    let dump = crate::capture("nft", &["list", "set", "ip", INGRESS_TABLE, &set]).unwrap_or_default();
    let mut ips = Vec::new();
    for tok in dump.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        if tok.split('.').filter(|o| !o.is_empty()).count() == 4 && tok.parse::<std::net::Ipv4Addr>().is_ok() {
            ips.push(tok.to_string());
        }
    }
    ips.sort();
    ips.dedup();
    ips
}

/// IPs FQDN aprendidos ao vivo para uma bridge — pergunta ao holder (`egress show`
/// do lado do CLI). Vazio se o holder estiver em baixo.
pub fn egress_members(bridge: &str) -> Vec<String> {
    // `control_query` já devolve o corpo (sem o prefixo `ok `).
    match control_query(&format!("egress-show {bridge}")) {
        Ok(body) => body.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
        Err(_) => Vec::new(),
    }
}

/// Regista (sem duplicar) os sufixos FQDN de uma bridge no [`FQDN_ALLOW`] para a
/// thread de DNS os snoopar. Chamado no apply e na re-aplicação pós-respawn.
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
        egress: EgressState::default(),
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
        egress: EgressState::default(),
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

/// **Liga um container via CNI (rootless)**: garante a infra de pé (ref-count++) e
/// pede ao holder para correr os plugins CNI (`conf_json` = conflist) na netns do
/// container. Devolve `(netns, ip_cidr)`. O IP vem do IPAM do plugin. Em falha
/// desfaz o ref-count. Preserva o rootless-first: o plugin corre no holder (dono
/// da netns), não no host sem privilégio.
pub fn cni_attach_container(id: &str, conf_json: &str) -> Result<(String, String)> {
    acquire()?; // ensure_up + refcount++
    let netns = sanitize(id);
    let hex = hex_encode(conf_json.as_bytes());
    let cmd = format!("cni-add {netns} {netns} {} {hex}", crate::cni::DEFAULT_IFNAME);
    match control_query(&cmd) {
        Ok(ip) => Ok((netns, ip)),
        Err(e) => {
            release();
            Err(e)
        }
    }
}

/// **Desliga um container CNI (rootless)**: pede ao holder o `DEL` dos plugins +
/// remoção da netns, e liberta o ref-count. Best-effort.
pub fn cni_detach_container(id: &str, conf_json: &str) -> Result<()> {
    let netns = sanitize(id);
    let hex = hex_encode(conf_json.as_bytes());
    let _ = control_send(&format!("cni-del {netns} {netns} {} {hex}", crate::cni::DEFAULT_IFNAME));
    release();
    Ok(())
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

/// **Liga um container A CORRER a uma rede ADICIONAL** (multi-homing ao vivo,
/// rootless): resolve a bridge/gateway/IP da rede e pede ao holder o `veth` extra
/// na interface `eth<idx>`. Sem ref-count novo (o attach primário já segura a infra).
/// Devolve `(ifname, ip)`.
pub fn attach_extra_container(id: &str, idx: u32, net: &str) -> Result<(String, String)> {
    let (bridge, prefix, gateway) = resolve_net(net)?;
    let ip = container_ip_on(&prefix, id);
    let ifname = format!("eth{idx}");
    let netns = sanitize(id);
    control_send(&format!("attach-extra {netns} {ifname} {ip} {bridge} {gateway}"))?;
    Ok((ifname, ip))
}

/// **Limita a largura de banda de um container A CORRER** (rootless, ao vivo):
/// pede ao holder o shaping no veth do lado do infra (`vh<fnv>`). `rate_bit` em
/// bit/s, `burst_bytes` em bytes. Idempotente.
pub fn set_net_rate(id: &str, rate_bit: u64, burst_bytes: u64) -> Result<()> {
    let vh = vh_name(&sanitize(id));
    control_send(&format!("netrate {vh} {rate_bit} {burst_bytes}"))
}

/// **Remove o limite de largura de banda** de um container (rootless). Best-effort.
pub fn clear_net_rate(id: &str) {
    let vh = vh_name(&sanitize(id));
    let _ = control_send(&format!("netrate-clear {vh}"));
}

/// **Desliga um container de uma rede adicional** (multi-homing ao vivo): pede ao
/// holder a remoção do `veth` extra. Best-effort.
pub fn detach_extra_container(id: &str, idx: u32) {
    let netns = sanitize(id);
    let ifname = format!("eth{idx}");
    let _ = control_send(&format!("detach-extra {netns} {ifname}"));
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
pub fn apply_firewall(id: &str, ip: &str, fw: &delonix_runtime_core::ContainerFw) -> Result<()> {
    let json = serde_json::to_vec(fw).map_err(|e| Error::Invalid(e.to_string()))?;
    control_send(&format!("firewall {} {} {}", sanitize(id), ip, hex_encode(&json)))
}

/// Define a política GLOBAL de egress do ingress único (via holder, no netns de
/// infra). `deny` bloqueia toda a saída para a Internet; `allow` repõe o default
/// (egress permitido). Idempotente.
pub fn set_egress_policy(deny: bool) -> Result<()> {
    control_send(&format!("egress {}", if deny { "deny" } else { "allow" }))
}

/// Como [`set_egress_policy`], mas SÓ para a bridge `<bridge>` (egress por-rede /
/// por-workspace). Não afeta as outras redes.
pub fn set_egress_policy_net(bridge: &str, deny: bool) -> Result<()> {
    control_send(&format!("egress-net {} {}", bridge, if deny { "deny" } else { "allow" }))
}

/// NET-A — egress em modo ALLOWLIST para a bridge `<bridge>`: nega toda a saída→
/// Internet EXCEPTO DNS (53) e os `cidrs` indicados (lista separada por vírgulas,
/// sem espaços). É o "nega tudo excepto X" que faltava (o `set_egress_policy_net`
/// é só denylist). Os CIDRs são validados (`fw_src_ok`) no holder — anti-injeção.
pub fn set_egress_policy_net_allowlist(bridge: &str, cidrs: &[&str]) -> Result<()> {
    control_send(&format!("egress-net {} allowlist:{}", bridge, cidrs.join(",")))
}

/// Egress por HOSTNAME: só deixa a bridge sair para os IPs que resolverem para
/// `<suffix>` (ou `*.<suffix>`), aprendidos ao vivo das respostas DNS. Nega o
/// resto (excepto DNS). Chamar mais que uma vez acrescenta hostnames à allowlist.
pub fn set_egress_host(bridge: &str, suffix: &str) -> Result<()> {
    control_send(&format!("egress-host {bridge} {suffix}"))
}

/// Ativa/atualiza a proteção DDoS L4 (rate-limit + ct-count por-origem). `conn_rate`
/// = novas ligações/segundo por IP; `conn_max` = ligações concorrentes por IP.
/// best-effort no holder (degrada se o kernel não suportar). Ver [`do_l4guard`].
pub fn set_l4_guard(conn_rate: u32, conn_max: u32) -> Result<()> {
    control_send(&format!("l4guard {conn_rate} {conn_max}"))
}

/// Remove a proteção DDoS L4 (idempotente).
pub fn clear_l4_guard() -> Result<()> {
    control_send("l4guard-clear")
}

/// Sobe a interface WireGuard `<iface>` no netns de infra (req #6) com a privada
/// do nó e a porta de escuta. A privada vai pelo control socket (0600 + SO_PEERCRED
/// = só o uid do engine). Ver [`crate::wg`].
pub fn set_wg_iface(iface: &str, private_key: &str, listen_port: u16, addr_cidr: &str) -> Result<()> {
    control_send(&format!("wg-up {iface} {listen_port} {private_key} {addr_cidr}"))
}

/// Adiciona um peer WireGuard (outro nó) à interface do overlay.
pub fn set_wg_peer(iface: &str, public_key: &str, endpoint: &str, allowed_ips: &[String]) -> Result<()> {
    control_send(&format!("wg-peer {iface} {public_key} {endpoint} {}", allowed_ips.join(",")))
}

/// **Realiza o uplink VXLAN de uma rede overlay** no netns de infra: bridge +
/// device VXLAN (`<dev>`/`<vni>`) + FDB dos pares (`dsts` = `wg_ip` se cifrado,
/// senão `node_ip`). O gateway alinha a subnet à decidida pelo `NetworkStore`.
/// Requer o holder de pé (`ensure_up` antes). Idempotente. Ver [`do_vxlan`].
pub fn set_vxlan(dev: &str, vni: u32, bridge: &str, gateway: &str, dsts: &[String]) -> Result<()> {
    // Valida os destinos AQUI, ANTES de os interpolar na linha do control-socket
    // (disciplina valid_* da auditoria — validar antes do `format!`/socket, não só
    // holder-side): um dst com espaço/newline malformaria a linha ou tentaria
    // smuggling de um 2.º comando. `do_vxlan` revalida, mas a fronteira é esta.
    if let Some(bad) = dsts.iter().find(|d| !valid_fdb_dst(d)) {
        return Err(Error::Invalid(format!("destino de par overlay inválido: {bad:?} (só IPs)")));
    }
    // CSV num único token (o control-loop faz `split_whitespace`); `-` = sem pares.
    let csv = if dsts.is_empty() { "-".to_string() } else { dsts.join(",") };
    control_send(&format!("vxlan {dev} {vni} {bridge} {gateway} {csv}"))
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

/// Like [`infra_join_argv`] but enters ONLY the net namespace (`-n`), keeping the
/// caller's user namespace and its init-ns capabilities. This is what a
/// privileged caller (root/`CAP_BPF`) needs to load an eBPF program into the
/// infra netns: entering the holder's userns (`-U`) would namespace the caps
/// away and the `bpf()` syscall would be refused. `None` if the holder is down.
pub fn infra_netns_argv() -> Option<Vec<String>> {
    let holder = read_pid(&holder_pid_path()).filter(|&p| pid_alive(p))?;
    Some(vec!["nsenter".into(), "-t".into(), holder.to_string(), "-n".into(), "--".into()])
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

/// Como [`publish_port`], mas restringe o acesso à `host_port` a uma **allowlist**
/// de CIDRs (firewall inbound): o resto é dropado antes do DNAT. `spec` é
/// `hostPort:contPort[/proto]`; `cidrs` são validados no holder (`fw_src_ok`).
/// Usado para expor a DB de uma app só a IPs autorizados.
pub fn publish_port_allow(cip: &str, spec: &str, cidrs: &[&str]) -> Result<()> {
    let (host_port, cont_port, proto) = crate::parse_publish(spec)?;
    crate::slirp_add_hostfwd(&slirp_sock_path(), &host_port, &host_port, &proto)?;
    let csv = cidrs.join(",");
    control_send(&format!("publish-allow {proto} {host_port} {cip} {cont_port} {csv}"))
}

/// Remove a publicação de uma `host_port`: tira o `add_hostfwd` do slirp e o DNAT
/// da chain `pre`. Best-effort.
pub fn unpublish_port(host_port: &str) {
    trace_unpublish("unpublish_port", host_port);
    let _ = slirp_remove_hostfwd(&slirp_sock_path(), host_port);
    let _ = control_send(&format!("unpublish {host_port}"));
}

/// Regista quem despublicou uma porta, quando `DELONIX_TRACE_UNPUBLISH` está
/// definido (aponta para um ficheiro; senão vai para o stderr).
///
/// Não é debug esquecido no código: há um bug em aberto em que hostfwds de
/// containers VIVOS desaparecem sem `stop`/`rm`, e a pergunta que o fecha é
/// "quem os removeu?". Um binário de longa duração (holder, supervisor de
/// `--restart`, log shim) continua a correr o código de quando NASCEU, por isso
/// a resposta não se obtém a ler o repo — só instrumentando e reproduzindo.
/// Custo zero quando a env var não está definida.
pub fn trace_unpublish(func: &str, host_port: &str) {
    let Ok(dest) = std::env::var("DELONIX_TRACE_UNPUBLISH") else { return };
    let pid = std::process::id();
    let exe = std::fs::read_link("/proc/self/exe").map(|p| p.display().to_string()).unwrap_or_default();
    let ppid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("PPid:")).map(|l| l.trim_start_matches("PPid:").trim().to_string()))
        .unwrap_or_default();
    let bt = std::backtrace::Backtrace::force_capture();
    let line = format!("[trace_unpublish] {func}(port={host_port}) pid={pid} ppid={ppid} exe={exe}\n{bt}\n");
    if dest == "1" || dest == "stderr" {
        eprint!("{line}");
    } else if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&dest) {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

/// Reconcilia os `hostfwd` do slirp ÚNICO do ingress contra os ports REALMENTE em
/// uso por containers vivos: remove as entradas órfãs (de containers já removidos,
/// ou que morreram sem limpar) que de outro modo bloqueavam o re-uso da porta de
/// host. `live_ports` = host_ports publicados por containers vivos. Parte do reaper
/// #1 (port-leak). Devolve quantas removeu. Barato (1 query ao api-socket).
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
    // A resposta vem como {"entries":[…]} ou {"return":{"entries":[…]}} conforme a versão.
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

/// Envia um comando JSON ao api-socket do slirp único e devolve a resposta.
fn slirp_api(sock: &Path, json: &str) -> Result<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    // Ponto de estrangulamento de TODOS os comandos ao slirp — inclusive os
    // `remove_hostfwd` que o `reap_orphan_hostfwds` envia directamente, sem
    // passar por `slirp_remove_hostfwd`. Instrumentar só as funções nomeadas
    // deixava esse caminho invisível.
    if !json.contains("list_hostfwd") {
        trace_unpublish("slirp_api", json);
    }
    let mut s = UnixStream::connect(sock).map_err(|e| Error::Runtime {
        context: "slirp api",
        message: e.to_string(),
    })?;
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    // O `\n` é OBRIGATÓRIO: o slirp4netns só PARSEIA o comando (e responde) ao ver
    // uma newline OU o EOF do cliente. Como aqui o cliente fica a LER a resposta
    // (`read_to_string`) sem fechar a escrita, sem o `\n` o slirp nunca parseava
    // e o `list_hostfwd` voltava VAZIO ao fim do timeout — pelo que o
    // `slirp_remove_hostfwd` não achava o `id` e NÃO removia nada. Efeito: a porta
    // de um cluster/container apagado ficava presa no ingress (visto na 6443 de um
    // `cluster delete`). O `add_hostfwd` safava-se por parsear no EOF (fire-and-
    // forget), o que escondia o bug.
    let line = if json.ends_with('\n') { json.to_string() } else { format!("{json}\n") };
    s.write_all(line.as_bytes()).map_err(|e| Error::Runtime {
        context: "slirp api write",
        message: e.to_string(),
    })?;
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
    Ok(resp)
}

/// Remove um `hostfwd` de UM slirp (o único do ingress, ou o de um container no
/// caminho slirp-por-container): descobre o `id` da entrada com aquela
/// `host_port` (via `list_hostfwd`) e remove-o.
///
/// `pub` porque o `container update` precisa de despublicar a quente uma porta
/// do slirp PRÓPRIO de um container (socket `delonix-slirp-<pid>.sock`), e não
/// só do slirp único do ingress — que é o que [`unpublish_port`] assume.
/// As entradas de um `list_hostfwd`, tolerante à FORMA da resposta.
///
/// O slirp4netns 1.2.1 responde `{"entries":[…]}` — sem o wrapper `return` que
/// envolve outras respostas (`remove_hostfwd` dá `{"return":{}}`). O parser
/// antigo procurava SÓ `return.entries` e por isso não achava nada e nunca
/// removia — a outra metade do bug do port-leak (a 1.ª era o `\n` em falta no
/// `slirp_api`). Aceita as duas formas para não voltar a partir entre versões.
fn hostfwd_entries(v: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    v.get("entries")
        .or_else(|| v.get("return").and_then(|r| r.get("entries")))
        .and_then(|e| e.as_array())
}

pub fn slirp_remove_hostfwd(sock: &Path, host_port: &str) -> Result<()> {
    trace_unpublish("slirp_remove_hostfwd", host_port);
    let hp: u32 = host_port.parse().map_err(|_| Error::Invalid("porta inválida".into()))?;
    let listed = slirp_api(sock, r#"{"execute":"list_hostfwd"}"#)?;
    let v: serde_json::Value = serde_json::from_str(&listed).unwrap_or(serde_json::Value::Null);
    if let Some(entries) = hostfwd_entries(&v) {
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

/// Bytes rx/tx do `eth0` de um container rootless, lidos DE DENTRO do seu netns
/// (via `join_argv`). Do ponto de vista do container, `rx`=download e `tx`=upload
/// (sem a troca do modelo root, onde se lê o veth do lado do host). Devolve
/// `(download, upload)` ou `None` se a infra/container não estiver de pé.
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

/// Envia um comando ao socket de controlo do holder e espera `ok`. Tenta
/// brevemente até o socket existir (o holder cria-o ao arrancar).
fn control_send(cmd: &str) -> Result<()> {
    // Só os comandos que DESFAZEM estado — o trace serve para responder a "quem
    // desligou isto?", e um log de todos os attach/publish afogaria a resposta.
    if cmd.starts_with("unpublish") || cmd.starts_with("detach") || cmd.starts_with("unfirewall") {
        trace_unpublish("control_send", cmd);
    }
    control_query(cmd).map(|_| ())
}

/// Como `control_send`, mas devolve o CORPO da resposta após `ok ` (vazio se só
/// `ok`). Usado pelo `cni-add`, cuja resposta carrega o IP atribuído pelo IPAM.
fn control_query(cmd: &str) -> Result<String> {
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
    // Proteção DDoS L4 por omissão (req #5): rate-limit + ct-count POR-ORIGEM.
    // Limites conservadores (tráfego legítimo não é afetado), best-effort e com
    // pré-flight `nft -c` (degrada em kernels sem `meter`). Configurável via API.
    let _ = do_l4guard(50, 200);
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
    // Nome externo: reencaminha e, se estiver numa allowlist FQDN, aprende os
    // A-records da resposta para o set nft do egress (antes de a devolver).
    let resp = forward_dns(q)?;
    snoop_fqdn(&name, &resp);
    Some(resp)
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
    #[test]
    fn parse_a_records_extracts_ipv4_answers() {
        // Resposta DNS para `example.com` com dois A-records (name compression no
        // answer via ponteiro 0xc00c), mais um AAAA que deve ser ignorado.
        let resp: Vec<u8> = vec![
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, // header: QD=1 AN=3
            7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00, 0x01, // Q
            0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1e, 0x00, 0x04, 93, 184, 216, 34, // A
            0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1e, 0x00, 0x04, 1, 2, 3, 4, // A
            0xc0, 0x0c, 0x00, 0x1c, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1e, 0x00, 0x10, // AAAA (16 bytes rdata)
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ];
        let ips = super::parse_a_records(&resp);
        assert_eq!(ips, vec![[93, 184, 216, 34], [1, 2, 3, 4]]);
    }

    #[test]
    fn hostfwd_entries_aceita_as_duas_formas() {
        // slirp4netns 1.2.1: {"entries":[…]} (sem wrapper). Outras versões podem
        // envolver em {"return":{"entries":[…]}}. Ambas têm de funcionar, senão o
        // remove nunca acha o id → porta presa.
        let a: serde_json::Value = serde_json::from_str(r#"{"entries":[{"id":1,"host_port":6443}]}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"return":{"entries":[{"id":2,"host_port":80}]}}"#).unwrap();
        assert_eq!(super::hostfwd_entries(&a).map(|e| e.len()), Some(1));
        assert_eq!(super::hostfwd_entries(&b).map(|e| e.len()), Some(1));
        let empty: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert!(super::hostfwd_entries(&empty).is_none());
    }

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
    fn egress_specs_compoem_cidrs_e_fqdn() {
        use super::EgressState;
        let st = |policy: Option<&str>, hosts: &[&str]| EgressState {
            policy: policy.map(String::from),
            hosts: hosts.iter().map(|s| s.to_string()).collect(),
        };
        // allow, sem hosts → nenhuma regra (default-allow).
        assert!(super::egress_specs("dlx1", &st(None, &[])).is_empty());
        // deny, sem hosts → um só drop.
        let d = super::egress_specs("dlx1", &st(Some("deny"), &[]));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].last().unwrap(), "drop");
        // allowlist + host COMPÕEM: 2xDNS + 1 CIDR válido + @set + drop (CIDR mau saltado).
        let a = super::egress_specs("dlx1", &st(Some("allowlist:1.1.1.0/24,lixo;rm"), &["github.com"]));
        assert_eq!(a.len(), 5, "2xDNS + 1 CIDR + @set FQDN + drop");
        assert!(a[2].contains(&"1.1.1.0/24".to_string()));
        assert!(a[3].iter().any(|x| x.starts_with("@dlxfq")), "a regra @set do FQDN está presente");
        assert_eq!(a[4].last().unwrap(), "drop");
        // só host (sem política CIDR) → 2xDNS + @set + drop.
        let h = super::egress_specs("dlx1", &st(None, &["example.com"]));
        assert_eq!(h.len(), 4);
        assert!(h[2].iter().any(|x| x.starts_with("@dlxfq")));
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
        let fw = delonix_runtime_core::ContainerFw {
            enabled: true,
            policy_in: "deny".into(),
            policy_out: "allow".into(),
            rules: vec![
                delonix_runtime_core::FwRule { dir: "in".into(), proto: "tcp".into(), port: "8080".into(), src: "10.200.0.0/16".into(), action: "allow".into(), note: String::new() },
                delonix_runtime_core::FwRule { dir: "out".into(), proto: "any".into(), port: String::new(), src: String::new(), action: "deny".into(), note: String::new() },
            ],
            namespace: "default".into(),
        };
        let body = fw_chain_body("10.200.0.5", &fw);
        // regra in: daddr==ip, peer saddr==src, tcp dport 8080 accept
        assert!(body.contains("ip daddr 10.200.0.5 ip saddr 10.200.0.0/16 tcp dport 8080 accept"), "{body}");
        // regra out: saddr==ip, drop (proto any → sem proto/dport)
        assert!(body.contains("ip saddr 10.200.0.5 drop"), "{body}");
        // política in=deny → drop final no daddr
        assert!(body.contains("ip daddr 10.200.0.5 drop"), "{body}");
        // política de entrada EXPLÍCITA (deny) → NÃO emite regras de namespace.
        assert!(!body.contains("@dlxall"), "{body}");
        // disabled → corpo vazio
        let off = delonix_runtime_core::ContainerFw { enabled: false, ..fw };
        assert!(fw_chain_body("10.200.0.5", &off).is_empty());
    }

    #[test]
    fn fw_body_emits_namespace_isolation_when_no_explicit_ingress() {
        // enabled, sem regras de entrada e policy_in != deny → isolamento de namespace.
        let fw = delonix_runtime_core::ContainerFw {
            enabled: true,
            namespace: "web".into(),
            ..Default::default()
        };
        let body = fw_chain_body("10.200.0.7", &fw);
        let nsset = dlxns_set("web");
        // same-ns accept + cross-ns (container) NEW drop, com ct state new.
        assert!(body.contains(&format!("ip daddr 10.200.0.7 ip saddr @{nsset} accept")), "{body}");
        assert!(body.contains("ip daddr 10.200.0.7 ip saddr @dlxall ct state new drop"), "{body}");
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
    fn valid_fdb_dst_accepts_only_ips() {
        // IPv4/IPv6 textuais — aceites.
        assert!(valid_fdb_dst("10.0.0.1"));
        assert!(valid_fdb_dst("192.168.1.254"));
        assert!(valid_fdb_dst("fd00::1"));
        assert!(valid_fdb_dst("2001:db8::a2f"));
        // Injeção / lixo — recusado (o dst vai a argv do `bridge fdb`, mas mantemos
        // a disciplina valid_* da auditoria: nada com espaço/`;`/`|`/`$` passa).
        assert!(!valid_fdb_dst(""));
        assert!(!valid_fdb_dst("10.0.0.1; rm -rf /"));
        assert!(!valid_fdb_dst("$(curl evil)"));
        assert!(!valid_fdb_dst("10.0.0.1 dev eth0"));
        assert!(!valid_fdb_dst(&"a".repeat(46))); // acima do teto IPv6 textual
    }

    #[test]
    fn fdb_presence_is_exact_token_not_substring() {
        // A saída real do `bridge fdb show`: cada destino é um token isolado.
        let have = "00:00:00:00:00:00 dst 10.0.0.50 self permanent\n\
                    1a:2b:3c:4d:5e:6f master br0 permanent";
        let present = |dst: &str| have.lines().any(|l| l.split_whitespace().any(|t| t == dst));
        assert!(present("10.0.0.50")); // presente de facto
        assert!(!present("10.0.0.5")); // NÃO presente — apesar de ser substring de 10.0.0.50
    }

    #[test]
    fn set_vxlan_empty_peers_uses_sentinel_token() {
        // Sem pares, o CSV colapsaria a nada e o control-loop (split_whitespace)
        // veria 5 tokens em vez de 6 — o sentinela `-` mantém a aridade. (Não toca
        // no holder: só validamos a forma do comando, montando-o à mão como o wrapper.)
        let dsts: Vec<String> = Vec::new();
        let csv = if dsts.is_empty() { "-".to_string() } else { dsts.join(",") };
        assert_eq!(csv, "-");
        let cmd = format!("vxlan dlxvx0042 66 dlxn0000002a 10.201.0.1 {csv}");
        assert_eq!(cmd.split_whitespace().count(), 6);
        // Com pares, um único token CSV (sem espaços) preserva a aridade.
        let csv2 = ["10.0.0.2".to_string(), "10.0.0.3".to_string()].join(",");
        let cmd2 = format!("vxlan dlxvx0042 66 dlxn0000002a 10.201.0.1 {csv2}");
        assert_eq!(cmd2.split_whitespace().count(), 6);
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
