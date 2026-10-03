# ADR-0067: O motor gere pools de armazenamento — LVM-thin, ZFS, btrfs e directório, atrás de uma porta

> **Cópia de revisão interna, em português.** O texto canónico é
> `docs/adr/0067-storage-pools.md` (inglês). Os dois têm o mesmo ID, estado, decisões e
> referências; em caso de divergência vale o inglês.

- **Estado:** Proposto (2026-10-02). Nada implementado. As decisões do dono de 2026-10-02 estão
  registadas em D3 e D5; ficam três perguntas em aberto (fim do documento).
- **Data:** 2026-10-02
- **Decisores:** Walter Angolar
- **Relaciona-se com:** decisão D2 do plano de maturidade (`docs/discovery/65_PLANO_MATURIDADE.md`,
  PR #658: «o motor gere pools»), ADR-0009 (provisionar numa NAS: posse por carimbo, remoto
  primeiro e registo em último), ADR-0008/ADR-0044 (registo de backends semeado pela raiz de
  composição), ADR-0050 (catálogo de capacidades e evidência), ADR-0059 D7 (portas por papel num
  contexto próprio), ADR-0031 (o que fica fora: armazenamento partilhado entre nós).

## Contexto

Hoje o motor **consome** armazenamento e não o gere. Um `kind: Volume` é um directório em
`<root>/volumes/<nome>/_data`, ou uma partilha de rede montada; a quota é **dura** só no modelo
root (uma imagem ext4 esparsa montada por loop, `delonix-volume/src/lib.rs`) e **vigiada** em
rootless (uso medido, alerta perto do limite). O disco de uma VM local é sempre um overlay qcow2
sobre a imagem dourada, num ficheiro em `<root>/vms/`. Na matriz de capacidades
(`docs/providers/capability-matrix.md`) isto lê-se assim:

| célula | libvirt | cloud-hypervisor | proxmox | linux |
|---|---|---|---|---|
| `storage.pools` | not-implemented | not-implemented | partial (`disk: <storage>:<gib>`) | unsupported-by-provider |
| `storage.lvm-thin` | — | — | — | not-implemented |
| `storage.zfs-btrfs` | — | — | — | not-implemented |
| `storage.ceph` | — | — | — | requires-external-component |

O dono decidiu (D2) que o motor passa a **gerir** pools: alocar volumes e discos dentro deles, com
quota, thin provisioning e snapshots nativos. É uma mudança de fronteira — o motor passa a escrever
em dispositivos de bloco e em metadados do kernel que hoje nunca toca — e por isso entra por ADR,
com a medição abaixo.

### O que o ambiente testado permite sem root (medido, 2026-10-02)

Spike corrido como `walter` (uid 1000, sem `sudo`, fora do grupo `disk`) num **único host**: kernel
7.0.0-34-generic, Ubuntu 24.04, raiz em ext4. Ficheiros só num directório de scratch do worktree
(256 MiB + 128 MiB esparsos), apagados no fim. **Estes resultados descrevem este ambiente testado,
não todos os hosts Linux**: as permissões dos nós de dispositivo, os grupos do utilizador, as regras
udev, os pacotes instalados e a configuração do kernel mudam-nos. São a razão de o desenho sondar
cada host em vez de assumir.

