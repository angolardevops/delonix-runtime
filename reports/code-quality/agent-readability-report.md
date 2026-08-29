# Legibilidade por agentes — Delonix Runtime (`cebf895`)

**Nota: 88/100** (§84)

## O que um agente consegue responder sem engenharia inversa

| Pergunta (§85) | Resposta disponível? | Onde |
|---|---|---|
| O que faz este crate? | **sim** | `//!` em 13/13 `lib.rs` |
| Que crates existem e o que fazem? | **sim, e é testado** | `ARCHITECTURE.md` + `AGENTS.md`, verificados por `tests/architecture.rs` |
| Que abstracção de provider uso para VMs? | **sim** | `VmBackend`, com 3 adaptadores e 5 fakes |
| …para rede / armazenamento / imagem? | **não** | não existe abstracção (ARCH-0001) |
| Que decisão fixou este desenho? | **sim** | `docs/adr/`, citado de dentro do código (`0008-proxmox-vm-backend.md`) |
| Que invariante de segurança se aplica aqui? | **parcialmente** | onde há SAFETY é bom; 58 `unsafe` sem ele (DOC-0003) |
| Que teste prova este caminho? | **sim** | nomes de teste descrevem comportamento (§59), ainda que em PT |

## O que puxa a nota para cima

**Os testes de arquitectura.** `crates/delonix-runtime-bin/tests/architecture.rs`
falha o build se `ARCHITECTURE.md` ou `AGENTS.md` deixarem de nomear um crate que
existe, ou se a contagem declarada divergir da real. É a diferença entre
documentação de arquitectura que se acredita e documentação que se verifica, e
é raro. Sem isto a nota seria ~78.

**Comentários que explicam porquê com o custo medido ao lado.** Um agente que
leia `delonix-vm/src/lib.rs:267` fica a saber não só o que a função faz, mas que
uma cópia privada dela já causou um desvio silencioso de 2 GiB vs 1 GiB entre
backends. Isso previne a repetição do erro; um comentário de sintaxe não.

**Mensagens de erro que ensinam.** `unknown_backend("proxmox")` não diz
«unknown backend» — diz que o backend existe, o que falta configurar, e em que
ADR está decidido.

## O que puxa a nota para baixo

1. **3 453 comentários em português** sem que o `AGENTS.md` o diga. Um agente que
   trabalhe em inglês encontra a maioria das explicações de invariantes numa
   língua que não é a do resto do contrato. Custo de correcção: uma linha.
2. **28 enums públicos sem doc** (DOC-0001) — o agente tem de reconstruir a
   semântica de cada variante a partir dos `match`.
3. **Ausência de porto de rede/armazenamento** (ARCH-0001): à pergunta «onde
   implemento isto?» a resposta é «dentro de um ficheiro de 8 859 linhas», que
   não é resposta.
4. **`container_init` com 37 parâmetros** (ARCH-0002): a assinatura mais
   importante do motor é a menos legível, e os `bool` são intermutáveis à vista
   do compilador.
