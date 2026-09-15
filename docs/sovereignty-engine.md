# Soberania — a fatia que é do motor

> Medido: **2026-09-15**. Motor: `delonix` 3.1.0 (build de árvore, commit
> `6a9e1fe5b` — ligeiramente atrás do `HEAD` desta medição; ver nota de método
> no fim). Host: Linux 7.0.0, cgroup v2, backends `cloud-hypervisor` e
> `libvirt` instalados.

Este documento não é "o relatório de soberania da N'GolaCloud" — é a fatia
dele que o `delonix-runtime`, sozinho, num nó, consegue provar. As oito
propriedades testáveis estão definidas em
`ngolacloud-doutrina/references/soberania.md` (workspace da plataforma); quatro
delas são deste motor, quatro são da plataforma por cima dele (residência,
chaves, prova para o regulador, saída sem reféns do serviço gerido). Ler as
duas metades como se fossem uma só é como um relatório de soberania se lê
mais forte do que é.

A plataforma já tem trabalho substancial feito no exercício "WAN desligada" —
cinco passagens, 2026-08-18, à escala do cluster inteiro
(`ngolacloud-doutrina/references/soberania.md`). Este documento não o repete;
mede a mesma propriedade um nível abaixo, no motor.

## Formato de reporte (`ngolacloud-doutrina` §"Ao reportar soberania")

```text
Independência operacional (motor): PROVADA a 2026-09-15 —
    container run/exec/stop de imagem já em cache: zero toque de rede.
    vm create/boot de imagem já em cache (cloud-hypervisor): IP confirmado por
    ARP, zero toque de rede. kubeadm init offline: por provar (razão: exigiria
    SSH para dentro da VM e um 2º/3º nó, num host partilhado — não é gasto
    justificável fora de uma sessão dedicada a isso).
Estrangeiras (motor): arranque 0 (imagem local) · primeiro-uso 1 (ghcr.io,
    golden VM/imagem) · kubeadm 0-1 (condicional a `--offline build`, não
    re-validado nesta passagem) · build-de-imagem 6 (todas build-time)
Chaves / cadeia de fornecimento: fora do âmbito desta passagem
Saída do cliente: fora do âmbito desta passagem
```

## O ensaio: independência operacional, à escala do motor

**Protocolo**: em vez de desligar a rede da máquina (host partilhado, com
containers reais de outras sessões a correr — não é um laboratório, e um
corte de rede a sério afectaria todos), o processo do motor correu inteiro
dentro da SUA PRÓPRIA netns isolada (`unshare --user --net`, sem privilégio),
com **zero interface além do loopback** e DNS a falhar por desenho — o mesmo
efeito de "sem WAN" que um corte a sério teria, sem o custo de o fazer.

```text
$ unshare --user --map-root-user --net -- sh -c 'ip addr show; getent hosts docker.io'
1: lo: <LOOPBACK> mtu 65536 qdisc noop state DOWN ...
(nenhuma outra interface)
getent hosts docker.io → falhou (sem DNS, como esperado)
```

### Container

| Caso | Resultado |
|---|---|
| `container run` de imagem **já em cache**, `--net none` | **rc=0** — `alpine 3.24.1` a responder dentro do container |
| `container run` de imagem **já em cache**, rede SDN por omissão (a que normalmente teria uplink NAT para a internet) | **rc=0** — o container sobe e corre; a ausência de rota de saída não bloqueia o arranque |
| `container run` de imagem **não cacheada** | **rc=1 em ~1s** — falha explícita a nomear o URL (`registry-1.docker.io/v2/...`), nunca um pendura |

### VM (cloud-hypervisor)

```text
$ unshare --user --map-root-user --net -- \
    delonix vm create sov-test --backend cloud-hypervisor \
    --disk delonix-vm-k8s:1.34 --wait --boot-timeout 60
...
info vm 'sov-test' is up — ip 10.200.254.29
```

