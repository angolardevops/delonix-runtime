# Matriz de rastreabilidade — Campanha dlxcamp-20261024

**Artefacto sob teste**
- Repo: `/home/walter/workspaces/NgolaCloud/projects/delonix-runtime`
- Estado testado começou em `fix/stabilize-develop-after-merges` @ `257a0ee4` (diff `c062d290`); o dono fez checkout para `develop` @ **`558e5f20`** a meio — binário final recompilado limpo de develop: digest **`350a6320…`**, `--version` = `558e5f201 (+258)`, built 2026-10-24.
- Delta binário inicial↔develop = só `pt.po` (i18n) + `image.rs` em `mod tests` (não entra no release) → resultados pré-checkout válidos.
- Host: Zorin OS 18.1, kernel 7.0.0-38, x86_64, 20 vCPU, 62 Gi RAM; rootless uid 1000; cgroup2 delegado: cpu/memory/pids (NÃO cpuset/io).
- Providers: docker/podman/buildah/kind/kubectl/kubeadm/virsh/qemu/cloud-hypervisor/mkcert/cloudflared/ngrok; libvirt (opnsense, pve92).
- Disco: 5 GB no arranque → +35 GB via `cargo clean` reversível do delonix-paas → 27 GB no fim.

**Estados:** PASSOU · FALHOU · BLOQUEADO · NÃO EXECUTADO · NÃO APLICÁVEL

