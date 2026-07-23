#!/usr/bin/env python3
"""Gerador do site de documentação (GitHub Pages, pasta `docs/`).

Filosofia: as páginas de referência embebem o `--help` REAL do binário
`delonix` (corrido no momento da geração) — a documentação nunca fica a
descrever flags que não existem. Regenerar depois de mexer na CLI:

    cargo build --release -p delonix-runtime-bin
    python3 docs/gen.py            # usa ./target/release/delonix

O conteúdo editorial (introduções, exemplos, notas) vive nos dicts abaixo.
"""

import html
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "..", "target", "release", "delonix")


def help_of(*args):
    out = subprocess.run([BIN, *args, "--help"], capture_output=True, text=True)
    return (out.stdout or out.stderr).strip()


# ---------------------------------------------------------------- conteúdo

GROUPS = {
    "container": {
        "title": "delonix container",
        "tagline": "Ciclo de vida de containers: run, ps, start, stop, rm, exec, logs, inspect, stats, apply.",
        "intro": """O grupo <code>container</code> é o dia a dia do runtime — o homólogo do
<code>docker container</code>. Cada invocação é um processo efémero (sem daemon): o
<code>run</code> faz <code>clone()</code> directo com os namespaces pedidos e o estado fica em
JSON no <code>$DELONIX_ROOT</code>. Em rootless, o rootfs do container é uma cópia flat
<em>persistente</em> — as escritas sobrevivem a <code>stop</code>/<code>start</code>, como no Docker.""",
        "subs": {
            "run": {"examples": [
                ("Servir nginx na porta 8080 do host (NAT userspace, sem root)",
                 "delonix container run -d --name web -p 8080:80 nginx"),
                ("Correr numa rede criada pelo utilizador, publicando pelo ingress",
                 "delonix network create minha-rede\ndelonix container run -d --net minha-rede -p 8443:443 caddy"),
                ("Shell descartável (remove-se sozinho à saída)",
                 "delonix container run --rm -e TERM=xterm alpine sh -c 'echo olá'"),
                ("Sobrepor o ENTRYPOINT para depurar uma imagem",
                 "delonix container run --rm --entrypoint /bin/sh nginx -c 'nginx -t'"),
            ], "notes": """<p><strong><code>-p</code> e a rede:</strong> com <code>--net host</code> (o default) o
container muda para um netns próprio com NAT em userspace (slirp4netns — o modelo do podman
rootless); com <code>--net &lt;rede&gt;</code> a porta é publicada pelo <em>ingress</em> (hostfwd no
slirp único + DNAT nft), o caminho que permite trocar portas a quente sem parar o container.
<code>--net none</code> recusa <code>-p</code>.</p>"""},
            "ps": {"examples": [
                ("Listar (alias `ls` também funciona)", "delonix container ls -a"),
                ("Compor com stop/rm", "delonix container rm -f $(delonix container ps -aq)"),
            ]},
            "start": {"examples": [
                ("Rearrancar um container parado, preservando o que foi escrito lá dentro",
                 "delonix container start web"),
            ], "notes": """<p>Reusa a spec guardada (comando, env, volumes, rede, portas) e o rootfs
persistente — ao contrário de <code>rm</code>+<code>run</code>, nada do que o container escreveu se perde.</p>"""},
            "stop": {"examples": [("SIGTERM, e SIGKILL ao fim de 5s", "delonix container stop -t 5 web db")]},
            "rm": {"examples": [("Forçar remoção de vários", "delonix container rm -f web db cache")]},
            "exec": {"examples": [("Shell interactiva", "delonix container exec -it web sh")]},
            "logs": {"examples": [("Seguir em contínuo (sai quando o container parar)", "delonix container logs -f web")]},
            "inspect": {"examples": [("Spec completa em JSON", "delonix container inspect web | jq .[0].ports")]},
            "stats": {"examples": [("Uma amostra de todos os que correm", "delonix container stats")],
                      "notes": """<p>CPU%/memória/PIDs lidos do cgroup v2 do próprio container (resolvido por
<code>/proc/&lt;pid&gt;/cgroup</code>, qualquer que seja a base delegada). Sem delegação de cgroup
(rootless sem <code>Delegate=yes</code>), a memória cai para o VmRSS do init do container,
marcada com <code>~</code>.</p>"""},
            "apply": {"examples": [("Aplicar só os `kind: Container` de um manifesto", "delonix container apply -f delonix-manifest.yaml")]},
        },
    },
    "pod": {
        "title": "delonix pod",
        "tagline": "Pods reais multi-container (create, ls, describe, rm, logs) — N containers como uma unidade.",
        "intro": """Pods de verdade, ao estilo Kubernetes: N containers que <strong>partilham as
namespaces do pod</strong> e se gerem como uma só unidade. Hoje partilham <strong>netns</strong>
(o mesmo IP, alcançam-se por <code>localhost</code>), <strong>IPC</strong> (System V/POSIX) e
<strong>UTS</strong> (o hostname). Tudo <em>rootless e daemonless</em>: o pod é uma netns SDN
nomeada no holder (<code>pod-&lt;nome&gt;</code>, com IP na <code>delonix0</code>), e cada container
junta-se a ela pelo re-exec <code>nsenter … ip netns exec</code> (a flag interna <code>--pod</code>);
o 1.º container segura o IPC/UTS e os restantes fazem <code>setns</code> de
<code>/proc/&lt;pid&gt;/ns/{ipc,uts}</code> — possível sem privilégio porque o re-exec já os põe no
userns do holder. A <em>membership</em> não tem store novo: deriva do label
<code>delonix.io/pod=&lt;nome&gt;</code> (como <code>cluster</code>/<code>stack</code>). Cria-se de um
manifesto <code>kind: Pod</code> (o mesmo schema <code>spec.containers[]</code> do
<code>kind: Container</code>, mas com N containers permitidos). <strong>Limitação conhecida:</strong>
a namespace de <strong>PID</strong> (<code>shareProcessNamespace</code>, já no schema) ainda NÃO é
partilhada — cada container mantém a sua própria árvore de processos; é a fatia seguinte.""",
        "subs": {
            "create": {"examples": [
                ("Criar um pod (web + sidecar que fala por localhost) de um manifesto",
                 "delonix pod create -f examples/pod-multi.yaml"),
            ], "notes": """<p>Idempotente (<em>garante-presente</em>): se o pod já tem containers, não
faz nada. Também se pode aplicar pelo <code>delonix stack apply</code> (grupo <code>pods:</code> no
<code>kind: Stack</code>) e pré-visualizar com <code>--dry-run</code>. Se a criação de um membro
falha, o pod é desfeito por inteiro (sem meio-pod).</p>"""},
            "ls": {"examples": [("Listar os pods (POD, CONTAINERS n/N, IP, STATUS)", "delonix pod ls")]},
            "describe": {"examples": [("Detalhe estilo kubectl: containers + IP e netns partilhados", "delonix pod describe web-app")]},
            "rm": {"examples": [
                ("Remover o pod: pára/remove TODOS os containers + a netns partilhada", "delonix pod rm web-app"),
                ("Forçar (mata os que estão a correr)", "delonix pod rm -f web-app"),
            ]},
            "logs": {"examples": [
                ("Logs do 1.º container do pod", "delonix pod logs web-app"),
                ("Logs de um container específico (nome curto dentro do pod)", "delonix pod logs web-app --container sidecar -f"),
            ]},
        },
    },
    "image": {
        "title": "delonix image",
        "tagline": "Imagens OCI: pull, ls, rm, export — e, com --vm, as imagens VM douradas (build/push).",
        "intro": """Gestão de imagens de container (registos OCI: Docker Hub, ghcr.io, …) com
verificação de digest no pull. Com <code>--vm</code>, o MESMO grupo opera sobre as
<strong>imagens VM douradas</strong> (um <code>.qcow2</code> + metadados por imagem): Ubuntu cloud
image + kubeadm/kubelet/kubectl + <code>delonix-cri</code> — a base do <code>delonix cluster</code>.""",
        "subs": {
            "pull": {"examples": [
                ("Referência com tag e digest (formato combinado suportado)",
                 "delonix image pull kindest/node:v1.34.0@sha256:7416a6…"),
            ]},
            "ls": {"examples": [("", "delonix image ls")]},
            "rm": {"examples": [("", "delonix image rm alpine:3.19")]},
            "export": {"examples": [
                ("Bundle OCI runtime para correr com runc/crun",
                 "delonix image export alpine:3.19 /tmp/bundle && sudo runc run -b /tmp/bundle teste"),
            ]},
            "push": {"examples": [
                ("Publicar a imagem VM dourada como artefacto OCI (padrão ORAS)",
                 "delonix image --vm push k8s-golden ghcr.io/angolardevops/delonix-vm-k8s:1.34"),
            ]},
            "build": {"examples": [
                ("Construir a imagem VM dourada (descarrega Ubuntu, valida SHA256SUMS, virt-customize)",
                 "delonix image --vm build --name k8s-golden --k8s-version 1.34"),
            ]},
            "apply": {"examples": [("", "delonix image apply -f delonix-manifest.yaml")]},
        },
    },
    "build": {
        "title": "delonix build",
        "tagline": "Constrói uma imagem a partir de um Dockerfile ou Delonixfile.",
        "intro": """Build sem daemon nem BuildKit: sobe um container de trabalho por estágio, corre
cada <code>RUN</code> por <code>exec</code>, aplica <code>COPY</code> no rootfs (confinado ao
contexto — path traversal é rejeitado) e empacota o resultado. Sem <code>-f</code>, procura
primeiro um <code>Delonixfile</code> no contexto e só depois um <code>Dockerfile</code> — a
gramática é a mesma, com extensões (<code>SCAN</code>, <code>CPUS</code>, <code>MEMORY</code>,
<code>SECURITY</code>, <code>HEALTHCHECK</code>). <strong>Multi-stage suportado</strong>
(<code>FROM ... AS &lt;nome&gt;</code> + <code>COPY --from=&lt;estágio&gt;</code>); limitação
conhecida: em modo root (overlay), o estágio final ainda tem de ser uma imagem real, não outro
estágio (sem lineage OCI para um estágio clonado) — sem restrição em rootless.
<code>ARG</code>/<code>--build-arg</code> e <code>USER</code>/<code>ENTRYPOINT</code> já
sobrevivem ao build (incluindo em rootless). <strong>Cache de camadas por instrução</strong>
(rootless — um <code>RUN</code>/<code>COPY</code> repetido não re-executa; <code>--no-cache</code>
para saltar; modo root continua sem cache). Sem BuildKit real (sem
<code>RUN --mount=secret</code>, sem <code>--platform</code>).""",
        "subs": {},
        "examples": [
            ("Build com tag", "delonix build -t minha-app:1.0 ."),
            ("Delonixfile explícito", "delonix build -t api:dev -f Delonixfile ./servico"),
        ],
    },
    "vm": {
        "title": "delonix vm",
        "tagline": "microVMs declarativas: create, ls, status, stop, rm, apply.",
        "intro": """MicroVMs geridas pelo trait <code>VmBackend</code> — Cloud Hypervisor ou libvirt.
O <code>create</code> é idempotente (cria ou auto-recupera) e suporta cloud-init por instância:
<code>--hostname</code>, <code>--ssh-key</code> e <code>--user-data</code> geram um ISO NoCloud
automaticamente. É a camada que o <code>delonix cluster kubeadm</code> usa para provisionar nós.""",
        "subs": {
            "create": {"examples": [
                ("VM a partir da imagem dourada, com chave SSH",
                 "delonix vm create --name node1 --image k8s-golden --cpus 2 --memory 4096 --ssh-key @~/.ssh/id_ed25519.pub"),
            ]},
            "ls": {"examples": [("", "delonix vm ls")]},
            "status": {"examples": [("Reconcilia liveness/IP com o backend", "delonix vm status node1")]},
            "stop": {"examples": [("", "delonix vm stop node1")]},
            "rm": {"examples": [("", "delonix vm rm node1")]},
            "apply": {"examples": [("", "delonix vm apply -f delonix-manifest.yaml")]},
        },
    },
    "volumes": {
        "title": "delonix volumes",
        "tagline": "Volumes nomeados e bind mounts: create, ls, inspect, rm, apply.",
        "intro": """Wrapper fino sobre o <code>VolumeStore</code>. No <code>container run</code>,
<code>-v nome:/destino[:ro]</code> resolve para um volume nomeado (criado on-demand) e
<code>-v /host:/destino[:ro]</code> para um bind mount — a distinção é automática.""",
        "subs": {
            "create": {"examples": [("Com quota e driver nfs disponíveis", "delonix volumes create dados --quota 10G")]},
            "ls": {"examples": [("", "delonix volumes ls")]},
            "inspect": {"examples": [("", "delonix volumes inspect dados")]},
            "rm": {"examples": [("", "delonix volumes rm dados")]},
            "apply": {"examples": [("", "delonix volumes apply -f delonix-manifest.yaml")]},
        },
    },
    "network": {
        "title": "delonix network",
        "tagline": "Redes de utilizador: create, ls, inspect, rm, apply.",
        "intro": """Para o driver <code>bridge</code> (o único a que containers se atacham hoje), o
<code>create</code> orquestra o registo declarativo (<code>NetworkStore</code>) e o plano físico
rootless (bridge dentro do netns do holder). Drivers <code>macvlan</code>/<code>ipvlan</code>/
<code>overlay</code> (VXLAN cifrado com WireGuard entre nós) ficam registados no store; o attach
de containers a esses drivers é trabalho futuro.""",
        "subs": {
            "create": {"examples": [("Rede bridge para um grupo de serviços", "delonix network create backend")]},
            "ls": {"examples": [("", "delonix network ls")]},
            "inspect": {"examples": [("", "delonix network inspect backend")]},
            "rm": {"examples": [("", "delonix network rm backend")]},
            "apply": {"examples": [("", "delonix network apply -f delonix-manifest.yaml")]},
        },
    },
    "stack": {
        "title": "delonix stack",
        "tagline": "Aplica um manifesto inteiro (delonix-manifest.yaml) — todos os Kinds, por ordem.",
        "intro": """O equivalente declarativo do compose, ao estilo Kubernetes: um YAML multi-documento
(<code>apiVersion: delonix.io/v1</code>) com 5 Kinds — <code>Network</code>, <code>Volume</code>,
<code>Image</code>, <code>Vm</code>, <code>Container</code> — aplicados por essa ordem de dependência.
Semântica <em>garante-presente</em> (idempotente por nome), não um reconciliador: sem diffing,
rollout nem rollback — fail-fast, o que já foi aplicado fica.""",
        "subs": {
            "init": {"examples": [
                ("Projecto COMPLETO de uma stack (FastAPI): código + Delonixfile + manifesto + testes",
                 "delonix stack init myapi --template python"),
                ("Ver os templates disponíveis", "delonix stack init --template list"),
            ], "notes": """<p><code>--template &lt;nome&gt;</code> gera um projecto real e funcional de uma
linguagem/framework, com boas práticas (multi-stage não-root, healthcheck, testes, dotfiles) e já
delonix-native (Delonixfile + manifesto). Sem <code>--template</code>, o <code>init</code> gera o
scaffold genérico. Os tokens <code>__NAME__</code>/<code>__MODULE__</code> são substituídos pelo nome
do projecto.</p>"""},
            "apply": {"examples": [
                ("Aplicar o manifesto por omissão (./delonix-manifest.yaml)", "delonix stack apply"),
                ("Manifesto explícito", "delonix stack apply -f infra/stack.yaml"),
            ]},
        },
        "extra": """<h3>Exemplo de manifesto</h3>
<pre><code>apiVersion: delonix.io/v1
kind: Network
metadata: { name: backend }
---
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: dados }
---
apiVersion: delonix.io/v1
kind: Container
metadata: { name: db }
spec:
  image: postgres:16-alpine
  network: backend
  volumes: [ "dados:/var/lib/postgresql/data" ]
  ports: [ "5432:5432" ]
  env: [ "POSTGRES_PASSWORD=segredo" ]
</code></pre>""",
    },
    "cluster": {
        "title": "delonix cluster",
        "tagline": "Kubernetes de ponta a ponta: bootstrap kubeadm idempotente sobre SSH, ou provisionamento completo de VMs.",
        "intro": """Dois caminhos para um cluster real (não emulado):
<code>cluster apply</code> faz bootstrap <code>kubeadm</code> em hosts já vivos e alcançáveis por SSH —
idempotente <em>sem ficheiro de estado</em> (cada passo tem um <code>check</code> e um <code>apply</code>;
nunca dessincroniza de um .tfstate porque não há nenhum). <code>cluster kubeadm</code> vai mais longe:
provisiona as VMs a partir da imagem VM dourada, espera pelo SSH e corre o MESMO bootstrap — um
comando, do zero a um cluster com o <code>delonix-cri</code> como runtime (sem containerd).""",
        "subs": {
            "apply": {"examples": [
                ("Bootstrap num manifesto `kind: Cluster`", "delonix cluster apply -f cloud.yaml"),
            ], "notes": """<p>Todas as entradas do manifesto que chegam a comandos remotos
(<code>controlPlaneEndpoint</code>, subnets, versão) passam por validação estrita antes de qualquer
interpolação — a injecção de comandos via manifesto foi um dos CRÍTICOS encontrados e fechados na
auditoria ofensiva do projecto, com testes a replicar o exploit.</p>"""},
            "kubeadm": {"examples": [
                ("Do zero: 1 control-plane + 2 workers", "delonix cluster kubeadm --name lab --control-plane 1 --workers 2"),
            ], "notes": """<p>Limitação conhecida: por agora só 1 control-plane (HA exige um endpoint
estável — LB/VIP — que este comando ainda não provisiona; <code>cluster apply</code> já suporta HA
com um <code>controlPlaneEndpoint</code> externo).</p>"""},
        },
    },
    "secret": {
        "title": "delonix secret",
        "tagline": "Cofre de segredos cifrado em repouso — o produtor do `run --secret`.",
        "intro": """Um cofre local (<code>SecretStore</code>) cifrado com XChaCha20-Poly1305. Os valores
NUNCA são impressos por omissão (redigidos; <code>--reveal</code> é opt-in). É a fonte dos
<code>container run --secret</code>/<code>--secret-files</code> e do <code>--password-secret</code> do
<code>storage</code> — o segredo entra uma vez, nunca fica no histórico do shell nem no manifesto.""",
        "subs": {
            "create": {"examples": [("Criar um segredo (valor via stdin, não no argv)", "printf 's3nha' | delonix secret create db-pass")]},
            "ls": {"examples": [("Listar (valores redigidos)", "delonix secret ls")]},
            "inspect": {"examples": [("Revelar explicitamente", "delonix secret inspect db-pass --reveal")]},
            "rotate-key": {"examples": [("Rodar a chave-mestra (re-cifra tudo)", "delonix secret rotate-key")]},
        },
    },
    "storage": {
        "title": "delonix storage",
        "tagline": "Volumes de REDE (NFS/CIFS/WebDAV) montáveis, estilo PersistentVolume do k8s.",
        "intro": """Monta pastas de um NAS (TrueNAS/Synology/Samba/Nextcloud) como volumes nomeados.
Por baixo é um volume do <code>delonix-volume</code> com driver de rede — <code>mount -t nfs|cifs|davfs</code>.
A password vem do cofre (<code>--password-secret</code>), nunca do argv. Ligado ao <code>stack apply</code>
(ordem Network→Volume→<strong>Storage</strong>→Image→Vm→Container). Montar precisa de CAP_SYS_ADMIN.""",
        "subs": {
            "create": {"examples": [
                ("NFS de um TrueNAS", "delonix storage create media --type nfs --server 10.0.0.5 --share /mnt/pool/media"),
                ("SMB/CIFS com password do cofre", "delonix storage create docs --type cifs --server nas --share docs --username user --password-secret nas-pass"),
            ]},
            "ls": {"examples": [("", "delonix storage ls")]},
            "rm": {"examples": [("Desmonta; os dados ficam no NAS", "delonix storage rm media")]},
        },
    },
    "sharevolume": {
        "title": "delonix sharevolume",
        "tagline": "Uma fatia ISOLADA e com QUOTA própria de um `Storage` — vários container/vm/pod partilham um NAS.",
        "intro": """Resolve um problema concreto de multi-tenant: várias cargas a partilhar UM export
NFS/CIFS/WebDAV, cada uma com o SEU ponto de montagem isolado e a SUA quota, sem se verem. Por baixo
não há mecanismo de montagem novo nenhum: cada <code>ShareVolume</code> é um SUBDIRECTÓRIO real da
árvore já montada pelo <code>kind: Storage</code> pai (<code>&lt;storage&gt;/_data/shares/&lt;nome&gt;</code>),
registado como o seu próprio volume — a isolação é confinamento de caminho puro e o consumo usa o
<code>-v &lt;nome&gt;:/destino</code> de sempre, sem código novo nenhum do lado do container/vm/pod. A
quota é SOFT (uso medido + alerta) — o caminho HARD (imagem ext4 loopback) precisa de armazenamento de
bloco local e não compõe com um subdirectório de um mount de rede.""",
        "subs": {
            "apply": {"examples": [
                ("Duas fatias isoladas do mesmo NAS, cada uma com a sua quota",
                 "delonix sharevolume apply -f sharevolume.yaml",
                 "sharevolume/tenant-a: ready (nas-shared -> /var/lib/delonix/volumes/nas-shared/_data/shares/tenant-a)\n"
                 "sharevolume/tenant-b: ready (nas-shared -> /var/lib/delonix/volumes/nas-shared/_data/shares/tenant-b)"),
            ]},
            "ls": {"examples": [
                ("Listar (quota + uso real medido)", "delonix sharevolume ls",
                 "NAME       STORAGE      QUOTA     USED   ALERT   MOUNTPOINT\n"
                 "tenant-b   nas-shared   2.0 MiB   0 B    -       .../shares/tenant-b\n"
                 "tenant-a   nas-shared   1.0 MiB   1.0 MiB   OVER    .../shares/tenant-a"),
            ]},
            "describe": {"examples": [
                ("Detalhe de uma fatia (aponta o comando -v para a consumir)",
                 "delonix sharevolume describe tenant-a",
                 "Name:           tenant-a\nStorage:        nas-shared\nMountpoint:     .../shares/tenant-a\n"
                 "Used:           1.0 MiB\nQuota:          1.0 MiB\nAlert:          OVER QUOTA\n"
                 "Consume with:   -v tenant-a:/path/in/container"),
            ]},
            "rm": {"examples": [
                ("Remove o registo; os DADOS ficam (a não ser que peças --purge-data)",
                 "delonix sharevolume rm tenant-a"),
            ]},
        },
    },
    "ingress": {
        "title": "delonix ingress",
        "tagline": "Firewall de ENTRADA (regras L4 + publishes DNAT) de um container na SDN.",
        "intro": """Metade da superfície unificada de firewall (a outra é <code>egress</code>). Edita a
única fonte de verdade — o <code>ContainerFw</code> por container, aplicado como regras nft na chain de
ingress. <code>ingress</code> governa a ENTRADA: regras allow/deny por <code>[proto/]porta</code> e CIDR,
a política por omissão, e os <em>publishes</em> DNAT. Só actua em containers numa rede custom (têm IP na
<code>delonix0</code>); <code>--net host</code> é recusado.""",
        "subs": {
            "allow": {"examples": [("Deixar entrar Postgres só da própria SDN", "delonix ingress allow db tcp/5432 --from 10.219.0.0/16")]},
            "deny": {"examples": [("Bloquear uma porta específica", "delonix ingress deny web tcp/22")]},
            "policy": {"examples": [("Default-deny (allowlist)", "delonix ingress policy db deny")]},
            "publish": {"examples": [("Publicar uma porta pelo ingress (DNAT)", "delonix ingress publish web 8080:80")]},
            "ls": {"examples": [("Ver regras + publishes", "delonix ingress ls db")]},
        },
    },
    "egress": {
        "title": "delonix egress",
        "tagline": "Firewall de SAÍDA (regras L4 + política de egress→Internet por-rede).",
        "intro": """A outra metade do firewall. Governa a SAÍDA de um container (regras allow/deny + política
por omissão) e, ao nível da REDE, a política de egress para a Internet: <code>allow</code>/<code>deny</code>,
ou <code>allowlist</code> (nega tudo excepto DNS e os CIDRs dados). Tudo sobre o mesmo <code>ContainerFw</code>
/nft do <code>ingress</code>.""",
        "subs": {
            "allow": {"examples": [("Só deixar sair HTTPS", "delonix egress allow app tcp/443 --to 0.0.0.0/0")]},
            "policy": {"examples": [("Default-deny de saída", "delonix egress policy app deny")]},
            "net": {"examples": [("Egress de uma rede em allowlist (só DNS + estes CIDRs)", "delonix egress net backend allowlist --to 10.0.0.0/8,1.1.1.1/32")]},
            "host": {"examples": [
                ("Só deixar sair para o GitHub (e *.github.com), aprendido do DNS",
                 "delonix egress host backend github.com"),
            ], "notes": """<p>O que o nft/CIDR não faz: allowlist por <strong>hostname</strong>. O resolver DNS
interno do ingress passa a snoopar os A-records das respostas e injecta-os num <code>set</code> nft
por-rede (com timeout = expira com o TTL); o egress aceita esse set + DNS e dropa o resto. 100%
rootless (sem eBPF) — a FQDN-policy do Cilium, via nftables. Repetível para vários hostnames.</p>"""},
            "show": {"examples": [("Ver a política de egress de uma rede + os IPs FQDN aprendidos ao vivo", "delonix egress show backend")]},
            "ls": {"examples": [("", "delonix egress ls app")]},
        },
    },
    "httproute": {
        "title": "delonix httproute",
        "tagline": "Reverse-proxy L7/HTTP embutido (`kind: HTTPRoute`) — routing por Host + prefixo de path.",
        "intro": """Um reverse-proxy HTTP/HTTPS <strong>embutido</strong> (hyper puro, sem Nginx/Envoy),
que corre dentro do netns do holder e roteia por <code>Host</code> + prefixo de <code>path</code> para
containers backend na SDN. TLS termina no proxy (self-signed ou <code>secretRef</code>); reload a
quente por SIGHUP (as rotas trocam sem downtime, os listeners ficam fixos no arranque). Um container
com <code>--expose &lt;porta&gt;</code> auto-regista-se sob
<code>&lt;nome&gt;.&lt;namespace&gt;.delonix.internal</code>, sem precisar de nenhum
<code>kind: HTTPRoute</code> manual. É o que o <code>kind: Tunnel</code> normalmente põe à frente
para dar uma única URL pública a vários backends.""",
        "subs": {
            "apply": {"examples": [
                ("Aplicar as HTTPRoutes de um manifesto (sobe/recarrega o proxy)",
                 "delonix httproute apply -f delonix-manifest.yaml"),
            ]},
            "ls": {"examples": [
                ("Estado do proxy + rotas activas", "delonix httproute ls"),
            ]},
            "rm": {"examples": [
                ("Parar o proxy e despublicar as portas", "delonix httproute rm"),
            ]},
        },
    },
    "tunnel": {
        "title": "delonix tunnel",
        "tagline": "Expõe uma porta local à internet pública via pinggy/ngrok/cloudflare (`kind: Tunnel`).",
        "intro": """Faz UMA coisa: leva tráfego da internet pública até UMA porta local — sem conta,
sem IP público, sem configurar o router. Junta-se ao <code>httproute</code> apontando
<code>--local-port</code> para a porta onde o proxy L7 escuta, e o routing por <code>Host</code>
do lado de lá continua a decidir para que container vai cada pedido — uma só URL pública, vários
backends. Três providers, cada um o binário/mecanismo REAL desse serviço (nunca simulado):
<strong>pinggy</strong> (zero binário extra — <code>ssh</code> puro, já uma dependência do
projecto), <strong>ngrok</strong> (precisa do agente <code>ngrok</code> no PATH; a URL pública sai
da API local do próprio agente) e <strong>cloudflare</strong> (precisa de <code>cloudflared</code>;
por agora só o quick-tunnel efémero <code>*.trycloudflare.com</code>, sem conta — um tunnel
NOMEADO com domínio próprio precisa da API do Cloudflare, ainda por implementar).""",
        "subs": {
            "expose": {"examples": [
                ("Expor uma porta local sem escrever manifesto (pinggy, grátis, efémero)",
                 "delonix tunnel expose --name demo --provider pinggy --local-port 8080",
                 "tunnel/demo: running — https://oxipg-197-148-40-67.free.pinggy.net"),
            ], "notes": """<p>Validado ao vivo nesta mesma sessão: tráfego HTTPS real da internet
chegou a um servidor local através do tunnel (HTTP 200) usando exactamente este comando.</p>"""},
            "ls": {"examples": [
                ("Listar túneis (estado + URL pública)", "delonix tunnel ls",
                 "NAME    PROVIDER   LOCAL PORT   PUBLIC URL                                    STATUS    UPTIME\n"
                 "test1   pinggy          18234   https://oxipg-197-148-40-67.free.pinggy.net   Running   Up 34 seconds"),
            ]},
            "describe": {"examples": [
                ("Detalhe de um túnel", "delonix tunnel describe demo"),
            ]},
            "rm": {"examples": [
                ("Parar e remover (mata o processo agente a sério)", "delonix tunnel rm demo",
                 "tunnel/demo: removed"),
            ]},
        },
    },
    "flow": {
        "title": "delonix flow",
        "tagline": "Tráfego por-container ao vivo — datapath eBPF (degrada para contadores veth).",
        "intro": """Telemetria de rede por container. Quando corre com privilégio (CAP_BPF/root), attacha
dois classificadores tc/clsact em eBPF às veths da SDN, que contam bytes/pacotes por IP num BPF map
partilhado — <strong>sem nunca fazer drop</strong> (o nft continua o único enforcer). Sem privilégio
(o caso rootless comum) diz-o e cai nos contadores veth, que sempre funcionam. <code>--watch</code>
redesenha a cada 2s.""",
        "subs": {},
        "examples": [
            ("Uma amostra", "sudo delonix flow"),
            ("Monitorização contínua", "sudo delonix flow --watch"),
        ],
    },
    "boot": {
        "title": "delonix boot",
        "tagline": "Persistência no arranque: units systemd para os containers voltarem a subir no boot.",
        "intro": """<code>boot enable</code> gera uma unit systemd por container em execução (rootless →
user units + <code>loginctl enable-linger</code>; root → system units), com <code>ExecStart</code>
=<code>container start</code>. Assim os containers voltam a subir quando o host arranca, sem daemon.""",
        "subs": {
            "enable": {"examples": [("Persistir os que correm agora", "delonix boot enable")]},
            "status": {"examples": [("Ver o que está instalado", "delonix boot status")]},
            "disable": {"examples": [("Remover as units de boot", "delonix boot disable")]},
        },
    },
    "system": {
        "title": "delonix system",
        "tagline": "O motor em si: events, info, df, prune, monitor, thermal.",
        "intro": """Introspecção e manutenção. <code>system prune</code> é o GC (recupera espaço: containers
parados, dirs órfãos, imagens dangling, blobs CAS, hostfwds órfãos, redes vazias); <code>system df</code>
mostra o uso de disco; <code>system monitor</code> segue ligações/conntrack; <code>system events</code> o
fluxo de eventos.""",
        "subs": {
            "prune": {"examples": [("Recuperar espaço (GC)", "delonix system prune")]},
            "df": {"examples": [("Uso de disco", "delonix system df")]},
            "info": {"examples": [("", "delonix system info")]},
        },
    },
    "dash": {
        "title": "delonix dash",
        "tagline": "Dashboard de resumo/KPIs (TUI estilo htop), global ou por grupo.",
        "intro": """Vista viva do estado do runtime — containers, VMs, imagens, redes, storage — num
só ecrã, sem precisar de correr <code>ls</code> em 5 grupos diferentes. Cada grupo também tem o
seu próprio (<code>container dash</code>, <code>vm dash</code>, ...); este é o agregado global.
<code>--once</code> imprime um snapshot de texto e sai (scripts/CI) — é também o que acontece
automaticamente quando o stdout não é um terminal.""",
        "subs": {},
        "examples": [
            ("TUI interactiva", "delonix dash"),
            ("Snapshot único, para um script", "delonix dash --once"),
        ],
    },
    "docker-api": {
        "title": "delonix docker-api",
        "tagline": "Fatia SÓ-LEITURA da API Docker Engine, num socket unix — `docker version/ps/images/info`.",
        "intro": """Serve o suficiente da API real do Docker Engine (protocolo capturado ao vivo
contra um <code>docker</code> CLI real, versão negociada via o header <code>Api-Version</code> da
resposta ao <code>/_ping</code>) para <code>docker version</code>/<code>ps</code>/<code>images</code>/
<code>info</code> apontados via <code>DOCKER_HOST=unix://&lt;socket&gt;</code> funcionarem contra o
estado REAL do delonix — útil para ferramentas que só sabem falar com a API do Docker. Mesma postura
de segurança do socket de gestão: 0600 + <code>SO_PEERCRED</code> (só o próprio utilizador). Por
fazer: mutações (<code>create</code>/<code>start</code>/<code>exec</code>) — o que falta para
<code>docker run</code>/<code>docker compose up</code>; qualquer rota não implementada dá 404 claro.""",
        "subs": {},
        "examples": [
            ("Servir no socket por omissão", "delonix docker-api &"),
            ("Um `docker` real a falar com o delonix",
             "DOCKER_HOST=unix:///run/delonix-docker.sock docker ps"),
        ],
    },
    "kube": {
        "title": "delonix kube",
        "tagline": "Gera manifestos Kubernetes a partir de containers.",
        "intro": """<code>kube generate</code> produz um manifesto <code>kind: Pod</code> a partir de um
container existente — a ponte para exportar uma carga do runtime local para um cluster.""",
        "subs": {
            "generate": {"examples": [("Pod a partir de um container", "delonix kube generate web > web-pod.yaml")]},
        },
    },
    "netns": {
        "title": "delonix netns",
        "tagline": "Gestão de baixo nível da infra de ingress rootless.",
        "intro": """A camada crua por baixo do <code>ingress</code>/<code>egress</code>: subir/descer o
holder do ingress, attach/detach de netns, publish/unpublish de portas e firewall por container. A
maioria dos utilizadores nunca precisa disto — usa os grupos de alto nível — mas está exposto para
depuração e integração.""",
        "subs": {
            "status": {"examples": [("Estado da infra de ingress", "delonix netns status")]},
            "up": {"examples": [("Subir o holder do ingress", "delonix netns up")]},
        },
    },
    "completion": {
        "title": "delonix completion",
        "tagline": "Autocompletion dinâmico para bash, zsh, fish, elvish e powershell.",
        "intro": """Imprime o script de registo do shell. A engine é dinâmica: o script pede as
sugestões ao próprio binário em tempo real, a partir da MESMA definição usada no parsing — nunca
fica desactualizado à mão.""",
        "subs": {},
        "examples": [
            ("Bash (persistente)", 'echo \'source <(delonix completion bash)\' >> ~/.bashrc'),
            ("Zsh", 'echo \'source <(delonix completion zsh)\' >> ~/.zshrc'),
        ],
    },
}

