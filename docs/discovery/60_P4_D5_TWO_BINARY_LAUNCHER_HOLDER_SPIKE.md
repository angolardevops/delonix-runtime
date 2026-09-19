# 60 — P4/D5: o par de binários launcher/holder, a sério (ADR-0044)

**Data:** 2026-09-19 · **Base:** `origin/main` (`fd11501a`, v4.0.0 +10 commits, o binário
desta MESMA série de spikes) · **Alvo:** `p4d-launcher`, VM nova (libvirt, Ubuntu 24.04.4
LTS, `delonix-vm-base:ubuntu-24.04`) — o mesmo ambiente do P1b, reconstruído do zero.
**Veredicto: GO, com um achado que simplifica D2.4** — `--net <rede-custom>` não precisa do
perfil do launcher, porque nunca cria o seu próprio userns.

## A pergunta

O último dos cinco spikes do ADR-0044: "Repeat P1b's exact matrix with `delonix`,
`delonix-launcher` and `delonix-netns-holder` as three genuinely separate installed
binaries — the gap the original spike's own «not validated» section names — and add
`--net <custom>` and a pod to the matrix, neither of which P1b touched."

O P1b (`53_P1B_LAUNCHER_SPIKE.md`) tinha medido isto: (1) com o CHAMADOR e o pin no MESMO
caminho instalado, alternando só a presença do perfil AppArmor por operação; (2) sem
`--net <custom>` nem pod, os dois casos que precisam mesmo do holder.

## Como se mediu

- VM nova (a do P1b já não existia — limpa por outra sessão), criada por
  `delonix vm create --backend libvirt --disk delonix-vm-base:ubuntu-24.04`. Ambiente
  confirmado idêntico ao P1b: Ubuntu 24.04.4 LTS, kernel 6.8.0-136-generic,
  `kernel.apparmor_restrict_unprivileged_userns=1`, `apparmor_parser` 4.x.
- **Achado lateral, ao entrar**: `--ssh-key` não injectou a chave (o `authorized_keys`
  ficou vazio, apesar da CLI imprimir "log in with the key you injected" — `AGENTS.md` já
  tem um achado análogo catalogado, "uma capacidade que só existe...", mas este é novo e
  fica registado aqui, não investigado a fundo — fora do âmbito deste spike). Acesso obtido
  pela consola série (autologin, como o P1b também usou), injectando a chave à mão.
- O binário desta MESMA série (`fd11501a`, com o spike de substituição já dentro) copiado
  para o convidado e instalado em **três caminhos com hardlink** (`ln`, não `cp` — o disco
  golden de 2,4 GiB não tinha espaço para três cópias de 282 MB; o AppArmor confina por
  CAMINHO, não por inode, por isso o hardlink preserva exactamente a variável que o spike
  testa): `/opt/p4d/delonix` (sem perfil), `/opt/p4d/delonix-launcher`,
  `/opt/p4d/delonix-netns-holder`.
- Dois perfis AppArmor, mesmo molde do `install.sh` (`profile <nome> <caminho>
  flags=(unconfined) { userns, }`), um por caminho privilegiado — nenhum no `delonix` puro.
  Confirmado que o perfil pré-existente do golden (`/etc/apparmor.d/delonix`, a apontar
  para `/usr/local/bin/delonix`, de um install anterior) não interfere — caminho diferente.
- Matriz do P1b repetida (agora com três caminhos GENUINAMENTE distintos, não um perfil
  ligado/desligado no MESMO caminho) + as duas operações que faltavam.

## O que se mediu

| Operação | `delonix` (sem perfil) | `delonix-launcher`/`delonix-netns-holder` (com perfil) |
|---|---|---|
| `container run --net none` | `EPERM: failed to prepare the rootfs`, rc=126 | rc=0 |
| `container run -d` + `exec` | *(não repetido — já coberto pelo P1b)* | rc=0 / rc=0, mesmo idioma |
| `container cp` de um parado (`__ovlhold`) | *(idem)* | rc=0 |
| `container rm` (`__rmtree`) | *(idem)* | rc=0 |
| `net netns up` (pin) | erro **auto-explicativo** (novo desde o P1b — nomeia a causa E o remédio), rc=1 | rc=0 — `ps` confirma `/opt/p4d/delonix-netns-holder netns pin`/`netns control`, os DOIS processos a correrem do caminho certo |
| **`container run --net <rede-custom>`** (NOVO) | **rc=0** — funciona SEM perfil (ver "Achado", abaixo) | rc=0, IP real na rede custom (`10.253.0.2/16`) |
| **`pod create`** (NOVO) | `EPERM: failed to create the pod netns`, mesmo texto auto-explicativo do pin, rc=1 | rc=0 — `10.200.0.2`, netns partilhada, `exec` dentro do pod funciona |

