# Estrutura de recursos

O que se pode escrever num manifesto, com que `apiVersion`, e o que acontece a
cada documento depois de carregado.

Esta página não é escrita à mão a partir de uma lista: sai do mesmo registo que
o parser, o schema, a completação e o reconciliador leem. Para a ver no teu
próprio motor, em vez de aqui:

```bash
delonix api-resources
delonix api-resources -o json
```

## Os grupos

Cada Kind vive num grupo, e o grupo faz parte da identidade — um
`apiVersion: storage.delonix.io/v1alpha1` num `kind: Pod` é recusado, com as
duas formas aceites nomeadas no erro.

| grupo | Kinds |
|---|---|
| `core.delonix.io/v1alpha1` | `Secret`, `Stack` |
| `compute.delonix.io/v1alpha1` | `Pod`, `VirtualMachine`, `Container`, `Workload` |
| `networking.delonix.io/v1alpha1` | `Network`, `NetworkRoute`, `NetworkPolicy`, `Dependency` |
| `gateway.delonix.io/v1alpha1` | `Gateway`, `HTTPRoute`, `Ingress` |
| `storage.delonix.io/v1alpha1` | `Volume` |
| `artifact.delonix.io/v1alpha1` | `Image` |
| `infrastructure.delonix.io/v1alpha1` | `KubernetesCluster` |

## `apiVersion: delonix.io/v1` continua a carregar

Não é uma grafia legada a caminho da porta. A [promessa de
estabilidade](estabilidade.html) diz que o `delonix.io/v1` só muda com um `v2`
que o continue a aceitar, e essa promessa mantém-se: **um manifesto escrito antes
desta reorganização carrega sem uma alteração**.

O corte limpo aplica-se a **comandos**. Um ficheiro que está em git, revisto em
PR e apontado por `$schema` num editor, ganha um degrau — não um erro.

## O que acontece a cada documento

A coluna `FORM` do `api-resources` é a que não se adivinha: diz se um documento
daquele Kind **sobrevive ao load com o próprio nome**. É a resposta a «porque é
que o meu `kind: Dependency` nunca aparece no plano com esse nome».

| forma | significado |
|---|---|
| `primary` | tem apply próprio e sobrevive ao load |
| `sugar → X` | é reescrito em `X` por conveniência |
| `compat → X` | schema estrangeiro aceite tal e qual, compilado sobre o mecanismo de `X` |
| `aggregate` | expande-se nos documentos que contém |
| `sunset → X` | **funciona e não é reescrito** — mas `X` é o caminho a seguir |

### `sunset` é diferente de `deprecated`, e a diferença é o ponto

Um Kind `deprecated` é **reescrito** no load, e quem o escreveu ganha o
comportamento do sucessor de graça. Um Kind `sunset` **não** é reescrito, porque
reescrevê-lo mudaria o que o motor FAZ.

O `kind: Container` é o caso que forçou a distinção. Baixá-lo para um
`kind: Pod` de um container parece uma renomeação e não é: um Pod constrói
sempre uma netns partilhada e os membros entram nela por re-exec, portanto todo
o container declarado passaria a ter um holder de netns extra e um caminho de
rede diferente. A metade do nome era solúvel; a da netns não é.

Por isso é **anunciado, não reescrito**: continua a funcionar, com um aviso por
carregamento a dizer que `kind: Pod` é o caminho. Uma major futura remove-o,
depois de os manifestos terem migrado.

## Kinds removidos

Três Kinds **deixaram de existir**. A recusa nomeia o que escrever em vez deles,
em vez de dizer «Kind desconhecido» — que faria um manifesto correcto até ontem
parecer um erro de escrita:

| removido | escrever |
|---|---|
| `Storage` | `kind: Volume` com um bloco `nfs:`/`cifs:`/`webdav:` |
| `ShareVolume` | `kind: Volume` com um bloco `share:` |
| `Egress` | `kind: NetworkPolicy` com `direction: egress` |

Os três eram **reescritos** no load para exactamente estas formas, portanto o que
o motor faz não mudou — mudou quem tem de escrever a forma final. Ver as notas
da versão que os removeu para a migração.

## Nomes antigos que continuam a resolver

Quatro Kinds foram renomeados. O nome antigo é um **alias silencioso** — não há
aviso, porque uma renomeação não muda o que o documento significa e não há nada
para migrar:

| antes | agora |
|---|---|
| `Vm` | `VirtualMachine` |
| `FirewallPolicy` | `NetworkPolicy` |
| `Tunnel` | `Gateway` |
| `Cluster` | `KubernetesCluster` |

O alias vale em todo o lado, não só no carregador: `delonix explain Cluster`
resolve tanto quanto `delonix explain KubernetesCluster`.

## Plurais e abreviaturas

O `get`, o `describe` e o `explain` aceitam quatro grafias de cada Kind — o nome
canónico, o singular em minúsculas, o plural e a abreviatura declarada:

```bash
delonix explain Pod
delonix explain pod
delonix explain pods
delonix explain po
```

As abreviaturas são deliberadamente poucas — existe uma quando é inequívoca e
vale a pena escrever. `delonix api-resources` lista as que há.

## O que ainda não existe

O `kind: Service` está previsto e **ainda não foi implementado**. Hoje a
publicação de portas faz-se pelo `-p` do `container run` e pelo
`delonix net ingress publish`; a forma declarativa entra numa versão seguinte.