# ---------------------------------------------------------------- template

CSS = """
:root{--accent:#e8590c;--accent-soft:#fff0e6;--ink:#1a1a2e;--muted:#5a6472;--line:#e6e8ec;
--bg:#ffffff;--side:#f7f8fa;--code-bg:#0f172a;--code-ink:#e2e8f0;--radius:10px}
@media (prefers-color-scheme: dark){:root{--ink:#e6e8ee;--muted:#9aa4b2;--line:#252a33;
--bg:#0d1117;--side:#10151c;--accent-soft:#2a1810;--code-bg:#161b22;--code-ink:#dbe2ea}}
*{box-sizing:border-box}body{margin:0;font:16px/1.65 -apple-system,'Segoe UI',Roboto,Ubuntu,
sans-serif;color:var(--ink);background:var(--bg)}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
.layout{display:flex;min-height:100vh}
nav.side{width:270px;flex-shrink:0;background:var(--side);border-right:1px solid var(--line);
padding:1.2rem 1rem 3rem;position:sticky;top:0;height:100vh;overflow-y:auto}
nav.side .brand{display:flex;align-items:center;gap:.55rem;font-weight:700;font-size:1.12rem;
margin-bottom:1.1rem;color:var(--ink)}
nav.side .brand .dot{width:26px;height:26px;border-radius:7px;background:var(--accent);
display:inline-flex;align-items:center;justify-content:center;color:#fff;font-size:.85rem}
nav.side h5{margin:1.3rem 0 .3rem;font-size:.72rem;letter-spacing:.09em;text-transform:uppercase;
color:var(--muted)}
nav.side a{display:block;padding:.28rem .55rem;border-radius:6px;color:var(--ink);font-size:.93rem}
nav.side a:hover{background:var(--accent-soft);text-decoration:none}
nav.side a.on{background:var(--accent-soft);color:var(--accent);font-weight:600}
main{flex:1;min-width:0;padding:2.2rem 3rem 5rem;max-width:940px}
main h1{font-size:1.9rem;margin:.2rem 0 .4rem}
main h2{font-size:1.35rem;margin-top:2.4rem;padding-bottom:.35rem;border-bottom:1px solid var(--line)}
main h3{font-size:1.05rem;margin-top:1.6rem}
.tagline{color:var(--muted);font-size:1.05rem;margin-top:0}
code{background:var(--accent-soft);padding:.1em .35em;border-radius:5px;font-size:.9em}
pre{background:var(--code-bg);color:var(--code-ink);padding:1rem 1.2rem;border-radius:var(--radius);
overflow-x:auto;font-size:.86rem;line-height:1.55}
pre code{background:none;padding:0;color:inherit;font-size:inherit}
.help pre{border-left:4px solid var(--accent)}
.ex{margin:.9rem 0}.ex .cap{font-size:.88rem;color:var(--muted);margin-bottom:.25rem}
.ex .out,div.out{margin-top:.4rem}
.ex .out pre,div.out pre{background:transparent;border:1px dashed var(--line);color:var(--muted);
padding:.7rem 1rem;font-size:.82rem}
.ex .out::before,div.out::before{content:"→ resultado";display:block;font-size:.75rem;color:var(--muted);
margin-bottom:.15rem;letter-spacing:.03em;text-transform:uppercase}
table{border-collapse:collapse;width:100%;font-size:.92rem}
td,th{border:1px solid var(--line);padding:.5rem .7rem;text-align:left;vertical-align:top}
th{background:var(--side)}
.cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(240px,1fr));gap:1rem;margin:1.4rem 0}
.card{border:1px solid var(--line);border-radius:var(--radius);padding:1rem 1.1rem}
.card b{display:block;margin-bottom:.25rem}.card a{font-weight:600}
.card p{margin:.2rem 0 0;font-size:.88rem;color:var(--muted)}
.arch{display:flex;flex-direction:column;gap:.6rem;margin:1.4rem 0}
.arch .row{display:flex;gap:.6rem;flex-wrap:wrap}
.arch .box{flex:1;min-width:150px;border:1.5px solid var(--accent);border-radius:8px;
padding:.55rem .8rem;font-size:.85rem;background:var(--accent-soft)}
.arch .box.mut{border-color:var(--line);background:var(--side)}
.arch .box b{display:block;font-size:.9rem}
.pill{display:inline-block;background:var(--accent);color:#fff;border-radius:99px;
padding:.05rem .6rem;font-size:.75rem;font-weight:600;vertical-align:middle}
.tag{display:inline-block;border-radius:6px;padding:.1rem .5rem;font-size:.82rem;font-weight:600}
.tag.ok{background:#d8f3dc;color:#1b4332}.tag.mid{background:#fff3bf;color:#5c4a00}
.tag.no{background:#ffe3e3;color:#7a1420}
@media (prefers-color-scheme: dark){.tag.ok{background:#0f3d24;color:#8fe3ac}
.tag.mid{background:#453800;color:#ffe066}.tag.no{background:#4a1015;color:#ffa8a8}}
.callout{border:1.5px solid var(--accent);border-radius:var(--radius);padding:1rem 1.2rem;margin:1.4rem 0;
background:var(--accent-soft)}
.callout.warn{border-color:#e03131;background:#fff0f0}
.callout.warn b{color:#c92a2a}
@media (prefers-color-scheme: dark){.callout.warn{background:#3a1414}.callout.warn b{color:#ff8787}}
.callout p:first-child{margin-top:0}.callout p:last-child{margin-bottom:0}
footer{margin-top:4rem;color:var(--muted);font-size:.85rem;border-top:1px solid var(--line);padding-top:1rem}
@media (max-width:840px){.layout{flex-direction:column}nav.side{width:100%;height:auto;position:static}
main{padding:1.4rem 1.2rem 4rem}}
"""


