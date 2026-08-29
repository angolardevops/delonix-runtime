# Guia — a política de segurança do nó (`policy.json`)

O que este nó **recusa correr**, seja quem for que peça: a CLI, um `crictl` a falar
com o socket, a Docker Engine API, ou alguém a escrever `delonix vm create` à mão.

O ficheiro vive em `$DELONIX_ROOT/policy.json` (por omissão `~/.delonix/policy.json`).
Não existe nenhum por omissão, e isso é deliberado — **sem ficheiro não há tecto**, e
o motor comporta-se exactamente como sempre se comportou.

## As três regras que governam este ficheiro

1. **Sem ficheiro, sem tecto.** Um nó que nunca escreveu política não recusa nada.
2. **Um ficheiro que não se consegue LER é um erro, não uma política ausente.** Um
   ficheiro truncado, ou com um campo mal escrito (`denyPriviledged`), pára o comando
   em vez de passar por «sem opinião». A intenção de alguém é desconhecida, e correr
   à mesma é a degradação silenciosa que este motor recusa em todo o lado.
3. **Um campo que ninguém põe nunca começa a recusar coisas.** É o que permite
   actualizar o motor sem que uma frota inteira pare de manhã.

## Os campos

### Caminho de container

| Campo | O que recusa |
|---|---|
| `denyPrivileged` | `container run --privileged` |
| `denyHostNetwork` | `--net host` — que é o modo **por omissão** deste motor, por isso pôr isto obriga cada carga a nomear a sua rede |
| `denyLatestTag` | uma imagem sem etiqueta, ou com `:latest`. Sem etiqueta conta: `alpine` e `alpine:latest` são a mesma coisa, e uma regra que só recusasse a forma explícita contornava-se tirando quatro caracteres. Um `@sha256:…` passa — é a forma mais forte que há |
| `allowedRegistries` | qualquer registo fora da lista. Leva **hosts** (`ghcr.io`), não referências. A comparação é da string inteira: `ghcr.io` não aceita `evil-ghcr.io` nem `ghcr.io.evil.com` |

### Caminho de VM

Estes campos são **novos** e todos desligados por omissão. Até existirem, o caminho de
VM não tinha política de nó nenhuma.

| Campo | O que recusa |
|---|---|
| `denyDevicePassthrough` | `vm create --device` (passthrough VFIO PCI). **É o `--privileged` das VMs, e mais um bocado**: um dispositivo passado dá ao convidado DMA ao hardware do host, que é um buraco mais largo do que qualquer capability que um container privilegiado receba |
| `denyLatestVmImage` | uma imagem de disco de VM sem etiqueta ou com `:latest`. Separado do `denyLatestTag` de propósito — um `vm build` etiqueta o que produz como `<nome>:latest` por omissão, e juntar os dois teria recusado uma construção normal a quem actualizasse |
| `allowedImageUrlHosts` | um `vm create --url-img` de um host fora da lista. Sem um `.sha256` publicado ao lado, esse descarregamento é confiado **só no TLS** — e o TLS prova que chegaste ao host que nomeaste, não que o host merecia ser nomeado |

### Modo

| Campo | Efeito |
|---|---|
| `mode: "enforce"` | recusa. É a omissão, e o comportamento que sempre houve |
| `mode: "warn"` | deixa passar e diz o que teria recusado. Para estrear uma regra numa frota antes de ela morder |

## Exemplo completo

```json
{
  "denyPrivileged": true,
  "denyHostNetwork": true,
  "denyLatestTag": true,
  "allowedRegistries": ["ghcr.io"],

  "denyDevicePassthrough": true,
  "denyLatestVmImage": true,
  "allowedImageUrlHosts": ["cloud.debian.org"]
}
```

## Os avisos que o nó te dá sobre ti próprio

Quando guardaste metade da casa e deixaste a outra aberta, o nó diz-to — pelo nome,
com um identificador estável, e **no caminho onde podes agir** (um aviso sobre VMs em
cada `container run` é ruído que se aprende a ignorar):

```
aviso: política de runtime [POLICY-VM-PASSTHROUGH-OPEN] este nó recusa containers
`--privileged` mas permite `vm create --device` (passthrough VFIO PCI)…
```

| Identificador | O que te está a dizer |
|---|---|
| `POLICY-VM-PASSTHROUGH-OPEN` | recusas containers privilegiados e permites passthrough VFIO |
| `POLICY-VM-LATEST-OPEN` | recusas `:latest` em containers e não em imagens de VM |
| `POLICY-VM-URL-OPEN` | restringes registos e o `--url-img` continua a ir a qualquer host |
| `POLICY-SILENT` | o ficheiro existe e não recusa nada — quase sempre um engano |
| `POLICY-REGISTRY-NOT-A-HOST` | puseste uma referência (`ghcr.io/org/app`) onde se espera um host |

Aparecem **uma vez por comando** (este motor não tem daemon: cada comando é um
processo que nasce, trabalha e morre). Para os calar, depois de os teres lido e
decidido: `DELONIX_POLICY_LINT=0`.

## O rasto

Cada recusa deixa uma linha em `$DELONIX_ROOT/events.jsonl`, com `kind: "security"` e
um identificador de regra estável:

```
delonix system events | grep security
{"ts":…,"kind":"security","action":"ADM-DEVICE-PASSTHROUGH","name":"db-01","detail":"0000:01:00.0"}
```

Os identificadores (`ADM-PRIVILEGED`, `ADM-HOST-NETWORK`, `ADM-LATEST-TAG`,
`ADM-REGISTRY`, `ADM-DEVICE-PASSTHROUGH`, `ADM-LATEST-VM-IMAGE`, `ADM-IMAGE-URL-HOST`)
são estáveis: a redacção da mensagem pode mudar, o identificador não. Alerta sobre
eles, não sobre o texto.

**Este log é sinal operacional, não prova.** É *best-effort* por desenho — um erro a
registar nunca faz falhar a operação que o gerou —, não detecta nada sobre as suas
próprias falhas, e quem tiver escrita na raiz de estado consegue editá-lo. A trilha à
prova de adulteração é a cadeia de hashes com âncora Ed25519, e essa vive no
`delonix-paas`. Dizer o contrário seria vender como prova o que é um aviso.

## O que esta política NÃO faz

Escrito para ninguém confundir o nome com o alcance:

- **Não vigia nada em execução.** Decide na admissão, e mais nada. Não há sensores
  eBPF, monitorização de integridade de ficheiros, detecção de malware, pontuação
  comportamental de ransomware nem detecção de anomalias de rede. Todos esses são
  processos residentes, e este motor é *daemonless* por desenho — abrir essa porta é
  matéria de ADR próprio, com um spike que meça o que um sensor desses consegue
  observar **em rootless**, onde este motor normalmente corre.
- **Não age.** Não congela, não isola, não põe em quarentena, não recua e não restaura.
- **Não sabe de inquilinos.** Não há `tenant`, `project` nem `environment` em lado
  nenhum, e há um teste que falha a construção se alguém acrescentar um. Isso vive no
  `delonix-paas`, por decisão registada (ADR-0010, ADR-0025).
- **Não substitui a admissão do cluster.** Complementa-a. Esta é a resposta **local**,
  e vale com o Pod Security mal configurado e com um `crictl` a falar directamente com
  o socket — porque uma política que só vive na cadeia de admissão de um cluster corre
  noutro processo, noutra máquina, que este nó não consegue ver nem verificar.

Ver [ADR-0026](adr/0026-security-runtime-decision-crate.md) para o porquê de cada uma
destas fronteiras.
