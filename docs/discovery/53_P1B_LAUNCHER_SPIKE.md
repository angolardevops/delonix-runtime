# 53 — P1b: spike do `delonix-launcher` e do holder de rede (ADR-0040)

**Data:** 2026-09-16 · **Base:** `fix/p0-findings` (`636ccaf1`, v3.1.0 +64 commits) ·
**Veredicto: GO com uma condição** — o holder de rede tem de criar os seus namespaces
dentro do próprio processo.

## A pergunta

O ADR-0040 (D2.4) separa o executável `delonix` em binários com um papel cada, e quer que
**só** o `delonix-launcher` e o `delonix-netns-holder` precisem de criar user namespaces.
Num Ubuntu com `kernel.apparmor_restrict_unprivileged_userns=1`, isso significa um perfil
AppArmor com `userns` nesses dois caminhos e **nenhum** na CLI. A pergunta do portão P1b:
isso funciona, ou a restrição parte o motor por dentro?

## Como se mediu

- VM `p1b-launcher` criada pelo próprio motor a partir da `delonix-vm-base:ubuntu-24.04`
  (libvirt, 2 vCPU, 2 GiB), raiz de estado isolada no host.
- No convidado: Ubuntu 24.04.4 LTS, kernel 6.8.0-136-generic,
  `kernel.apparmor_restrict_unprivileged_userns = 1`, AppArmor parser 4.0.1,
  util-linux 2.39.3, uidmap 4.13. Utilizador sem privilégio (`delonix`, subuid
  `100000:65536`).
- **O mesmo binário** (build desta árvore, sem símbolos) instalado em três caminhos:
  `/opt/p1b/delonix` (sem perfil, faz de CLI), `/opt/p1b/delonix-launcher` e
  `/opt/p1b/delonix-netns-holder`. O AppArmor autoriza por caminho, por isso três cópias
  do mesmo ficheiro são a forma honesta de isolar a variável.
- A matriz (`53_P1B_LAUNCHER_SPIKE/matrix.sh`) corre as operações que criam namespaces:
  `container run`, `run -d`, `exec`, `cp` num container parado (re-exec `__ovlhold`),
  `rm` (re-exec `__rmtree`) e o pin de rede (`net netns up`). Cada caminho tem a sua raiz.
- Duas corridas, com o output em bruto guardado ao lado deste ficheiro: sem perfis
  (`matrix-base.txt`), e com perfis `unconfined + userns` só para o launcher e o holder
  (`matrix-profiles.txt`). Controlo: o `/usr/local/bin/delonix` que a imagem traz, com o
  perfil que o `install.sh` escreve.

## O que se mediu

| Operação | CLI sem `userns` | Launcher com `userns` |
|---|---|---|
| `image load` | rc=0 | rc=0 |
| `container run --net none` | **rc=126** `failed to prepare the rootfs: EPERM` | rc=0 |
| `container run -d` + `exec` | run **rc=0**, exec rc=3 `container is not running` | rc=0 / rc=0 |
| `container cp` de um parado (`__ovlhold`) | **rc=1** `__ovlhold unshare: EPERM` | rc=0 |
| `container rm` (`__rmtree`) | rc=0 | rc=0 |
| `net netns up` (pin) | **rc=1** `timeout waiting for the netns holder` | rc=1 `control socket: No such file` — **não é o AppArmor**, ver «Correcção» |

1. **Launcher: GO.** Com o perfil só no caminho do launcher, tudo o que cria namespaces
   para containers passa — incluindo os re-execs `__ovlhold`/`__rmtree`, que re-executam
   `current_exe()` e por isso herdam o caminho, e com ele o perfil. O mesmo binário noutro
   caminho, sem perfil, é recusado. A separação do D2.4 é viável.
2. **Holder: NÃO na divisão do D2.4, e a prova que aqui estava era a errada** (ver
   «Correcção», abaixo). O pin arranca com `Command::new("unshare") --user --map-root-user
   --net --mount -- <exe> netns pin` (`delonix-net/src/infra.rs`, `start_pin`), e quem cria
   o user namespace é o **`/usr/bin/unshare`**. Isso só passa enquanto o CHAMADOR tem um
   perfil com `userns`, porque um perfil `unconfined` é herdado no `exec`; um chamador sem
   perfil faz o `unshare(1)` cair no `unprivileged_userns` e o `mount` é negado. Dar
   `userns` ao `/usr/bin/unshare` abriria user namespaces a qualquer utilizador e anularia
   a restrição — não é opção.
