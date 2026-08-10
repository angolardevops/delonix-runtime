# 47 — Revisão de IaC: o que falta ao Delonix para ser gerido por Terraform e Ansible

| Campo | Valor |
|---|---|
| Data | 2026-08-10 |
| Linha de base | `0.46.0` (commit `c097d0d`, ramo `ciclo-v046-bloco-a`) |
| Âmbito | **Só IaC.** Modelo declarativo, convergência, API de gestão, contrato e documentação. Não é auditoria de segurança nem de runtime. |
| Método | Leitura de código + do site de documentação. Nenhuma alteração de produção. |

> **Aviso de leitura.** Cada afirmação abaixo aponta para o ficheiro e a linha que a
> sustenta. Onde a conclusão vem de ausência (não existe rota, não existe comando),
> está dito como foi procurada. Nada é inferido da documentação — este repo já
> registou pelo menos dois casos em que a documentação afirmava o contrário do código.

---

## 0. O critério

Uma ferramenta de IaC «completa» não é um formato declarativo. São seis coisas, e o
Delonix tem duas inteiras, duas a meio e duas por começar:

| # | Capacidade | Estado |
|---|---|---|
| 1 | **Descrever** o estado desejado num ficheiro versionável | ✅ forte |
| 2 | **Prever** o que vai mudar antes de mudar (`plan`) | 🟡 parcial (`--dry-run` mostra o desejado, não a diferença) |
| 3 | **Convergir** — criar *e actualizar* até bater com o declarado | ❌ **só cria** |
| 4 | **Remover** o que saiu do ficheiro | ❌ inexistente |
| 5 | **API** com contrato, para uma ferramenta externa conduzir tudo isto | 🟡 fatia pequena, local, sem contrato publicado |
| 6 | **Documentação** que dispensa decorar | ✅ forte para a CLI, ❌ ausente para a API |

O ponto 3 é o defeito estrutural. Tudo o resto é trabalho; o 3 é a diferença entre
«um instalador declarativo» e «IaC».

---

## 1. Pontos fortes (medidos, não assumidos)

### 1.1 O modelo declarativo é largo e coerente

18 Kinds com a forma k8s (`apiVersion`/`kind`/`metadata`/`spec`), documentados em
`docs/kinds.html` e com 26 exemplos YAML em `examples/`:

```
Container · Pod · Vm · Workload · Image · Volume · Storage · ShareVolume · Network
Secret · Ingress · Egress · FirewallPolicy · HTTPRoute · Dependency · Tunnel · Cluster · Stack
```

