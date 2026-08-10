# Prompt de trabalho — ciclo v0.46 (networking, VM/VMfile, docs, performance)

> **Como usar:** cola este ficheiro inteiro como prompt inicial de uma sessão no
> `delonix-runtime` (branch a partir de `main`, base actual: **v0.45.0**, commit `544060a`).
> Não é um pedido para fazer os 15 itens numa só sessão — é o *contrato* do ciclo.
> Segue a ordem dos blocos: cada bloco depende do anterior estar fechado e provado.

---

## 0. Regras não-negociáveis (aplicam-se a TODOS os itens)

Estas já estão no `CLAUDE.md`; ficam aqui porque são exactamente as que se perdem quando o
âmbito é grande:

1. **Medir antes de mudar.** Nenhuma correcção entra sem o sintoma reproduzido primeiro, com
   output real colado na resposta. Nenhuma optimização entra sem número antes/depois.
2. **Fail-closed, nunca silencioso.** Uma opção aceite e ignorada é pior que uma opção em falta.
   Se um caminho não é suportado, recusa com erro accionável (facto → comando de recuperação).
3. **Estado necessário para RECONSTRUIR o recurso tem de ser persistido**, não só usado na
   criação. Antes de dar qualquer caminho de `start`/`restart` por pronto, compara campo a campo
   o que a criação USA com o que o registo GUARDA. (Esta armadilha já custou 4 bugs: `-v`, `-p`
   em rede custom, redes extra, `Container.pod`.)
4. **`capture()` devolve `Ok` mesmo quando o comando falha** — lê sempre a SAÍDA, nunca o
   `Result`. E revê a checklist «X não é Y» do `CLAUDE.md` antes de escrever qualquer sonda nova.
5. **Sem dependências novas** nos crates de motor (`cargo tree -e normal`). Zero noção de
   tenant/licença/billing/Console — isso é `delonix-paas`. Nenhum crate privado.
6. **i18n:** string de utilizador nova = EN na fonte + entrada em `data/pt.po`. Zero PT
   hardcoded (esta regra já foi violada 380+ vezes; não repetir).
7. **Validação ao vivo é obrigatória** para tudo o que toque namespaces/cgroups/nft/holder.
   **Não respawnar o holder** sem verificar primeiro o refcount e os containers vivos deste host.
   Se um fix só puder ser provado por teste unitário, di-lo explicitamente (como o `CLAUDE.md`
   já faz para o `do_firewall`) em vez de dar por validado.
8. **Cada bug corrigido leva um teste de regressão que FALHA com a correcção revertida** —
   demonstra essa falha, não a assumas.
9. **Sem rodapé/atribuição de agente** em commits, PRs, notas de release ou docs. Autor único.
10. **Antes de escrever código para os itens marcados `[DECISÃO]`**, pára e pergunta. São
    decisões de desenho com efeito breaking; escolhê-las às pressas é pior que não as fazer.

**Entregável transversal:** um `docs/discovery/46_GAPS_ENCONTRADOS.md` (mesmo formato do
`33_GAPS_ENCONTRADOS.md`) escrito **antes** das correcções dos blocos A e B — cada achado com
comando de reprodução, output medido, severidade e o ficheiro/linha.

---

## Bloco A — Fundação de rede e namespaces (primeiro, tudo o resto depende disto)

### A1 · Ingress/egress como fonte única de verdade, sem fugas *(item 6)*

**Objectivo:** provar que não existe caminho de dados que escape à política do
`ingress`/`egress`/`namespace`/`Dependency` — e fechar o que escapar.

**Âmbito de auditoria (cada um verificado ao vivo, não deduzido):**
- IPv4 primário, IPs de `extra_networks` (multi-homing), IPv6/ULA e link-local, `tap` de VM,
  netns de pod, tráfego publicado (`-p`), o proxy L7 (`httproute`), o `vm bridge` privilegiado,
  storage de rede (NFS/CIFS) e o caminho `--net host|none`.
- Precedência real das chains (`fwguard -20`, `fwdeny -10`, `fwcont -5`, política 0) — confirma
  por `nft list` real, e restringe qualquer comparação de prioridade ao **mesmo hook**.
- Counters: toda a regra tem `counter`, e o LEITOR e o GERADOR partilham a formatação
  (`fw_rule_tail`) — se divergirem, o `ls` deixa de casar em silêncio.

**Critério de aceitação:**
- Uma matriz de alcançabilidade preenchida com medições (não «esperado»): origem × destino ×
  veredicto × regra que decidiu, para os 10 caminhos acima.
- Zero caminho onde a política diga `deny` e o pacote passe, ou diga `allow` e o pacote caia.
- Onde o motor **não pode** governar (ex.: `--net host`), o `ls` di-lo (`n/a`), nunca
  `allow (default)`.