| Pergunta | Comando | Resultado neste host |
|---|---|---|
| Ferramentas presentes | `command -v …` | `lvm2` 2.03.16, `thin-provisioning-tools` 0.9.0, `btrfs-progs` 6.6.3, `dmsetup`, `losetup`, `qemu-img`, `qemu-nbd`. **Sem `zfs`/`zpool`** (o `zfsutils-linux` não está instalado) |
| Módulos | `lsmod`, `modinfo zfs` | `btrfs` carregado; `zfs` existe no kernel (2.4.1-1ubuntu5.1) mas **não carregado**; `/dev/zfs` existe com `crw------- root root` |
| LVM como utilizador | `lvs` | `WARNING: Running as a non-root user` e `/run/lock/lvm/P_global:aux: open failed: Permission denied` — **sai 0** com a lista vazia |
| device-mapper | `dmsetup ls` | `/dev/mapper/control: open failed: Permission denied` |
| Loop como utilizador | `losetup --find --show img` | `/dev/loop-control` é `root:disk 0660` → `Permission denied` |
| `mkfs.btrfs` num ficheiro | `mkfs.btrfs -q -L dlxspike img` (256 MiB) | **funciona**, 0,26 s; 4,6 MiB ocupados de 256 MiB aparentes; `btrfs inspect-internal dump-super`, `btrfs filesystem show` e `btrfs check --readonly` lêem a imagem sem a montar |
| Userns + mount ns | `unshare --user --map-root-user --mount` | `tmpfs` e `ramfs` montam; `mount -o loop`, `losetup` e `mount -t btrfs <ficheiro>` falham (o loop é negado mesmo como root do userns); `lvs`, `dmsetup` e `/dev/zfs` dão `Permission denied` |
| Disco de VM sem pool | `qemu-img create -f raw` / `-f qcow2` | funcionam; `qemu-nbd --connect=/dev/nbd0` falha (não há `/dev/nbd0` neste host) |
| Pools do libvirt | `virsh -c qemu:///system pool-capabilities` | `dir`, `fs`, `netfs`, `logical`, `disk`, `iscsi`, `scsi`, `mpath` suportados; **`zfs` e `rbd` não** |

Três armadilhas que a medição mostrou, e que o código tem de respeitar:

1. **`lvs` sem privilégio sai 0 com uma lista vazia.** Um driver que leia o `rc` lê «não há VG»
   num host que tem VGs. É a classe já catalogada «um `read` que falha não é uma resposta vazia».
   A regra (decisão do dono, D3.5): sem privilégio suficiente a resposta é **«não foi possível
   determinar»**, nunca «não há pools».
2. **Neste host, sem acesso a loop nem a device-mapper, uma imagem btrfs/ext4 num ficheiro não
   serve para nada em rootless**: cria-se, mas não se monta — nem dentro de um userns. Outro host
   pode diferir (um utilizador no grupo `disk`, outro kernel); a sonda decide por host.
3. **`/dev/zfs` a `0600` aqui é a ausência do `zfsutils-linux`**, não uma regra do OpenZFS: o
   pacote instala a regra udev que o põe a `0666`, e é isso que torna a delegação `zfs allow`
   utilizável por um não-root. Num host sem o pacote, a sonda diz «ZFS indisponível», nunca «ZFS
   sem delegação».

### O que se leu e não se mediu (sem root, sem pool real)

- **btrfs**: `btrfs subvolume create` precisa só de escrita no directório pai; `subvolume delete`
  sem privilégio precisa da opção de montagem `user_subvol_rm_allowed` (ou de o subvolume estar
  vazio, kernel ≥ 4.18); snapshot de um subvolume que o utilizador possui é permitido; **quotas
  (`quota enable`, `qgroup limit`) são ioctls de administração — root**. O btrfs não é montável
  dentro de um userns (não tem `FS_USERNS_MOUNT`), por isso o pool tem de vir montado pelo
  administrador.
- **ZFS**: `zfs allow <user> create,destroy,snapshot,rollback,clone,quota,refquota,volsize,mount`
  delega operações num dataset; no Linux, a permissão `mount` delegada não chega sozinha (o
  `mount(2)` exige `CAP_SYS_ADMIN`), excepto pela delegação a um user namespace do OpenZFS ≥ 2.2
  (`zfs zone` / propriedade `zoned`). Um zvol aparece como `/dev/zd*` (`root:disk 0660`): um VMM
  rootless só o abre com uma regra udev ou ACL dada pelo administrador.
- **LVM-thin**: tudo passa pelo device-mapper — **root, sem delegação possível**. Um thin pool
  cheio põe os LVs em erro de I/O; o `thin_pool_autoextend_threshold` do `lvm.conf` é
  configuração do host, não do motor.

## Decisão

### D1 — Uma porta `StoragePoolDriver` num contexto novo, `delonix-storage`

