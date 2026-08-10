# Roteiro de vídeos — Delonix Runtime

Guião de gravação para a série de vídeos de lançamento à comunidade. Seis episódios curtos, cada
um autónomo, a levar alguém de "nunca ouvi falar disto" a "consigo publicar uma stack real" — sem
Docker instalado, em nenhum momento. Todos os comandos abaixo foram corridos e confirmados no host
de desenvolvimento antes de entrarem no guião.

Ordem pensada para binge-watch (cada um assume o anterior), mas cada vídeo também funciona
sozinho — quem procura só "Kubernetes sem Docker" no YouTube tem de perceber o episódio 05 sem ter
visto o 01. Por isso cada guião abaixo repete o essencial do contexto nos primeiros 20 segundos (o
"gancho").

## Visão geral

| # | Título | Foco | Duração |
|---|---|---|---|
| 01 | Delonix em 6 minutos | Instalar, correr o primeiro container, porquê sem Docker | 5–7 min |
| 02 | Volumes, redes e build | Dados persistentes, SDN rootless, `delonix build` | 8–10 min |
| 03 | MicroVMs declarativas | `delonix vm`, imagem dourada, cloud-init | 7–9 min |
| 04 | Stack e Compose | Manifesto declarativo + `docker-compose.yml` nativo | 9–11 min |
| 05 | Kubernetes sem Docker | `cluster kubeadm` — cluster real de ponta a ponta | 10–13 min |
| 06 | Produção: firewall e dash | Segurança de rede, dashboard, disco | 8–10 min |

---

## 01 — Delonix em 6 minutos (5–7 min)

O "olá mundo": instalar, correr o primeiro container, e a única ideia que importa reter — motor de
containers e microVMs sem daemon, sem root.

### Gancho (primeiros 15s)

> "Isto é o Delonix a correr um container nginx — sem Docker instalado nesta máquina, sem root,
> sem daemon nenhum a correr em segundo plano." *(ecrã: `docker: command not found` seguido de
> `delonix container run` a funcionar na mesma.)*

### Objectivos

- Instalar com um comando (`install.sh`)
- Perceber "rootless-first, daemonless" em 1 frase, sem jargão
- Correr, ver, e remover o primeiro container

### Guião de ecrã

**Falar:** Instalação — um script, sem sudo (excepto o passo pontual de subuid/subgid na primeira vez).

```bash
curl -fsSL https://angolardevops.github.io/delonix-runtime/install.sh | sh
delonix --version
```

**Falar:** O primeiro container — publica uma porta, sem privilégio nenhum.

```bash
delonix container run -d --name web -p 8080:80 nginx
curl localhost:8080
# <title>Welcome to nginx!</title>
```

**Falar:** `ps`/`logs`/`rm` — o vocabulário docker que já conheces, funciona igual.

```bash
delonix container ls
delonix container logs web
delonix container rm -f web
```

### A frase que fica

"Sem daemon" quer dizer: cada comando é o processo todo, do princípio ao fim — se o `delonix`
morresse a meio, não haveria um daemon zombie a gerir nada. "Rootless" quer dizer: o teu utilizador
normal, sem `sudo`, sem grupo `docker` a dar root de facto.

### CTA final

"No próximo vídeo: volumes que sobrevivem a reinícios, redes isoladas entre containers, e
`delonix build` a construir a tua própria imagem — sem Dockerfile especial, é o mesmo de sempre."

---

## 02 — Volumes, redes e build (8–10 min)

Dados que sobrevivem ao container, containers que se descobrem por nome, e construir uma imagem
própria com um Dockerfile normal.

### Gancho

> "Vou apagar este container com a base de dados lá dentro — e os dados vão sobreviver." *(ecrã:
> cria um volume, escreve um ficheiro dentro, remove o container, sobe outro a apontar ao mesmo
> volume, o ficheiro continua lá.)*

### Objectivos

- Volume nomeado — criar, montar, sobreviver a um `rm`
- Rede custom — dois containers a falarem-se pelo NOME (DNS interno)
- `delonix build` — Dockerfile normal, zero sintaxe nova

### Guião de ecrã

**Falar:** Volume nomeado — o dado vive fora do ciclo de vida do container.

```bash
delonix volumes create dados
delonix container run -d --name db -v dados:/var/lib/postgresql/data postgres:16-alpine
delonix container rm -f db
# recria — o volume, não o container, é que guardava os dados
delonix container run -d --name db2 -v dados:/var/lib/postgresql/data postgres:16-alpine
```

**Falar:** Rede custom + DNS por nome — a app fala com "db", não com um IP decorado.

```bash
delonix network create backend
delonix container run -d --name db --net backend postgres:16-alpine
delonix container run -it --rm --net backend alpine sh -c \
  "apk add -q bind-tools && nslookup db"
```

**Falar:** Build — um Dockerfile qualquer, sem adaptar nada.

```dockerfile
# Dockerfile
FROM alpine
RUN echo "construído no Delonix" > /msg.txt
CMD ["cat", "/msg.txt"]
```