**Não fazer:** suporte IPv6 a sério (tabela `inet` + SDN v6) — é trabalho próprio; aqui basta
que o v6 continue fechado e recusado com clareza.

### A2 · Namespace `default` implícito, em TODOS os recursos *(itens 7, 8)*

**Objectivo:** semântica k8s completa — sem `--namespace`/`metadata.namespace`, o recurso nasce
em `default`, e o isolamento comporta-se identicamente qualquer que seja o tipo de workload.

**Superfície a cobrir (cada uma com `--namespace` na CLI **e** `metadata.namespace` no
manifesto, os dois a convergirem no mesmo campo persistido):** `container`, `pod`, `vm`,
`stack`, `workload`, `network`, `volume`, `storage`, `secret`, `httproute`/`ingress`,
`dependency`, `compose` (projecto), `cluster`.

**Critério de aceitação:**
- Tabela «recurso × tem namespace? × persistido? × sobrevive a `stop`+`start`? × sobrevive a
  respawn do pin/controlo? × recusa clara onde não é aplicável (ex.: VM libvirt)».
- Um `describe` de qualquer recurso mostra a namespace efectiva, sempre — nunca vazio.
- A assimetria conhecida `default ↔ não-default` fica **documentada e testada** como decisão
  (o `default` é o namespace público), ou muda — mas não fica ambígua.
- Compatibilidade de holder: qualquer linha de controlo nova cresce em tokens só quando há
  namespace a aplicar, e contra um holder antigo falha **alto**, nunca em silêncio.

**Não fazer:** RBAC, quotas por namespace, ou qualquer noção de tenant — fronteira do PaaS.

---

## Bloco B — Dados: volumes e storage partilhado

### B1 · Volumes sem perda de dados *(item 14)*

**Objectivo:** nenhuma escrita perdida numa corrupção, num crash a meio, ou numa falha de rede
de um volume partilhado.

**Âmbito:**
- Toda a escrita de metadados: temp por-escritor + `fsync` + modo na criação + rename atómico
  (o padrão do `write_atomic_mode`). Grepa por `fs::write`/`File::create` em todos os stores e
  justifica cada um que ficar sem isto.
- **`fs::remove_dir_all` não é atómico** — nenhuma árvore com metadados cuja ausência mude o
  significado do objecto pode ser apagada por ele. A contabilidade apaga-se em ÚLTIMO lugar.
- Um directório ilegível **não é** um directório vazio (subuid mapeado): medição incompleta é
  *desconhecida*, nunca zero (`Usage { bytes, unreadable }`, `__duusage`).
- Volumes de rede (NFS/CIFS/WebDAV): o que acontece a uma escrita em curso quando o NAS
  desaparece? O container vê erro, ou pendura? Documenta e, onde possível, torna detectável.

**Critério de aceitação:** um cenário de caos novo (arnês existente) que mate o processo a meio
de uma escrita de metadados e prove que o store re-lido está íntegro — e que **falha** com a
correcção revertida.

### B2 · [DECISÃO] Storage partilhado por *context* e namespace *(item 15)*

**Objectivo:** `sharevolume`/`storage` deixam de poder colidir entre namespaces, sem introduzir
noção de tenant.

**Perguntar ANTES de código:**
1. O que é exactamente um *context* aqui — a namespace, ou um eixo novo (host/cluster/perfil)?
2. A chave passa a `<context>/<namespace>/<nome>` no caminho em disco, ou só na chave lógica do
   store com o caminho a manter-se plano?
3. É **breaking** para volumes já existentes. Migração automática no primeiro acesso, comando
   `migrate` explícito, ou corte limpo (a política que a v0.30.0 usou na CLI)?
4. Codificação livre de prefixo (o `compose_scoped_name` já resolveu esta classe de colisão) ou
   separador reservado com validação?

**Critério de aceitação:** dois namespaces com um `sharevolume` do mesmo nome coexistem, e um
`rm --purge-data` de um **não toca** no outro — provado ao vivo com dados reais dos dois lados.

---

## Bloco C — Superfície da CLI e performance

### C1 · Performance de execução, criação e pull *(item 10)*

**Objectivo:** o modelo é o `uv` — paralelismo agressivo, cache que acerta, zero trabalho
repetido. **Mede primeiro, com o agente `performance-engineer`.**

**Alvos a medir antes de tocar em código (tabela de baseline obrigatória):**
- `image pull` (registo real): concorrência de blobs hoje vs. possível, `Cas::has` a acertar,
  descompressão, ordem de layers, retomar um download interrompido.
- `container run` frio e quente: cópia FLAT do rootfs em rootless é o custo dominante deste
  host — quantifica-o antes de propor qualquer coisa (reflink/`copy_file_range`/CoW por FS).
