---
name: delonix-runtime-stack
description: Domínio do IaC declarativo do delonix — o manifesto multi-documento (15 Kinds), o reconciliador (`stack plan`/`apply` convergente/`destroy`), o diff de 3 vias sem ficheiro de estado, posse por label, e o schema gerado do código. Usa quando acrescentares ou mudares um Kind, mexeres em `cmd/{manifest,reconcile,stack}.rs`, ou quando algo aparecer como deriva eterna num plano.
---

# IaC declarativo — o reconciliador, e o que o mantém honesto

## O modelo

`apiVersion: delonix.io/v1` / `kind` / `metadata` / `spec`, multi-documento, 15
Kinds. `stack plan` → `apply` (convergente) → `destroy`. Ficheiros:
`cmd/manifest.rs` (parsing central), `cmd/reconcile.rs` (o diff, **PURO**),
`cmd/stack.rs` (orquestração), e um `spec` tipado + `apply()` por grupo.

**O `reconcile.rs` é puro de propósito** — recebe os dois lados já lidos e
devolve `Vec<Change>`; nunca abre um store nem corre um comando. É o que torna
testáveis como dados os casos que interessam.

**Diff de 3 vias sem ficheiro de estado.** O último spec aplicado vive no
PRÓPRIO recurso (`delonix.io/last-applied`) — o mecanismo do kubectl. É o 3.º
lado que distingue «tiraste o campo do ficheiro» (reverte) de «alguém pôs isto à
mão» (não mexe). Nunca dessincroniza de um `.tfstate` porque não há nenhum.

**Posse por label** `delonix.io/stack`. Um recurso criado à mão é `Adopt`
(dispensa um `import`); de outra stack é `Conflict` e nunca é tocado; nem
`--prune` nem `destroy` vêem o que não tem a label.

## O que este subsistema existe para não voltar a fazer

**O `apply` só criava.** Um recurso existente imprimia «already exists» e o
comando devolvia **0** — mudar a imagem no manifesto não fazia nada e reportava
sucesso. Gémeo declarativo do relato desonesto que a v0.37.0 tirou do CLI, e
pior, porque o utilizador mudou o ficheiro de propósito.

**A capacidade já cá estava**: o `cmd_update` reconfigura a quente sem mudar o
PID e o caminho declarativo nunca lhe chamou — 5.ª ocorrência do padrão
«função pública, zero chamadores, bug latente à espera do primeiro».

## Regras que não se quebram

**A normalização é o ponto crítico.** Se os dois lados não derem a mesma string,
tudo aparece como deriva PARA SEMPRE. Cada Kind tem teste a provar que um
manifesto inalterado dá ZERO diferenças, e `stack plan --fields` diz o que é
comparado e o que não é e porquê. Ao acrescentar um campo ao diff, escreve o
teste primeiro.

**Três listas de Kinds convergentes têm de concordar, e já divergiram.** O
`CONVERGING_KINDS` decide três coisas (se o `actual_of` sonda, se o
`converge_and_stamp` aplica, se carimba) e os braços do `match` e a tabela do
`--fields` são escritos à parte. Vm/FirewallPolicy/ShareVolume ganharam
adaptador, ficaram fora da constante, e eram SALTADOS — escondido porque o apply
antigo de cada Kind é idempotente e convergia pelo caminho errado. Há teste a
exigir as três de acordo nos dois sentidos.

**`Desired.ownable` separa «converge» de «é possuível».** Uma `Image` é cache
partilhada com endereço de conteúdo (o mesmo `alpine:latest` serve todas as
stacks); uma `FirewallPolicy` e uma `ShareVolume` não têm registo onde carimbar.
Sem esta distinção apareciam como `Adopt` em TODOS os planos.

**Fail-closed na recriação.** `-/+` nomeia TODOS os campos frios e o `apply`
recusa sem `--replace <Kind>/<nome>`, ANTES da primeira criação — o apply é
fail-fast sem rollback, e recusar a meio deixaria a stack meio convergida E com
erro. O valor de `--replace` é verificado contra o manifesto: sem isso, um typo
lia-se como autorizado.

**Um Kind que o plano não consegue comparar é MARCADO, nunca omitido.** Só o
`Secret` fica em «garante presente» (o estado são valores cifrados, e um plano
não os decifra). O plano marca-o `!` — um plano que esconde um recurso lê-se
como «sem alterações» — e o `--fields` diz o obstáculo concreto, porque «ainda
não converge» lê-se como «ninguém chegou lá».

**Os filhos de um `kind: Stack` são construídos DENTRO do `load`** e não passam
pelo ciclo, por isso qualquer açúcar/redução tem de correr nos DOIS caminhos ou
um grupo do Stack produz documentos que nenhum handler reclama.

**Duas políticas para o mesmo (alvo, direcção) são RECUSADAS.** O `apply_fw_doc`
substitui as regras de uma direcção: a segunda apagava as da primeira, ambas
reportavam sucesso, e o `validate` dizia «OK».

## Schema

Gerado do código (ADR-0007), publicado em `docs/schema/v1/delonix.json`, com
teste a garantir que É o gerado. `additionalProperties: false` para apanhar o
typo — mas a lista de aceites vem dos MESMOS `*_SPEC_FIELDS` do
`warn_unknown_fields`: derivar a estritez só do struct sinalizaria manifestos
correctos, e um falso positivo é pior que a lacuna.

**O `warn_unknown_fields_in` aceita caminho com pontos** — um bloco pode conter
um bloco, e sem isso um typo lá dentro é engolido para dentro de «campo em
falta».

## Ao acrescentar um Kind

1. `spec` tipado com `Serialize` + `spec_with_defaults` (para o `--dry-run`).
2. Entrada em `*_SPEC_FIELDS`, e o schema regenerado.
3. Decide `converges`/`ownable` e põe-no nas TRÊS listas, ou o teste falha.
4. Teste de round-trip: manifesto inalterado → zero diferenças.
5. Exemplo em `examples/` — há teste que o valida contra o schema e o dry-run.
6. Corre `delonix-testing`; se o Kind tocar em credenciais ou destruir dados,
   `delonix-runtime-sec` também.

## No roteiro de auditoria

Cobre os pontos **1, 2 e 3** no domínio declarativo. É também o subsistema onde o
ponto **10** se mede melhor: o `stack plan --detailed-exitcode` é o gate de
deriva de um nó em produção, e a prova de que uma mudança reverteu não é o `rc`
do apply — é o plano seguinte nada ter a propor. Ordem e relatório em
`delonix-auditoria`; gates e aprendizados em `delonix-aprendizados`.
