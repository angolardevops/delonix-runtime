# 56 — P4/D6: spike do `StateRepository<T>` (ADR-0044)

**Data:** 2026-09-18 · **Base:** `origin/main` (`d16b2f69`, v4.0.0 +7 commits) ·
**Veredicto: GO, com o sketch do D6 corrigido em quatro pontos — e um achado novo que
reduz o âmbito do que o D6 tinha por certo.**

## A pergunta

O ADR-0044 D6 propõe um port `StateRepository<T>` para fechar cinco excepções gémeas do
`scripts/arch_fitness.py` (`delonix-linux`, `delonix-vm`, `delonix-sdn`, `delonix-oci`,
`delonix-volume` → `delonix-state`), todas com a mesma razão escrita: "o adaptador abre o
seu próprio registo directamente; o P4 entrega-lhe um port `StateRepository` a partir da
composition root." O spike nº4 que o ADR exige antes de "Accepted" pergunta duas coisas:

1. O port pode ser introduzido **sem mudar** a garantia de `JsonStore::update`/
   `Store::update` (lock + re-leitura sob o lock + escrita)?
2. As cinco excepções são mesmo a mesma forma de dívida?

## Como se mediu

- Implementação real do sketch do D6, contra o `Store<Container>` que `delonix-linux`
  já usa (ver `docs/discovery/56_P4_D6_STATE_REPOSITORY_SPIKE/repository.rs.snapshot`,
  o ficheiro `crates/adapters/delonix-state/src/repository.rs` de uma branch de spike).
- Dois testes de concorrência, mesma forma dos já existentes
  `update_concorrente_nao_perde_escritas`/`jsonstore_update_concorrente_nao_perde_escritas`
  (`store.rs`): 24 threads a incrementar um contador no MESMO container através da porta,
  cada uma a abrir o seu próprio `Store::open` (mimetiza CLI e CRI como dois processos),
  com uma pausa de 2 ms entre a leitura e a escrita para forçar a janela da corrida.
- **Verificação por reversão** (a mesma disciplina que este repo já aplica a cada
  correcção): comentou-se o `FileLock::acquire` real dentro de `Store::update` e
  correu-se o teste "real" (através da porta, com `Store` por trás) — se a garantia
  vier mesmo do `flock` e não da forma do trait, o teste tem de FALHAR sem ele.
  Reposto o `FileLock::acquire` a seguir e corrida a suite completa do crate.

## O que se mediu

| Corrida | `state_repository_update_atraves_da_porta_nao_perde_escritas` | `sem_lock_por_baixo_a_mesma_porta_perde_escritas` (controlo) |
|---|---|---|
| `Store::update` sem o `flock` (reversão) | **FALHOU** — 5 de 24 (`docs/discovery/56_.../without-lock.txt`) | passou (perdeu escritas, como esperado do controlo) |
| `Store::update` com o `flock` restaurado | passou — 24 de 24 | passou (o controlo continua a perder, prova de que a corrida é real) |

Suite completa do `delonix-state` com o lock restaurado: **34/34** (`with-lock.txt`).

**1. GO para a pergunta 1** — a garantia sobrevive ao trait: o `StateRepository<T>`
proposto não precisa de reinventar nada do `flock`/read-modify-write, só de o expor.

**2. O sketch do D6 tinha quatro formas que não sobrevivem ao primeiro chamador real**
(`repository.rs.snapshot`, doc-comment do módulo, tudo confirmado ao tentar migrar
`delonix-linux::wait_and_record`/`persist_stop` de verdade):

1. O `f` do sketch devolve `Result<()>`; os chamadores reais usam `false` para dizer
   "este registo já não descreve a incarnação sobre a qual estou a reportar — aborta,
   e NÃO é erro". Forçar isso a `Result<()>` obrigava um `Err` para um no-op saudável —
   exactamente o "relato desonesto ao contrário" que a doutrina deste repo evita.
2. `update` tem de devolver o valor FINAL (`Result<T>`), não `Result<()>` — o próprio
   `Store::update` já o faz precisamente para o chamador não precisar de uma 2.ª leitura
   fora do lock, o que reabriria a janela da corrida que o port existe para fechar.
3. **Falta um `set`/`save`.** `update` não serve para a criação do PRIMEIRO registo (dá
   `NotFound`) — e `create_with` cria contentores novos com `store.save(...)`. Sem `set`
   o port não cobre a maior utilização que `delonix-linux` faz do store.
4. `impl Trait` na posição de argumento de um método do trait **não é object-safe** — só
   é chamável por bound genérico, nunca por `dyn StateRepository<T>`. Aceite como
   compromisso do sketch (o D9/P4a não precisa de um `dyn` a trocar em runtime), mas fica
   escrito para quem um dia precisar de um fake de teste por trás de um `dyn`.