O contexto **storage** (`storage.delonix.io`, ADR-0040 D2.2) ganha o seu crate,
`crates/contexts/delonix-storage`, com a forma que o `delonix-networking` tem desde o ADR-0059 D7:
a porta, o registo por nome, as marcas de posse e os tipos puros do pedido. Entra na tabela
`LAYERS` do `scripts/arch_fitness.py` como `CONTEXT` no mesmo commit em que o directório nasce.

```rust
pub trait StoragePoolDriver: Send + Sync {
    fn id(&self) -> &'static str;                        // "dir" | "btrfs" | "zfs" | "lvm-thin"
    fn probe(&self, pool: &PoolRef) -> PoolProbe;        // ferramentas, módulo, privilégio, saúde — nunca escreve
    fn required(&self, op: PoolOp) -> Privilege;         // Unprivileged | Delegated(&'static str) | Helper
    fn adopt(&self, pool: &PoolRef) -> Result<PoolState>;
    fn allocate(&self, pool: &PoolState, req: &VolumeRequest, owner: &Owner) -> Result<Allocation>;
    fn resize(&self, a: &Allocation, bytes: u64) -> Result<()>;   // só crescer (como o rootfs da ADR-0058)
    fn snapshot(&self, a: &Allocation, name: &str) -> Result<()>;
    fn rollback(&self, a: &Allocation, name: &str) -> Result<()>;
    fn clone_from(&self, snap: &SnapshotRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation>;
    fn release(&self, a: &Allocation, owner: &Owner) -> Result<()>;
    fn usage(&self, pool: &PoolState) -> PoolUsage;      // data% e metadata% de um thin pool
}
```

- `PoolProbe` tem **três valores**: `Available`, `Unavailable { missing }` e
  `Undetermined { reason, remedy }`. Uma listagem vazia obtida sem privilégio para listar é
  `Undetermined` (D3.5), nunca `Available` com zero pools.
- `VolumeRequest` tem uma forma: `Filesystem` (um directório montado — volume de container) ou
  `Block` (um caminho de dispositivo ou ficheiro raw — disco de VM). Um driver que não serve uma
  forma recusa-a pelo nome, nunca a aproxima.
- **Criar e destruir um pool não estão na porta.** São operações do administrador (D3.3, D5),
  feitas pelo comando administrativo do auxiliar, nunca pelo motor a pedido de um manifesto.
- **Os drivers são providers, um crate por backend** (`crates/providers/delonix-provider-btrfs`,
  `-zfs`, `-lvm`), como o libvirt e o Cloud Hypervisor desde a P4b.4: dependem só da fundação e
  do contexto, e a raiz de composição semeia-os com `registration()`. O driver **`dir`** é o
  comportamento de hoje expresso como pool, e vive no `delonix-volume` (adaptador).
- **Nenhum `if driver == …` fora do registo.** O CLI, o reconciliador e o compute pedem uma
  `Allocation` pelo nome do pool.
- Registar não faz I/O (ADR-0008); a sonda corre só quando o pool é usado ou listado.

### D2 — `kind: StoragePool`, e como `Volume` e as VMs o referem

```yaml
apiVersion: storage.delonix.io/v1alpha1
kind: StoragePool
metadata: { name: fast }        # tem de nomear um pool da lista do administrador (D3.2)
spec:
  overcommit: { maxRatio: 1.0 } # tecto de thin provisioning; acima recusa alocar
  alertPct: 80
```

- **Um manifesto nunca leva um dispositivo, um VG, um dataset ou um caminho de montagem, e nunca
  um comando.** O Kind *usa* um pool que o administrador declarou (D3.2): o nome é procurado na
  lista, e o driver e o objecto vêm de lá. Um nome fora da lista é recusado antes de qualquer
  sonda. Campos como `devices`, `vg`, `dataset`, `path` ou `mode: create` são recusados pelo nome,
  nunca ignorados.
- **Não tem namespace** (`Namespaced::Never`): um pool é recurso do nó. O motor não conhece
  inquilinos; quem divide um pool por clientes é quem consome o motor, com volumes e quotas.
