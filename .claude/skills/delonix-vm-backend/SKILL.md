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
do spike GO/NO-GO do kind — e **o spike já correu (2026-08-11): é GO.** O
appliance `proxmox-ve:9.1` deste repo, arrancado como o `verify-boot.sh` arranca
os outros (QEMU + hostfwd), serve a API e completou o ciclo inteiro: criar,
arrancar (`status: running`), snapshot, parar, destruir. A tabela
método↔operação e os achados estão no fim do [ADR-0008](../../../docs/adr/0008-proxmox-vm-backend.md).

**O que saber antes do primeiro pedido**, e que custa uma sessão a descobrir
sozinho: **quase tudo no Proxmox é uma TAREFA assíncrona.** Um create, um start,
um snapshot e um destroy respondem `UPID:pve:…` — uma string, não um resultado.
O desfecho lê-se em `/nodes/{n}/tasks/{upid}/status`, e a forma é uma armadilha:

```json
{"status": "stopped", "exitstatus": "OK"}
```

`status: stopped` quer dizer que a **tarefa acabou**, não que falhou — o
veredicto está no `exitstatus`. Quem leia o `status` como resultado conclui
exactamente o contrário da verdade. É a mesma classe de armadilha que o
`delonix-truenas` já trata (`wait_job`): partilha-se a DISCIPLINA, não o código
— os payloads não têm nada em comum.

E ainda:

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

## No roteiro de auditoria

O registo da Fase 1 é um exemplo canónico dos pontos **3 e 11** (Strategy +
Registry, e o fim de um `_ =>` que falhava aberto) — ver `delonix-engenharia`
para o resto do vocabulário e para quando NÃO abstrair. A disciplina da Fase 2 —
não publicar o que nunca se viu arrancar — é o ponto **10**. Ordem e relatório em
`delonix-auditoria`.
