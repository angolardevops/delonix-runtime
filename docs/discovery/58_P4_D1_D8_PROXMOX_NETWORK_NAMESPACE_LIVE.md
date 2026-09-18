# 58 — P4/D1,D8: `network`/`namespace` no Proxmox, confirmado ao vivo (ADR-0044)

**Data:** 2026-09-18 · **Base:** `origin/main` (`afbe4bfb`, v4.0.0 +8 commits) · **Alvo:**
`pve92`, uma VM libvirt já existente neste host (Proxmox VE 9.2.2-6-pve, criada numa
sessão anterior do ADR-0008), 192.168.122.220 · **Veredicto: o achado do D1 estava
CERTO para `network`, e ERRADO para `namespace` — o próprio ADR-0044 media a sua
Context contra um grep incompleto.**

## A pergunta

O item 5 dos spikes exigidos pelo ADR-0044 pede exactamente isto: correr
`delonix vm create --backend proxmox --network <sdn-net> --namespace teamA` contra o
`proxmox-ve:9.2` real e observar o que acontece de facto — a Context do ADR já tinha
marcado a sua própria leitura estática como "should be confirmed live before anyone
treats it as more than a strong signal".

## Como se mediu

- `pve92` arrancada (estava `Stopped`), API confirmada com `root@pam`/`delonix-admin`
  (as credenciais conhecidas dos appliances Proxmox deste repo — `answer-pve.toml`).
  Nó chama-se `pve`, uma única bridge `vmbr0` ligada a `eth0` (que por sua vez está no
  `virbr0`/192.168.122.0/24 do HOST, o mesmo NAT do libvirt — não há bridging nenhum
  entre isto e a SDN rootless do delonix, que vive dentro do netns holder deste host).
- Rede delonix própria para o spike (`p4spike-net`, `10.253.0.0/16`) — nunca uma rede de
  produção já em uso.
- `delonix vm create p4d5-proxmox-test --backend proxmox --disk local-lvm:2 --network
  p4spike-net --namespace teamA`.

## O que se mediu

1. **`--namespace teamA` foi RECUSADO, antes de qualquer chamada à API**:
   ```
   error invalid argument: namespace 'teamA' is not enforceable on the 'proxmox' backend:
   its VMs live on the host's libvirt bridge, outside the Delonix SDN, so nothing here can
   isolate them. Use `--backend cloud-hypervisor` (its VMs share the containers' SDN), or
   drop `--namespace`
   ```
   `git blame` (`crates/adapters/delonix-vm/src/lib.rs`, `vm_namespace_supported`) situa
   este guarda em **`c1ed34ec8`, 2026-08-05** — mais de um mês antes de o ADR-0044 ter
   sido escrito. `vm_namespace_supported` devolve `true` só para `"cloud-hypervisor"`,
   por isso a recusa vale para libvirt E para Proxmox, genericamente, a partir de
   `delonix-vm::create_with` — não é código do `delonix-proxmox`, e por isso o `grep`
   da Context do ADR (restrito a `crates/providers/delonix-proxmox/src/lib.rs`) nunca lhe
   chegou. **O achado "nem lido nem recusado" estava certo sobre onde procurou e errado
   na conclusão prática** — o gap que preocupava o ADR já estava fechado, só que um nível
   acima de onde a busca olhou.
2. **Sem `--namespace`, `--network p4spike-net` foi ACEITE e IGNORADO** — confirmado não
   por leitura, mas pelo formulário HTTP real que o cliente envia (instrumentado com um
   `eprintln!` temporário em `post_form`, revertido depois desta medição):
   ```
   post_form /nodes/pve/qemu form=[("vmid","101"),("name","p4d5-proxmox-test3"),
   ("memory","1024"),("cores","1"),("ostype","l26"),("scsihw","virtio-scsi-single"),
   ("scsi0","local-lvm:2"),("net0","virtio,bridge=vmbr0"),("agent","1"),
   ("ipconfig0","ip=dhcp"),("ide2","local-lvm:cloudinit")]
   ```
   `net0` é `bridge=vmbr0` — o DEFAULT do nó, nunca `p4spike-net` nem a bridge da SDN
   (`dlxn…`). `delonix vm describe p4d5-proxmox-test3` mostra `Network: p4spike-net` no
   registo LOCAL — o registo **mente sobre o que a VM realmente é**, porque
   `net0_arg` (`crates/providers/delonix-proxmox/src/lib.rs`) só lê `cfg.bridge`, nunca
   `cfg.network`. Confirmado: `grep -n "cfg\.network\b" crates/providers/delonix-proxmox`
   continua zero hits nesta árvore, tal como a Context do ADR já tinha lido.
