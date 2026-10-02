# ADR-0067: O motor gere pools de armazenamento — LVM-thin, ZFS, btrfs e directório, atrás de uma porta

- **Estado:** Proposto (2026-10-02). Nada implementado.
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

O dono decidiu (D2) que o motor passa a **gerir** pools: criar ou adoptar um pool, alocar volumes
e discos dentro dele, com quota, thin provisioning e snapshots nativos. É uma mudança de
fronteira — o motor passa a escrever em dispositivos de bloco e em metadados do kernel que hoje
nunca toca — e por isso entra por ADR, com a medição abaixo.

### O que este host permite sem root (medido, 2026-10-02)

Spike corrido como `walter` (uid 1000, sem `sudo`, sem pertencer ao grupo `disk`), kernel
7.0.0-34-generic, Ubuntu 24.04, raiz em ext4. Ficheiros só num directório de scratch do worktree
(256 MiB + 128 MiB esparsos), apagados no fim.

| Pergunta | Comando | Resultado |
|---|---|---|
| Ferramentas presentes | `command -v …` | `lvm2` 2.03.16, `thin-provisioning-tools` 0.9.0, `btrfs-progs` 6.6.3, `dmsetup`, `losetup`, `qemu-img`, `qemu-nbd`. **Sem `zfs`/`zpool`** (o `zfsutils-linux` não está instalado) |
| Módulos | `lsmod`, `modinfo zfs` | `btrfs` carregado; `zfs` existe no kernel (2.4.1-1ubuntu5.1) mas **não carregado**; `/dev/zfs` existe com `crw------- root root` |
| LVM como utilizador | `lvs` | `WARNING: Running as a non-root user` e `/run/lock/lvm/P_global:aux: open failed: Permission denied` — **sai 0** com a lista vazia |
| device-mapper | `dmsetup ls` | `/dev/mapper/control: open failed: Permission denied` |
| Loop como utilizador | `losetup --find --show img` | `/dev/loop-control` é `root:disk 0660` → `Permission denied` |
| `mkfs.btrfs` num ficheiro | `mkfs.btrfs -q -L dlxspike img` (256 MiB) | **funciona**, 0,26 s; 4,6 MiB ocupados de 256 MiB aparentes; `btrfs inspect-internal dump-super`, `btrfs filesystem show` e `btrfs check --readonly` lêem a imagem sem a montar |
| Userns + mount ns | `unshare --user --map-root-user --mount` | `tmpfs` e `ramfs` montam; `mount -o loop`, `losetup` e `mount -t btrfs <ficheiro>` falham (o loop é negado mesmo como root do userns); `lvs`, `dmsetup` e `/dev/zfs` dão `Permission denied` |
| Disco de VM sem pool | `qemu-img create -f raw` / `-f qcow2` | funcionam; `qemu-nbd --connect=/dev/nbd0` falha (sem `/dev/nbd0`, e seria root de qualquer forma) |
| Pools do libvirt | `virsh -c qemu:///system pool-capabilities` | `dir`, `fs`, `netfs`, `logical`, `disk`, `iscsi`, `scsi`, `mpath` suportados; **`zfs` e `rbd` não** |

Três armadilhas que a medição mostrou, e que o código tem de respeitar:

1. **`lvs` sem privilégio sai 0 com uma lista vazia.** Um driver que leia o `rc` lê «não há VG»
   num host que tem VGs. É a classe já catalogada «um `read` que falha não é uma resposta vazia»;
   o driver tem de ler o aviso e o erro de lock, ou perguntar o privilégio primeiro.
2. **Sem loop nem device-mapper, uma imagem btrfs/ext4 num ficheiro não serve para nada em
   rootless**: cria-se, mas não se monta — nem dentro de um userns. Um «pool num ficheiro»
   rootless não existe neste kernel.
