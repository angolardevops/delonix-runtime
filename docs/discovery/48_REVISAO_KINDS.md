# 48 — Revisão dos 18 Kinds: duplicados, fusões e o que expandir

| Campo | Valor |
|---|---|
| Data | 2026-08-10 |
| Linha de base | `0.46.0` + os quatro commits do IaC nativo (`664454b`) |
| Lentes | SRE · Platform Engineering · DevOps · Cloud Architect |
| Âmbito | **Só o modelo declarativo.** Não é auditoria de segurança nem de runtime. |
| Alterações de produção | Nenhuma. Este documento é o único artefacto. |

> Cada afirmação aponta para o ficheiro que a sustenta. Onde a conclusão vem de
> ausência, está dito como foi procurada.

---

## 0. O achado que muda a ordem do trabalho

**18 Kinds. 8 mecanismos.**

O manifesto tem 18 substantivos, mas por baixo há muito menos coisas distintas.
Sete Kinds não têm mecanismo próprio nenhum — compilam para outro Kind ou
escrevem no mesmo store que um irmão:

| Kind | O que É, por baixo | Onde acaba |
|---|---|---|
| `Ingress` | **compila para `HttpRouteSpec`** (`httproute::ingress_to_httproute:452`) | proxy L7 |
| `Egress` | **a MESMA struct** que `FirewallPolicy` (`firewall::FwDocSpec:913`) | firewall nft |
| `FirewallPolicy` | idem, com `direction` explícito | firewall nft |
| `Dependency` | açúcar → regras de ingress por-container (`dependency::apply:72`) | firewall nft |
| `Storage` | um `Volume` com driver de rede + declaração amigável (`storage::build_mount`) | `VolumeStore` |
| `ShareVolume` | `VolumeStore::register_external` + um `JsonStore` próprio | `VolumeStore` |
| `Workload` | **açúcar** → `Container`/`Vm`/`Pod` (`workload::lower_workload:48`) | os outros |

Isto tem uma consequência directa no que foi pedido: **converger os 14
restantes ao mesmo formato seria dar convergência, schema e promessa de
estabilidade a Kinds que não deviam existir separadamente.** Fundir depois
custa uma quebra; fundir antes não custa nada.

E há uma janela que se fecha: a promessa de estabilidade que acabou de entrar
em `cli-stability.md` cobre **só** `Container`/`Pod`/`Volume`/`Network`. Todos
os candidatos a fusão estão **fora** dela. Depois de lhes dar schema tipado e
estabilidade, deixam de poder ser fundidos sem quebra.

---

## 1. Os duplicados, por severidade

### 🔴 D1 — `Egress` e `FirewallPolicy` são literalmente o mesmo objecto

Partilham a struct, o validador, o `apply` e o dataplane. A única diferença é
de onde vem a direcção: `Egress` implica `egress`, `FirewallPolicy` lê
`spec.direction`.

```rust
// firewall.rs:913 — uma struct, dois Kinds
struct FwDocSpec { direction: Option<String>, scope: Option<String>, target: String, ... }
```

Pior: **`Ingress` já foi este Kind** e mudou de significado numa versão anterior
(hoje é L7 estilo k8s). Ou seja, quem leu a documentação da v0.6 e a da v0.8
viu `kind: Ingress` a significar duas coisas diferentes.

Como as quatro lentes vêem isto:
- **SRE**: três nomes para «política de rede» significa três sítios a procurar
  durante um incidente, e o `describe` de um deles não mostra os outros.
- **DevOps**: um code review não consegue dizer se um PR abriu tráfego sem
  saber qual dos três Kinds é autoritativo.
- **Cloud Architect**: nenhum modelo de rede conhecido (k8s NetworkPolicy, AWS
  SG, Azure NSG) separa ingress e egress em *tipos* diferentes — separa-os num
  campo. Estamos a divergir da convenção sem ganho.