`is up` aqui é o valor **confirmado por ARP** (`infra::sdn_reachable`), não o
endereço calculado a partir do MAC que o `AGENTS.md` já documenta como a
armadilha a evitar (ver a secção "O `--wait` de uma VM CH esperava por um
número que já sabia") — a diferença entre "o motor disse que está de pé" e
"provei que respondeu". Um qcow2 de 3,5 GiB (679 MiB reais em disco, imagem
`delonix-vm-k8s:1.34`) arrancou, ganhou IP na SDN interna e respondeu, sem
uma única rota de saída disponível no processo que o criou.

**Achado lateral, sem relação com soberania, registado por disciplina de
método**: a primeira tentativa falhou com `Fatal error: ... path must be
shorter than SUN_LEN` — o caminho do `DELONIX_ROOT` de teste (sob um
directório de scratchpad profundo) excedeu o limite do kernel para um socket
Unix do Cloud Hypervisor. Corrigido usando um `DELONIX_ROOT` curto
(`/tmp/sov`); não é um defeito do motor nem da rede.

**Ambiente limpo depois do ensaio**: `delete vm sov-test`, confirmado sem
processo `cloud-hypervisor` residual, `DELONIX_ROOT` de teste removido.

### O que NÃO foi provado nesta passagem, e porquê

`kubeadm init` propriamente dito não correu dentro da VM isolada. Exigiria
SSH para dentro dela (mais um salto de rede dentro do isolamento a
construir) e, mais relevante, um segundo/terceiro nó para um bootstrap de
control-plane real — num host partilhado com carga de outras sessões, esse
gasto de recursos não se justifica fora de uma sessão dedicada ao exercício.

Fica como o próprio `AGENTS.md` já documenta (secção "Pré-semear as imagens
do `kubeadm`"): um `image vm build --offline` fetcha as 7 imagens core do
Kubernetes (apiserver, controller-manager, scheduler, etcd, coredns, pause)
no HOST de build e injecta-as no `ImageStore` embutido no qcow2 — **se** a
imagem golden publicada foi construída por esse caminho. A imagem usada neste
ensaio (`delonix-vm-k8s:1.34`) foi obtida por `vm pull` de
`ghcr.io/angolardevops/delonix-vm-k8s:1.34`, e o `pull` **não recupera
metadados** que digam se a build de origem foi `--offline` (gap já
documentado no `AGENTS.md`) — por isso esta linha fica **condicional**, não
confirmada.

## A contagem de dependências estrangeiras (código, não estimativa)

Medido por leitura directa do código-fonte (`grep` de domínios/registos
hardcoded em `crates/`), classificado pela mesma disciplina do
`ngolacloud-doutrina` (build-time vs run-time; bloqueante vs degradável vs
conveniência):

| Caminho | Domínio(s) | Classe | Bloqueante? |
|---|---|---|---|
| **Arranque** (`container run`/`exec`/`stop` de imagem já local) | nenhum | — | **Não** — confirmado acima |
| **Arranque** (`container run` sem a imagem em cache) | `registry-1.docker.io` (default quando o cliente não qualifica o registo) | run-time, **acção explícita do cliente** | Só se o cliente pedir uma imagem nova — não é um "chamar casa" automático do motor |
| **Primeiro `vm pull`/`cluster kubeadm --vm-image` sem cache** | `ghcr.io/angolardevops/delonix-vm-*` | run-time, primeiro uso | **Sim**, para um nó a criar o PRIMEIRO cluster/VM; local depois disso |
| **`kubeadm init`** | `registry.k8s.io` | run-time — condicional a `--offline build` da golden | Condicional ao modo de build; não garantido por omissão (ver secção acima) |
| **Build de imagem golden** (`image vm build`) | `cloud-images.ubuntu.com`, `cloud.debian.org`, `dl.rockylinux.org`, `download.fedoraproject.org`, `pkgs.k8s.io`, `archive.ubuntu.com` | **build-time**, não run-time | Não — a doutrina já classifica build-time como fora da conta crítica |
| **Firmware Cloud Hypervisor** (`install.sh`) | `github.com/cloud-hypervisor/edk2` (releases) | instalação do host, uma vez | Não — por-host, não por-VM |

**O que isto não cobre**: dependências de build do próprio Rust (`crates.io`
no `Cargo.lock`) — a doutrina já as classifica como build-time e fora da
conta crítica (confirmado na varredura de 2026-08-18 da plataforma: 68
ocorrências de `crates.io`, todas build). Não recontado aqui por já estar
resolvido noutro sítio.

## Nota de método

O binário usado (`target/release/delonix`, commit `6a9e1fe5b`) está
ligeiramente atrás do `HEAD` usado para desenhar este ensaio (`e66ae8da`
local / `adae8343` em `origin/main` no momento de escrever este documento) —
ambos são commits de documentação (`AGENTS.md`), sem alteração de código de
motor entre eles. Não foi feita uma reconstrução completa por ser um ensaio
de propriedade estrutural (a rede é ou não é tocada), não uma validação de
uma funcionalidade específica dessa janela de commits.

## Ligações

- As oito propriedades e os dois exercícios completos:
  `ngolacloud-doutrina/references/soberania.md` (workspace da plataforma).
- O que falta para "arranque" fechar 100%: exercitar `kubeadm init` dentro de
  uma VM isolada, e confirmar (ou negar) que uma golden `--offline` chega a
  produzir um control-plane sem tocar `registry.k8s.io`.
- Sovereignty como eixo de posicionamento contra AWS/GCP:
  `delonix-engine/SKILL.md` §2.
