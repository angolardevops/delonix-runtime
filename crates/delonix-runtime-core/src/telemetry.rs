//! Fundação de observabilidade do Delonix Runtime (C1): logging ESTRUTURADO via
//! `tracing` (fatia 1) + **spans distribuídos via OpenTelemetry/OTLP** (fatia 3,
//! esta), para substituir os `eprintln!`/`println!` ad-hoc espalhados pelo motor
//! e dar a um SRE traces correlacionáveis a milhares de nós.
//!
//! Um runtime de containers a competir com containerd/CRI-O tem de emitir logs e
//! traces que um SRE consiga filtrar e correlacionar; `println` num terminal não
//! chega. Esta camada liga o subscriber `fmt` (sempre) e, quando pedido, uma
//! layer OTLP que exporta os spans do `tracing` para um collector OpenTelemetry.
//!
//! ## Nuance honesta: exportador em batch vs. CLI síncrona
//!
//! O exportador OTLP corre num **modelo de batch**: as spans são bufferizadas e
//! despachadas periodicamente. Escolhemos deliberadamente o `BatchSpanProcessor`
//! do `opentelemetry_sdk`, que corre numa **thread dedicada** (`futures_executor::
//! block_on`) e um cliente HTTP **bloqueante** (`reqwest/blocking`) — logo **não
//! precisa de um runtime tokio ambiente** no momento da instalação. Isto é o que
//! permite ligar OTLP tanto no servidor `delonix-cri` (que tem tokio) como na CLI
//! `delonix` (síncrona, sem tokio) sem fingir nada.
//!
//! **A limitação real fica na CLI síncrona**: o batch só despacha ao fim de
//! `scheduled_delay` (~5s) ou num `force_flush`/`shutdown` explícito. A CLI
//! `delonix` é de vida curta e os seus `main`/entry-points **não** chamam
//! `shutdown()` (ficam fora do território desta camada), por isso uma invocação
//! rápida pode terminar ANTES do primeiro flush e perder as spans. O caminho onde
//! OTLP entrega de forma fiável é o **servidor `delonix-cri`** (processo de longa
//! duração — o timer do batch flusha em ciclo enquanto o processo vive). Não se
//! promete o contrário para a CLI síncrona.
//!
//! ## Transporte
//!
//! OTLP/HTTP protobuf, cliente `reqwest` **sem backend de TLS** (endpoint em texto
//! simples, típico de um collector local). É uma decisão de supply-chain: evita
//! arrastar `aws-lc-sys`/`openssl`/`native-tls` (C + licença OpenSSL) para um
//! runtime público mínimo. Um endpoint `https://` não é suportado por desenho.

use std::sync::atomic::{AtomicBool, Ordering};

static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Mantém o `TracerProvider` vivo durante todo o processo. Se fosse largado, o
/// `BatchSpanProcessor` desligava a thread de exportação e as spans deixavam de
/// sair. Guardado aqui (além do provider global) para o tempo de vida ser
/// explícito e independente do estado global do `opentelemetry`.
static OTLP_PROVIDER: std::sync::OnceLock<opentelemetry_sdk::trace::SdkTracerProvider> =
    std::sync::OnceLock::new();

/// Instala o subscriber global de `tracing` a partir do ambiente. **Idempotente**
/// e à prova de concorrência: chamar duas vezes (ou de dois binários no mesmo
/// processo de teste) não entra em pânico — a segunda chamada é um no-op.
///
/// Controlo por ambiente (nada de flags novas na CLI):
/// - **`DELONIX_LOG`** (ou `RUST_LOG` como alternativa) — filtro por nível/alvo,
///   sintaxe `env_filter` (ex.: `info`, `delonix_net=debug,warn`). Default `info`.
/// - **`DELONIX_LOG_FORMAT=json`** — emite JSON estruturado (uma linha por evento,
///   para ingestão por Loki/ELK); qualquer outro valor (ou ausente) → texto legível.
/// - **`DELONIX_OTLP_ENDPOINT`** — quando definido (e não vazio), adiciona uma layer
///   OTLP que exporta as spans para esse collector (ex.: `http://localhost:4318`).
///   O caminho `/v1/traces` é acrescentado se faltar. Ausente → só `fmt` (sem OTLP,
///   sem custo). Ver a nuance CLI-sync no topo do módulo.
///
/// Deve ser chamada UMA vez, no arranque de cada binário (`delonix`, `delonix-cri`).
pub fn init() {
    // Evita reinstalar (e o pânico do `set_global_default` já-definido) quando dois
    // caminhos chamam `init` — o `try_init` já falha em silêncio, mas o guard torna
    // a intenção explícita e evita o custo de reconstruir o filtro.
    if INITIALISED.swap(true, Ordering::SeqCst) {
        return;
    }

    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter, Layer};

    // Prioridade: DELONIX_LOG > RUST_LOG > "info". `env` (não `env_or`) para poder
    // cair no RUST_LOG quando DELONIX_LOG não está definido.
    let filter = EnvFilter::try_from_env("DELONIX_LOG")
        .or_else(|_| EnvFilter::try_from_default_env()) // RUST_LOG
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let json = std::env::var("DELONIX_LOG_FORMAT").ok().as_deref() == Some("json");

    // Logs vão para stderr (stdout é reservado ao OUTPUT do utilizador — tabelas
    // `ls`, `describe` — que NÃO deve ser poluído por linhas de log). Boxed para
    // unificar os dois tipos (json vs texto) num só `dyn Layer`.
    let fmt_layer = if json {
        fmt::layer().with_writer(std::io::stderr).json().boxed()
    } else {
        fmt::layer().with_writer(std::io::stderr).boxed()
    };

    // Layer OTLP OPCIONAL: `None` (sem `DELONIX_OTLP_ENDPOINT`) é um no-op — a
    // `Option<Layer>` implementa `Layer`. O `EnvFilter` aplica-se ao registry
    // inteiro, logo governa tanto o `fmt` como o OTLP.
    let otlp_layer = build_otlp_layer();

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otlp_layer)
        .try_init();
}