**Fusão proposta**: `kind: FirewallPolicy` fica o único; `direction` obrigatório
(`ingress`|`egress`). `kind: Egress` passa a **alias canonicalizado** (o
mecanismo `canonical_kind` já existe e já faz isto para `KnowDepends`) com um
aviso de depreciação. Custo: uma entrada em `canonical_kind` e uma linha no
`apply`.

### 🔴 D2 — `Dependency` é uma terceira forma de escrever a mesma firewall

`Dependency app→db` compila para «no `db`: ingress default-deny + allow do IP do
`app`» — exactamente o que uma `FirewallPolicy` com `direction: ingress`
escreve à mão. E os dois **colidem**: o AGENTS.md já documenta que um `to` que
seja alvo dos dois avisa, porque o `Dependency` é autoritativo e substitui.

Que duas construções da mesma linguagem tenham de avisar uma sobre a outra é o
sintoma; a causa é serem a mesma coisa.

**Mas não é candidato a apagar** — é o único que exprime a intenção
*direccional* de forma legível (`from`/`to`), e é o que um utilizador escreve
naturalmente. O erro é serem irmãos.

**Fusão proposta**: `Dependency` passa a ser **explicitamente açúcar
documentado** que baixa para `FirewallPolicy` no `manifest::load` — o mesmo
tratamento que o `Workload` e o `Stack` já recebem (não sobrevivem ao load).
Ganhos concretos: (1) desaparece a colisão, porque passa a haver um só objecto
no fim; (2) o `plan`/`describe` mostram a regra REAL que vai ser aplicada, em
vez de uma intenção cuja tradução é invisível; (3) converge de graça quando o
`FirewallPolicy` convergir.

### 🟠 D3 — `Ingress` e `HTTPRoute` são o mesmo proxy com duas gramáticas

`Ingress` (forma `networking.k8s.io/v1`) compila para `HttpRouteSpec` e ambos
alimentam o mesmo processo. As limitações são idênticas porque são o mesmo
código (um só certificado, sem SNI; `pathType: Exact` tratado como prefixo).

**Aqui a duplicação defende-se** — e é a diferença importante face a D1/D2. Não
são dois nomes para a mesma gramática: são **duas gramáticas para o mesmo
mecanismo**, e cada uma serve um público real. Quem vem do k8s cola um
`Ingress` que já tem; quem começa aqui escreve o `HTTPRoute`, que é mais curto.

**Proposta**: manter os dois, mas **dizer qual é o canónico** (`HTTPRoute`) e
tratar `Ingress` como *tradutor de esquema estrangeiro* — a mesma categoria
onde o repo já pôs o `docker-compose.yml` e a API Docker, com o mesmo contrato:
o tradutor documenta o que não traduz, e nunca finge. Já é assim no código; o
que falta é dizê-lo na documentação, onde os dois aparecem lado a lado como se
fossem alternativas equivalentes.

### 🟠 D4 — `Volume`, `Storage` e `ShareVolume`: três Kinds, um store

Os três acabam no `VolumeStore`:

* `Volume` — `driver: local|nfs`, com `device`/`mountOptions` crus.
* `Storage` — a MESMA coisa, com uma declaração amigável
  (`type`/`server`/`share`/`username`/`passwordSecret`) traduzida em
  `device`+`options` pelo `build_mount`.
* `ShareVolume` — um subdirectório isolado de um `Storage`, via
  `register_external` + um `JsonStore` só para o registo da partilha.

O problema não é haver três; é que **`Volume` e `Storage` sobrepõem-se
inteiramente**. `kind: Volume` com `driver: nfs` e `device: nas:/export` é o
mesmo objecto que `kind: Storage` com `type: nfs`/`server: nas`/`share:
/export`. Duas maneiras de escrever a mesma montagem, e nada diz qual usar.

- **Platform Engineering**: um catálogo de self-service não consegue expor duas
  primitivas que fazem o mesmo — tem de escolher, e a escolha fica não
  documentada no template.
- **SRE**: `volumes ls` mostra os dois misturados (é o mesmo store), mas
  `storage ls` só mostra uns — a mesma pergunta dá respostas diferentes
  conforme o comando.

