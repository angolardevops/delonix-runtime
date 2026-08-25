// Uma entrada por comando da CLI. GERADA a partir da árvore viva do binário
// (`scratchpad/build_entries.py`) e do conteúdo editorial, mas é o ficheiro
// COMMITADO que conta — a partir daqui edita-se à mão.
//
// `include!`d por `manual.rs` — à parte só por tamanho: aqui está o CONTEÚDO (o
// que cada comando faz e como se usa), lá está o mecanismo que o aplica.
// Misturá-los faria a revisão de uma frase de exemplo abrir 400 linhas de
// renderização de texto.
//
// Regras, todas com teste em `manual::tests`:
//   * um `path` tem de existir na CLI, e todo o comando da CLI tem de estar aqui
//     COM pelo menos um exemplo (presente-mas-vazio é a falha que o teste caça);
//   * cada exemplo TEM de invocar o comando que documenta;
//   * o comentário é traduzido (`pt.po`), a linha de comando nunca.

pub static ENTRIES: &[Entry] = &[
    Entry {
        path: "build",
        group: "",
        examples: &[
            ("build and tag from the Dockerfile in this directory", "delonix build -t myapp:v1 ."),
            ("a Delonixfile, or any other build file path", "delonix build -t myapp:v1 -f Delonixfile ."),
            ("a build secret, bind-mounted live for one `RUN` and structurally unable to reach a layer", "delonix build -t myapp:v1 --secret id=npmrc,src=/home/me/.npmrc ."),
            ("cross-arch: resolves the right base image and stamps the arch into the result", "delonix build -t myapp:v1 --platform linux/arm64 ."),
        ],
        see_also: &["image ls", "image push", "container run", "init"],
    },
    Entry {
        path: "cluster",
        group: "",
        examples: &[
            ("a local Kubernetes cluster with no manifest and no Docker", "delonix cluster create --name dev"),
            ("what is up, node by node", "delonix cluster ls"),
            ("from zero to VMs plus kubeadm — above one control-plane the load balancer comes with it", "delonix cluster kubeadm --name lab --network k8s --control-plane 3"),
        ],
        see_also: &["cluster kube generate", "stack apply", "vm create", "image vm pull"],
    },
    Entry {
        path: "cluster create",
        group: "Lifecycle",
        examples: &[
            ("a single node: control-plane untainted, so it schedules everything", "delonix cluster create --name dev"),
            ("one control-plane and two workers", "delonix cluster create --name dev --workers 2"),
            ("your own CNI instead of kindnet — the node stays NotReady until you apply it", "delonix cluster create --name dev --cni none"),
            ("pin the apiserver to the host port your kubeconfig already expects", "delonix cluster create --name dev --api-port 6443"),
        ],
        see_also: &["cluster ls", "cluster load", "cluster delete", "cluster kubeadm"],
    },
    Entry {
        path: "cluster delete",
        group: "Lifecycle",
        examples: &[
            ("stop and remove the nodes plus the kubeconfig", "delonix cluster delete --name dev"),
            ("the default cluster, when you never named one", "delonix cluster delete"),
        ],
        see_also: &["cluster create", "cluster ls", "system prune"],
    },
    Entry {
        path: "cluster prune",
        group: "Maintenance",
        examples: &[
            ("collect what clusters with no nodes left behind: their directory, their exported kubeconfig, and the `~/.kube/config` context still pointing at a port that may now answer for something else", "delonix cluster prune"),
            ("in CI, where there is no terminal to confirm at", "delonix cluster prune -f"),
        ],
        see_also: &["cluster delete", "cluster ls", "vm prune", "system prune"],
    },
    Entry {
        path: "cluster kubeadm",
        group: "Lifecycle",
        examples: &[
            ("VMs from the golden image plus kubeadm, without writing a manifest by hand", "delonix cluster kubeadm --name lab --network k8s"),
            ("HA: above one control-plane a HAProxy load balancer is provisioned and used as the endpoint", "delonix cluster kubeadm --name lab --network k8s --control-plane 3 --workers 3"),
            ("merge the kubeconfig into your own only after every node reports Ready", "delonix cluster kubeadm --name lab --network k8s --copy-kubeconfig"),
            ("etcd on VMs of its own instead of stacked on the control-planes", "delonix cluster kubeadm --name lab --network k8s --etcd-cluster 3"),
        ],
        see_also: &["cluster apply", "cluster ls", "vm ls", "image vm pull"],
    },
    Entry {
        path: "cluster init",
        group: "Create",
        examples: &[
            ("a project with the cluster manifests already filled in", "delonix cluster init"),
            ("into a directory of its own, named, with the image you intend to run", "delonix cluster init ./lab --name lab --image nginx:alpine"),
            ("regenerate over a scaffold you already edited", "delonix cluster init --force"),
        ],
        see_also: &["cluster apply", "cluster create", "stack init"],
    },
    Entry {
        path: "cluster ls",
        group: "Inspect",
        examples: &[
            ("clusters and the state of each node, derived from the container labels", "delonix cluster ls"),
        ],
        see_also: &["cluster create", "cluster delete", "container ps"],
    },
    Entry {
        path: "cluster load",
        group: "Interact",
        examples: &[
            ("the image you just built, into every node, with no registry in between", "delonix cluster load myapp:dev"),
            ("several in one go", "delonix cluster load myapp:dev sidecar:dev"),
            ("say which cluster, when more than one is up", "delonix cluster load myapp:dev --name dev"),
        ],
        see_also: &["cluster create", "image ls", "build", "cluster ls"],
    },
    Entry {
        path: "cluster apply",
        group: "Declarative",
        examples: &[
            ("bootstrap the hosts a manifest declares — idempotent, and with no state file to drift", "delonix cluster apply -f cloud.yaml"),
            ("the manifest in this directory", "delonix cluster apply"),
        ],
        see_also: &["cluster kubeadm", "stack apply", "stack validate", "cluster ls"],
    },
    Entry {
        path: "cluster kube",
        group: "Declarative",
        examples: &[
            ("the Kubernetes YAML for something you already have running locally", "delonix cluster kube generate web"),
        ],
        see_also: &["container describe", "pod describe", "stack apply"],
    },
    Entry {
        path: "cluster kube generate",
        group: "Declarative",
        examples: &[
            ("a kind: Pod out of a running container, straight to stdout", "delonix cluster kube generate web"),
            ("keep it in the repo as the starting point for a real deployment", "delonix cluster kube generate web > pod.yaml"),
            ("every member of a pod, as one manifest", "delonix cluster kube generate api-pod"),
        ],
        see_also: &["container describe", "pod describe", "container apply", "stack apply"],
    },
    Entry {
        path: "completion",
        group: "",
        examples: &[
            ("bash, into the user's own completion directory", "delonix completion bash > ~/.local/share/bash-completion/completions/delonix"),
            ("zsh, into a directory already on your fpath", "delonix completion zsh > ~/.zfunc/_delonix"),
            ("fish", "delonix completion fish > ~/.config/fish/completions/delonix.fish"),
        ],
        see_also: &["version", "init", "dash"],
    },
    Entry {
        path: "compose",
        group: "",
        examples: &[
            ("a docker-compose.yml running natively — no daemon, no Docker", "delonix compose up -d"),
            ("what belongs to this project", "delonix compose ps"),
            ("tear it down, named volumes included", "delonix compose down -v"),
        ],
        see_also: &["stack apply", "container run", "volumes ls", "network ls"],
    },
    Entry {
        path: "compose down",
        group: "Lifecycle",
        examples: &[
            ("remove every container this project's up created", "delonix compose down"),
            ("and its named volumes — the data goes with them", "delonix compose down -v"),
            ("a project whose name does not come from the directory", "delonix compose down -p staging"),
        ],
        see_also: &["compose up", "compose ps", "stack destroy"],
    },
    Entry {
        path: "compose up",
        group: "Lifecycle",
        examples: &[
            ("build, then network, then volumes, then the containers in depends_on order", "delonix compose up -d"),
            ("a compose file that lives somewhere else", "delonix compose up -f docker-compose.yml -d"),
            ("the resolved project, creating nothing", "delonix compose up --dry-run"),
            ("two copies of the same file side by side, told apart by project name", "delonix compose up -p staging -d"),
        ],
        see_also: &["compose ps", "compose logs", "compose down", "compose config"],
    },
    Entry {
        path: "compose config",
        group: "Inspect",
        examples: &[
            ("validate the file and print the project it resolves to", "delonix compose config"),
            ("a file elsewhere, under a project name of your own", "delonix compose config -f docker-compose.yml -p staging"),
        ],
        see_also: &["compose up", "stack validate", "compose ps"],
    },
    Entry {
        path: "compose logs",
        group: "Inspect",
        examples: &[
            ("every service, one after another", "delonix compose logs"),
            ("one service only", "delonix compose logs web"),
            ("follow it live — the flag is --follow here, because -f is already the compose file", "delonix compose logs web --follow"),
        ],
        see_also: &["compose ps", "container logs", "compose up"],
    },
    Entry {
        path: "compose ps",
        group: "Inspect",
        examples: &[
            ("the containers of this project, derived from their labels", "delonix compose ps"),
            ("another project up on the same host", "delonix compose ps -p staging"),
        ],
        see_also: &["compose logs", "compose up", "container ps"],
    },
    Entry {
        path: "container",
        group: "",
        examples: &[
            ("a web service in seconds — host port 8080 onto the container's 80", "delonix container run -d -p 8080:80 --name web nginx"),
            ("what is running, and for how long", "delonix container ps"),
            ("a shell inside it, without stopping anything", "delonix container ssh web"),
        ],
        see_also: &["pod", "image", "workload", "compose up"],
    },
    Entry {
        path: "container kill",
        group: "Lifecycle",
        examples: &[
            ("SIGKILL, no waiting", "delonix container kill web"),
            ("any signal, by name or number — this one asks nginx to reload", "delonix container kill -s HUP web"),
        ],
        see_also: &["container stop", "container restart"],
    },
    Entry {
        path: "container pause",
        group: "Lifecycle",
        examples: &[
            ("freeze the processes — the state stays in memory, unlike stop", "delonix container pause web"),
        ],
        see_also: &["container unpause", "container stop"],
    },
    Entry {
        path: "container restart",
        group: "Lifecycle",
        examples: &[
            ("stop and start again, keeping the original configuration", "delonix container restart web"),
        ],
        see_also: &["container start", "container stop", "container update"],
    },
    Entry {
        path: "container rm",
        group: "Lifecycle",
        examples: &[
            ("remove a stopped container", "delonix container rm web"),
            ("force it even while running, and drop its anonymous volumes", "delonix container rm -f -v web"),
            ("everything that has already exited", "delonix container rm $(delonix container ps -aq)"),
        ],
        see_also: &["container stop", "container ps", "volumes rm"],
    },
    Entry {
        path: "container prune",
        group: "Maintenance",
        examples: &[
            ("remove every stopped container, and the rootfs debris no `rm` ever reaches", "delonix container prune"),
            ("in CI, where there is no terminal to confirm at", "delonix container prune -f"),
        ],
        see_also: &["container rm", "container ps", "image prune", "system prune"],
    },
    Entry {
        path: "container run",
        group: "Lifecycle",
        examples: &[
            ("detached, published on the host, with a name of your own", "delonix container run -d -p 8080:80 --name web nginx"),
            ("a database with a named volume and a network of its own, so the data outlives the container", "delonix container run -d --net app -v pgdata:/var/lib/postgresql/data -e POSTGRES_PASSWORD=dev postgres:16"),
            ("in the foreground, removed on exit — the shape for a one-off task", "delonix container run --rm alpine echo hello"),
            ("with a memory and CPU cap, and a restart policy", "delonix container run -d -m 512M --cpus 1.5 --restart on-failure nginx"),
        ],
        see_also: &["container ps", "container logs", "container stop", "container update"],
    },
    Entry {
        path: "container start",
        group: "Lifecycle",
        examples: &[
            ("bring a stopped container back, with the same ports, volumes and network", "delonix container start web"),
            ("several at once", "delonix container start web db cache"),
        ],
        see_also: &["container stop", "container restart", "container ps"],
    },
    Entry {
        path: "container stop",
        group: "Lifecycle",
        examples: &[
            ("SIGTERM, then SIGKILL if it does not leave", "delonix container stop web"),
            ("give it 30 seconds to shut down cleanly", "delonix container stop -t 30 db"),
        ],
        see_also: &["container start", "container kill", "container rm"],
    },
    Entry {
        path: "container unpause",
        group: "Lifecycle",
        examples: &[
            ("resume a frozen container", "delonix container unpause web"),
        ],
        see_also: &["container pause"],
    },
    Entry {
        path: "container wait",
        group: "Lifecycle",
        examples: &[
            ("block until it exits, then print the exit code", "delonix container wait job"),
        ],
        see_also: &["container run", "container logs", "container ps"],
    },
    Entry {
        path: "container describe",
        group: "Inspect",
        examples: &[
            ("the readable detail — kubectl describe style", "delonix container describe web"),
        ],
        see_also: &["container inspect", "container ps"],
    },
    Entry {
        path: "container diff",
        group: "Inspect",
        examples: &[
            ("what changed relative to the image: A created/changed, D deleted", "delonix container diff web"),
        ],
        see_also: &["container commit", "container cp"],
    },
    Entry {
        path: "container healthcheck",
        group: "Inspect",
        examples: &[
            ("run the image's HEALTHCHECK — exits 1 if unhealthy, so a script can gate on it", "delonix container healthcheck web"),
        ],
        see_also: &["container describe", "container run"],
    },
    Entry {
        path: "container inspect",
        group: "Inspect",
        examples: &[
            ("the full record as JSON, for a script", "delonix container inspect web"),
        ],
        see_also: &["container describe", "container ps"],
    },
    Entry {
        path: "container logs",
        group: "Inspect",
        examples: &[
            ("everything the container has written", "delonix container logs web"),
            ("follow it live", "delonix container logs -f web"),
            ("the last 50 lines with timestamps (needs --log-cri at run time)", "delonix container logs --tail 50 --timestamps web"),
        ],
        see_also: &["container attach", "container ps", "container describe"],
    },
    Entry {
        path: "container port",
        group: "Inspect",
        examples: &[
            ("which host ports reach this container", "delonix container port web"),
        ],
        see_also: &["container update", "net ingress ls"],
    },
    Entry {
        path: "container ps",
        group: "Inspect",
        examples: &[
            ("the running ones", "delonix container ps"),
            ("all of them, stopped included", "delonix container ps -a"),
            ("just the IDs, to feed another command", "delonix container ps -q"),
        ],
        see_also: &["container describe", "container stats", "container logs"],
    },
    Entry {
        path: "container stats",
        group: "Inspect",
        examples: &[
            ("CPU, memory and PIDs of everything running, one sample", "delonix container stats"),
            ("just these two", "delonix container stats web db"),
        ],
        see_also: &["container dash", "container top", "system df"],
    },
    Entry {
        path: "container top",
        group: "Inspect",
        examples: &[
            ("the processes inside the container", "delonix container top web"),
        ],
        see_also: &["container stats", "container exec"],
    },
    Entry {
        path: "container attach",
        group: "Interact",
        examples: &[
            ("re-attach to the output of a detached container (output only)", "delonix container attach web"),
        ],
        see_also: &["container logs", "container exec"],
    },
    Entry {
        path: "container commit",
        group: "Interact",
        examples: &[
            ("turn what was written inside into a new image", "delonix container commit web myapp:v1"),
        ],
        see_also: &["image ls", "build", "container diff"],
    },
    Entry {
        path: "container cp",
        group: "Interact",
        examples: &[
            ("out of the container onto the host", "delonix container cp web:/etc/nginx/nginx.conf ."),
            ("from the host into the container", "delonix container cp ./site.conf web:/etc/nginx/conf.d/"),
        ],
        see_also: &["container exec", "container diff"],
    },
    Entry {
        path: "container exec",
        group: "Interact",
        examples: &[
            ("run a command inside a container that is already up", "delonix container exec web nginx -t"),
            ("an interactive shell", "delonix container exec -it web sh"),
            ("as another user, in another directory, with an extra variable", "delonix container exec -u root -w /srv -e DEBUG=1 web env"),
        ],
        see_also: &["container ssh", "container attach", "container logs"],
    },
    Entry {
        path: "container ssh",
        group: "Interact",
        examples: &[
            ("a shell inside the container — tries bash, falls back to sh", "delonix container ssh web"),
            ("one command and out", "delonix container ssh web cat /etc/hosts"),
        ],
        see_also: &["container exec", "container logs"],
    },
    Entry {
        path: "container rename",
        group: "Configure",
        examples: &[
            ("give it a new name — the ID does not change", "delonix container rename web frontend"),
        ],
        see_also: &["container ps", "container describe"],
    },
    Entry {
        path: "container update",
        group: "Configure",
        examples: &[
            ("publish a new port on a RUNNING container — the PID does not change", "delonix container update --publish-add 9000:80 web"),
            ("swap a port in one command: removals run before additions", "delonix container update --publish-rm 8080 --publish-add 8080:9000 web"),
            ("raise the memory and CPU caps live", "delonix container update --memory 1G --cpus 2 db"),
            ("attach it to another network and cap that link's bandwidth", "delonix container update --net-connect backend --net-rate 10mbit web"),
        ],
        see_also: &["container run", "container restart", "network create"],
    },
    Entry {
        path: "container apply",
        group: "Declarative",
        examples: &[
            ("apply the kind: Container documents of the default manifest", "delonix container apply"),
            ("from another file", "delonix container apply -f prod.yaml"),
        ],
        see_also: &["stack apply", "stack plan", "container init"],
    },
    Entry {
        path: "container init",
        group: "Declarative",
        examples: &[
            ("scaffold a project already filled in, ready to run", "delonix container init"),
        ],
        see_also: &["init", "stack init", "container apply"],
    },
    Entry {
        path: "container dash",
        group: "Dashboards",
        examples: &[
            ("interactive dashboard of the containers", "delonix container dash"),
            ("one text snapshot, for a script or CI", "delonix container dash --once"),
        ],
        see_also: &["dash", "container stats", "container ps"],
    },
    Entry {
        path: "dash",
        group: "",
        examples: &[
            ("an htop-style live view of containers, VMs, networks and volumes", "delonix dash"),
            ("one text snapshot and exit — the default when stdout is not a terminal", "delonix dash --once"),
            ("one JSON snapshot, for a script or a Grafana JSON datasource", "delonix dash --json"),
        ],
        see_also: &["container dash", "vm dash", "system info", "serve api"],
    },
    Entry {
        path: "explain",
        group: "",
        examples: &[
            ("every field of a Kind, `kubectl explain` style, straight from the generated schema", "delonix explain Container"),
            ("drill into one field instead of reading the whole Kind", "delonix explain Container.ports"),
            ("a nested field of a pod member", "delonix explain Pod.containers.image"),
        ],
        see_also: &["schema print", "stack validate", "stack apply"],
    },
    Entry {
        path: "image",
        group: "",
        examples: &[
            ("fetch an image from a registry", "delonix image pull postgres:16"),
            ("what is on disk, and how much of it", "delonix image ls"),
            ("golden VM images live in a store of their own, next to the container ones", "delonix image vm ls"),
        ],
        see_also: &["build", "container run", "image vm", "image scan"],
    },
    Entry {
        path: "image pull",
        group: "Lifecycle",
        examples: &[
            ("the common case — repository and tag", "delonix image pull postgres:16"),
            ("a registry other than Docker Hub — write it out in the reference", "delonix image pull ghcr.io/angolardevops/kaeso-odoo:18"),
            ("refuse the image unless it carries a cosign signature made by this key", "delonix image pull --verify cosign.pub ghcr.io/angolardevops/app:1.2.0"),
        ],
        see_also: &["image ls", "image scan", "image verify", "image vm pull"],
    },
    Entry {
        path: "image push",
        group: "Lifecycle",
        examples: &[
            ("publish under the image's own reference", "delonix image push ghcr.io/angolardevops/app:1.2.0"),
            ("publish a locally-named image under another reference, without tagging it first", "delonix image push kaeso-odoo:18 ghcr.io/angolardevops/kaeso-odoo:18"),
        ],
        see_also: &["image login", "image tag", "image vm push"],
    },
    Entry {
        path: "image rm",
        group: "Lifecycle",
        examples: &[
            ("remove an image nothing is using", "delonix image rm alpine:3.19"),
            ("remove it even though a container still references it", "delonix image rm -f odoo:16"),
        ],
        see_also: &["image ls", "container rm", "system prune"],
    },
    Entry {
        path: "image prune",
        group: "Maintenance",
        examples: &[
            ("drop the dangling images and the blobs nobody references", "delonix image prune"),
            ("also drop tagged images no container uses", "delonix image prune -a -f"),
        ],
        see_also: &["image rm", "image ls", "container prune", "system prune"],
    },
    Entry {
        path: "image describe",
        group: "Inspect",
        examples: &[
            ("tags, digest, size, layers and the OCI config (entrypoint/cmd/env/workdir) in one screen", "delonix image describe postgres:16"),
            ("several at once, to compare what two images actually run", "delonix image describe alpine:3.19 redis:7-alpine"),
        ],
        see_also: &["image ls", "image history", "image scan"],
    },
    Entry {
        path: "image history",
        group: "Inspect",
        examples: &[
            ("the layers from base to top, with the digest and size of each", "delonix image history postgres:16"),
            ("where the size went, before deciding what to change in the Dockerfile", "delonix image history kaeso-odoo:18"),
        ],
        see_also: &["image describe", "build", "image ls"],
    },
    Entry {
        path: "image ls",
        group: "Inspect",
        examples: &[
            ("what is on disk, newest first", "delonix image ls"),
            ("as JSON, for a script to read instead of a table meant for eyes", "delonix image ls -o json"),
        ],
        see_also: &["image describe", "image history", "image rm"],
    },
    Entry {
        path: "image scan",
        group: "Inspect",
        examples: &[
            ("the CVEs of an image, read from the layers on disk — nothing is executed", "delonix image scan postgres:16"),
            ("the installed packages (SBOM) instead of the findings", "delonix image scan --sbom nginx"),
            ("a CI gate: exit 1 if anything high or worse is found", "delonix image scan --fail-on high ghcr.io/angolardevops/app:1.2.0"),
            ("refresh the local feed first — a scan is only as fresh as the database behind it", "delonix image scan --update"),
        ],
        see_also: &["image describe", "image pull", "image verify"],
    },
    Entry {
        path: "image verify",
        group: "Inspect",
        examples: &[
            ("check the cosign signature of an image you already have against the publisher's key", "delonix image verify ghcr.io/angolardevops/app:1.2.0 cosign.pub"),
            ("the gate to run before promoting a local build to production", "delonix image verify kaeso-odoo:18 keys/kaeso.pub"),
        ],
        see_also: &["image pull", "image scan", "image describe"],
    },
    Entry {
        path: "image login",
        group: "Configure",
        examples: &[
            ("authenticate to a registry — the token comes from stdin, never from the argv, where it would land in the shell history and in /proc", "printf '%s' \"$GITHUB_TOKEN\" | delonix image login ghcr.io -u angolardevops --password-stdin"),
            ("the same for Docker Hub", "printf '%s' \"$DOCKERHUB_TOKEN\" | delonix image login docker.io -u myuser --password-stdin"),
        ],
        see_also: &["image logout", "image push", "image pull"],
    },
    Entry {
        path: "image logout",
        group: "Configure",
        examples: &[
            ("drop the stored credentials of a registry", "delonix image logout ghcr.io"),
        ],
        see_also: &["image login", "image push"],
    },
    Entry {
        path: "image tag",
        group: "Configure",
        examples: &[
            ("a second name for the same content — nothing is copied", "delonix image tag postgres:16 db:stable"),
            ("the shape a locally-built image needs before it can be pushed to your registry", "delonix image tag kaeso-odoo:18 ghcr.io/angolardevops/kaeso-odoo:18"),
        ],
        see_also: &["image push", "image ls", "image rm"],
    },
    Entry {
        path: "image export",
        group: "Storage",
        examples: &[
            ("an OCI runtime bundle (rootfs + config.json) that `runc` or `crun` runs directly", "delonix image export alpine:latest /tmp/alpine-bundle"),
            ("the directory you then hand to another OCI runtime — this is a bundle to run, not an archive to ship", "delonix image export postgres:16 /tmp/pg-bundle"),
        ],
        see_also: &["image save", "image load", "container run"],
    },
    Entry {
        path: "image load",
        group: "Storage",
        examples: &[
            ("read back an archive written by `save`, `docker save` or `podman save`", "delonix image load -i postgres-16.tar"),
            ("a gzipped archive has to be gunzipped first", "gunzip -c nginx.tar.gz > nginx.tar && delonix image load -i nginx.tar"),
        ],
        see_also: &["image save", "image ls", "image pull"],
    },
    Entry {
        path: "image save",
        group: "Storage",
        examples: &[
            ("move an image to another machine with no registry in between", "delonix image save postgres:16 -o postgres-16.tar"),
            ("pipe it straight into gzip instead of writing the plain archive", "delonix image save nginx -o /dev/stdout | gzip > nginx.tar.gz"),
        ],
        see_also: &["image load", "image push", "image export"],
    },
    Entry {
        path: "image apply",
        group: "Declarative",
        examples: &[
            ("apply only the `kind: Image` documents of `./delonix-manifest.yaml`, ignoring the other Kinds", "delonix image apply"),
            ("from a file of your own", "delonix image apply -f delonix-manifest.yaml"),
        ],
        see_also: &["stack apply", "stack plan", "image pull"],
    },
    Entry {
        path: "image dash",
        group: "Dashboards",
        examples: &[
            ("a live view of the image store, htop-style", "delonix image dash"),
            ("one snapshot, for a log or a terminal that is not a TTY", "delonix image dash --once"),
            ("the same numbers as JSON, for a script or a Grafana datasource", "delonix image dash --json"),
        ],
        see_also: &["image ls", "dash", "system df"],
    },
    Entry {
        path: "image build",
        group: "Advanced",
        examples: &[
            ("the same as `image vm build` — this spelling needs the group's `--vm` flag", "delonix image --vm build --offline --k8s-version 1.34 -t delonix-vm-k8s:1.34"),
        ],
        see_also: &["image vm build", "image vm init", "build"],
    },
    Entry {
        path: "image convert",
        group: "Advanced",
        examples: &[
            ("the same as `image vm convert` — this spelling needs the group's `--vm` flag", "delonix image --vm convert --to vmdk delonix-vm-base:ubuntu-24.04 -o ubuntu-24.04.vmdk"),
        ],
        see_also: &["image vm convert", "image vm import", "image vm ls"],
    },
    Entry {
        path: "image import",
        group: "Advanced",
        examples: &[
            ("the same as `image vm import` — this spelling needs the group's `--vm` flag", "delonix image --vm import -t rocky-9 rocky-9.qcow2"),
        ],
        see_also: &["image vm import", "image vm ls", "vm create"],
    },
    Entry {
        path: "image init",
        group: "Advanced",
        examples: &[
            ("the same as `image vm init` — this spelling needs the group's `--vm` flag", "delonix image --vm init myimage"),
        ],
        see_also: &["image vm init", "vm init", "image vm build"],
    },
    Entry {
        path: "image ls-remote",
        group: "Advanced",
        examples: &[
            ("the same as `image vm ls-remote` — this spelling needs the group's `--vm` flag", "delonix image --vm ls-remote"),
            ("the same for the Kubernetes-free golden repo", "delonix image --vm ls-remote --no-k8s"),
        ],
        see_also: &["image vm ls-remote", "image vm pull", "image vm ls"],
    },
    Entry {
        path: "image vm",
        group: "Advanced",
        examples: &[
            ("what golden images this node already has", "delonix image vm ls"),
            ("fetch the official Kubernetes golden — the source is known, so no argument is needed", "delonix image vm pull"),
            ("build one of your own from a `VMfile`", "delonix image vm build -f VMfile -t myimage:1"),
        ],
        see_also: &["vm create", "image", "cluster kubeadm", "vm build"],
    },
    Entry {
        path: "image vm pull",
        group: "Lifecycle",
        examples: &[
            ("the official Kubernetes golden (`ghcr.io/angolardevops/delonix-vm-k8s`) — no argument needed", "delonix image vm pull"),
            ("the Kubernetes-free golden instead: the same base with `delonix` on board and rootless already set up", "delonix image vm pull --no-k8s"),
            ("a specific published version", "delonix image vm pull ghcr.io/angolardevops/delonix-vm-k8s:1.34"),
            ("under a local name of your own, so `vm create --disk base` finds it", "delonix image vm pull ghcr.io/angolardevops/delonix-vm-base:ubuntu-24.04 --name base"),
        ],
        see_also: &["image vm ls-remote", "image vm ls", "vm create", "cluster kubeadm"],
    },
    Entry {
        path: "image vm push",
        group: "Lifecycle",
        examples: &[
            ("publish to the official repository this image belongs in — the destination is derived from its own metadata", "delonix image vm push delonix-vm-k8s:1.34"),
            ("publish to a registry of your own instead", "delonix image vm push opnsense:26.1 ghcr.io/angolardevops/delonix-vm-appliances:opnsense-26.1.2"),
        ],
        see_also: &["image login", "image vm ls", "image vm build"],
    },
    Entry {
        path: "image vm build",
        group: "Create",
        examples: &[
            ("the built-in golden recipe, offline: the k8s packages are fetched and verified on the HOST, so the guest needs no DHCP or DNS", "delonix image vm build --offline --k8s-version 1.34 -t delonix-vm-k8s:1.34"),
            ("a golden with no Kubernetes at all — just `delonix` itself, rootless ready on first boot", "delonix image vm build --no-k8s --distro debian --debian-release bookworm -t delonix-vm-base:debian-bookworm"),
            ("your own image from a `VMfile`, with network during `RUN` so a package can be installed", "delonix image vm build -f VMfile --network -t myimage:1 ."),
            ("extra packages on top of the golden recipe, without touching the code", "delonix image vm build --offline -t node:1.34 --extra-package htop --extra-package jq"),
        ],
        see_also: &["image vm init", "image vm push", "vm create", "cluster kubeadm"],
    },
    Entry {
        path: "image vm import",
        group: "Create",
        examples: &[
            ("register a disk this engine did not build, so `vm create --disk <name>` can use it", "delonix image vm import -t rocky-9 rocky-9.qcow2"),
            ("an appliance that configures itself: `vm create` then attaches no NoCloud seed, which nothing in the guest would read", "delonix image vm import -t opnsense:26.1 --appliance --distro opnsense --release 26.1.2 opnsense-nano.qcow2"),
            ("carry the vCPU and memory the product really needs, so a VM does not start at 1 vCPU", "delonix image vm import -t truenas-scale:25.10 --appliance --default-vcpus 4 --default-memory 8G truenas.qcow2"),
            ("replace one already registered under the name, stored uncompressed", "delonix image vm import -t base --force --no-compress base.qcow2"),
        ],
        see_also: &["image vm ls", "image vm convert", "vm create", "image vm push"],
    },
    Entry {
        path: "image vm init",
        group: "Create",
        examples: &[
            ("scaffold a `VMfile` in the current directory, ready for `image vm build`", "delonix image vm init myimage"),
            ("write it somewhere else, overwriting whatever is already there", "delonix image vm init app -d ./images --force"),
        ],
        see_also: &["image vm build", "vm init", "vm build"],
    },
    Entry {
        path: "image vm describe",
        group: "Inspect",
        examples: &[
            ("everything recorded about a golden image, `kubectl describe`-style", "delonix image vm describe delonix-vm-k8s:1.34"),
            ("two at once, to compare the bases before choosing one to build on", "delonix image vm describe delonix-vm-base:ubuntu-24.04 delonix-vm-base:debian-bookworm"),
        ],
        see_also: &["image vm ls", "image vm import", "vm create"],
    },
    Entry {
        path: "image vm rm",
        group: "Lifecycle",
        examples: &[
            ("remove an image no VM backs onto", "delonix image vm rm rocky-9"),
            (
                "refused while a VM uses it — the image is that VM's backing file, so removing it makes the VM unreadable rather than freeing anything",
                "delonix image vm rm proxmox-ve:9.1",
            ),
            ("remove it anyway, and lose those VMs", "delonix image vm rm -f proxmox-ve:9.1"),
        ],
        see_also: &["image vm ls", "vm rm", "system df"],
    },
    Entry {
        path: "image vm ls",
        group: "Inspect",
        examples: &[
            ("distro, type (cloud-init or appliance), kernel and the vCPU/memory each image recommends", "delonix image vm ls"),
            ("as JSON, for a script", "delonix image vm ls -o json"),
        ],
        see_also: &["image vm describe", "image vm pull", "vm create"],
    },
    Entry {
        path: "image vm ls-remote",
        group: "Inspect",
        examples: &[
            ("which Kubernetes versions are published, before choosing one to pull", "delonix image vm ls-remote"),
            ("the tags of the Kubernetes-free golden repo", "delonix image vm ls-remote --no-k8s"),
            ("any other repository — the appliances, for instance", "delonix image vm ls-remote ghcr.io/angolardevops/delonix-vm-appliances"),
        ],
        see_also: &["image vm pull", "image vm ls", "image vm push"],
    },
    Entry {
        path: "image vm convert",
        group: "Maintenance",
        examples: &[
            ("hand a Delonix image to VMware", "delonix image vm convert --to vmdk delonix-vm-base:ubuntu-24.04 -o ubuntu-24.04.vmdk"),
            ("raw sectors: Proxmox VE's default on LVM/ZFS/Ceph, and the fallback anything else reads", "delonix image vm convert --to raw proxmox-ve:9.1 -o proxmox-ve.raw"),
            ("Hyper-V and Azure — `vhdx` and `vhd` are different formats, not two spellings of one", "delonix image vm convert --to vhdx delonix-vm-k8s:1.34 -o k8s-1.34.vhdx"),
            ("compressed, which only qcow2 and vmdk can be", "delonix image vm convert --to qcow2 --compress rocky-9 -o rocky-9.qcow2"),
        ],
        see_also: &["image vm import", "image vm ls", "vm create"],
    },
    Entry {
        path: "init",
        group: "",
        examples: &[
            ("look at this directory and start the right project for it, saying what it detected and why", "delonix init"),
            ("a directory other than the current one", "delonix init ./myapp"),
            ("override the guess when the detection picked the broader rule", "delonix init -t go"),
            ("regenerate on top of files that already exist", "delonix init --force"),
        ],
        see_also: &["stack init", "vm init", "stack apply", "compose up"],
    },
    Entry {
        path: "man",
        group: "",
        examples: &[
            ("read the manual for a command, the way you would any other tool", "delonix man container run | man -l -"),
            ("install every page so `man delonix-container-run` works from anywhere", "delonix man --dir ~/.local/share/man && sudo mandb"),
            ("the top-level page", "delonix man"),
        ],
        see_also: &["completion", "explain", "version"],
    },
    Entry {
        path: "net",
        group: "",
        examples: &[
            ("who can reach what: the inbound policy, rules and published ports of every container on the SDN", "delonix net ingress ls"),
            ("the state of the rootless plumbing every `--net <network>` container depends on", "delonix net netns status"),
            ("live per-container traffic, redrawn every 2s", "delonix net flow -w"),
            ("one public URL for a local port, with no account and no public IP on this host", "delonix net tunnel expose 8080"),
        ],
        see_also: &["network ls", "container run", "container update", "network create"],
    },
    Entry {
        path: "net l4guard",
        group: "Configure",
        examples: &[
            ("turn on the ingress-wide DDoS guard: at most 20 new connections/s and 100 concurrent connections per source IP", "delonix net l4guard set 20 100"),
            ("check whether it's actually doing anything, with its drop counters", "delonix net l4guard status"),
            ("turn it off", "delonix net l4guard clear"),
        ],
        see_also: &["net ingress ls", "net egress net", "net netns status"],
    },
    Entry {
        path: "net l4guard set",
        group: "Configure",
        examples: &[
            ("a conservative default for a public-facing node", "delonix net l4guard set 20 100"),
        ],
        see_also: &["net l4guard status", "net l4guard clear"],
    },
    Entry {
        path: "net l4guard clear",
        group: "Configure",
        examples: &[
            ("stop rate-limiting inbound connections", "delonix net l4guard clear"),
        ],
        see_also: &["net l4guard set", "net l4guard status"],
    },
    Entry {
        path: "net l4guard status",
        group: "Inspect",
        examples: &[
            ("is the guard on, and has it dropped anything", "delonix net l4guard status"),
        ],
        see_also: &["net l4guard set", "net ingress ls"],
    },
    Entry {
        path: "net flow",
        group: "Inspect",
        examples: &[
            ("per-container RX/TX right now", "delonix net flow"),
            ("keep it on screen, redrawn every 2s", "delonix net flow -w"),
            ("one veth only, when you already know which container you are chasing", "delonix net flow --iface vh3f2a91c4"),
        ],
        see_also: &["container stats", "net ingress ls", "net netns status"],
    },
    Entry {
        path: "net egress",
        group: "Configure",
        examples: &[
            ("what a container is allowed to reach on its way out", "delonix net egress ls app"),
            ("cut a whole network off the Internet in one command", "delonix net egress net app deny"),
            ("let it out to one hostname only, learnt live from the DNS answers", "delonix net egress host app github.com"),
        ],
        see_also: &["net ingress ls", "net egress show", "network create", "container run"],
    },
    Entry {
        path: "net egress ls",
        group: "Inspect",
        examples: &[
            ("one container's outbound policy and rules", "delonix net egress ls app"),
            ("every container's, side by side", "delonix net egress ls"),
        ],
        see_also: &["net egress show", "net ingress ls", "net egress allow"],
    },
    Entry {
        path: "net egress show",
        group: "Inspect",
        examples: &[
            ("a network's egress mode, its CIDR allowlist, and the addresses learnt so far for each allowed hostname", "delonix net egress show app"),
        ],
        see_also: &["net egress net", "net egress host", "network inspect"],
    },
    Entry {
        path: "net egress allow",
        group: "Configure",
        examples: &[
            ("let the app reach a database elsewhere on the SDN", "delonix net egress allow app tcp/5432 --to 10.200.0.20/32"),
            ("DNS out, which a default-deny policy would otherwise take away and leave nothing resolving", "delonix net egress allow app udp/53"),
            ("HTTPS anywhere, for a workload that has to talk to the outside", "delonix net egress allow app tcp/443"),
        ],
        see_also: &["net egress deny", "net egress policy", "net egress host", "net egress ls"],
    },
    Entry {
        path: "net egress deny",
        group: "Configure",
        examples: &[
            ("block SMTP, so a compromised app cannot send mail from this node", "delonix net egress deny app tcp/25"),
            ("block a whole destination network without closing the rest of the world", "delonix net egress deny app tcp/* --to 192.168.1.0/24"),
        ],
        see_also: &["net egress allow", "net egress rm", "net egress net"],
    },
    Entry {
        path: "net egress host",
        group: "Configure",
        examples: &[
            ("let a network reach GitHub and its subdomains — the FQDN allowlist that nft and CIDRs cannot express on their own", "delonix net egress host app github.com"),
            ("repeat it per name; the addresses are learnt as the DNS answers come back, so a CDN that renumbers keeps working", "delonix net egress host app registry.npmjs.org"),
        ],
        see_also: &["net egress net", "net egress show", "net egress allow"],
    },
    Entry {
        path: "net egress net",
        group: "Configure",
        examples: &[
            ("take a whole network off the Internet, containers and all", "delonix net egress net app deny"),
            ("allow nothing but DNS and the CIDRs you name", "delonix net egress net app allowlist --to 10.0.0.0/8,1.1.1.1/32"),
            ("back to the default, unrestricted", "delonix net egress net app allow"),
        ],
        see_also: &["net egress show", "net egress host", "network create"],
    },
    Entry {
        path: "net egress policy",
        group: "Configure",
        examples: &[
            ("deny by default: only what `egress allow` names gets out", "delonix net egress policy app deny"),
            ("back to open — note that a policy change never tears down flows already established, it only decides new ones", "delonix net egress policy app allow"),
        ],
        see_also: &["net egress allow", "net egress ls", "net ingress policy"],
    },
    Entry {
        path: "net egress clear",
        group: "Maintenance",
        examples: &[
            ("remove every outbound rule; the default policy stays as it was", "delonix net egress clear app"),
        ],
        see_also: &["net egress rm", "net egress policy", "net ingress clear"],
    },
    Entry {
        path: "net egress rm",
        group: "Maintenance",
        examples: &[
            ("drop every outbound rule for that port, whatever the protocol", "delonix net egress rm app 5432"),
            ("only the rule that named this destination", "delonix net egress rm app tcp/5432 --to 10.200.0.20/32"),
        ],
        see_also: &["net egress clear", "net egress allow", "net egress ls"],
    },
    Entry {
        path: "net ingress",
        group: "Configure",
        examples: &[
            ("what is open on a container, and from where", "delonix net ingress ls web"),
            ("shut the door by default, so only the rules you write let anything in", "delonix net ingress policy db deny"),
            ("then open Postgres to the SDN alone — nothing from the host's LAN", "delonix net ingress allow db tcp/5432 --from 10.200.0.0/16"),
        ],
        see_also: &["net egress ls", "net ingress publish", "container run", "net httproute apply"],
    },
    Entry {
        path: "net ingress ls",
        group: "Inspect",
        examples: &[
            ("one container: policy, rules with their packet counters, and the ports published to it", "delonix net ingress ls web"),
            ("the whole node at once — a `--net host` container shows as `n/a`, because the firewall does not govern it", "delonix net ingress ls"),
        ],
        see_also: &["net egress ls", "net ingress allow", "net flow"],
    },
    Entry {
        path: "net ingress allow",
        group: "Configure",
        examples: &[
            ("open the port the CONTAINER listens on — the DNAT has already rewritten the host port away by the time the rule is evaluated", "delonix net ingress allow web tcp/80"),
            ("Postgres, but only from the SDN", "delonix net ingress allow db tcp/5432 --from 10.200.0.0/16"),
            ("leave a reason behind, so `ingress ls` explains itself to the next person", "delonix net ingress allow api tcp/8080 --note \"public API\""),
            ("every port from a single peer, when the rule is about the source and not the service", "delonix net ingress allow db tcp/* --from 10.200.0.15/32"),
        ],
        see_also: &["net ingress deny", "net ingress policy", "net ingress ls", "net ingress rm"],
    },
    Entry {
        path: "net ingress deny",
        group: "Configure",
        examples: &[
            ("close one port without touching the default policy", "delonix net ingress deny web tcp/8080"),
            ("block one noisy peer from a port everyone else may use", "delonix net ingress deny api tcp/8080 --from 10.200.0.99/32"),
            ("the last command for the same match wins — a later `allow` on this very rule re-opens it, no need to remove anything first", "delonix net ingress deny db tcp/5432"),
        ],
        see_also: &["net ingress allow", "net ingress rm", "net ingress policy"],
    },
    Entry {
        path: "net ingress policy",
        group: "Configure",
        examples: &[
            ("default-deny: only what `ingress allow` names gets in", "delonix net ingress policy db deny"),
            ("back to open, keeping the explicit rules already written", "delonix net ingress policy db allow"),
        ],
        see_also: &["net ingress allow", "net ingress ls", "net egress policy"],
    },
    Entry {
        path: "net ingress publish",
        group: "Networking",
        examples: &[
            ("host 8080 onto the container's 80", "delonix net ingress publish web 8080:80"),
            ("the same number on both sides", "delonix net ingress publish web 8443"),
            ("a UDP service — rootless cannot bind host ports below 1024, so publish high and let a proxy own :53 if you need it", "delonix net ingress publish dns 5353:53/udp"),
        ],
        see_also: &["net ingress unpublish", "net ingress ls", "container update", "net httproute apply"],
    },
    Entry {
        path: "net ingress unpublish",
        group: "Networking",
        examples: &[
            ("give the host port back — the hostfwd and the DNAT go together, and the port stops being owned", "delonix net ingress unpublish web 8080"),
        ],
        see_also: &["net ingress publish", "net ingress ls"],
    },
    Entry {
        path: "net ingress clear",
        group: "Maintenance",
        examples: &[
            ("wipe the inbound rules and keep the published ports — the DNAT is a different plane from the filter", "delonix net ingress clear web"),
        ],
        see_also: &["net ingress rm", "net ingress unpublish", "net egress clear"],
    },
    Entry {
        path: "net ingress rm",
        group: "Maintenance",
        examples: &[
            ("drop every rule written for that port, whatever the protocol", "delonix net ingress rm web 8080"),
            ("only the rule that named this source", "delonix net ingress rm db tcp/5432 --from 10.200.0.15/32"),
        ],
        see_also: &["net ingress clear", "net ingress allow", "net ingress ls"],
    },
    Entry {
        path: "net httproute",
        group: "Networking",
        examples: &[
            ("is the proxy serving, and with which routes", "delonix net httproute ls"),
            ("apply the HTTPRoutes of a manifest — it brings the proxy up, or reloads it in place", "delonix net httproute apply -f delonix-manifest.yaml"),
            ("tear it down and give the host ports back", "delonix net httproute rm"),
        ],
        see_also: &["net ingress publish", "net tunnel expose", "stack apply", "container run"],
    },
    Entry {
        path: "net httproute apply",
        group: "",
        examples: &[
            ("the routes from the manifest in this directory", "delonix net httproute apply"),
            ("from a file of your own — a re-apply reloads by SIGHUP, same pid, no dropped connection", "delonix net httproute apply -f examples/httproute.yaml"),
            ("listeners and TLS are fixed when the proxy starts: changing a port means `httproute rm` and then this again", "delonix net httproute apply -f examples/httproute.yaml"),
        ],
        see_also: &["net httproute ls", "net httproute rm", "stack apply", "secret create"],
    },
    Entry {
        path: "net httproute ls",
        group: "",
        examples: &[
            ("proxy state and the routes in effect, read from the config the proxy is really serving — not from the manifest you last edited", "delonix net httproute ls"),
        ],
        see_also: &["net httproute apply", "net httproute rm", "net ingress ls"],
    },
    Entry {
        path: "net httproute rm",
        group: "",
        examples: &[
            ("stop the proxy and unpublish its ports — only the MANUAL routes go, the ones auto-registered by `container run --expose` survive", "delonix net httproute rm"),
        ],
        see_also: &["net httproute apply", "net httproute ls", "container rm"],
    },
    Entry {
        path: "net tunnel",
        group: "Networking",
        examples: &[
            ("a local port on the public internet, with no account and no public IP on this host", "delonix net tunnel expose 8080"),
            ("the tunnels and the public URLs they got", "delonix net tunnel ls"),
            ("stop one when the demo is over", "delonix net tunnel rm demo"),
        ],
        see_also: &["net httproute apply", "net ingress publish", "secret create", "stack apply"],
    },
    Entry {
        path: "net tunnel expose",
        group: "Lifecycle",
        examples: &[
            ("pinggy needs no account and no extra binary — it is plain ssh, and the free URL is ephemeral", "delonix net tunnel expose 8080"),
            ("a name of your own, instead of the `tunnel-<port>` default", "delonix net tunnel expose 8080 --name demo"),
            ("ngrok, with the authtoken read from a secret rather than typed into argv", "delonix net tunnel expose 8080 --provider ngrok --token-secret ngrok-token"),
            ("a cloudflare quick tunnel — random trycloudflare.com URL, no account, `cloudflared` must be on PATH", "delonix net tunnel expose 8080 --provider cloudflare"),
            ("a cloudflare NAMED tunnel you already created — its public hostname is whatever you configured for it in the dashboard", "delonix net tunnel expose 8080 --provider cloudflare --token-secret cf-tunnel-token"),
        ],
        see_also: &["net tunnel ls", "net tunnel rm", "net tunnel apply", "net httproute apply"],
    },
    Entry {
        path: "net tunnel rm",
        group: "Lifecycle",
        examples: &[
            ("stop the provider's agent and forget the tunnel — the URL dies with it", "delonix net tunnel rm demo"),
        ],
        see_also: &["net tunnel ls", "net tunnel expose", "net tunnel apply"],
    },
    Entry {
        path: "net tunnel describe",
        group: "Inspect",
        examples: &[
            ("one tunnel in full: provider, local port, the agent's pid and the URL in effect", "delonix net tunnel describe demo"),
        ],
        see_also: &["net tunnel ls", "net tunnel rm", "net tunnel expose"],
    },
    Entry {
        path: "net tunnel ls",
        group: "Inspect",
        examples: &[
            ("every tunnel, its state and the public URL the provider gave it", "delonix net tunnel ls"),
        ],
        see_also: &["net tunnel describe", "net tunnel expose", "net tunnel rm"],
    },
    Entry {
        path: "net tunnel apply",
        group: "Declarative",
        examples: &[
            ("the `kind: Tunnel` documents of the manifest here — idempotent, an unchanged tunnel is left alone", "delonix net tunnel apply"),
            ("from a file, where the token comes from a `kind: Secret` instead of the shell history", "delonix net tunnel apply -f delonix-manifest.yaml"),
        ],
        see_also: &["net tunnel expose", "net tunnel ls", "secret create", "stack apply"],
    },
    Entry {
        path: "net boot",
        group: "Advanced",
        examples: &[
            ("make the containers running right now come back after a reboot", "delonix net boot enable"),
            ("what is installed, and in which mode", "delonix net boot status"),
            ("undo it", "delonix net boot disable"),
        ],
        see_also: &["container run", "container start", "system info"],
    },
    Entry {
        path: "net boot disable",
        group: "",
        examples: &[
            ("disable and delete the generated units — what is running keeps running, it just no longer comes back by itself", "delonix net boot disable"),
        ],
        see_also: &["net boot enable", "net boot status"],
    },
    Entry {
        path: "net boot enable",
        group: "",
        examples: &[
            ("one unit per RUNNING container; rootless installs user units and turns linger on, so they start without a login", "delonix net boot enable"),
            ("bake a gentler policy into the units than the `always` default", "delonix net boot enable --restart on-failure:3"),
        ],
        see_also: &["net boot status", "net boot disable", "container run"],
    },
    Entry {
        path: "net boot status",
        group: "",
        examples: &[
            ("which units exist, and whether they are user units with linger or system units", "delonix net boot status"),
        ],
        see_also: &["net boot enable", "net boot disable", "container ps"],
    },
    Entry {
        path: "net netns",
        group: "Advanced",
        examples: &[
            ("holder and slirp pids, the bridge, and how many containers still hold a reference", "delonix net netns status"),
            ("bring the plumbing up by hand — idempotent, so it is safe before debugging a path", "delonix net netns up"),
            ("look at the SDN from the inside of an attached netns", "delonix net netns exec dbg1 ip addr show"),
        ],
        see_also: &["net ingress ls", "net flow", "network create", "system info"],
    },
    Entry {
        path: "net netns down",
        group: "Lifecycle",
        examples: &[
            ("force the plumbing down — every container on the SDN loses its network until it is rebuilt, so this is an operator decision, never automatic", "delonix net netns down"),
            ("the recovery after an in-place upgrade left a holder from the previous binary behind", "delonix net netns down && delonix net netns up"),
        ],
        see_also: &["net netns up", "net netns status", "container start"],
    },
    Entry {
        path: "net netns up",
        group: "Lifecycle",
        examples: &[
            ("holder netns, the delonix0 bridge and the single slirp — idempotent, repeat it as often as you like", "delonix net netns up"),
            ("it also hardens containers that are already running (IPv6 off in their netns) without restarting them", "delonix net netns up"),
        ],
        see_also: &["net netns status", "net netns down", "container run"],
    },
    Entry {
        path: "net netns status",
        group: "Inspect",
        examples: &[
            ("the first thing to read when a container has no network", "delonix net netns status"),
            ("the same facts as JSON, for a monitoring script", "delonix net netns status --json"),
        ],
        see_also: &["net netns up", "net flow", "system info"],
    },
    Entry {
        path: "net netns exec",
        group: "Interact",
        examples: &[
            ("the address the holder actually handed this netns", "delonix net netns exec dbg1 ip addr show"),
            ("the way out through the holder's bridge, when traffic leaves but never comes back", "delonix net netns exec dbg1 ip route show"),
        ],
        see_also: &["net netns attach", "container exec", "net flow"],
    },
    Entry {
        path: "net netns attach",
        group: "Networking",
        examples: &[
            ("give a netns a veth on delonix0, with an address derived from its name", "delonix net netns attach dbg1"),
            ("pin the address instead, when something on the network already expects it", "delonix net netns attach dbg1 --ip 10.200.0.42"),
        ],
        see_also: &["net netns detach", "net netns exec", "net netns publish"],
    },
    Entry {
        path: "net netns detach",
        group: "Networking",
        examples: &[
            ("destroy the netns and give its veth and address back to the pool", "delonix net netns detach dbg1"),
        ],
        see_also: &["net netns attach", "net netns status"],
    },
    Entry {
        path: "net netns firewall",
        group: "Networking",
        examples: &[
            ("install a default-deny inbound firewall with a single open port, straight at the ingress", "delonix net netns firewall web --spec '{\"enabled\":true,\"policyIn\":\"deny\",\"rules\":[{\"dir\":\"in\",\"proto\":\"tcp\",\"port\":\"80\",\"action\":\"allow\"}]}'"),
            ("take the container's firewall out of the ingress entirely", "delonix net netns firewall web --clear"),
        ],
        see_also: &["net ingress allow", "net ingress policy", "net ingress ls"],
    },
    Entry {
        path: "net netns publish",
        group: "Networking",
        examples: &[
            ("host 8080 onto the container's 80 — the hostfwd and the DNAT in one step", "delonix net netns publish web 8080:80"),
            ("a UDP service, with the container address spelled out instead of derived from the name", "delonix net netns publish dns 5353:53/udp --ip 10.200.0.42"),
        ],
        see_also: &["net netns unpublish", "net ingress publish", "container run"],
    },
    Entry {
        path: "net netns unpublish",
        group: "Networking",
        examples: &[
            ("take the host port back — the slirp hostfwd and the DNAT rule both go", "delonix net netns unpublish 8080"),
        ],
        see_also: &["net netns publish", "net ingress unpublish"],
    },
    Entry {
        path: "namespace",
        group: "",
        examples: &[
            ("every namespace in use, and what is in each", "delonix namespace ls"),
            ("what one tenant holds, and whether the boundary is enforced here", "delonix namespace describe inquilino-b"),
        ],
        see_also: &["container run", "vm create", "net ingress", "volumes prune"],
    },
    Entry {
        path: "namespace ls",
        group: "Inspect",
        examples: &[
            ("counts by Kind, per namespace", "delonix namespace ls"),
            ("for a script", "delonix namespace ls -o json"),
        ],
        see_also: &["namespace describe"],
    },
    Entry {
        path: "namespace describe",
        group: "Inspect",
        examples: &[
            ("the contents by Kind, plus the nft set that carries the boundary", "delonix namespace describe inquilino-b"),
        ],
        see_also: &["namespace ls", "net netns firewall"],
    },
    Entry {
        path: "network",
        group: "",
        examples: &[
            ("a bridge network of its own, where containers reach each other by name", "delonix network create app"),
            ("what exists, with driver, bridge and subnet", "delonix network ls"),
            ("the detail of one, including whether it was physically realized", "delonix network describe app"),
        ],
        see_also: &["container run", "net ingress", "net httproute", "pod create"],
    },
    Entry {
        path: "network create",
        group: "Lifecycle",
        examples: &[
            ("a bridge network — the default driver, and the one rootless really realizes", "delonix network create app"),
            ("pin the address space instead of letting the engine pick a free octet", "delonix network create backend --subnet 10.220.0.0/16"),
            ("an encrypted VXLAN overlay across nodes, realized in the rootless holder", "delonix network create mesh --driver overlay --vni 42 --peer 10.0.0.7 --wg-ip 10.42.0.1"),
            ("macvlan is registered but NOT realized without privilege — the warning says so instead of pretending", "delonix network create lan --driver macvlan --parent eth0 --subnet 192.168.1.0/24"),
        ],
        see_also: &["network ls", "network rm", "network node init", "container run"],
    },
    Entry {
        path: "network vlan",
        group: "Lifecycle",
        examples: &[
            ("see the plan — this is the ONE network command that needs root, so it changes nothing until you say so", "delonix network vlan eth0 100"),
            ("run it for real, as root", "sudo delonix network vlan eth0 100 --apply"),
            ("take it back out", "sudo delonix network vlan eth0 100 --rm --apply"),
        ],
        see_also: &["network create", "network route", "vm bridge"],
    },
    Entry {
        path: "network route",
        group: "Lifecycle",
        examples: &[
            ("open a DIRECTED path: web may reach db, db may not reach web", "delonix network route web db"),
            ("close it again", "delonix network route web db --rm"),
        ],
        see_also: &["network create", "net ingress", "container run"],
    },
    Entry {
        path: "network rm",
        group: "Lifecycle",
        examples: &[
            ("remove a network no container is attached to — both the record and the holder's bridge", "delonix network rm app"),
        ],
        see_also: &["network ls", "network create", "container update"],
    },
    Entry {
        path: "network describe",
        group: "Inspect",
        examples: &[
            ("a readable block instead of the compact view", "delonix network describe app"),
            ("several networks in one go", "delonix network describe app backend"),
        ],
        see_also: &["network inspect", "network ls", "container describe"],
    },
    Entry {
        path: "network inspect",
        group: "Inspect",
        examples: &[
            ("driver, bridge, subnet and gateway of one network", "delonix network inspect app"),
        ],
        see_also: &["network describe", "network ls", "net ingress ls"],
    },
    Entry {
        path: "network ls",
        group: "Inspect",
        examples: &[
            ("name, driver, bridge and subnet of each network", "delonix network ls"),
            ("as JSON, to feed automation", "delonix network ls -o json"),
        ],
        see_also: &["network inspect", "network describe", "network dash"],
    },
    Entry {
        path: "network apply",
        group: "Declarative",
        examples: &[
            ("apply only the `kind: Network` documents of a manifest, idempotent by name", "delonix network apply -f delonix-manifest.yaml"),
            ("the networks of a shipped example, leaving the other kinds alone", "delonix network apply -f examples/network.yaml"),
        ],
        see_also: &["stack apply", "stack plan", "network create", "volumes apply"],
    },
    Entry {
        path: "network dash",
        group: "Dashboards",
        examples: &[
            ("a live TUI of the networks and their traffic", "delonix network dash"),
            ("one snapshot, for a pipe or a terminal without a TTY", "delonix network dash --once"),
            ("JSON, to feed a script or a dashboard", "delonix network dash --json"),
        ],
        see_also: &["dash", "network ls", "system monitor"],
    },
    Entry {
        path: "network node",
        group: "Advanced",
        examples: &[
            ("create this node's WireGuard identity and print the public key to hand out", "delonix network node init"),
            ("the public key alone, for composing in a script", "delonix network node key"),
        ],
        see_also: &["network create", "network inspect", "net netns status"],
    },
    Entry {
        path: "network node key",
        group: "Inspect",
        examples: &[
            ("just the public key, to paste into the peer configuration of another node", "delonix network node key"),
        ],
        see_also: &["network node init", "network create", "network inspect"],
    },
    Entry {
        path: "network node init",
        group: "Configure",
        examples: &[
            ("generate the key once and print what to do with it — idempotent, the private half stays 0600", "delonix network node init"),
        ],
        see_also: &["network node key", "network create", "network describe"],
    },
    Entry {
        path: "pod",
        group: "",
        examples: &[
            ("which pods exist and which containers each one holds", "delonix pod ls"),
            ("create one from a manifest — N containers on ONE IP, reaching each other over localhost", "delonix pod create -f examples/pod-multi.yaml"),
            ("the logs of a chosen member", "delonix pod logs web --container api"),
        ],
        see_also: &["pod create", "container run", "workload ls", "stack apply"],
    },
    Entry {
        path: "pod create",
        group: "Lifecycle",
        examples: &[
            ("N containers sharing one netns — same IP and `localhost` between them, like a k8s Pod", "delonix pod create -f examples/pod-multi.yaml"),
            ("from the default manifest in the current directory", "delonix pod create"),
        ],
        see_also: &["pod ls", "pod describe", "stack apply"],
    },
    Entry {
        path: "pod rm",
        group: "Lifecycle",
        examples: &[
            ("stop and remove every member plus the shared netns — nothing is left holding the IP", "delonix pod rm web"),
            ("kill the members that are still running", "delonix pod rm -f web"),
        ],
        see_also: &["pod ls", "pod create", "container rm"],
    },
    Entry {
        path: "pod describe",
        group: "Inspect",
        examples: &[
            ("members, the shared IP and the status, kubectl style", "delonix pod describe web"),
            ("several pods in one pass", "delonix pod describe web api"),
        ],
        see_also: &["pod ls", "pod logs", "container describe"],
    },
    Entry {
        path: "pod logs",
        group: "Inspect",
        examples: &[
            ("the first member's logs, which is what you usually want", "delonix pod logs web"),
            ("a specific container inside the pod, by its short name", "delonix pod logs web --container api"),
            ("follow it live", "delonix pod logs -f web"),
        ],
        see_also: &["pod describe", "pod ls", "container logs"],
    },
    Entry {
        path: "pod ls",
        group: "Inspect",
        examples: &[
            ("the pods, derived from the container labels — there is no separate store to drift", "delonix pod ls"),
            ("as JSON, for a script", "delonix pod ls -o json"),
        ],
        see_also: &["pod describe", "pod logs", "container ps"],
    },
    Entry {
        path: "schema",
        group: "",
        examples: &[
            ("the schema generated from the code — point an editor at it and the manifest completes as you type", "delonix schema print > delonix.schema.json"),
            ("one Kind only", "delonix schema print --kind Container"),
        ],
        see_also: &["schema print", "explain", "stack validate", "stack apply"],
    },
    Entry {
        path: "schema print",
        group: "Inspect",
        examples: &[
            ("the whole schema, ready for `# yaml-language-server: $schema=./delonix.schema.json`", "delonix schema print > delonix.schema.json"),
            ("just one Kind's spec, when that is all you are editing", "delonix schema print --kind Pod"),
        ],
        see_also: &["explain", "stack validate", "stack apply"],
    },
    Entry {
        path: "secret",
        group: "",
        examples: &[
            ("what exists — names and key counts; the values are never printed", "delonix secret ls"),
            ("create one without the value ever reaching argv or the shell history", "printf 'PASSWORD=s3cr3t\\n' | delonix secret create db-pass --from-env-file -"),
            ("hand it to a container, decrypted only at spawn — the registry keeps the name, never the value", "delonix container run -d --secret db-pass postgres:16"),
        ],
        see_also: &["secret create", "secret ls", "container run", "stack apply"],
    },
    Entry {
        path: "secret create",
        group: "Lifecycle",
        examples: &[
            ("from literal pairs — quick, but the value lands in your shell history", "delonix secret create db-pass --from-literal PASSWORD=s3cr3t"),
            ("from a `.env` file, every key at once", "delonix secret create db-pass --from-env-file .env"),
            ("from stdin — the dry form: the value touches neither argv nor the history", "printf 'PASSWORD=s3cr3t\\n' | delonix secret create db-pass --from-env-file -"),
        ],
        see_also: &["secret set", "secret ls", "secret apply", "container run"],
    },
    Entry {
        path: "secret rm",
        group: "Lifecycle",
        examples: &[
            ("remove a secret for good — a container that names it fails loudly on the next start", "delonix secret rm db-pass"),
        ],
        see_also: &["secret ls", "secret unset", "secret create"],
    },
    Entry {
        path: "secret inspect",
        group: "Inspect",
        examples: &[
            ("which keys a secret carries, with the values redacted", "delonix secret inspect db-pass"),
            ("the values in cleartext — avoid it on a shared terminal", "delonix secret inspect db-pass --reveal"),
        ],
        see_also: &["secret ls", "secret set", "secret unset"],
    },
    Entry {
        path: "secret ls",
        group: "Inspect",
        examples: &[
            ("the vault at a glance — name and number of keys, never a value", "delonix secret ls"),
            ("as JSON, for a script", "delonix secret ls -o json"),
        ],
        see_also: &["secret inspect", "secret create", "secret rm"],
    },
    Entry {
        path: "secret set",
        group: "Configure",
        examples: &[
            ("add or update one key and leave the rest untouched", "delonix secret set db-pass PASSWORD=n3w"),
            ("several keys at once — creates the secret if it does not exist yet", "delonix secret set db-pass USER=app PASSWORD=n3w"),
        ],
        see_also: &["secret unset", "secret create", "secret inspect"],
    },
    Entry {
        path: "secret unset",
        group: "Configure",
        examples: &[
            ("drop a single key, keeping the secret and everything else in it", "delonix secret unset db-pass PASSWORD"),
            ("empty the whole secret in one go", "delonix secret unset db-pass --all"),
        ],
        see_also: &["secret set", "secret rm", "secret inspect"],
    },
    Entry {
        path: "secret apply",
        group: "Declarative",
        examples: &[
            ("create the `kind: Secret` documents of the manifest in this directory", "delonix secret apply"),
            ("from a file of your own, so the vault is provisioned from git like everything else", "delonix secret apply -f examples/secret.yaml"),
        ],
        see_also: &["secret create", "stack apply", "schema print"],
    },
    Entry {
        path: "secret rotate-key",
        group: "Maintenance",
        examples: &[
            ("re-encrypt every secret under a new host master key; the values are preserved", "delonix secret rotate-key"),
        ],
        see_also: &["secret ls", "secret inspect", "secret create"],
    },
    Entry {
        path: "serve",
        group: "",
        examples: &[
            ("be the runtime a kubelet talks to, in place of containerd/CRI-O", "delonix serve cri"),
            ("answer a real docker CLI on a socket of your own", "delonix serve docker-api --addr unix:///tmp/delonix-docker.sock"),
        ],
        see_also: &["serve cri", "serve docker-api", "cluster kubeadm"],
    },
    Entry {
        path: "serve api",
        group: "Advanced",
        examples: &[
            ("the management API on the default unix socket, for a control-plane on this same host", "delonix serve api"),
            ("a socket of your own", "delonix serve api --addr unix:///tmp/delonix-mgmt.sock"),
        ],
        see_also: &["serve cri", "dash", "system info"],
    },
    Entry {
        path: "serve cri",
        group: "Advanced",
        examples: &[
            ("the CRI endpoint on the default socket — point the kubelet's `--container-runtime-endpoint` at it", "delonix serve cri"),
            ("a socket of your own", "delonix serve cri --addr unix:///run/delonix-cri.sock"),
            ("a node-level bound on capabilities that holds even if the admission chain is misconfigured", "delonix serve cri --cap-ceiling default,NET_ADMIN"),
            ("clamp instead of refusing, for a node whose PodSpecs you cannot change today", "delonix serve cri --cap-ceiling default --cap-ceiling-mode clamp"),
        ],
        see_also: &["serve api", "cluster kubeadm", "container run"],
    },
    Entry {
        path: "serve docker-api",
        group: "Advanced",
        examples: &[
            ("answer `docker ps`/`images`/`info` and the container lifecycle on a unix socket", "delonix serve docker-api"),
            ("a socket of your own, then `export DOCKER_HOST=unix:///tmp/delonix-docker.sock`", "delonix serve docker-api --addr unix:///tmp/delonix-docker.sock"),
            ("see where the slice ends before third-party tooling hits a 404 mid-run", "delonix serve docker-api --matrix"),
        ],
        see_also: &["serve api", "compose up", "container ps"],
    },
    Entry {
        path: "sharevolume",
        group: "",
        examples: &[
            ("carve isolated, individually-quota'd slices out of one NAS export", "delonix sharevolume apply -f examples/sharevolume.yaml"),
            ("what slices exist, with parent storage, quota and live usage", "delonix sharevolume ls"),
            ("how close one of them is to its quota", "delonix sharevolume describe app-data"),
        ],
        see_also: &["storage", "volumes ls", "stack apply", "container run"],
    },
    Entry {
        path: "sharevolume rm",
        group: "Lifecycle",
        examples: &[
            ("un-register the slice — the data on the NAS is PRESERVED", "delonix sharevolume rm app-data"),
            ("un-register and delete the underlying subdirectory as well", "delonix sharevolume rm app-data --purge-data"),
            ("a slice owned by a namespace other than `default`", "delonix sharevolume rm app-data -n backend"),
        ],
        see_also: &["sharevolume ls", "storage rm", "volumes rm"],
    },
    Entry {
        path: "sharevolume describe",
        group: "Inspect",
        examples: &[
            ("quota, alert threshold and real usage of one slice", "delonix sharevolume describe app-data"),
            ("a slice owned by another namespace", "delonix sharevolume describe app-data -n backend"),
        ],
        see_also: &["sharevolume ls", "storage inspect", "volumes describe"],
    },
    Entry {
        path: "sharevolume ls",
        group: "Inspect",
        examples: &[
            ("parent storage, quota and measured usage of each slice", "delonix sharevolume ls"),
            ("as JSON, to alert on a slice approaching its quota", "delonix sharevolume ls -o json"),
        ],
        see_also: &["sharevolume describe", "storage ls", "volumes inspect"],
    },
    Entry {
        path: "sharevolume apply",
        group: "Declarative",
        examples: &[
            ("create or converge the slices declared in a manifest, idempotent", "delonix sharevolume apply -f examples/sharevolume.yaml"),
            ("from your own manifest — this is how a share volume is created", "delonix sharevolume apply -f delonix-manifest.yaml"),
        ],
        see_also: &["storage apply", "stack apply", "sharevolume ls", "sharevolume describe"],
    },
    Entry {
        path: "sharevolume migrate",
        group: "Maintenance",
        examples: &[
            ("see which pre-scoping records would move into the `default` namespace, changing nothing", "delonix sharevolume migrate --dry-run"),
            ("move the records — only the bookkeeping, the bytes stay exactly where they are", "delonix sharevolume migrate"),
        ],
        see_also: &["sharevolume ls", "sharevolume describe", "sharevolume apply"],
    },
    Entry {
        path: "stack",
        group: "",
        examples: &[
            ("everything a manifest declares, in dependency order", "delonix stack apply -f delonix-manifest.yaml"),
            ("what would change, before anything changes", "delonix stack plan -f prod.yaml"),
            ("a complete project, files already filled in", "delonix stack init -t python"),
        ],
        see_also: &["container apply", "compose up", "cluster apply", "schema print"],
    },
    Entry {
        path: "stack apply",
        group: "Lifecycle",
        examples: &[
            ("converge the machine onto the manifest — creates what is missing and updates what drifted", "delonix stack apply -f delonix-manifest.yaml"),
            ("the full manifest with every default filled in, applying nothing", "delonix stack apply -f prod.yaml --dry-run"),
            ("authorize the one recreation the plan asked for — a cold field cannot converge live", "delonix stack apply -f prod.yaml --replace Container/web"),
            ("also remove what this stack owns and the file no longer declares", "delonix stack apply -f prod.yaml --prune"),
        ],
        see_also: &["stack plan", "stack wait", "stack destroy", "stack validate"],
    },
    Entry {
        path: "stack destroy",
        group: "Lifecycle",
        examples: &[
            ("remove everything this stack owns, in the reverse of the creation order", "delonix stack destroy -f prod.yaml"),
            ("read the list first — nothing is removed", "delonix stack destroy -f prod.yaml --dry-run"),
            ("when the file declares no Stack, say whose resources these are", "delonix stack destroy -f prod.yaml --name shop"),
        ],
        see_also: &["stack apply", "stack plan", "stack ls", "compose down"],
    },
    Entry {
        path: "stack prune",
        group: "Maintenance",
        examples: &[
            ("remove what this stack owns and the manifest no longer declares, WITHOUT re-running everything else the file declares", "delonix stack prune -f prod.yaml"),
            ("see the list first and remove nothing", "delonix stack prune -f prod.yaml --dry-run"),
        ],
        see_also: &["stack apply", "stack destroy", "stack plan", "stack ls"],
    },
    Entry {
        path: "stack wait",
        group: "Lifecycle",
        examples: &[
            ("block until everything declared is present and, where it has one, healthy", "delonix stack wait -f prod.yaml"),
            ("give a slow image pull three minutes before failing the pipeline", "delonix stack wait -f prod.yaml --timeout 180"),
        ],
        see_also: &["stack apply", "stack describe", "container healthcheck"],
    },
    Entry {
        path: "stack init",
        group: "Create",
        examples: &[
            ("a complete Python project — Delonixfile, manifest and README, runnable without editing anything", "delonix stack init -t python"),
            ("which templates exist", "delonix stack init --template list"),
            ("scaffold into a directory of its own, with the name and image you want", "delonix stack init ./api --name api --image nginx:alpine"),
            ("generate, build and wait until it answers healthy", "delonix stack init -t go --up"),
        ],
        see_also: &["stack apply", "stack plan", "init", "cluster init"],
    },
    Entry {
        path: "stack describe",
        group: "Inspect",
        examples: &[
            ("each declared resource in kubectl-describe style, confirmed against its store", "delonix stack describe -f prod.yaml"),
            ("after an apply that died halfway, see exactly what did get created", "delonix stack describe"),
        ],
        see_also: &["stack ls", "stack plan", "container describe"],
    },
    Entry {
        path: "stack ls",
        group: "Inspect",
        examples: &[
            ("every resource the manifest composes, and whether it exists yet", "delonix stack ls -f prod.yaml"),
            ("the manifest in this directory", "delonix stack ls"),
        ],
        see_also: &["stack describe", "stack plan", "stack wait"],
    },
    Entry {
        path: "stack plan",
        group: "Inspect",
        examples: &[
            ("what an apply would change — the machine is not touched", "delonix stack plan -f prod.yaml"),
            ("a drift gate in CI: exit 2 means the machine no longer matches the file", "delonix stack plan -f prod.yaml --detailed-exitcode"),
            ("machine-readable, for a bot to comment on a pull request", "delonix stack plan -f prod.yaml -o json"),
            ("which fields the plan compares, per Kind, and which it does not — answers why a change is not showing up", "delonix stack plan --fields"),
        ],
        see_also: &["stack apply", "stack validate", "stack describe", "stack destroy"],
    },
    Entry {
        path: "stack validate",
        group: "Inspect",
        examples: &[
            ("resolve every cross-reference before an apply that has no rollback", "delonix stack validate -f prod.yaml"),
            ("the manifest in this directory, as a pre-commit check", "delonix stack validate"),
        ],
        see_also: &["stack plan", "stack apply", "schema print", "explain"],
    },
    Entry {
        path: "storage",
        group: "",
        examples: &[
            ("a NAS export becomes a named volume any container can mount", "delonix storage create nas --type nfs --server 10.0.0.5 --share /mnt/pool/media"),
            ("what is mounted, and from where", "delonix storage ls"),
            ("the server, export and mount options really in use", "delonix storage inspect nas"),
        ],
        see_also: &["sharevolume", "volumes create", "secret create", "container run"],
    },
    Entry {
        path: "storage create",
        group: "Lifecycle",
        examples: &[
            ("an NFS export from a NAS — mounting needs CAP_SYS_ADMIN, so root or a privileged session", "delonix storage create nas --type nfs --server 10.0.0.5 --share /mnt/pool/media"),
            ("an SMB share with the password read from the vault, never from the shell history", "delonix storage create backups --type smb --server nas.local --share backups --username delonix --password-secret nas-creds"),
            ("read-only, with extra mount options appended to the derived ones", "delonix storage create media --type nfs --server 10.0.0.5 --share /mnt/pool/media --read-only --options vers=4.1,soft"),
            ("WebDAV from a Nextcloud instance", "delonix storage create cloud --type webdav --server https://cloud.example.org --share /remote.php/dav/files/delonix --username delonix --password-secret cloud-creds"),
        ],
        see_also: &["storage ls", "sharevolume apply", "secret create", "volumes create"],
    },
    Entry {
        path: "storage rm",
        group: "Lifecycle",
        examples: &[
            ("unmount and unregister it — the DATA stays on the NAS, only the local mount goes", "delonix storage rm nas"),
        ],
        see_also: &["storage ls", "sharevolume rm", "volumes rm"],
    },
    Entry {
        path: "storage inspect",
        group: "Inspect",
        examples: &[
            ("server, export and the mount options actually derived for it", "delonix storage inspect nas"),
        ],
        see_also: &["storage ls", "volumes inspect", "sharevolume describe"],
    },
    Entry {
        path: "storage ls",
        group: "Inspect",
        examples: &[
            ("the network storages, with type and mountpoint", "delonix storage ls"),
            ("as JSON, for automation", "delonix storage ls -o json"),
        ],
        see_also: &["storage inspect", "volumes ls", "sharevolume ls"],
    },
    Entry {
        path: "storage apply",
        group: "Declarative",
        examples: &[
            ("declare the shares in a manifest instead of typing credentials on the command line", "delonix storage apply -f examples/storage.yaml"),
            ("the storages of your own manifest", "delonix storage apply -f delonix-manifest.yaml"),
        ],
        see_also: &["stack apply", "sharevolume apply", "secret apply", "volumes apply"],
    },
    Entry {
        path: "storage dash",
        group: "Dashboards",
        examples: &[
            ("a live TUI of storages and volumes, with usage per area", "delonix storage dash"),
            ("one snapshot, for CI or a terminal without a TTY", "delonix storage dash --once"),
            ("JSON, to alert on a share filling up", "delonix storage dash --json"),
        ],
        see_also: &["dash", "storage ls", "system df"],
    },
    Entry {
        path: "syntax",
        group: "",
        examples: &[
            ("vim/neovim — the syntax AND the ftdetect that activates it", "delonix syntax vim --dir ~/.vim"),
            ("VS Code, as an extension directory (active in the next window)", "delonix syntax vscode --dir ~/.vscode/extensions/delonix.vmfile-0.1.0"),
            ("just the grammar, to place it yourself", "delonix syntax vim > ~/.vim/syntax/vmfile.vim"),
        ],
        see_also: &["vm init", "vm build", "completion"],
    },
    Entry {
        path: "system",
        group: "",
        examples: &[
            ("is this rootless, is the cgroup delegated, what is the network doing", "delonix system info"),
            ("where the disk went", "delonix system df"),
            ("get it back, orphan container directories included", "delonix system prune -f"),
        ],
        see_also: &["dash", "system events", "container stats", "image ls"],
    },
    Entry {
        path: "backup",
        group: "Configure",
        examples: &[
            ("the record and the volumes' data — not the image, which restore pulls back", "delonix backup container db"),
            ("somewhere that is not here (a directory, or a named volume on a NAS)", "delonix backup container db --to volume:nas-backups"),
            ("twice a day on a systemd user timer, keeping the newest two", "delonix backup container db --max-for-day 2 --to /srv/backups"),
            ("or on your own schedule, in crontab syntax", "delonix backup stack loja --cron \"30 3 * * 1\" --to /srv/backups"),
            ("a RUNNING VM — the guest does not pause and its PID does not change", "delonix backup vm dev --to /srv/backups"),
            ("guest-filesystem consistency, if qemu-guest-agent is installed in it", "delonix backup vm dev --quiesce --to /srv/backups"),
            ("actually stop it, for an application that keeps state only in RAM", "delonix backup container cache --stop --to /srv/backups"),
            ("see what would go in, without writing anything", "delonix backup pod api --dry-run"),
        ],
        see_also: &["restore", "system backup", "volumes snapshot", "vm snapshot"],
    },
    Entry {
        path: "restore",
        group: "Configure",
        examples: &[
            ("put the data back (refuses while it is running — that would corrupt it)", "delonix restore container container-db-20260811-205312.tar.gz"),
            ("stop it, restore, start it again", "delonix restore container ./container-db-20260811-205312.tar.gz --force"),
            ("from the directory the backups live in, by bare name", "delonix restore stack stack-loja-20260811-210000 --from /srv/backups"),
            ("what it would touch, without touching it", "delonix restore vm vm-dev-20260811-210109.tar.gz --dry-run"),
        ],
        see_also: &["backup", "system restore", "container start", "vm start"],
    },
    Entry {
        path: "system backup",
        group: "Configure",
        examples: &[
            ("the registries, IPAM, secrets and PKI — everything that cannot be rebuilt", "delonix system backup"),
            ("name the file yourself", "delonix system backup -o /mnt/nas/node-a.tar.gz"),
            ("take the volumes' data with it (this is the part that can be hundreds of GiB)", "delonix system backup --volumes"),
            ("to rebuild a node from scratch: without the key the secrets never decrypt there", "delonix system backup --volumes --include-master-key"),
        ],
        see_also: &["system restore", "volumes snapshot", "system df", "secret ls"],
    },
    Entry {
        path: "system restore",
        group: "Configure",
        examples: &[
            ("what it would change, without writing anything", "delonix system restore node-a.tar.gz --dry-run"),
            ("put the state back (refuses while a container or VM is still running)", "delonix system restore node-a.tar.gz"),
            ("restore anyway, accepting that the running workloads lose their registry", "delonix system restore node-a.tar.gz --force"),
        ],
        see_also: &["system backup", "container ls", "vm ls", "secret ls"],
    },
    Entry {
        path: "system df",
        group: "Inspect",
        examples: &[
            ("disk usage by area: images, containers, volumes and VM images", "delonix system df"),
        ],
        see_also: &["system prune", "image ls", "volumes ls", "system info"],
    },
    Entry {
        path: "system events",
        group: "Inspect",
        examples: &[
            ("what the engine did, oldest first — with no daemon this is a shared append-only log", "delonix system events"),
            ("only the last twenty lines", "delonix system events -n 20"),
            ("follow it live while you reproduce a crash in another terminal", "delonix system events -f"),
        ],
        see_also: &["system info", "container logs", "system monitor"],
    },
    Entry {
        path: "system info",
        group: "Inspect",
        examples: &[
            ("engine state in one screen: rootless, cgroup delegation, network infra, counts", "delonix system info"),
            ("the same screen in Portuguese", "delonix system info --l18n pt"),
        ],
        see_also: &["system setup", "system df", "system virt", "dash"],
    },
    Entry {
        path: "system setup",
        group: "Configure",
        examples: &[
            ("diagnose cgroup delegation — without it --memory and --cpus are accepted and inert", "delonix system setup"),
            ("apply the fix instead of only reporting it (the system-wide drop-in needs root)", "delonix system setup --delegate"),
        ],
        see_also: &["system info", "container run", "container update", "cluster create"],
    },
    Entry {
        path: "system virt",
        group: "Configure",
        examples: &[
            ("what the host offers a VM: hypervisor, KVM, virtio — and what is left to tune", "delonix system virt"),
            ("apply the recommended tuning (needs root)", "delonix system virt --tune"),
        ],
        see_also: &["vm create", "vm status", "system info"],
    },
    Entry {
        path: "system prune",
        group: "Maintenance",
        examples: &[
            ("reclaim space: stopped containers, unused images, unreferenced blobs, orphan rootfs directories", "delonix system prune"),
            ("in CI, where there is no terminal to confirm at", "delonix system prune -f"),
            ("from a nightly timer: reclaim only when the disk is at 75% or above, and do nothing at all below it", "delonix system prune --auto --threshold 75"),
            ("also drop tagged images nobody uses, not just the dangling ones", "delonix system prune -a -f"),
            ("see what it WOULD take, split into declared resources and debris, and take nothing — the report to read before putting `--auto` in a timer", "delonix system prune --dry-run"),
            ("what the scheduled sweep would do right now, threshold gate included", "delonix system prune --dry-run --auto"),
        ],
        see_also: &["system df", "image rm", "container rm", "volumes ls"],
    },
    Entry {
        path: "system thermal",
        group: "Maintenance",
        examples: &[
            ("keep the engine's CPU budget in check when the machine heats up", "delonix system thermal"),
            ("one reading and out, for cron", "delonix system thermal --once"),
            ("cool down earlier, and never drop below half the CPU", "delonix system thermal --high 75 --floor 50"),
        ],
        see_also: &["system monitor", "container update", "system info"],
    },
    Entry {
        path: "system monitor",
        group: "Dashboards",
        examples: &[
            ("who talks to whom, per container, live from conntrack", "delonix system monitor"),
            ("one sample and out — the shape for a script or a cron line", "delonix system monitor --no-stream"),
            ("a slower refresh on a busy host", "delonix system monitor --interval 3000"),
        ],
        see_also: &["dash", "network ls", "system events", "container stats"],
    },
    Entry {
        path: "version",
        group: "",
        examples: &[
            ("which build this is, and the commit it came from", "delonix version"),
        ],
        see_also: &["system info", "dash", "completion"],
    },
    Entry {
        path: "vm",
        group: "",
        examples: &[
            ("the official golden image, ready to boot without installing anything", "delonix vm pull"),
            ("a VM from it, waiting until it has a real IP", "delonix vm create dev --vcpus 2 --memory 4G --wait"),
            ("what is up, and the address each one got", "delonix vm ls"),
            ("the boot log and a login, with no IP and no SSH yet", "delonix vm console dev"),
        ],
        see_also: &["image vm ls", "cluster kubeadm", "workload ls", "container run"],
    },
    Entry {
        path: "vm create",
        group: "Lifecycle",
        examples: &[
            ("from the local golden image, blocking until it answers on an address", "delonix vm create dev --vcpus 2 --memory 4G --wait"),
            ("with your key and hostname applied on the first boot, by cloud-init", "delonix vm create dev --ssh-key @$HOME/.ssh/id_ed25519.pub --hostname dev --wait"),
            ("a Kubernetes node on libvirt, on a NAT network so the IP is visible and routable", "delonix vm create k8s-cp1 --disk delonix-vm-k8s:1.34 --backend libvirt --net-mode nat --vcpus 2 --memory 4G"),
            ("isolated from other namespaces — only cloud-hypervisor puts the tap in the SDN where that is enforced", "delonix vm create nas --backend cloud-hypervisor --namespace teamA"),
        ],
        see_also: &["vm ls", "vm console", "vm start", "image vm pull"],
    },
    Entry {
        path: "vm restart",
        group: "Lifecycle",
        examples: &[
            ("a real reboot — `start` on an already-running VM does nothing, this one always cycles it", "delonix vm restart dev"),
        ],
        see_also: &["vm start", "vm stop", "vm console", "vm status"],
    },
    Entry {
        path: "vm rm",
        group: "Lifecycle",
        examples: &[
            ("stop it and delete the overlay and the record", "delonix vm rm dev"),
            ("drop the local state even when the libvirt cleanup fails, instead of leaving a record you cannot get rid of", "delonix vm rm dev --force"),
        ],
        see_also: &["vm stop", "vm ls", "vm create", "vm snapshot ls", "vm prune"],
    },
    Entry {
        path: "vm prune",
        group: "Maintenance",
        examples: &[
            ("reclaim the VM state directory: stale create locks, sockets and overlays of VMs that no longer exist — declared VMs and the disks their records point to are left alone", "delonix vm prune"),
            ("in CI, where there is no terminal to confirm at", "delonix vm prune -f"),
            ("ALSO destroy every VM that is not running, disks included — the `container prune` behaviour, opt-in because a stopped VM is a machine at rest, not a corpse", "delonix vm prune --stopped"),
        ],
        see_also: &["vm rm", "vm ls", "cluster prune", "system prune"],
    },
    Entry {
        path: "vm start",
        group: "Lifecycle",
        examples: &[
            ("boot a stopped VM again with the disk, vcpus, memory and backend of its last create — the overlay is reused, so its disk state survives", "delonix vm start dev"),
        ],
        see_also: &["vm stop", "vm restart", "vm create", "vm status"],
    },
    Entry {
        path: "vm stop",
        group: "Lifecycle",
        examples: &[
            ("give the host back its CPU and RAM, keeping the disk, the record and the snapshots — the libvirt domain is undefined here, so its snapshot metadata is preserved on our side and given back on the next start", "delonix vm stop dev"),
        ],
        see_also: &["vm start", "vm snapshot", "vm rm", "vm ls"],
    },
    Entry {
        path: "vm describe",
        group: "Inspect",
        examples: &[
            ("everything the record holds about a VM, plus the live state, kubectl style", "delonix vm describe dev"),
            ("several at once, to compare how they were created", "delonix vm describe dev k8s-cp1"),
        ],
        see_also: &["vm status", "vm ls", "vm snapshot ls"],
    },
    Entry {
        path: "vm ls",
        group: "Inspect",
        examples: &[
            ("every VM, with vcpus, memory, state and the IP it got", "delonix vm ls"),
            ("also knock on 22/6443/10250/80/443 to see what already answers — live network I/O, so it is off by default", "delonix vm ls --ports"),
            ("the same rows as JSON, for a script", "delonix vm ls -o json"),
            ("only one isolation namespace — and the NAMESPACE column stays, which it does not when every row would say `default`", "delonix vm ls --namespace teamA"),
        ],
        see_also: &["vm status", "vm describe", "vm dash", "vm prune", "image vm ls"],
    },
    Entry {
        path: "vm status",
        group: "Inspect",
        examples: &[
            ("one VM, with liveness and IP reconciled against the backend rather than read off a stale record", "delonix vm status dev"),
            ("the state of all of them at once", "delonix vm status"),
        ],
        see_also: &["vm ls", "vm describe", "vm console"],
    },
    Entry {
        path: "vm console",
        group: "Interact",
        examples: &[
            ("watch the boot and log in before there is any IP or SSH — Ctrl-] brings you back to the host", "delonix vm console dev"),
        ],
        see_also: &["vm vnc", "vm status", "vm create", "vm restart"],
    },
    Entry {
        path: "vm ssh",
        group: "Interact",
        examples: &[
            ("a shell inside the VM — the IP comes from the record, so the name is enough", "delonix vm ssh dev"),
            ("one command and out, instead of an interactive shell", "delonix vm ssh dev -- systemctl status kubelet"),
            ("straight to an address, as another user", "delonix vm ssh 192.168.122.50 -l root"),
        ],
        see_also: &["vm console", "vm ls", "vm create"],
    },
    Entry {
        path: "vm vnc",
        group: "Interact",
        examples: &[
            ("the address to point a VNC client at, for a VM created with `--vnc` on libvirt", "delonix vm vnc dev"),
        ],
        see_also: &["vm console", "vm create", "vm status"],
    },
    Entry {
        path: "vm default-backend",
        group: "Configure",
        examples: &[
            ("which backend `vm create` will pick when nothing on the command line says otherwise", "delonix vm default-backend"),
            ("pin libvirt once, instead of passing `--backend` on every create — it is the backend with snapshots and a NAT address", "delonix vm default-backend --set libvirt"),
            ("hand the choice back to auto-detection", "delonix vm default-backend --clear"),
        ],
        see_also: &["vm create", "vm snapshot", "vm ls"],
    },
    Entry {
        path: "vm bridge",
        group: "Networking",
        examples: &[
            ("read the privileged plan before running any of it — without `--apply` this only prints", "delonix vm bridge app"),
            ("establish it as root: the VM then reaches container IPs on that network directly, no published port needed", "sudo delonix vm bridge app --apply"),
            ("route back a VM subnet the `virbr*` auto-detection did not find", "sudo delonix vm bridge app --apply --vm-subnet 192.168.122.0/24"),
        ],
        see_also: &["vm unbridge", "vm reach", "network create", "vm create"],
    },
    Entry {
        path: "vm reach",
        group: "Networking",
        examples: &[
            ("which published container ports a VM can actually get to, and the exact republish command for each one that is stuck on loopback", "delonix vm reach"),
        ],
        see_also: &["vm bridge", "container port", "net ingress publish", "vm status"],
    },
    Entry {
        path: "vm unbridge",
        group: "Networking",
        examples: &[
            ("what the teardown would remove, without removing it", "delonix vm unbridge app"),
            ("actually tear the veth and the routes down, as root", "sudo delonix vm unbridge app --apply"),
        ],
        see_also: &["vm bridge", "vm reach", "network ls"],
    },
    Entry {
        path: "vm build",
        group: "Storage",
        examples: &[
            ("build a bootable qcow2 from the `VMfile` in this directory", "delonix vm build -t lab:1.0"),
            ("let the guest reach a package mirror during `RUN` — offline is the default so the same recipe gives the same image tomorrow", "delonix vm build -t lab:1.0 --network"),
            ("another recipe and another build context", "delonix vm build -t lab:1.0 -f VMfile.dev ./image"),
        ],
        see_also: &["vm init", "image vm ls", "vm create", "vm push"],
    },
    Entry {
        path: "vm convert",
        group: "Storage",
        examples: &[
            ("hand an image built here to VMware, flattened so there is no backing chain to carry with it", "delonix vm convert lab:1.0 --to vmdk"),
            ("raw for Proxmox VE's LVM/ZFS/Ceph storage, written where you want it", "delonix vm convert lab:1.0 --to raw -o /srv/lab.raw"),
            ("back to qcow2 and compressed — only qcow2 and vmdk can, the rest are refused up front", "delonix vm convert /srv/lab.raw --to qcow2 --compress"),
            ("Hyper-V and Azure take vhdx; the older vhd is a different format, not another spelling", "delonix vm convert lab:1.0 --to vhdx"),
        ],
        see_also: &["vm build", "image vm ls", "vm push", "vm create"],
    },
    Entry {
        path: "vm ls-remote",
        group: "Storage",
        examples: &[
            ("which Kubernetes versions are published, before spending a pull on one", "delonix vm ls-remote"),
            ("the tags of the Kubernetes-free golden", "delonix vm ls-remote --no-k8s"),
            ("any other OCI repository holding VM images", "delonix vm ls-remote ghcr.io/angolardevops/delonix-vm-base"),
        ],
        see_also: &["vm pull", "image vm ls", "vm push"],
    },
    Entry {
        path: "vm pull",
        group: "Storage",
        examples: &[
            ("the official Kubernetes golden image, with kubeadm and the CRI already installed", "delonix vm pull"),
            ("the Kubernetes-free golden instead — just the engine, rootless-ready", "delonix vm pull --no-k8s"),
            ("one published tag in particular, under a local name of your own", "delonix vm pull ghcr.io/angolardevops/delonix-vm-k8s:1.35 --name k8s-1.35"),
        ],
        see_also: &["vm ls-remote", "image vm ls", "vm create", "cluster kubeadm"],
    },
    Entry {
        path: "vm push",
        group: "Storage",
        examples: &[
            ("publish an image you built to a registry of your own", "delonix vm push lab:1.0 ghcr.io/acme/lab:1.0"),
            ("omit the target and it goes to the official repository the image's own metadata names", "delonix vm push delonix-vm-k8s:1.34"),
        ],
        see_also: &["vm pull", "vm build", "vm ls-remote", "image vm ls"],
    },
    Entry {
        path: "vm apply",
        group: "Declarative",
        examples: &[
            ("apply the `kind: Vm` documents of ./delonix-manifest.yaml — idempotent by name, so it creates or recovers", "delonix vm apply"),
            ("from a manifest of your own", "delonix vm apply -f examples/vm.yaml"),
        ],
        see_also: &["vm init", "stack apply", "stack plan", "vm create"],
    },
    Entry {
        path: "vm init",
        group: "Declarative",
        examples: &[
            ("a `kind: Vm` manifest already filled in, applicable as it is", "delonix vm init"),
            ("name the project and pin the image it should boot", "delonix vm init --name lab --image delonix-vm-base:ubuntu-24.04"),
            ("a `VMfile` instead — the recipe for building your own qcow2, not for running an existing one", "delonix vm init --vmfile"),
            ("generate into a directory of its own, overwriting what is there", "delonix vm init ./lab --name lab --force"),
        ],
        see_also: &["vm apply", "vm build", "vm create", "stack init"],
    },
    Entry {
        path: "vm snapshot",
        group: "Maintenance",
        examples: &[
            ("a checkpoint before an upgrade — of a RUNNING VM it takes the memory too", "delonix vm snapshot create dev before-upgrade"),
            ("which checkpoints a VM has, before reverting to one", "delonix vm snapshot ls dev"),
        ],
        see_also: &["vm stop", "vm status", "volumes snapshot", "vm default-backend"],
    },
    Entry {
        path: "vm snapshot create",
        group: "Lifecycle",
        examples: &[
            ("of a RUNNING libvirt VM: memory and disk, so a restore puts it back mid-flight", "delonix vm snapshot create dev before-upgrade"),
            ("of a stopped one: disk-only, and the VM stays stopped", "delonix vm snapshot create dev cold-copy"),
        ],
        see_also: &["vm snapshot ls", "vm snapshot restore", "vm snapshot rm", "vm stop"],
    },
    Entry {
        path: "vm snapshot ls",
        group: "Inspect",
        examples: &[
            ("which checkpoints a VM has, before reverting to one — also with the VM stopped", "delonix vm snapshot ls dev"),
        ],
        see_also: &["vm snapshot create", "vm snapshot restore", "vm describe"],
    },
    Entry {
        path: "vm snapshot rm",
        group: "Lifecycle",
        examples: &[
            ("drop a checkpoint you no longer need — its state leaves the disk with it", "delonix vm snapshot rm dev before-upgrade"),
        ],
        see_also: &["vm snapshot ls", "vm snapshot create", "vm rm"],
    },
    Entry {
        path: "vm snapshot restore",
        group: "Lifecycle",
        examples: &[
            ("put the VM back exactly as it was when the checkpoint was taken", "delonix vm snapshot restore dev before-upgrade"),
        ],
        see_also: &["vm snapshot ls", "vm snapshot create", "vm status"],
    },
    Entry {
        path: "vm dash",
        group: "Dashboards",
        examples: &[
            ("a live TUI of every VM — state, resources and problems, refreshed in place", "delonix vm dash"),
            ("one snapshot of text, for a terminal that is not interactive", "delonix vm dash --once"),
            ("the same numbers as JSON, for a script or a dashboard datasource", "delonix vm dash --json"),
        ],
        see_also: &["vm ls", "vm status", "dash", "container dash"],
    },
    Entry {
        path: "volumes",
        group: "",
        examples: &[
            ("a named volume, so the data outlives the container that writes to it", "delonix volumes create pgdata"),
            ("what exists, with driver and mountpoint", "delonix volumes ls"),
            ("how much disk one of them really holds, measured from inside the userns", "delonix volumes inspect pgdata"),
        ],
        see_also: &["container run", "storage", "sharevolume", "volumes snapshot"],
    },
    Entry {
        path: "volumes create",
        group: "Lifecycle",
        examples: &[
            ("a local named volume — the default driver", "delonix volumes create pgdata"),
            ("with a size cap, so one workload cannot fill the whole disk", "delonix volumes create pgdata --quota 20g"),
            ("an NFS export straight as a volume, without the friendlier `storage` declaration", "delonix volumes create backups --driver nfs --device 10.0.0.5:/mnt/pool/backups --options vers=4.1,soft"),
        ],
        see_also: &["volumes ls", "volumes inspect", "storage create", "container run"],
    },
    Entry {
        path: "volumes rm",
        group: "Lifecycle",
        examples: &[
            ("remove a volume nothing references any more", "delonix volumes rm pgdata"),
            ("force it past a live reference — this DESTROYS the data of whatever still uses it", "delonix volumes rm -f pgdata"),
        ],
        see_also: &["volumes ls", "volumes snapshot create", "container rm", "sharevolume rm"],
    },
    Entry {
        path: "volumes prune",
        group: "Maintenance",
        examples: &[
            ("DESTROY every local volume nothing references — the data does not come back", "delonix volumes prune"),
            ("in CI, where there is no terminal to confirm at", "delonix volumes prune -f"),
            ("reclaim the disk of a tenant that no longer exists — its volumes live under its own namespace, where an unscoped prune never looks", "delonix volumes prune --namespace acme -f"),
            ("the whole store: the unscoped root AND every tenant", "delonix volumes prune -A -f"),
        ],
        see_also: &["volumes rm", "volumes ls", "volumes snapshot create", "system prune"],
    },
    Entry {
        path: "volumes describe",
        group: "Inspect",
        examples: &[
            ("a kubectl-style block, meant to be read rather than parsed", "delonix volumes describe pgdata"),
            ("several at once, to compare them side by side", "delonix volumes describe pgdata backups"),
        ],
        see_also: &["volumes inspect", "volumes ls", "sharevolume describe"],
    },
    Entry {
        path: "volumes inspect",
        group: "Inspect",
        examples: &[
            ("driver, mountpoint, quota and the REAL usage — an unreadable directory is reported as unknown, never as zero", "delonix volumes inspect pgdata"),
        ],
        see_also: &["volumes describe", "volumes ls", "sharevolume describe"],
    },
    Entry {
        path: "volumes ls",
        group: "Inspect",
        examples: &[
            ("every volume, with driver and mountpoint", "delonix volumes ls"),
            ("as JSON, for a backup job or a monitoring script", "delonix volumes ls -o json"),
        ],
        see_also: &["volumes inspect", "volumes describe", "storage ls", "system df"],
    },
    Entry {
        path: "volumes apply",
        group: "Declarative",
        examples: &[
            ("apply only the `kind: Volume` documents of a manifest, idempotent by name", "delonix volumes apply -f delonix-manifest.yaml"),
            ("the volumes of a shipped example, leaving the other kinds untouched", "delonix volumes apply -f examples/volume.yaml"),
        ],
        see_also: &["stack apply", "stack plan", "volumes create", "storage apply"],
    },
    Entry {
        path: "volumes snapshot",
        group: "Maintenance",
        examples: &[
            ("freeze the current contents of a volume as a tar.gz, safe in rootless", "delonix volumes snapshot create pgdata"),
            ("what has been kept, and when", "delonix volumes snapshot ls pgdata"),
        ],
        see_also: &["volumes inspect", "volumes rm", "vm snapshot"],
    },
    Entry {
        path: "volumes snapshot create",
        group: "Lifecycle",
        examples: &[
            ("a point-in-time copy, named with the UTC timestamp", "delonix volumes snapshot create pgdata"),
            ("a name of your own, so the restore reads clearly months later", "delonix volumes snapshot create pgdata --name before-upgrade"),
        ],
        see_also: &["volumes snapshot ls", "volumes snapshot restore", "volumes inspect"],
    },
    Entry {
        path: "volumes snapshot rm",
        group: "Lifecycle",
        examples: &[
            ("drop a snapshot you no longer keep, freeing the space it holds under the volume", "delonix volumes snapshot rm pgdata before-upgrade"),
        ],
        see_also: &["volumes snapshot ls", "volumes rm", "system df"],
    },
    Entry {
        path: "volumes snapshot ls",
        group: "Inspect",
        examples: &[
            ("the snapshots of one volume", "delonix volumes snapshot ls pgdata"),
            ("of every volume — omit the name", "delonix volumes snapshot ls"),
        ],
        see_also: &["volumes snapshot create", "volumes snapshot restore", "volumes snapshot rm"],
    },
    Entry {
        path: "volumes snapshot restore",
        group: "Maintenance",
        examples: &[
            ("put the data back as it was — it REPLACES the contents, so stop the consumers first", "delonix volumes snapshot restore pgdata before-upgrade"),
        ],
        see_also: &["volumes snapshot ls", "volumes snapshot create", "container stop"],
    },
    Entry {
        path: "workload",
        group: "",
        examples: &[
            ("containers AND VMs in one table, so you stop guessing which group owns what", "delonix workload ls"),
            ("stop something by name — routed to whichever backend owns it", "delonix workload stop web"),
        ],
        see_also: &["workload ls", "container ps", "vm ls", "stack apply"],
    },
    Entry {
        path: "workload rm",
        group: "Lifecycle",
        examples: &[
            ("remove a workload by name, whichever backend holds it", "delonix workload rm web"),
            ("force it even if it is still running or the backend cleanup refuses", "delonix workload rm -f web"),
        ],
        see_also: &["workload stop", "workload ls", "container rm"],
    },
    Entry {
        path: "workload stop",
        group: "Lifecycle",
        examples: &[
            ("stop by exact name, container or VM — a name owned by both is refused, never guessed", "delonix workload stop web"),
        ],
        see_also: &["workload rm", "workload ls", "container stop"],
    },
    Entry {
        path: "workload describe",
        group: "Inspect",
        examples: &[
            ("the details of a workload, routed to the backend that owns the name", "delonix workload describe web"),
        ],
        see_also: &["workload ls", "container describe", "vm describe"],
    },
    Entry {
        path: "workload ls",
        group: "Inspect",
        examples: &[
            ("every workload of the node — containers and VMs — in a single table", "delonix workload ls"),
            ("as JSON, with stable field names, for automation", "delonix workload ls -o json"),
        ],
        see_also: &["workload describe", "container ps", "vm ls"],
    },
];
