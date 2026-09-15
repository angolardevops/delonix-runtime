# Performance — NÃO EXECUTADO nesta auditoria

Motivo: o host é de produção (Docker e Podman activos, containers reais do utilizador, várias sessões
de build em paralelo — load 1m oscilou entre 2,9 e 18 durante a sessão). Um benchmark aqui não teria
configuração equivalente entre Docker, Podman e Delonix nem ruído controlado; `scripts/bench.sh` recusa
por omissão acima do limiar de carga, e forçá-lo produziria números não publicáveis.

Última evidência existente (STALE, não reutilizada como resultado):

| Medição | Número | Versão |
|---|---|---|
| `run --rm` (4a) Docker / Podman / Delonix | 299 / 277 / 80 ms | v0.63.1 |
| Tabela principal | 208 / 268 / 91 ms | v0.53.0 |

Tratar como **hipótese** até correr `scripts/bench.sh` sobre o commit candidato numa VM dedicada, com
mediana, p95, p99, variância e amostras brutas.