def sidebar(active, depth=0):
    p = "../" * depth
    items_docs = [
        ("index.html", "Início"),
        ("cheatsheet.html", "Cheatsheet"),
        ("kinds.html", "Kinds e templates"),
        ("arquitectura.html", "Arquitectura"),
        ("c4.html", "Modelo C4 e system design"),
        ("cri.html", "CRI — kubelet sem containerd"),
        ("comparacao.html", "Delonix vs Docker vs Podman"),
        ("tutorial-delonix-temp.html", "Projecto completo: Delonix Temp"),
    ]
    items_cmd = [(f"comandos/{g}.html", GROUPS[g]["title"]) for g in GROUPS]
    def link(href, label):
        cls = ' class="on"' if href == active else ""
        return f'<a href="{p}{href}"{cls}>{html.escape(label)}</a>'
    return f"""<nav class="side">
<div class="brand"><span class="dot">▲</span> Delonix Engine</div>
<h5>Documentação</h5>
{''.join(link(h, l) for h, l in items_docs)}
<h5>Referência CLI</h5>
{''.join(link(h, l) for h, l in items_cmd)}
<h5>Projecto</h5>
<a href="https://github.com/angolardevops/delonix-runtime">GitHub</a>
<a href="https://github.com/angolardevops/delonix-runtime/releases">Releases</a>
</nav>"""