```bash
delonix build -t minha-app:dev .
delonix container run --rm minha-app:dev
```

### Ponto a sublinhar

A rede não é um detalhe cosmético: dois containers na MESMA rede resolvem-se por nome (como o
CoreDNS do Kubernetes) — é a base de tudo o que vem no episódio 04 (Compose) e 05 (Kubernetes).

### CTA final

"Container é só metade da história — no próximo vídeo: microVMs de verdade, com o próprio kernel,
prontas para correr Kubernetes."

---

## 03 — MicroVMs declarativas (7–9 min)

Quando um container não chega — isolamento de hipervisor, kernel próprio, pronto a arrancar um nó
Kubernetes sem instalar nada na primeira vez que liga.

### Gancho

> "Isto não é um container — é uma VM completa, com o SEU PRÓPRIO kernel, a arrancar em segundos,
> sem eu ter tocado num instalador." *(ecrã: `delonix vm create dev` → `vm console dev` → login
> funcional.)*

### Objectivos

- Diferença prática container vs. microVM (isolamento, não performance)
- Criar uma VM com a imagem dourada, entrar por consola e por SSH
- Cloud-init declarativo (hostname/chave SSH sem tocar num ISO à mão)

### Guião de ecrã

**Falar:** Uma VM com chave SSH já injectada — nada de senha por defeito em produção.

```bash
delonix vm create dev --ssh-key ~/.ssh/id_ed25519.pub --hostname dev
delonix vm ls
# espera o IP aparecer, depois:
ssh delonix@$(delonix vm ls | grep dev | awk '{print $4}')
```

**Falar:** Consola série — para quando nem a rede está pronta ainda.

```bash
delonix vm console dev
# Ctrl+] para voltar ao host
```

**Falar:** A golden image já vem com Kubernetes pronto — é o gancho para o episódio 05.

```bash
ssh delonix@<ip> "kubeadm version && kubelet --version"
```

### Ponto a sublinhar

"Golden image" = a VM já nasce com `kubeadm`/`kubelet`/`delonix-cri` instalados — arrancar um nó
não instala nada, só faz `kubeadm init`/`join`. É o que torna o próximo vídeo rápido de verdade.

### CTA final

"Container para o dia a dia, VM para isolamento a sério — no próximo vídeo, um jeito de descrever
os dois (mais rede, mais volumes) num único ficheiro YAML, e o `docker-compose.yml` que já tens a
funcionar sem alterar uma linha."

---

## 04 — Stack e Compose (9–11 min)

Duas portas de entrada para o mesmo resultado: o manifesto declarativo nativo do Delonix, e o
`docker-compose.yml` que já existe no teu projecto.

### Gancho

> "Este `docker-compose.yml` nunca foi pensado para o Delonix — e vai simplesmente funcionar."
> *(ecrã: um compose de app+Postgres real, `delonix compose up`, os dois serviços a subir na
> ordem certa.)*

### Objectivos

- `delonix stack init` → projecto completo pronto a editar
- `stack plan`/`apply` — manifesto declarativo que CONVERGE (a quente, sem mudar o PID)
- `compose up` — o mesmo `docker-compose.yml`, sem tradução manual

### Guião de ecrã

**Falar:** Scaffold — um projecto completo (volume + BD + app) num comando.

```bash
delonix stack init minhaapp && cd minhaapp
cat delonix-manifest.yaml
delonix stack apply --dry-run   # mostra o plano, não cria nada
delonix stack apply
```

**Falar:** Agora o mesmo resultado, mas a partir de um docker-compose.yml existente.

```bash
delonix compose config          # valida, mostra o plano resolvido
delonix compose up
delonix compose ps
delonix compose logs db
```

**Falar:** `depends_on` a sério — a app só arranca depois da BD responder de facto.

```yaml
# no docker-compose.yml:
#   depends_on:
#     db:
#       condition: service_healthy
```

```bash
delonix compose down -v   # limpa tudo, incl. volumes nomeados
```

### Ponto a sublinhar

Não é um shim que só finge — `depends_on: service_healthy` espera mesmo pelo healthcheck real (o
do serviço ou o da própria imagem) antes de arrancar o próximo. Um ciclo no grafo dá erro claro,
nunca uma ordem inventada.

### CTA final

"Um container, uma VM, uma stack — falta a peça maior: um cluster Kubernetes real, do zero, sem
Docker instalado em lado nenhum. É o próximo vídeo."

---

## 05 — Kubernetes sem Docker (10–13 min)

O vídeo-bandeira: um comando, do zero a um control-plane `Ready` — VMs provisionadas, `kubeadm` a
correr, HAProxy automático em HA.

### Gancho

> "Vou criar um cluster Kubernetes com dois control-planes e três workers — e o Docker nunca vai
> aparecer nesta demo, nem uma vez." *(ecrã: `kubectl get nodes` no fim, todos `Ready`.)*

### Objectivos