3. **Achado lateral, não relacionado com D1/D8**: as duas primeiras tentativas (antes
   desta) falharam com `400 {"errors":{"ipconfig0":"type check ('string') failed - got
   ARRAY"}}` — o mesmo texto de um bug JÁ corrigido no código (o comentário em
   `create_form` documenta-o e há teste, `the_create_sends_each_key_once`). Reproduzido
   duas vezes, e depois de a VM `pve92` ter tido mais tempo para assentar (`pmxcfs`/
   `pveproxy` completamente prontos), a MESMA chamada, com o MESMO binário, sem qualquer
   mudança de código, teve sucesso — e o formulário instrumentado confirma que
   `ipconfig0` aparece **uma única vez**. Não é o bug já documentado a repetir-se; é uma
   corrida de arranque do próprio nó Proxmox aninhado, à parte deste ADR. Registado aqui
   para quem repetir este spike não perder tempo a persegui-lo: esperar a API responder
   não é esperar o nó estar pronto a validar SCHEMAS — os dois amadurecem em momentos
   diferentes.

## Interpretação, para D1 e D8

- **D1's field-classification table fica correcta como está**: `network`/`namespace` são
  universais por intenção (todo o provider anexa a ALGUMA rede e ALGUM domínio de
  isolamento), e hoje o Proxmox não honra nenhum dos dois — um silenciosamente, o outro
  com uma recusa explícita que já existe, só que num sítio diferente do que a Context
  tinha mapeado.
- **A "network"/"namespace" gap do D8 tem, na prática, DUAS metades com pesos
  diferentes**: a de `namespace` já está fechada (fail-closed, com uma mensagem que nomeia
  a alternativa) — D8 só precisa de **mover essa mesma recusa para dentro de
  `refuse_unsupported`** quando `VmSpec`/`Extensions` existirem, não de a inventar. A de
  `network` continua aberta e é a que exige uma decisão real: aceitar silenciosamente
  «network: p4spike-net» sobre uma VM que fica em `vmbr0` é o "aceite e ignorado" que
  este repo já corrigiu três vezes para outras flags — D8 tem de ou (a) fazer
  `delonix-provider-proxmox` recusar `network` explicitamente quando não é `default`
  (a via mais barata, e a que a topologia medida aqui recomenda: não há bridging entre a
  SDN do delonix e a rede de um Proxmox remoto, e forjar um não é trabalho deste ADR), ou
  (b) construir um mecanismo de bridging novo (o equivalente remoto do `vm bridge`
  EXPERIMENTAL já existente para CH em libvirt) — o que é um ADR à parte, não uma linha
  do D8.
- **A topologia por si só já responde à pergunta de reachability que o item 5 do spike
  pedia**, sem precisar de arrancar um SO convidado completo: `vmbr0` de `pve92` está
  bridged ao SEU `eth0`, que está no `virbr0`/192.168.122.0/24 do HOST — a MESMA rede
  onde `pve92` em si vive, e estruturalmente desligada da SDN rootless (`10.2xx.0.0/16`,
  dentro do netns holder). Uma VM criada assim NUNCA estaria "isolada nem aberta" em
  relação a um container delonix — estaria simplesmente noutra rede, sem caminho nenhum
  entre as duas, com ou sem `--namespace`. Arrancar um convidado real só confirmaria essa
  mesma ausência de caminho, já decidida pela cablagem.

## Proven vs not validated

**Proven**: os dois comportamentos medidos acima (recusa de `namespace`, aceitação-e-
ignorância de `network`), com o formulário HTTP real; a origem e a data do guarda de
`namespace` via `git blame`; a topologia de rede de `pve92` via a própria API Proxmox
(`/nodes/pve/network`); que o erro `ipconfig0`/ARRAY é transitório e não um defeito de
código (reproduzido a falhar, depois a passar, mesmo binário, mesmo comando).

**Not validated**: reachability de facto de um convidado a sério nesta rede (não
necessário — a topologia já responde); o comportamento de um `network`/`namespace`
REJEITADO explicitamente (D8 ainda não o implementa — hoje só `namespace` recusa,
`network` não).

## Limpeza

`p4d5-proxmox-test*` removida (`delonix delete vm`), `p4spike-net` removida
(`delonix network rm`), `pve92` reposta a `Stopped` — nada ficou de pé além do que já
estava neste host antes deste spike. O `eprintln!` de depuração em `post_form`
(`crates/providers/delonix-proxmox/src/lib.rs`) foi revertido no mesmo commit que este
relatório — não faz parte do código final.
