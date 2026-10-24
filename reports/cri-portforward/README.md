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