def page(path, title, body, depth=0):
    doc = f"""<!DOCTYPE html>
<html lang="pt">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{html.escape(title)} · Delonix Engine</title>
<style>{CSS}</style>
</head>
<body>
<div class="layout">
{sidebar(path, depth)}
<main>
{body}
<footer>Delonix Engine · Apache-2.0 · <a href="https://github.com/angolardevops/delonix-runtime">angolardevops/delonix-runtime</a>
· Referência gerada do <code>--help</code> real do binário por <code>docs/gen.py</code>.</footer>
</main>
</div>
</body>
</html>"""
    out = os.path.join(ROOT, path)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as f:
        f.write(doc)


def examples_html(exs):
    """Each example is (caption, command) or (caption, command, output) — the
    3rd, optional element is REAL output captured from an actual run (never
    invented), rendered in a dimmer block right under the command so a reader
    sees not just how to invoke something but what it actually returns."""
    parts = []
    for ex in exs:
        cap, cmd = ex[0], ex[1]
        out = ex[2] if len(ex) > 2 else None
        cap_html = f'<div class="cap">{html.escape(cap)}</div>' if cap else ""
        out_html = f'<div class="out"><pre><code>{html.escape(out)}</code></pre></div>' if out else ""
        parts.append(
            f'<div class="ex">{cap_html}<pre><code>{html.escape(cmd)}</code></pre>{out_html}</div>'
        )
    return "".join(parts)


