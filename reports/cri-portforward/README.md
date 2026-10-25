# CRI PortForward — conformância critest (A/B, mesma base)

critest **v1.36.0**, `-ginkgo.focus "portforward"`, contra um `delonix-cri`
isolado (WORK + `DELONIX_ROOT` próprios, holder de rede livre), host rootless.
A/B na **mesma base** (`origin/main` @ `ab4751ac`): só muda o binário.

Specs em foco:
- `runtime should support portforward [Conformance]` (netns normal)
- `runtime should support portforward in host network`

## Antes (binário de main, sem fix) — `before.log`

```
FAIL! -- 0 Passed | 2 Failed | 0 Pending | 120 Skipped
```

- **in host network** → `error upgrading connection: unable to upgrade connection: pod sandbox has no live netns`.
  Bug de código: um pod `hostNetwork` não tem infra/netns próprio, e o
  `portforward_upgrade` exigia o pid do infra `pod-cri-<id>` (inexistente).
- **[Conformance]** → falha ANTES do port-forward, no pré-requisito
  «ensure the web server is serving»: `dial tcp 10.200.0.2:80: i/o timeout`
  (networking.go:305). É alcance do **IP do pod** a partir do host — topologia
  **rootless** (slirp não roteável do host), não o código de port-forward.

## Depois (binário com o fix) — `after.log`

```
FAIL! -- 1 Passed | 1 Failed | 0 Pending | 120 Skipped
```

- **in host network** → **PASSA** (verde). O forward passou a ligar em
  `127.0.0.1:<port>` no netns do host (`PfNetns::Host`) em vez de `setns` a um
  netns de pod que nunca existiu.
- **[Conformance]** → continua a falhar no MESMO pré-requisito ambiental
  (`dial 10.200.0.2:80 i/o timeout`), idêntico antes/depois — independente do
  port-forward. Exercitar o caminho de dados do port-forward normal exige um
  ambiente com IP de pod roteável (root/CNI ou nó kubeadm); fica fora deste fix.

## O fix

`crates/interfaces/delonix-cri/src/`:
- `streaming.rs` — `portforward_upgrade` ramifica por `sandbox_is_host_network`:
  host-network → `PfNetns::Host`; pod normal → pid do infra (erro "no live
  netns" mantém-se para infra morto, que é o caso correcto).
- `spdy.rs` — `PfNetns { Pid(i32), Host }` + `connect_pf`: `Host` liga directo,
  `Pid` faz `setns` como antes.
- `runtime_svc/lifecycle.rs` — `pub fn sandbox_is_host_network(base, id)`.

---

## PortForward [Conformance] (netns normal) — A/B na via ROOT + CNI

O spec `portforward [Conformance]` nunca ficava verde no host rootless porque
falhava ANTES do port-forward, no pré-requisito «web server is serving» (o IP do
pod não é roteável do host em slirp/pasta). Para o exercitar é preciso um ambiente
com **IP de pod roteável**. Em CI falha DEPOIS do pré-requisito, no forward em si
(`streaming.go`) — logo há um bug de código escondido atrás do ambiente.

### Ambiente roteável (sem root no host)

Sudo e root no host estão vedados ao arnês. A via limpa: correr o `delonix-cri`
como **root DENTRO de um container `--privileged`** (grupo `docker`), onde ele
toma a via ROOT + CNI (bridge real, veth, IPAM `host-local`) e os IPs de pod são
roteáveis na netns do container. Arnês: `scripts/critest-root-cni.sh` (base
`ubuntu:24.04` p/ bater o glibc 2.39 do host; binários+critest+crictl+plugins CNI
montados; conflist bridge+portmap; `DELONIX_ROOT` num **volume** p/ fugir ao
overlay-on-overlay; dança de delegação de cgroup v2 p/ o controlador `memory`).
A/B na **mesma base** (`cab3fcdf`): só muda o binário (o diff deste relatório).

### Antes (sem o fix de named-netns) — `before-root-cni.log`

```
FAIL! -- 0 Passed | 1 Failed | 0 Pending | 121 Skipped
```

Pré-requisito **PASSA** (`the IP:port is http://10.88.0.2:80` → `check port
mapping succeed` — IP de pod roteável, sem i/o timeout). O forward **FALHA**:
`start port forward` → `error upgrading connection: ... pod sandbox has no live
netns` (`streaming.go:391`). É a falha de CI, reproduzida ao vivo.

### Depois (com o fix) — `after-root-cni.log`

```
SUCCESS! -- 1 Passed | 0 Failed | 0 Pending | 121 Skipped
```

O caminho de dados corre ponta-a-ponta: `start port forward` liga, e
`check if we can get nginx main page via localhost:12001` → `Check port forward
URL ... succeed`. Os bytes atravessaram a netns nomeada do pod e voltaram.

### A causa-raiz e o fix

Na via **ROOT + CNI**, o sandbox é uma netns NOMEADA no host
(`/run/netns/cri-<id>`, gravada em `SandboxRec.cni_netns`) e os containers do pod
juntam-se a ela com `--net host` — **não existe infra `pod-cri-<id>`** com um pid.
Mas o `portforward_upgrade` resolvia a netns SÓ pelo pid do infra
(`pod_sandbox_pid`), que nessa via nunca existe → `CONFLICT "pod sandbox has no
live netns"` para um pod perfeitamente vivo.

- `spdy.rs` — `PfNetns` ganha `NsPath(PathBuf)`; `connect_in_netns` passa a abrir
  um **caminho de netns** (`/proc/<pid>/ns/net` OU `/run/netns/cri-<id>`) e
  `setns`-a nele; `connect_pf` mapeia `Pid`→`/proc/<pid>/ns/net`,
  `NsPath`→o ficheiro nomeado, `Host`→directo. (`PfNetns` deixa de ser `Copy`.)
- `lifecycle.rs` — `pub fn sandbox_cni_netns(base, id) -> Option<String>` lê o
  `cni_netns` (vazio em toda a outra via).
- `streaming.rs` — `portforward_upgrade` ramifica: host-network → `Host`; senão
  `cni_netns` presente → `NsPath`; senão pid do infra (com o "no live netns" real
  do infra morto intacto).