- `kind: Volume` ganha `spec.pool: <nome>` e `spec.size`. Com `pool` a quota passa a ser a do
  backend (dura onde ele a dá). `pool` é exclusivo com `nfs:`/`cifs:`/`webdav:`/`provision:` —
  recusado em conjunto, nunca um ganha ao outro em silêncio. Sem `pool`, comportamento de hoje.
- `kind: VirtualMachine` ganha `spec.storage.pool` (o disco de raiz) e `extraDisks[].pool`; na CLI,
  `vm create --pool` e `volume create --pool <nome> --size <s>`. **Sem grupo de topo novo**: o
  pool é endereçado pelos verbos genéricos `get|describe|delete storagepools` e por `apply -f`,
  como a restruturação da CLI (B1, B5) já decidiu para os Kinds sem verbo próprio.
  `delete storagepools` desregista o uso; nunca destrói o pool (D5).
- `stack plan` compara o que o pool **é** (lido do backend) e não o que o manifesto diz. Campos
  quentes: `overcommit`, `alertPct`, `size` a crescer.

### D3 — Privilégio, o auxiliar e a lista do administrador (decisões do dono, 2026-10-02)

O dono decidiu, a 2026-10-02:

1. **As operações privilegiadas do LVM-thin são aceites, mas o motor no seu todo nunca exige
   root. Rootless continua a ser a omissão.** Toda a operação que pode correr sem privilégio
   (D3.4) corre no processo do motor; só as que precisam de root passam para o auxiliar.
2. **Dispositivos e pools ficam limitados a uma lista do administrador**, e um manifesto nunca pode
   nomear um dispositivo, um caminho ou um comando fora dela.
3. **Os pools só se criam por configuração explícita do administrador.**
4. **A gestão de pools corre por um auxiliar restrito, ou num serviço de nó rootful configurado
   explicitamente.** A escolha e as razões estão abaixo.
5. **Um `lvs` sem privilégio que sai 0 com saída vazia nunca se lê como «não há pools»**; a
   resposta é «não foi possível determinar», com um diagnóstico accionável.

#### A lista

`/etc/delonix/storage-pools.yaml`, do root, modo `0644`, lida pelo motor e pelo auxiliar; **só o
root a escreve**, e essa escrita é a configuração explícita do administrador.

```yaml
pools:
  fast:
    driver: lvm-thin
    vg: vg0
    thinPool: dlx
    create: { devices: [/dev/disk/by-id/nvme-SAMSUNG_X_1234] }   # opcional; só o `delonix-storage-helper create-pool` o lê
    allowUsers: [walter]           # uids/grupos autorizados a alocar pelo auxiliar
    maxVolumeBytes: 500G
  tank:
    driver: zfs
    dataset: tank/dlx
  media:
    driver: btrfs
    path: /srv/dlx                 # montado pelo administrador
```

- O auxiliar relê o ficheiro em cada operação; o motor nunca lhe passa um caminho, um dispositivo
  ou uma opção — só um nome de pool, um nome de volume, um tamanho e uma operação.
- Os dispositivos nomeiam-se por links estáveis `by-id`/`by-path`, canonicalizados e comparados
  depois de resolvidos; um link que resolva para outro dispositivo de bloco que o da criação é
  recusado.

#### O auxiliar: por operação, activado por socket, sem processo residente (escolhido)

Três mecanismos foram avaliados contra o princípio daemonless (AGENTS.md: «o que precisa de
persistir é do systemd — unit, timer, socket activation»):

| Opção | Processo residente | Superfície de entrada | Quem verifica o chamador | Veredicto |
|---|---|---|---|---|
| **A. Auxiliar activado por socket** — `delonix-storage-helper.socket` com `Accept=yes` e um template `delonix-storage-helper@.service` (root, unit endurecida), um processo por ligação, sai depois de um pedido | **Nenhum** (o systemd segura o socket) | Um pedido tipado (JSON) por ligação, validado contra a lista | `SO_PEERCRED` uid/gid contra `allowUsers`; socket a `0660`, grupo `delonix-storage` | **Escolhida** |
| B. Regra `sudoers` para um comando (`walter ALL=(root) NOPASSWD: /usr/libexec/delonix-storage-helper *`) | Nenhum | Um argv, com a semântica de wildcards do sudo | sudo | Rejeitada: wildcards de argv no sudoers são uma superfície de injecção conhecida; a política fica partida entre o sudoers e a lista; prompts/TTY variam por host |
| C. Um daemon root residente | **Sim** | Um servidor de socket de longa vida | ele próprio | Rejeitada pelo princípio daemonless; precisaria do seu próprio ADR com evidência de que A não funciona |