def group_page(name, g):
    body = [f"<h1>{html.escape(g['title'])}</h1><p class='tagline'>{html.escape(g['tagline'])}</p>"]
    body.append(f"<p>{g['intro']}</p>")
    top_help = help_of(name) if name != "completion" else help_of("completion")
    body.append(f"<div class='help'><pre><code>{html.escape(top_help)}</code></pre></div>")
    if g.get("examples"):
        body.append("<h2>Exemplos</h2>" + examples_html(g["examples"]))
    if g.get("extra"):
        body.append(g["extra"])
    for sub, meta in g["subs"].items():
        args = ["image", "--vm", sub] if name == "image" and sub in ("push", "build") else [name, sub]
        body.append(f"<h2 id='{sub}'><code>{html.escape(name)} {html.escape(sub)}</code></h2>")
        body.append(f"<div class='help'><pre><code>{html.escape(help_of(*args))}</code></pre></div>")
        if meta.get("notes"):
            body.append(meta["notes"])
        if meta.get("examples"):
            body.append("<h3>Exemplos</h3>" + examples_html(meta["examples"]))
    page(f"comandos/{name}.html", g["title"], "\n".join(body), depth=1)


INDEX = """
<h1>Delonix Engine <span class="pill">v{ver}</span></h1>
<p class="tagline"><strong>Engine</strong> de containers e microVMs <strong>daemonless</strong>,
<strong>rootless-first</strong>, kernel-native, em Rust — com CRI próprio para Kubernetes.
<em>O engine open-source que alimenta o Delonix.</em></p>

<p>Não é um <em>runtime</em> OCI de baixo nível (isso é o <code>runc</code>/<code>crun</code>): é um
engine COMPLETO de containers <strong>e</strong> VMs — build, run, rede, firewall, storage e
bootstrap de clusters Kubernetes, tudo num só binário. É a camada aberta (Apache-2.0) sobre a qual
assenta a plataforma <strong>Delonix</strong>.</p>

<p>O Delonix Engine faz o trabalho do Docker/Podman sem daemon residente: cada comando é um
processo efémero que fala directamente com o kernel (namespaces, cgroups v2, pivot_root),
guarda estado em ficheiros e desaparece. Em rootless, a rede é servida por um único par
holder-netns + slirp4netns partilhado — não um slirp por container — com DNAT nft para o
publish de portas, o que permite <em>trocar portas e volumes a quente</em>, sem reiniciar o
container.</p>

<h2>Instalar</h2>
<p>Um comando instala o binário <strong>e</strong> todas as dependências de runtime
(rede rootless, VMs, tuning de kernel), escolhendo a variante certa para o teu CPU.
Funciona em Debian/Ubuntu, Fedora/RHEL, openSUSE e Arch:</p>
<pre><code>curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash</code></pre>
<p>Alternativa (só o binário, dependências por tua conta):</p>
<pre><code>curl -fL -o ~/.local/bin/delonix \\
  https://github.com/angolardevops/delonix-runtime/releases/latest/download/delonix-x86_64-linux
chmod +x ~/.local/bin/delonix
echo 'source &lt;(delonix completion bash)' &gt;&gt; ~/.bashrc</code></pre>

<h2>Primeiros passos</h2>
<pre><code># um serviço web na porta 8080, sem root, sem daemon
delonix container run -d --name web -p 8080:80 nginx
curl localhost:8080

delonix container stats          # CPU/memória/PIDs
delonix container logs -f web    # logs em contínuo
delonix container stop web       # a porta fecha sozinha
delonix container start web      # rearranca com o mesmo estado</code></pre>

<h2>Referência da CLI</h2>
<div class="cards">{cards}</div>

<h2>Porque é diferente</h2>
<table>
<tr><th></th><th>Docker</th><th>Podman</th><th>Delonix</th></tr>
<tr><td>Daemon</td><td>dockerd (root)</td><td>não (conmon por container)</td><td>não — e sem monitor residente por container</td></tr>
<tr><td>Rootless</td><td>opcional</td><td>sim (slirp/pasta por container)</td><td>por omissão — 1 slirp partilhado + ingress nft</td></tr>
<tr><td>VMs</td><td>—</td><td>machine (para si próprio)</td><td>microVMs declarativas de 1.ª classe (Cloud Hypervisor/libvirt)</td></tr>
<tr><td>Kubernetes</td><td>—</td><td>—</td><td>CRI próprio + bootstrap kubeadm do zero (<code>delonix cluster</code>)</td></tr>
</table>
"""

ARCH = """
<h1>Arquitectura</h1>
<p class="tagline">8 crates, um binário — e nenhum processo residente.</p>

<h2>Visão geral</h2>
<div class="arch">
<div class="row"><div class="box mut" style="flex:3"><b>delonix (CLI) — delonix-runtime-bin</b>
comandos agrupados: container · image · build · vm · volumes · network · stack · cluster</div></div>
<div class="row">
<div class="box"><b>delonix-runtime</b>clone() + namespaces (mount/pid/ipc/uts/net/user/cgroup),
pivot_root, seccomp/caps, cgroups v2 delegados, exec, reconcile</div>
<div class="box"><b>delonix-image</b>pull OCI (digest verificado), build, export, buildpacks CNB,
assinaturas, registo</div>
<div class="box"><b>delonix-net</b>SDN rootless: holder netns + bridge + slirp único, DNAT/firewall
nft, DNS interno, overlay WireGuard</div>
</div>
<div class="row">
<div class="box"><b>delonix-vm</b>microVMs (trait VmBackend: Cloud Hypervisor · libvirt), cloud-init</div>
<div class="box"><b>delonix-volume</b>volumes nomeados, bind mounts, quotas, nfs</div>
<div class="box"><b>delonix-cri</b>servidor CRI runtime.v1 — o kubelet fala com o Delonix</div>
<div class="box mut"><b>delonix-runtime-core</b>tipos partilhados: Container, Vm, Status, Store JSON,
Secret Manager</div>
</div>
</div>

<h2>Daemonless a sério</h2>
<p>Não há daemon, nem sequer um monitor por container (o conmon do podman). O <code>run</code> faz
<code>clone()</code> directo; em modo detached, um <em>shim</em> de logging efémero fica só a escoar o
stdout/stderr para o ficheiro de log (com rotação) e morre com o container. O estado
(spec completa de cada container/VM/volume/rede) vive em JSON sob <code>$DELONIX_ROOT</code> —
o <code>ps</code>/<code>start</code>/<code>inspect</code> reconstruem tudo daí, e <em>reapers</em>
oportunistas limpam órfãos (slirp sem alvo, hostfwd sem container) a cada invocação relevante.</p>

<h2>Rootless-first</h2>
<p>Sem root, o isolamento vem de user namespaces com mapeamento de subuid
(<code>newuidmap</code>/<code>newgidmap</code>, como o podman) — o uid 0 do container é um uid
não-privilegiado do host. O rootfs é uma cópia flat persistente por container (em root, overlayfs
com upper preservado). Com <code>--privileged</code> + labels de node Kind, o runtime prepara a
delegação de cgroup v2 dedicada que um systemd aninhado (kindest/node) exige.</p>

<h2>Rede rootless: o ingress</h2>
<div class="arch">
<div class="row">
<div class="box mut" style="flex:1"><b>host</b>portas publicadas (127.0.0.1 por omissão;
<code>DELONIX_PUBLISH_ADDR</code> para expor)</div>
<div class="box" style="flex:1"><b>holder netns (1 por utilizador)</b>bridge delonix0 ·
slirp4netns único · nft (DNAT «pre», firewall) · DNS interno com os nomes dos containers</div>
<div class="box mut" style="flex:1"><b>containers</b>veth por container, ligados à bridge;
IP determinístico por id</div>
</div>
</div>
<p>Publicar uma porta = um <code>add_hostfwd</code> no api-socket do slirp único + uma regra DNAT na
chain de ingress — <strong>estado do dataplane, não do container</strong>. É por isso que portas (e
volumes, via mounts live) se trocam a quente: o processo do container nunca é tocado. Com
<code>--net host</code> + <code>-p</code>, o container recebe um netns próprio com um slirp4netns
dedicado (modelo podman), que morre com ele.</p>

<h2>Segurança</h2>
<p>Rootless por omissão; seccomp e drop de capabilities fora de <code>--privileged</code>, com
arranque a FALHAR se não ficarem mesmo activos; pull com verificação de digest (incluindo
artefactos VM OCI); inputs de manifesto de recursos como <code>Cluster</code>/<code>Vm</code>
validados por whitelist antes de chegarem a qualquer shell remoto.</p>
<div class="callout warn">
<p><b>Auditoria de 2026-07-21 — 6 achados de severidade alta, CORRIGIDOS em 2026-07-23</b> (o
<code>COPY</code> do build, por exemplo, era contornável por symlink apesar de uma correcção
anterior ter tentado fechá-lo — agora canonicaliza e confirma o confinamento). Não há indícios de
RCE pela rede, mas os fixes ainda não foram confirmados por uma 2.ª auditoria independente e o
núcleo de syscalls nunca teve revisão de segurança — por prudência, evita ainda imagens/manifestos
não confiáveis ou expor o motor num host partilhado até à confirmação. Detalhe completo em
<a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/AUDITORIA-E2E.md">AUDITORIA-E2E.md</a>
— ver também a <a href="comparacao.html">comparação com Docker/Podman</a> para o estado geral do
projecto.</p>
</div>
"""