3. **O caminho que funciona já está provado pelo launcher:** um binário com perfil que
   chama `unshare(2)` e `newuidmap` DENTRO do próprio processo (o `reexec_mapped` faz
   exactamente isso) passa. A condição do GO é o holder fazer o mesmo.
4. **Upgrade in-place (lido no código, sem VM):** o pin é reconhecido pelo par `netns pin`
   no argv e pelas variáveis de ambiente que o motor fixa (`argv_matches`,
   `env_names_this_root`), nunca pelo nome do binário — e já há precedente de aceitar a
   grafia antiga (`is_pre_split_holder`, `netns holder`, anterior à v0.42.0). Um
   `delonix-netns-holder` reconhece um pin arrancado por `delonix netns pin` desde que
   mantenha essa regra; é um teste puro quando o binário existir.

## Correcção (2026-09-16, depois do spike)

A linha do pin na coluna do launcher **foi mal lida**. A saída em bruto
(`matrix-profiles.txt`) diz `control socket: No such file or directory`, não um timeout do
holder: a matriz punha `DELONIX_NET_RUNTIME_DIR` em `/run/user/<uid>/…`, e o control monta
um tmpfs em `/run` dentro do mount namespace do pin, o que esconde o socket. É um defeito
próprio, independente do AppArmor.

Remedido na mesma imagem, com a restrição a 1 (`pin-matrix.sh`, saída em
`pin-matrix-result.txt`): binário da base (`unshare(1)`) e binário com o pin a criar os
namespaces no próprio processo, cada um num caminho com perfil e noutro sem, e a pasta de
runtime em `/tmp` e em `/run`.

| Binário | Perfil no caminho | `/tmp` | `/run` |
|---|---|---|---|
| base, `unshare(1)` | sim | **UP** (456 ms) | `control socket: No such file` |
| pin no próprio processo | sim | **UP** (393 ms) | `control socket: No such file` |
| base, `unshare(1)` | não | timeout de 5 s | timeout de 5 s |
| pin no próprio processo | não | recusa em ~170 ms com a razão (`mount … EACCES`) | igual |

O audit explica a primeira linha: nas corridas com perfil não há nenhum `userns_create`
transitado para `unprivileged_userns` — o `unshare(1)` herdou o perfil do chamador. Nas
corridas sem perfil há, com `comm="unshare"`. Portanto:

- **Com a CLI e o holder no mesmo caminho com perfil (o `install.sh` de hoje), o `unshare(1)`
  funciona.** O spike não mostrou o contrário.
- **A condição do D2.4 mantém-se, por outra razão:** com o perfil só no holder, o chamador
  não tem perfil, e o `unshare(1)` que ele lança cai no `unprivileged_userns` — medido na
  linha «sem perfil». Tem de ser o executável do holder a chamar `unshare(2)`.
- **A pasta de runtime debaixo de `/run` parte o control** em qualquer binário. Fica
  registado como defeito à parte.

## Achado lateral

**`container run -d` responde rc=0 com o container morto.** Com a restrição activa e sem
perfil, o init do container morre a preparar o rootfs (`EPERM`), o `run -d` devolve o id e
**sai 0**, e só o `exec` seguinte diz `container is not running`. O `AGENTS.md` já o
registava como decisão por tomar («um run -d cujo init morre a montar continua a reportar
o que reportava»); aqui está medido num host real com uma causa comum. Merece correcção
própria: um script ou CI lê este `0` como sucesso.

## Não validado

- A criação no próprio processo foi escrita depois (branch `fix/holder-in-process-userns`)
  e medida na tabela da «Correcção», mas **com a CLI e o pin no mesmo caminho**: a divisão
  em dois executáveis ainda não existe.
- `--net <rede-custom>` e pods (precisam do holder) não foram medidos.
- Só rootless, só libvirt, só Ubuntu 24.04. A imagem dourada usada é de 2026-08-12.
- Build de debug (sem símbolos), não uma release.