Na opção A o processo do auxiliar vive só durante um pedido. A unit corre com
`ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`,
`CapabilityBoundingSet` limitado a `CAP_SYS_ADMIN CAP_MKNOD CAP_DAC_OVERRIDE CAP_CHOWN CAP_FOWNER`,
`DeviceAllow=` restrito a `/dev/mapper/control`, `/dev/zfs` e aos dispositivos da lista, e
`ReadWritePaths=` limitado aos caminhos dos pools da lista. Não se instala nenhum binário setuid.
O auxiliar:

- aceita só as operações `probe`, `allocate`, `resize`, `snapshot`, `rollback`, `clone`, `release`,
  `usage`, `grant-device` (uma ACL no nó de dispositivo de um volume que ele alocou, para o uid que
  chama — o que um VMM rootless precisa para abrir um zvol ou um thin LV);
- nunca aceita `create-pool` nem `destroy-pool` pelo socket: são o comando administrativo
  `delonix-storage-helper create-pool <nome>` / `destroy-pool <nome>`, corrido pelo root num
  terminal, que lê o bloco `create:` da lista (D3.3, D5);
- regista cada operação no journal com o uid do chamador.

**O serviço de nó rootful configurado explicitamente** é o segundo modo aceite: um motor arrancado
pelo administrador como root (como o `delonix-cri.service` corre num nó Kubernetes) chama o mesmo
código do auxiliar **no próprio processo**, contra a mesma lista — sem socket, mas com a mesma
validação e as mesmas recusas. Escolhe-se pela configuração do administrador
(`/etc/delonix/storage-pools.yaml: mode: in-process`), nunca se infere de `geteuid() == 0`.

Sem nenhum dos dois configurado, toda a operação que precisa do auxiliar é recusada antes de
qualquer comando, com a classe de saída **69** (`Unavailable`) e um código `DX-` do domínio
storage escolhido contra o dicionário na implementação, a nomear o que falta e o remédio («activa
o `delonix-storage-helper.socket`, acrescenta o teu utilizador ao `allowUsers` do pool `fast`»).

#### O que cada driver precisa

| driver | volume `Filesystem` | volume `Block` (disco de VM) | quota dura | snapshot | criar o pool |
|---|---|---|---|---|---|
| `dir` | sem privilégio (directório) | sem privilégio (ficheiro qcow2/raw) | auxiliar (loop ext4); senão vigiada | sem privilégio (cópia) | administrador (`mkdir` + entrada na lista) |
| `btrfs` | sem privilégio (subvolume) num btrfs **montado pelo administrador** onde o utilizador escreve | sem privilégio (ficheiro raw com `chattr +C`) | auxiliar (qgroups); senão vigiada | sem privilégio (snapshot de um subvolume próprio); apagar pede `user_subvol_rm_allowed` ou o auxiliar | administrador (`create-pool`: `mkfs.btrfs` + mount) |
| `zfs` | delegado (`zfs allow` + `/dev/zfs` legível); montar pede o auxiliar ou `zoned` (OpenZFS ≥ 2.2) — **a medir** | delegado (`zfs create -s -V`) + acesso ao dispositivo por `grant-device` | delegado (`refquota`/`volsize`) | delegado | administrador (`create-pool`: `zpool create`) |
| `lvm-thin` | auxiliar (LV + `mkfs` + mount) | auxiliar (thin LV) + `grant-device` | auxiliar (tamanho do LV) | auxiliar (thin snapshot) | administrador (`create-pool`: `pvcreate`/`vgcreate`/`lvcreate --type thin-pool`) |

