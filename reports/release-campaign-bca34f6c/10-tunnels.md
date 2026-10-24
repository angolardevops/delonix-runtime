# 10 — Túneis (net tunnel expose)

SUT: `delonix 5.1.1` @ `bca34f6c3`. App sintética: nginx-unprivileged em `127.0.0.1:18100`. Verbo: `delonix net tunnel expose <port> --provider <p>`.

| # | Caso (§17) | Observado | Estado |
|---|---|---|---|
| TUNNEL-1 pinggy | HTTP público + origem | URL `https://gtzfv-….run.pinggy-free.link`; **fetch público HTTP 200 (896 B)**; log do nginx mostra `GET /?relcprobe=pinggy7a3f9 200` com XFF `102.130.192.246` (edge pinggy) → **round-trip público→origem provado** | **PASSOU** |
| TUNNEL-1 ngrok | estabelecimento | URL `https://repose-unsworn-aviator.ngrok-free.dev` (host tem authtoken ngrok em cache) | **PASSOU** (estabelecimento) |
| TUNNEL-1 cloudflare | HTTP público | URL quick `https://yacht-norm-assists-claims.trycloudflare.com` atribuído; **fetch público expira (curl rc=124)**, origem não recebe o pedido | **FALHOU no edge (ambiente)** |
| TUNNEL-origin | prova de readiness da origem | pinggy/ngrok reportam `(origin ready)` — a sonda `probe_origin` (commit 06df2d56) corre após (re)arranque do agente | **PASSOU** |
| TUNNEL-2 stop/remove | `delete gateway <name>` | `tunnel-18100`/`tunnel-19999: removed`; processos filhos (ngrok/ssh) reapados; registos pré-existentes (6) intactos | **PASSOU** |

## Notas de rigor

- **cloudflare edge não-fiável neste sandbox:** o túnel ESTABELECE (URL atribuído, agente a correr),
  mas o caminho edge→origem expira (HTTP 525/timeout) — coerente com observação prévia do projecto;
  classificado como **limitação de ambiente**, não defeito do produto. Registada a 1ª ocorrência (rc=124),
  sem retries a esconder.
- **Teste "origem indisponível" invalidado por mim:** expus a porta 19999 assumindo-a livre, mas há
  um listener pré-existente (não meu, owned por outro uid/root) em `0.0.0.0:19999` — logo a origem
  NÃO estava morta e o `(origin ready)` estava **correcto**. O caminho `Unavailable` do `probe_origin`
  está coberto por testes unitários (`tunnel.rs:2716,2720`). **Não é defeito.**
- **Método de paragem:** parar com `timeout`/`pkill` do wrapper deixa filhos `ssh -R`/`ngrok` órfãos;
  a forma limpa é `delonix delete gateway <name>` (reapa os filhos). Nota de operação, não defeito
  confirmado (não testei SIGTERM directo ao wrapper isoladamente).
- **`net tunnel` mantém uma lista persistente de registos** (`get gateway`) — a campanha removeu só os
  seus (`tunnel-18100/19999`); os 6 pré-existentes (`kambacode-*`, `nokubiko-*`, `devfast-*`) preservados.

## Não testado

Domínio próprio/named tunnel (exige token/conta — fora do âmbito sem credenciais), reconexão após queda
de rede, TCP/outros protocolos, resolução de localhost com agente remoto.