## Achado — `--net <rede-custom>` não precisa do perfil do launcher, e a razão está no
próprio mecanismo já documentado

Medido, não hipotético: `delonix container run --net p4dnet alpine:3.20 echo hi` **teve
sucesso sem qualquer perfil AppArmor**, atribuindo um IP real (`10.253.0.2/16`) — ao
contrário de `--net none`, que falha sem o perfil.

A razão está já escrita no `AGENTS.md` deste repositório, secção "CLI" (`container run`):
o re-exec de `--net <rede-custom>` corre `nsenter -t <holder_pid> -U -m -n … ip netns exec
<netns>`, e a SEGUNDA passagem corre com `RunSpec.inherit_userns` — suprime
`CLONE_NEWUSER`/`CLONE_NEWNET` e **herda** os namespaces do holder em vez de criar os seus
próprios. `nsenter -U` para ENTRAR num userns já existente não é a mesma chamada que
`unshare(CLONE_NEWUSER)` para CRIAR um — e é só a segunda que a regra `userns` do AppArmor
medeia. Como o userns já existe (criado UMA vez, quando o holder arrancou — esse sim, com o
perfil), juntar-se a ele não precisa de privilégio nenhum que o AppArmor negue.

**Isto simplifica o D2.4 real**: nem toda operação que hoje "cria namespaces" precisa do
perfil do `delonix-launcher` — só as que criam um userns de RAIZ (`--net none`/rede
por omissão, `pod create` — que cria a SUA PRÓPRIA netns do zero, confirmado pelo erro
"failed to create the pod netns", diferente do erro do pin). Um container que se anexa a
uma rede custom já existente, com o holder já de pé, vive inteiramente da CRIAÇÃO anterior
do userns do holder. **Não corrigido nem construído aqui** — é um achado para quem desenhar
a matriz de capabilities de D2.4/D3 a sério: a fronteira é "quem CRIA um namespace", não
"quem toca em namespaces".

## Confirmações que batem certo com o P1b

- O launcher continua GO: toda a criação de namespaces PARA CONTAINERS passa com o perfil
  só no seu caminho.
- **A condição do holder está CONFIRMADA, agora com separação real**: os processos do pin/
  control correm mesmo do binário `delonix-netns-holder`, não de uma cópia do `delonix`
  a fingir ser holder — o `ps aux` prova-o pelo `/proc/<pid>/comm`/caminho do executável,
  não pela leitura do código.
- O erro do pin sem perfil MELHOROU desde o P1b: já não é um timeout genérico
  ("waiting for the netns holder") nem o defeito do `/run` vs `/tmp` que o P1b teve de
  corrigir por leitura — é uma mensagem que nomeia a causa (`kernel.
  apparmor_restrict_unprivileged_userns=1`) e o remédio (`install.sh` escreve o perfil) —
  a mesma classe de conserto que este repositório já tinha feito para outros erros de
  privilégio.

## Proven vs not validated

**Proven**: os três binários, em caminhos genuinamente distintos, com perfis
independentes, convergem correctamente — incluindo os dois casos que o P1b nunca tocou;
que `net netns up`/`pod create` precisam do perfil e `container run --net <custom>` não,
com a razão mecânica já documentada noutro sítio deste repositório, agora confirmada ao
vivo; que o erro sem perfil é hoje auto-explicativo nos dois caminhos que o exigem.

**Not validated**: upgrade in-place com os TRÊS binários reais (P1b só leu o código para
esta parte, ver o seu item 4; este spike também não o correu — precisava de uma versão
ANTERIOR do binário instalada primeiro); um `delonix-launcher`/`delonix-netns-holder`
como CRATES/`[[bin]]` targets DISTINTOS com dispatch próprio (este spike, como o P1b,
usa três cópias hardlinked do MESMO binário — a divisão real de código é P4d, não este
spike); libvirt CH não foi re-testado aqui (já coberto pelo spike nº1); Debian/Rocky/
outras distros; root (só rootless).

## Limpeza

VM `p4d-launcher` destruída (`delonix delete vm`) — nada ficou no host além do que já lá
estava. O estado dentro do convidado (rede `p4dnet`, pod, containers) morreu com a VM.