**Fusão proposta**: `Storage` passa a **forma de `Volume`** — `spec.nfs`/
`spec.cifs`/`spec.webdav` como blocos nomeados dentro de `kind: Volume`, com o
`driver` derivado do bloco presente (exactamente o padrão que o `Workload` já
usa: o bloco tem o nome do tipo, e o tipo não pode contradizê-lo). `kind:
Storage` fica alias com aviso. **`ShareVolume` mantém-se** — é genuinamente
outra coisa (subdivide um volume existente, com isolamento entre inquilinos), e
o nome diz isso.

### 🟡 D5 — `kind: Container` com `spec.containers[]` é um Pod

O `container::apply` escolhe entre duas formas pela presença de
`spec.containers` — e a forma-Pod aceita **exactamente um** container, senão
manda usar `kind: Pod`.

Ou seja: `kind: Container` com `spec.containers` de um elemento e `kind: Pod`
com um elemento são o mesmo objecto, escrito de duas maneiras, e a fronteira
entre eles é uma contagem.

Isto tem custo medido neste próprio trabalho: o schema gerado teve de oferecer
`anyOf: [ContainerSpec, PodSpec]` para o mesmo `kind`, e o `explain Container`
mostra a união dos dois — a completação do editor fica pior por causa da
ambiguidade.

**Proposta**: **não fundir, deprecar uma direcção.** A forma-Pod dentro de
`kind: Container` existe por compatibilidade com quem cola YAML do k8s; o
caminho certo é esse YAML virar `kind: Pod` no `load` (uma reescrita, como o
`Workload` já faz), deixando `kind: Container` com uma só gramática. Ganho
directo: o schema deixa de ser ambíguo, o `explain` fica útil, e a convergência
do Container deixa de ter dois caminhos de normalização.

### 🟡 D6 — `Workload` é açúcar sobre quatro Kinds, e é o único com futuro

`type: container|vm|pod|microvm` + um bloco com o nome do tipo, que é
**exactamente** a spec do Kind autónomo. Não redefine um campo, logo não pode
divergir. Baixa no `load`.

**Não é um duplicado — é a direcção certa**, e está subaproveitado: é o único
objecto que exprime «computação» sem o utilizador ter de saber se é container
ou VM, que é a promessa do Runtime Abstraction Layer que o AGENTS.md fixa como
norte do produto. O que lhe falta é ser o caminho **recomendado** na
documentação em vez de uma alternativa listada no fim.

---

## 2. Os Kinds a expandir, e as directivas

### `Vm` — o maior défice, e o pedido explícito

Hoje: dois backends (`cloud-hypervisor`, `libvirt`) atrás do trait `VmBackend`,
que já É o padrão de driver plugável. Ver §4 — tem secção própria.

### `Secret` — o único Kind sem round-trip tipado, e é o mais sensível

`manifest::filled_spec` deixa-o deliberadamente no spec cru («não reformatar o
`stringData`»). Consequência: é o único Kind sem `--dry-run` real, sem schema, e
que ficará sem convergência.

**Directiva**: dar-lhe spec tipado com um tipo `SecretValue` que **nunca**
serializa o valor (só `<redigido>`), o que resolve as duas coisas de uma vez —
o dry-run passa a existir e continua a não imprimir segredos. Sem isso, o Kind
mais sensível é o único fora de todas as garantias.

### `Image` — a idempotência é diferente conforme o campo, e ninguém o diz

`spec.pull` é idempotente (`resolve_or_pull`); `spec.build` **reconstrói e
substitui a tag em cada apply** (não há cache de build). Dois comportamentos
opostos no mesmo Kind, decididos por qual campo está preenchido.

**Directiva**: para convergir, `Image` precisa de uma noção de identidade que
não seja a tag — o **digest**. Um `plan` honesto para `spec.pull` compara
digests (a tag pode mover); para `spec.build`, sem cache de build o plano só
pode dizer «vai reconstruir», e deve dizê-lo em vez de mostrar `=`.