- `cluster kubeadm` — provisiona VMs + bootstrap, um comando só
- HA automático (>1 control-plane provisiona um HAProxy sozinho)
- `delonix-cri` como runtime — sem containerd, sem Docker

### Guião de ecrã

**Falar:** Um comando: provisiona as VMs, gera a chave SSH, arranca o cluster.

```bash
delonix cluster kubeadm --name lab --control-plane 2 --workers 3
# progresso estilo `kind create cluster` — cada etapa fecha com ✓
```

**Falar (enquanto sobe, sem ficar em silêncio):** explicar o que está a acontecer por trás —
provisiona 5+1 VMs (a +1 é o HAProxy, automático com >1 control-plane), prepara cada host,
`kubeadm init`, junta os restantes control-planes e os workers.

**Falar:** O cluster está pronto — puxa o kubeconfig e confirma.

```bash
export KUBECONFIG=~/.local/share/delonix/clusters/lab-kubeconfig.yaml
kubectl get nodes -o wide
# NAME       STATUS   ROLES           VERSION
# lab-cp1    Ready    control-plane   v1.34.0
# lab-cp2    Ready    control-plane   v1.34.0
# lab-w1     Ready    <none>          v1.34.0
# ...
```

**Falar:** Prova de fogo — sobe uma app real no cluster.

```bash
kubectl create deployment web --image=nginx --replicas=3
kubectl get pods -o wide
```

### Ponto a sublinhar

O runtime de cada nó é o `delonix-cri` — não há containerd nem Docker em lado nenhum da pilha. E é
idempotente "à Terraform, mas sem ficheiro de estado": corre o mesmo comando outra vez e ele só
corrige o que faltar.

### CTA final

"Cluster a correr é só metade — no último vídeo: firewall entre containers, o dashboard estilo
htop, e como ver onde está a ir o disco antes que seja tarde demais."

---

## 06 — Produção: firewall e dashboard (8–10 min)

O que muda quando isto deixa de ser uma demo — isolar tráfego entre containers, observar tudo num
só ecrã, e não deixar o disco encher em silêncio.

### Gancho

> "Esta base de dados vai deixar de responder a QUALQUER container excepto à app — em duas
> linhas." *(ecrã: `curl` a falhar depois do `net ingress policy deny`.)*

### Objectivos

- Firewall L4 por container (`net ingress`/`egress`)
- Dashboard interactivo — KPIs de RAM/rede/disco num só ecrã
- `system df`/`prune` — não deixar disco acumular em silêncio

### Guião de ecrã

**Falar:** Bloquear tudo por omissão, só deixar entrar quem precisa mesmo.

```bash
delonix net ingress policy db deny
delonix net ingress allow db tcp/5432 --from 10.219.0.0/16
delonix net ingress ls db
```

**Falar:** O dashboard — não é um snapshot, é ao vivo (o equivalente a um `htop` do runtime).

```bash
delonix dash
# setas para navegar, `m` alterna o gráfico containers/memória
```

**Falar:** Também dá para script/Grafana — JSON puro, sem TUI.

```bash
delonix dash --json | jq '.tiles'
```

**Falar:** Disco — o vilão silencioso de qualquer runtime rootless (rootfs por container).

```bash
delonix system df
delonix system prune   # só remove órfãos/não referenciados — pede confirmação
```

### Ponto a sublinhar

"Última regra ganha" no firewall — mudar uma `allow`/`deny` para o MESMO alvo substitui a
anterior, nunca acumula uma sombra invisível por trás.

### CTA final

"Isto fecha a série de base — instalação, containers, VMs, stacks, Kubernetes, e produção. A
partir daqui, a documentação completa e o `--help` de cada grupo levam-te ao resto. Se ficaste com
alguma dúvida, deixa nos comentários."

---

## Notas de produção

- **Terminal:** fonte grande (≥18pt), tema claro ou escuro consistente em TODOS os vídeos
  (reconhecimento de marca), prompt curto (evita `user@hostname:~/caminho/longo$` a comer o ecrã).
- **Antes de gravar cada episódio:** correr o guião de comandos uma vez a seco fora de gravação —
  todos os comandos deste documento já foram testados neste host, mas o ESTADO local
  (containers/redes já existentes) pode diferir; limpar o ambiente antes de cada gravação evita um
  `already exists` em directo.
- **Erros em directo não são má sorte — são conteúdo:** se algo falhar durante a gravação, não
  cortar — mostrar a mensagem de erro e o que ela diz para fazer é a favor do produto (mensagens de
  erro claras é um objectivo explícito deste projecto).
- **Legenda/overlay:** cada bloco "Falar" acima é o guia de voz, não texto para ler — dizer por
  palavras próprias em cima do que o ecrã mostra.
- **Miniatura/título consistente:** "Delonix Runtime — Ep. 0X: \<título\>", mesma paleta laranja
  (`#e8590c`) do site/dashboard, para a série ser reconhecível na lista do canal.

---

*Roteiro preparado para a publicação do Delonix Runtime à comunidade — todos os comandos foram
executados e confirmados neste host antes de entrarem no guião.*
