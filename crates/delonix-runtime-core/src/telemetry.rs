//! Fundação de observabilidade do Delonix Runtime (C1, fatia 1): logging
//! ESTRUTURADO via `tracing`, para substituir os `eprintln!`/`println!` ad-hoc
//! espalhados pelo motor. É o alicerce de que tudo o resto (spans, métricas,
//! OpenTelemetry) depende — sem isto, cada feature nova nasce sem instrumentação.
//!
//! Um runtime de containers a competir com containerd/CRI-O tem de emitir logs
//! que um SRE consiga filtrar e correlacionar a milhares de nós; `println` num
//! terminal não chega. Esta fatia liga só o subscriber; a migração dos
//! `eprintln!` faz-se incrementalmente por cima desta base.

use std::sync::atomic::{AtomicBool, Ordering};

static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Instala o subscriber global de `tracing` a partir do ambiente. **Idempotente**
/// e à prova de concorrência: chamar duas vezes (ou de dois binários no mesmo
/// processo de teste) não entra em pânico — a segunda chamada é um no-op.
///
/// Controlo por ambiente (nada de flags novas na CLI):
/// - **`DELONIX_LOG`** (ou `RUST_LOG` como alternativa) — filtro por nível/alvo,
///   sintaxe `env_filter` (ex.: `info`, `delonix_net=debug,warn`). Default `info`.
/// - **`DELONIX_LOG_FORMAT=json`** — emite JSON estruturado (uma linha por evento,
///   para ingestão por Loki/ELK); qualquer outro valor (ou ausente) → texto legível.
///
/// Deve ser chamada UMA vez, no arranque de cada binário (`delonix`, `delonix-cri`).
pub fn init() {
    // Evita reinstalar (e o pânico do `set_global_default` já-definido) quando dois
    // caminhos chamam `init` — o `try_init` já falha em silêncio, mas o guard torna
    // a intenção explícita e evita o custo de reconstruir o filtro.
    if INITIALISED.swap(true, Ordering::SeqCst) {
        return;
    }

    use tracing_subscriber::{fmt, EnvFilter};

    // Prioridade: DELONIX_LOG > RUST_LOG > "info". `env` (não `env_or`) para poder
    // cair no RUST_LOG quando DELONIX_LOG não está definido.
    let filter = EnvFilter::try_from_env("DELONIX_LOG")
        .or_else(|_| EnvFilter::try_from_default_env()) // RUST_LOG
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let json = std::env::var("DELONIX_LOG_FORMAT").ok().as_deref() == Some("json");

    // Logs vão para stderr (stdout é reservado ao OUTPUT do utilizador — tabelas
    // `ls`, `describe` — que NÃO deve ser poluído por linhas de log).
    let builder = fmt().with_env_filter(filter).with_writer(std::io::stderr);
    if json {
        let _ = builder.json().try_init();
    } else {
        let _ = builder.try_init();
    }
}
