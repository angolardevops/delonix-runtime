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
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "..", "target", "release", "delonix")


# v0.30.0 nested some former top-level groups under `net`/`serve`/`cluster`
# (deep grouping, no aliases kept) — this maps a GROUPS dict key (the stable
# doc page name, unchanged) to the real argv prefix needed to reach its
# `--help` today. Anything absent here is still a real top-level command.
GROUP_PATH = {
    "netns": ("net", "netns"),
    "flow": ("net", "flow"),
    "ingress": ("net", "ingress"),
    "egress": ("net", "egress"),
    "httproute": ("net", "httproute"),
    "tunnel": ("net", "tunnel"),
    "boot": ("net", "boot"),
    "cri": ("serve", "cri"),
    "api": ("serve", "api"),
    "docker-api": ("serve", "docker-api"),
    "kube": ("cluster", "kube"),
}


def group_argv(name):
    return GROUP_PATH.get(name, (name,))


def help_of(*args):
    out = subprocess.run([BIN, *args, "--help"], capture_output=True, text=True)
    return (out.stdout or out.stderr).strip()


def split_help_intro(help_text):
    """Separa o parágrafo `about` (clap) — se existir — do resto do `--help`.

    O `about` é sempre o texto ANTES da primeira linha `Usage:`. Promovê-lo a
    parágrafo próprio evita mostrá-lo duas vezes (uma como prosa, outra
    dentro do bloco de código) e dá a cada secção uma introdução real, feita
    do texto que já existe no binário — nunca inventada.
    """
    idx = help_text.find("Usage:")
    if idx <= 0:
        return None, help_text
    intro = help_text[:idx].strip()
    return (intro or None), help_text[idx:]


def bi(tag, pt_html, en_html, cls=""):
    """Emits a PT block then an EN block (same tag), tagged for the language
    toggle. Both stay in the DOM — CSS shows only the active one — so a
    reader never sees blank content while a translation is still pending
    for that particular piece of text."""
    c = (cls + " ").lstrip()
    return (
        f"<{tag} class='{c}lang-pt'>{pt_html}</{tag}>"
        f"<{tag} class='{c}lang-en'>{en_html}</{tag}>"
    )


def render_prose(text):
    """Escapa HTML e traduz as convenções markdown-ish do `--help` (clap
    `about`/`long_about`) para HTML real: `` `code` `` e `**bold**`."""
    out = html.escape(text)
    out = re.sub(r"`([^`]+)`", r"<code>\1</code>", out)
    out = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", out)
    return out


# Mapeia cada grupo de comandos (chave de `GROUPS`) para o ficheiro Rust que o
# implementa, para a página de referência poder linkar para a implementação
# real — quem quiser contribuir vê imediatamente onde mexer.
SOURCE_FILES = {
    "container": "container.rs",
    "workload": "workload.rs",
    "pod": "pod.rs",
    "image": "image.rs",
    "build": "build.rs",
    "vm": "vm.rs",
    "volumes": "volume.rs",
    "network": "network.rs",
    "stack": "stack.rs",
    "compose": "compose.rs",
    "cluster": "cluster.rs",
    "secret": "secret.rs",
    "storage": "storage.rs",
    "sharevolume": "sharevolume.rs",
    "ingress": "firewall.rs",
    "egress": "firewall.rs",
    "httproute": "httproute.rs",
    "tunnel": "tunnel.rs",
    "flow": "flow.rs",
    "boot": "boot.rs",
    "system": "system.rs",
    "dash": "dash.rs",
    "docker-api": "dockerapi.rs",
    "kube": "kube.rs",
    "netns": "netns.rs",
    "completion": "complete.rs",
}
SOURCE_BASE_URL = (
    "https://github.com/angolardevops/delonix-runtime/blob/main/"
    "crates/delonix-runtime-bin/src/cmd/"
)