/// Constrói a layer OTLP se `DELONIX_OTLP_ENDPOINT` estiver definido; senão `None`.
///
/// Falha **suave**: se o endpoint for inválido ou o exportador não construir,
/// avisa via `tracing` (que já está a caminho de ser instalado — o aviso sai pelo
/// `fmt`) e devolve `None`. Nunca faz `panic` no arranque por causa de telemetria.
fn build_otlp_layer<S>() -> Option<Box<dyn tracing_subscriber::Layer<S> + Send + Sync>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a> + Send + Sync,
{
    let endpoint = std::env::var("DELONIX_OTLP_ENDPOINT").ok()?;
    if endpoint.trim().is_empty() {
        return None;
    }
    let endpoint = normalise_traces_endpoint(endpoint.trim());

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
    use tracing_subscriber::Layer as _;

    let exporter = match SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(&endpoint)
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = %e,
                endpoint = %endpoint,
                "OTLP: exportador não construiu — a seguir só com logs (fmt)"
            );
            return None;
        }
    };

    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name())
        .build();

    // `with_batch_exporter` usa o BatchSpanProcessor default: thread dedicada +
    // `block_on` (sem runtime tokio ambiente). Ver a nuance no topo do módulo.
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("delonix-runtime");

    // Mantém o provider vivo: no global do `opentelemetry` (macros) e no nosso
    // `OnceLock`. Como o `init()` inteiro é idempotente, não há corrida real aqui.
    opentelemetry::global::set_tracer_provider(provider.clone());
    let _ = OTLP_PROVIDER.set(provider);

    Some(tracing_opentelemetry::layer().with_tracer(tracer).boxed())
}

/// Nome do serviço para os `Resource` attributes das spans — distingue a CLI do
/// servidor CRI no collector. Deriva do nome do executável (`delonix` /
/// `delonix-cri`); `OTEL_SERVICE_NAME` tem prioridade (convenção OpenTelemetry).
fn service_name() -> String {
    if let Ok(name) = std::env::var("OTEL_SERVICE_NAME") {
        if !name.trim().is_empty() {
            return name;
        }
    }
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(std::path::Path::file_name)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "delonix-runtime".to_string())
}

/// Normaliza o endpoint para o caminho de spans OTLP/HTTP. Como passamos o valor
/// programaticamente (`with_endpoint`), o SDK usa-o VERBATIM (não acrescenta
/// `/v1/traces`); por isso acrescentamo-lo nós se faltar, para o utilizador poder
/// dar só a base do collector (`http://host:4318`). Função pura → testável.
fn normalise_traces_endpoint(endpoint: &str) -> String {
    const SIGNAL_PATH: &str = "/v1/traces";
    if endpoint.contains(SIGNAL_PATH) {
        endpoint.to_string()
    } else {
        format!("{}{}", endpoint.trim_end_matches('/'), SIGNAL_PATH)
    }
}

#[cfg(test)]
mod tests {
    use super::normalise_traces_endpoint;

    #[test]
    fn acrescenta_signal_path_a_base() {
        assert_eq!(
            normalise_traces_endpoint("http://localhost:4318"),
            "http://localhost:4318/v1/traces"
        );
    }

    #[test]
    fn tolera_barra_final_na_base() {
        assert_eq!(
            normalise_traces_endpoint("http://localhost:4318/"),
            "http://localhost:4318/v1/traces"
        );
    }

    #[test]
    fn preserva_endpoint_ja_completo() {
        assert_eq!(
            normalise_traces_endpoint("http://collector:4318/v1/traces"),
            "http://collector:4318/v1/traces"
        );
    }
}