- O `create-pool` recusa um dispositivo com assinatura de sistema de ficheiros (`blkid`) a não ser
  que o administrador passe `--wipe-devices` no terminal.
- Um disco de VM `Block` cujo dispositivo o VMM não consegue abrir em leitura-escrita é recusado no
  `create`, a nomear o dispositivo e a ACL em falta — antes de arrancar um VMM que morreria com
  `Permission denied`.

#### «Não foi possível determinar» é uma resposta

Toda a listagem que pode ser parcial sem privilégio é classificada, não confiada:

- `lvs`/`vgs` corridos sem privilégio nunca se interpretam. O driver LVM pergunta ao auxiliar; sem
  auxiliar, `probe` devolve `Undetermined { reason: "o estado do LVM pede root (lvs imprimiu:
  P_global:aux: open failed: Permission denied)", remedy: "activa o
  delonix-storage-helper.socket" }`, e `get storagepools` mostra `UNKNOWN` na coluna de estado,
  classe de saída 69 com `--detailed-exitcode`.
- A mesma regra para o ZFS sem `/dev/zfs` legível e para os qgroups do btrfs.
- Um gate (teste unitário sobre saída capturada) falha se o driver LVM alguma vez transformar a
  saída aviso-mais-vazio em `Available`.

### D4 — Como volumes de container e discos de VM se mapeiam em cada backend

| | `Filesystem` (container) | `Block` (VM) | imagem dourada partilhada |
|---|---|---|---|
| `dir` | directório | overlay qcow2 sobre a imagem (hoje) | backing file qcow2 (hoje) |
| `btrfs` | subvolume | ficheiro raw no subvolume da VM, `+C` (sem CoW de dados) | volume-base por imagem, snapshot por VM |
| `zfs` | dataset | zvol esparso (`-s`) | zvol-base por imagem + snapshot + `zfs clone` por VM |
| `lvm-thin` | thin LV + ext4/xfs | thin LV | thin LV-base + thin snapshot por VM (`--setactivationskip n`) |

- A imagem entra no pool **uma vez** (`qemu-img convert` do qcow2 do `VmImageStore` para o
  volume-base, carimbado com o digest), e cada VM é um clone fino dela: o equivalente do backing
  qcow2, nativo do backend. Um volume-base com clones vivos não se apaga (o ZFS recusa sozinho;
  nos outros o motor conta os clones pelo carimbo).
- Um `Block` num pool é **raw**: o qcow2 sobre um zvol ou LV duplicaria o thin provisioning e os
  snapshots. Os dois backends locais aceitam um dispositivo raw (`--disk path=` no Cloud
  Hypervisor, `<disk type='block'>` no libvirt). Os snapshots de VM (`vm snapshot`) passam a ser os
  do pool quando o disco é de um pool; o snapshot de memória do libvirt fica limitado a discos
  qcow2 e é recusado pelo nome num disco de pool.
- O `rootfs` dos containers (o `upper` do overlay) **não** vai para pools nesta decisão.
- **Proxmox fica como está**: `disk: <storage>:<gib>` nomeia o storage do nó; o pool do Proxmox é
  do Proxmox, e a porta não o administra (ADR-0049 D3).
- **O libvirt não é usado para gerir pools** (`virsh pool-define`): o motor teria uma segunda
  noção de pool que o Cloud Hypervisor não tem, e no host testado o libvirt não suporta `zfs`. O
  backend libvirt consome a `Allocation` como qualquer outro.

### D5 — O caminho destrutivo

- **Posse por carimbo, no próprio objecto**: tags de LVM (`delonix.io_owner=<stack>/<nome>`),
  propriedades de utilizador do ZFS (`delonix.io:owner`), e no btrfs/dir um `.delonix-volume.json`
  por volume. O carimbo leva só referências, nunca credenciais (ADR-0009).
