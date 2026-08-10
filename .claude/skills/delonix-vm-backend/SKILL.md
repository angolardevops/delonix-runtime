---
name: delonix-vm-backend
description: Como acrescentar um backend de VM ao delonix-runtime (ADR-0008) — o registo de backends que substitui o `match` privado do `backend_for`, e a disciplina que um backend REMOTO (Proxmox VE) tem de cumprir antes de existir. Usa quando o utilizador pedir um backend novo (Proxmox, Firecracker, VMware, um hypervisor qualquer), quando mexeres em `backend_for`/`VmBackend`, ou quando alguém propuser falar com um hypervisor por API.
---

# Backends de VM: o registo, e o que um backend remoto tem de provar

Decidido em [ADR-0008](../../../docs/adr/0008-proxmox-vm-backend.md): **aceite em
duas fases**. O registo entra; o backend Proxmox espera por um alvo real.

## Fase 1 — o registo (é o que se faz primeiro, sempre)

Hoje a escolha é privada e **falha aberta**:

```rust
fn backend_for(vm: &Vm) -> Box<dyn VmBackend>   // crates/delonix-vm/src/lib.rs
// match sobre dois literais, e `_ => CloudHypervisorBackend`
```

Esse `_ =>` é o guarda-rio #6 partido no sítio onde é mais provável haver um
typo. O registo fecha-o:

1. `VmBackend` já é público e implementável de fora — **não mexer no trait**.
2. Substituir o `match` por um mapa populado no arranque. O registo de libvirt e
   Cloud Hypervisor fica DENTRO do `delonix-vm`: quem já chama não vê diferença.
3. **Um nome desconhecido é erro**, com a lista dos registados. Nunca um default.
4. **O registo não é um sistema de plugins.** É um mapa populado no arranque, e
   nada mais. No dia em que alguém propuser carregar um `.so`, isto é um ADR novo.

Testável sem hypervisor nenhum: é lógica pura. Um teste que exija que um nome
desconhecido dê erro, e outro que os dois de omissão continuem a resolver.

## Fase 2 — um backend remoto, e o que ele tem de provar

**Não se escreve um backend que nunca se viu arrancar uma VM.** É a mesma regra
do spike GO/NO-GO do kind, e a razão de a fase 2 estar bloqueada: não há host
Proxmox neste sandbox. O alvo previsto é o appliance `proxmox-ve:9.1` construído
neste repo — quando um correr algures alcançável, começa-se por um spike contra
ele, não por um merge.

Quando chegar essa altura:

- **Crate próprio** (`delonix-proxmox`), a depender de `delonix-vm`. O
  `delonix-vm` tem QUATRO dependências e **não ganha nenhuma** — um cliente HTTP
  traz tokio + hyper + TLS, e isso não entra num crate de motor (guarda-rio #4).
  Confirma com `cargo tree -e normal -p delonix-vm` antes e depois.
- **API, não SSH.** Nada de `qm`/`pvesh` interpolado num shell remoto: este repo
  já pagou uma injecção de comandos por esse caminho (achado CRÍTICO #1).
- **Credenciais de `kind: Secret`**, nunca de uma flag nem de um literal no
  manifesto — o precedente é o `tokenSecretRef` do `Tunnel`.
- **Um nó, endereçado explicitamente.** Sem inventário, sem escalonamento, sem
  escolher o nó pelo utilizador. Se a decisão precisar de saber *quem é o
  cliente*, está do lado do PaaS (guarda-rio #2).
- **O que a API não sabe fazer RECUSA-SE a nomear o campo.** Hugepages,
  afinidade de CPU, volumes 9p e os escape-hatches de XML do libvirt não têm
  equivalente em Proxmox. Aceitar e descartar em silêncio é a falha que este
  repo trata como a pior de todas.

## Antes de dar por feito

Corre a skill `delonix-testing` para a disciplina de validação, e a
`delonix-runtime-sec` se o backend passar a segurar uma credencial que mexe
noutra máquina. E diz nas notas de release o que **não** foi validado ao vivo —
nunca o implícito.