`kind: Stack` agrega tudo num documento e expande-se no `load`
([manifest.rs:145](../../crates/delonix-runtime-bin/src/cmd/manifest.rs#L145)); `kind:
Workload` unifica container/vm/pod/microvm (ADR-0001, ADR-0006). Ambos são **açúcar
que baixa** para os Kinds base — não são um segundo motor a divergir. É a decisão
certa e está tomada.

Cobertura: `ContainerSpec` tem ~35 campos, `VmSpec` 32. Não é um subconjunto de
brinquedo do que a CLI faz.

### 1.2 `stack validate` — resolve as referências cruzadas antes de tocar em nada

[stack.rs:74](../../crates/delonix-runtime-bin/src/cmd/stack.rs#L74) resolve
`Container.network`/`.volumes`, `Vm.network`, alvos de `Ingress`/`Egress` contra o que
o manifesto declara **mais** o que já existe nos stores, e sai com erro se sobrar uma
referência por resolver. É a rede de segurança contra um `apply` fail-fast sem
rollback. Poucos projectos deste tamanho têm isto.

### 1.3 `--dry-run` materializa os defaults

`render_with_defaults`
([manifest.rs:56](../../crates/delonix-runtime-bin/src/cmd/manifest.rs#L56)) faz
round-trip de cada spec pelo struct tipado, por isso o YAML impresso mostra o que
**realmente** vai ser aplicado, com os `#[serde(default)]` visíveis, os Stacks já
expandidos e os Kinds canonicalizados. Cobre todos os Kinds excepto `Secret`
(deliberado). É o `kubectl apply --dry-run=client -o yaml`, e funciona.

### 1.4 `conditions.rs` — o recurso é proibido de mentir por omissão

[conditions.rs](../../crates/delonix-runtime-bin/src/cmd/conditions.rs) computa, a
partir do spec mais uma sonda ao ambiente, condições estilo kubectl (`ok` + `reason`
estável + mensagem accionável) para os casos em que um recurso é criado mas **não faz
o que aparenta**: `Storage` NFS em rootless que não monta, quota que é só monitorada,
`Network` macvlan que fica no registo sem plano físico, `restartPolicy` numa VM Cloud
Hypervisor que ninguém supervisiona.

**Isto é uma vantagem competitiva genuína e vale a pena dizê-lo em voz alta.** É
precisamente o que o Terraform *não* faz: um `terraform apply` diz `Creation complete`
sobre um recurso que ficou inerte. O Delonix já sabe distinguir «criado» de «criado e
a funcionar». O problema é que hoje isso só aparece no `stack describe` — não é um
campo de estado que uma ferramenta externa consiga ler.

### 1.5 `-o json` com contrato escrito (ADR-0005)

Dez comandos de listagem emitem um array JSON de linhas tipadas, com **nomes de campo
em `snake_case` independentes da língua** — explicitamente nunca os cabeçalhos de
tabela traduzidos. A ADR raciocina sobre isto pelas três lentes (Platform/DevOps/SRE) e
o compromisso está fixado: campos podem ser acrescentados, nunca removidos nem com o
tipo mudado.

### 1.6 Uma promessa de estabilidade escrita

[docs/cli-stability.md](../cli-stability.md) diz o que quebra e o que não quebra dentro
do `0.x`, e a regra de como uma quebra é feita («falhar alto, sem aliases»). Isto é
raro em 0.x e é **exactamente** o documento que quem escreve um provider precisa de
ler antes de decidir adoptar.

### 1.7 Higiene declarativa que já está feita

- `apiVersion` desconhecida **recusa** em vez de avançar
  ([manifest.rs:120](../../crates/delonix-runtime-bin/src/cmd/manifest.rs#L120)).
- `warn_unknown_fields` avisa sobre um campo mal escrito **antes** do early-continue,
  para que um typo continue a aparecer em re-applies e não só na primeira criação
  ([container.rs:1709](../../crates/delonix-runtime-bin/src/cmd/container.rs#L1709)).
- `canonical_kind` é case-insensitive por inteiro — meia-medida deixaria `kind: vm` a
  ser ignorado em silêncio.
- Idempotência sem ficheiro de estado no `cluster apply` (cada passo tem `check` e
  `apply`) — nunca dessincroniza de um `.tfstate` porque não há nenhum.

---

## 2. Pontos fracos, por severidade

### 🔴 F1 — `apply` **não actualiza**. Só cria.

```rust
// container.rs:1717
if store.list()?.iter().any(|c| &c.name == name) {
    println!("container/{name}: already exists, nothing to do");
    continue;
}
```

Muda a imagem no manifesto, muda a memória, acrescenta uma porta, corre `stack apply`:
**não acontece nada, e o comando devolve 0**. O mesmo padrão em
[volume.rs:165](../../crates/delonix-runtime-bin/src/cmd/volume.rs#L165) e
[network.rs](../../crates/delonix-runtime-bin/src/cmd/network.rs) («ensure present»,
como o próprio doc-comment do `manifest.rs` assume à cabeça).

Isto é a versão IaC do **relato desonesto** que a v0.37.0 corrigiu no CLI imperativo:
reportar sucesso sobre uma operação que não fez nada. Aqui é pior porque o utilizador
mudou o ficheiro *de propósito* — o sinal de intenção é explícito e é descartado.

Agrava-o o facto de o motor **já ter** a capacidade: `container update` reconfigura
portas, volumes, redes, memória e CPU **a quente, sem mudar o PID** — a diferença de
fundo deste motor para o Docker. O `apply` simplesmente não lhe chama. É a mesma
família de armadilha que este repo já pagou quatro vezes (`mount_live`, `set_net_rate`,
`update_limits`, `JsonStore::update`): a capacidade existe, testada, sem chamador.

**Sem isto não há provider de Terraform possível.** O ciclo do Terraform é
Create/Read/**Update**/Delete; um `Update` que não faz nada e devolve sucesso produz
drift permanente e silencioso — o pior modo de falha que uma ferramenta de IaC tem.

### 🔴 F2 — Não há forma declarativa de remover

`StackCmd` tem `Init`/`Apply`/`Ls`/`Describe`/`Validate`
([stack.rs:17](../../crates/delonix-runtime-bin/src/cmd/stack.rs#L17)). Não há
`destroy`, não há `prune`, não há `--prune`. Um recurso retirado do manifesto fica vivo
para sempre e nada o assinala — nem o `describe`, que só olha para o que o ficheiro
declara.

Consequência prática: o manifesto deixa de ser a fonte de verdade no momento em que
alguém apaga um bloco. E o teardown de um ambiente é hoje um script de `rm` à mão, na
ordem inversa, escrito pelo utilizador — exactamente o que o IaC existe para eliminar.

### 🔴 F3 — Não há detecção de drift, nem `plan`

`stack describe` responde **presença** (existe / não existe), não **conformidade**
(o que está lá é o que foi declarado?). O doc-comment é honesto sobre isso: «a coluna
que interessa é PRESENÇA». `--dry-run` imprime o estado desejado, não a diferença
contra o real.

Falta a operação central do IaC: `delonix stack plan` → «container `web`: imagem
`nginx:1.24` → `nginx:1.27`, memória `256M` → `512M`; volume `dados`: sem alteração;
rede `interna`: **em falta**». Sem isto não há revisão em PR, não há aprovação antes de
aplicar, não há CI que falhe por drift.

### 🟠 F4 — A API de gestão é uma fatia pequena do motor

Rotas existentes ([lib.rs:186-229](../../crates/delonix-mgmt/src/lib.rs#L186)):

| Recurso | Create | Read (get) | List | Update | Delete |
|---|---|---|---|---|---|
| Volume | ✅ | ✅ | ✅ | ❌ | ✅ |
| Container | ✅ | ✅ | ✅ | 🟡 só `publish-add/rm` | ✅ |
| Image | 🟡 pull/build | ❌ | ✅ | ❌ | ✅ |
| Network | ✅ | ❌ | ❌ | ❌ | ✅ |
| VM | ❌ | ❌ | ❌ | ❌ | 🟡 só `action` |
| Pod, Secret, Storage, ShareVolume, HTTPRoute, Ingress, Egress, FirewallPolicy, Dependency, Workload, Cluster | ❌ | ❌ | ❌ | ❌ | ❌ |

**Nenhuma rota aplica um manifesto.** A API é imperativa (POST `/containers` reconstrói
uma linha de comando) enquanto o modelo declarativo vive só na CLI — são duas
superfícies com poderes diferentes.

E a fidelidade da que existe é baixa: `RunSpecBody` tem **11 campos** (`image`, `name`,
`ports`, `env`, `network`, `memory`, `restart`, `command`, `volumes`, `knows`,
`knows_none`) contra **71 flags** de `container run` e ~35 campos de `ContainerSpec`.
Sem labels, sem CPU, sem user/workdir/entrypoint, sem secrets, sem healthcheck, sem
namespace, sem capabilities, sem devices/GPU, sem tmpfs/sysctl/ulimits.

Um provider de Terraform escrito sobre esta API exprimiria **menos** do que o YAML já
exprime hoje. A falta de `Read` para redes e VMs é bloqueante por si só: o Terraform
faz `Read` a cada refresh de *todos* os recursos, e sem ele não há `import` nem
detecção de alteração fora-de-banda.

### 🟠 F5 — A API não é remota, nem autenticada, nem tem contrato publicado

- **Transporte**: só socket unix, sem opção de TCP/TLS
  ([serve.rs:26](../../crates/delonix-runtime-bin/src/cmd/serve.rs#L26)).
- **Autenticação**: `SO_PEERCRED` e o par tem de ser o **mesmo euid** do servidor. Não
  há token, mTLS, nem qualquer noção de identidade. Isto está **certo** para o modelo
  actual (é a superfície de maior privilégio do runtime — `/exec` é execução arbitrária
  dentro de qualquer container) e a decisão está bem argumentada no código.
- **Contrato**: zero OpenAPI, zero JSON-Schema (procurado por `openapi|schemars|
  JsonSchema` em todo o repo — nenhuma ocorrência). Zero documentação de rotas no site:
  o único vestígio no `README.rst` é uma linha numa tabela de crates.
- **Estabilidade**: `docs/cli-stability.md` classifica `serve api` explicitamente como
  **NÃO estável** — «pode mudar em qualquer versão».

Somando: Terraform e Ansible correm tipicamente **de fora** do nó gerido. Hoje o único
caminho é `ssh nó -- delonix ...`, e a API não ajuda. O objectivo «API pronta para ser
gerida por Terraform» exige decidir onde vive o plano remoto — e essa decisão colide
com a fronteira do PaaS (multi-tenant/authz é do `delonix-paas`). **É uma decisão de
arquitectura, não uma tarefa; merece um ADR antes de qualquer código.**

### 🟠 F6 — A saída dos `apply` não é máquina-legível, e é traduzida a meio

```
volume.rs:179     println!("volume/{name}: {}", po::t("ensured"));       ← traduzido
network.rs:203    println!("network/{name}: {}", po::t("created"));      ← traduzido
container.rs:1718 println!("container/{name}: already exists, ...");     ← EN cru
```

Duas consequências:

1. **Inconsistência de i18n** — parte da saída de `apply` muda com `--l18n=pt` e parte
   não. É a mesma classe do bug que a v0.32.2 corrigiu em 380 strings, sobrevivente
   neste caminho.
2. **Não há sinal `changed`/`unchanged` estruturado.** O `-o json` da ADR-0005 cobre
   listagens; **as mutações ficaram de fora**. Um módulo Ansible vive de saber se
   mudou alguma coisa (`changed_when`) — hoje teria de fazer `grep` a uma string que
   muda com o idioma do operador.

### 🟡 F7 — `--dry-run` só existe no `stack apply`

Nenhum comando imperativo tem check-mode. O `--check` do Ansible não teria para onde
mapear em `container run`, `network create`, `volumes create`, `vm create`.

### 🟡 F8 — Identidade e leitura não são uniformes entre Kinds

O `import` do Terraform exige, para cada tipo, «dá-me este recurso por id, em JSON».
Hoje: containers têm id **e** nome; volumes/redes/secrets/storage são por nome; VMs por
nome; membros de pod derivam de labels; stacks e clusters **não têm registo próprio** —
derivam do ficheiro e das labels. E a leitura tem três formas (`inspect` JSON,
`describe` texto, `ls -o json` resumo) que não coincidem.

Não é errado — a decisão de não inventar registos que dessincronizam está bem tomada e
é defensável. Mas significa que um provider tem de tratar `stack` e `cluster` como
recursos sem identidade estável, e isso tem de ser dito à partida.

### 🟡 F9 — Sem versionamento de recurso nem concorrência optimista

Não há `resourceVersion`/`generation`/ETag. O `flock` do `Store` protege o **ficheiro**
contra escritas concorrentes; não protege a **intenção**. Dois `apply` em paralelo — ou
um Terraform e um operador com a CLI — sobrepõem-se sem detecção. Para um único
operador é irrelevante; para uma pipeline de CI partilhada, não é.

### 🟡 F10 — Ordem por Kind, não por grafo de dependências

`stack apply` corre numa ordem **fixa de 13 Kinds**
([stack.rs:126](../../crates/delonix-runtime-bin/src/cmd/stack.rs#L126)). Dentro de um
Kind, a ordem é a do ficheiro. Não existe `dependsOn` por recurso nem porta de
prontidão entre recursos — e `spec.detach` tem default `true`, por isso o `apply` não
espera que nada fique saudável antes de seguir.

Comparar com o `compose`, que **já tem** ordenação topológica com as três condições de
`depends_on` e espera pelo healthcheck real. A capacidade existe no repo; o caminho
declarativo nativo não a tem. (`kind: Dependency` é alcançabilidade de rede, não
ordenação — nomes parecidos, coisas diferentes.)

### 🟡 F11 — O schema dos manifestos está declarado **não estável**

`cli-stability.md`: «o schema dos manifestos (`kind: *`) — campos são aditivos na
prática, mas não é uma promessa». Para IaC isto é exactamente ao contrário do que é
preciso: o schema **é** o artefacto que os utilizadores põem em git e revêem em PR. A
CLI está mais protegida que o formato declarativo, quando devia ser o inverso.

### 🟡 F12 — Não há `explain`, e o schema é documentado à mão

Não existe `delonix explain container.spec.ports`. O schema dos 18 Kinds vive em dois
sítios independentes — nos structs Rust e em `docs/kinds.html` escrito à mão — sem
geração de um a partir do outro. Este repo já registou o custo dessa divergência
(páginas que descreviam `serve docker-api` como só-leitura, `cluster kubeadm` sem HA,
`network` sem overlay). É a mesma armadilha, num sítio onde o utilizador não tem como
verificar.

---

## 3. O que falta especificamente para o Terraform

Ordem obrigatória — cada linha depende da anterior:

| # | Requisito do provider | Estado | Bloqueado por |
|---|---|---|---|
| 1 | Read de **todos** os tipos, por id, em JSON | 🟡 parcial | F4, F8 |
| 2 | **Update** (in-place ou replace declarado) | ❌ | **F1** |
| 3 | Delete por id | 🟡 CLI sim, API parcial | F2, F4 |
| 4 | Diferença calculável antes de aplicar (`plan`) | ❌ | F3 |
| 5 | Contrato de API versionado e estável | ❌ | F5 |
| 6 | Acesso remoto autenticado | ❌ | F5 |
| 7 | `import` de recursos pré-existentes | ❌ | F8 |
| 8 | Concorrência segura (lock/versão) | ❌ | F9 |

**O atalho pragmático, e é uma boa opção:** um provider que não fale HTTP nenhum e
execute a CLI local por SSH — o mesmo modelo do provider `null`/`external` ou do
`terraform-provider-shell`. Precisa apenas de (1), (2) e (4), ou seja: **`-o json` nas
mutações, `apply` que actualiza, e `stack plan`**. Nada de API remota, nada de
autenticação, nada que colida com a fronteira do PaaS. Fica funcional muito antes de a
decisão de arquitectura do F5 estar tomada.

## 4. O que falta especificamente para o Ansible

O Ansible é **mais fácil** do que o Terraform aqui, porque já corre por SSH e não guarda
estado. Faltam três coisas, e só três:

1. **Sinal `changed` estruturado.** Cada mutação devia emitir, com `-o json`,
   `{"kind","name","action":"created|updated|unchanged|deleted","changed":true|false}`.
   É o coração de um módulo Ansible e hoje não existe (F6).
2. **Check-mode.** `--dry-run` nos comandos imperativos, não só no `stack apply` (F7).
3. **Convergência.** Um módulo Ansible é idempotente por definição: correr duas vezes
   dá o mesmo resultado, e correr depois de uma alteração **converge**. Hoje converge
   para «já existe, nada a fazer» (F1).

Com estas três, uma colecção `angolardevops.delonix` com módulos `delonix_container`,
`delonix_network`, `delonix_volume`, `delonix_stack` escreve-se em dias, não em meses —
cada módulo é um wrapper fino sobre a CLI. Sem elas, cada módulo teria de fazer parsing
de texto traduzível, que é o anti-padrão que o `-o json` da ADR-0005 existe para matar.

---

## 5. Documentação

### Forte

- **26 páginas de comandos** + `kinds.html` (1171 linhas, os 18 Kinds) + `cheatsheet` +
  `comparacao` + `c4`/`arquitectura` + `cloud`/`labs`, geradas por `docs/gen.py`
  (4670 linhas), **bilingues EN/PT** com alternador.
- **26 exemplos YAML** em `examples/`, um por Kind, executáveis.
- **6 ADRs** com alternativas consideradas e consequências — o rasto de *porquê*, que é
  o que falta a 90% dos projectos e é o que faz alguém confiar no formato.
- `docs/cli-stability.md` — o contrato.
- `stack init --template` gera um projecto **já preenchido**, o que é a melhor
  documentação que existe (funciona sem ler nada).

### Fraco — e é aqui que o objectivo «não decorar nada» falha

1. **A API de gestão não está documentada em lado nenhum.** Nem uma página, nem uma
   lista de rotas, nem um exemplo de `curl`. Quem quiser automatizar via API tem de ler
   `crates/delonix-mgmt/src/lib.rs`. Isto é o oposto do objectivo declarado.
2. **Não há referência de campos gerada a partir do código.** `kinds.html` é escrito à
   mão; os structs são a verdade. Enquanto forem duas fontes, divergem — e o repo já
   tem historial disso. O remédio é geração (um `delonix explain`, ou um JSON-Schema
   emitido pelo binário e consumido pelo `gen.py`), não mais revisões manuais.
3. **Falta a página que responde «como é que eu opero isto a sério».** Existem páginas
   por comando; falta o guia transversal: GitOps com o Delonix, o que meter em CI, como
   fazer rollback quando o `apply` falha a meio (hoje a resposta é «não há — corre
   `stack describe` e limpa à mão», e isso devia estar escrito).
4. **Não há changelog de schema.** `RELEASES.md` é grande e detalhado, mas quem versiona
   manifestos precisa de «o que mudou nos Kinds na v0.46» separado do resto.
5. **Sem catálogo de erros.** As mensagens são boas e accionáveis (mérito real), mas não
   há uma página onde se procure o texto de um erro. Numa CLI com ~208 subcomandos, é
   isso que evita ficar preso.

---

## 6. Roteiro proposto, por ordem de valor

Cada bloco é entregável sozinho e desbloqueia os seguintes.

**Bloco 1 — Convergência.** `apply` passa a diferenciar spec-declarado de estado-real e
a chamar o `container update` que já existe. Onde não for possível a quente, recriar
**só com opt-in explícito** (`--replace`), nunca em silêncio. Fecha F1. É a peça sem a
qual nada mais conta.

**Bloco 2 — `stack plan` + `-o json` nas mutações.** O `plan` cai quase de graça do
Bloco 1 (o diff já tem de ser calculado para converger — só falta imprimi-lo em vez de
o aplicar). O `-o json` estende a ADR-0005 às mutações com o campo `changed`. Fecha F3
e F6. **Com os blocos 1 e 2 o Ansible torna-se viável.**

**Bloco 3 — `stack destroy` + `apply --prune`.** Fecha F2. Exige decidir o que é
«pertence a este stack» sem inventar um registo — a resposta provável é a mesma que
`compose` e `cluster` já usam: uma label determinística.

**Bloco 4 — ADR da API remota.** Não escrever código antes. Decidir: transporte, modelo
de identidade, e onde fica a fronteira com o PaaS. Só depois expandir rotas, publicar
OpenAPI e promover `serve api` a estável em `cli-stability.md`. Fecha F5, e só então F4
vale o esforço.

**Bloco 5 — `delonix explain` gerado do código**, com o `gen.py` a consumir o mesmo
JSON-Schema. Fecha F12 e os pontos 2 e 5 da documentação de uma vez.

**Transversal, barato, fazer já:** promover o schema dos manifestos a estável no
`cli-stability.md` (F11) — é o compromisso que os utilizadores precisam e que na prática
já está a ser cumprido.

---

## 7. Veredicto

O Delonix tem o **modelo** de uma boa ferramenta de IaC — 18 Kinds coerentes, validação
de grafo, dry-run com defaults materializados, condições de honestidade que o Terraform
não tem, e um contrato de estabilidade escrito. O que lhe falta é o **motor de
convergência**: hoje é um instalador declarativo idempotente-por-presença, não um
reconciliador.

A boa notícia é que a distância é menor do que parece, e não passa por API nenhuma: as
peças difíceis (reconfiguração a quente sem mudar o PID, ordenação topológica com espera
por saúde, `-o json` com contrato) **já estão escritas e testadas neste repo** — estão
noutros caminhos. Os blocos 1 e 2 são maioritariamente ligação, não invenção, e são o
que separa «Delonix é interessante» de «Delonix é gerível por Terraform e Ansible».