O `repository.rs` da branch de spike já implementa o port corrigido nestes quatro
pontos, com `impl StateRepository<Container> for Store` e um `impl<T> StateRepository<T>
for JsonStore<T>` genérico (encaminhamento verbatim, sem lógica nova).

**3. Achado novo, fora do que o D6 tinha por certo: as cinco excepções NÃO são a mesma
forma de dívida.** Medido por leitura directa (não pelo texto do `arch_fitness.py`, que
generaliza a mesma frase para as cinco):

| Crate | O que usa hoje | Forma da dívida |
|---|---|---|
| `delonix-linux` | `delonix_state::Store<Container>` | Exactamente a forma que o D6 assume — `Store::update` chamado directamente; migra para o port sem mudar de mecanismo. |
| `delonix-vm` | `delonix_state::JsonStore<Vm>` | Mesma forma, outro tipo — o `impl<T> StateRepository<T> for JsonStore<T>` genérico já cobre. |
| `delonix-sdn` | **Lock `flock` PRÓPRIO** em `ipam.rs` (não é o `FileLock` do `delonix-state` — o próprio comentário do ficheiro di-lo: "`Store`'s `FileLock` degrades the same way, but it at least says so"), mais `delonix_state::write_atomic` cru em `infra.rs` (rotas/serviços) sem read-modify-write nenhum. | Uma dívida DIFERENTE: não é "chama o store directamente em vez do port", é "tem o seu próprio mecanismo de lock paralelo, nunca ligado ao do `delonix-state`". Migrar para `StateRepository<T>` aqui não é troca de chamada — é substituir um lock por outro, com o IPAM a decidir se aceita perder a mensagem de degradação que o seu próprio comentário valoriza. |
| `delonix-oci` | `delonix_state::write_atomic_mode`, **uma vez**, para a chave de assinatura ECDSA (`sign.rs::ensure_signing_key`) — criação única "se não existir", sem colecção de registos nenhuma. | Não há aqui NADA para o `StateRepository<T>` envolver — não existe um `T` que se liste, actualize ou remova. A excepção do `arch_fitness.py` para este crate está a citar a razão genérica de outras quatro; a razão real deste ficheiro é outra (um TOCTOU de `chmod`, já fechado, não uma falta de port). |
| `delonix-volume` | `VolumeStore` próprio (`lib.rs`), sobre `delonix_state::write_atomic` cru — sem `FileLock` visível nem no ficheiro nem nos usos grepados. | Read-modify-write SEM lock nenhum, que o `StateRepository<T>` não corrige por si — dar-lhe o port sem primeiro lhe dar o `flock` seria vestir a dívida de segurança, não fechá-la. |

Ou seja: das cinco, **duas** (`delonix-linux`, `delonix-vm`) migram directamente pelo
sketch corrigido. As outras **três** precisam de uma decisão à parte cada — não é
mecânico, e tratá-las como se fosse (a leitura que o D6/D9 faziam antes deste spike)
subestima o P4a real.

## O que isto muda no ADR-0044

- **D6 e o item de spike nº4** podem passar a "medido" só para a parte
  `delonix-linux`/`delonix-vm` (`Store<Container>`/`JsonStore<T>` genérico) — a prova
  em cima.
- **`delonix-sdn`/`delonix-oci`/`delonix-volume` precisam de uma frase própria em D6**,
  não da mesma linha genérica repetida cinco vezes — e possivelmente de uma fatia
  própria em D9 (P4a fica maior do que "cinco chamadas trocadas por um trait").
- Nenhuma das três é bloqueio para P4b–P4e (que não dependem de D6) — só para o
  `arch_fitness.py` remover as SUAS três excepções correspondentes.

## Proven vs not validated

**Proven** (medido nesta sessão, contra `origin/main` `d16b2f69`): a garantia de
`Store::update` sobrevive à porta e vem mesmo do `flock` (prova por reversão); as quatro
divergências do sketch do D6; a forma real de persistência de `delonix-sdn`/
`delonix-oci`/`delonix-volume`, por leitura directa do código, não pela frase do
`arch_fitness.py`.

**Not validated:** o mesmo teste de concorrência através de `JsonStore<T>` (a instância
genérica só foi verificada por leitura — encaminha verbatim para `JsonStore::update`,
que já tem prova própria, mas nunca correu através do port); qualquer decisão sobre COMO
`delonix-sdn`/`delonix-oci`/`delonix-volume` adoptam (ou não) o port — fica para quem
escrever a fatia de D6/D9 que os cobre.

## O que aconteceu ao código do spike

Marcado `// SPIKE ... not for merge as-is` no próprio módulo, na tradição do
`53_P1B_LAUNCHER_SPIKE` (código de spike não fica em `main` — o que fica é este
relatório + a evidência ao lado). O `repository.rs` fica como
`repository.rs.snapshot` neste directório; a branch `spike/p4-state-repository` que o
produziu não é fundida.
