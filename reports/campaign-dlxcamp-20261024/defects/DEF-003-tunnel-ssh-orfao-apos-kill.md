# DEF-003 — `net tunnel expose` deixa o `ssh -R` órfão (exposição pública persiste) quando o processo pai é morto

- **Requisito/teste:** TUNNEL-02 (§17 — stop/remoção; "restart do Delonix"), §19 (recuperação/órfãos)
- **Severidade:** P2 (segurança — exposição pública sobrevive ao que o operador julga ser a paragem)
- **Ambiente:** delonix v5.1.1 `350a6320…` (develop `558e5f20`), pinggy provider, host rootless

## Esperado
Parar um túnel (fechar o processo `delonix net tunnel expose`) termina o encaminhamento e fecha a exposição pública.

## Observado
- `net tunnel expose 18090 --provider pinggy` lança um filho `ssh -p 443 ... -R 0:localhost:18090 free.pinggy.io`.
- Ao matar o processo pai `delonix net tunnel expose` (SIGTERM via `pkill -f`), o filho `ssh -R` **sobrevive** e a URL pública **continua a servir a origem** (verificado: `curl https://<id>.free.pinggy.net/index.html` → `dlxcamp-20261024-OK` DEPOIS do kill do pai).
- Só um **SIGKILL explícito ao processo `ssh`** fechou a exposição.

## Passos mínimos de reprodução
```
delonix net tunnel expose 18090 --provider pinggy &   # anota a URL
curl https://<id>.free.pinggy.net/           # serve a origem
pkill -f 'net tunnel expose 18090'            # "parar" o túnel
curl https://<id>.free.pinggy.net/           # AINDA serve (ssh -R órfão vivo)
pgrep -af 'ssh -p 443.*pinggy'                # processo órfão presente
```

## Impacto
Um operador que termina o processo do túnel (ou cujo processo morre) pode acreditar que a exposição fechou, quando o `ssh -R` órfão a mantém pública. Risco de exposição não-intencional prolongada.

## Nuance / limites do teste
- A paragem foi por kill não-gracioso do pai; o teardown gracioso (SIGINT/Ctrl+C em foreground, ou um eventual `tunnel stop`) pode limpar correctamente — não foi possível exercitar um `tunnel stop` (não existe subcomando; o help só expõe `expose`/`apply`).
- Portanto o defeito é: **morte não-graciosa do pai não arrasta o filho `ssh`**. Confirmar se há (ou falta) um process-group kill / `prctl(PDEATHSIG)` no supervisor do túnel.

## Critério de encerramento
Matar o processo `delonix net tunnel expose` (por qualquer sinal) termina também o `ssh -R` filho (ex.: colocá-lo no mesmo process group e sinalizar o grupo, ou `PR_SET_PDEATHSIG`), fechando a exposição; idealmente existir também um `net tunnel stop`/`ls`.