CRI = """
<h1>CRI — kubelet sem containerd</h1>
<p class="tagline">O crate <code>delonix-cri</code> implementa o Container Runtime Interface
(<code>runtime.v1</code>) do Kubernetes.</p>

<p>O kubelet não sabe correr containers — delega num runtime via CRI (gRPC sobre socket unix).
Normalmente esse runtime é o containerd ou o CRI-O; com o Delonix, é o binário
<code>delonix-cri</code>: <em>pods e containers do Kubernetes a correr directamente sobre o motor
Delonix</em>, sem mais nenhuma peça.</p>

<h2>Como se liga</h2>
<pre><code># o serviço (a imagem VM dourada já o traz como unit systemd)
DELONIX_CRI_ADDR=/run/delonix-cri.sock delonix-cri

# o kubelet aponta para lá
kubelet --container-runtime-endpoint=unix:///run/delonix-cri.sock …</code></pre>

<h2>O que implementa</h2>
<table>
<tr><th>Área CRI</th><th>Suporte</th></tr>
<tr><td>RuntimeService — sandboxes (pods)</td><td>criação do pod sandbox com netns partilhado
(os containers do pod juntam-se à rede do sandbox via <code>join_netns</code>), labels/annotations,
estado e remoção</td></tr>
<tr><td>RuntimeService — containers</td><td>create/start/stop/remove, exec, logs em formato CRI
(<code>&lt;rfc3339nano&gt; stdout F linha</code>), limites cpu/memória por pod via cgroups v2</td></tr>
<tr><td>ImageService</td><td>pull (digest verificado), list, status, remove — sobre o
<code>ImageStore</code> normal do Delonix</td></tr>
<tr><td>Rede</td><td>compatibilidade CNI (attach/detach por conf JSON)</td></tr>
</table>

<h2>Do zero a um cluster</h2>
<p>É esta peça que fecha o ciclo do <code>delonix cluster</code>: a imagem VM dourada
(<code>delonix image --vm build</code>) já traz kubeadm/kubelet/kubectl e o
<code>delonix-cri</code> activo; <code>delonix cluster kubeadm</code> provisiona as VMs e faz o
bootstrap — o cluster resultante corre Kubernetes com o Delonix como runtime de ponta a ponta.</p>
"""


COMPARE = """
<h1>Delonix vs Docker vs Podman</h1>
<p class="tagline">Comparação honesta, para decidir com que motor construir — não um argumento de
venda.</p>

<p>O Delonix Engine é um motor de containers e microVMs <strong>daemonless, rootless-first</strong>,
em Rust, com Kubernetes de raiz (CRI próprio). Em vários pontos concretos já vai mais longe que o
Docker e o Podman rootless. Noutros, fica muito atrás. Esta página diz exactamente onde é onde —
para uma pessoa a decidir o que instalar hoje, ou uma empresa a avaliar para produção.</p>

<div class="callout warn">
<p><b>Estado actual (2026-07): beta público, em hardening activo.</b> Uma auditoria de segurança
independente encontrou 6 falhas de severidade alta (essencialmente: escrita/eliminação de
ficheiros fora do esperado a partir de uma imagem ou manifesto não confiável, e um socket de
gestão sem autenticação) — <strong>as 6 já estão corrigidas</strong>, mas ainda NÃO foram
confirmadas por uma 2.ª auditoria adversarial independente, e o núcleo de syscalls do motor
(clone/mount/namespaces) nunca teve revisão de segurança nenhuma. Não há indícios de execução
remota de código a partir da rede — a fronteira rootless→root está sólida — mas
<strong>por prudência, evita ainda imagens não confiáveis ou expor o motor num host partilhado por
várias pessoas</strong> até à confirmação. Detalhe completo, com ficheiro e linha de cada achado e
o estado da correcção:
<a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/AUDITORIA-E2E.md">relatório
da auditoria</a>.</p>
</div>

<h2>Decisão rápida</h2>
<table>
<tr><th>Se precisas de…</th><th>Usa</th></tr>
<tr><td>Correr um <code>docker-compose.yml</code> já existente</td><td>Docker ou Podman</td></tr>
<tr><td>Um pipeline de build com BuildKit (segredos de build, cross-compile)</td>
<td>Docker ou Podman — o Delonix faz multi-stage e cache de camadas (rootless), mas não tem
BuildKit</td></tr>
<tr><td>Cargas GPU/CUDA</td><td>Docker ou Podman (com nvidia-container-toolkit)</td></tr>
<tr><td><code>docker version</code>/<code>ps</code>/<code>images</code>/<code>info</code> via
<code>DOCKER_HOST</code> (scripts/CI de leitura)</td>
<td><strong>Delonix</strong> — <code>delonix docker-api</code>, validado contra um
<code>docker</code> CLI real</td></tr>
<tr><td><code>docker run</code>/<code>docker compose up</code>/testcontainers (precisam de
criar containers via a API)</td><td>Docker ou Podman — só a parte de leitura da API está feita</td></tr>
<tr><td>Bootstrap de um cluster Kubernetes real sem instalar Docker/containerd</td>
<td><strong>Delonix</strong> — CRI próprio, já validado com um control-plane v1.34 <code>Ready</code></td></tr>
<tr><td>Um só motor para containers <strong>e</strong> microVMs <strong>e</strong> Kubernetes</td>
<td><strong>Delonix</strong> — ninguém no espaço Docker/Podman cobre isto junto</td></tr>
<tr><td>Trocar portas/volumes/redes de um container a quente, sem o recriar</td>
<td><strong>Delonix</strong> — o Docker obriga a recriar</td></tr>
<tr><td>Rede rootless avançada (overlay cifrado entre nós, firewall dirigido por container)</td>
<td><strong>Delonix</strong> — acima do Podman rootless nestes pontos</td></tr>
<tr><td>Um motor com anos de produção, comunidade enorme, máxima compatibilidade de ferramentas</td>
<td>Docker ou Podman — ainda sem substituto à vista</td></tr>
</table>

<h2>Comparação por área</h2>
<p><span class="tag ok">forte</span> · <span class="tag mid">parcial ou com limitações</span> ·
<span class="tag no">ausente</span></p>

<table>
<tr><th>Área</th><th>Docker</th><th>Podman</th><th>Delonix</th></tr>
<tr><td>Correr/parar/inspeccionar containers</td>
<td><span class="tag ok">forte</span></td><td><span class="tag ok">forte</span></td>
<td><span class="tag ok">forte</span> — mais reconfiguração a quente e diagnóstico automático de
crash (razão + snapshot do log, não só "Exited")</td></tr>
<tr><td>Rootless por omissão</td>
<td><span class="tag no">não é o modo por omissão</span></td>
<td><span class="tag ok">forte, é a proposta do Podman</span></td>
<td><span class="tag ok">forte — e falha de propósito se o isolamento não ficar activo</span></td></tr>
<tr><td>Build de imagens (<code>Dockerfile</code>)</td>
<td><span class="tag ok">forte — multi-stage, BuildKit, cache</span></td>
<td><span class="tag ok">forte — via buildah</span></td>
<td><span class="tag mid">multi-stage + ARG/USER/ENTRYPOINT + cache de camadas (rootless) já
funcionam; sem BuildKit real (sem <code>RUN --mount=secret</code>)</span></td></tr>
<tr><td><code>docker compose</code> / orquestração local</td>
<td><span class="tag ok">nativo</span></td><td><span class="tag mid">podman-compose</span></td>
<td><span class="tag no">manifesto próprio, sem parser de compose</span></td></tr>
<tr><td>Rede rootless avançada (overlay inter-nó, firewall por-container)</td>
<td><span class="tag no">overlay exige swarm</span></td>
<td><span class="tag mid">sem overlay rootless nativo</span></td>
<td><span class="tag ok">VXLAN+WireGuard rootless, firewall dirigido por container</span></td></tr>
<tr><td>Bootstrap de Kubernetes sem Docker/containerd</td>
<td><span class="tag no">não é o papel do Docker</span></td>
<td><span class="tag no">não tem CRI próprio</span></td>
<td><span class="tag ok">CRI próprio + <code>cluster kubeadm</code>, validado com cluster real</span></td></tr>
<tr><td>MicroVMs no mesmo motor</td>
<td><span class="tag no">ausente</span></td><td><span class="tag no">ausente</span></td>
<td><span class="tag ok">Cloud Hypervisor / libvirt, declarativo</span></td></tr>
<tr><td>GPU/CUDA</td>
<td><span class="tag ok">nvidia-container-toolkit maduro</span></td>
<td><span class="tag ok">idem</span></td>
<td><span class="tag no">só bind dos nós de dispositivo, sem injecção de driver</span></td></tr>
<tr><td>Assinatura de imagens + scan de CVE embutidos</td>
<td><span class="tag no">precisa de cosign/trivy à parte</span></td>
<td><span class="tag no">idem</span></td>
<td><span class="tag ok">cosign/sigstore + scan de CVE no próprio motor</span></td></tr>
<tr><td>Maturidade de segurança EM PRODUÇÃO (anos de uso adversarial real)</td>
<td><span class="tag ok">muito madura</span></td><td><span class="tag ok">muito madura</span></td>
<td><span class="tag mid">projecto novo — auditoria própria já encontrou e corrigiu falhas altas,
ainda sem confirmação independente, ver aviso acima</span></td></tr>
<tr><td>Ecossistema (docs, fóruns, integrações de terceiros)</td>
<td><span class="tag ok">enorme</span></td><td><span class="tag ok">grande</span></td>
<td><span class="tag no">início — este site + o repositório é tudo o que há por agora</span></td></tr>
</table>

<h2>Onde o Delonix já vai mais longe</h2>
<ul>
<li><strong>Um motor só, três problemas</strong> — containers, microVMs e Kubernetes (via CRI
próprio) na mesma ferramenta. Já correu um control-plane Kubernetes v1.34 completo
<code>Ready</code>, com o próprio <code>kube-proxy</code> a programar netfilter dentro do modelo
rootless.</li>
<li><strong>Reconfiguração a quente</strong> — mudar portas, volumes, redes ou limite de banda de um
container <em>sem o recriar</em> e com o mesmo PID. No Docker, mudar uma porta obriga a apagar e
recriar o container.</li>
<li><strong>Diagnóstico automático de crash</strong> — quando um container morre inesperadamente, o
Delonix regista a razão (processo desapareceu vs PID reciclado) e guarda um excerto do log
automaticamente. Docker e Podman só dizem "Exited"/"Dead".</li>
<li><strong>Segurança rootless mais rígida por desenho</strong> — no-new-privs sempre activo, e o
arranque de um container <em>falha</em> se seccomp/capabilities não ficarem mesmo a valer, em vez
de seguir em frente a fingir que está protegido.</li>
<li><strong>Storage de rede estilo Kubernetes</strong> — uma pasta NFS/CIFS/WebDAV vira um volume
nomeado montável por qualquer container, como um <code>PersistentVolume</code>.</li>
</ul>

<h2>Onde ainda não chega</h2>
<ul>
<li><strong>Não corre um <code>docker-compose.yml</code> existente</strong> — sem parser de compose.
A API do Docker (<code>DOCKER_HOST</code>) já responde a leituras (<code>version</code>/<code>ps</code>/
<code>images</code>/<code>info</code>), mas ainda não cria containers — testcontainers e
<code>docker compose up</code> continuam sem se ligar.</li>
<li><strong>Build de imagens ainda não tem BuildKit real</strong> — multi-stage,
<code>ARG</code>/<code>--build-arg</code>, <code>USER</code>/<code>ENTRYPOINT</code> e cache de
camadas (rootless) já funcionam, mas sem <code>RUN --mount=secret</code> nem
<code>--platform</code>.</li>
<li><strong>Sem GPU real</strong> — nenhuma carga CUDA corre hoje.</li>
<li><strong>Projecto novo</strong> — sem o histórico de produção que o Docker e o Podman têm; ver o
aviso de segurança no topo desta página antes de decidir.</li>
</ul>

<h2>Recomendação por perfil</h2>
<table>
<tr><th>Quem és</th><th>Sugestão</th></tr>
<tr><td>Programador(a) a experimentar em local/homelab, ou a fazer bootstrap de um cluster
Kubernetes pequeno sem instalar Docker</td>
<td>Experimenta o Delonix hoje — é exactamente o caso em que já está forte.</td></tr>
<tr><td>Equipa com um pipeline de build maduro (BuildKit, compose)</td>
<td>Fica no Docker/Podman para o build; podes correr as imagens resultantes no Delonix se quiseres
testar a operação — multi-stage e cache de camadas já funcionam (rootless), mas sem BuildKit
real.</td></tr>
<tr><td>Empresa a avaliar para produção multi-tenant ou com dados sensíveis</td>
<td>Os 6 achados de segurança altos já estão corrigidos, mas aguarda a confirmação por uma 2.ª
auditoria independente (aviso acima) antes de expor o motor a imagens ou utilizadores não
confiáveis — acompanha o
<a href="https://github.com/angolardevops/delonix-runtime/releases">changelog</a>.</td></tr>
<tr><td>Quer avaliar tecnicamente ao detalhe (gap-a-gap, com ficheiro e linha)</td>
<td>Lê a <a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/COMPARACAO-DOCKER-PODMAN.md">análise de gaps completa</a> no repositório.</td></tr>
</table>
"""