### `Cluster` — não é um recurso, é um procedimento

`cluster apply` é bootstrap `kubeadm` sobre SSH, idempotente por `check`/`apply`
passo a passo. Não tem registo, não tem estado, e o «recurso» é um conjunto de
máquinas remotas.

**Directiva**: **não convergir**, e dizê-lo. É a única resposta honesta — o
`plan` não pode prever o que 6 hosts remotos vão dizer sem lhes falar, e falar
com eles deixaria de ser um plano. Fica `NotConverged` permanente, com razão
explícita («procedimento remoto, não recurso local»).

### `Tunnel` — o único Kind cujo estado desejado depende de um terceiro

Tem `JsonStore` próprio, com identidade por PID. A URL pública é atribuída pelo
provider e muda a cada respawn — não é comparável com nada declarado.

**Directiva**: convergível só nos campos declarados (`localPort`, `provider`);
a URL é **status**, não spec. Isso pede a distinção que o modelo ainda não tem —
ver §3.

### `Network` — o único convergente com uma lacuna admitida

`converge` avisa que um peer de overlay retirado do manifesto não é removido
(`add_overlay_peer` não tem inverso). Está honesto, mas é uma promessa por
cumprir.

**Directiva**: `remove_overlay_peer`, e o `peers` passa a convergir nos dois
sentidos.

---

## 3. A lacuna transversal: falta `status`

Nenhum Kind separa **spec** (o desejado) de **status** (o observado). O
`describe` mistura os dois, o `plan` compara tudo o que consegue ler, e campos
que são resultado — o IP de um container, a URL de um túnel, o lease DHCP de uma
VM — não têm onde viver.

As quatro lentes convergem aqui:
- **SRE** quer `status.conditions` para alertar (o `conditions.rs` já computa
  condições; não têm sítio no objecto).
- **Platform** quer `status.phase` para um catálogo mostrar progresso.
- **DevOps** quer `kubectl wait`-equivalente, que é impossível sem status.
- **Cloud Architect**: é a convenção de todo o ecossistema declarativo desde o
  k8s; divergir dela é gratuito.

**Directiva**: antes de converger os 14, decidir se `status` entra. Se entrar, o
`conditions.rs` deixa de ser saída de um comando e passa a ser um campo — e o
`plan` passa a poder dizer «criado, mas não saudável», que hoje é indizível.

---

## 4. `kind: Vm` e os providers pedidos — o que cabe e o que não cabe

O trait `VmBackend` (`delonix-vm/src/lib.rs:438`) já é o ponto de extensão.
Acrescentar um backend é implementá-lo. Mas os seis providers pedidos **não são
a mesma classe de coisa**, e tratá-los como se fossem seria o erro:

| Provider | Cabe? | Porquê |
|---|---|---|
| **Cloud Hypervisor** | ✅ existe | microVM, local, rootless |
| **KVM/libvirt** | ✅ existe | é o backend `libvirt` |
| **VirtualBox** | 🟡 cabe | `VBoxManage` local, sem daemon nosso. Mas **não coexiste com KVM** no mesmo host (ambos querem o `/dev/kvm`), e não é rootless — é um backend de *estação de trabalho*, não de nó |
| **VMware** | 🟡 dois produtos diferentes | Workstation/Fusion local (`vmrun`) cabe pela mesma porta que o VirtualBox; **vSphere/ESXi é REMOTO** (API de datacenter) e é outra categoria — ver abaixo |
| **Proxmox** | 🔴 remoto | API REST de um cluster. O AGENTS.md **já fixou** que inventário/scheduler multi-cluster é do `delonix-paas`. Um `ProxmoxBackend` de **nó único** cabe; o produto Proxmox não |
| **Hyper-V** | ❌ não cabe | É **Windows**. Este motor é Linux/KVM/rootless, com namespaces e cgroups v2 no núcleo. Não é um backend em falta — é outro sistema operativo |

