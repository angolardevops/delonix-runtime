# Avaliação de segurança — `fa8c6297`

Não é um pentest completo. É o modelo de ameaça das fronteiras pedidas, cruzado com o que foi
**medido** nesta sessão e o que foi apenas **lido** no código.

## Fronteiras e resultado

| Fronteira | Medido | Resultado |
|---|---|---|
| Registo / pull | digest inexistente recusado; digest correcto honrado; tag+digest em conflito grava etiqueta errada | PASS + G-033 |
| Extracção de archive | `../`, caminho absoluto e symlink→/tmp recusados, nada fora do root | PASS (resíduo G-035) |
| Build context / Dockerfile | não re-testado (corrigido e testado em auditorias anteriores) | NOT TESTED |
| exec logo após `run -d` (corrida para o host) | gates E2E da janela e do registo passaram | PASS |
| Bind mounts / devices / GPU | cobertos por E2E; GPU sem hardware | PARTIAL |
| Rede — anti-spoof | **falha aberto** com nft a falhar (injecção) | FAIL (G-008) |
| Rede — isolamento de namespace | caos PASS (containers e pods) | PASS |
| Socket CRI / Docker API / mgmt | 0600 medido no Docker API; SO_PEERCRED por leitura; chmod após bind | PARTIAL (G-015) |
| MCP | stdio, sem shell; `name` com `-` inicial não recusado | PARTIAL (G-017) |
| Secrets `--secret-files` | caminho sem `/run` gravável entrega zero segredos com rc=0 (medido); falha de tmpfs escreveria no rootfs (lido) | FAIL (G-026 medido, G-007 lido) |
| slirp api-socket por container | 0755 medido (não conectável por outros); caminho previsível em /tmp | PARTIAL (G-016) |
| Hardening best-effort | masked/readonly paths e rlimits só avisam | PARTIAL (G-009) |
| Env var sem efeito | `DELONIX_INTERNAL` definido, nunca lido | G-014 |
| Supply chain CI | 0/20 actions pinadas | FAIL (G-020) |
| VM providers | opção não honrável recusada; nenhuma acção fora do delonix | PASS |
| PaaS × runtime (portas) | reaper do PaaS remove forwards com host address e sem registo (lido) | FAIL (G-036, G-037) |
| SELinux | host sem SELinux | ENVIRONMENT BLOCKED |
| Rootful | não exercitado | NOT TESTED |

## P0 abertos

Nenhum P0 **confirmado**. Candidato a P0 dependendo de reprodução: **G-007** (segredos no rootfs em
disco se o tmpfs falhar) — hoje só lido no código, sem gatilho natural encontrado.

## P1 de segurança

- **G-008** — isolamento por IP de origem é a base do isolamento de namespace e das `Dependency`; um
  `nft` que falha (ruleset cheio, binário/versão incompatível, holder antigo) deixa o workload sem
  anti-spoof e ninguém é avisado.
- **G-036** — no deploy PaaS com root partilhado, containers publicados com endereço perdem o
  forward a cada 30 s (disponibilidade, não confidencialidade).