def c4_page():
    """`c4.html` a partir do ARCHITECTURE.md canónico (Martin): markdown →
    HTML, blocos ```mermaid → <pre class="mermaid"> renderizados por mermaid.js
    (CDN). Regenerar sempre que o ARCHITECTURE.md mudar."""
    import re

    import markdown

    src = open(os.path.join(ROOT, "..", "ARCHITECTURE.md")).read()
    # separa os blocos mermaid ANTES do markdown (para não serem tratados como código)
    parts = re.split(r"```mermaid\n(.*?)```", src, flags=re.S)
    out = []
    for i, part in enumerate(parts):
        if i % 2 == 1:
            out.append(f'<pre class="mermaid">{html.escape(part)}</pre>')
        else:
            out.append(markdown.markdown(part, extensions=["tables", "fenced_code"]))
    body = (
        "\n".join(out)
        + """
<script type="module">
import mermaid from "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs";
mermaid.initialize({ startOnLoad: true, theme: matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'default' });
</script>
<style>
/* diagramas maiores: página larga + SVG a preencher a largura, com scroll se preciso */
main{max-width:1280px}
.mermaid{background:transparent;border:1px solid var(--line);border-radius:10px;
  padding:1.2rem;margin:1.4rem 0;overflow-x:auto;text-align:center}
.mermaid svg{width:100%!important;max-width:1180px!important;height:auto!important;min-height:340px}
</style>"""
    )
    page("c4.html", "Modelo C4 e system design", body)


def subcommands_of(group):
    """(sub, short-help) de cada subcomando de um grupo, lido do `--help` real."""
    out, seen, rows = help_of(group), False, []
    for line in out.splitlines():
        if line.strip().startswith("Commands:"):
            seen = True
            continue
        if seen:
            if line.strip().startswith("Options:") or not line.strip():
                if rows:
                    break
                continue
            m = line.strip().split(None, 1)
            if m and m[0] not in ("help",) and m[0][0].isalpha():
                rows.append((m[0], m[1] if len(m) > 1 else ""))
    return rows


# Tarefas comuns — o "cola e corre" no topo do cheatsheet.
CHEAT_TASKS = [
    ("Serviço web, sem root, sem daemon", "delonix container run -d --name web -p 8080:80 nginx"),
    ("Shell descartável", "delonix container run --rm -it alpine sh"),
    ("Rede própria + publicar pelo ingress", "delonix network create backend\ndelonix container run -d --net backend -p 8443:443 caddy"),
    ("Trocar uma porta a QUENTE (sem reiniciar)", "delonix container update web --publish-add 9090:80"),
    ("Firewall: só deixar entrar Postgres da SDN", "delonix ingress allow db tcp/5432 --from 10.219.0.0/16\ndelonix ingress policy db deny"),
    ("Firewall: egress da rede só p/ DNS + CIDRs", "delonix egress net backend allowlist --to 10.0.0.0/8"),
    ("Tráfego por container ao vivo (eBPF)", "sudo delonix flow --watch"),
    ("Volume de rede de um NAS (NFS)", "delonix storage create media --type nfs --server 10.0.0.5 --share /mnt/pool/media"),
    ("Segredo no cofre (não no argv)", "printf 's3nha' | delonix secret create db-pass"),
    ("Expor um container à internet pública (sem conta, sem router)",
     "delonix container run -d --name web --expose 80 nginx\ndelonix tunnel expose --provider pinggy --local-port 8080",
     "tunnel/tunnel-8080: running — https://oxipg-197-148-40-67.free.pinggy.net"),
    ("NAS partilhado por vários tenants, cada um com a sua quota",
     "delonix storage create nas --type nfs --server 10.0.0.5 --share /pool/data\n"
     "delonix sharevolume apply -f sharevolume.yaml"),
    ("microVM com cloud-init", "delonix vm create node1 --disk base.qcow2 --ssh-key @~/.ssh/id_ed25519.pub"),
    ("Cluster Kubernetes do zero", "delonix cluster kubeadm --name lab --control-plane 1 --workers 2"),
    ("Aplicar um manifesto inteiro", "delonix stack apply -f delonix-manifest.yaml"),
    ("Persistir os containers no arranque", "delonix boot enable"),
    ("Recuperar espaço (GC)", "delonix system prune"),
]


# Kinds do manifesto — cada um com um template COMPLETO e funcional (lido dos
# `examples/*.yaml`, que são a referência canónica com todos os campos + defaults).
KINDS_DOC = [
    ("Network", "network.yaml", "Uma rede de utilizador. Os containers juntam-se com <code>--net &lt;nome&gt;</code>; "
     "as VMs com <code>network:</code>. Driver <code>bridge</code> é o único a que containers se atacham hoje."),
    ("Volume", "volume.yaml", "Um volume local nomeado — os dados sobrevivem a <code>container rm</code>. Para "
     "armazenamento de REDE (NFS/SMB/WebDAV) usa antes <code>kind: Storage</code>."),
    ("Storage", "storage.yaml", "Um volume de REDE montado de um NAS (TrueNAS/Synology/Samba/Nextcloud), estilo "
     "PersistentVolume do k8s. A password vem do cofre (<code>--password-secret</code>). Montar precisa de CAP_SYS_ADMIN."),
    ("Image", "image.yaml", "Pré-puxa (ou constrói) uma imagem antes dos containers que dependem dela. Com "
     "<code>--vm</code> o mesmo Kind cobre as imagens VM douradas."),
    ("Vm", "vm.yaml", "Uma microVM declarativa (Cloud Hypervisor ou libvirt), com cloud-init por instância. É a "
     "camada que o <code>delonix cluster kubeadm</code> usa para provisionar nós."),
    ("Container", "container.yaml", "A carga do dia a dia. Só <code>image</code> é obrigatório; todos os outros campos "
     "têm default. Cobre rede, storage, recursos (cgroup v2), segredos, segurança, devices e limites."),
    ("Pod", "pod-multi.yaml", "Um pod REAL multi-container: N containers a partilhar as namespaces do pod (mesmo "
     "schema <code>spec.containers[]</code> do <code>kind: Container</code>, mas com N containers). Partilham "
     "<strong>netns</strong> (mesmo IP, <code>localhost</code> entre si), <strong>IPC</strong> e <strong>UTS</strong> "
     "(hostname). A namespace de PID (<code>shareProcessNamespace</code>) é follow-up. Gere-se com "
     "<code>delonix pod create/ls/describe/rm/logs</code>."),
    ("Ingress / Egress", "firewall.yaml", "Firewall L4 declarativo por direcção (estilo k8s NetworkPolicy). Cada "
     "documento é o estado desejado de uma direcção de um container-alvo — allowlist + default-deny, idempotente."),
    ("HTTPRoute", "httproute.yaml", "Reverse-proxy L7/HTTP embutido — routing por <code>Host</code> + prefixo de "
     "<code>path</code> para containers backend. TLS termina no proxy (self-signed ou <code>secretRef</code>); "
     "reload a quente por SIGHUP."),
    ("Tunnel", "tunnel.yaml", "Expõe UMA porta local à internet pública via pinggy/ngrok/cloudflare — sem conta, "
     "sem IP público. Junta-se ao <code>HTTPRoute</code> apontando <code>localPort</code> para onde o proxy L7 "
     "escuta: uma URL pública, routing por Host do lado de lá para vários backends."),
    ("ShareVolume", "sharevolume.yaml", "Uma fatia ISOLADA e com quota própria de um <code>Storage</code> — vários "
     "container/vm/pod partilham UM export NFS/CIFS/WebDAV sem se verem. Cada fatia é um subdirectório real do "
     "mount pai, registado como o seu próprio volume; consome-se com <code>-v &lt;nome&gt;:/destino</code>, sem "
     "nada de novo do lado do consumidor."),
]


def kinds_page():
    body = ["<h1>Kinds do manifesto</h1><p class='tagline'>Cada Kind com um template COMPLETO e funcional — "
            "todos os campos, com os defaults e um comentário. Aplica um só com "
            "<code>delonix &lt;grupo&gt; apply -f</code>, ou todos de uma vez com <code>delonix stack apply</code> "
            "(ordem por dependência: Secret → Network → Volume → Storage → ShareVolume → Image → Vm → Container → "
            "Pod → Ingress/Egress → Dependency → HTTPRoute → Tunnel).</p>"]
    body.append("<p>Semântica <em>garante-presente</em> (idempotente por nome), não um reconciliador: sem diffing, "
                "rollout nem rollback — fail-fast, o que já foi aplicado fica. Os templates abaixo são os ficheiros "
                "reais em <a href='https://github.com/angolardevops/delonix-runtime/tree/main/examples'><code>examples/</code></a>.</p>")
    for kind, fname, intro in KINDS_DOC:
        anchor = kind.split()[0].lower()
        body.append(f"<h2 id='{anchor}'>{html.escape(kind)}</h2>")
        body.append(f"<p>{intro}</p>")
        path = os.path.join(ROOT, "..", "examples", fname)
        try:
            yaml = open(path).read().strip()
        except OSError:
            yaml = f"# (exemplo em falta: examples/{fname})"
        body.append(f"<pre><code>{html.escape(yaml)}</code></pre>")
    page("kinds.html", "Kinds do manifesto", "\n".join(body))