**A fronteira que isto revela**, e que merece um ADR antes de qualquer código: o
`VmBackend` actual assume um hypervisor **local**, invocado por processo filho.
VirtualBox e VMware Workstation respeitam isso. vSphere e Proxmox **não** —
falam HTTP com um datacenter, têm autenticação, e o «host» deixa de ser esta
máquina. Enfiá-los no mesmo trait faria o `delonix vm create` significar duas
coisas diferentes conforme o backend, que é exactamente a confusão que esta
revisão está a tentar remover dos Kinds.

**Proposta de desenho**: dois traços, não um.
- **`VmBackend` (local)** — o que existe. Ganha `VirtualBox` e `VMwareWorkstation`.
- **`VmProvider` (remoto)** — novo, para vSphere/Proxmox: cria numa frota que não
  é esta máquina, e por isso precisa de credenciais, de endpoint e de uma noção
  de «onde». **É aqui que a fronteira com o `delonix-paas` tem de ser decidida**,
  e é decisão de arquitectura, não tarefa.

**O `VMfile` é a peça que unifica** e é onde o pedido tem mais retorno imediato:
hoje constrói um `.qcow2` (`vm build`), e o `HYPERVISOR` já é um campo. O que
falta para «pronto para os principais providers» não é um backend por provider —
é o **artefacto** sair no formato que cada um consome (`.vmdk` para VMware,
`.vdi` para VirtualBox, `.vhdx` para Hyper-V, `.raw` para Proxmox), e o
`qemu-img convert` já faz todos. O `vm convert` existe e hoje só oferece
`qcow2|raw`.

---

## 5. Recomendação de sequência

A ordem pedida (14 Kinds → depois VM) inverte-se em dois pontos, por uma razão
concreta: **fundir depois de estabilizar custa uma quebra.**

1. **Fusões primeiro** (D1, D2, D4, D5) — todas em `manifest::load`/
   `canonical_kind`, o mecanismo que já existe e que o `Stack`/`Workload` já
   usam. Nenhuma toca no dataplane. Depois disto os 18 Kinds passam a ~13, e o
   trabalho de convergência encolhe na mesma proporção.
2. **Decidir o `status`** (§3) — muda a forma de todo o objecto; fazê-lo depois
   de converger 13 Kinds é refazê-los.
3. **Converger o que sobra**, por ordem de valor: `Image` (digest), `Secret`
   (com spec redigida), `Storage`→`Volume` (já fundido no passo 1),
   `HTTPRoute`+`FirewallPolicy` (já unificados), `Vm`. `Cluster` fica
   explicitamente fora, com razão.
4. **`vm convert` para os formatos de cada provider** — retorno imediato, zero
   fronteira nova, usa o `qemu-img` que já está lá.
5. **Backends locais novos** (VirtualBox, VMware Workstation) — cada um com o
   seu spike GO/NO-GO, como o repo exige.
6. **ADR do `VmProvider` remoto** (vSphere/Proxmox) — decidir a fronteira com o
   `delonix-paas` **antes** de escrever código. Hyper-V não entra.

---

## 6. Resumo executivo

| | |
|---|---|
| Kinds hoje | 18 |
| Mecanismos distintos | ~8 |
| A fundir | `Egress`→`FirewallPolicy` · `Dependency`→açúcar de `FirewallPolicy` · `Storage`→forma de `Volume` · forma-Pod de `Container`→`Pod` |
| A manter, com o papel dito | `Ingress` (tradutor de k8s) · `Workload` (o caminho recomendado) · `ShareVolume` |
| A não convergir, com razão | `Cluster` (procedimento remoto, não recurso) |
| Lacuna transversal | não existe `status` — sem ele, «criado mas não saudável» é indizível |
| Providers de VM | 2 cabem já (VBox, VMware Workstation) · 2 precisam de um trait novo e de um ADR (vSphere, Proxmox) · 1 não cabe (Hyper-V, é Windows) |
| Maior retorno imediato | `vm convert` para `.vmdk`/`.vdi`/`.vhdx`/`.raw` — o `qemu-img` já os faz |