3. **`/dev/zfs` a `0600` não é a regra do OpenZFS, é a ausência do `zfsutils-linux`**: o pacote
   instala a regra udev que o põe a `0666`, e é isso que torna a delegação `zfs allow` utilizável
   por um não-root. Num host sem o pacote, a sonda diz «ZFS indisponível», nunca «ZFS sem
   delegação».

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
    fn probe(&self, pool: &PoolSpec) -> PoolProbe;       // ferramentas, módulo, privilégio, saúde — sem escrever
    fn required(&self, op: PoolOp) -> Privilege;         // Unprivileged | Delegated(&'static str) | Root
    fn adopt(&self, pool: &PoolSpec) -> Result<PoolState>;
    fn create(&self, pool: &PoolSpec, owner: &Owner) -> Result<PoolState>;
    fn destroy(&self, pool: &PoolState, owner: &Owner) -> Result<()>;
    fn allocate(&self, pool: &PoolState, req: &VolumeRequest, owner: &Owner) -> Result<Allocation>;
    fn resize(&self, a: &Allocation, bytes: u64) -> Result<()>;   // só crescer (como o rootfs da ADR-0058)
    fn snapshot(&self, a: &Allocation, name: &str) -> Result<()>;
    fn rollback(&self, a: &Allocation, name: &str) -> Result<()>;
    fn clone_from(&self, snap: &SnapshotRef, req: &VolumeRequest, owner: &Owner) -> Result<Allocation>;
    fn release(&self, a: &Allocation, owner: &Owner) -> Result<()>;
    fn usage(&self, pool: &PoolState) -> PoolUsage;      // data% e metadata% num thin pool; `unmeasured` com razão
}
```

- `VolumeRequest` tem uma forma: `Filesystem` (um directório montado — volume de container) ou
  `Block` (um caminho de dispositivo ou ficheiro raw — disco de VM). Um driver que não serve uma
  forma recusa-a pelo nome, nunca a aproxima.
- **Os drivers são providers, um crate por backend** (`crates/providers/delonix-provider-btrfs`,
  `-zfs`, `-lvm`), como o libvirt e o Cloud Hypervisor desde a P4b.4: dependem só da fundação e
  do contexto, e a raiz de composição semeia-os com `registration()`. O driver **`dir`** é o
  comportamento de hoje expresso como pool, e vive no `delonix-volume` (adaptador) — mudar o
  sítio onde o directório de hoje vive não é desta decisão.
- **Nenhum `if driver == …` fora do registo.** O CLI, o reconciliador e o compute pedem uma
  `Allocation` pelo nome do pool.
- Registar não faz I/O (ADR-0008); a sonda corre só quando o pool é usado ou listado.

### D2 — `kind: StoragePool`, e como `Volume` e as VMs o referem

```yaml
apiVersion: storage.delonix.io/v1alpha1
kind: StoragePool
metadata: { name: fast }
spec:
  driver: lvm-thin            # dir | btrfs | zfs | lvm-thin
  mode: adopt                 # adopt (omissão) | create
  lvmThin: { vg: vg0, thinPool: dlx }            # create: + devices: [/dev/sdb], size: 500G
  # zfs:   { dataset: tank/dlx }                 # create: + devices / layout
  # btrfs: { path: /srv/dlx }                    # create: + device, mountpoint
  # dir:   { path: /srv/volumes }
  overcommit: { maxRatio: 2.0 }                  # tecto de thin provisioning; acima recusa alocar
  alertPct: 80