def cheatsheet_page():
    body = ["<h1>Cheatsheet</h1><p class='tagline'>Todos os grupos de comandos e subcomandos, "
            "num só sítio. Gerado do <code>--help</code> real do binário.</p>"]
    body.append("<h2>Tarefas comuns</h2>")
    body.append(examples_html(CHEAT_TASKS))
    body.append("<h2>Todos os grupos</h2>")
    order = list(GROUPS.keys()) + ["cri"]
    for g in order:
        title = GROUPS[g]["title"] if g in GROUPS else "delonix cri"
        href = f"comandos/{g}.html" if g in GROUPS else "cri.html"
        subs = subcommands_of(g)
        head = f"<h3 id='{g}'><a href='{href}'><code>{html.escape(title)}</code></a></h3>"
        if not subs:
            tl = GROUPS[g]["tagline"] if g in GROUPS else "Serve o endpoint CRI (runtime.v1) num socket unix."
            body.append(head + f"<p>{html.escape(tl)}</p>")
            continue
        rows = "".join(
            f"<tr><td><code>{html.escape(g)} {html.escape(s)}</code></td><td>{html.escape(d)}</td></tr>"
            for s, d in subs
        )
        body.append(head + f"<table><tr><th>Comando</th><th>O que faz</th></tr>{rows}</table>")
    body.append("<h2>Global</h2><p><code>--l18n en|pt</code> — idioma da saída (EN por omissão; "
                "<code>pt</code> para pt_AO). <code>$DELONIX_ROOT</code> — raiz do estado. "
                "<code>delonix completion &lt;shell&gt;</code> — autocompletion.</p>")
    page("cheatsheet.html", "Cheatsheet", "\n".join(body))


TUTORIAL = """
<h1>Projecto completo: Delonix Temp</h1>
<p class="tagline">Do zero a um serviço na internet pública — build, run, e uma URL real, em 4
comandos. Tudo neste guia foi corrido a sério; o output é real, copiado da execução.</p>

<p>Uma API de tempo real em <a href="https://fastapi.tiangolo.com/">FastAPI</a> — consulta a
temperatura ACTUAL de qualquer cidade (via <a href="https://open-meteo.com/">Open-Meteo</a>, sem
API key) e serve uma página que actualiza sozinha a cada 30s. O objectivo é percorrer o ciclo
completo do Delonix num só projecto pequeno: <code>build</code> multi-stage → <code>container
run</code> → expor à internet com <code>tunnel</code>. Os ficheiros completos estão em
<a href="https://github.com/angolardevops/delonix-runtime/tree/main/examples/delonix-temp"><code>examples/delonix-temp/</code></a>
— <code>git clone</code> e segue os passos abaixo tal como estão.</p>

<h2>1. A app</h2>
<p>Três ficheiros. <code>main.py</code> — duas rotas: <code>/api/weather/{city}</code> (geocodifica
a cidade, pede a temperatura actual, devolve JSON) e <code>/</code> (uma página HTML que chama a
API própria via <code>fetch</code> e refaz a cada 30s):</p>
<pre><code>from fastapi import FastAPI, HTTPException
from fastapi.responses import HTMLResponse
import httpx

app = FastAPI(title="Delonix Temp")

@app.get("/api/weather/{city}")
async def weather(city: str):
    async with httpx.AsyncClient(timeout=8.0) as client:
        geo = await client.get("https://geocoding-api.open-meteo.com/v1/search",
                                params={"name": city, "count": 1})
        place = geo.json()["results"][0]
        fc = await client.get("https://api.open-meteo.com/v1/forecast", params={
            "latitude": place["latitude"], "longitude": place["longitude"],
            "current": "temperature_2m,weather_code",
        })
    current = fc.json()["current"]
    return {"city": place["name"], "country": place.get("country"),
            "temperature_c": current["temperature_2m"], "observed_at": current["time"]}

@app.get("/", response_class=HTMLResponse)
async def index():
    return PAGE  # HTML com um &lt;script&gt; que faz fetch("/api/weather/"+city) a cada 30s
</code></pre>
<p>(versão completa, com a página HTML e <code>/health</code>, no ficheiro real do repo.)</p>

<p><code>requirements.txt</code> — <code>fastapi</code>, <code>uvicorn[standard]</code>,
<code>httpx</code>, com versões fixas.</p>

<p><code>Delonixfile</code> — build <strong>multi-stage</strong>: um estágio instala as
dependências Python, o outro só copia o resultado + o código — a imagem final não carrega o
cache do pip nem ferramentas de build:</p>
<pre><code>FROM python:3.12-slim AS builder
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir --prefix=/install -r requirements.txt

FROM python:3.12-slim
WORKDIR /app
COPY --from=builder /install /usr/local
COPY main.py .
ENV PYTHONUNBUFFERED=1
CMD ["uvicorn", "main:app", "--host", "0.0.0.0", "--port", "80"]</code></pre>

<h2>2. Build</h2>
<pre><code>cd examples/delonix-temp
delonix build -t delonix-temp:1 .</code></pre>
<div class="out"><pre><code>Collecting fastapi==0.115.0 (from -r requirements.txt (line 1))
...
Successfully installed annotated-types-0.7.0 anyio-4.14.2 ... fastapi-0.115.0 ...
ef708d73f029</code></pre></div>
<p>O ID no fim (<code>ef708d73f029</code>) é a imagem. <code>delonix image ls</code> confirma o
tamanho — o estágio final, sem as ferramentas de build, fica bem mais pequeno que se fosse tudo
num único <code>FROM</code>:</p>
<pre><code>delonix image ls</code></pre>
<div class="out"><pre><code>REPOSITORY:TAG     IMAGE ID       CREATED          SIZE
delonix-temp:1     ef708d73f029   agora mesmo      157.2 MiB
python:3.12-slim   25c5b8011a34   agora mesmo       41.2 MiB</code></pre></div>

<h2>3. Correr</h2>
<pre><code>delonix container run -d --name delonix-temp -p 8080:80 delonix-temp:1
curl -s http://localhost:8080/api/weather/Luanda</code></pre>
<div class="out"><pre><code>{"city":"Luanda","country":"Angola","temperature_c":20.2,"observed_at":"2026-07-23T19:00"}</code></pre></div>
<p>Temperatura REAL, consultada ao vivo — não é um valor fixo. Os logs do container confirmam
os pedidos:</p>
<pre><code>delonix container logs delonix-temp</code></pre>
<div class="out"><pre><code>INFO:     Uvicorn running on http://0.0.0.0:80 (Press CTRL+C to quit)
INFO:     10.0.2.2:57786 - "GET /health HTTP/1.1" 200 OK
INFO:     10.0.2.2:57802 - "GET /api/weather/Luanda HTTP/1.1" 200 OK</code></pre></div>

<h2>4. Expor à internet</h2>
<p>Uma porta local não chega — o objectivo é uma URL que qualquer pessoa, em qualquer rede,
consiga abrir. É aqui que entra o <a href="comandos/tunnel.html"><code>kind: Tunnel</code></a>:</p>
<pre><code>delonix tunnel expose --name delonix-temp --provider pinggy --local-port 8080</code></pre>
<div class="out"><pre><code>tunnel/delonix-temp: running — https://lfdhz-197-148-40-67.free.pinggy.net</code></pre></div>
<p>Essa URL é REAL — foi a que este guião recebeu ao correr o comando. (A tua vai ser diferente
de cada vez: o provider grátis atribui uma nova de cada sessão.) Confirmação, de fora, sem
tocar em nada local:</p>
<pre><code>curl https://lfdhz-197-148-40-67.free.pinggy.net/api/weather/Luanda</code></pre>
<div class="out"><pre><code>{"city":"Luanda","country":"Angola","temperature_c":20.2,"observed_at":"2026-07-23T19:00"}</code></pre></div>
<p>O mesmo JSON, desta vez a chegar de fora da máquina, por um tunnel SSH até um servidor
público (pinggy) e de volta — zero configuração de router, zero IP público próprio, zero conta.
Abrir a URL num browser mostra a página <em>Delonix Temp</em> a actualizar-se sozinha.</p>

<div class="callout">
<p><b>Ir mais longe:</b> com mais de um serviço, mete o <a href="comandos/httproute.html"><code>kind:
HTTPRoute</code></a> à frente (routing por <code>Host</code>/path para vários containers) e aponta
o <code>tunnel</code> para a PORTA DO PROXY em vez de directamente ao container — uma só URL
pública, tantos backends quantos precisares. Ver <code>examples/httproute.yaml</code> +
<code>examples/tunnel.yaml</code>.</p>
</div>

<h2>Arrumar</h2>
<pre><code>delonix tunnel rm delonix-temp
delonix container rm -f delonix-temp</code></pre>

<h2>O que isto provou</h2>
<table>
<tr><th>Comando</th><th>O que validou</th></tr>
<tr><td><code>delonix build</code></td><td>build multi-stage real (2 estágios, <code>COPY --from</code>), com rede no build</td></tr>
<tr><td><code>delonix container run -p</code></td><td>NAT userspace sem root, porta publicada no host</td></tr>
<tr><td><code>delonix container logs</code></td><td>observabilidade de um serviço real a correr</td></tr>
<tr><td><code>delonix tunnel expose</code></td><td>tráfego REAL da internet pública a chegar a um container local, sem conta nem IP público</td></tr>
</table>
"""


def main():
    # Só a 1.ª linha: desde a v0.6.1 o --version é um cartão multi-linha e o
    # último token do output inteiro deixou de ser a versão.
    ver = (
        subprocess.run([BIN, "--version"], capture_output=True, text=True)
        .stdout.strip()
        .splitlines()[0]
        .split()[-1]
    )
    cards = "".join(
        f'<div class="card"><b><a href="comandos/{n}.html">{html.escape(g["title"])}</a></b>'
        f'<p>{html.escape(g["tagline"])}</p></div>'
        for n, g in GROUPS.items()
    )
    page("index.html", "Delonix Engine", INDEX.replace("{ver}", ver).replace("{cards}", cards))
    cheatsheet_page()
    kinds_page()
    page("arquitectura.html", "Arquitectura", ARCH)
    c4_page()
    page("cri.html", "CRI", CRI)
    page("comparacao.html", "Delonix vs Docker vs Podman", COMPARE)
    page("tutorial-delonix-temp.html", "Projecto completo: Delonix Temp", TUTORIAL)
    for n, g in GROUPS.items():
        group_page(n, g)
    open(os.path.join(ROOT, ".nojekyll"), "w").close()
    print(f"docs geradas (delonix {ver}) em {ROOT}")


if __name__ == "__main__":
    main()