def source_link_html(name):
    fname = SOURCE_FILES.get(name)
    if not fname:
        return ""
    return (
        "<p class='src-link'>📄 Implementação real em Rust: "
        f"<a href=\"{SOURCE_BASE_URL}{fname}\"><code>cmd/{fname}</code></a></p>"
    )


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
            "dash": {"examples": [
                ('Dashboard só dos containers',
                 'delonix container dash')]},
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
            "init": {"examples": [("Scaffold de um projecto completo, pronto a usar", "delonix container init myapp && cd myapp")]},
            "kill": {"examples": [("Sinal arbitrário, sem forçar `Stopped`", "delonix container kill -s USR1 web")],
                     "notes": """<p>Ao contrário de <code>stop</code>, não espera nem força o estado —
o resultado real (ex.: <code>Crashed</code> para um <code>KILL</code>) só se confirma na
observação seguinte.</p>"""},
            "wait": {"examples": [("Bloqueia até sair, imprime o exit code", "delonix container wait web")],
                     "notes": """<p>O exit code real só é garantido quando um supervisor
<code>--restart</code> é o pai real do processo — um container <code>-d</code> simples sem
supervisor mostra <code>Crashed</code>/137, limite arquitectural conhecido (o motor não é o pai
real desse processo).</p>"""},
            "restart": {"examples": [("Pára e arranca de novo, mesma configuração", "delonix container restart web")]},
            "rename": {"examples": [("", "delonix container rename web frontend")]},
            "port": {"examples": [("Portas publicadas deste container", "delonix container port web")]},
            "pause": {"examples": [("Suspende os processos (cgroup v2 freezer)", "delonix container pause web")]},
            "unpause": {"examples": [("Resume um container suspenso", "delonix container unpause web")]},
            "commit": {"examples": [
                ("Cria uma imagem a partir do rootfs actual do container",
                 "delonix container commit web minha-app:debug"),
            ]},
            "ssh": {"examples": [("Atalho para `exec -t` — tenta bash, cai para sh", "delonix container ssh web")]},
            "healthcheck": {"examples": [
                ("Corre o HEALTHCHECK da imagem, exit 1 se unhealthy (usável em CI)",
                 "delonix container healthcheck web"),
            ]},
            "top": {"examples": [("Processos a correr dentro do container", "delonix container top web")]},
            "diff": {"examples": [("Ficheiros alterados relativos à imagem (A/D)", "delonix container diff web")]},
            "cp": {"examples": [
                ("Do container para o host", "delonix container cp web:/etc/nginx.conf ."),
                ("Do host para o container", "delonix container cp ./nginx.conf web:/etc/nginx.conf"),
            ]},
            "describe": {"examples": [
                ("Detalhe estilo `kubectl describe` (para humanos; `inspect` é para scripts)",
                 "delonix container describe web"),
            ]},
            "update": {"examples": [
                ("Troca uma porta a QUENTE, sem reiniciar", "delonix container update web --publish-add 9090:80"),
                ("Liga a uma rede nova + limite de banda", "delonix container update web --net-connect backend --net-rate 10mbit"),
                ("Sobe o limite de memória/CPU a QUENTE, sem reiniciar", "delonix container update web --memory 512M --cpus 2"),
            ], "notes": """<p>Reconfigura portas, volumes, redes, limite de banda e limites de
memória/CPU de um container <strong>a correr</strong>, sem o parar — o PID não muda. Remoções
correm antes das adições, para <code>--publish-rm 8080 --publish-add 8080:9000</code> funcionar
num só comando. <code>--memory</code>/<code>--cpus</code> reescrevem o cgroup real de imediato
(<code>memory.max</code>/<code>cpu.max</code>) — nada de esperar por um <code>restart</code>.</p>"""},
            "attach": {"examples": [("Volta a ligar ao stream de output de um container detached", "delonix container attach web")],
                       "notes": """<p>Deliberadamente <strong>só output</strong> — ao contrário do
<code>docker attach</code>, não há stdin ao vivo para um container já iniciado em detached (sem
shim persistente por-container). <code>-i</code>/<code>--stdin</code> é recusado com um erro claro
a apontar para <code>exec -it</code>.</p>"""},
        },
    },
    "workload": {
        "title": "delonix workload",
        "tagline": "Camada unificada sobre containers E VMs: ls, describe, stop, rm (ADR-0002).",
        "intro": """O grupo <code>workload</code> é o lado imperativo do Runtime Abstraction Layer:
um trait <code>ComputeDriver</code> despacha por nome para o motor de containers ou de VMs, para
geres os dois como uma coisa só. A <strong>criação é declarativa</strong> — <code>kind: Workload</code>
num manifesto (<code>spec.type: container|vm|pod|microvm</code>) baixa para o Kind respectivo no
<code>manifest::load</code>; ver <a href="../kinds.html">Kinds</a> e <code>examples/workload.yaml</code>.""",
        "subs": {
            "ls": {"examples": [
                ("Containers E VMs numa só tabela", "delonix workload ls"),
                ("Saída estruturada para automação (chaves estáveis, independentes de língua)",
                 "delonix workload ls -o json | jq '.[] | select(.type==\"vm\")'"),
            ], "notes": """<p><strong>Routing por nome exacto</strong>, fail-closed: um nome inexistente dá
<code>no such workload</code>; um container E uma vm com o mesmo nome dão <code>ambiguous</code> (aponta
para o comando específico, nunca adivinha).</p>"""},
            "describe": {"examples": [("Detalhe do workload, com routing automático para o motor certo",
                                       "delonix workload describe web")]},
            "stop": {"examples": [("Parar por nome, seja container ou vm", "delonix workload stop web")]},
            "rm": {"examples": [("Remover por nome", "delonix workload rm -f web")]},
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
            "init": {"examples": [
                ('Scaffold de um VMfile (equivalente a vm init --vmfile)',
                 'delonix image --vm init minha-base')]},
            "vm": {"examples": [
                ('O mesmo grupo de imagens VM, por outro caminho',
                 'delonix image vm ls')]},
            "logout": {"examples": [
                ('Esquecer as credenciais desse registo',
                 'delonix image logout ghcr.io')]},
            "login": {"examples": [
                ('Autenticar num registo (a password vem do stdin, fora do histórico)',
                 'printf \'%s\' "$GHCR_TOKEN" | delonix image login ghcr.io --username aminhaorg')]},
            "load": {"examples": [
                ('Importar esse tar do outro lado',
                 'delonix image load --input app-dev.tar')]},
            "save": {"examples": [
                ('Exportar para um tar (para levar para uma máquina sem rede)',
                 'delonix image save app:dev --output app-dev.tar')]},
            "scan": {"examples": [
                ('Procurar vulnerabilidades conhecidas numa imagem',
                 'delonix image scan nginx:alpine'),
                ('Varrer todas as imagens locais',
                 'delonix image scan')]},
            "verify": {"examples": [
                ('Confirmar a assinatura contra uma chave pública',
                 'delonix image verify ghcr.io/aminhaorg/app:1.0 chave.pem')]},
            "history": {"examples": [
                ('Que instrução criou cada camada',
                 'delonix image history app:dev')]},
            "tag": {"examples": [
                ('Dar um segundo nome à mesma imagem (não copia nada)',
                 'delonix image tag app:dev ghcr.io/aminhaorg/app:1.0')]},
            "describe": {"examples": [
                ('Camadas, config e digest de uma imagem',
                 'delonix image describe nginx:alpine')]},
            "ls-remote": {"examples": [
                ('Tags publicadas num repositório, sem puxar nada',
                 'delonix image ls-remote ghcr.io/aminhaorg/app')]},
            "dash": {"examples": [
                ('Dashboard só das imagens',
                 'delonix image dash')]},
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
            "snapshots": {"examples": [
                ('Listar os checkpoints de uma VM',
                 'delonix vm snapshots dev')]},
            "restore": {"examples": [
                ('Voltar ao checkpoint',
                 'delonix vm restore dev antes-do-upgrade')]},
            "snapshot": {"examples": [
                ('Checkpoint de sistema (memória + disco) de uma VM A CORRER',
                 'delonix vm snapshot dev antes-do-upgrade')]},
            "restart": {"examples": [
                ('Reinício forçado (pára e volta a arrancar)',
                 'delonix vm restart dev')]},
            "start": {"examples": [
                ('Voltar a arrancar uma VM parada, sem repetir as flags do create',
                 'delonix vm start dev')]},
            "describe": {"examples": [
                ('Tudo sobre uma VM, estilo kubectl describe',
                 'delonix vm describe dev')]},
            "unbridge": {"examples": [
                ('Fechar a ponte VM↔container',
                 'sudo delonix vm unbridge minha-rede')]},
            "bridge": {"examples": [
                ('Ver o plano SEM aplicar (o default é dry-run)',
                 'delonix vm bridge minha-rede'),
                ('Aplicar mesmo — precisa de root, é a excepção deliberada ao rootless',
                 'sudo delonix vm bridge minha-rede --apply')]},
            "reach": {"examples": [
                ('Que portas de container é que as VMs conseguem alcançar',
                 'delonix vm reach')]},
            "vnc": {"examples": [
                ('Abrir o ecrã gráfico da VM',
                 'delonix vm vnc dev')]},
            "console": {"examples": [
                ('Consola série (voltar ao host: Ctrl+])',
                 'delonix vm console dev')]},
            "push": {"examples": [
                ('Publicar a tua imagem como artefacto OCI',
                 'printf \'%s\' "$GHCR_TOKEN" | delonix image login ghcr.io --username aminhaorg\ndelonix vm push minha-base:1.0 ghcr.io/aminhaorg/minha-base:1.0')]},
            "ls-remote": {"examples": [
                ('Que versões existem publicadas, antes de puxar',
                 'delonix vm ls-remote'),
                ('As tags de um repositório teu',
                 'delonix vm ls-remote ghcr.io/aminhaorg/base')]},
            "pull": {"examples": [
                ('A golden oficial com Kubernetes (sem argumento)',
                 'delonix vm pull'),
                ('A golden SEM Kubernetes — só o motor, pronta a rootless',
                 'delonix vm pull --no-k8s'),
                ('De um registo teu, com nome local próprio',
                 'delonix vm pull ghcr.io/aminhaorg/base:24.04 --name base:24.04')]},
            "build": {"examples": [
                ('Construir a partir do VMfile do directório actual',
                 'delonix vm build -t minha-base:1.0 .'),
                ('VMfile noutro caminho, sem compressão (build mais rápido, imagem maior)',
                 'delonix vm build -t minha-base:dev -f receitas/VMfile --no-compress .'),
                ('Com rede no convidado — precisa disto para `apt-get install` num RUN '
                 '(o build deixa de ser reproduzível: o resultado passa a depender do dia)',
                 'delonix vm build --network -t minha-base:1.0 .')]},
            "init": {"examples": [
                ('Projecto com manifesto, pronto a correr',
                 'delonix vm init --name lab'),
                ('Scaffold de um VMfile para CONSTRUIR a tua imagem',
                 'delonix vm init --vmfile --name minha-base')]},
            "dash": {"examples": [
                ('Dashboard só das VMs (htop-style; `q` sai)',
                 'delonix vm dash'),
                ('Snapshot para um script ou para o Grafana',
                 "delonix vm dash --json | jq '.tiles'")]},
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
            "snapshot": {"examples": [
                ('Tirar e listar snapshots de um volume',
                 'delonix volumes snapshot create dados antes-da-migracao\ndelonix volumes snapshot ls dados')]},
            "describe": {"examples": [
                ('Detalhe de um volume (uso, quota, montagens)',
                 'delonix volumes describe dados')]},
            "create": {"examples": [("Com quota e driver nfs disponíveis", "delonix volumes create dados --quota 10G")]},
            "ls": {"examples": [("", "delonix volumes ls")]},
            "inspect": {"examples": [("", "delonix volumes inspect dados")]},
            "rm": {"examples": [("", "delonix volumes rm dados")]},
            "apply": {"examples": [("", "delonix volumes apply -f delonix-manifest.yaml")]},
        },
    },
    "network": {
        "title": "delonix network",
        "tagline": "Redes de utilizador: create, ls, inspect, rm, apply — bridge e overlay realizados fisicamente.",
        "intro": """Para os drivers <code>bridge</code> e <code>overlay</code>, o <code>create</code>
orquestra o registo declarativo (<code>NetworkStore</code>) E o plano físico rootless em conjunto —
<code>bridge</code> dentro do netns do holder; <code>overlay</code> sobe um uplink VXLAN cifrado com
WireGuard entre nós (device <code>dlxvx&lt;vni&gt;</code> a masterizar a bridge, FDB semeado com os
pares), tudo realizável sem privilégio de host. <code>macvlan</code>/<code>ipvlan</code> ficam só
registados no store — o <code>create</code> AVISA alto que a rede não foi realizada fisicamente
(precisam de <code>CAP_NET_ADMIN</code> na init-netns do host, fora do modelo rootless).""",
        "subs": {
            "describe": {"examples": [
                ('Detalhe de uma rede, estilo kubectl',
                 'delonix network describe minha-rede')]},
            "node": {"examples": [
                ('Gerir nós de uma rede overlay entre máquinas',
                 'delonix network node ls')]},
            "dash": {"examples": [
                ('Dashboard só das redes',
                 'delonix network dash')]},
            "create": {"examples": [
                ("Rede bridge para um grupo de serviços", "delonix network create backend"),
                ("Overlay cifrado entre nós (VXLAN + WireGuard)", "delonix network create mesh --driver overlay --vni 42 --peer 10.0.0.2"),
            ]},
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
            "validate": {"examples": [
                ('Validar o manifesto SEM aplicar nada',
                 'delonix stack validate -f delonix-manifest.yaml')]},
            "describe": {"examples": [
                ('Estado recurso a recurso, confrontado com o manifesto',
                 'delonix stack describe')]},
            "ls": {"examples": [
                ('Que recursos do manifesto existem de facto neste host',
                 'delonix stack ls')]},
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
    "compose": {
        "title": "delonix compose",
        "tagline": "Suporte NATIVO a docker-compose.yml (Compose Spec v2.x) — sem Docker, sem shim, direto para o motor.",
        "intro": """Um tradutor de esquema estrangeiro, da mesma família do <code>kind: Pod</code> (k8s) e da
API Docker: parser tipado à mão (sem dependência nova), traduzido directamente para o motor —
containers reaproveitando o mesmo caminho do <code>container run</code>, redes/volumes reaproveitando
<code>network</code>/<code>volume apply</code> verbatim (mesma idempotência, mesmo hardening de input).
<code>depends_on</code> respeita as 3 condições reais do Compose Spec
(<code>service_started</code>/<code>service_healthy</code>/<code>service_completed_successfully</code>)
via ordenação topológica do grafo de serviços — um ciclo dá erro claro, nunca uma ordem arbitrária — e
espera pelo healthcheck real (inline do serviço ou o da própria imagem). O projecto
(<code>compose down/ps/logs</code>) é uma label nos containers; redes/volumes usam nomeação
determinística (<code>&lt;projecto&gt;_&lt;nome&gt;</code>) — sem registo próprio, a mesma filosofia do
<code>stack describe</code>.""",
        "subs": {
            "up": {"examples": [
                ("Sobe tudo (build, rede, volumes, containers, por ordem de `depends_on`)", "delonix compose up"),
                ("Ficheiro/projecto explícitos", "delonix compose up -f infra/docker-compose.yml -p minhaapp"),
                ("Só valida e mostra o plano, sem criar nada", "delonix compose up --dry-run"),
            ]},
            "ps": {"examples": [("Containers deste projecto", "delonix compose ps")]},
            "logs": {"examples": [
                ("Logs de um serviço", "delonix compose logs db"),
                ("Todos os serviços, um a seguir ao outro", "delonix compose logs"),
            ]},
            "config": {"examples": [("Valida e imprime o projecto resolvido (equivalente a `docker compose config`)", "delonix compose config")]},
            "down": {"examples": [
                ("Remove os containers deste projecto", "delonix compose down"),
                ("Remove também os volumes NOMEADOS (nunca os `external: true`)", "delonix compose down -v"),
            ]},
        },
        "extra": """<h3>Por fazer, documentado (nunca em silêncio)</h3>
<p><code>profiles</code>/<code>extends</code>/<code>configs</code>/<code>secrets</code> top-level (usa
<code>kind: Secret</code> em vez disso) / multi-ficheiro (<code>-f a -f b</code>/<code>include:</code>),
<code>build.target</code> (selecção de estágio), <code>deploy.replicas != 1</code>,
<code>networks.*.ipv4_address</code> fixo, e volumes anónimos (sem <code>source</code> explícito —
precisa de semântica própria de limpeza, ainda por desenhar). <code>working_dir:</code> É aplicado
(via <code>container run -w/--workdir</code>, novo) e uma porta sem host explícito (<code>ports:
["80"]</code>) GANHA uma porta livre real do host, em vez de recusar.</p>""",
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
            "kube": {"examples": [
                ('Gerar manifestos Kubernetes a partir de um recurso Delonix',
                 'delonix cluster kube generate')]},
            "load": {"examples": [
                ('Levar uma imagem local para dentro dos nós (o kind load, sem registo)',
                 'delonix build -t app:dev .\ndelonix cluster load app:dev --name lab')]},
            "delete": {"examples": [
                ('Apagar o cluster e os seus nós',
                 'delonix cluster delete --name lab')]},
            "ls": {"examples": [
                ('Que clusters existem neste host',
                 'delonix cluster ls')]},
            "create": {"examples": [
                ('Cluster local em modo kind (containers como nós, sem Docker)',
                 'delonix cluster create --name lab'),
                ('Com workers',
                 'delonix cluster create --name lab --workers 2')]},
            "init": {"examples": [
                ('Scaffold de um cloud.yaml para cluster apply',
                 'delonix cluster init ./meu-cluster')]},
            "apply": {"examples": [
                ("Bootstrap num manifesto `kind: Cluster`", "delonix cluster apply -f cloud.yaml"),
            ], "notes": """<p>Todas as entradas do manifesto que chegam a comandos remotos
(<code>controlPlaneEndpoint</code>, subnets, versão) passam por validação estrita antes de qualquer
interpolação — a injecção de comandos via manifesto foi um dos CRÍTICOS encontrados e fechados na
auditoria ofensiva do projecto, com testes a replicar o exploit.</p>"""},
            "kubeadm": {"examples": [
                ("Do zero: 1 control-plane + 2 workers", "delonix cluster kubeadm --name lab --control-plane 1 --workers 2"),
                ("HA: 2 control-planes + 3 workers (HAProxy automático)", "delonix cluster kubeadm --name lab --control-plane 2 --workers 3"),
                ("Etcd externo dedicado (3 VMs extra, quórum ímpar)", "delonix cluster kubeadm --name lab --control-plane 2 --etcd-cluster 3"),
            ], "notes": """<p><code>--control-plane &gt; 1</code> provisiona automaticamente uma VM
extra a correr HAProxy (L4, passthrough — a TLS do apiserver termina sempre no control-plane real)
à frente da porta 6443 de cada control-plane, e usa-a como <code>controlPlaneEndpoint</code> — sem
flag nova, dispara sozinho a partir do número de control-planes pedido. <code>--name</code> é
opcional (gera um nome livre no mesmo padrão dos containers); sem <code>--vm-image</code>, resolve
a única imagem VM dourada local ou descarrega-a do repositório oficial automaticamente. Progresso
por etapa, estilo <code>kind create cluster</code> (cada etapa fecha com ✓/✗), degrada para uma
linha por etapa sem TTY (pipes/CI).</p>"""},
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
            "apply": {"examples": [
                ('Criar segredos a partir de um manifesto kind: Secret',
                 'delonix secret apply -f segredos.yaml')]},
            "rm": {"examples": [
                ('Apagar o segredo inteiro',
                 'delonix secret rm db-creds')]},
            "unset": {"examples": [
                ('Remover UMA chave (sem apagar o segredo)',
                 'delonix secret unset db-creds password')]},
            "set": {"examples": [
                ('Acrescentar/actualizar chaves de um segredo existente',
                 'delonix secret set db-creds user=admin password=s3cr3t')]},
            "create": {"examples": [("Criar um segredo (valor via stdin, não no argv)", "printf 'password=s3nha' | delonix secret create db-pass --from-env-file -")]},
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
            "apply": {"examples": [
                ('Declarar o NAS num manifesto kind: Storage',
                 'delonix storage apply -f storage.yaml')]},
            "inspect": {"examples": [
                ('JSON do storage (para scripts)',
                 'delonix storage inspect nas-fotos')]},
            "dash": {"examples": [
                ('Dashboard só do armazenamento de rede',
                 'delonix storage dash')]},
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
        "title": "delonix net ingress",
        "tagline": "Firewall de ENTRADA (regras L4 + publishes DNAT) de um container na SDN.",
        "intro": """Metade da superfície unificada de firewall (a outra é <code>egress</code>). Edita a
única fonte de verdade — o <code>ContainerFw</code> por container, aplicado como regras nft na chain de
ingress. <code>ingress</code> governa a ENTRADA: regras allow/deny por <code>[proto/]porta</code> e CIDR,
a política por omissão, e os <em>publishes</em> DNAT. Só actua em containers numa rede custom (têm IP na
<code>delonix0</code>); <code>--net host</code> é recusado.""",
        "subs": {
            "clear": {"examples": [
                ('Limpar a firewall inteira desse container',
                 'delonix net ingress clear web')]},
            "rm": {"examples": [
                ('Tirar UMA regra, sem limpar as outras',
                 'delonix net ingress rm web tcp/80')]},
            "unpublish": {"examples": [
                ('Deixar de publicar uma porta',
                 'delonix net ingress unpublish 8080')]},
            "allow": {"examples": [("Deixar entrar Postgres só da própria SDN", "delonix net ingress allow db tcp/5432 --from 10.219.0.0/16")]},
            "deny": {"examples": [("Bloquear uma porta específica", "delonix net ingress deny web tcp/22")]},
            "policy": {"examples": [("Default-deny (allowlist)", "delonix net ingress policy db deny")]},
            "publish": {"examples": [("Publicar uma porta pelo ingress (DNAT)", "delonix net ingress publish web 8080:80")]},
            "ls": {"examples": [("Ver regras + publishes", "delonix net ingress ls db")]},
        },
    },
    "egress": {
        "title": "delonix net egress",
        "tagline": "Firewall de SAÍDA (regras L4 + política de egress→Internet por-rede).",
        "intro": """A outra metade do firewall. Governa a SAÍDA de um container (regras allow/deny + política
por omissão) e, ao nível da REDE, a política de egress para a Internet: <code>allow</code>/<code>deny</code>,
ou <code>allowlist</code> (nega tudo excepto DNS e os CIDRs dados). Tudo sobre o mesmo <code>ContainerFw</code>
/nft do <code>ingress</code>.""",
        "subs": {
            "clear": {"examples": [
                ('Limpar as regras de saída desse container',
                 'delonix net egress clear web')]},
            "rm": {"examples": [
                ('Tirar UMA regra de saída',
                 'delonix net egress rm web tcp/443')]},
            "deny": {"examples": [
                ('Bloquear a saída para uma rede',
                 'delonix net egress deny web --to 10.0.0.0/8')]},
            "allow": {"examples": [("Só deixar sair HTTPS", "delonix net egress allow app tcp/443 --to 0.0.0.0/0")]},
            "policy": {"examples": [("Default-deny de saída", "delonix net egress policy app deny")]},
            "net": {"examples": [("Egress de uma rede em allowlist (só DNS + estes CIDRs)", "delonix net egress net backend allowlist --to 10.0.0.0/8,1.1.1.1/32")]},
            "host": {"examples": [
                ("Só deixar sair para o GitHub (e *.github.com), aprendido do DNS",
                 "delonix net egress host backend github.com"),
            ], "notes": """<p>O que o nft/CIDR não faz: allowlist por <strong>hostname</strong>. O resolver DNS
interno do ingress passa a snoopar os A-records das respostas e injecta-os num <code>set</code> nft
por-rede (com timeout = expira com o TTL); o egress aceita esse set + DNS e dropa o resto. 100%
rootless (sem eBPF) — a FQDN-policy do Cilium, via nftables. Repetível para vários hostnames.</p>"""},
            "show": {"examples": [("Ver a política de egress de uma rede + os IPs FQDN aprendidos ao vivo", "delonix net egress show backend")]},
            "ls": {"examples": [("", "delonix net egress ls app")]},
        },
    },
    "httproute": {
        "title": "delonix net httproute",
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
                 "delonix net httproute apply -f delonix-manifest.yaml"),
            ]},
            "ls": {"examples": [
                ("Estado do proxy + rotas activas", "delonix net httproute ls"),
            ]},
            "rm": {"examples": [
                ("Parar o proxy e despublicar as portas", "delonix net httproute rm"),
            ]},
        },
    },
    "tunnel": {
        "title": "delonix net tunnel",
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
            "apply": {"examples": [
                ('Declarar o túnel num manifesto kind: Tunnel',
                 'delonix net tunnel apply -f tunel.yaml')]},
            "expose": {"examples": [
                ("Expor uma porta local sem escrever manifesto (pinggy, grátis, efémero)",
                 "delonix net tunnel expose --name demo --provider pinggy --local-port 8080",
                 "tunnel/demo: running — https://oxipg-197-148-40-67.free.pinggy.net"),
            ], "notes": """<p>Validado ao vivo nesta mesma sessão: tráfego HTTPS real da internet
chegou a um servidor local através do tunnel (HTTP 200) usando exactamente este comando.</p>"""},
            "ls": {"examples": [
                ("Listar túneis (estado + URL pública)", "delonix net tunnel ls",
                 "NAME    PROVIDER   LOCAL PORT   PUBLIC URL                                    STATUS    UPTIME\n"
                 "test1   pinggy          18234   https://oxipg-197-148-40-67.free.pinggy.net   Running   Up 34 seconds"),
            ]},
            "describe": {"examples": [
                ("Detalhe de um túnel", "delonix net tunnel describe demo"),
            ]},
            "rm": {"examples": [
                ("Parar e remover (mata o processo agente a sério)", "delonix net tunnel rm demo",
                 "tunnel/demo: removed"),
            ]},
        },
    },
    "flow": {
        "title": "delonix net flow",
        "tagline": "Tráfego por-container ao vivo — datapath eBPF (degrada para contadores veth).",
        "intro": """Telemetria de rede por container. Quando corre com privilégio (CAP_BPF/root), attacha
dois classificadores tc/clsact em eBPF às veths da SDN, que contam bytes/pacotes por IP num BPF map
partilhado — <strong>sem nunca fazer drop</strong> (o nft continua o único enforcer). Sem privilégio
(o caso rootless comum) diz-o e cai nos contadores veth, que sempre funcionam. <code>--watch</code>
redesenha a cada 2s.""",
        "subs": {},
        "examples": [
            ("Uma amostra", "sudo delonix net flow"),
            ("Monitorização contínua", "sudo delonix net flow --watch"),
        ],
    },
    "boot": {
        "title": "delonix net boot",
        "tagline": "Persistência no arranque: units systemd para os containers voltarem a subir no boot.",
        "intro": """<code>boot enable</code> gera uma unit systemd por container em execução (rootless →
user units + <code>loginctl enable-linger</code>; root → system units), com <code>ExecStart</code>
=<code>container start</code>. Assim os containers voltam a subir quando o host arranca, sem daemon.""",
        "subs": {
            "enable": {"examples": [("Persistir os que correm agora", "delonix net boot enable")]},
            "status": {"examples": [("Ver o que está instalado", "delonix net boot status")]},
            "disable": {"examples": [("Remover as units de boot", "delonix net boot disable")]},
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
            "thermal": {"examples": [
                ('Temperatura e throttling do host',
                 'delonix system thermal')]},
            "monitor": {"examples": [
                ('Acompanhar os eventos ao vivo',
                 'delonix system monitor')]},
            "virt": {"examples": [
                ('O que o host oferece de virtualização, e o que há para afinar',
                 'delonix system virt'),
                ('Aplicar a afinação recomendada',
                 'sudo delonix system virt --tune')]},
            "setup": {"examples": [
                ('Diagnosticar a delegação de cgroup — porque é que -m/--cpus não pegam',
                 'delonix system setup'),
                ('A 1.ª correcção: um scope delegado (sem root, sem reboot, vale já)',
                 'systemd-run --user --scope -p Delegate=yes -- delonix system setup'),
                ('Só se a de cima ainda disser que falta o `cpu` (precisa de root, sobrevive ao reboot)',
                 'sudo delonix system setup --delegate')]},
            "events": {"examples": [
                ('Ver o que o motor andou a fazer',
                 'delonix system events -n 20'),
                ('Seguir em tempo real (Ctrl-C sai)',
                 'delonix system events --follow')]},
            "prune": {"examples": [("Recuperar espaço (GC)", "delonix system prune")]},
            "df": {"examples": [("Uso de disco", "delonix system df")]},
            "info": {"examples": [("", "delonix system info")]},
        },
    },
    "dash": {
        "title": "delonix dash",
        "tagline": "Dashboard de resumo/KPIs (TUI estilo htop) — RAM/rede/disco, uptime por-container, JSON e Prometheus.",
        "intro": """Vista viva do estado do runtime — containers, VMs, imagens, redes, storage — num
só ecrã, sem precisar de correr <code>ls</code> em 5 grupos diferentes. Cada grupo também tem o
seu próprio (<code>container dash</code>, <code>vm dash</code>, ...); este é o agregado global.
KPIs dinâmicos: memória do slice cgroup, tráfego rx/tx acumulado por-container (com contagem
explícita de containers <code>--net host/none</code> não medidos, nunca somados como zero em
silêncio), uso de disco por área (imagens/volumes/VM-images/containers), e uptime real por
container (coluna <code>UP</code>, do <code>pid_starttime</code>). A tecla <code>m</code> alterna o
sparkline entre containers a correr e memória usada. <code>--once</code> imprime um snapshot de
texto e sai (scripts/CI) — é também o que acontece automaticamente quando o stdout não é um
terminal. <code>--json</code> dá o mesmo snapshot em JSON, para scripts ou um datasource do
Grafana. Para scrape contínuo, o <code>delonix-mgmt</code>/<code>delonix-cri</code> expõem
<code>/metrics</code> (Prometheus — gauges de containers/VMs/memória/rede/disco) e
<code>GET /v1/dash</code> (o mesmo <code>DashSummary</code> em JSON); os campos caros (rede/disco)
recalculam em background a cada 30s, o scrape em si fica sempre rápido.""",
        "subs": {},
        "examples": [
            ("TUI interactiva", "delonix dash"),
            ("Snapshot único, para um script", "delonix dash --once"),
            ("JSON, para um datasource ou pipeline", "delonix dash --json | jq '.tiles'"),
        ],
    },
    "docker-api": {
        "title": "delonix serve docker-api",
        "tagline": "Fatia da API Docker Engine, num socket unix — ciclo de vida completo de um container, não só leitura.",
        "intro": """Serve o suficiente da API real do Docker Engine (protocolo capturado ao vivo
contra um <code>docker</code> CLI real, versão negociada via o header <code>Api-Version</code> da
resposta ao <code>/_ping</code>) para <code>docker version</code>/<code>ps</code>/<code>images</code>/
<code>info</code> apontados via <code>DOCKER_HOST=unix://&lt;socket&gt;</code> funcionarem contra o
estado REAL do delonix. Desde a v0.26.0, também o ciclo de vida completo de um container —
<code>POST /containers/create|start|stop|kill|wait|restart|rename</code>,
<code>DELETE /containers/{id}</code>, <code>GET /containers/{id}/json</code> — todos a delegar nas
mesmas funções do CLI (<code>cmd_run</code>/<code>cmd_stop</code>/<code>cmd_kill</code>/...), zero
lógica duplicada; é o suficiente para <code>docker compose up</code> apontado a este socket
funcionar. Mesma postura de segurança do socket de gestão: 0600 + <code>SO_PEERCRED</code> (só o
próprio utilizador). Fora de escopo: <code>exec</code>/attach interactivo (HTTP hijacking) e
<code>--restart</code> (precisa de um supervisor <code>fork()</code> cru, recusado com erro claro);
qualquer rota não implementada dá 404 claro.""",
        "subs": {},
        "examples": [
            ("Servir no socket por omissão", "delonix serve docker-api &"),
            ("Um `docker` real a falar com o delonix",
             "DOCKER_HOST=unix:///run/delonix-docker.sock docker ps"),
            ("`docker compose up` apontado ao delonix",
             "DOCKER_HOST=unix:///run/delonix-docker.sock docker compose up -d"),
        ],
    },
    "kube": {
        "title": "delonix cluster kube",
        "tagline": "Gera manifestos Kubernetes a partir de containers.",
        "intro": """<code>kube generate</code> produz um manifesto <code>kind: Pod</code> a partir de um
container existente — a ponte para exportar uma carga do runtime local para um cluster.""",
        "subs": {
            "generate": {"examples": [("Pod a partir de um container", "delonix cluster kube generate web > web-pod.yaml")]},
        },
    },
    "netns": {
        "title": "delonix net netns",
        "tagline": "Gestão de baixo nível da infra de ingress rootless.",
        "intro": """A camada crua por baixo do <code>ingress</code>/<code>egress</code>: subir/descer o
holder do ingress, attach/detach de netns, publish/unpublish de portas e firewall por container. A
maioria dos utilizadores nunca precisa disto — usa os grupos de alto nível — mas está exposto para
depuração e integração.""",
        "subs": {
            "firewall": {"examples": [
                ('Aplicar a firewall de um container no ingress',
                 'delonix net netns firewall <id> 10.200.0.5')]},
            "unpublish": {"examples": [
                ('Despublicar',
                 'delonix net netns unpublish 8080')]},
            "publish": {"examples": [
                ('Publicar uma porta pelo ingress, à mão',
                 'delonix net netns publish 8080:80 10.200.0.5')]},
            "exec": {"examples": [
                ('Correr um comando DENTRO de uma netns anexada — para depurar a rede',
                 'delonix net netns exec minha-netns ip -br addr')]},
            "detach": {"examples": [
                ('Desligar e destruir essa netns',
                 'delonix net netns detach minha-netns')]},
            "attach": {"examples": [
                ('Ligar uma netns à bridge (o motor faz isto sozinho no run)',
                 'delonix net netns attach minha-netns')]},
            "down": {"examples": [
                ('Derrubar a infra de rede (mata slirp + holder) — derruba TODOS os containers da SDN',
                 'delonix net netns down')]},
            "status": {"examples": [("Estado da infra de ingress", "delonix net netns status")]},
            "up": {"examples": [("Subir o holder do ingress", "delonix net netns up")]},
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

# Tradução EN de `tagline`/`intro` por grupo (nível de página, não por
# subcomando) — usada pelo toggle de idioma no cabeçalho. Fica num dict à
# parte em vez de dentro de `GROUPS` para não obrigar a tocar em 26 entradas
# grandes já existentes; `group_page` funde os dois por chave.
GROUPS_EN = {
    "container": {
        "tagline": "Container lifecycle: run, ps, start, stop, rm, exec, logs, inspect, stats, apply.",
        "intro": """The <code>container</code> group is the runtime's everyday surface — the
counterpart to <code>docker container</code>. Each invocation is an ephemeral process (no daemon):
<code>run</code> does a direct <code>clone()</code> with the requested namespaces and the state
lands as JSON under <code>$DELONIX_ROOT</code>. In rootless mode, a container's rootfs is a
<em>persistent</em> flat copy — writes survive <code>stop</code>/<code>start</code>, just like in
Docker.""",
    },
    "workload": {
        "tagline": "Unified layer over containers AND VMs: ls, describe, stop, rm (ADR-0002).",
        "intro": """The <code>workload</code> group is the imperative side of the Runtime
Abstraction Layer: a <code>ComputeDriver</code> trait dispatches by name to either the container
or the VM engine, so you can manage both as one thing. <strong>Creation stays declarative</strong>
— a <code>kind: Workload</code> in a manifest (<code>spec.type: container|vm|pod|microvm</code>)
lowers to the matching Kind in <code>manifest::load</code>; see
<a href="../kinds.html">Kinds</a> and <code>examples/workload.yaml</code>.""",
    },
    "pod": {
        "tagline": "Real multi-container pods (create, ls, describe, rm, logs) — N containers as one unit.",
        "intro": """Real Kubernetes-style pods: N containers that <strong>share the pod's
namespaces</strong> and are managed as a single unit. Today they share <strong>netns</strong>
(same IP, reachable via <code>localhost</code>), <strong>IPC</strong> (System V/POSIX) and
<strong>UTS</strong> (the hostname). All of it <em>rootless and daemonless</em>: the pod is a
named SDN netns on the holder (<code>pod-&lt;name&gt;</code>, with an IP on
<code>delonix0</code>), and each container joins it through the <code>nsenter … ip netns
exec</code> re-exec (the internal <code>--pod</code> flag); the first container holds the
IPC/UTS namespaces and the rest <code>setns</code> into <code>/proc/&lt;pid&gt;/ns/{ipc,uts}</code>
— possible without privilege because the re-exec already places them in the holder's userns.
<em>Membership</em> needs no new store: it derives from the <code>delonix.io/pod=&lt;name&gt;</code>
label (like <code>cluster</code>/<code>stack</code>). Created from a <code>kind: Pod</code>
manifest (the same <code>spec.containers[]</code> schema as <code>kind: Container</code>, but with
N containers allowed). <strong>Known limitation:</strong> the <strong>PID</strong> namespace
(<code>shareProcessNamespace</code>, already in the schema) is NOT shared yet — each container
keeps its own process tree; that's the next slice.""",
    },
    "image": {
        "tagline": "OCI images: pull, ls, rm, export — and, with --vm, the golden VM images (build/push).",
        "intro": """Container image management (OCI registries: Docker Hub, ghcr.io, …) with
digest verification on pull. With <code>--vm</code>, the SAME group operates on <strong>golden VM
images</strong> (a <code>.qcow2</code> plus per-image metadata): Ubuntu cloud image +
kubeadm/kubelet/kubectl + <code>delonix-cri</code> — the base <code>delonix cluster</code> builds
on.""",
    },
    "build": {
        "tagline": "Builds an image from a Dockerfile or Delonixfile.",
        "intro": """Build with no daemon and no BuildKit: it spins up one working container per
stage, runs each <code>RUN</code> via <code>exec</code>, applies <code>COPY</code> to the rootfs
(confined to the build context — path traversal is rejected) and packages the result. Without
<code>-f</code>, it looks for a <code>Delonixfile</code> in the context first and only then a
<code>Dockerfile</code> — same grammar, with extensions (<code>SCAN</code>, <code>CPUS</code>,
<code>MEMORY</code>, <code>SECURITY</code>, <code>HEALTHCHECK</code>). <strong>Multi-stage
supported</strong> (<code>FROM ... AS &lt;name&gt;</code> + <code>COPY --from=&lt;stage&gt;</code>);
known limitation: in root mode (overlay), the final stage still has to be a real image, not
another stage (no OCI lineage for a cloned stage) — no such restriction in rootless.
<code>ARG</code>/<code>--build-arg</code> and <code>USER</code>/<code>ENTRYPOINT</code> already
survive the build (including in rootless). <strong>Per-instruction layer cache</strong> (rootless
— a repeated <code>RUN</code>/<code>COPY</code> doesn't re-execute; <code>--no-cache</code> to skip
it; root mode still has no cache). No real BuildKit (no <code>RUN --mount=secret</code>, no
<code>--platform</code>).""",
    },
    "vm": {
        "tagline": "Declarative microVMs: create, ls, status, stop, rm, apply.",
        "intro": """MicroVMs managed by the <code>VmBackend</code> trait — Cloud Hypervisor or
libvirt. <code>create</code> is idempotent (creates or self-heals) and supports per-instance
cloud-init: <code>--hostname</code>, <code>--ssh-key</code> and <code>--user-data</code> generate a
NoCloud ISO automatically. It's the layer <code>delonix cluster kubeadm</code> uses to provision
nodes.""",
    },
    "volumes": {
        "tagline": "Named volumes and bind mounts: create, ls, inspect, rm, apply.",
        "intro": """A thin wrapper over <code>VolumeStore</code>. In <code>container run</code>,
<code>-v name:/dest[:ro]</code> resolves to a named volume (created on demand) and
<code>-v /host:/dest[:ro]</code> to a bind mount — the distinction is automatic.""",
    },
    "network": {
        "tagline": "User networks: create, ls, inspect, rm, apply — bridge and overlay are physically realized.",
        "intro": """For the <code>bridge</code> and <code>overlay</code> drivers,
<code>create</code> orchestrates both the declarative record (<code>NetworkStore</code>) AND the
rootless physical plane together — <code>bridge</code> inside the holder's netns;
<code>overlay</code> brings up a WireGuard-encrypted VXLAN uplink between nodes (a
<code>dlxvx&lt;vni&gt;</code> device enslaved to the bridge, FDB seeded with the peers), all
achievable without host privilege. <code>macvlan</code>/<code>ipvlan</code> only get recorded in
the store — <code>create</code> WARNS loudly that the network wasn't physically realized (they
need <code>CAP_NET_ADMIN</code> in the host's init-netns, outside the rootless model).""",
    },
    "stack": {
        "tagline": "Applies a whole manifest (delonix-manifest.yaml) — every Kind, in dependency order.",
        "intro": """The declarative, Kubernetes-style counterpart to compose: a multi-document
YAML (<code>apiVersion: delonix.io/v1</code>) with 5 Kinds — <code>Network</code>,
<code>Volume</code>, <code>Image</code>, <code>Vm</code>, <code>Container</code> — applied in that
dependency order. <em>Ensure-present</em> semantics (idempotent by name), not a reconciler: no
diffing, rollout or rollback — fail-fast, whatever was already applied stays applied.""",
    },
    "compose": {
        "tagline": "NATIVE support for docker-compose.yml (Compose Spec v2.x) — no Docker, no shim, straight into the engine.",
        "intro": """A foreign-schema translator, the same family as <code>kind: Pod</code> (k8s)
and the Docker API: a hand-written typed parser (no new dependency), translated directly into the
engine — containers reusing the exact <code>container run</code> path, networks/volumes reusing
<code>network</code>/<code>volume apply</code> verbatim (same idempotency, same input hardening).
<code>depends_on</code> honors the 3 real Compose Spec conditions
(<code>service_started</code>/<code>service_healthy</code>/<code>service_completed_successfully</code>)
via topological ordering of the service graph — a cycle is a clear error, never an arbitrary order
— and waits for the real healthcheck (inline in the service, or the image's own). The project
(<code>compose down/ps/logs</code>) is a label on the containers; networks/volumes use
deterministic naming (<code>&lt;project&gt;_&lt;name&gt;</code>) — no registry of its own, the same
philosophy as <code>stack describe</code>.""",
    },
    "cluster": {
        "tagline": "End-to-end Kubernetes: idempotent kubeadm bootstrap over SSH, or full VM provisioning.",
        "intro": """Two paths to a real (not emulated) cluster: <code>cluster apply</code>
bootstraps <code>kubeadm</code> on hosts that are already alive and reachable over SSH —
idempotent <em>with no state file</em> (every step has a <code>check</code> and an
<code>apply</code>; it can never drift from a .tfstate because there isn't one).
<code>cluster kubeadm</code> goes further: it provisions the VMs from the golden VM image, waits
for SSH, and runs the SAME bootstrap — one command, from zero to a cluster running
<code>delonix-cri</code> as its runtime (no containerd).""",
    },
    "secret": {
        "tagline": "Encrypted-at-rest secret vault — the source behind `run --secret`.",
        "intro": """A local vault (<code>SecretStore</code>) encrypted with XChaCha20-Poly1305.
Values are NEVER printed by default (redacted; <code>--reveal</code> is opt-in). It's the source
for <code>container run --secret</code>/<code>--secret-files</code> and <code>storage</code>'s
<code>--password-secret</code> — the secret goes in once, and never ends up in shell history or in
the manifest.""",
    },
    "storage": {
        "tagline": "NETWORK volumes (NFS/CIFS/WebDAV) you can mount, k8s PersistentVolume-style.",
        "intro": """Mounts a folder from a NAS (TrueNAS/Synology/Samba/Nextcloud) as a named
volume. Under the hood it's a <code>delonix-volume</code> volume with a network driver —
<code>mount -t nfs|cifs|davfs</code>. The password comes from the vault
(<code>--password-secret</code>), never from argv. Wired into <code>stack apply</code> (order
Network→Volume→<strong>Storage</strong>→Image→Vm→Container). Mounting needs CAP_SYS_ADMIN.""",
    },
    "sharevolume": {
        "tagline": "An ISOLATED, individually-QUOTA'd slice of a `Storage` — several container/vm/pod share one NAS.",
        "intro": """Solves a concrete multi-tenant problem: several workloads sharing ONE
NFS/CIFS/WebDAV export, each with ITS OWN isolated mount point and ITS OWN quota, without seeing
each other. Under the hood there's no new mount mechanism at all: each <code>ShareVolume</code> is
a real SUBDIRECTORY of the tree already mounted by the parent <code>kind: Storage</code>
(<code>&lt;storage&gt;/_data/shares/&lt;name&gt;</code>), registered as its own volume — the
isolation is pure path confinement, and consumption uses the usual
<code>-v &lt;name&gt;:/dest</code>, no new code at all on the container/vm/pod side. The quota is
SOFT (measured usage + alert) — the HARD path (a loopback ext4 image) needs local block storage
and doesn't compose with a subdirectory of a network mount.""",
    },
    "ingress": {
        "tagline": "INBOUND firewall (L4 rules + DNAT publishes) for a container on the SDN.",
        "intro": """Half of the unified firewall surface (the other is <code>egress</code>). Edits
the single source of truth — the per-container <code>ContainerFw</code>, applied as nft rules in
the ingress chain. <code>ingress</code> governs INBOUND traffic: allow/deny rules by
<code>[proto/]port</code> and CIDR, the default policy, and DNAT <em>publishes</em>. Only acts on
containers on a custom network (they have an IP on <code>delonix0</code>); <code>--net host</code>
is refused.""",
    },
    "egress": {
        "tagline": "OUTBOUND firewall (L4 rules + per-network egress→Internet policy).",
        "intro": """The other half of the firewall. Governs a container's OUTBOUND traffic
(allow/deny rules + default policy) and, at the NETWORK level, egress policy to the Internet:
<code>allow</code>/<code>deny</code>, or <code>allowlist</code> (denies everything except DNS and
the given CIDRs). All on the same <code>ContainerFw</code>/nft as <code>ingress</code>.""",
    },
    "httproute": {
        "tagline": "Built-in L7/HTTP reverse proxy (`kind: HTTPRoute`) — routing by Host + path prefix.",
        "intro": """A <strong>built-in</strong> HTTP/HTTPS reverse proxy (pure hyper, no
Nginx/Envoy), running inside the holder's netns and routing by <code>Host</code> + <code>path</code>
prefix to backend containers on the SDN. TLS terminates at the proxy (self-signed or
<code>secretRef</code>); hot reload via SIGHUP (routes swap with no downtime, listeners stay fixed
at startup). A container with <code>--expose &lt;port&gt;</code> self-registers under
<code>&lt;name&gt;.&lt;namespace&gt;.delonix.internal</code>, with no manual
<code>kind: HTTPRoute</code> needed. It's what <code>kind: Tunnel</code> normally sits in front of
to give several backends a single public URL.""",
    },
    "tunnel": {
        "tagline": "Exposes a local port to the public internet via pinggy/ngrok/cloudflare (`kind: Tunnel`).",
        "intro": """Does ONE thing: carries traffic from the public internet down to ONE local
port — no account, no public IP, no router config. Pairs with <code>httproute</code> by pointing
<code>--local-port</code> at the port the L7 proxy listens on, and <code>Host</code>-based routing
on the other end still decides which container each request goes to — one public URL, several
backends. Three providers, each the REAL binary/mechanism of that service (never simulated):
<strong>pinggy</strong> (zero extra binary — plain <code>ssh</code>, already a project dependency),
<strong>ngrok</strong> (needs the <code>ngrok</code> agent on PATH; the public URL comes from the
agent's own local API) and <strong>cloudflare</strong> (needs <code>cloudflared</code>; for now
only the ephemeral quick-tunnel <code>*.trycloudflare.com</code>, no account — a NAMED tunnel with
its own domain needs the Cloudflare API, not implemented yet).""",
    },
    "flow": {
        "tagline": "Live per-container traffic — eBPF datapath (degrades to veth counters).",
        "intro": """Per-container network telemetry. When run with privilege (CAP_BPF/root), it
attaches two eBPF tc/clsact classifiers to the SDN veths, which count bytes/packets per IP in a
shared BPF map — <strong>never dropping anything</strong> (nft remains the only enforcer). Without
privilege (the common rootless case) it says so and falls back to veth counters, which always
work. <code>--watch</code> redraws every 2s.""",
    },
    "boot": {
        "tagline": "Boot persistence: systemd units so containers come back up after a reboot.",
        "intro": """<code>boot enable</code> generates one systemd unit per running container
(rootless → user units + <code>loginctl enable-linger</code>; root → system units), with
<code>ExecStart</code>=<code>container start</code>. So containers come back up when the host
boots, with no daemon.""",
    },
    "system": {
        "tagline": "The engine itself: events, info, df, prune, monitor, thermal.",
        "intro": """Introspection and maintenance. <code>system prune</code> is the GC (reclaims
space: stopped containers, orphan dirs, dangling images, CAS blobs, orphan hostfwds, empty
networks); <code>system df</code> shows disk usage; <code>system monitor</code> follows
connections/conntrack; <code>system events</code> the event stream.""",
    },
    "dash": {
        "tagline": "Summary/KPI dashboard (htop-style TUI) — RAM/network/disk, per-container uptime, JSON and Prometheus.",
        "intro": """A live view of runtime state — containers, VMs, images, networks, storage —
in one screen, without running <code>ls</code> across 5 different groups. Each group also has its
own (<code>container dash</code>, <code>vm dash</code>, ...); this is the global aggregate.
Dynamic KPIs: cgroup slice memory, accumulated rx/tx traffic per container (with an explicit count
of unmeasured <code>--net host/none</code> containers, never silently summed as zero), disk usage
by area (images/volumes/VM-images/containers), and real per-container uptime (the <code>UP</code>
column, from <code>pid_starttime</code>). The <code>m</code> key toggles the sparkline between
running containers and memory used. <code>--once</code> prints a text snapshot and exits
(scripts/CI) — also what happens automatically when stdout isn't a terminal. <code>--json</code>
gives the same snapshot as JSON, for scripts or a Grafana datasource. For continuous scraping,
<code>delonix-mgmt</code>/<code>delonix-cri</code> expose <code>/metrics</code> (Prometheus —
container/VM/memory/network/disk gauges) and <code>GET /v1/dash</code> (the same
<code>DashSummary</code> as JSON); the expensive fields (network/disk) recompute in the background
every 30s, so the scrape itself always stays fast.""",
    },
    "docker-api": {
        "tagline": "A slice of the Docker Engine API, on a unix socket — full container lifecycle, not just reads.",
        "intro": """Serves enough of the real Docker Engine API (protocol captured live against a
real <code>docker</code> CLI, version negotiated via the <code>Api-Version</code> header on the
<code>/_ping</code> response) for <code>docker version</code>/<code>ps</code>/<code>images</code>/
<code>info</code> pointed at <code>DOCKER_HOST=unix://&lt;socket&gt;</code> to work against
delonix's REAL state. Since v0.26.0, also the full container lifecycle —
<code>POST /containers/create|start|stop|kill|wait|restart|rename</code>,
<code>DELETE /containers/{id}</code>, <code>GET /containers/{id}/json</code> — all delegating to
the same CLI functions (<code>cmd_run</code>/<code>cmd_stop</code>/<code>cmd_kill</code>/...), zero
duplicated logic; enough for <code>docker compose up</code> pointed at this socket to work. Same
security posture as the management socket: 0600 + <code>SO_PEERCRED</code> (owner only). Out of
scope: interactive <code>exec</code>/attach (HTTP hijacking) and <code>--restart</code> (needs a
raw <code>fork()</code> supervisor, refused with a clear error); any unimplemented route gives a
clear 404.""",
    },
    "kube": {
        "tagline": "Generates Kubernetes manifests from containers.",
        "intro": """<code>kube generate</code> produces a <code>kind: Pod</code> manifest from an
existing container — the bridge for exporting a workload from the local runtime to a cluster.""",
    },
    "netns": {
        "tagline": "Low-level management of the rootless ingress infrastructure.",
        "intro": """The raw layer underneath <code>ingress</code>/<code>egress</code>: bringing
the ingress holder up/down, attaching/detaching netns, publishing/unpublishing ports and
per-container firewalling. Most users never need this — they use the higher-level groups — but
it's exposed for debugging and integration.""",
    },
    "completion": {
        "tagline": "Dynamic autocompletion for bash, zsh, fish, elvish and powershell.",
        "intro": """Prints the shell registration script. The engine is dynamic: the script asks
the binary itself for suggestions in real time, from the SAME definition used for parsing — it
never goes stale by hand.""",
    },
}


def lab_challenge_html(entry):
    """Secção "Laboratório"/"Desafio" bilingue de uma página de referência
    CLI: um exercício guiado com comandos reais desse grupo, e um desafio
    mais aberto a seguir — para o leitor aprender fazendo, não só lendo."""
    parts = [f"<h2>{bi('span', 'Laboratório', 'Lab')}</h2>"]
    parts.append(bi("div", entry["lab"]["pt"], entry["lab"]["en"], cls="callout"))
    parts.append(f"<h2>{bi('span', 'Desafio', 'Challenge')}</h2>")
    parts.append(bi("div", entry["challenge"]["pt"], entry["challenge"]["en"], cls="callout"))
    return "\n".join(parts)


# Lab + Desafio por grupo de comandos — cada um usa SÓ subcomandos/flags reais
# (confirmados contra `GROUPS[name]["subs"]` e o `AGENTS.md` do projecto),
# nunca inventados. `lab` é guiado (passo a passo); `challenge` é mais aberto,
# normalmente estica uma garantia já documentada do motor (idempotência,
# isolamento, persistência) em vez de introduzir comportamento novo.
CLI_LABS = {
    "container": {
        "lab": {"pt": """<p>Sobe um nginx publicado, confirma que responde, e prova que o estado
sobrevive a um <code>stop</code>/<code>start</code> (ao contrário de recriar o container).</p>
<pre><code>delonix container run -d --name web -p 8080:80 nginx
curl localhost:8080
delonix container logs web
delonix container stop web
delonix container start web
curl localhost:8080</code></pre>""",
                "en": """<p>Bring up a published nginx, confirm it answers, and prove the state
survives a <code>stop</code>/<code>start</code> (unlike recreating the container).</p>
<pre><code>delonix container run -d --name web -p 8080:80 nginx
curl localhost:8080
delonix container logs web
delonix container stop web
delonix container start web
curl localhost:8080</code></pre>"""},
        "challenge": {"pt": """<p>Sem parar o <code>web</code>, troca a porta publicada a quente
com <code>container update</code> (o PID não muda) e confirma com <code>container diff</code> que
escrever um ficheiro dentro do container aparece na comparação com a imagem original.</p>
<pre><code>delonix container update web --publish-rm 8080 --publish-add 9090:80
delonix container exec web sh -c 'echo oi > /tmp/marca'
delonix container diff web</code></pre>""",
                "en": """<p>Without stopping <code>web</code>, hot-swap the published port with
<code>container update</code> (the PID doesn't change) and confirm with <code>container diff</code>
that writing a file inside the container shows up against the original image.</p>
<pre><code>delonix container update web --publish-rm 8080 --publish-add 9090:80
delonix container exec web sh -c 'echo hi > /tmp/mark'
delonix container diff web</code></pre>"""},
    },
    "workload": {
        "lab": {"pt": """<p>Cria um container normal e observa-o pela lente unificada — o mesmo
<code>workload</code> que também sabe falar de VMs.</p>
<pre><code>delonix container run -d --name api nginx
delonix workload ls
delonix workload describe api
delonix workload stop api</code></pre>""",
                "en": """<p>Create a plain container and look at it through the unified lens — the
same <code>workload</code> group that also knows how to talk about VMs.</p>
<pre><code>delonix container run -d --name api nginx
delonix workload ls
delonix workload describe api
delonix workload stop api</code></pre>"""},
        "challenge": {"pt": """<p>Cria um container E uma VM com o MESMO nome e chama
<code>workload describe &lt;nome&gt;</code> — confirma que o comando recusa por ambiguidade em vez
de adivinhar qual dos dois querias, e que a mensagem aponta para o comando específico.</p>""",
                "en": """<p>Create a container AND a VM with the SAME name and call
<code>workload describe &lt;name&gt;</code> — confirm the command refuses for ambiguity instead of
guessing which of the two you meant, and that the message points at the specific command.</p>"""},
    },
    "pod": {
        "lab": {"pt": """<p>Cria um pod de 2 containers e confirma que partilham IP —
alcançam-se por <code>localhost</code>, como no Kubernetes.</p>
<pre><code>delonix pod create web-pod --container nginx --container redis
delonix pod describe web-pod
delonix pod logs web-pod</code></pre>""",
                "en": """<p>Create a 2-container pod and confirm they share an IP — reachable via
<code>localhost</code> from each other, just like in Kubernetes.</p>
<pre><code>delonix pod create web-pod --container nginx --container redis
delonix pod describe web-pod
delonix pod logs web-pod</code></pre>"""},
        "challenge": {"pt": """<p>Escreve o MESMO pod como um <code>kind: Pod</code> num
manifesto e aplica-o com <code>stack apply</code>. Compara o <code>pod describe</code> resultante
com o pod criado pela CLI — devem ter a mesma forma (netns/IPC/UTS partilhados).</p>""",
                "en": """<p>Write the SAME pod as a <code>kind: Pod</code> in a manifest and apply
it with <code>stack apply</code>. Compare the resulting <code>pod describe</code> against the
CLI-created pod — they should look the same (shared netns/IPC/UTS).</p>"""},
    },
    "image": {
        "lab": {"pt": """<p>Traz uma imagem, dá-lhe uma tag própria, e olha para o histórico de
camadas antes de a exportar.</p>
<pre><code>delonix image pull alpine:3.20
delonix image tag alpine:3.20 meu-alpine:v1
delonix image history meu-alpine:v1
delonix image export meu-alpine:v1 -o alpine.tar</code></pre>""",
                "en": """<p>Pull an image, give it your own tag, and look at the layer history
before exporting it.</p>
<pre><code>delonix image pull alpine:3.20
delonix image tag alpine:3.20 my-alpine:v1
delonix image history my-alpine:v1
delonix image export my-alpine:v1 -o alpine.tar</code></pre>"""},
        "challenge": {"pt": """<p>Antes de trazer a imagem VM dourada, vê que versões existem
publicadas com <code>ls-remote</code> — sem descarregar nada — e só depois traz a que quiseres.</p>
<pre><code>delonix image --vm ls-remote
delonix image --vm pull</code></pre>""",
                "en": """<p>Before pulling the golden VM image, check which versions are
published with <code>ls-remote</code> — without downloading anything — and only then pull the one
you want.</p>
<pre><code>delonix image --vm ls-remote
delonix image --vm pull</code></pre>"""},
    },
    "build": {
        "lab": {"pt": """<p>Escreve um <code>Delonixfile</code> multi-stage pequeno e constrói-o
— sem daemon, sem BuildKit.</p>
<pre><code>printf 'FROM golang:1.22 AS build\\nWORKDIR /src\\nCOPY . .\\nRUN go build -o app\\n\\nFROM alpine\\nCOPY --from=build /src/app /app\\nENTRYPOINT ["/app"]\\n' > Delonixfile
delonix build -t minha-app .
delonix container run --rm minha-app</code></pre>""",
                "en": """<p>Write a small multi-stage <code>Delonixfile</code> and build it — no
daemon, no BuildKit.</p>
<pre><code>printf 'FROM golang:1.22 AS build\\nWORKDIR /src\\nCOPY . .\\nRUN go build -o app\\n\\nFROM alpine\\nCOPY --from=build /src/app /app\\nENTRYPOINT ["/app"]\\n' > Delonixfile
delonix build -t my-app .
delonix container run --rm my-app</code></pre>"""},
        "challenge": {"pt": """<p>Passa um segredo com <code>--secret id=token,src=./token.txt</code>
e usa <code>RUN --mount=type=secret,id=token</code> dentro de uma instrução. Depois de o build
terminar, confirma que o valor NÃO está na imagem final (nem sequer um ficheiro vazio).</p>""",
                "en": """<p>Pass a secret with <code>--secret id=token,src=./token.txt</code> and
use <code>RUN --mount=type=secret,id=token</code> in one instruction. After the build finishes,
confirm the value is NOT in the final image (not even an empty file).</p>"""},
    },
    "vm": {
        "lab": {"pt": """<p>Cria uma microVM com cloud-init automático e liga-te por SSH.</p>
<pre><code>delonix vm create dev --hostname dev --ssh-key ~/.ssh/id_ed25519.pub
delonix vm status dev
ssh delonix@$(delonix vm status dev --ip)</code></pre>""",
                "en": """<p>Create a microVM with automatic cloud-init and SSH into it.</p>
<pre><code>delonix vm create dev --hostname dev --ssh-key ~/.ssh/id_ed25519.pub
delonix vm status dev
ssh delonix@$(delonix vm status dev --ip)</code></pre>"""},
        "challenge": {"pt": """<p>Só no backend libvirt: tira um <code>snapshot</code> da VM a
correr, muda alguma coisa lá dentro, e usa <code>restore</code> para voltar atrás. Confirma que a
mudança desapareceu.</p>
<pre><code>delonix vm snapshot dev antes-da-mudanca
delonix vm restore dev antes-da-mudanca</code></pre>""",
                "en": """<p>libvirt backend only: take a <code>snapshot</code> of the running VM,
change something inside it, then use <code>restore</code> to roll back. Confirm the change is
gone.</p>
<pre><code>delonix vm snapshot dev before-the-change
delonix vm restore dev before-the-change</code></pre>"""},
    },
    "volumes": {
        "lab": {"pt": """<p>Prova que um volume nomeado sobrevive a um restart do container —
ao contrário de escrever directamente no rootfs.</p>
<pre><code>delonix volumes create dados
delonix container run -d --name db -v dados:/var/lib/data alpine sleep infinity
delonix container exec db sh -c 'echo ok > /var/lib/data/marca'
delonix container stop db && delonix container start db
delonix container exec db cat /var/lib/data/marca</code></pre>""",
                "en": """<p>Prove a named volume survives a container restart — unlike writing
straight to the rootfs.</p>
<pre><code>delonix volumes create data
delonix container run -d --name db -v data:/var/lib/data alpine sleep infinity
delonix container exec db sh -c 'echo ok > /var/lib/data/mark'
delonix container stop db && delonix container start db
delonix container exec db cat /var/lib/data/mark</code></pre>"""},
        "challenge": {"pt": """<p>Tira um <code>snapshot</code> do volume <code>dados</code> depois
de escrever nele, e usa <code>volumes describe</code> para veres o histórico de snapshots.</p>""",
                "en": """<p>Take a <code>snapshot</code> of the <code>data</code> volume after
writing to it, and use <code>volumes describe</code> to see the snapshot history.</p>"""},
    },
    "network": {
        "lab": {"pt": """<p>Cria uma rede própria e confirma a descoberta por nome (DNS interno)
entre dois containers na mesma rede.</p>
<pre><code>delonix network create minha-rede
delonix container run -d --name db --net minha-rede postgres:16-alpine
delonix container run --rm --net minha-rede alpine ping -c1 db</code></pre>""",
                "en": """<p>Create your own network and confirm name-based discovery (internal
DNS) between two containers on the same network.</p>
<pre><code>delonix network create my-net
delonix container run -d --name db --net my-net postgres:16-alpine
delonix container run --rm --net my-net alpine ping -c1 db</code></pre>"""},
        "challenge": {"pt": """<p>Corre <code>network describe minha-rede</code> e identifica que
IP a rede atribuiu ao <code>db</code>; depois tenta <code>network create --driver macvlan</code> e
lê o aviso — porque é que esse driver não é realizado fisicamente em rootless?</p>""",
                "en": """<p>Run <code>network describe my-net</code> and find the IP it assigned to
<code>db</code>; then try <code>network create --driver macvlan</code> and read the warning — why
isn't that driver physically realized in rootless?</p>"""},
    },
    "stack": {
        "lab": {"pt": """<p>Gera um projecto COMPLETO já pronto (código + Delonixfile +
manifesto) a partir de um template, e aplica-o.</p>
<pre><code>delonix stack init minha-api --template python
cd minha-api
delonix stack apply</code></pre>""",
                "en": """<p>Generate a COMPLETE, ready-to-run project (code + Delonixfile +
manifest) from a template, and apply it.</p>
<pre><code>delonix stack init my-api --template python
cd my-api
delonix stack apply</code></pre>"""},
        "challenge": {"pt": """<p>Corre <code>stack apply --dry-run</code> e compara o YAML
impresso (com todos os defaults preenchidos) com o teu <code>delonix-manifest.yaml</code>
original — que campos é que o motor preencheu por ti?</p>""",
                "en": """<p>Run <code>stack apply --dry-run</code> and compare the printed YAML
(every default filled in) against your original <code>delonix-manifest.yaml</code> — which fields
did the engine fill in for you?</p>"""},
    },
    "compose": {
        "lab": {"pt": """<p>Sobe um <code>docker-compose.yml</code> com uma app que só arranca
depois da base de dados estar saudável (<code>depends_on: condition: service_healthy</code>).</p>
<pre><code>delonix compose up -d
delonix compose ps
delonix compose logs -f app</code></pre>""",
                "en": """<p>Bring up a <code>docker-compose.yml</code> where the app only starts
after the database is healthy (<code>depends_on: condition: service_healthy</code>).</p>
<pre><code>delonix compose up -d
delonix compose ps
delonix compose logs -f app</code></pre>"""},
        "challenge": {"pt": """<p>Corre <code>compose down -v</code> e confirma com
<code>delonix volumes ls</code> que o volume nomeado do projecto foi mesmo removido — não só os
containers.</p>""",
                "en": """<p>Run <code>compose down -v</code> and confirm with
<code>delonix volumes ls</code> that the project's named volume was actually removed — not just
the containers.</p>"""},
    },
    "cluster": {
        "lab": {"pt": """<p>Um comando, do zero a um cluster Kubernetes real a correr —
sem Docker, sem containerd, com <code>delonix-cri</code> como runtime.</p>
<pre><code>delonix cluster kubeadm --control-plane 1 --workers 2
export KUBECONFIG=~/.delonix/clusters/*-kubeconfig.yaml
kubectl get nodes</code></pre>""",
                "en": """<p>One command, from zero to a real Kubernetes cluster running — no
Docker, no containerd, with <code>delonix-cri</code> as the runtime.</p>
<pre><code>delonix cluster kubeadm --control-plane 1 --workers 2
export KUBECONFIG=~/.delonix/clusters/*-kubeconfig.yaml
kubectl get nodes</code></pre>"""},
        "challenge": {"pt": """<p>Constrói uma imagem local com <code>delonix build</code>,
importa-a directamente no containerd de cada nó com <code>cluster load</code> (sem passar por
registo nenhum) e corre um pod com <code>imagePullPolicy: Never</code> a usá-la.</p>
<pre><code>delonix cluster load minha-app:v1</code></pre>""",
                "en": """<p>Build a local image with <code>delonix build</code>, import it
directly into every node's containerd with <code>cluster load</code> (no registry involved), and
run a pod with <code>imagePullPolicy: Never</code> using it.</p>
<pre><code>delonix cluster load my-app:v1</code></pre>"""},
    },
    "secret": {
        "lab": {"pt": """<p>Cria um segredo, usa-o num container, e confirma que fica redigido
por omissão.</p>
<pre><code>printf 's3nha' | delonix secret create db-pass
delonix container run --rm --secret db-pass alpine env
delonix secret inspect db-pass
delonix secret inspect db-pass --reveal</code></pre>""",
                "en": """<p>Create a secret, use it in a container, and confirm it's redacted by
default.</p>
<pre><code>printf 's3cr3t' | delonix secret create db-pass
delonix container run --rm --secret db-pass alpine env
delonix secret inspect db-pass
delonix secret inspect db-pass --reveal</code></pre>"""},
        "challenge": {"pt": """<p>Roda a chave do cofre com <code>secret rotate-key</code> e
confirma que o segredo <code>db-pass</code> criado antes continua legível depois — a rotação não
pode obrigar a recriar segredos.</p>""",
                "en": """<p>Rotate the vault key with <code>secret rotate-key</code> and confirm
the <code>db-pass</code> secret created earlier is still readable afterwards — rotation must never
force you to recreate secrets.</p>"""},
    },
    "storage": {
        "lab": {"pt": """<p>Regista um export NFS como <code>storage</code> e monta-o num
container, com a password a vir do cofre em vez do manifesto.</p>
<pre><code>printf 'password-do-nas' | delonix secret create nas-pass
delonix storage create nas1 --server 192.168.1.50 --share /export/dados --password-secret nas-pass
delonix container run --rm -v nas1:/mnt alpine sh -c 'echo ok > /mnt/teste'</code></pre>""",
                "en": """<p>Register an NFS export as <code>storage</code> and mount it into a
container, with the password coming from the vault instead of the manifest.</p>
<pre><code>printf 'nas-password' | delonix secret create nas-pass
delonix storage create nas1 --server 192.168.1.50 --share /export/data --password-secret nas-pass
delonix container run --rm -v nas1:/mnt alpine sh -c 'echo ok > /mnt/test'</code></pre>"""},
        "challenge": {"pt": """<p>Confirma no próprio NAS (por outra via — outro host, ou o
painel do NAS) que o ficheiro <code>teste</code> chegou mesmo lá — a prova de que o
<code>storage</code> não é um volume local disfarçado.</p>""",
                "en": """<p>Confirm on the NAS itself (through another path — another host, or the
NAS's own panel) that the <code>test</code> file really landed there — the proof that
<code>storage</code> isn't a local volume in disguise.</p>"""},
    },
    "sharevolume": {
        "lab": {"pt": """<p>Duas fatias isoladas do MESMO <code>Storage</code>, cada uma com a
sua quota, para dois containers que não se devem ver.</p>
<pre><code>delonix sharevolume apply -f sharevolume-a.yaml
delonix sharevolume apply -f sharevolume-b.yaml
delonix sharevolume ls</code></pre>""",
                "en": """<p>Two isolated slices of the SAME <code>Storage</code>, each with its
own quota, for two containers that shouldn't see each other.</p>
<pre><code>delonix sharevolume apply -f sharevolume-a.yaml
delonix sharevolume apply -f sharevolume-b.yaml
delonix sharevolume ls</code></pre>"""},
        "challenge": {"pt": """<p>Monta o ShareVolume A num container e o B noutro. Confirma que
listar o ponto de montagem de um NUNCA mostra ficheiros do outro — a isolação é só confinamento de
caminho, sem mecanismo de montagem novo.</p>""",
                "en": """<p>Mount ShareVolume A into one container and B into another. Confirm
that listing one's mount point never shows the other's files — the isolation is pure path
confinement, no new mount mechanism involved.</p>"""},
    },
    "ingress": {
        "lab": {"pt": """<p>Fecha tudo por omissão e abre só uma porta — o modelo
default-deny.</p>
<pre><code>delonix net ingress policy web deny
delonix net ingress allow web 80
curl web-host:80</code></pre>""",
                "en": """<p>Close everything by default and open just one port — the
default-deny model.</p>
<pre><code>delonix net ingress policy web deny
delonix net ingress allow web 80
curl web-host:80</code></pre>"""},
        "challenge": {"pt": """<p>Reproduz o bug histórico já corrigido: <code>ingress allow web
9999</code> (SEM indicar proto) só deveria abrir a porta 9999. Confirma com <code>ingress ls</code>
que as outras portas continuam fechadas — o veredicto da coluna tem de bater com o que o
<code>curl</code> mostra.</p>""",
                "en": """<p>Reproduce the historical bug that's already fixed: <code>ingress allow
web 9999</code> (with NO proto given) should only open port 9999. Confirm with
<code>ingress ls</code> that the other ports stay closed — the column's verdict has to match what
<code>curl</code> actually shows.</p>"""},
    },
    "egress": {
        "lab": {"pt": """<p>Nega tudo excepto DNS e um destino específico — a política
<code>allowlist</code> por rede.</p>
<pre><code>delonix net egress policy minha-rede allowlist --allow 1.1.1.1/32
delonix container run --rm --net minha-rede alpine wget -qO- https://1.1.1.1
delonix container run --rm --net minha-rede alpine wget -qO- https://example.com</code></pre>""",
                "en": """<p>Deny everything except DNS and one specific destination — the
per-network <code>allowlist</code> policy.</p>
<pre><code>delonix net egress policy my-net allowlist --allow 1.1.1.1/32
delonix container run --rm --net my-net alpine wget -qO- https://1.1.1.1
delonix container run --rm --net my-net alpine wget -qO- https://example.com</code></pre>"""},
        "challenge": {"pt": """<p>Abre uma ligação de saída de longa duração e só DEPOIS aplica
<code>egress policy deny</code>. Confirma que a ligação já estabelecida continua viva — só ligações
NOVAS são bloqueadas.</p>""",
                "en": """<p>Open a long-lived outbound connection and only THEN apply
<code>egress policy deny</code>. Confirm the already-established connection keeps working — only
NEW connections get blocked.</p>"""},
    },
    "httproute": {
        "lab": {"pt": """<p>Um proxy L7, duas apps, uma porta — routing por <code>Host</code>.</p>
<pre><code>delonix net httproute apply -f httproute.yaml
curl app1.local:8443 -H 'Host: app1.local'
curl app2.local:8443 -H 'Host: app2.local'</code></pre>""",
                "en": """<p>One L7 proxy, two apps, one port — routing by <code>Host</code>.</p>
<pre><code>delonix net httproute apply -f httproute.yaml
curl app1.local:8443 -H 'Host: app1.local'
curl app2.local:8443 -H 'Host: app2.local'</code></pre>"""},
        "challenge": {"pt": """<p>Em vez de escreveres um <code>kind: HTTPRoute</code> à mão, usa
<code>container run --expose 8080</code> e alcança o container pelo FQDN interno automático
(<code>&lt;nome&gt;.&lt;namespace&gt;.delonix.internal</code>) — zero configuração manual de
rota.</p>""",
                "en": """<p>Instead of writing a <code>kind: HTTPRoute</code> by hand, use
<code>container run --expose 8080</code> and reach the container through the automatic internal
FQDN (<code>&lt;name&gt;.&lt;namespace&gt;.delonix.internal</code>) — zero manual route
configuration.</p>"""},
    },
    "tunnel": {
        "lab": {"pt": """<p>Uma porta local, uma URL pública — sem conta, sem router.</p>
<pre><code>delonix net tunnel expose --provider pinggy --local-port 8080
delonix net tunnel ls</code></pre>""",
                "en": """<p>One local port, one public URL — no account, no router config.</p>
<pre><code>delonix net tunnel expose --provider pinggy --local-port 8080
delonix net tunnel ls</code></pre>"""},
        "challenge": {"pt": """<p>Aponta o <code>--local-port</code> do tunnel para a porta onde o
proxy L7 (<code>httproute</code>) escuta, e confirma que a MESMA URL pública consegue servir vários
containers backend diferentes, decididos pelo <code>Host</code> do pedido.</p>""",
                "en": """<p>Point the tunnel's <code>--local-port</code> at the port the L7 proxy
(<code>httproute</code>) listens on, and confirm the SAME public URL can serve several different
backend containers, decided by the request's <code>Host</code>.</p>"""},
    },
    "flow": {
        "lab": {"pt": """<p>Observa tráfego por-container ao vivo enquanto geras carga noutro
terminal.</p>
<pre><code># num terminal
delonix net flow --watch
# noutro terminal
while true; do curl -s localhost:8080 >/dev/null; done</code></pre>""",
                "en": """<p>Watch live per-container traffic while generating load in another
terminal.</p>
<pre><code># in one terminal
delonix net flow --watch
# in another terminal
while true; do curl -s localhost:8080 >/dev/null; done</code></pre>"""},
        "challenge": {"pt": """<p>Corre <code>flow</code> sem privilégio e depois com
CAP_BPF/root, e compara: sem privilégio cai nos contadores veth (sempre funciona); com privilégio
usa os classificadores eBPF tc/clsact — nos dois casos o nft continua o único a fazer drop.</p>""",
                "en": """<p>Run <code>flow</code> without privilege and then with CAP_BPF/root, and
compare: without privilege it falls back to veth counters (always works); with privilege it uses
the eBPF tc/clsact classifiers — in both cases nft remains the only thing that drops packets.</p>"""},
    },
    "boot": {
        "lab": {"pt": """<p>Faz um container sobreviver a um reboot do host, sem daemon
nenhum a vigiá-lo.</p>
<pre><code>delonix container run -d --name web nginx
delonix net boot enable web
delonix net boot status web</code></pre>""",
                "en": """<p>Make a container survive a host reboot, with no daemon watching over
it.</p>
<pre><code>delonix container run -d --name web nginx
delonix net boot enable web
delonix net boot status web</code></pre>"""},
        "challenge": {"pt": """<p>Em rootless, confirma que a unit gerada é uma <em>user unit</em>
(não system) e que <code>loginctl enable-linger</code> ficou activo para a tua conta — sem isso a
unit nunca arrancaria antes do login.</p>""",
                "en": """<p>In rootless mode, confirm the generated unit is a <em>user unit</em>
(not a system one) and that <code>loginctl enable-linger</code> got enabled for your account —
without it, the unit would never start before login.</p>"""},
    },
    "system": {
        "lab": {"pt": """<p>Enche disco com lixo (containers parados) e depois recupera-o.</p>
<pre><code>for i in 1 2 3; do delonix container run --name lixo-$i alpine true; done
delonix system df
delonix system prune
delonix system df</code></pre>""",
                "en": """<p>Fill up disk with junk (stopped containers) and then reclaim it.</p>
<pre><code>for i in 1 2 3; do delonix container run --name junk-$i alpine true; done
delonix system df
delonix system prune
delonix system df</code></pre>"""},
        "challenge": {"pt": """<p>Segue <code>system events</code> num terminal enquanto corres
<code>container run</code>/<code>stop</code>/<code>rm</code> noutro, e mapeia cada acção da CLI ao
evento exacto que ela emite.</p>""",
                "en": """<p>Follow <code>system events</code> in one terminal while running
<code>container run</code>/<code>stop</code>/<code>rm</code> in another, and map each CLI action
to the exact event it emits.</p>"""},
    },
    "dash": {
        "lab": {"pt": """<p>Um ecrã só, sem correr <code>ls</code> em 5 grupos diferentes.</p>
<pre><code>delonix dash --once
delonix dash</code></pre>
<p>Na TUI, carrega em <code>m</code> para alternar o sparkline entre containers a correr e memória
usada.</p>""",
                "en": """<p>One screen, instead of running <code>ls</code> across 5 different
groups.</p>
<pre><code>delonix dash --once
delonix dash</code></pre>
<p>In the TUI, press <code>m</code> to toggle the sparkline between running containers and memory
used.</p>"""},
        "challenge": {"pt": """<p>Corre <code>delonix dash --json | jq</code> e confirma que os
campos de rede/disco vêm marcados de forma explícita quando ainda não foram medidos — nunca
disfarçados de zero.</p>""",
                "en": """<p>Run <code>delonix dash --json | jq</code> and confirm the network/disk
fields are explicitly marked when they haven't been measured yet — never disguised as zero.</p>"""},
    },
    "docker-api": {
        "lab": {"pt": """<p>Aponta o próprio <code>docker</code> CLI para o delonix, sem Docker
Desktop nenhum instalado.</p>
<pre><code>delonix serve docker-api --addr unix:///tmp/delonix.sock &
DOCKER_HOST=unix:///tmp/delonix.sock docker ps
DOCKER_HOST=unix:///tmp/delonix.sock docker run -d --name web nginx</code></pre>""",
                "en": """<p>Point the real <code>docker</code> CLI at delonix, with no Docker
Desktop installed at all.</p>
<pre><code>delonix serve docker-api --addr unix:///tmp/delonix.sock &
DOCKER_HOST=unix:///tmp/delonix.sock docker ps
DOCKER_HOST=unix:///tmp/delonix.sock docker run -d --name web nginx</code></pre>"""},
        "challenge": {"pt": """<p>Aponta um <code>docker-compose.yml</code> real para o mesmo
socket (<code>DOCKER_HOST=unix://... docker compose up</code>) e confirma que funciona de ponta a
ponta contra o delonix.</p>""",
                "en": """<p>Point a real <code>docker-compose.yml</code> at the same socket
(<code>DOCKER_HOST=unix://... docker compose up</code>) and confirm it works end to end against
delonix.</p>"""},
    },
    "kube": {
        "lab": {"pt": """<p>Exporta um container já a correr localmente para um manifesto
Kubernetes pronto a aplicar.</p>
<pre><code>delonix container run -d --name web nginx
delonix cluster kube generate web > web-pod.yaml
cat web-pod.yaml</code></pre>""",
                "en": """<p>Export a container already running locally into a Kubernetes manifest
ready to apply.</p>
<pre><code>delonix container run -d --name web nginx
delonix cluster kube generate web > web-pod.yaml
cat web-pod.yaml</code></pre>"""},
        "challenge": {"pt": """<p>Aplica o <code>web-pod.yaml</code> gerado a um cluster real
criado com <code>delonix cluster kubeadm</code> e confirma com <code>kubectl get pods</code> que o
pod agenda e arranca.</p>""",
                "en": """<p>Apply the generated <code>web-pod.yaml</code> to a real cluster created
with <code>delonix cluster kubeadm</code> and confirm with <code>kubectl get pods</code> that the
pod schedules and starts.</p>"""},
    },
    "netns": {
        "lab": {"pt": """<p>Olha para dentro da infra de rede rootless que a maioria dos
utilizadores nunca precisa de tocar.</p>
<pre><code>delonix net netns status
delonix net netns exec -- ip addr show delonix0</code></pre>""",
                "en": """<p>Look inside the rootless network infrastructure most users never need
to touch.</p>
<pre><code>delonix net netns status
delonix net netns exec -- ip addr show delonix0</code></pre>"""},
        "challenge": {"pt": """<p>Desliga o holder com <code>net netns down</code>, depois corre
QUALQUER <code>container run</code> — confirma que ele reaparece sozinho (<code>ensure_up</code>) e
que <code>net netns status</code> volta a mostrar tudo saudável.</p>""",
                "en": """<p>Bring the holder down with <code>net netns down</code>, then run ANY
<code>container run</code> — confirm it comes back up by itself (<code>ensure_up</code>) and that
<code>net netns status</code> reports everything healthy again.</p>"""},
    },
    "completion": {
        "lab": {"pt": """<p>Autocompletion real, gerado a partir da MESMA definição da CLI —
nunca fica desactualizado à mão.</p>
<pre><code>echo 'source <(delonix completion bash)' >> ~/.bashrc
source ~/.bashrc
delonix con&lt;TAB&gt;</code></pre>""",
                "en": """<p>Real autocompletion, generated from the SAME CLI definition — it never
goes stale by hand.</p>
<pre><code>echo 'source <(delonix completion bash)' >> ~/.bashrc
source ~/.bashrc
delonix con&lt;TAB&gt;</code></pre>"""},
        "challenge": {"pt": """<p>Compara o output de <code>delonix completion bash</code> entre
duas versões do binário (antes/depois de um upgrade) — confirma que um comando novo aparece
sozinho no script, sem qualquer edição manual.</p>""",
                "en": """<p>Compare the output of <code>delonix completion bash</code> between two
binary versions (before/after an upgrade) — confirm a new command shows up in the script by
itself, with no manual editing.</p>"""},
    },
}

# Tradução EN das `notes` por-subcomando (autoral, técnicas) — chave
# (grupo, subcomando). Só 14 blocos no total; ver `group_page` para onde
# entram no render.
NOTES_EN = {
    ("container", "run"): """<p><strong><code>-p</code> and networking:</strong> with
<code>--net host</code> (the default) the container switches to its own netns with userspace NAT
(slirp4netns — the podman rootless model); with <code>--net &lt;network&gt;</code> the port is
published by the <em>ingress</em> (hostfwd on the single slirp + nft DNAT), the path that lets you
swap ports on the fly without stopping the container. <code>--net none</code> refuses
<code>-p</code>.</p>""",
    ("container", "start"): """<p>Reuses the saved spec (command, env, volumes, network, ports)
and the persistent rootfs — unlike <code>rm</code>+<code>run</code>, nothing the container wrote
is lost.</p>""",
    ("container", "stats"): """<p>CPU%/memory/PIDs read from the container's own cgroup v2
(resolved via <code>/proc/&lt;pid&gt;/cgroup</code>, whatever the delegated base is). Without
cgroup delegation (rootless with no <code>Delegate=yes</code>), memory falls back to the container
init's VmRSS, marked with <code>~</code>.</p>""",
    ("container", "kill"): """<p>Unlike <code>stop</code>, it doesn't wait for or force the state
— the real outcome (e.g. <code>Crashed</code> for a <code>KILL</code>) is only confirmed on the
next observation.</p>""",
    ("container", "wait"): """<p>The real exit code is only guaranteed when a
<code>--restart</code> supervisor is the process's real parent — a plain <code>-d</code> container
with no supervisor shows <code>Crashed</code>/137, a known architectural limit (the engine isn't
that process's real parent).</p>""",
    ("container", "update"): """<p>Reconfigures a <strong>running</strong> container's ports,
volumes, networks, bandwidth limit and memory/CPU limits without stopping it — the PID doesn't
change. Removals run before additions, so <code>--publish-rm 8080 --publish-add 8080:9000</code>
works in a single command. <code>--memory</code>/<code>--cpus</code> rewrite the real cgroup
immediately (<code>memory.max</code>/<code>cpu.max</code>) — no waiting for a
<code>restart</code>.</p>""",
    ("container", "attach"): """<p>Deliberately <strong>output-only</strong> — unlike
<code>docker attach</code>, there's no live stdin to an already-started detached container (no
persistent per-container shim). <code>-i</code>/<code>--stdin</code> is refused with a clear error
pointing at <code>exec -it</code>.</p>""",
    ("workload", "ls"): """<p><strong>Exact-name routing</strong>, fail-closed: a name that
doesn't exist gives <code>no such workload</code>; a container AND a vm with the same name give
<code>ambiguous</code> (points at the specific command, never guesses).</p>""",
    ("pod", "create"): """<p>Idempotent (<em>ensure-present</em>): if the pod already has
containers, it does nothing. Can also be applied via <code>delonix stack apply</code> (the
<code>pods:</code> group in <code>kind: Stack</code>) and previewed with <code>--dry-run</code>.
If creating one member fails, the whole pod is torn down (no half-pod).</p>""",
    ("stack", "init"): """<p><code>--template &lt;name&gt;</code> generates a real, functional
project for a language/framework, with best practices (non-root multi-stage, healthcheck, tests,
dotfiles) and already delonix-native (Delonixfile + manifest). Without <code>--template</code>,
<code>init</code> generates the generic scaffold. The <code>__NAME__</code>/<code>__MODULE__</code>
tokens get replaced with the project name.</p>""",
    ("cluster", "apply"): """<p>Every manifest field that reaches remote commands
(<code>controlPlaneEndpoint</code>, subnets, version) goes through strict validation before any
interpolation — command injection via the manifest was one of the CRITICAL findings closed in the
project's offensive audit, with tests replicating the exploit.</p>""",
    ("cluster", "kubeadm"): """<p><code>--control-plane &gt; 1</code> automatically provisions an
extra VM running HAProxy (L4, passthrough — the apiserver's TLS always terminates on the real
control-plane) in front of port 6443 on each control-plane, and uses it as
<code>controlPlaneEndpoint</code> — no new flag, it triggers on its own from the number of
control-planes requested. <code>--name</code> is optional (generates a free name in the same
pattern as containers); without <code>--vm-image</code>, it resolves the single local golden VM
image or downloads it from the official repository automatically. Step-by-step progress,
<code>kind create cluster</code>-style (each step closes with ✓/✗), degrading to one line per step
with no TTY (pipes/CI).</p>""",
    ("egress", "host"): """<p>What nft/CIDR can't do: allowlisting by <strong>hostname</strong>.
The internal ingress DNS resolver starts snooping the A-records in responses and injects them into
a per-network nft <code>set</code> (with a timeout = expires with the TTL); egress accepts that
set plus DNS and drops the rest. 100% rootless (no eBPF) — Cilium's FQDN policy, via nftables.
Repeatable for several hostnames.</p>""",
    ("tunnel", "expose"): """<p>Validated live in this very session: real HTTPS traffic from the
internet reached a local server through the tunnel (HTTP 200) using exactly this command.</p>""",
}

# Tradução EN das legendas de exemplos (`examples_html`'s `captions_en`) —
# chave (grupo, subcomando), com sub=None para os exemplos ao nível do
# grupo. Cada lista tem de bater exactamente com o número/ordem de
# `GROUPS[grupo]["examples"]` ou `GROUPS[grupo]["subs"][sub]["examples"]` —
# uma legenda vazia ("") fica vazia nas duas línguas (sem legenda nenhuma).
EXAMPLES_EN = {
    ("container", "dash"): ["Containers-only dashboard"],
    ("container", "run"): [
        "Serve nginx on host port 8080 (userspace NAT, no root)",
        "Run on a user-created network, published via ingress",
        "Disposable shell (removes itself on exit)",
        "Override ENTRYPOINT to debug an image",
    ],
    ("container", "ps"): ["List (the `ls` alias also works)", "Compose with stop/rm"],
    ("container", "start"): ["Restart a stopped container, preserving what was written inside"],
    ("container", "stop"): ["SIGTERM, then SIGKILL after 5s"],
    ("container", "rm"): ["Force-remove several"],
    ("container", "exec"): ["Interactive shell"],
    ("container", "logs"): ["Follow continuously (exits when the container stops)"],
    ("container", "inspect"): ["Full spec as JSON"],
    ("container", "stats"): ["One sample of everything running"],
    ("container", "apply"): ["Apply just the `kind: Container` entries from a manifest"],
    ("container", "init"): ["Scaffold a complete, ready-to-use project"],
    ("container", "kill"): ["Arbitrary signal, without forcing `Stopped`"],
    ("container", "wait"): ["Blocks until it exits, prints the exit code"],
    ("container", "restart"): ["Stop and start again, same configuration"],
    ("container", "rename"): [""],
    ("container", "port"): ["This container's published ports"],
    ("container", "pause"): ["Suspends the processes (cgroup v2 freezer)"],
    ("container", "unpause"): ["Resumes a suspended container"],
    ("container", "commit"): ["Creates an image from the container's current rootfs"],
    ("container", "ssh"): ["Shortcut for `exec -t` — tries bash, falls back to sh"],
    ("container", "healthcheck"): ["Runs the image's HEALTHCHECK, exit 1 if unhealthy (usable in CI)"],
    ("container", "top"): ["Processes running inside the container"],
    ("container", "diff"): ["Files changed relative to the image (A/D)"],
    ("container", "cp"): ["From the container to the host", "From the host to the container"],
    ("container", "describe"): ["`kubectl describe`-style detail (for humans; `inspect` is for scripts)"],
    ("container", "update"): [
        "Hot-swap a port, no restart",
        "Connect to a new network + bandwidth limit",
        "Raise the memory/CPU limit on the fly, no restart",
    ],
    ("container", "attach"): ["Reconnect to a detached container's output stream"],
    ("workload", "ls"): [
        "Containers AND VMs in one table",
        "Structured output for automation (stable keys, language-independent)",
    ],
    ("workload", "describe"): ["Workload detail, auto-routed to the right engine"],
    ("workload", "stop"): ["Stop by name, whether container or vm"],
    ("workload", "rm"): ["Remove by name"],
    ("pod", "create"): ["Create a pod (web + sidecar talking over localhost) from a manifest"],
    ("pod", "ls"): ["List pods (POD, CONTAINERS n/N, IP, STATUS)"],
    ("pod", "describe"): ["kubectl-style detail: containers + shared IP and netns"],
    ("pod", "rm"): [
        "Remove the pod: stops/removes ALL containers + the shared netns",
        "Force (kills the ones still running)",
    ],
    ("pod", "logs"): [
        "Logs from the pod's 1st container",
        "Logs from a specific container (short name inside the pod)",
    ],
    ("image", "init"): ["Scaffold a VMfile (equivalent to vm init --vmfile)"],
    ("image", "vm"): ["The same VM image group, via another path"],
    ("image", "logout"): ["Forget that registry's credentials"],
    ("image", "login"): ["Authenticate to a registry (the password comes from stdin, out of history)"],
    ("image", "load"): ["Import that tar on the other end"],
    ("image", "save"): ["Export to a tar (to carry to a machine with no network)"],
    ("image", "scan"): ["Look for known vulnerabilities in an image", "Scan every local image"],
    ("image", "verify"): ["Confirm the signature against a public key"],
    ("image", "history"): ["Which instruction created each layer"],
    ("image", "tag"): ["Give the same image a second name (copies nothing)"],
    ("image", "describe"): ["An image's layers, config and digest"],
    ("image", "ls-remote"): ["Tags published in a repository, without pulling anything"],
    ("image", "dash"): ["Images-only dashboard"],
    ("image", "pull"): ["Reference with tag and digest (combined format supported)"],
    ("image", "ls"): [""],
    ("image", "rm"): [""],
    ("image", "export"): ["OCI runtime bundle, to run with runc/crun"],
    ("image", "push"): ["Publish the golden VM image as an OCI artifact (ORAS pattern)"],
    ("image", "build"): ["Build the golden VM image (downloads Ubuntu, verifies SHA256SUMS, virt-customize)"],
    ("image", "apply"): [""],
    ("build", None): ["Build with a tag", "Explicit Delonixfile"],
    ("vm", "snapshots"): ["List a VM's checkpoints"],
    ("vm", "restore"): ["Roll back to the checkpoint"],
    ("vm", "snapshot"): ["System checkpoint (memory + disk) of a RUNNING VM"],
    ("vm", "restart"): ["Forced restart (stops and boots again)"],
    ("vm", "start"): ["Boot a stopped VM again, without repeating the create flags"],
    ("vm", "describe"): ["Everything about a VM, kubectl describe-style"],
    ("vm", "unbridge"): ["Close the VM↔container bridge"],
    ("vm", "bridge"): [
        "See the plan WITHOUT applying it (dry-run is the default)",
        "Actually apply it — needs root, the deliberate exception to rootless",
    ],
    ("vm", "reach"): ["Which container ports VMs can reach"],
    ("vm", "vnc"): ["Open the VM's graphical screen"],
    ("vm", "console"): ["Serial console (back to the host: Ctrl+])"],
    ("vm", "push"): ["Publish your image as an OCI artifact"],
    ("vm", "ls-remote"): ["Which versions are published, before pulling", "The tags of a repository of yours"],
    ("vm", "pull"): [
        "The official golden image with Kubernetes (no argument)",
        "The golden image WITHOUT Kubernetes — just the engine, rootless-ready",
        "From a registry of yours, with your own local name",
    ],
    ("vm", "build"): [
        "Build from the current directory's VMfile",
        "VMfile at another path, no compression (faster build, larger image)",
        "With guest networking — needed for `apt-get install` in a RUN (the build stops being "
        "reproducible: the result now depends on the day)",
    ],
    ("vm", "init"): ["Project with a manifest, ready to run", "Scaffold a VMfile to BUILD your image"],
    ("vm", "dash"): ["VMs-only dashboard (htop-style; `q` to quit)", "Snapshot for a script or for Grafana"],
    ("vm", "create"): ["VM from the golden image, with an SSH key"],
    ("vm", "ls"): [""],
    ("vm", "status"): ["Reconciles liveness/IP with the backend"],
    ("vm", "stop"): [""],
    ("vm", "rm"): [""],
    ("vm", "apply"): [""],
    ("volumes", "snapshot"): ["Take and list a volume's snapshots"],
    ("volumes", "describe"): ["Volume detail (usage, quota, mounts)"],
    ("volumes", "create"): ["With quota and the nfs driver available"],
    ("volumes", "ls"): [""],
    ("volumes", "inspect"): [""],
    ("volumes", "rm"): [""],
    ("volumes", "apply"): [""],
    ("network", "describe"): ["Network detail, kubectl-style"],
    ("network", "node"): ["Manage nodes of an overlay network between machines"],
    ("network", "dash"): ["Networks-only dashboard"],
    ("network", "create"): ["Bridge network for a group of services", "Encrypted overlay between nodes (VXLAN + WireGuard)"],
    ("network", "ls"): [""],
    ("network", "inspect"): [""],
    ("network", "rm"): [""],
    ("network", "apply"): [""],
    ("stack", "validate"): ["Validate the manifest WITHOUT applying anything"],
    ("stack", "describe"): ["Resource-by-resource state, checked against the manifest"],
    ("stack", "ls"): ["Which manifest resources actually exist on this host"],
    ("stack", "init"): [
        "COMPLETE stack project (FastAPI): code + Delonixfile + manifest + tests",
        "See the available templates",
    ],
    ("stack", "apply"): ["Apply the default manifest (./delonix-manifest.yaml)", "Explicit manifest"],
    ("compose", "up"): [
        "Bring everything up (build, network, volumes, containers, in `depends_on` order)",
        "Explicit file/project",
        "Just validate and show the plan, without creating anything",
    ],
    ("compose", "ps"): ["This project's containers"],
    ("compose", "logs"): ["One service's logs", "All services, one after another"],
    ("compose", "config"): ["Validates and prints the resolved project (equivalent to `docker compose config`)"],
    ("compose", "down"): ["Removes this project's containers", "Also removes NAMED volumes (never `external: true` ones)"],
    ("cluster", "kube"): ["Generate Kubernetes manifests from a Delonix resource"],
    ("cluster", "load"): ["Get a local image into the nodes (kind load, no registry)"],
    ("cluster", "delete"): ["Delete the cluster and its nodes"],
    ("cluster", "ls"): ["Which clusters exist on this host"],
    ("cluster", "create"): ["Local cluster in kind mode (containers as nodes, no Docker)", "With workers"],
    ("cluster", "init"): ["Scaffold a cloud.yaml for cluster apply"],
    ("cluster", "apply"): ["Bootstrap from a `kind: Cluster` manifest"],
    ("cluster", "kubeadm"): [
        "From scratch: 1 control-plane + 2 workers",
        "HA: 2 control-planes + 3 workers (automatic HAProxy)",
        "Dedicated external etcd (3 extra VMs, odd quorum)",
    ],
    ("secret", "apply"): ["Create secrets from a kind: Secret manifest"],
    ("secret", "rm"): ["Delete the whole secret"],
    ("secret", "unset"): ["Remove ONE key (without deleting the secret)"],
    ("secret", "set"): ["Add/update keys on an existing secret"],
    ("secret", "create"): ["Create a secret (value via stdin, not in argv)"],
    ("secret", "ls"): ["List (values redacted)"],
    ("secret", "inspect"): ["Reveal explicitly"],
    ("secret", "rotate-key"): ["Rotate the master key (re-encrypts everything)"],
    ("storage", "apply"): ["Declare the NAS in a kind: Storage manifest"],
    ("storage", "inspect"): ["Storage as JSON (for scripts)"],
    ("storage", "dash"): ["Network storage-only dashboard"],
    ("storage", "create"): ["NFS from a TrueNAS", "SMB/CIFS with the password from the vault"],
    ("storage", "ls"): [""],
    ("storage", "rm"): ["Unmounts; the data stays on the NAS"],
    ("sharevolume", "apply"): ["Two isolated slices of the same NAS, each with its own quota"],
    ("sharevolume", "ls"): ["List (quota + measured real usage)"],
    ("sharevolume", "describe"): ["Slice detail (points at the -v command to consume it)"],
    ("sharevolume", "rm"): ["Removes the record; the DATA stays (unless you ask for --purge-data)"],
    ("ingress", "clear"): ["Clear that container's whole firewall"],
    ("ingress", "rm"): ["Remove ONE rule, without clearing the others"],
    ("ingress", "unpublish"): ["Stop publishing a port"],
    ("ingress", "allow"): ["Only let Postgres in from the SDN itself"],
    ("ingress", "deny"): ["Block a specific port"],
    ("ingress", "policy"): ["Default-deny (allowlist)"],
    ("ingress", "publish"): ["Publish a port via ingress (DNAT)"],
    ("ingress", "ls"): ["See rules + publishes"],
    ("egress", "clear"): ["Clear that container's outbound rules"],
    ("egress", "rm"): ["Remove ONE outbound rule"],
    ("egress", "deny"): ["Block outbound traffic to a network"],
    ("egress", "allow"): ["Only let HTTPS out"],
    ("egress", "policy"): ["Default-deny outbound"],
    ("egress", "net"): ["Network egress in allowlist mode (DNS + these CIDRs only)"],
    ("egress", "host"): ["Only let traffic out to GitHub (and *.github.com), learned from DNS"],
    ("egress", "show"): ["See a network's egress policy + the FQDN IPs learned live"],
    ("egress", "ls"): [""],
    ("httproute", "apply"): ["Apply the HTTPRoutes from a manifest (brings up/reloads the proxy)"],
    ("httproute", "ls"): ["Proxy status + active routes"],
    ("httproute", "rm"): ["Stop the proxy and unpublish the ports"],
    ("tunnel", "apply"): ["Declare the tunnel in a kind: Tunnel manifest"],
    ("tunnel", "expose"): ["Expose a local port with no manifest (pinggy, free, ephemeral)"],
    ("tunnel", "ls"): ["List tunnels (status + public URL)"],
    ("tunnel", "describe"): ["Tunnel detail"],
    ("tunnel", "rm"): ["Stop and remove (really kills the agent process)"],
    ("flow", None): ["One sample", "Continuous monitoring"],
    ("boot", "enable"): ["Persist the ones running now"],
    ("boot", "status"): ["See what's installed"],
    ("boot", "disable"): ["Remove the boot units"],
    ("system", "thermal"): ["Host temperature and throttling"],
    ("system", "monitor"): ["Follow events live"],
    ("system", "virt"): ["What the host offers for virtualization, and what's left to tune", "Apply the recommended tuning"],
    ("system", "setup"): [
        "Diagnose cgroup delegation — why -m/--cpus aren't taking effect",
        "The 1st fix: a delegated scope (no root, no reboot, works right away)",
        "Only if the one above still says `cpu` is missing (needs root, survives a reboot)",
    ],
    ("system", "events"): ["See what the engine has been doing", "Follow in real time (Ctrl-C to exit)"],
    ("system", "prune"): ["Reclaim space (GC)"],
    ("system", "df"): ["Disk usage"],
    ("system", "info"): [""],
    ("dash", None): ["Interactive TUI", "One-off snapshot, for a script", "JSON, for a datasource or pipeline"],
    ("docker-api", None): [
        "Serve on the default socket",
        "A real `docker` talking to delonix",
        "`docker compose up` pointed at delonix",
    ],
    ("kube", "generate"): ["Pod from a container"],
    ("netns", "firewall"): ["Apply a container's firewall on the ingress"],
    ("netns", "unpublish"): ["Unpublish"],
    ("netns", "publish"): ["Publish a port via ingress, by hand"],
    ("netns", "exec"): ["Run a command INSIDE an attached netns — for debugging the network"],
    ("netns", "detach"): ["Detach and destroy that netns"],
    ("netns", "attach"): ["Attach a netns to the bridge (the engine does this on its own during run)"],
    ("netns", "down"): ["Tear down the network infra (kills slirp + holder) — brings down EVERY container on the SDN"],
    ("netns", "status"): ["Ingress infra status"],
    ("netns", "up"): ["Bring up the ingress holder"],
    ("completion", None): ["Bash (persistent)", "Zsh"],
}

# ---------------------------------------------------------------- template

CSS = """
:root{--accent:#e8590c;--accent-soft:#fff0e6;--ink:#1a1a2e;--muted:#5a6472;--line:#e6e8ec;
--bg:#ffffff;--side:#f7f8fa;--code-bg:#0f172a;--code-ink:#e2e8f0;--radius:10px;
--tok-key:#0550ae;--tok-string:#116329;--tok-comment:#6e7781;--tok-flag:#953800;
--tok-cmd:#8250df;--tok-ph:#6e7781;--tok-head:#c2410c;--tok-sep:#6e7781}
@media (prefers-color-scheme: dark){:root:not([data-theme="light"]){--ink:#e6e8ee;--muted:#9aa4b2;--line:#252a33;
--bg:#0d1117;--side:#10151c;--accent-soft:#2a1810;--code-bg:#161b22;--code-ink:#dbe2ea;
--tok-key:#79c0ff;--tok-string:#7ee787;--tok-comment:#8b949e;--tok-flag:#ffa657;
--tok-cmd:#d2a8ff;--tok-ph:#8b949e;--tok-head:#ff9a5a;--tok-sep:#8b949e}}
:root[data-theme="dark"]{--ink:#e6e8ee;--muted:#9aa4b2;--line:#252a33;
--bg:#0d1117;--side:#10151c;--accent-soft:#2a1810;--code-bg:#161b22;--code-ink:#dbe2ea;
--tok-key:#79c0ff;--tok-string:#7ee787;--tok-comment:#8b949e;--tok-flag:#ffa657;
--tok-cmd:#d2a8ff;--tok-ph:#8b949e;--tok-head:#ff9a5a;--tok-sep:#8b949e}
*{box-sizing:border-box}body{margin:0;font:16px/1.65 -apple-system,'Segoe UI',Roboto,Ubuntu,
sans-serif;color:var(--ink);background:var(--bg)}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
.layout{display:grid;grid-template-columns:270px minmax(0,1fr) 240px;max-width:1600px;
margin:0 auto;min-height:100vh}
nav.side{width:270px;background:var(--side);border-right:1px solid var(--line);
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
nav.side .brand .toggles{display:flex;align-items:center;gap:.4rem;margin-left:auto}
.theme-toggle{appearance:none;border:1px solid var(--line);background:var(--bg);color:var(--ink);
border-radius:8px;width:30px;height:30px;flex-shrink:0;display:inline-flex;align-items:center;
justify-content:center;cursor:pointer;font-size:.95rem;line-height:1}
.theme-toggle:hover{border-color:var(--accent)}
.theme-toggle .i-sun{display:none}
:root[data-theme="dark"] .theme-toggle .i-moon{display:none}
:root[data-theme="dark"] .theme-toggle .i-sun{display:inline}
@media (prefers-color-scheme: dark){:root:not([data-theme="light"]) .theme-toggle .i-moon{display:none}
:root:not([data-theme="light"]) .theme-toggle .i-sun{display:inline}}
.lang-toggle{appearance:none;border:1px solid var(--line);background:var(--bg);color:var(--ink);
border-radius:8px;height:30px;padding:0 .55rem;flex-shrink:0;display:inline-flex;align-items:center;
justify-content:center;cursor:pointer;font-size:.72rem;font-weight:700;letter-spacing:.02em;line-height:1}
.lang-toggle:hover{border-color:var(--accent)}
.lang-toggle .lbl-pt{display:none}
:root[data-lang="en"] .lang-toggle .lbl-en{display:none}
:root[data-lang="en"] .lang-toggle .lbl-pt{display:inline}
.lang-en{display:none}
:root[data-lang="en"] .lang-en{display:inline}
:root[data-lang="en"] .lang-pt{display:none}
p.lang-en,p.lang-pt,div.lang-en,div.lang-pt{display:none}
:root[data-lang="en"] p.lang-en,:root[data-lang="en"] div.lang-en{display:block}
:root:not([data-lang="en"]) p.lang-pt,:root:not([data-lang="en"]) div.lang-pt{display:block}
main{min-width:0;padding:2.2rem 3rem 5rem;max-width:860px;margin:0 auto}
main h1{font-size:1.9rem;margin:.2rem 0 .4rem}
main h2{font-size:1.35rem;margin-top:2.4rem;padding-bottom:.35rem;border-bottom:1px solid var(--line)}
main h3{font-size:1.05rem;margin-top:1.6rem}
.tagline{color:var(--muted);font-size:1.05rem;margin-top:0}
p.intro{font-size:1.02rem;margin:1rem 0}
p.src-link{margin:.7rem 0 0;font-size:.85rem;color:var(--muted)}
p.src-link a{font-weight:600}
code{background:var(--accent-soft);padding:.1em .35em;border-radius:5px;font-size:.9em}
pre{background:var(--code-bg);color:var(--code-ink);padding:1rem 1.2rem;border-radius:var(--radius);
overflow-x:auto;font-size:.86rem;line-height:1.55;white-space:pre-wrap;word-break:break-word}
pre code{background:none;padding:0;color:inherit;font-size:inherit}
.help pre{border-left:4px solid var(--accent)}
.tok-key{color:var(--tok-key)}.tok-string{color:var(--tok-string)}
.tok-comment{color:var(--tok-comment);font-style:italic}
.tok-flag{color:var(--tok-flag)}.tok-cmd{color:var(--tok-cmd);font-weight:600}
.tok-ph{color:var(--tok-ph);font-style:italic}.tok-head{color:var(--tok-head);font-weight:700}
.tok-sep{color:var(--tok-sep)}
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
aside.toc{padding:2.2rem 1.2rem 3rem 0;position:sticky;top:0;height:100vh;overflow-y:auto;font-size:.82rem}
aside.toc .toc-inner{border-left:1px solid var(--line);padding-left:1rem}
aside.toc h5{margin:0 0 .5rem;font-size:.72rem;letter-spacing:.09em;text-transform:uppercase;color:var(--muted)}
aside.toc a{display:block;padding:.2rem 0;color:var(--muted);line-height:1.4}
aside.toc a:hover{color:var(--accent);text-decoration:none}
@media (max-width:1200px){.layout{grid-template-columns:270px minmax(0,1fr)}aside.toc{display:none}}
@media (max-width:840px){.layout{display:block}nav.side{width:100%;height:auto;position:static}
main{padding:1.4rem 1.2rem 4rem;max-width:none}}
"""


def sidebar(active, depth=0):
    p = "../" * depth
    # (href, rótulo PT, rótulo EN) — as páginas ligadas continuam PT-only por
    # agora (ver relatório de âmbito), mas o rótulo do menu já traduz.
    items_docs = [
        ("index.html", "Início", "Home"),
        ("cheatsheet.html", "Cheatsheet", "Cheatsheet"),
        ("kinds.html", "Kinds e templates", "Kinds & templates"),
        ("cloud.html", "cloud-init, cloud-img e CH", "cloud-init, cloud-img & CH"),
        ("labs.html", "Laboratórios", "Labs"),
        ("arquitectura.html", "Arquitectura", "Architecture"),
        ("c4.html", "Modelo C4 e system design", "C4 model & system design"),
        ("cri.html", "CRI — kubelet sem containerd", "CRI — kubelet without containerd"),
        ("comparacao.html", "Delonix vs Docker vs Podman", "Delonix vs Docker vs Podman"),
        ("tutorial-delonix-temp.html", "Projecto completo: Delonix Temp", "Full project: Delonix Temp"),
    ]
    items_cmd = [(f"comandos/{g}.html", GROUPS[g]["title"]) for g in GROUPS]
    def link_bi(href, pt, en):
        cls = ' class="on"' if href == active else ""
        return (
            f'<a href="{p}{href}"{cls}>'
            f"<span class='lang-pt'>{html.escape(pt)}</span>"
            f"<span class='lang-en'>{html.escape(en)}</span></a>"
        )
    def link(href, label):
        cls = ' class="on"' if href == active else ""
        return f'<a href="{p}{href}"{cls}>{html.escape(label)}</a>'
    return f"""<nav class="side">
<div class="brand"><span class="dot">▲</span> Delonix Engine
<div class="toggles">
<button class="lang-toggle" type="button" aria-label="Switch language / Mudar idioma" title="EN / PT">
<span class="lbl-en">EN</span><span class="lbl-pt">PT</span></button>
<button class="theme-toggle" type="button" aria-label="Alternar tema claro/escuro" title="Tema claro/escuro">
<span class="i-moon">🌙</span><span class="i-sun">☀️</span></button>
</div></div>
<h5>{bi('span', 'Documentação', 'Docs')}</h5>
{''.join(link_bi(h, pt, en) for h, pt, en in items_docs)}
<h5>{bi('span', 'Referência CLI', 'CLI reference')}</h5>
{''.join(link(h, l) for h, l in items_cmd)}
<h5>{bi('span', 'Projecto', 'Project')}</h5>
<a href="https://github.com/angolardevops/delonix-runtime">GitHub</a>
<a href="https://github.com/angolardevops/delonix-runtime/releases">Releases</a>
</nav>"""


# Script anti-FOUC: corre em <head>, SÍNCRONO, antes de qualquer pintura —
# só lê a escolha manual do leitor (`localStorage`) e marca `data-theme` no
# <html> logo de início. Sem isto, a página pintava sempre no tema do SO por
# uma fracção de segundo antes do JS do fim da página a corrigir.
THEME_INIT_JS = """<script>(function(){try{
var t=localStorage.getItem('delonix-docs-theme');
if(t)document.documentElement.setAttribute('data-theme',t);
var l=localStorage.getItem('delonix-docs-lang');
if(l)document.documentElement.setAttribute('data-lang',l);
}catch(e){}})();</script>"""

# JS partilhado por todas as páginas: toggle de tema (persistido), índice
# "Nesta página" gerado a partir dos <h2> reais, e um highlighter leve
# (bash/YAML/`--help`) — tudo vendored, sem dependência externa nenhuma,
# coerente com o resto do site ser 100% auto-contido.
SITE_JS = """<script>(function(){
function esc(s){return s.replace(/[&<>]/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;'}[c];});}

function tokBash(line){
  var out='',last=0,m;
  var re=/(#.*$)|("(?:[^"\\\\]|\\\\.)*")|('(?:[^'\\\\]|\\\\.)*')|(--?[A-Za-z][\\w-]*)|\\b(delonix|delonixctl|dlx)\\b/g;
  while((m=re.exec(line))){
    out+=esc(line.slice(last,m.index));
    if(m[1])out+='<span class="tok-comment">'+esc(m[1])+'</span>';
    else if(m[2]||m[3])out+='<span class="tok-string">'+esc(m[0])+'</span>';
    else if(m[4])out+='<span class="tok-flag">'+esc(m[0])+'</span>';
    else if(m[5])out+='<span class="tok-cmd">'+esc(m[0])+'</span>';
    last=re.lastIndex;
  }
  return out+esc(line.slice(last));
}
function highlightBash(text){return text.split('\\n').map(tokBash).join('\\n');}

function highlightFlagsPh(line){
  var out='',last=0,m;
  var re=/(--?[A-Za-z][\\w-]*)|(<[^>]+>)|(\\[[^\\]]+\\])/g;
  while((m=re.exec(line))){
    out+=esc(line.slice(last,m.index));
    if(m[1])out+='<span class="tok-flag">'+esc(m[0])+'</span>';
    else out+='<span class="tok-ph">'+esc(m[0])+'</span>';
    last=re.lastIndex;
  }
  return out+esc(line.slice(last));
}
function highlightHelp(text){
  return text.split('\\n').map(function(line){
    var mm=line.match(/^(Usage|Commands|Options|Arguments|Examples):(.*)$/);
    if(mm)return '<span class="tok-head">'+mm[1]+':</span>'+highlightFlagsPh(mm[2]);
    return highlightFlagsPh(line);
  }).join('\\n');
}
function highlightYaml(text){
  return text.split('\\n').map(function(line){
    if(/^\\s*#/.test(line))return '<span class="tok-comment">'+esc(line)+'</span>';
    if(/^---\\s*$/.test(line))return '<span class="tok-sep">'+esc(line)+'</span>';
    var m=line.match(/^(\\s*(?:- )?)([A-Za-z0-9_.-]+)(:)(.*)$/);
    if(m){
      var rest=m[4],restHtml=esc(rest);
      var sm=rest.match(/^(\\s*)("(?:[^"\\\\]|\\\\.)*"|'(?:[^'\\\\]|\\\\.)*'|[^\\s#][^#]*?)(\\s*(#.*)?)$/);
      if(sm&&sm[2])restHtml=esc(sm[1])+'<span class="tok-string">'+esc(sm[2])+'</span>'+esc(sm[3]||'');
      return esc(m[1])+'<span class="tok-key">'+esc(m[2])+'</span>'+esc(m[3])+restHtml;
    }
    return esc(line);
  }).join('\\n');
}

function highlightAll(){
  document.querySelectorAll('pre > code').forEach(function(code){
    if(code.dataset.hl)return;
    code.dataset.hl='1';
    var pre=code.parentElement;
    if(pre.closest('.out'))return;
    var text=code.textContent,html;
    var isHelp=pre.parentElement&&pre.parentElement.classList.contains('help');
    if(isHelp)html=highlightHelp(text);
    else if(/^apiVersion:|\\nkind:\\s/.test(text))html=highlightYaml(text);
    else if(pre.closest('.ex')||/^(delonix|delonixctl|dlx)\\b/.test(text.trim()))html=highlightBash(text);
    else return;
    code.innerHTML=html;
  });
}

function visibleHeadingText(h){
  // A heading pode conter os dois <span class="lang-pt/lang-en"> do bi() —
  // h.textContent junta os dois, mesmo com um deles escondido por CSS. Usa
  // só o do idioma activo à data da construção do índice.
  var lang=document.documentElement.getAttribute('data-lang')||'pt';
  var want=lang==='en'?'lang-en':'lang-pt';
  var pick=h.querySelector(':scope > .'+want);
  return (pick?pick.textContent:h.textContent).trim();
}
function isVisibleForLang(el,lang){
  // Páginas como o index duplicam a secção INTEIRA por idioma (dois <h2>
  // separados, um por língua) em vez de um <h2> só com dois spans — sem
  // este filtro o índice listava as duas línguas ao mesmo tempo.
  var wrap=el.closest('.lang-pt, .lang-en');
  if(!wrap)return true;
  return lang==='en'?wrap.classList.contains('lang-en'):wrap.classList.contains('lang-pt');
}
function buildToc(){
  var main=document.querySelector('main'),nav=document.getElementById('toc-nav'),
      aside=document.getElementById('pagetoc');
  if(!main||!nav)return;
  var lang=document.documentElement.getAttribute('data-lang')||'pt';
  var heads=[].slice.call(main.querySelectorAll('h2')).filter(function(h){
    return isVisibleForLang(h,lang);
  });
  nav.innerHTML='';
  if(heads.length<2){if(aside)aside.style.display='none';return;}
  if(aside)aside.style.display='';
  var used={};
  heads.forEach(function(h){
    var label=visibleHeadingText(h);
    if(!h.id){
      var slug=label.toLowerCase().replace(/[^a-z0-9]+/g,'-').replace(/(^-|-$)/g,'')||'sec';
      while(used[slug])slug+='-x';
      used[slug]=1;h.id=slug;
    }
    var a=document.createElement('a');
    a.href='#'+h.id;a.textContent=label;
    nav.appendChild(a);
  });
}

function initThemeToggle(){
  var KEY='delonix-docs-theme',btn=document.querySelector('.theme-toggle');
  if(!btn)return;
  btn.addEventListener('click',function(){
    var sys=matchMedia('(prefers-color-scheme: dark)').matches?'dark':'light';
    var eff=document.documentElement.getAttribute('data-theme')||sys;
    var next=eff==='dark'?'light':'dark';
    document.documentElement.setAttribute('data-theme',next);
    try{localStorage.setItem(KEY,next);}catch(e){}
  });
}

function initLangToggle(){
  var KEY='delonix-docs-lang',btn=document.querySelector('.lang-toggle');
  if(!btn)return;
  btn.addEventListener('click',function(){
    var eff=document.documentElement.getAttribute('data-lang')||'pt';
    var next=eff==='en'?'pt':'en';
    document.documentElement.setAttribute('data-lang',next);
    try{localStorage.setItem(KEY,next);}catch(e){}
    buildToc();
  });
}

document.addEventListener('DOMContentLoaded',function(){
  initThemeToggle();initLangToggle();buildToc();highlightAll();
});
})();</script>"""


FOOTER_PT = ('Delonix Engine · Apache-2.0 · '
             '<a href="https://github.com/angolardevops/delonix-runtime">angolardevops/delonix-runtime</a>'
             ' · Referência gerada do <code>--help</code> real do binário por <code>docs/gen.py</code>.')
FOOTER_EN = ('Delonix Engine · Apache-2.0 · '
             '<a href="https://github.com/angolardevops/delonix-runtime">angolardevops/delonix-runtime</a>'
             ' · Reference generated from the real <code>--help</code> of the binary by <code>docs/gen.py</code>.')
FOOTER_HTML = bi("span", FOOTER_PT, FOOTER_EN)
TOC_HEADING_HTML = bi("span", "Nesta página", "On this page")


def page(path, title, body, depth=0):
    doc = f"""<!DOCTYPE html>
<html lang="pt">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{html.escape(title)} · Delonix Engine</title>
{THEME_INIT_JS}
<style>{CSS}</style>
</head>
<body>
<div class="layout">
{sidebar(path, depth)}
<main>
{body}
<footer>{FOOTER_HTML}</footer>
</main>
<aside class="toc" id="pagetoc"><div class="toc-inner"><h5>{TOC_HEADING_HTML}</h5><nav id="toc-nav"></nav></div></aside>
</div>
{SITE_JS}
</body>
</html>"""
    out = os.path.join(ROOT, path)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as f:
        f.write(doc)


def examples_html(exs, captions_en=None):
    """Each example is (caption, command) or (caption, command, output) — the
    3rd, optional element is REAL output captured from an actual run (never
    invented), rendered in a dimmer block right under the command so a reader
    sees not just how to invoke something but what it actually returns.

    `captions_en`, when given, is an EN caption per example (same order) —
    the caption renders bilingually via `bi()`; without it, the caption
    stays PT-only (unaffected, the default for the ~200 examples not yet
    translated)."""
    parts = []
    for i, ex in enumerate(exs):
        cap, cmd = ex[0], ex[1]
        out = ex[2] if len(ex) > 2 else None
        cap_en = captions_en[i] if captions_en else None
        if cap and cap_en:
            cap_html = bi("div", html.escape(cap), html.escape(cap_en), cls="cap")
        elif cap:
            cap_html = f'<div class="cap">{html.escape(cap)}</div>'
        else:
            cap_html = ""
        out_html = f'<div class="out"><pre><code>{html.escape(out)}</code></pre></div>' if out else ""
        parts.append(
            f'<div class="ex">{cap_html}<pre><code>{html.escape(cmd)}</code></pre>{out_html}</div>'
        )
    return "".join(parts)


def group_page(name, g):
    en = GROUPS_EN.get(name, {})
    body = [f"<h1>{html.escape(g['title'])}</h1>"]
    if en.get("tagline"):
        body.append(bi("p", html.escape(g["tagline"]), html.escape(en["tagline"]), cls="tagline"))
    else:
        body.append(f"<p class='tagline'>{html.escape(g['tagline'])}</p>")
    if en.get("intro"):
        body.append(bi("p", g["intro"], en["intro"]))
    else:
        body.append(f"<p>{g['intro']}</p>")
    body.append(source_link_html(name))
    top_help = help_of(*group_argv(name))
    # A intro já vem de `g['intro']` (prosa autoral) — o `about` do clap aqui
    # seria quase sempre uma repetição em EN da mesma frase, por isso só se
    # descarta (nunca se duplica), mantendo o bloco de código a começar já
    # em "Usage:".
    _, top_rest = split_help_intro(top_help)
    body.append(f"<div class='help'><pre><code>{html.escape(top_rest)}</code></pre></div>")
    if g.get("examples"):
        body.append(f"<h2>{bi('span', 'Exemplos', 'Examples')}</h2>" + examples_html(g["examples"], EXAMPLES_EN.get((name, None))))
    if g.get("extra"):
        body.append(g["extra"])
    for sub, meta in g["subs"].items():
        args = ["image", "--vm", sub] if name == "image" and sub in ("push", "build") else list(group_argv(name)) + [sub]
        body.append(f"<h2 id='{sub}'><code>{html.escape(name)} {html.escape(sub)}</code></h2>")
        sub_help = help_of(*args)
        sub_intro, sub_rest = split_help_intro(sub_help)
        if sub_intro:
            for para in sub_intro.split("\n\n"):
                if para.strip():
                    body.append(f"<p class='intro'>{render_prose(para.strip())}</p>")
        body.append(f"<div class='help'><pre><code>{html.escape(sub_rest)}</code></pre></div>")
        if meta.get("notes"):
            notes_en = NOTES_EN.get((name, sub))
            if notes_en:
                body.append(bi("div", meta["notes"], notes_en))
            else:
                body.append(meta["notes"])
        if meta.get("examples"):
            body.append(f"<h3>{bi('span', 'Exemplos', 'Examples')}</h3>" + examples_html(meta["examples"], EXAMPLES_EN.get((name, sub))))
    if name in CLI_LABS:
        body.append(lab_challenge_html(CLI_LABS[name]))
    page(f"comandos/{name}.html", g["title"], "\n".join(body), depth=1)


INDEX_PT = """
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
<p>Para publicar a porta <strong>80</strong> ou <strong>443</strong> (<code>-p 80:80</code>)
acrescenta <code>--low-ports</code>:</p>
<pre><code>curl -fsSL .../install.sh | bash -s -- --low-ports</code></pre>
<p>Sem essa flag, <code>-p 80:80</code> é recusado: quem liga a porta do lado do host é o
<code>slirp4netns</code>, sem privilégios, e o kernel reserva as portas abaixo de
<code>net.ipv4.ip_unprivileged_port_start</code> (1024 por omissão) — o Podman e o Docker
rootless têm o mesmo muro. É opt-in porque baixa esse limiar para o <em>host inteiro</em>:
a partir daí qualquer programa local pode ligar-se às portas 80-1023. Num portátil é um
compromisso razoável; numa máquina partilhada, a alternativa que não baixa nada é um proxy
como root na porta 80 a encaminhar para uma porta alta. Reverter: apagar
<code>/etc/sysctl.d/99-delonix-lowports.conf</code>.</p>
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

INDEX_EN = """
<h1>Delonix Engine <span class="pill">v{ver}</span></h1>
<p class="tagline"><strong>Engine</strong> for containers and microVMs, <strong>daemonless</strong>,
<strong>rootless-first</strong>, kernel-native, in Rust — with its own CRI for Kubernetes.
<em>The open-source engine that powers Delonix.</em></p>

<p>Not a low-level OCI <em>runtime</em> (that's <code>runc</code>/<code>crun</code>): it's a
COMPLETE engine for containers <strong>and</strong> VMs — build, run, networking, firewall,
storage and Kubernetes cluster bootstrap, all in a single binary. It's the open layer
(Apache-2.0) the <strong>Delonix</strong> platform is built on.</p>

<p>Delonix Engine does Docker/Podman's job with no resident daemon: every command is an ephemeral
process that talks to the kernel directly (namespaces, cgroups v2, pivot_root), keeps its state in
files, and exits. In rootless mode, networking is served by a single shared holder-netns +
slirp4netns pair — not one slirp per container — with nft DNAT for port publishing, which is what
lets you <em>swap ports and volumes on the fly</em>, with no container restart.</p>

<h2>Install</h2>
<p>One command installs the binary <strong>and</strong> every runtime dependency (rootless
networking, VMs, kernel tuning), picking the right variant for your CPU. Works on
Debian/Ubuntu, Fedora/RHEL, openSUSE and Arch:</p>
<pre><code>curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash</code></pre>
<p>To publish port <strong>80</strong> or <strong>443</strong> (<code>-p 80:80</code>) add
<code>--low-ports</code>:</p>
<pre><code>curl -fsSL .../install.sh | bash -s -- --low-ports</code></pre>
<p>Without that flag, <code>-p 80:80</code> is refused: the process that binds the port on the
host side is <code>slirp4netns</code>, unprivileged, and the kernel reserves ports below
<code>net.ipv4.ip_unprivileged_port_start</code> (1024 by default) — rootless Podman and Docker
hit the same wall. It's opt-in because it lowers that threshold for the <em>whole host</em>: from
then on, any local program can bind ports 80-1023. On a laptop that's a reasonable trade-off; on a
shared machine, the alternative that doesn't lower anything is a root proxy on port 80 forwarding
to a high port. To revert: delete <code>/etc/sysctl.d/99-delonix-lowports.conf</code>.</p>
<p>Alternative (binary only, dependencies are on you):</p>
<pre><code>curl -fL -o ~/.local/bin/delonix \\
  https://github.com/angolardevops/delonix-runtime/releases/latest/download/delonix-x86_64-linux
chmod +x ~/.local/bin/delonix
echo 'source &lt;(delonix completion bash)' &gt;&gt; ~/.bashrc</code></pre>

<h2>Getting started</h2>
<pre><code># a web service on port 8080, no root, no daemon
delonix container run -d --name web -p 8080:80 nginx
curl localhost:8080

delonix container stats          # CPU/memory/PIDs
delonix container logs -f web    # follow logs
delonix container stop web       # the port closes on its own
delonix container start web      # restarts with the same state</code></pre>

<h2>CLI reference</h2>
<div class="cards">{cards}</div>

<h2>Why it's different</h2>
<table>
<tr><th></th><th>Docker</th><th>Podman</th><th>Delonix</th></tr>
<tr><td>Daemon</td><td>dockerd (root)</td><td>no (conmon per container)</td><td>no — and no resident per-container monitor</td></tr>
<tr><td>Rootless</td><td>optional</td><td>yes (slirp/pasta per container)</td><td>by default — 1 shared slirp + nft ingress</td></tr>
<tr><td>VMs</td><td>—</td><td>machine (for itself)</td><td>first-class declarative microVMs (Cloud Hypervisor/libvirt)</td></tr>
<tr><td>Kubernetes</td><td>—</td><td>—</td><td>own CRI + kubeadm bootstrap from scratch (<code>delonix cluster</code>)</td></tr>
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

ARCH_EN = """
<h1>Architecture</h1>
<p class="tagline">8 crates, one binary — and no resident process.</p>

<h2>Overview</h2>
<div class="arch">
<div class="row"><div class="box mut" style="flex:3"><b>delonix (CLI) — delonix-runtime-bin</b>
grouped commands: container · image · build · vm · volumes · network · stack · cluster</div></div>
<div class="row">
<div class="box"><b>delonix-runtime</b>clone() + namespaces (mount/pid/ipc/uts/net/user/cgroup),
pivot_root, seccomp/caps, delegated cgroups v2, exec, reconcile</div>
<div class="box"><b>delonix-image</b>OCI pull (digest verified), build, export, CNB buildpacks,
signatures, registry</div>
<div class="box"><b>delonix-net</b>rootless SDN: holder netns + bridge + single slirp, nft
DNAT/firewall, internal DNS, WireGuard overlay</div>
</div>
<div class="row">
<div class="box"><b>delonix-vm</b>microVMs (VmBackend trait: Cloud Hypervisor · libvirt), cloud-init</div>
<div class="box"><b>delonix-volume</b>named volumes, bind mounts, quotas, nfs</div>
<div class="box"><b>delonix-cri</b>runtime.v1 CRI server — the kubelet talks to Delonix</div>
<div class="box mut"><b>delonix-runtime-core</b>shared types: Container, Vm, Status, JSON Store,
Secret Manager</div>
</div>
</div>

<h2>Genuinely daemonless</h2>
<p>There's no daemon, not even a per-container monitor (podman's conmon). <code>run</code> does a
direct <code>clone()</code>; in detached mode, an ephemeral logging <em>shim</em> just drains
stdout/stderr to the log file (with rotation) and dies with the container. State
(the full spec of every container/VM/volume/network) lives as JSON under
<code>$DELONIX_ROOT</code> — <code>ps</code>/<code>start</code>/<code>inspect</code> rebuild
everything from there, and opportunistic <em>reapers</em> clean up orphans (slirp with no target,
hostfwd with no container) on every relevant invocation.</p>

<h2>Rootless-first</h2>
<p>Without root, isolation comes from user namespaces with subuid mapping
(<code>newuidmap</code>/<code>newgidmap</code>, like podman) — the container's uid 0 is an
unprivileged host uid. The rootfs is a persistent flat copy per container (in root mode, overlayfs
with the upper layer preserved). With <code>--privileged</code> plus Kind node labels, the runtime
sets up the dedicated cgroup v2 delegation a nested systemd (kindest/node) needs.</p>

<h2>Rootless networking: the ingress</h2>
<div class="arch">
<div class="row">
<div class="box mut" style="flex:1"><b>host</b>published ports (127.0.0.1 by default;
<code>DELONIX_PUBLISH_ADDR</code> to expose)</div>
<div class="box" style="flex:1"><b>holder netns (1 per user)</b>delonix0 bridge ·
single slirp4netns · nft (DNAT "pre", firewall) · internal DNS with container names</div>
<div class="box mut" style="flex:1"><b>containers</b>one veth per container, attached to the
bridge; deterministic IP per id</div>
</div>
</div>
<p>Publishing a port = one <code>add_hostfwd</code> on the single slirp's api-socket plus one DNAT
rule in the ingress chain — <strong>dataplane state, not container state</strong>. That's why ports
(and volumes, via live mounts) swap on the fly: the container process is never touched. With
<code>--net host</code> + <code>-p</code>, the container gets its own netns with a dedicated
slirp4netns (the podman model), which dies with it.</p>

<h2>Security</h2>
<p>Rootless by default; seccomp and capability drop outside <code>--privileged</code>, with startup
FAILING if they don't actually come up; pull with digest verification (including OCI VM
artifacts); manifest inputs for resources like <code>Cluster</code>/<code>Vm</code> validated by
whitelist before reaching any remote shell.</p>
<div class="callout warn">
<p><b>2026-07-21 audit — 6 high-severity findings, FIXED by 2026-07-23</b> (the build's
<code>COPY</code>, for instance, was bypassable via symlink despite an earlier fix having tried to
close it — it now canonicalizes and confirms confinement). There's no sign of remote code
execution, but the fixes haven't yet been confirmed by a 2nd independent audit and the syscall core
has never had a security review — out of caution, still avoid untrusted images/manifests or
exposing the engine on a shared host until confirmed. Full detail in
<a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/AUDITORIA-E2E.md">AUDITORIA-E2E.md</a>
— see also the <a href="comparacao.html">comparison with Docker/Podman</a> for the project's
overall status.</p>
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

<h2>Conformidade — medida, não afirmada</h2>
<p>«Serve um kubelet» é uma alegação; <strong>79 de 103 specs nomeados</strong> é um facto que
outra pessoa verifica. O <code>delonix-cri</code> é corrido contra o
<code>critest</code> do <a href="https://github.com/kubernetes-sigs/cri-tools">cri-tools</a>, a
suite de upstream, e o número é publicado:</p>
<pre><code>Ran 103 of 122 Specs
79 Passed | 24 Failed | 19 Skipped        # rootless, cgroup v2</code></pre>
<p>Reproduz com <code>tests/compat/cri-conformance.sh</code>. O detalhe completo do que falha e
porquê — incluindo o que <em>não</em> é nosso — está em
<a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/cri-conformance.md">docs/cri-conformance.md</a>.</p>
<p>Cerca de metade das falhas restantes não são lacunas do motor: nove são specs de AppArmor, que
exigem <code>CAP_MAC_ADMIN</code> no user namespace <em>inicial</em> (o Docker e o containerd têm
exactamente o mesmo limite), e quatro são testes de montagem em que a própria suite não consegue
montar no host sem root.</p>
<p>Uma divergência é <strong>deliberada</strong> e não muda para ganhar um spec: um container sem
perfil seccomp declarado corre sob o allowlist embutido do motor, não <em>unconfined</em>. É mais
apertado do que a especificação pede.</p>

<h2>Do zero a um cluster</h2>
<p>É esta peça que fecha o ciclo do <code>delonix cluster</code>: a imagem VM dourada
(<code>delonix image --vm build</code>) já traz kubeadm/kubelet/kubectl e o
<code>delonix-cri</code> activo; <code>delonix cluster kubeadm</code> provisiona as VMs e faz o
bootstrap — o cluster resultante corre Kubernetes com o Delonix como runtime de ponta a ponta.</p>
"""

CRI_EN = """
<h1>CRI — kubelet with no containerd</h1>
<p class="tagline">The <code>delonix-cri</code> crate implements Kubernetes' Container Runtime
Interface (<code>runtime.v1</code>).</p>

<p>The kubelet doesn't know how to run containers — it delegates to a runtime over CRI (gRPC over
a unix socket). Usually that runtime is containerd or CRI-O; with Delonix, it's the
<code>delonix-cri</code> binary: <em>Kubernetes pods and containers running directly on the
Delonix engine</em>, with no other piece involved.</p>

<h2>How it connects</h2>
<pre><code># the service (the golden VM image already ships it as a systemd unit)
DELONIX_CRI_ADDR=/run/delonix-cri.sock delonix-cri

# the kubelet points at it
kubelet --container-runtime-endpoint=unix:///run/delonix-cri.sock …</code></pre>

<h2>What it implements</h2>
<table>
<tr><th>CRI area</th><th>Support</th></tr>
<tr><td>RuntimeService — sandboxes (pods)</td><td>pod sandbox creation with shared netns
(the pod's containers join the sandbox's network via <code>join_netns</code>), labels/annotations,
status and removal</td></tr>
<tr><td>RuntimeService — containers</td><td>create/start/stop/remove, exec, logs in CRI format
(<code>&lt;rfc3339nano&gt; stdout F line</code>), per-pod cpu/memory limits via cgroups v2</td></tr>
<tr><td>ImageService</td><td>pull (digest verified), list, status, remove — over Delonix's normal
<code>ImageStore</code></td></tr>
<tr><td>Networking</td><td>CNI compatibility (attach/detach via JSON conf)</td></tr>
</table>

<h2>Conformance — measured, not claimed</h2>
<p>"Serves a kubelet" is a claim; <strong>79 of 103 named specs</strong> is a fact someone else
can verify. <code>delonix-cri</code> is run against <a href="https://github.com/kubernetes-sigs/cri-tools">cri-tools</a>'
<code>critest</code>, the upstream suite, and the number is published:</p>
<pre><code>Ran 103 of 122 Specs
79 Passed | 24 Failed | 19 Skipped        # rootless, cgroup v2</code></pre>
<p>Reproduce it with <code>tests/compat/cri-conformance.sh</code>. Full detail on what fails and
why — including what <em>isn't</em> ours to fix — is in
<a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/cri-conformance.md">docs/cri-conformance.md</a>.</p>
<p>About half of the remaining failures aren't engine gaps: nine are AppArmor specs, which need
<code>CAP_MAC_ADMIN</code> in the <em>initial</em> user namespace (Docker and containerd hit
exactly the same limit), and four are mount tests where the suite itself can't mount on the host
without root.</p>
<p>One divergence is <strong>deliberate</strong> and doesn't change just to pass a spec: a
container with no declared seccomp profile runs under the engine's built-in allowlist, not
<em>unconfined</em>. That's stricter than the spec asks for.</p>

<h2>From zero to a cluster</h2>
<p>This is the piece that closes the <code>delonix cluster</code> loop: the golden VM image
(<code>delonix image --vm build</code>) already ships kubeadm/kubelet/kubectl and
<code>delonix-cri</code> running; <code>delonix cluster kubeadm</code> provisions the VMs and does
the bootstrap — the resulting cluster runs Kubernetes with Delonix as the runtime end to end.</p>
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
<p><b>Estado actual (2026-07): beta público, em hardening activo.</b> Várias rondas de auditoria
ofensiva já correram sobre o núcleo de syscalls do motor (<code>clone</code>/<code>mount</code>/
namespaces, ~104 blocos <code>unsafe</code>), a fronteira rootless→root, o socket de controlo, e o
código mais recente (rede/cluster/manifesto) — todos os achados CRÍTICOS e ALTOS encontrados
<strong>já estão corrigidos</strong>, e os de maior severidade (os 6 HIGH originais + os
CRITICAL/HIGH de rondas seguintes) foram <strong>re-confirmados por uma auditoria adversarial
INDEPENDENTE genuína (2026-07-26)</strong> — um TOCTOU adicional real foi encontrado nessa ronda
(kubeconfig com uma janela de permissões antes do <code>chmod</code>) e já está corrigido também.
Continuam por fechar ~27 achados de severidade MÉDIA/BAIXA (documentados, sem exploit conhecido).
Não há indícios de execução remota de código a partir da rede — a fronteira rootless→root está
sólida e já foi testada por um 2.º par de olhos — mas <strong>por prudência, um projecto sem anos
de produção continua a merecer cautela</strong> em produção multi-tenant com dados sensíveis.
Detalhe completo, com ficheiro e linha de cada achado e o estado da correcção:
<a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/AUDITORIA-E2E.md">relatório
da auditoria original</a> e
<a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/COMPARACAO-DOCKER-PODMAN.md">análise
de gaps com o histórico completo das rondas seguintes</a>.</p>
</div>

<h2>Decisão rápida</h2>
<table>
<tr><th>Se precisas de…</th><th>Usa</th></tr>
<tr><td>Correr um <code>docker-compose.yml</code> já existente</td>
<td><strong>Delonix</strong> — <code>delonix compose up</code>, suporte nativo (Compose Spec v2.x),
sem Docker instalado</td></tr>
<tr><td>Um pipeline de build com BuildKit completo (cache mounts, SSH forwarding, cross-compile
paralelo)</td>
<td>Docker ou Podman — o Delonix faz multi-stage, <code>--mount=type=secret</code> e cache de
camadas (rootless), mas não <code>type=cache</code>/<code>type=ssh</code> nem paralelismo de
estágios</td></tr>
<tr><td>Cargas GPU/CUDA</td>
<td>Delonix via CDI funciona (mesma fonte que Docker/Podman) mas nunca foi validado num host GPU
real — para produção GPU hoje, prefere Docker/Podman</td></tr>
<tr><td><code>docker version</code>/<code>ps</code>/<code>images</code>/<code>info</code> e o
ciclo de vida completo de um container via <code>DOCKER_HOST</code></td>
<td><strong>Delonix</strong> — <code>delonix serve docker-api</code>, validado contra um
<code>docker</code> CLI real, incl. <code>docker compose up</code> apontado ao socket</td></tr>
<tr><td><code>docker exec</code>/attach interactivo via a API, ou <code>--restart</code> via a API</td>
<td>Docker ou Podman — deliberadamente fora de escopo (hijacking HTTP / modelo de supervisor
incompatível com um servidor multi-thread)</td></tr>
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
<td><span class="tag mid">multi-stage + ARG/USER/ENTRYPOINT + <code>--mount=type=secret</code> +
cache de camadas (rootless) já funcionam; sem <code>type=cache</code>/<code>type=ssh</code> nem
paralelismo de estágios do BuildKit real</span></td></tr>
<tr><td><code>docker compose</code> / orquestração local</td>
<td><span class="tag ok">nativo</span></td><td><span class="tag mid">podman-compose</span></td>
<td><span class="tag ok">nativo (<code>delonix compose</code>), sem Docker — <code>depends_on</code>
com healthcheck real</span></td></tr>
<tr><td>API compatível com <code>DOCKER_HOST</code></td>
<td><span class="tag ok">é a própria</span></td><td><span class="tag ok">compatível</span></td>
<td><span class="tag mid">ciclo de vida completo do container (create/start/stop/kill/wait/
restart/rename/rm); sem <code>exec</code>/attach interactivo nem <code>--restart</code></span></td></tr>
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
<td><span class="tag mid">via CDI (a mesma fonte que Docker/Podman consomem), mas nunca validado
num host GPU real</span></td></tr>
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
<li><strong><code>docker-compose.yml</code> nativo, sem Docker</strong> — <code>delonix compose
up/down/ps/logs</code>, com <code>depends_on</code> a esperar por um healthcheck real, não só por
ordem declarada.</li>
</ul>

<h2>Onde ainda não chega</h2>
<ul>
<li><strong>Build de imagens ainda não tem BuildKit real</strong> — multi-stage,
<code>ARG</code>/<code>--build-arg</code>, <code>USER</code>/<code>ENTRYPOINT</code>,
<code>--mount=type=secret</code> e cache de camadas (rootless) já funcionam, mas sem
<code>type=cache</code>/<code>type=ssh</code> nem paralelismo de estágios.</li>
<li><strong>GPU nunca validado num host real</strong> — o caminho CDI existe e usa a mesma fonte
que Docker/Podman consomem, mas sem um host com GPU não há confirmação ao vivo.</li>
<li><strong><code>docker exec</code>/attach interactivo e <code>--restart</code> via a API</strong> —
deliberadamente fora de escopo (hijacking HTTP / modelo de supervisor incompatível com um servidor
multi-thread).</li>
<li><strong><code>compose</code>: cobertura da spec ainda parcial</strong> — sem
<code>profiles</code>/<code>extends</code>/<code>configs</code>/<code>secrets</code> top-level,
multi-ficheiro, <code>build.target</code>, <code>deploy.replicas != 1</code>, IP fixo por rede, ou
volumes anónimos.</li>
<li><strong>Projecto novo</strong> — sem o histórico de produção que o Docker e o Podman têm; ver o
aviso de segurança no topo desta página antes de decidir.</li>
</ul>

<h2>Recomendação por perfil</h2>
<table>
<tr><th>Quem és</th><th>Sugestão</th></tr>
<tr><td>Programador(a) a experimentar em local/homelab, ou a fazer bootstrap de um cluster
Kubernetes pequeno sem instalar Docker</td>
<td>Experimenta o Delonix hoje — é exactamente o caso em que já está forte.</td></tr>
<tr><td>Equipa com um pipeline de build maduro que precisa de <code>--mount=type=cache</code>/
<code>type=ssh</code> ou paralelismo de estágios</td>
<td>Fica no Docker/Podman para esse build específico; podes correr as imagens resultantes no
Delonix se quiseres testar a operação — <code>docker-compose.yml</code>, multi-stage e
<code>--mount=type=secret</code> já funcionam (rootless).</td></tr>
<tr><td>Empresa a avaliar para produção multi-tenant ou com dados sensíveis</td>
<td>Os achados de severidade CRÍTICA/ALTA já estão corrigidos e re-confirmados por uma auditoria
independente (aviso acima); ainda faltam ~27 achados MÉDIO/BAIXO documentados (sem exploit
conhecido) e o histórico de produção que só o tempo dá — acompanha o
<a href="https://github.com/angolardevops/delonix-runtime/releases">changelog</a>.</td></tr>
<tr><td>Quer avaliar tecnicamente ao detalhe (gap-a-gap, com ficheiro e linha)</td>
<td>Lê a <a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/COMPARACAO-DOCKER-PODMAN.md">análise de gaps completa</a> no repositório.</td></tr>
</table>
"""

COMPARE_EN = """
<h1>Delonix vs Docker vs Podman</h1>
<p class="tagline">An honest comparison, to help you decide which engine to build on — not a sales
pitch.</p>

<p>Delonix Engine is a <strong>daemonless, rootless-first</strong> container and microVM engine, in
Rust, with Kubernetes built in from the ground up (its own CRI). On several concrete points it
already goes further than Docker and rootless Podman. On others, it lags well behind. This page
says exactly where is where — for someone deciding what to install today, or a company evaluating
it for production.</p>

<div class="callout warn">
<p><b>Current status (2026-07): public beta, under active hardening.</b> Several rounds of
offensive audit have already run over the engine's syscall core (<code>clone</code>/<code>mount</code>/
namespaces, ~104 <code>unsafe</code> blocks), the rootless→root boundary, the control socket, and the
most recent code (networking/cluster/manifest) — every CRITICAL and HIGH finding
<strong>is already fixed</strong>, and the highest-severity ones (the original 6 HIGH findings plus
the CRITICAL/HIGH ones from later rounds) were <strong>re-confirmed by a genuinely INDEPENDENT
adversarial audit (2026-07-26)</strong> — one additional real TOCTOU was found in that round
(a kubeconfig permissions window before <code>chmod</code>) and is already fixed too.
About 27 MEDIUM/LOW findings remain open (documented, no known exploit).
There's no sign of remote code execution from the network — the rootless→root boundary is
solid and has already been tested by a second pair of eyes — but <strong>out of caution, a project
without years of production history still deserves care</strong> in multi-tenant production with
sensitive data. Full detail, with the file and line of every finding and its fix status:
<a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/AUDITORIA-E2E.md">original audit
report</a> and
<a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/COMPARACAO-DOCKER-PODMAN.md">gap
analysis with the full history of later rounds</a>.</p>
</div>

<h2>Quick decision</h2>
<table>
<tr><th>If you need…</th><th>Use</th></tr>
<tr><td>To run an existing <code>docker-compose.yml</code></td>
<td><strong>Delonix</strong> — <code>delonix compose up</code>, native support (Compose Spec v2.x),
no Docker installed</td></tr>
<tr><td>A build pipeline with full BuildKit (cache mounts, SSH forwarding, parallel
cross-compile)</td>
<td>Docker or Podman — Delonix does multi-stage, <code>--mount=type=secret</code> and layer
caching (rootless), but not <code>type=cache</code>/<code>type=ssh</code> or stage
parallelism</td></tr>
<tr><td>GPU/CUDA workloads</td>
<td>Delonix works via CDI (the same source as Docker/Podman) but has never been validated on a
real GPU host — for GPU production today, prefer Docker/Podman</td></tr>
<tr><td><code>docker version</code>/<code>ps</code>/<code>images</code>/<code>info</code> and the
full lifecycle of a container via <code>DOCKER_HOST</code></td>
<td><strong>Delonix</strong> — <code>delonix serve docker-api</code>, validated against a real
<code>docker</code> CLI, including <code>docker compose up</code> pointed at the socket</td></tr>
<tr><td>Interactive <code>docker exec</code>/attach via the API, or <code>--restart</code> via the
API</td>
<td>Docker or Podman — deliberately out of scope (HTTP hijacking / supervisor model incompatible
with a multi-threaded server)</td></tr>
<tr><td>Bootstrapping a real Kubernetes cluster with no Docker/containerd installed</td>
<td><strong>Delonix</strong> — own CRI, already validated with a <code>Ready</code> v1.34
control-plane</td></tr>
<tr><td>One engine for containers <strong>and</strong> microVMs <strong>and</strong> Kubernetes</td>
<td><strong>Delonix</strong> — nobody in the Docker/Podman space covers this together</td></tr>
<tr><td>Swapping a container's ports/volumes/networks on the fly, with no recreate</td>
<td><strong>Delonix</strong> — Docker forces a recreate</td></tr>
<tr><td>Advanced rootless networking (encrypted inter-node overlay, per-container directed
firewall)</td>
<td><strong>Delonix</strong> — ahead of rootless Podman on these points</td></tr>
<tr><td>An engine with years of production use, a huge community, maximum tooling
compatibility</td>
<td>Docker or Podman — still no substitute in sight</td></tr>
</table>

<h2>Comparison by area</h2>
<p><span class="tag ok">strong</span> · <span class="tag mid">partial or limited</span> ·
<span class="tag no">absent</span></p>

<table>
<tr><th>Area</th><th>Docker</th><th>Podman</th><th>Delonix</th></tr>
<tr><td>Run/stop/inspect containers</td>
<td><span class="tag ok">strong</span></td><td><span class="tag ok">strong</span></td>
<td><span class="tag ok">strong</span> — plus hot reconfiguration and automatic crash diagnosis
(reason + log snapshot, not just "Exited")</td></tr>
<tr><td>Rootless by default</td>
<td><span class="tag no">not the default mode</span></td>
<td><span class="tag ok">strong, it's Podman's whole pitch</span></td>
<td><span class="tag ok">strong — and fails on purpose if isolation doesn't actually come up</span></td></tr>
<tr><td>Image builds (<code>Dockerfile</code>)</td>
<td><span class="tag ok">strong — multi-stage, BuildKit, cache</span></td>
<td><span class="tag ok">strong — via buildah</span></td>
<td><span class="tag mid">multi-stage + ARG/USER/ENTRYPOINT + <code>--mount=type=secret</code> +
layer cache (rootless) already work; no real BuildKit
<code>type=cache</code>/<code>type=ssh</code> or stage parallelism</span></td></tr>
<tr><td><code>docker compose</code> / local orchestration</td>
<td><span class="tag ok">native</span></td><td><span class="tag mid">podman-compose</span></td>
<td><span class="tag ok">native (<code>delonix compose</code>), no Docker — <code>depends_on</code>
with a real healthcheck</span></td></tr>
<tr><td><code>DOCKER_HOST</code>-compatible API</td>
<td><span class="tag ok">is the real thing</span></td><td><span class="tag ok">compatible</span></td>
<td><span class="tag mid">full container lifecycle (create/start/stop/kill/wait/
restart/rename/rm); no interactive <code>exec</code>/attach or <code>--restart</code></span></td></tr>
<tr><td>Advanced rootless networking (inter-node overlay, per-container firewall)</td>
<td><span class="tag no">overlay needs swarm</span></td>
<td><span class="tag mid">no native rootless overlay</span></td>
<td><span class="tag ok">rootless VXLAN+WireGuard, per-container directed firewall</span></td></tr>
<tr><td>Kubernetes bootstrap with no Docker/containerd</td>
<td><span class="tag no">not Docker's job</span></td>
<td><span class="tag no">no own CRI</span></td>
<td><span class="tag ok">own CRI + <code>cluster kubeadm</code>, validated with a real cluster</span></td></tr>
<tr><td>MicroVMs in the same engine</td>
<td><span class="tag no">absent</span></td><td><span class="tag no">absent</span></td>
<td><span class="tag ok">Cloud Hypervisor / libvirt, declarative</span></td></tr>
<tr><td>GPU/CUDA</td>
<td><span class="tag ok">mature nvidia-container-toolkit</span></td>
<td><span class="tag ok">same</span></td>
<td><span class="tag mid">via CDI (the same source Docker/Podman consume), but never validated on
a real GPU host</span></td></tr>
<tr><td>Built-in image signing + CVE scan</td>
<td><span class="tag no">needs separate cosign/trivy</span></td>
<td><span class="tag no">same</span></td>
<td><span class="tag ok">cosign/sigstore + CVE scan in the engine itself</span></td></tr>
<tr><td>Security maturity IN PRODUCTION (years of real adversarial use)</td>
<td><span class="tag ok">very mature</span></td><td><span class="tag ok">very mature</span></td>
<td><span class="tag mid">new project — its own audit already found and fixed high-severity
flaws, still no independent confirmation beyond what's noted above</span></td></tr>
<tr><td>Ecosystem (docs, forums, third-party integrations)</td>
<td><span class="tag ok">huge</span></td><td><span class="tag ok">large</span></td>
<td><span class="tag no">early days — this site plus the repository is all there is for now</span></td></tr>
</table>

<h2>Where Delonix already goes further</h2>
<ul>
<li><strong>One engine, three problems</strong> — containers, microVMs and Kubernetes (via its own
CRI) in the same tool. A full Kubernetes v1.34 control-plane has already run <code>Ready</code>,
with <code>kube-proxy</code> itself programming netfilter inside the rootless model.</li>
<li><strong>Hot reconfiguration</strong> — changing a container's ports, volumes, networks or
bandwidth limit <em>without recreating it</em>, same PID. On Docker, changing a port means deleting
and recreating the container.</li>
<li><strong>Automatic crash diagnosis</strong> — when a container dies unexpectedly, Delonix
records the reason (process disappeared vs. recycled PID) and automatically saves a log excerpt.
Docker and Podman just say "Exited"/"Dead".</li>
<li><strong>Stricter rootless security by design</strong> — no-new-privs always on, and a
container's start <em>fails</em> if seccomp/capabilities don't actually take effect, instead of
proceeding while pretending to be protected.</li>
<li><strong>Kubernetes-style network storage</strong> — an NFS/CIFS/WebDAV folder becomes a named
volume any container can mount, like a <code>PersistentVolume</code>.</li>
<li><strong>Native <code>docker-compose.yml</code>, no Docker</strong> — <code>delonix compose
up/down/ps/logs</code>, with <code>depends_on</code> waiting for a real healthcheck, not just
declared order.</li>
</ul>

<h2>Where it still falls short</h2>
<ul>
<li><strong>Image builds still have no real BuildKit</strong> — multi-stage,
<code>ARG</code>/<code>--build-arg</code>, <code>USER</code>/<code>ENTRYPOINT</code>,
<code>--mount=type=secret</code> and layer caching (rootless) already work, but no
<code>type=cache</code>/<code>type=ssh</code> or stage parallelism.</li>
<li><strong>GPU never validated on a real host</strong> — the CDI path exists and uses the same
source Docker/Podman consume, but with no GPU host there's no live confirmation.</li>
<li><strong>Interactive <code>docker exec</code>/attach and <code>--restart</code> via the
API</strong> — deliberately out of scope (HTTP hijacking / supervisor model incompatible with a
multi-threaded server).</li>
<li><strong><code>compose</code>: spec coverage still partial</strong> — no top-level
<code>profiles</code>/<code>extends</code>/<code>configs</code>/<code>secrets</code>,
multi-file, <code>build.target</code>, <code>deploy.replicas != 1</code>, fixed per-network IP, or
anonymous volumes.</li>
<li><strong>New project</strong> — without the production track record Docker and Podman have; see
the security notice at the top of this page before deciding.</li>
</ul>

<h2>Recommendation by profile</h2>
<table>
<tr><th>Who you are</th><th>Suggestion</th></tr>
<tr><td>Developer experimenting locally/homelab, or bootstrapping a small Kubernetes cluster with
no Docker install</td>
<td>Try Delonix today — it's exactly the case where it's already strong.</td></tr>
<tr><td>Team with a mature build pipeline that needs <code>--mount=type=cache</code>/
<code>type=ssh</code> or stage parallelism</td>
<td>Stay on Docker/Podman for that specific build; you can run the resulting images on Delonix if
you want to test operations — <code>docker-compose.yml</code>, multi-stage and
<code>--mount=type=secret</code> already work (rootless).</td></tr>
<tr><td>Company evaluating for multi-tenant production or with sensitive data</td>
<td>CRITICAL/HIGH severity findings are already fixed and re-confirmed by an independent audit
(notice above); about 27 documented MEDIUM/LOW findings remain (no known exploit), plus the
production track record only time can give — follow the
<a href="https://github.com/angolardevops/delonix-runtime/releases">changelog</a>.</td></tr>
<tr><td>Wants to evaluate it technically in detail (gap by gap, with file and line)</td>
<td>Read the <a href="https://github.com/angolardevops/delonix-runtime/blob/main/docs/COMPARACAO-DOCKER-PODMAN.md">full gap
analysis</a> in the repository.</td></tr>
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
    out, seen, rows = help_of(*group_argv(group)), False, []
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
    ("Firewall: só deixar entrar Postgres da SDN", "delonix net ingress allow db tcp/5432 --from 10.219.0.0/16\ndelonix net ingress policy db deny"),
    ("Firewall: egress da rede só p/ DNS + CIDRs", "delonix net egress net backend allowlist --to 10.0.0.0/8"),
    ("Tráfego por container ao vivo (eBPF)", "sudo delonix net flow --watch"),
    ("Volume de rede de um NAS (NFS)", "delonix storage create media --type nfs --server 10.0.0.5 --share /mnt/pool/media"),
    ("Segredo no cofre (não no argv)", "printf 'password=s3nha' | delonix secret create db-pass --from-env-file -"),
    ("Expor um container à internet pública (sem conta, sem router)",
     "delonix container run -d --name web --expose 80 nginx\ndelonix net tunnel expose --provider pinggy --local-port 8080",
     "tunnel/tunnel-8080: running — https://oxipg-197-148-40-67.free.pinggy.net"),
    ("NAS partilhado por vários tenants, cada um com a sua quota",
     "delonix storage create nas --type nfs --server 10.0.0.5 --share /pool/data\n"
     "delonix sharevolume apply -f sharevolume.yaml"),
    ("microVM com cloud-init", "delonix vm create node1 --disk base.qcow2 --ssh-key @~/.ssh/id_ed25519.pub"),
    ("Cluster Kubernetes do zero", "delonix cluster kubeadm --name lab --control-plane 1 --workers 2"),
    ("Aplicar um manifesto inteiro", "delonix stack apply -f delonix-manifest.yaml"),
    ("Persistir os containers no arranque", "delonix net boot enable"),
    ("Recuperar espaço (GC)", "delonix system prune"),
]

# Tradução EN das legendas de `CHEAT_TASKS` (mesma ordem/tamanho).
CHEAT_TASKS_EN = [
    "Web service, no root, no daemon",
    "Disposable shell",
    "Own network + publish via ingress",
    "Hot-swap a port (no restart)",
    "Firewall: only let Postgres in from the SDN",
    "Firewall: network egress to DNS + CIDRs only",
    "Live per-container traffic (eBPF)",
    "Network volume from a NAS (NFS)",
    "Secret in the vault (not in argv)",
    "Expose a container to the public internet (no account, no router)",
    "NAS shared by several tenants, each with its own quota",
    "microVM with cloud-init",
    "Kubernetes cluster from scratch",
    "Apply a whole manifest",
    "Persist containers on boot",
    "Reclaim space (GC)",
]


# Kinds do manifesto — cada um com um template COMPLETO e funcional (lido dos
# `examples/*.yaml`, que são a referência canónica com todos os campos + defaults).
KINDS_DOC = [
    ("Secret", "secret.yaml", "Um segredo do cofre cifrado em repouso. Consumido por <code>run --secret</code>/"
     "<code>--secret-files</code> e por <code>passwordSecret</code> do Storage. Os valores NUNCA ficam no registo do "
     "container em texto — são resolvidos no arranque a partir do NOME."),
    ("Pod", "pod.yaml", "A forma de Pod do Kubernetes (<code>spec.containers[]</code>) para <code>kind: Container</code> — "
     "portas/env/resources/securityContext/volumeMounts estruturados. v1 aceita UM container; para vários, "
     "<code>kind: Pod</code> (ver <code>examples/pod-multi.yaml</code>)."),
    ("Workload", "workload.yaml", "UM objecto declarativo para os dois tipos de computação: "
     "<code>spec.type: container | vm | pod | microvm</code> + o bloco com o mesmo nome. Baixa para o Kind "
     "correspondente no load — não redefine um único campo, por isso não pode divergir dele."),
    ("Dependency", "dependency.yaml", "Alcançabilidade DIRIGIDA entre containers (ao contrário da rede, que é "
     "bidireccional): <code>from</code> alcança <code>to</code>, e <code>to</code> não fica exposto aos outros. "
     "Compila para firewall L4 por-container, sem dataplane novo."),
    ("FirewallPolicy", "firewallpolicy.yaml", "Firewall L4 por container, estilo NetworkPolicy do k8s, com a "
     "direcção em <code>spec.direction</code>. Aplicar substitui as regras dessa direcção e deixa a outra intacta."),
    ("Ingress", "ingress.yaml", "Ingress L7 no formato <code>networking.k8s.io/v1</code> (host/path → backend), "
     "compilado para o proxy embutido. Limitações herdadas: um só certificado (sem SNI) e "
     "<code>pathType: Exact</code> tratado como prefixo."),
    ("Stack", "stack.yaml", "Agrupa vários recursos num só documento. Expandido no load para os Kinds individuais, "
     "em ordem de dependência — o Stack não sobrevive ao load, tudo o resto vê os filhos."),
    ("Cluster", "cluster-ssh.yaml", "Bootstrap kubeadm idempotente sobre hosts JÁ vivos, por SSH. Sem ficheiro de "
     "estado: cada passo tem um <code>check</code>, por isso nunca dessincroniza. Ver também "
     "<code>cluster-vm.yaml</code> (provisiona as VMs) e <code>cluster-kind.yaml</code> (modo kind)."),
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

# Tradução EN das intros do `KINDS_DOC` (mesma ordem/tamanho — o YAML em si
# vem sempre de `examples/*.yaml`, ficheiros reais, e não se traduz).
KINDS_DOC_EN = [
    "A secret from the vault, encrypted at rest. Consumed by <code>run --secret</code>/"
    "<code>--secret-files</code> and by Storage's <code>passwordSecret</code>. Values are NEVER kept in the "
    "container's registry as plaintext — they're resolved at startup from the NAME.",
    "The Kubernetes Pod shape (<code>spec.containers[]</code>) for <code>kind: Container</code> — "
    "structured ports/env/resources/securityContext/volumeMounts. v1 accepts ONE container; for several, "
    "use <code>kind: Pod</code> (see <code>examples/pod-multi.yaml</code>).",
    "ONE declarative object for both kinds of compute: "
    "<code>spec.type: container | vm | pod | microvm</code> plus the block with that same name. Lowers to the "
    "matching Kind on load — it doesn't redefine a single field, so it can never drift from it.",
    "DIRECTED reachability between containers (unlike a network, which is "
    "bidirectional): <code>from</code> reaches <code>to</code>, and <code>to</code> stays unexposed to the "
    "others. Compiles to a per-container L4 firewall, with no new dataplane.",
    "Per-container L4 firewall, k8s NetworkPolicy-style, with the "
    "direction in <code>spec.direction</code>. Applying replaces that direction's rules and leaves the other "
    "intact.",
    "L7 Ingress in the <code>networking.k8s.io/v1</code> shape (host/path → backend), "
    "compiled to the built-in proxy. Inherited limitations: one certificate only (no SNI) and "
    "<code>pathType: Exact</code> treated as a prefix.",
    "Groups several resources into one document. Expanded on load into the "
    "individual Kinds, in dependency order — the Stack doesn't survive the load, everything else sees the "
    "children.",
    "Idempotent kubeadm bootstrap on hosts that are ALREADY alive, over SSH. No state "
    "file: every step has a <code>check</code>, so it can never drift. See also "
    "<code>cluster-vm.yaml</code> (provisions the VMs) and <code>cluster-kind.yaml</code> (kind mode).",
    "A user network. Containers join it with <code>--net &lt;name&gt;</code>; "
    "VMs with <code>network:</code>. <code>bridge</code> is the only driver containers can attach to today.",
    "A named local volume — the data survives <code>container rm</code>. For "
    "NETWORK storage (NFS/SMB/WebDAV) use <code>kind: Storage</code> instead.",
    "A NETWORK volume mounted from a NAS (TrueNAS/Synology/Samba/Nextcloud), "
    "k8s PersistentVolume-style. The password comes from the vault (<code>--password-secret</code>). Mounting "
    "needs CAP_SYS_ADMIN.",
    "Pre-pulls (or builds) an image before the containers that depend on it. With "
    "<code>--vm</code> the same Kind covers golden VM images.",
    "A declarative microVM (Cloud Hypervisor or libvirt), with per-instance cloud-init. "
    "It's the layer <code>delonix cluster kubeadm</code> uses to provision nodes.",
    "The everyday workload. Only <code>image</code> is required; every other field "
    "has a default. Covers networking, storage, resources (cgroup v2), secrets, security, devices and limits.",
    "A REAL multi-container pod: N containers sharing the pod's namespaces (the same "
    "<code>spec.containers[]</code> schema as <code>kind: Container</code>, but with N containers allowed). "
    "They share <strong>netns</strong> (same IP, reachable via <code>localhost</code>), <strong>IPC</strong> "
    "and <strong>UTS</strong> (hostname). The PID namespace (<code>shareProcessNamespace</code>) is a "
    "follow-up. Managed with <code>delonix pod create/ls/describe/rm/logs</code>.",
    "Declarative per-direction L4 firewall (k8s NetworkPolicy-style). Each "
    "document is the desired state of one direction for one target container — allowlist + default-deny, "
    "idempotent.",
    "Built-in L7/HTTP reverse proxy — routing by <code>Host</code> + "
    "<code>path</code> prefix to backend containers. TLS terminates at the proxy (self-signed or "
    "<code>secretRef</code>); hot reload via SIGHUP.",
    "Exposes ONE local port to the public internet via pinggy/ngrok/cloudflare — no "
    "account, no public IP. Pairs with <code>HTTPRoute</code> by pointing <code>localPort</code> at where the "
    "L7 proxy listens: one public URL, Host-based routing on the other end to several backends.",
    "An ISOLATED, individually-quota'd slice of a <code>Storage</code> — several "
    "container/vm/pod share ONE NFS/CIFS/WebDAV export without seeing each other. Each slice is a real "
    "subdirectory of the parent mount, registered as its own volume; consumed with "
    "<code>-v &lt;name&gt;:/dest</code>, nothing new on the consumer side.",
]


# ---------------------------------------------------------------------------
# Laboratórios — o que falta entre "li a referência" e "sei usar isto"
# ---------------------------------------------------------------------------
#
# Cada lab é uma sessão curta, do zero a um resultado verificável, com a
# LIMPEZA incluída. A verificação não é decorativa: um lab que não diz como
# confirmar o resultado ensina a copiar comandos, não a usar a ferramenta.
LABS = [
    ("lab-1", "Primeiro serviço, em 60 segundos", "iniciante", """
<p>Objectivo: perceber o ciclo <code>run → ps → logs → exec → rm</code> e ver
que não há daemon nenhum por baixo.</p>
<pre><code># 1. Um serviço web, em segundo plano, publicado na porta 8080 do host
delonix container run -d --name web -p 8080:80 nginx:alpine

# 2. Confirmar — o STATUS diz há quanto tempo está de pé
delonix ps

# 3. A prova de que não há daemon: NENHUM processo residente do motor
pgrep -a delonix || echo "sem daemon — o container é filho do init do host"

# 4. Falar com ele
curl -s localhost:8080 | head -3

# 5. Entrar lá dentro
delonix exec -it web sh -c 'hostname; id; ls /etc/nginx'

# 6. Ver o que ele escreveu
delonix logs web | tail -5

# 7. Limpar
delonix rm -f web</code></pre>
<p class="note"><strong>Verificação:</strong> o passo 3 não imprime processo
nenhum do motor. Um container a correr sem supervisor residente é a diferença
de fundo para o Docker, e vê-se aqui em duas linhas.</p>"""),

    ("lab-2", "Limites de recursos que pegam mesmo", "iniciante", """
<p>Objectivo: descobrir se este host aplica limites — e o que fazer quando não
aplica. É o erro nº1 de quem começa em rootless.</p>
<pre><code># 1. Perguntar ANTES de assumir
delonix system setup

# 2. Se disser INERT ou PARTIAL, a 1.ª correcção não precisa de root nenhum
systemd-run --user --scope -p Delegate=yes -- delonix system setup
#   … e é dentro desse scope que corres os passos seguintes.
#   Só se ELE ainda disser que falta o `cpu`:
#     sudo delonix system setup --delegate   (+ logout / login)

# 3. Um container com tecto de memória
delonix container run -d --name limitado -m 128M alpine sleep 300

# 4. A prova: ler o cgroup REAL, não o que o comando diz
PID=$(delonix container inspect limitado | jq -r .pid)
cat /sys/fs/cgroup$(awk -F: '/^0::/{print $3}' /proc/$PID/cgroup)/memory.max

delonix rm -f limitado</code></pre>
<p class="note"><strong>Verificação:</strong> o passo 4 imprime
<code>134217728</code> (128 MiB), não <code>max</code>. Se imprimir
<code>max</code>, a delegação não está feita — e o container corre <em>sem</em>
limite nenhum, em silêncio. É por isso que o passo 1 existe.</p>
<p class="note"><strong>Nota:</strong> <code>cpuset</code> e <code>io</code>
aparecem quase sempre como <em>absent</em>, e está certo — num Ubuntu de
fábrica o <code>user.slice</code> (da root) só passa <code>cpu memory
pids</code> para baixo. Nada aqui precisa deles.</p>"""),

    ("lab-3", "Duas aplicações que se falam por nome", "intermédio", """
<p>Objectivo: rede de utilizador, DNS interno, e isolamento — sem configurar
DNS nenhum.</p>
<pre><code># 1. Uma rede própria
delonix network create loja

# 2. Base de dados e app, ambas nela
delonix container run -d --name db  --net loja -e POSTGRES_PASSWORD=x postgres:16-alpine
delonix container run -d --name app --net loja -p 8080:80 nginx:alpine

# 3. A app resolve a db PELO NOME, sem /etc/hosts nem variáveis
delonix exec app sh -c 'getent hosts db; nc -z db 5432 && echo "porta aberta"'

# 4. Fechar a db a tudo menos à app (alcançabilidade DIRIGIDA)
cat &gt; dep.yaml &lt;&lt;'EOF'
apiVersion: delonix.io/v1
kind: Dependency
metadata:
  name: app-conhece-db
spec:
  from: app
  to: db
  ports: ["5432"]
EOF
delonix stack apply -f dep.yaml

# 5. Provar: um terceiro container na MESMA rede já não alcança a db
delonix container run --rm --net loja alpine sh -c 'nc -z -w2 db 5432 || echo BLOQUEADO'

delonix rm -f db app; delonix network rm loja; rm dep.yaml</code></pre>
<p class="note"><strong>Verificação:</strong> o passo 3 diz "porta aberta" e o
passo 5 diz "BLOQUEADO". A mesma rede, dois resultados — é isso que
<code>kind: Dependency</code> faz e uma rede sozinha não faz.</p>"""),

    ("lab-4", "Do Dockerfile à imagem, sem daemon", "intermédio", """
<p>Objectivo: construir uma imagem e correr o que construíste.</p>
<pre><code>mkdir -p lab-build &amp;&amp; cd lab-build
cat &gt; Delonixfile &lt;&lt;'EOF'
FROM alpine:latest
RUN apk add --no-cache curl
COPY ola.txt /ola.txt
HEALTHCHECK CMD test -f /ola.txt
CMD ["sh", "-c", "cat /ola.txt; sleep 300"]
EOF
echo "construído com delonix" &gt; ola.txt

# `Delonixfile` é procurado antes de `Dockerfile` — sem -f nenhum
delonix build -t minha-app:1.0 .

# `--wait` bloqueia até o HEALTHCHECK passar: sem isto escreve-se
# `until ...; do sleep 1; done`, e escreve-se mal
delonix run -d --name a1 --wait --health-interval 2 minha-app:1.0
delonix ps

delonix rm -f a1; delonix image rm minha-app:1.0; cd ..; rm -rf lab-build</code></pre>
<p class="note"><strong>Verificação:</strong> o <code>ps</code> mostra
<code>(healthy)</code> na coluna STATUS. O motor está a sondar o container
sozinho — sem systemd, ao contrário do Podman rootless.</p>"""),

    ("lab-5", "Uma microVM a sério", "intermédio", """
<p>Objectivo: uma VM completa com o mesmo CLI dos containers.</p>
<pre><code># 1. Sem imagem local, o create descarrega a oficial sozinho
delonix vm create dev

# 2. Onde ficou, e com que IP
delonix vm ls
delonix vm describe dev

# 3. Entrar (voltar ao host: Ctrl+])
delonix vm console dev

# 4. Checkpoint de sistema — memória E disco, com a VM a correr
delonix vm snapshot dev limpa
delonix vm snapshots dev

# 5. Estragar alguma coisa lá dentro e voltar atrás
delonix vm restore dev limpa

delonix vm rm dev</code></pre>
<p class="note"><strong>Verificação:</strong> o passo 4 devolve sem erro com a
VM A CORRER. Um snapshot de uma VM parada falha de propósito — o
<code>vm stop</code> faz <em>undefine</em> do domínio para não deixar
órfãos.</p>"""),

    ("lab-6", "A tua própria imagem de VM, com VMfile", "avançado", """
<p>Objectivo: construir uma imagem qcow2 à tua medida, como se constrói uma
imagem de container.</p>
<pre><code>mkdir -p lab-vm &amp;&amp; cd lab-vm

# Scaffold que CONSTRÓI COMO ESTÁ — apaga o que não precisares
delonix vm init --vmfile --name minha-base
cat VMfile

# Precisa de libguestfs no host: sudo apt install libguestfs-tools
delonix vm build -t minha-base:1.0 .
delonix vm ls

# Um RUN com `apt-get install` precisa de rede no convidado, e pede-se:
#   delonix vm build --network -t minha-base:1.0 .

# Arrancar a partir dela
delonix vm create teste --disk-image minha-base:1.0 --ssh-key @~/.ssh/id_ed25519.pub

# …ou a partir de um qcow2 publicado por ti, sem passar pelo store
delonix vm create outra --url-img https://o-teu-bucket/imagem.qcow2

delonix vm rm teste; cd ..; rm -rf lab-vm</code></pre>
<p class="note"><strong>Verificação:</strong> o build imprime
<code>[1/1] stage-1: FROM ubuntu:24.04</code> e verifica o checksum da cloud
image. Com <code>--url-img</code>, se não houver <code>&lt;url&gt;.sha256</code>
publicado, o motor <em>diz</em> que está a confiar só no TLS — em vez de calar.
A chave que injectas vai para a conta <code>delonix</code>, não para a conta
default da distro — o bloco de próximos passos do <code>vm create</code>
imprime o <code>ssh</code> exacto.</p>"""),

    ("lab-7", "Kubernetes sem Docker", "avançado", """
<p>Objectivo: um cluster local cujo runtime dos nós É o Delonix.</p>
<pre><code># 1. Preflight — falha em milissegundos se faltar a delegação de `cpu`,
#    em vez de descarregar 425 MB para morrer aos 90 segundos
delonix system setup

# 2. O cluster
delonix cluster create --name lab

# 3. Falar com ele
export KUBECONFIG=$(delonix cluster ls -o json | jq -r '.[0].kubeconfig')
kubectl get nodes -o wide

# 4. Levar uma imagem TUA para dentro dos nós, sem registo nenhum
delonix build -t app:dev .
delonix cluster load app:dev --name lab
kubectl run app --image=app:dev --image-pull-policy=Never

# 5. A prova que interessa
kubectl get pod app -w

delonix cluster delete --name lab</code></pre>
<p class="note"><strong>Verificação:</strong> o passo 5 chega a
<code>Running</code>. Nem o <code>ctr images import</code> nem o
<code>crictl images</code> provam isto — os dois já reportaram sucesso sobre
uma imagem que o kubelet depois não resolvia.</p>"""),
]

# Tradução EN de `LABS` — dict por âncora (title, level, body), para não
# depender de ordem posicional. Nomes de recursos de exemplo (redes/
# containers arbitrários, ex.: "loja"→"shop") traduzem-se também, por
# naturalidade de leitura; os comandos em si nunca mudam.
LABS_EN = {
    "lab-1": ("First service, in 60 seconds", "beginner", """
<p>Goal: understand the <code>run → ps → logs → exec → rm</code> cycle and see
that there's no daemon underneath.</p>
<pre><code># 1. A web service, in the background, published on host port 8080
delonix container run -d --name web -p 8080:80 nginx:alpine

# 2. Confirm — STATUS shows how long it's been up
delonix ps

# 3. Proof there's no daemon: NO resident engine process
pgrep -a delonix || echo "no daemon — the container is a child of the host's init"

# 4. Talk to it
curl -s localhost:8080 | head -3

# 5. Go inside
delonix exec -it web sh -c 'hostname; id; ls /etc/nginx'

# 6. See what it wrote
delonix logs web | tail -5

# 7. Clean up
delonix rm -f web</code></pre>
<p class="note"><strong>Verification:</strong> step 3 prints no engine process
at all. A container running with no resident supervisor is the fundamental
difference from Docker, and you can see it here in two lines.</p>"""),

    "lab-2": ("Resource limits that actually take", "beginner", """
<p>Goal: find out whether this host enforces limits — and what to do when it
doesn't. It's mistake #1 for anyone starting out in rootless.</p>
<pre><code># 1. Ask BEFORE assuming
delonix system setup

# 2. If it says INERT or PARTIAL, the 1st fix needs no root at all
systemd-run --user --scope -p Delegate=yes -- delonix system setup
#   … and you run the following steps INSIDE that scope.
#   Only if it STILL says `cpu` is missing:
#     sudo delonix system setup --delegate   (+ logout / login)

# 3. A container with a memory ceiling
delonix container run -d --name limited -m 128M alpine sleep 300

# 4. The proof: read the REAL cgroup, not what the command says
PID=$(delonix container inspect limited | jq -r .pid)
cat /sys/fs/cgroup$(awk -F: '/^0::/{print $3}' /proc/$PID/cgroup)/memory.max

delonix rm -f limited</code></pre>
<p class="note"><strong>Verification:</strong> step 4 prints
<code>134217728</code> (128 MiB), not <code>max</code>. If it prints
<code>max</code>, delegation isn't set up — and the container runs with
<em>no</em> limit at all, silently. That's why step 1 exists.</p>
<p class="note"><strong>Note:</strong> <code>cpuset</code> and <code>io</code>
almost always show up as <em>absent</em>, and that's correct — on a stock
Ubuntu, root's <code>user.slice</code> only passes <code>cpu memory
pids</code> down. Nothing here needs them.</p>"""),

    "lab-3": ("Two apps that talk to each other by name", "intermediate", """
<p>Goal: user network, internal DNS, and isolation — with zero DNS
configuration.</p>
<pre><code># 1. Your own network
delonix network create shop

# 2. Database and app, both on it
delonix container run -d --name db  --net shop -e POSTGRES_PASSWORD=x postgres:16-alpine
delonix container run -d --name app --net shop -p 8080:80 nginx:alpine

# 3. The app resolves db BY NAME, no /etc/hosts, no env vars
delonix exec app sh -c 'getent hosts db; nc -z db 5432 && echo "port open"'

# 4. Lock db down to everyone except app (DIRECTED reachability)
cat &gt; dep.yaml &lt;&lt;'EOF'
apiVersion: delonix.io/v1
kind: Dependency
metadata:
  name: app-knows-db
spec:
  from: app
  to: db
  ports: ["5432"]
EOF
delonix stack apply -f dep.yaml

# 5. Prove it: a third container on the SAME network can no longer reach db
delonix container run --rm --net shop alpine sh -c 'nc -z -w2 db 5432 || echo BLOCKED'

delonix rm -f db app; delonix network rm shop; rm dep.yaml</code></pre>
<p class="note"><strong>Verification:</strong> step 3 says "port open" and
step 5 says "BLOCKED". Same network, two different outcomes — that's what
<code>kind: Dependency</code> does that a plain network alone doesn't.</p>"""),

    "lab-4": ("From Dockerfile to image, no daemon", "intermediate", """
<p>Goal: build an image and run what you built.</p>
<pre><code>mkdir -p lab-build &amp;&amp; cd lab-build
cat &gt; Delonixfile &lt;&lt;'EOF'
FROM alpine:latest
RUN apk add --no-cache curl
COPY hello.txt /hello.txt
HEALTHCHECK CMD test -f /hello.txt
CMD ["sh", "-c", "cat /hello.txt; sleep 300"]
EOF
echo "built with delonix" &gt; hello.txt

# `Delonixfile` is looked for before `Dockerfile` — no -f needed
delonix build -t my-app:1.0 .

# `--wait` blocks until HEALTHCHECK passes: without it you'd have to
# write `until ...; do sleep 1; done` yourself, and get it wrong
delonix run -d --name a1 --wait --health-interval 2 my-app:1.0
delonix ps

delonix rm -f a1; delonix image rm my-app:1.0; cd ..; rm -rf lab-build</code></pre>
<p class="note"><strong>Verification:</strong> <code>ps</code> shows
<code>(healthy)</code> in the STATUS column. The engine is probing the
container on its own — no systemd, unlike rootless Podman.</p>"""),

    "lab-5": ("A real microVM", "intermediate", """
<p>Goal: a full VM with the same CLI as containers.</p>
<pre><code># 1. With no local image, create downloads the official one on its own
delonix vm create dev

# 2. Where it landed, and with what IP
delonix vm ls
delonix vm describe dev

# 3. Go in (back to the host: Ctrl+])
delonix vm console dev

# 4. System checkpoint — memory AND disk, VM still running
delonix vm snapshot dev clean
delonix vm snapshots dev

# 5. Break something inside and roll back
delonix vm restore dev clean

delonix vm rm dev</code></pre>
<p class="note"><strong>Verification:</strong> step 4 returns with no error
and the VM STILL RUNNING. A snapshot of a stopped VM fails on purpose —
<code>vm stop</code> <em>undefines</em> the domain to avoid leaving orphans.</p>"""),

    "lab-6": ("Your own VM image, with a VMfile", "advanced", """
<p>Goal: build your own qcow2 image, the same way you build a container
image.</p>
<pre><code>mkdir -p lab-vm &amp;&amp; cd lab-vm

# Scaffold that BUILDS AS-IS — delete what you don't need
delonix vm init --vmfile --name my-base
cat VMfile

# Needs libguestfs on the host: sudo apt install libguestfs-tools
delonix vm build -t my-base:1.0 .
delonix vm ls

# A RUN with `apt-get install` needs guest networking, so ask for it:
#   delonix vm build --network -t my-base:1.0 .

# Boot from it
delonix vm create test --disk-image my-base:1.0 --ssh-key @~/.ssh/id_ed25519.pub

# …or from a qcow2 you published yourself, bypassing the store
delonix vm create other --url-img https://your-bucket/image.qcow2

delonix vm rm test; cd ..; rm -rf lab-vm</code></pre>
<p class="note"><strong>Verification:</strong> the build prints
<code>[1/1] stage-1: FROM ubuntu:24.04</code> and verifies the cloud image's
checksum. With <code>--url-img</code>, if there's no published
<code>&lt;url&gt;.sha256</code>, the engine <em>says</em> it's trusting TLS
alone — instead of staying quiet. The key you inject goes to the
<code>delonix</code> account, not the distro's default account — the
"next steps" block from <code>vm create</code> prints the exact
<code>ssh</code> command.</p>"""),

    "lab-7": ("Kubernetes with no Docker", "advanced", """
<p>Goal: a local cluster whose node runtime IS Delonix.</p>
<pre><code># 1. Preflight — fails in milliseconds if `cpu` delegation is missing,
#    instead of downloading 425 MB just to die at the 90-second mark
delonix system setup

# 2. The cluster
delonix cluster create --name lab

# 3. Talk to it
export KUBECONFIG=$(delonix cluster ls -o json | jq -r '.[0].kubeconfig')
kubectl get nodes -o wide

# 4. Get an image of YOURS into the nodes, with no registry at all
delonix build -t app:dev .
delonix cluster load app:dev --name lab
kubectl run app --image=app:dev --image-pull-policy=Never

# 5. The proof that matters
kubectl get pod app -w

delonix cluster delete --name lab</code></pre>
<p class="note"><strong>Verification:</strong> step 5 reaches
<code>Running</code>. Neither <code>ctr images import</code> nor
<code>crictl images</code> prove this — both have reported success on an
image the kubelet then couldn't resolve.</p>"""),
}


def labs_page():
    body = ["<h1>Laboratórios</h1>",
            bi("p",
               "Sessões curtas, do zero a um resultado que se verifica. "
               "Cada uma inclui a limpeza — nenhuma deixa nada para trás.",
               "Short sessions, from zero to a verifiable result. "
               "Each one includes cleanup — none of them leaves anything behind.",
               cls="tagline"),
            bi("p",
               "A verificação no fim de cada lab não é decorativa: é o que separa "
               "«copiei comandos» de «sei o que isto faz». Onde um passo pode falhar em "
               "silêncio, o lab diz exactamente o que olhar.",
               "The verification at the end of each lab isn't decorative: it's what tells apart "
               "\"I copied commands\" from \"I know what this does.\" Where a step could fail "
               "silently, the lab tells you exactly what to look at.")]
    for anchor, title, level, html_body in LABS:
        en = LABS_EN.get(anchor)
        if en:
            title_en, level_en, body_en = en
            heading = (
                bi("span", html.escape(title), html.escape(title_en))
                + " <span class='badge'>"
                + bi("span", html.escape(level), html.escape(level_en))
                + "</span>"
            )
            body.append(f"<h2 id='{anchor}'>{heading}</h2>")
            body.append(bi("div", html_body, body_en))
        else:
            body.append(f"<h2 id='{anchor}'>{html.escape(title)} "
                        f"<span class='badge'>{html.escape(level)}</span></h2>")
            body.append(html_body)
    page("labs.html", "Laboratórios", "\n".join(body))


CLOUD = """
<h1>cloud-init, cloud image e Cloud Hypervisor</h1>
<p class="tagline">Três nomes que aparecem juntos e fazem coisas diferentes.
Confundi-los é a causa mais comum de uma VM que arranca e não faz nada — ou que
não arranca de todo.</p>

<h2>Os três em uma frase</h2>
<table>
<tr><th>Peça</th><th>O que é</th><th>Quando aparece</th></tr>
<tr><td><strong>cloud image</strong> (cloud-img)</td>
    <td>Um disco <code>.qcow2</code> publicado por uma distro, já instalado e
    pronto a arrancar. Não tem utilizador, nem password, nem chave SSH.</td>
    <td>O <code>FROM</code> de um <code>VMfile</code>, e o disco base de
    <code>vm create</code>.</td></tr>
<tr><td><strong>cloud-init</strong></td>
    <td>O programa que corre DENTRO dessa imagem no primeiro arranque e a
    personaliza: hostname, utilizadores, chaves, rede, comandos.</td>
    <td>É o que torna a cloud image utilizável. Sem ele, ficas com um disco
    genérico onde não consegues entrar.</td></tr>
<tr><td><strong>Cloud Hypervisor</strong></td>
    <td>Um VMM — o programa do HOST que executa a VM. Alternativa ao
    QEMU/libvirt, feito para microVMs.</td>
    <td><code>--backend cloud-hypervisor</code>, e obrigatório para
    <code>type: microvm</code>.</td></tr>
</table>
<p>Analogia com containers, que ajuda mais do que qualquer definição:
a <strong>cloud image</strong> é a imagem, o <strong>cloud-init</strong> é o
<code>ENTRYPOINT</code> da primeira execução, e o
<strong>Cloud Hypervisor</strong> é o runtime — o equivalente ao
<code>runc</code>.</p>

<h2>1 · Cloud image — o disco</h2>
<p>Cada distro publica um qcow2 pré-instalado, pequeno (algumas centenas de MB)
e com o cloud-init já lá dentro. O motor sabe descarregar três famílias e
<strong>verifica sempre o checksum</strong> — nunca aceita um download sem o
confrontar.</p>
<pre><code># Num VMfile
FROM ubuntu:24.04        # cloud-images.ubuntu.com
FROM debian:bookworm     # cloud.debian.org
FROM rocky:9             # dl.rockylinux.org

# Ou directamente, sem VMfile nenhum
delonix vm create dev --url-img https://.../imagem.qcow2</code></pre>
<p class="note"><strong>Os três publicam checksums de maneiras diferentes</strong>,
e isso está tratado: Ubuntu usa <code>SHA256SUMS</code> no formato GNU, Debian
publica <strong>só</strong> <code>SHA512SUMS</code> (não há SHA256 nenhum), e
Rocky usa um <code>.CHECKSUM</code> por ficheiro no formato BSD
<code>SHA256 (ficheiro) = hash</code>. Com <code>--url-img</code>, o motor
procura um <code>&lt;url&gt;.sha256</code> ao lado; se não existir,
<strong>diz</strong> que está a confiar só no TLS em vez de calar.</p>
<p><strong>O disco vem pequeno de propósito</strong> — tipicamente 2 GB. Cresce-o
antes de instalares seja o que for, com <code>SIZE 20G</code> no VMfile: crescer
depois de um <code>RUN</code> ter enchido o disco é tarde.</p>

<h2>2 · cloud-init — a personalização do primeiro arranque</h2>
<p>Uma cloud image acabada de descarregar não tem conta nenhuma. O cloud-init
procura um <em>datasource</em> no arranque, lê de lá o <code>user-data</code>, e
aplica-o. O Delonix usa o datasource <strong>NoCloud</strong>: gera um ISO por
instância e liga-o à VM.</p>
<pre><code># O motor gera o seed sozinho quando lhe dás qualquer um destes
delonix vm create dev --hostname dev-01 --ssh-key ~/.ssh/id_ed25519.pub
delonix vm create dev --user-data ./user-data.yaml
delonix vm create dev --seed ./o-meu-seed.iso   # ISO já feito por ti</code></pre>
<p>Um <code>user-data</code> mínimo e completo:</p>
<pre><code>#cloud-config
hostname: dev-01
users:
  - name: delonix
    groups: [sudo]
    shell: /bin/bash
    sudo: ['ALL=(ALL) NOPASSWD:ALL']
    ssh_authorized_keys:
      - ssh-ed25519 AAAA... o-teu-email@exemplo

package_update: true
packages: [nginx]

runcmd:
  - [systemctl, enable, --now, nginx]

final_message: "pronto em $UPTIME segundos"</code></pre>
<p class="note"><strong>Duas camadas, e a distinção importa.</strong> O
<code>CLOUDINIT</code> de um VMfile é assado NA IMAGEM (fica em
<code>/etc/cloud/cloud.cfg.d</code>) — é o comportamento por omissão de
<em>todas</em> as VMs feitas a partir dela. O <code>--user-data</code> do
<code>vm create</code> é POR INSTÂNCIA e assenta por cima. Uma é a receita da
imagem, a outra é a configuração daquela VM.</p>
<p class="note"><strong>Armadilha real, já paga:</strong> um <code>kind: Vm</code>
sem seed nenhum fazia o cloud-init saltar a fase de rede, e a VM ficava sem IP e
sem rota (<em>Network is unreachable</em> lá dentro). Por isso o motor gera
<strong>sempre</strong> um seed mínimo, mesmo quando não pedes nada — não é
opcional.</p>
<p>E o cloud-init só corre uma vez: guarda um marcador em
<code>/var/lib/cloud</code>. Um <code>vm restart</code> não o volta a executar —
para reaplicar, ou limpas esse estado dentro da VM ou crias outra.</p>

<h2>3 · Cloud Hypervisor — o VMM</h2>
<p>Quem executa a VM. O Delonix tem dois backends por trás do mesmo trait, e a
escolha não é cosmética:</p>
<table>
<tr><th></th><th>Cloud Hypervisor</th><th>libvirt/QEMU</th></tr>
<tr><td>Arranque</td><td>Muito rápido (dezenas de ms) — feito para microVMs</td>
    <td>Segundos; máquina completa emulada</td></tr>
<tr><td>Dispositivos</td><td>Só virtio, mínimo</td><td>Tudo (TPM, vídeo, USB, …)</td></tr>
<tr><td>Rede</td><td><code>tap</code> na SDN do holder — <strong>alcança os
    containers por IP directo</strong></td>
    <td><code>virbr0</code>, no netns do HOST — outra L2</td></tr>
<tr><td>Isolamento por namespace</td><td>Sim</td>
    <td><strong>Não</strong> — <code>--namespace</code> é recusado com erro, não
    aceite-e-ignorado</td></tr>
<tr><td>Snapshots</td><td>Não implementado (fail-closed, com erro que aponta para o libvirt)</td>
    <td><code>vm snapshot</code>/<code>restore</code>: memória + disco</td></tr>
<tr><td>Arranque de cloud image</td><td>Precisa de firmware
    (<code>hypervisor-fw</code>/EDK2) ou de kernel+initrd directos</td>
    <td>Arranca a cloud image tal como está</td></tr>
</table>
<pre><code>delonix vm create micro --backend cloud-hypervisor --firmware /usr/share/hypervisor-fw
delonix vm create pesada --backend libvirt          # default quando CH não está instalado</code></pre>

<h2>Qual usar, e quando</h2>
<table>
<tr><th>Se queres…</th><th>Usa</th></tr>
<tr><td>Uma VM de trabalho, com snapshots e consola</td>
    <td><code>--backend libvirt</code> (é o que a golden do Kubernetes exige — não arranca em CH)</td></tr>
<tr><td>Arranque em milissegundos, isolamento por namespace, ou alcançar containers por IP</td>
    <td><code>--backend cloud-hypervisor</code> + firmware</td></tr>
<tr><td>Personalizar UMA VM</td><td>cloud-init por instância: <code>--hostname</code>/<code>--ssh-key</code>/<code>--user-data</code></td></tr>
<tr><td>Personalizar TODAS as VMs de um modelo</td><td>Um <code>VMfile</code> com <code>CLOUDINIT</code>, e <code>vm build</code></td></tr>
<tr><td>Um disco à tua medida, publicável</td><td><code>vm init --vmfile</code> → <code>vm build</code> → <code>vm push</code></td></tr>
</table>

<h2>Onde isto falha, e o que ver</h2>
<table>
<tr><th>Sintoma</th><th>Quase sempre é</th></tr>
<tr><td>A VM arranca e não entras por SSH</td>
    <td>Sem seed de cloud-init — não há utilizador nenhum. Passa
    <code>--ssh-key</code>.</td></tr>
<tr><td><code>IP &lt;none&gt;</code> para sempre</td>
    <td>libvirt caiu em modo <em>user</em> (SLIRP), cujo IP é invisível. Junta-te
    ao grupo <code>libvirt</code> — o motor avisa-o no <code>create</code>.</td></tr>
<tr><td>A VM não arranca em Cloud Hypervisor</td>
    <td>Falta o firmware. CH não faz boot BIOS: precisa de
    <code>--firmware</code> ou de <code>--kernel</code>+<code>--initrd</code>.</td></tr>
<tr><td>O disco enche a meio do <code>vm build</code></td>
    <td><code>SIZE</code> em falta, ou depois de um <code>RUN</code>. É
    propriedade da stage, e corre antes de tudo.</td></tr>
<tr><td>Mudaste o <code>user-data</code> e nada muda</td>
    <td>O cloud-init já correu naquela VM. Cria outra, ou limpa
    <code>/var/lib/cloud</code> lá dentro.</td></tr>
</table>

<h2>Referências</h2>
<ul>
<li><a href="https://cloudinit.readthedocs.io/">Documentação do cloud-init</a> —
    e em especial os
    <a href="https://cloudinit.readthedocs.io/en/latest/reference/examples.html">exemplos de user-data</a>
    e o <a href="https://cloudinit.readthedocs.io/en/latest/reference/datasources/nocloud.html">datasource NoCloud</a>,
    que é o que o Delonix usa.</li>
<li><a href="https://cloud-images.ubuntu.com/">Cloud images do Ubuntu</a> ·
    <a href="https://cloud.debian.org/images/cloud/">do Debian</a> ·
    <a href="https://dl.rockylinux.org/pub/rocky/">do Rocky</a></li>
<li><a href="https://www.cloudhypervisor.org/">Cloud Hypervisor</a> e o
    <a href="https://github.com/cloud-hypervisor/rust-hypervisor-firmware">rust-hypervisor-firmware</a>
    (o <code>--firmware</code> que uma cloud image precisa para arrancar em CH).</li>
<li><a href="https://libvirt.org/formatdomain.html">Formato do domínio libvirt</a> —
    útil se usares os escape-hatches <code>libvirtXml</code>/<code>libvirtXmlOverlay</code>
    do <code>kind: Vm</code>.</li>
</ul>
"""

CLOUD_EN = """
<h1>cloud-init, cloud image and Cloud Hypervisor</h1>
<p class="tagline">Three names that show up together and do different things.
Mixing them up is the most common reason a VM boots and does nothing — or doesn't
boot at all.</p>

<h2>The three, in one sentence each</h2>
<table>
<tr><th>Piece</th><th>What it is</th><th>Where it shows up</th></tr>
<tr><td><strong>cloud image</strong> (cloud-img)</td>
    <td>A <code>.qcow2</code> disk published by a distro, already installed and
    ready to boot. No user, no password, no SSH key.</td>
    <td>The <code>FROM</code> of a <code>VMfile</code>, and the base disk for
    <code>vm create</code>.</td></tr>
<tr><td><strong>cloud-init</strong></td>
    <td>The program that runs INSIDE that image on first boot and
    customizes it: hostname, users, keys, networking, commands.</td>
    <td>It's what makes the cloud image usable. Without it you're left with a
    generic disk you can't log into.</td></tr>
<tr><td><strong>Cloud Hypervisor</strong></td>
    <td>A VMM — the HOST-side program that runs the VM. An alternative to
    QEMU/libvirt, built for microVMs.</td>
    <td><code>--backend cloud-hypervisor</code>, and required for
    <code>type: microvm</code>.</td></tr>
</table>
<p>A container analogy helps more than any definition:
the <strong>cloud image</strong> is the image, <strong>cloud-init</strong> is the
<code>ENTRYPOINT</code> of the first run, and
<strong>Cloud Hypervisor</strong> is the runtime — the equivalent of
<code>runc</code>.</p>

<h2>1 · Cloud image — the disk</h2>
<p>Every distro publishes a small (a few hundred MB), pre-installed qcow2 with
cloud-init already inside. The engine knows how to download three families and
<strong>always verifies the checksum</strong> — it never accepts a download without
checking it against one.</p>
<pre><code># In a VMfile
FROM ubuntu:24.04        # cloud-images.ubuntu.com
FROM debian:bookworm     # cloud.debian.org
FROM rocky:9             # dl.rockylinux.org

# Or directly, with no VMfile at all
delonix vm create dev --url-img https://.../image.qcow2</code></pre>
<p class="note"><strong>The three publish checksums differently</strong>,
and that's handled: Ubuntu uses <code>SHA256SUMS</code> in GNU format, Debian
publishes <strong>only</strong> <code>SHA512SUMS</code> (no SHA256 at all), and
Rocky uses a per-file <code>.CHECKSUM</code> in BSD format
(<code>SHA256 (file) = hash</code>). With <code>--url-img</code>, the engine
looks for a <code>&lt;url&gt;.sha256</code> next to it; if it doesn't exist, it
<strong>says so</strong> — trusting TLS alone — instead of staying quiet.</p>
<p><strong>The disk comes small on purpose</strong> — typically 2 GB. Grow it
before installing anything, with <code>SIZE 20G</code> in the VMfile: growing it
after a <code>RUN</code> has already filled the disk is too late.</p>

<h2>2 · cloud-init — first-boot customization</h2>
<p>A freshly downloaded cloud image has no account at all. cloud-init
looks for a <em>datasource</em> at boot, reads <code>user-data</code> from it, and
applies it. Delonix uses the <strong>NoCloud</strong> datasource: it generates a
per-instance ISO and attaches it to the VM.</p>
<pre><code># The engine generates the seed on its own when you give it any of these
delonix vm create dev --hostname dev-01 --ssh-key ~/.ssh/id_ed25519.pub
delonix vm create dev --user-data ./user-data.yaml
delonix vm create dev --seed ./my-seed.iso   # ISO you already built yourself</code></pre>
<p>A minimal, complete <code>user-data</code>:</p>
<pre><code>#cloud-config
hostname: dev-01
users:
  - name: delonix
    groups: [sudo]
    shell: /bin/bash
    sudo: ['ALL=(ALL) NOPASSWD:ALL']
    ssh_authorized_keys:
      - ssh-ed25519 AAAA... your-email@example

package_update: true
packages: [nginx]

runcmd:
  - [systemctl, enable, --now, nginx]

final_message: "ready in $UPTIME seconds"</code></pre>
<p class="note"><strong>Two layers, and the distinction matters.</strong> A
VMfile's <code>CLOUDINIT</code> is baked INTO THE IMAGE (it lands in
<code>/etc/cloud/cloud.cfg.d</code>) — it's the default behavior of
<em>every</em> VM made from it. <code>vm create</code>'s <code>--user-data</code>
is PER INSTANCE and sits on top. One is the image's recipe, the other is that
particular VM's configuration.</p>
<p class="note"><strong>Real trap, already paid for:</strong> a <code>kind: Vm</code>
with no seed at all made cloud-init skip the networking phase, leaving the VM
with no IP and no route (<em>Network is unreachable</em> inside). That's why the
engine <strong>always</strong> generates a minimal seed, even when you ask for
nothing — it isn't optional.</p>
<p>And cloud-init only runs once: it keeps a marker in
<code>/var/lib/cloud</code>. A <code>vm restart</code> doesn't re-run it —
to reapply it, either clear that state inside the VM or create another one.</p>

<h2>3 · Cloud Hypervisor — the VMM</h2>
<p>The thing that runs the VM. Delonix has two backends behind the same trait, and
the choice isn't cosmetic:</p>
<table>
<tr><th></th><th>Cloud Hypervisor</th><th>libvirt/QEMU</th></tr>
<tr><td>Boot</td><td>Very fast (tens of ms) — built for microVMs</td>
    <td>Seconds; a full emulated machine</td></tr>
<tr><td>Devices</td><td>virtio only, minimal</td><td>Everything (TPM, video, USB, …)</td></tr>
<tr><td>Networking</td><td><code>tap</code> on the holder's SDN — <strong>reaches
    containers by direct IP</strong></td>
    <td><code>virbr0</code>, in the HOST's netns — a different L2</td></tr>
<tr><td>Per-namespace isolation</td><td>Yes</td>
    <td><strong>No</strong> — <code>--namespace</code> is refused with an error,
    never accepted-and-ignored</td></tr>
<tr><td>Snapshots</td><td>Not implemented (fail-closed, with an error pointing at libvirt)</td>
    <td><code>vm snapshot</code>/<code>restore</code>: memory + disk</td></tr>
<tr><td>Booting a cloud image</td><td>Needs firmware
    (<code>hypervisor-fw</code>/EDK2) or a direct kernel+initrd</td>
    <td>Boots the cloud image as-is</td></tr>
</table>
<pre><code>delonix vm create micro --backend cloud-hypervisor --firmware /usr/share/hypervisor-fw
delonix vm create heavy --backend libvirt          # default when CH isn't installed</code></pre>

<h2>Which to use, and when</h2>
<table>
<tr><th>If you want…</th><th>Use</th></tr>
<tr><td>A workhorse VM, with snapshots and a console</td>
    <td><code>--backend libvirt</code> (it's what the Kubernetes golden image needs — it won't boot in CH)</td></tr>
<tr><td>Millisecond boot, per-namespace isolation, or to reach containers by IP</td>
    <td><code>--backend cloud-hypervisor</code> + firmware</td></tr>
<tr><td>To customize ONE VM</td><td>Per-instance cloud-init: <code>--hostname</code>/<code>--ssh-key</code>/<code>--user-data</code></td></tr>
<tr><td>To customize EVERY VM from one template</td><td>A <code>VMfile</code> with <code>CLOUDINIT</code>, and <code>vm build</code></td></tr>
<tr><td>Your own publishable disk</td><td><code>vm init --vmfile</code> → <code>vm build</code> → <code>vm push</code></td></tr>
</table>

<h2>Where this breaks, and what to check</h2>
<table>
<tr><th>Symptom</th><th>Almost always</th></tr>
<tr><td>The VM boots and you can't SSH in</td>
    <td>No cloud-init seed — there's no user at all. Pass
    <code>--ssh-key</code>.</td></tr>
<tr><td><code>IP &lt;none&gt;</code> forever</td>
    <td>libvirt fell back to <em>user</em> mode (SLIRP), whose IP is invisible. Join
    the <code>libvirt</code> group — the engine warns about it at <code>create</code>.</td></tr>
<tr><td>The VM won't boot under Cloud Hypervisor</td>
    <td>Missing firmware. CH doesn't do BIOS boot: it needs
    <code>--firmware</code> or <code>--kernel</code>+<code>--initrd</code>.</td></tr>
<tr><td>The disk fills up mid-<code>vm build</code></td>
    <td>Missing <code>SIZE</code>, or set after a <code>RUN</code>. It's a
    stage property, and runs before everything else.</td></tr>
<tr><td>You changed <code>user-data</code> and nothing changes</td>
    <td>cloud-init already ran on that VM. Create another one, or clear
    <code>/var/lib/cloud</code> inside it.</td></tr>
</table>

<h2>References</h2>
<ul>
<li><a href="https://cloudinit.readthedocs.io/">cloud-init documentation</a> —
    especially the
    <a href="https://cloudinit.readthedocs.io/en/latest/reference/examples.html">user-data examples</a>
    and the <a href="https://cloudinit.readthedocs.io/en/latest/reference/datasources/nocloud.html">NoCloud datasource</a>,
    which is what Delonix uses.</li>
<li><a href="https://cloud-images.ubuntu.com/">Ubuntu cloud images</a> ·
    <a href="https://cloud.debian.org/images/cloud/">Debian's</a> ·
    <a href="https://dl.rockylinux.org/pub/rocky/">Rocky's</a></li>
<li><a href="https://www.cloudhypervisor.org/">Cloud Hypervisor</a> and
    <a href="https://github.com/cloud-hypervisor/rust-hypervisor-firmware">rust-hypervisor-firmware</a>
    (the <code>--firmware</code> a cloud image needs to boot under CH).</li>
<li><a href="https://libvirt.org/formatdomain.html">libvirt domain format</a> —
    useful if you use the <code>kind: Vm</code> escape hatches
    <code>libvirtXml</code>/<code>libvirtXmlOverlay</code>.</li>
</ul>
"""


def kinds_page():
    body = [f"<h1>Kinds do manifesto</h1>{bi('p', 'Cada Kind com um template COMPLETO e funcional — '
            'todos os campos, com os defaults e um comentário. Aplica um só com '
            '<code>delonix &lt;grupo&gt; apply -f</code>, ou todos de uma vez com <code>delonix stack apply</code> '
            '(ordem por dependência: Secret → Network → Volume → Storage → ShareVolume → Image → Vm → Container → '
            'Pod → Ingress/Egress → Dependency → HTTPRoute → Tunnel).',
            'Each Kind with a COMPLETE, functional template — '
            'every field, with defaults and a comment. Apply just one with '
            '<code>delonix &lt;group&gt; apply -f</code>, or all at once with <code>delonix stack apply</code> '
            '(dependency order: Secret → Network → Volume → Storage → ShareVolume → Image → Vm → Container → '
            'Pod → Ingress/Egress → Dependency → HTTPRoute → Tunnel).', cls='tagline')}"]
    body.append(bi('p',
        "Semântica <em>garante-presente</em> (idempotente por nome), não um reconciliador: sem diffing, "
        "rollout nem rollback — fail-fast, o que já foi aplicado fica. Os templates abaixo são os ficheiros "
        "reais em <a href='https://github.com/angolardevops/delonix-runtime/tree/main/examples'><code>examples/</code></a>.",
        "<em>Ensure-present</em> semantics (idempotent by name), not a reconciler: no diffing, "
        "rollout or rollback — fail-fast, whatever was already applied stays applied. The templates below are the "
        "real files in <a href='https://github.com/angolardevops/delonix-runtime/tree/main/examples'><code>examples/</code></a>."))
    for (kind, fname, intro), intro_en in zip(KINDS_DOC, KINDS_DOC_EN):
        anchor = kind.split()[0].lower()
        body.append(f"<h2 id='{anchor}'>{html.escape(kind)}</h2>")
        body.append(bi('p', intro, intro_en))
        path = os.path.join(ROOT, "..", "examples", fname)
        try:
            yaml = open(path).read().strip()
        except OSError:
            yaml = f"# (exemplo em falta: examples/{fname})"
        body.append(f"<pre><code>{html.escape(yaml)}</code></pre>")
    page("kinds.html", "Kinds do manifesto", "\n".join(body))


def cheatsheet_page():
    cheatsheet_tagline = bi(
        "p",
        "Todos os grupos de comandos e subcomandos, num só sítio. Gerado do <code>--help</code> real do binário.",
        "All command groups and subcommands, in one place. Generated from the real <code>--help</code> of the binary.",
        cls="tagline",
    )
    body = [f"<h1>Cheatsheet</h1>{cheatsheet_tagline}"]
    body.append(f"<h2>{bi('span', 'Tarefas comuns', 'Common tasks')}</h2>")
    body.append(examples_html(CHEAT_TASKS, CHEAT_TASKS_EN))
    body.append(f"<h2>{bi('span', 'Todos os grupos', 'All groups')}</h2>")
    order = list(GROUPS.keys()) + ["cri"]
    for g in order:
        title = GROUPS[g]["title"] if g in GROUPS else "delonix serve cri"
        href = f"comandos/{g}.html" if g in GROUPS else "cri.html"
        subs = subcommands_of(g)
        head = f"<h3 id='{g}'><a href='{href}'><code>{html.escape(title)}</code></a></h3>"
        if not subs:
            tl_en = GROUPS_EN.get(g, {}).get("tagline") if g in GROUPS else "Serves the CRI endpoint (runtime.v1) on a unix socket."
            tl = GROUPS[g]["tagline"] if g in GROUPS else "Serve o endpoint CRI (runtime.v1) num socket unix."
            if tl_en:
                body.append(head + bi("p", html.escape(tl), html.escape(tl_en)))
            else:
                body.append(head + f"<p>{html.escape(tl)}</p>")
            continue
        prefix = " ".join(group_argv(g)) if g in GROUPS else "serve cri"
        rows = "".join(
            f"<tr><td><code>{html.escape(prefix)} {html.escape(s)}</code></td><td>{html.escape(d)}</td></tr>"
            for s, d in subs
        )
        # `d` (a descrição curta) vem do `--help` REAL do binário, que já é EN
        # por omissão — não precisa de tradução própria, ao contrário da prosa
        # autoral desta página.
        table_head = bi("span", "Comando", "Command") + "</th><th>" + bi("span", "O que faz", "What it does")
        body.append(head + f"<table><tr><th>{table_head}</th></tr>{rows}</table>")
    body.append(f"<h2>{bi('span', 'Global', 'Global')}</h2>")
    body.append(bi("p",
        "<code>--l18n en|pt</code> — idioma da saída (EN por omissão; "
        "<code>pt</code> para pt_AO). <code>$DELONIX_ROOT</code> — raiz do estado. "
        "<code>delonix completion &lt;shell&gt;</code> — autocompletion.",
        "<code>--l18n en|pt</code> — output language (EN by default; "
        "<code>pt</code> for pt_AO). <code>$DELONIX_ROOT</code> — the state root. "
        "<code>delonix completion &lt;shell&gt;</code> — autocompletion."))
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
<pre><code>delonix net tunnel expose --name delonix-temp --provider pinggy --local-port 8080</code></pre>
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
<pre><code>delonix net tunnel rm delonix-temp
delonix container rm -f delonix-temp</code></pre>

<h2>O que isto provou</h2>
<table>
<tr><th>Comando</th><th>O que validou</th></tr>
<tr><td><code>delonix build</code></td><td>build multi-stage real (2 estágios, <code>COPY --from</code>), com rede no build</td></tr>
<tr><td><code>delonix container run -p</code></td><td>NAT userspace sem root, porta publicada no host</td></tr>
<tr><td><code>delonix container logs</code></td><td>observabilidade de um serviço real a correr</td></tr>
<tr><td><code>delonix net tunnel expose</code></td><td>tráfego REAL da internet pública a chegar a um container local, sem conta nem IP público</td></tr>
</table>
"""

TUTORIAL_EN = """
<h1>Full project: Delonix Temp</h1>
<p class="tagline">From zero to a service on the public internet — build, run, and a real URL, in 4
commands. Everything in this guide was actually run; the output is real, copied from the run.</p>

<p>A real-time API in <a href="https://fastapi.tiangolo.com/">FastAPI</a> — looks up the CURRENT
temperature of any city (via <a href="https://open-meteo.com/">Open-Meteo</a>, no API key) and
serves a page that refreshes itself every 30s. The goal is to walk the full Delonix cycle in one
small project: multi-stage <code>build</code> → <code>container
run</code> → expose to the internet with <code>tunnel</code>. The complete files are in
<a href="https://github.com/angolardevops/delonix-runtime/tree/main/examples/delonix-temp"><code>examples/delonix-temp/</code></a>
— <code>git clone</code> it and follow the steps below exactly as they are.</p>

<h2>1. The app</h2>
<p>Three files. <code>main.py</code> — two routes: <code>/api/weather/{city}</code> (geocodes the
city, asks for the current temperature, returns JSON) and <code>/</code> (an HTML page that calls
the API itself via <code>fetch</code> and redoes it every 30s):</p>
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
    return PAGE  # HTML with a &lt;script&gt; that fetch("/api/weather/"+city) every 30s
</code></pre>
<p>(full version, with the HTML page and <code>/health</code>, in the repo's real file.)</p>

<p><code>requirements.txt</code> — <code>fastapi</code>, <code>uvicorn[standard]</code>,
<code>httpx</code>, with pinned versions.</p>

<p><code>Delonixfile</code> — a <strong>multi-stage</strong> build: one stage installs the Python
dependencies, the other just copies the result plus the code — the final image carries no pip
cache and no build tools:</p>
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
<p>The ID at the end (<code>ef708d73f029</code>) is the image. <code>delonix image ls</code>
confirms the size — the final stage, with no build tools, ends up much smaller than if everything
were in a single <code>FROM</code>:</p>
<pre><code>delonix image ls</code></pre>
<div class="out"><pre><code>REPOSITORY:TAG     IMAGE ID       CREATED          SIZE
delonix-temp:1     ef708d73f029   just now         157.2 MiB
python:3.12-slim   25c5b8011a34   just now          41.2 MiB</code></pre></div>

<h2>3. Run it</h2>
<pre><code>delonix container run -d --name delonix-temp -p 8080:80 delonix-temp:1
curl -s http://localhost:8080/api/weather/Luanda</code></pre>
<div class="out"><pre><code>{"city":"Luanda","country":"Angola","temperature_c":20.2,"observed_at":"2026-07-23T19:00"}</code></pre></div>
<p>A REAL temperature, looked up live — not a fixed value. The container's logs confirm the
requests:</p>
<pre><code>delonix container logs delonix-temp</code></pre>
<div class="out"><pre><code>INFO:     Uvicorn running on http://0.0.0.0:80 (Press CTRL+C to quit)
INFO:     10.0.2.2:57786 - "GET /health HTTP/1.1" 200 OK
INFO:     10.0.2.2:57802 - "GET /api/weather/Luanda HTTP/1.1" 200 OK</code></pre></div>

<h2>4. Expose it to the internet</h2>
<p>A local port isn't enough — the goal is a URL anyone, on any network, can open. This is where
<a href="comandos/tunnel.html"><code>kind: Tunnel</code></a> comes in:</p>
<pre><code>delonix net tunnel expose --name delonix-temp --provider pinggy --local-port 8080</code></pre>
<div class="out"><pre><code>tunnel/delonix-temp: running — https://lfdhz-197-148-40-67.free.pinggy.net</code></pre></div>
<p>That URL is REAL — it's the one this guide got when the command was actually run. (Yours will
be different every time: the free provider assigns a new one each session.) Confirmation, from
the outside, without touching anything local:</p>
<pre><code>curl https://lfdhz-197-148-40-67.free.pinggy.net/api/weather/Luanda</code></pre>
<div class="out"><pre><code>{"city":"Luanda","country":"Angola","temperature_c":20.2,"observed_at":"2026-07-23T19:00"}</code></pre></div>
<p>The same JSON, this time arriving from outside the machine, through an SSH tunnel to a public
server (pinggy) and back — zero router configuration, zero public IP of your own, zero account.
Opening the URL in a browser shows the <em>Delonix Temp</em> page updating itself.</p>

<div class="callout">
<p><b>Going further:</b> with more than one service, put <a href="comandos/httproute.html"><code>kind:
HTTPRoute</code></a> in front (routing by <code>Host</code>/path to several containers) and point
<code>tunnel</code> at the PROXY'S PORT instead of directly at the container — one public URL, as
many backends as you need. See <code>examples/httproute.yaml</code> +
<code>examples/tunnel.yaml</code>.</p>
</div>

<h2>Cleanup</h2>
<pre><code>delonix net tunnel rm delonix-temp
delonix container rm -f delonix-temp</code></pre>

<h2>What this proved</h2>
<table>
<tr><th>Command</th><th>What it validated</th></tr>
<tr><td><code>delonix build</code></td><td>a real multi-stage build (2 stages, <code>COPY --from</code>), with build networking</td></tr>
<tr><td><code>delonix container run -p</code></td><td>userspace NAT with no root, port published on the host</td></tr>
<tr><td><code>delonix container logs</code></td><td>observability of a real service running</td></tr>
<tr><td><code>delonix net tunnel expose</code></td><td>REAL public internet traffic reaching a local container, no account, no public IP</td></tr>
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
    def card_tagline(n, g):
        en = GROUPS_EN.get(n, {}).get("tagline")
        if en:
            return bi("span", html.escape(g["tagline"]), html.escape(en))
        return html.escape(g["tagline"])
    cards = "".join(
        f'<div class="card"><b><a href="comandos/{n}.html">{html.escape(g["title"])}</a></b>'
        f'<p>{card_tagline(n, g)}</p></div>'
        for n, g in GROUPS.items()
    )
    index_html = bi("div", INDEX_PT, INDEX_EN).replace("{ver}", ver).replace("{cards}", cards)
    page("index.html", "Delonix Engine", index_html)
    cheatsheet_page()
    kinds_page()
    page("arquitectura.html", "Arquitectura", bi("div", ARCH, ARCH_EN))
    c4_page()
    page("cloud.html", "cloud-init, cloud image e Cloud Hypervisor", bi("div", CLOUD, CLOUD_EN))
    labs_page()
    page("cri.html", "CRI", bi("div", CRI, CRI_EN))
    page("comparacao.html", "Delonix vs Docker vs Podman", bi("div", COMPARE, COMPARE_EN))
    page("tutorial-delonix-temp.html", "Projecto completo: Delonix Temp", bi("div", TUTORIAL, TUTORIAL_EN))
    for n, g in GROUPS.items():
        group_page(n, g)
    open(os.path.join(ROOT, ".nojekyll"), "w").close()
    print(f"docs geradas (delonix {ver}) em {ROOT}")


if __name__ == "__main__":
    main()