- `vm pull`/`vm create`: overlay qcow2, custo do zstd na leitura do backing file.
- `dash`/`/metrics`: já teve incidente de 68 GiB/>1min — confirma que não regrediu.

**Critério de aceitação:** para cada alvo, número antes/depois no MESMO host e comando. Uma
melhoria sem número medido não entra. Se uma optimização exigir dependência nova, **não a
faças** — propõe e pára.

### C2 · Cluster: arranque rápido, com meta honesta *(item 13)*

**Corrigir a premissa do pedido, antes de qualquer código:** «cluster k8s no ar em alguns
milissegundos» é fisicamente impossível — o `kubeadm init` real espera etcd + apiserver +
control-plane a ficarem saudáveis, e um nó Kind arranca `systemd` + `containerd` por dentro.
A meta correcta é **reduzir o caminho crítico medido** e eliminar espera desnecessária:

- Baseline: cronometra cada etapa do `cluster kubeadm` e do `cluster create` (modo kind) — VM
  boot, espera de SSH, preparação de host, `init`, `join`, CNI — e diz qual domina.
- Ganhos plausíveis a avaliar com números: paralelizar o que é independente (já feito para
  preparação de hosts), pré-semear imagens (já existe no `--offline`), evitar re-download,
  encurtar poll/backoff excessivos, e reutilizar uma VM/nó já quente.
- **Entrega:** um `docs/` com o perfil de tempo por etapa e o ganho real obtido, e uma
  declaração honesta do piso teórico. Nunca prometer «milissegundos» na documentação.

### C3 · `delonix version` como alias de `--version` *(item 1, parte 1)*

Nota: o pedido escreveu «delonic» — o comando é **`delonix version`**. Subcomando de topo, mesma
saída byte-a-byte do `--version`, i18n incluído, sem tocar na interceção crua de `argv` em
`main()` (holder/re-exec) — essa é anterior ao `clap` e não pode ser perturbada.

### C4 · `delonix init` útil e objectivo *(item 11)*

**[DECISÃO] antes de código:** hoje existem `stack init` e `vm init --vmfile`, com scaffolds que
já produziram comentários desactualizados. Perguntar: `init` de topo é (a) um wizard que detecta
o contexto (há `Dockerfile`? `docker-compose.yml`? `VMfile`?) e gera o manifesto correspondente,
(b) só um alias organizado dos `init` já existentes, ou (c) um `init` de *projecto* (pasta com
manifesto + `.gitignore` + README)? Escolher uma; não implementar as três.

**Critério de aceitação:** o que o `init` gera **aplica-se sem edição** (`stack apply --dry-run`
limpo), e nenhum comentário do scaffold afirma limitações que já não existem.

### C5 · `scan` a funcionar como esperado *(item 12)*

**Objectivo:** o `scan` melhora de facto a segurança de imagens de container **e** de VM.

- Primeiro **caracteriza o que existe** (`cmd/scan.rs` + `delonix-scan`): que fonte de
  vulnerabilidades, que formatos, o que faz quando a base de dados não está disponível, e o que
  o `SCAN` do Dockerfile/Delonixfile realmente dispara.
- Fail-closed é obrigatório: um scan que não conseguiu correr **não é** um scan sem achados.
  Distinguir «0 vulnerabilidades» de «não medido» é o requisito central deste item.
- Imagens VM (qcow2) precisam de decisão própria — inspecção do sistema de ficheiros do
  convidado exige `libguestfs`, que esta máquina não tem. Se não puder ser provado, di-lo e
  recusa com erro accionável em vez de reportar limpo.

---

## Bloco D — Manifestos e documentação (só depois de A, B e C fecharem)

### D1 · Revisão de todos os YAML/templates *(item 9)*

**Âmbito:** todos os `kind:` (`Container`, `Pod`, `Vm`, `Workload`, `Stack`, `Network`,
`Volume`, `Storage`, `ShareVolume`, `Secret`, `Image`, `HTTPRoute`, `Ingress`, `Egress`,
`FirewallPolicy`, `Dependency`, `Cluster`), os `examples/*.yaml`, os scaffolds e o `VMfile`.

**Procurar, com achado por achado:** campos parseados e nunca usados (o `HYPERVISOR`/`VCPUS` do
VMfile já foi um caso real: parseado, testado, nunca escrito); campos redundantes entre Kinds;
blocos com semântica sobreposta; defaults que o `--dry-run` não materializa; `Serialize`
assimétrico (round-trip que perde campos); nomes que não seguem a convenção k8s; validação em
falta em campos que chegam a `format!`/argv/caminho.

**Critério de aceitação:** cada `kind` com round-trip provado (`spec_with_defaults` → parse →
igual), e uma tabela de campos «parseado × usado × persistido × reconstruído no restart».

### D2 · Republicar a documentação na nova estrutura *(itens 2, 4)*