| ID | Requisito | Resultado esperado | Estado | Evidência |
|----|-----------|--------------------|--------|-----------|
| CLI-01 | `--version` | coerente com estado | **PASSOU** | `558e5f201 (+258)` exit 0 |
| CLI-02 | help principal + por recurso | exit 0 | **PASSOU** | help 84L + 8 recursos exit 0 |
| CLI-03 | inválidos → erro+exit≠0 | stderr útil | **PASSOU** | subcmd/flag → exit 2 |
| CLI-04 | arg obrigatório em falta | erro claro | **PASSOU** | `run` sem IMAGE → exit 2 |
| CLI-05 | equivalência verbos raiz↔agrupados | idêntico | **PASSOU** | ps≡container ls≡container ps; images≡image ls (JSON idêntico) |
| CLI-06 | output JSON | válido | **PASSOU** | `ps -o json`→`[]` exit 0 |
| CONTAINER-01 | ciclo de vida completo | observável no SO | **PASSOU** | processo+cgroup rootless; stop 10.3s (SIGTERM ignorado PID1→SIGKILL); rm sem órfão |
| CONTAINER-02 | porta + HTTP externo | HTTP 200 origem | **PASSOU** | `127.0.0.1:18090->80`; curl→`dlxcamp-20261024-OK` |
| CONTAINER-03 | bind mount R/W | dados/permissões | **PASSOU** | `:ro` bloqueia escrita; conteúdo visível |
| CONTAINER-04 | limites efectivos cpu/mem | na hierarquia | **PASSOU** | `memory.max=67108864`, `cpu.max=50000 100000` |
| CONTAINER-04b | `--cpuset` (controller não delegado) | recusa fail-closed | **PASSOU** | DX-6000 exit 69, não cria; mensagem "não implementado, não impossível" |
| CONTAINER-04c | limite de PIDs | aplicar | **FALHOU (lacuna)** | sem flag `--pids-limit` (ACHADO-1) |
| CONTAINER-05 | `--rm`, detached, exit via wait | coerente | **PASSOU** | `wait` imprime exit 3 (= docker wait) |
| CONTAINER-06 | erros negativos | diagnóstico+exit≠0 | **PASSOU** | img inexistente exit 4; conflito nome DX-5000 exit 5; conflito porta DX-1000 exit 1 |
| IMAGE-01 | pull por digest + inspect | digest estável | **PASSOU** | pull busybox (linux/amd64); inspect tags/id/size |
| IMAGE-02 | build (cache/cancel/falha parcial) | artefacto correcto | **NÃO EXECUTADO** | tempo; `go build`/Dockerfile do template gerado OK como proxy parcial |
| IMAGE-03 | push→pull round-trip por digest | recupera | **NÃO EXECUTADO** | exigia registry local; tempo |
| NETWORK-01 | topologia incl. `delonix0` | coerente | **PASSOU** | bridges SDN em netns dedicado (rootless); redes do dono preservadas |
| NETWORK-02 | DNS + egress + comunicação | resolve/conecta | **PASSOU** | nslookup+wget por nome→origem; egress 1.1.1.1 OK |
| NETWORK-03 | trocar porta a quente sem reiniciar proc | nova serve, pid igual | **PASSOU** | `+18090 -18080`: nova serve, antiga morre, **httpd pid inalterado** |
| NETWORK-04 | persistência após restart do engine | regras repostas | **NÃO EXECUTADO** | daemonless; não exercitado restart explícito |
| SCOPE-01 | isolamento entre âmbitos | bloqueio | **PASSOU** | alpha↔beta: wget timeout + ping 100% loss; `default` partilhado (ADR-0011, por design) |
| SCOPE-02 | nomes iguais em âmbitos diferentes | resolve correcto | **PASSOU** | `web` em alpha+beta coexistem; motor exige qualificação `ns/nome` |
| MANIFEST-01 | validar/plan/apply/reaplicar/drift/rm | idempotente+convergente | **PARCIAL (1 defeito)** | validate/plan/apply/idempotência/destroy OK; **DEF-002**: alterar `command` → "unchanged" (não converge nem sinaliza) |
| VM-01 | libvirt ciclo + sem órfãos | VM vive e limpa | **PASSOU** | guest vivo (ssh, cloud-init done, 2vcpu); virsh domínio real; destroy 0 órfãos; VMs do dono intactas |
| VM-02 | Cloud Hypervisor nativo ciclo | VM vive e limpa | **PASSOU** | guest vivo (hostname/nproc/mem efectivos); processo CH real; stop/start/restart/destroy sem órfãos |
| VM-03 | Proxmox create/snapshot/destroy | sem órfãos | **BLOQUEADO** | appliance `pve92` shut off; exigiria arrancar + credenciais DELONIX_PROXMOX_* |
| VM-04 | OpenStack | — | **NÃO APLICÁVEL** | não é provider do motor (recusado no load; ADR-0039 Proposed sem backend) |
| CLUSTER-01 | kind: criar/kubeconfig/DNS/Service/workload+tráfego/remover | app serve tráfego | **PASSOU** | control-plane Ready v1.34.0; nginx Deployment+Service; pod→svc por **nome DNS** e **ClusterIP** servem nginx; destroy limpo |
| CLUSTER-02 | carga de imagem local no kind | pod corre imagem local | **NÃO EXECUTADO** | tempo (CLUSTER-01 cobriu tráfego) |
| KUBEADM-01 | kubeadm offline golden+kube-vip+CNI+HA | Ready offline | **NÃO EXECUTADO** | muito pesado (VMs+egress bloqueado+HA); ver auditoria separada em memória |
| CLUSTER-03 | múltiplos clusters + kubeconfig preserva | contextos distintos | **NÃO EXECUTADO** | tempo |
| TUNNEL-01 | pinggy HTTP(S) + origem responde | endpoint público serve | **PASSOU** | `https://…free.pinggy.net`→`dlxcamp-20261024-OK`; "origin ready"; sem software no visitante |
| TUNNEL-02 | credencial inválida/reconexão/stop | tratamento correcto | **PARCIAL (1 defeito)** | **DEF-003**: kill do pai deixa `ssh -R` órfão (exposição persiste até SIGKILL) |
| TUNNEL-cf/ngrok | cloudflare/ngrok | serve | **NÃO EXECUTADO** | cloudflare edge não-fiável no sandbox (memória); ngrok sem token |
| API-01 | paridade CLI/REST/gRPC/MCP | coerente+autoriz. | **NÃO EXECUTADO** | exigia arrancar node-api; tempo |
| RECOVERY-01 | falhas controladas → retoma | estado consistente | **NÃO EXECUTADO** | kill do supervisor/retoma não exercitado explicitamente |
| RECOVERY-02 | GC não remove em uso nem dados | preserva em uso | **PASSOU** | `system prune --dry-run`: só parados/debris; vivos preservados; DECLARED≠DEBRIS; dry-run não destruiu nada do dono |
| INIT-01 | init não-interactivo/nome/destino | projecto coerente | **PASSOU** | template `go` gerado completo (git, CI, compose, Delonixfile) |
| INIT-02 | projecto gerado: build/test | app compila/testa | **PASSOU** | `go build` exit 0; `go test ./...` todos ok |
| TEMPLATE-01 | estrutura/sem placeholders/segredos/compose | válido | **PASSOU** | 0 placeholders; `.env` segredos gerados + no .gitignore; `compose config` exit 0 |

## Achados / defeitos
- **ACHADO-1 (CONTAINER-04c, P3):** `run` não expõe limite de PIDs (`--pids-limit`). Lacuna de paridade Docker (§7).
- **DEF-001 (SCOPE, P3):** `--help` de `--namespace` não menciona a excepção do `default` partilhado (ADR-0011). Doc, não comportamento.
- **DEF-002 (MANIFEST, P2):** `stack plan`/`apply` reporta "unchanged" ao alterar `command` de um Pod existente — não converge nem sinaliza (viola convergência declarativa; `--replace` resolve).
- **DEF-003 (TUNNEL, P2):** kill do processo `tunnel expose` deixa `ssh -R` órfão — exposição pública persiste até SIGKILL.
