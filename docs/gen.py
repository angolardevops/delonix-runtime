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
        "intro": """Build sem daemon nem BuildKit: sobe um container de trabalho, corre cada
<code>RUN</code> por <code>exec</code>, aplica <code>COPY</code> no rootfs (confinado ao contexto —
path traversal é rejeitado) e empacota o resultado. Sem <code>-f</code>, procura primeiro um
<code>Delonixfile</code> no contexto e só depois um <code>Dockerfile</code> — a gramática é a mesma,
com extensões (<code>SCAN</code>, <code>CPUS</code>, <code>MEMORY</code>, <code>SECURITY</code>,
<code>HEALTHCHECK</code>). Limitação conhecida: só single-stage (multi-stage é recusado com erro claro).""",
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
    ]
    items_cmd = [(f"comandos/{g}.html", GROUPS[g]["title"]) for g in GROUPS]
    def link(href, label):
        cls = ' class="on"' if href == active else ""
        return f'<a href="{p}{href}"{cls}>{html.escape(label)}</a>'
    return f"""<nav class="side">
<div class="brand"><span class="dot">▲</span> Delonix Runtime</div>
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
<title>{html.escape(title)} · Delonix Runtime</title>
<style>{CSS}</style>
</head>
<body>
<div class="layout">
{sidebar(path, depth)}
<main>
{body}
<footer>Delonix Runtime · Apache-2.0 · <a href="https://github.com/angolardevops/delonix-runtime">angolardevops/delonix-runtime</a>
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
    parts = []
    for cap, cmd in exs:
        cap_html = f'<div class="cap">{html.escape(cap)}</div>' if cap else ""
        parts.append(f'<div class="ex">{cap_html}<pre><code>{html.escape(cmd)}</code></pre></div>')
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
<h1>Delonix Runtime <span class="pill">v{ver}</span></h1>
<p class="tagline"><strong>Engine</strong> de containers e microVMs <strong>daemonless</strong>,
<strong>rootless-first</strong>, kernel-native, em Rust — com CRI próprio para Kubernetes.
<em>O engine open-source que alimenta o Delonix.</em></p>

<p>Não é um <em>runtime</em> OCI de baixo nível (isso é o <code>runc</code>/<code>crun</code>): é um
engine COMPLETO de containers <strong>e</strong> VMs — build, run, rede, firewall, storage e
bootstrap de clusters Kubernetes, tudo num só binário. É a camada aberta (Apache-2.0) sobre a qual
assenta a plataforma <strong>Delonix</strong>.</p>

<p>O Delonix Runtime faz o trabalho do Docker/Podman sem daemon residente: cada comando é um
processo efémero que fala directamente com o kernel (namespaces, cgroups v2, pivot_root),
guarda estado em ficheiros e desaparece. Em rootless, a rede é servida por um único par
holder-netns + slirp4netns partilhado — não um slirp por container — com DNAT nft para o
publish de portas, o que permite <em>trocar portas e volumes a quente</em>, sem reiniciar o
container.</p>

<h2>Instalar</h2>
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
<p>Rootless por omissão; seccomp e drop de capabilities fora de <code>--privileged</code>; pull com
verificação de digest (incluindo artefactos VM OCI); COPY do build confinado ao contexto; inputs de
manifesto validados por whitelist antes de chegarem a qualquer shell remoto. O projecto passou uma
auditoria ofensiva dedicada (3 revisões adversariais em paralelo) — os 4 achados críticos foram
corrigidos com testes que replicam cada exploit.</p>
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
    ("Ingress / Egress", "firewall.yaml", "Firewall L4 declarativo por direcção (estilo k8s NetworkPolicy). Cada "
     "documento é o estado desejado de uma direcção de um container-alvo — allowlist + default-deny, idempotente."),
]


def kinds_page():
    body = ["<h1>Kinds do manifesto</h1><p class='tagline'>Cada Kind com um template COMPLETO e funcional — "
            "todos os campos, com os defaults e um comentário. Aplica um só com "
            "<code>delonix &lt;grupo&gt; apply -f</code>, ou todos de uma vez com <code>delonix stack apply</code> "
            "(ordem por dependência: Network → Volume → Storage → Image → Vm → Container → Ingress → Egress).</p>"]
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


def main():
    ver = subprocess.run([BIN, "--version"], capture_output=True, text=True).stdout.strip().split()[-1]
    cards = "".join(
        f'<div class="card"><b><a href="comandos/{n}.html">{html.escape(g["title"])}</a></b>'
        f'<p>{html.escape(g["tagline"])}</p></div>'
        for n, g in GROUPS.items()
    )
    page("index.html", "Delonix Runtime", INDEX.replace("{ver}", ver).replace("{cards}", cards))
    cheatsheet_page()
    kinds_page()
    page("arquitectura.html", "Arquitectura", ARCH)
    c4_page()
    page("cri.html", "CRI", CRI)
    for n, g in GROUPS.items():
        group_page(n, g)
    open(os.path.join(ROOT, ".nojekyll"), "w").close()
    print(f"docs geradas (delonix {ver}) em {ROOT}")


if __name__ == "__main__":
    main()