- **O motor nunca destrói um pool.** `delete storagepools <nome>` desregista o uso;
  `stack destroy`/`--prune` também. Destruir um pool é `delonix-storage-helper destroy-pool
  <nome>` corrido pelo root num terminal, e é recusado enquanto o pool tiver um volume sem o
  carimbo deste motor, ou um volume carimbado ainda referido por um container ou VM existente; um
  pool que a lista não marca com `create:` (um que já existia antes do motor) nunca é destruído, só
  retirado do ficheiro pelo administrador.
- Volumes: `volume rm` liberta um volume carimbado pelo driver (ou pelo auxiliar); os volumes
  primeiro, o registo em último. Um `apply` que morre entre «LV criado» e «registo escrito»
  reencontra o objecto pelo carimbo no apply seguinte e adopta-o, em vez de criar um segundo.

### D6 — Thin provisioning: o motor recusa antes de o pool encher

`overcommit.maxRatio` (omissão 1,0 — sem sobre-alocação; ver a pergunta em aberto 2) limita a soma
dos tamanhos alocados face à capacidade; acima disso `allocate` recusa. Num thin pool, `usage` lê
`data_percent` e `metadata_percent` (`lvs` pelo auxiliar, `zpool list`, `btrfs filesystem usage`);
acima de `alertPct` o motor avisa e acima de 95 % recusa alocações novas. O autoextend continua do
administrador.

### D7 — O que fica fora

- **Ceph/RBD e qualquer armazenamento partilhado entre nós**: `requires-external-component` (um
  cluster Ceph que o motor não traz), como hoje; a migração a quente continua bloqueada pelo
  ADR-0031.
- Montar o btrfs dentro de um userns: o kernel não o permite.
- Layouts de vdevs além do declarado, encriptação nativa (ZFS/LUKS), deduplicação, replicação
  (`zfs send`), encolher.
- Um daemon de armazenamento residente (opção C acima).

## Plano por fases

| Fase | O quê | Ficheiros | Prova (bateria / caos) | Células |
|---|---|---|---|---|
| **P0** | Porta, registo, leitor da lista, `kind: StoragePool` com o driver `dir`, sonda de três valores, recusa com classe 69, `volume create --pool` | `crates/contexts/delonix-storage/{lib,pool,registry,allowlist,ownership,error}.rs`; `Cargo.toml`; `scripts/arch_fitness.py` (LAYERS); `crates/adapters/delonix-volume` (driver `dir`); `crates/contexts/delonix-stack/src/kinds.rs`; `bins/delonix-runtime-bin/src/cmd/{storage_pool,volume,schema}.rs`; `data/pt.po`; `docs/schema/v1/delonix.json`; `crates/contexts/delonix-compute/src/capability.rs` | check «storagepool dir: apply, volume com pool, plan sem diferenças, delete desregista sem apagar dados»; check «um nome de pool fora da lista é recusado; um manifesto com `devices:` é recusado pelo nome» | nenhuma ainda |
| **P1** | O auxiliar: `bins/delonix-storage-helper` (activado por socket), `dist/delonix-storage-helper.{socket,service}`, comandos administrativos `create-pool`/`destroy-pool`, `install.sh --with-storage-helper` | crate bin novo (camada BIN); `dist/`; `scripts/install.sh` | numa VM do laboratório: o socket recusa um uid fora do `allowUsers` (`SO_PEERCRED`); um pedido a nomear um dispositivo é recusado; nenhum processo do auxiliar fica depois de um pedido; check «sem o auxiliar, o lvm-thin responde UNKNOWN com saída 69, e um `lvcreate` falso no PATH nunca é chamado» | — |
| **P2** | btrfs (subvolumes, snapshots; qgroups pelo auxiliar) | `crates/providers/delonix-provider-btrfs` | numa VM do laboratório com um disco extra: rootless num btrfs montado pelo administrador; caos `storagepool_apply_dies_midway` | `storage.btrfs` → supported (pergunta em aberto 1) |
| **P3** | ZFS (delegado; datasets, zvols, clones) | `crates/providers/delonix-provider-zfs` | VM do laboratório com `zfsutils-linux`; check da delegação `zfs allow` e da recusa sem ela; medir o mount delegado e o `zoned` | `storage.zfs` → supported |
| **P4** | LVM-thin (pelo auxiliar) | `crates/providers/delonix-provider-lvm` | VM do laboratório; caos `storagepool_thin_full` (encher até ao limiar: alocação recusada antes de 100 %); gate sobre a saída aviso-mais-vazio capturada do `lvs` | `storage.lvm-thin` → supported |
| **P5** | Discos de VM em pools (volume-base + clone) para libvirt e Cloud Hypervisor, com `grant-device` para VMMs rootless | `crates/contexts/delonix-compute/src/ports.rs` (`LocalDiskImages` aceita um pool); `delonix-vm/src/local_ports.rs`; os dois crates de provider | check por backend: VM criada num pool de cada driver arranca, snapshot e restore do pool, `vm rm` liberta o clone e deixa o base | `storage.pools` libvirt e CH → supported |
| **P6** | `destroy-pool` e as guardas destrutivas | o auxiliar; os três providers | caos `storagepool_destroy_owned_only`: um pool sem `create:` nunca é destruído (ler um ficheiro depois); um volume sem carimbo bloqueia o destroy | — |