```

- **Não tem namespace** (`Namespaced::Never`): um pool é recurso do nó. O motor não conhece
  inquilinos; quem divide um pool por clientes é quem consome o motor, com volumes e quotas.
- `kind: Volume` ganha `spec.pool: <nome>` e `spec.size`. Com `pool` a quota passa a ser a do
  backend (dura onde ele a dá). `pool` é exclusivo com `nfs:`/`cifs:`/`webdav:`/`provision:` —
  recusado em conjunto, nunca um ganha ao outro em silêncio. Sem `pool`, comportamento de hoje.
- `kind: VirtualMachine` ganha `spec.storage.pool` (o disco de raiz) e `extraDisks[].pool`; na CLI,
  `vm create --pool` e `volume create --pool <nome> --size <s>`. **Sem grupo de topo novo**: o
  pool é endereçado pelos verbos genéricos `get|describe|delete storagepools` e por `apply -f`,
  como a restruturação da CLI (B1, B5) já decidiu para os Kinds sem verbo próprio.
- `stack plan` compara o que o pool **é** (lido do backend) e não o que o manifesto diz: um
  `lvextend` feito à mão é deriva. Campos quentes: `overcommit`, `alertPct`, `size` a crescer.
  Frios: `driver`, `mode`, identidade (`vg`/`dataset`/`path`) — recusados sem `--replace`, e um
  `--replace` de um pool com volumes é sempre recusado (D5).

### D3 — Privilégio: o que corre sem root, o que pede delegação, o que pede root

Cada operação declara o privilégio (`required`), e o motor **sonda antes de escrever**. Faltando
o privilégio, a recusa sai antes de qualquer comando com classe **69** (`Unavailable`, uma
capacidade que este host não tem), com um código `DX-` do domínio storage escolhido contra o
dicionário na implementação, e diz as três coisas: o que falta, porquê, e o que o administrador
pode fazer. Não há válvula para «fingir»: uma operação que precisa de root não tem forma degradada.

| driver | adoptar um pool existente | volume `Filesystem` | volume `Block` (disco de VM) | quota dura | snapshot | criar o pool |
|---|---|---|---|---|---|---|
| `dir` | sem root | sem root (directório) | sem root (ficheiro qcow2/raw) | root (loop ext4, como hoje); rootless vigiada | sem root (cópia, como hoje) | sem root (`mkdir`) |
| `btrfs` | sem root, num btrfs **montado pelo administrador** onde o utilizador escreve | sem root (subvolume) | sem root (ficheiro raw com `chattr +C`) | **root** (qgroups); rootless vigiada | sem root (snapshot de um subvolume próprio); apagar pede `user_subvol_rm_allowed` | root (`mkfs.btrfs` + mount) |
| `zfs` | delegado (`zfs allow` + `/dev/zfs` legível) | delegado; montar pede root ou `zoned` (OpenZFS ≥ 2.2) — **a medir** | delegado (`zfs create -s -V`) + acesso ao `/dev/zd*` (regra udev do administrador) | delegado (`refquota`/`volsize`) | delegado | root (`zpool create`) |
| `lvm-thin` | root | root (LV + `mkfs` + mount) | root (o VMM rootless precisa ainda de acesso ao `/dev/dm-*`) | root (tamanho do LV) | root (thin snapshot) | root (`pvcreate`/`vgcreate`/`lvcreate --type thin-pool`) |

- **`mode: create` exige root E uma escolha explícita**: os `devices` são nomeados no manifesto,
  nunca descobertos; um dispositivo com assinatura de sistema de ficheiros (`blkid`) é recusado.
  Sem uma flag `--wipe-devices` no `apply`, o motor nunca apaga uma assinatura. É o único caminho
  que escreve em discos crus, e é o único que o motor nunca faz por omissão.
- **Rootless-first quer dizer `adopt`**: o administrador cria o pool uma vez (como o drop-in de
  delegação de cgroup), e o motor gere volumes dentro dele.
- Um disco de VM `Block` num pool cujo dispositivo o VMM não consegue abrir em leitura-escrita é
  recusado no `create`, com o caminho do dispositivo e o grupo/ACL em falta — antes de arrancar
  um VMM que morreria com `Permission denied`.

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
  do pool quando o disco é de um pool; o snapshot de memória do libvirt (D4 do ADR-0050) fica
  limitado a discos qcow2 e é recusado pelo nome num disco de pool.
- O `rootfs` dos containers (o `upper` do overlay) **não** vai para pools nesta decisão.
- **Proxmox fica como está**: `disk: <storage>:<gib>` nomeia o storage do nó; o pool do Proxmox é
  do Proxmox, e a porta não o administra (ADR-0049 D3).
- **O libvirt não é usado para gerir pools** (`virsh pool-define`): o motor teria uma segunda
  noção de pool que o Cloud Hypervisor não tem, e neste host o libvirt não suporta `zfs`. O
  backend libvirt consome a `Allocation` como qualquer outro.

### D5 — O caminho destrutivo

- **Posse por carimbo, no próprio objecto**: tags de LVM (`delonix.io_owner=<stack>/<nome>`,
  `delonix.io_created=1`), propriedades de utilizador do ZFS (`delonix.io:owner`,
  `delonix.io:created`), e no btrfs/dir um ficheiro `.delonix-pool.json` na raiz do pool e um por
  volume. O carimbo leva só referências, nunca credenciais (ADR-0009).
- **`delete storagepools <nome>` nunca destrói dados por omissão**: desregista. Destruir pede
  `--destroy-data`, e mesmo assim é recusado se (1) o pool foi **adoptado** (`created` ausente),
  (2) há volumes no pool sem o carimbo deste motor, ou (3) há volumes carimbados ainda referidos
  por um container ou VM existente. Os volumes primeiro, o pool depois, o registo em último.
- `stack destroy`/`--prune` de um `StoragePool` desregista; nunca destrói o pool, mesmo criado
  pelo motor — destruir um pool é sempre um comando dado à mão.
- Um `apply` que morre entre «LV criado» e «registo escrito» reencontra o objecto pelo carimbo no
  apply seguinte e adopta-o, em vez de criar um segundo.

### D6 — Thin provisioning: o motor recusa antes de o pool encher

`overcommit.maxRatio` (omissão 1,0 — sem sobre-alocação) limita a soma dos tamanhos alocados
face à capacidade; acima disso `allocate` recusa. Num thin pool, `usage` lê `data_percent` e
`metadata_percent` (`lvs`, `zpool list`, `btrfs filesystem usage`); acima de `alertPct` o motor
avisa e acima de 95 % recusa alocações novas. O autoextend continua do administrador.

### D7 — O que fica fora

- **Ceph/RBD e qualquer armazenamento partilhado entre nós**: `requires-external-component`
  (um cluster Ceph que o motor não traz), como hoje; e a migração a quente continua bloqueada
  pelo ADR-0031.
- Montar o btrfs dentro de um userns: o kernel não o permite.
- RAID/layout de vdevs além do declarado, encriptação nativa (ZFS/LUKS), deduplicação, replicação
  (`zfs send`), resize a encolher.

## Plano por fases

| Fase | O quê | Ficheiros | Prova (bateria / caos) | Células |
|---|---|---|---|---|
| **P0** | Porta, registo, `kind: StoragePool` com driver `dir`, recusa de privilégio, `volume create --pool` | `crates/contexts/delonix-storage/{lib,pool,registry,ownership,error}.rs`; `Cargo.toml`; `scripts/arch_fitness.py` (LAYERS); `crates/adapters/delonix-volume` (driver `dir`); `crates/contexts/delonix-stack/src/kinds.rs`; `bins/delonix-runtime-bin/src/cmd/{storage_pool,volume,schema}.rs`; `data/pt.po`; `docs/schema/v1/delonix.json`; `crates/contexts/delonix-compute/src/capability.rs` | check «storagepool dir: apply, volume com pool, plan sem diferenças, delete desregista sem apagar dados» | nenhuma ainda |
| **P1** | btrfs (adopt num btrfs montado; subvolumes, snapshots; qgroups com root) | `crates/providers/delonix-provider-btrfs` | numa VM do laboratório (D1 do plano) com um disco extra: rootless num btrfs montado pelo root; caos `storagepool_apply_dies_midway` | `storage.zfs-btrfs` → partial |
| **P2** | ZFS (adopt delegado; datasets, zvols, clones) | `crates/providers/delonix-provider-zfs` | VM do laboratório com `zfsutils-linux`; check da delegação `zfs allow` e da recusa sem ela; medir o mount delegado e o `zoned` | `storage.zfs-btrfs` → supported |
| **P3** | LVM-thin (root) | `crates/providers/delonix-provider-lvm` | VM do laboratório; caos `storagepool_thin_full` (encher até ao limiar: alocação recusada antes de 100 %); check «rootless recusa com 69 sem chamar o `lvcreate`» (um `lvcreate` falso no PATH regista se foi chamado) | `storage.lvm-thin` → supported |
| **P4** | Discos de VM em pools (volume-base + clone) para libvirt e Cloud Hypervisor | `crates/contexts/delonix-compute/src/ports.rs` (`LocalDiskImages` aceita um pool); `delonix-vm/src/local_ports.rs`; os dois crates de provider | check por backend: VM criada num pool de cada driver, arranca, snapshot e restore do pool, `vm rm` liberta o clone e deixa o base | `storage.pools` libvirt e CH → supported |
| **P5** | `mode: create` (pool a partir de dispositivos) e `--destroy-data` | os três providers; `cmd/storage_pool.rs` | caos `storagepool_destroy_owned_only`: um pool adoptado nunca é destruído (ler um ficheiro depois), um volume sem carimbo bloqueia o destroy | — |

As provas de P1–P5 precisam de root e de discos: correm **numa VM descartável** do laboratório,
nunca no host. Uma célula só passa a `supported` com a evidência citada no relatório do provider
(ADR-0050).

## Alternativas consideradas

- **Usar os pools do libvirt** (`virsh pool-*`): rejeitado (D4) — só serve um dos dois backends
  locais, e neste host não suporta ZFS (medido).
- **Um crate só para os três drivers**: menos crates, mas um driver ZFS a puxar a compilação do
  LVM num host sem nenhum dos dois, e um provider por porta é a regra que o motor já segue.
- **Só consumir pools feitos à mão (sem `create`)**: é o que o `adopt` dá, e é o caminho rootless;
  o dono pediu gestão, e `create` fica atrás de root e de dispositivos nomeados.
- **Pool rootless num ficheiro** (btrfs/ext4 numa imagem): medido impossível sem loop.

## Consequências

- O motor passa a poder escrever em discos crus — só com root, `mode: create`, dispositivos
  nomeados e sem assinatura. É a fronteira que esta decisão move; precisa de uma passagem
  `delonix-runtime-sec` antes da P5.
- Um nó rootless ganha pools só onde o administrador preparou o terreno (montou o btrfs, delegou
  o ZFS, deu acesso aos zvols). A recusa diz qual destes falta.
- O laboratório de root (D1 do plano) passa a ser pré-requisito das fases P1–P5.

## Perguntas em aberto para o dono

1. **Correr com privilégio**: aceita que `delonix` corra como root para LVM-thin e para criar
   pools (como o `delonix-cri` já corre em root num nó Kubernetes), ou prefere um auxiliar
   privilegiado mínimo (um binário `delonix-storage-helper` com capabilities, chamado por
   socket) — o que seria um processo residente e pede o seu próprio ADR pelo princípio daemonless?
2. A célula `storage.zfs-btrfs` junta dois backends. Partir em `storage.zfs` e `storage.btrfs`
   (catálogo 1.3.0) ou manter, com `supported` só quando os dois tiverem prova?
3. A omissão do `overcommit.maxRatio` (1,0 — sem sobre-alocação) é aceitável, ou o thin
   provisioning deve sobre-alocar por omissão como o Proxmox?
4. Os pools aparecem em `provider ls` sob o provider `linux` (o nó) ou cada driver é um provider
   próprio (`btrfs`, `zfs`, `lvm-thin`) com coluna na matriz?