Usa o agente **`escriba`**. Regra dele, que vale aqui: nunca documentar o que não foi confirmado
no código ou num comando executado.

- Trazer as melhorias dos blocos A–C para o site (`docs/gen.py` + páginas HTML), o `README.rst`,
  `docs/RELEASES.md` e as notas de release, **na estrutura nova já definida**.
- **Matriz de SO testados** *(item 4)*: explícita e honesta — distribuição, versão, kernel,
  cgroup v2 delegado ou não, rootless vs. root, e **o que foi de facto exercitado em cada um**.
  Um SO onde só o build foi corrido não pode aparecer como «funciona bem». Marca claramente:
  `validado E2E` / `validado parcialmente (o quê)` / `não testado`.

### D3 · Tabela comparativa Docker × Podman × Delonix *(item 3)*

**Regra do pedido, a respeitar à letra:** não decidir quem é melhor — mostrar a força de cada um,
com **dados reais e validados na prática**.

- Base de partida: `docs/COMPARACAO-DOCKER-PODMAN.md` e `docs/paridade-docker-podman.md`.
- Cada linha da tabela tem: capacidade, comportamento de cada um dos três, **como foi medido**
  (comando + versão da ferramenta + host), e a data. Sem medição → a célula diz
  `não verificado`, nunca uma afirmação.
- Incluir também onde o Delonix é **mais fraco** (ex.: `exec` interactivo na API Docker,
  `macvlan`/`ipvlan` não realizados, snapshot em Cloud Hypervisor, exit code de `-d` sem
  supervisor). Uma comparação sem isto não é honesta e perde credibilidade.

### D4 · Secção completa de VM + lab k8s/Prometheus/ELK *(item 5)*

**Objectivo:** um capítulo que leve um DevOps de zero até construir um `.qcow2` próprio com
`delonix`, terminando num lab real.

**Conteúdo mínimo:**
1. Todo o recurso de VM, ponta a ponta: `vm create` (incl. `--url-img`), ciclo de vida
   (`start`/`stop`/`restart`/`rm`, a armadilha do `undefine`/managed save), `console`,
   `snapshot`/`restore` (e o fail-closed do CH), rede (`nat`/`--ip`/`vm reach`/`vm bridge`),
   `namespace` (só CH), `image vm` (`build`/`--offline`/`push`/`pull`/`ls-remote`/`convert`),
   `default-backend`, e a golden (k8s e `--no-k8s`).
2. **`VMfile` do zero**: cada instrução, multi-stage, e a decisão `--no-network` por omissão vs.
   `--network` opt-in (com o *porquê*, não só o *como*).
3. **Lab**: uma imagem que corra k8s + Prometheus + ELK integrados e funcionais, com as portas
   expostas por um **Ingress k8s** real. Manifestos completos em `examples/`.

**Aviso de âmbito, a resolver antes de prometer:** este lab é o maior item do ciclo — a máquina
de desenvolvimento **não tem `libguestfs-tools`**, logo o `virt-customize` (coração do
`vm build`) nunca foi exercitado aqui; e ELK+Prometheus+k8s numa VM pede vários GiB de RAM.
Ou instalas o `libguestfs-tools` e o lab é validado a sério neste host, ou o capítulo é escrito
com a fronteira exacta do que foi executado marcada em cada passo. **Não publicar um lab que
não arrancou.**

---

## Bloco E — Release *(item 1, parte 2)*

Só depois de A–D. Segue a skill `delonix-release`:

- Bump de versão + `docs/releases/<tag>.md` + `docs/RELEASES.md`.
- **PR** contra `main` (conta `angolardevops` para o push — a outra dá 403).
- Publicar é **bump + tag `vX.Y.Z`**; o CI é que constrói e republica os binários (localmente
  falta `protoc`). Acompanha com o agente `sentinela` até `completed success` **e valida os
  assets publicados como um utilizador real** — `gh run watch` a sair 0 não é prova.
- Sincronizar a documentação publicada com o binário que saiu de facto.

---

## Definição de «pronto» (para cada item)

Um item só fecha com as cinco coisas:

1. Sintoma/estado inicial **medido** e colado.
2. Correcção/feature implementada, com `cargo build --workspace`, `clippy` a 0, `fmt` aplicado,
   `cargo test --workspace` limpo.
3. Teste de regressão que **falha** com a correcção revertida (demonstrado).
4. Validação ao vivo, ou a razão explícita de não ser possível neste host.
5. Documentação/i18n actualizados no mesmo passo — não «depois».

**E o mais importante:** se um item não puder ser feito como pedido (como o «milissegundos» do
C2 ou o lab do D4), **diz isso cedo, entrega tudo o resto por inteiro, e nomeia exactamente o
que ficou de fora e porquê.** Reduzir âmbito em silêncio é a única falha inaceitável aqui.