P1–P6 precisam de root e de discos: correm **numa VM descartável** do laboratório (decisão D1 do
plano), nunca num host de produção. Uma célula só passa a `supported` com a evidência citada no
relatório do provider (ADR-0050). É obrigatória uma passagem `delonix-runtime-sec` sobre a
superfície de pedidos do auxiliar antes de a P1 fundir.

## Alternativas consideradas

- **Exigir root ao motor inteiro quando se usam pools**: rejeitado pelo dono (D3.1).
- **`sudoers` ou um daemon residente para a parte privilegiada**: avaliados em D3 e rejeitados.
- **Os pools do libvirt** (`virsh pool-*`): rejeitado (D4) — só servem um dos dois backends locais,
  e no host testado não suportam ZFS.
- **Um crate só para os três drivers**: menos crates, mas um driver ZFS a puxar a compilação do
  LVM num host sem nenhum dos dois, e um provider por porta é a regra que o motor já segue.
- **Criar pools a partir de um manifesto** (`mode: create` com `devices:`): rejeitado pelo dono
  (D3.2, D3.3): um manifesto não é onde se escolhem dispositivos crus.
- **Um pool rootless num ficheiro** (btrfs/ext4 numa imagem): impossível no host testado sem
  acesso a loop; não se oferece.

## Consequências

- O motor nunca escreve num dispositivo cru por palavra de um manifesto: só o `create-pool` do
  administrador, em dispositivos da lista do root, sem assinatura a não ser com `--wipe-devices`.
  É a fronteira que esta decisão move.
- Um nó rootless ganha pools só onde o administrador preparou o terreno (lista, socket do
  auxiliar, um btrfs montado, delegação ZFS). A recusa diz qual destes falta.
- O motor continua a correr sem root; o auxiliar só existe durante um pedido.
- O laboratório de root (D1 do plano) passa a ser pré-requisito das fases P1–P6.

## Perguntas em aberto (com recomendação)

1. **Partir `storage.zfs-btrfs`?** *Recomendação:* partir em `storage.zfs` e `storage.btrfs`
   (catálogo 1.3.0, o nome antigo mantido como alias no relatório). Os dois backends têm modelos de
   privilégio e evidência diferentes; uma célula só ficaria `partial` até o mais lento estar
   provado, a esconder um backend acabado.
2. **Omissão da sobre-alocação?** *Recomendação:* manter `overcommit.maxRatio: 1.0` (sem
   sobre-alocação). Um thin pool LVM cheio põe todos os LVs em erro de I/O; sobre-alocar por
   omissão faria do pior modo de falha a omissão, e quem quiser a sobre-alocação à Proxmox define o
   rácio explicitamente.
3. **Ids de provider?** *Recomendação:* reportar os pools sob o provider de armazenamento `linux`
   do nó, uma linha de capacidade por driver (com a divisão da pergunta 1), em vez de um provider
   por driver. Os drivers partilham o nó, a lista e o auxiliar; ids separados multiplicariam
   colunas da matriz cheias de `unsupported-by-provider` nos outros domínios.
