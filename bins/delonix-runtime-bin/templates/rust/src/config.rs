//! Loads the service configuration from the environment and validates it once,
//! at startup. A bad value stops the process before it listens, with every
//! problem listed at once.

use std::time::Duration;

/// Where the process runs. Production turns the dev shortcuts off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Environment {
    Development,
    Test,
    Production,
}

impl Environment {
    pub fn as_str(&self) -> &'static str {
        match self {
            Environment::Development => "development",
            Environment::Test => "test",
            Environment::Production => "production",
        }
    }
}

/// The whole runtime configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub service_name: String,
    pub version: String,
    pub env: Environment,
    pub log_level: String,

    pub host: String,
    pub port: u16,
    pub max_body_bytes: usize,
    pub shutdown_timeout: Duration,
    /// Keeps serving (with readiness at 503) before the listener closes, so a
    /// load balancer has time to stop routing here.
    pub drain_delay: Duration,

    /// Enables `POST /api/v1/webhooks/inbound`. `None`: the endpoint answers 404.
    pub inbound_webhook_secret: Option<String>,
    /// Receives a signed event per created note. `None`: nothing is sent.
    pub outbound_webhook_url: Option<String>,
    pub outbound_webhook_secret: Option<String>,

    /// Redis cache. `None`: an in-process cache (local runs need no Redis).
    pub redis_url: Option<String>,
    pub cache_ttl: Duration,
    /// RabbitMQ async messaging. `None`: messaging off.
    pub rabbitmq_url: Option<String>,
}

impl Config {
    /// The address the server binds.
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Reads the configuration from `get` (`std::env::var` outside tests). `version`
/// is the build's own version, not something the environment may override.
/// Returns every problem at once, so a misconfiguration is fixed in one pass.
pub fn load(
    get: impl Fn(&str) -> Option<String>,
    version: &str,
) -> Result<Config, Vec<String>> {
    let mut errs: Vec<String> = Vec::new();
    let s = |key: &str, def: &str| -> String {
        match get(key) {
            Some(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => def.to_string(),
        }
    };
    let opt = |key: &str| -> Option<String> {
        get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    };

    let env = match s("APP_ENV", "development").as_str() {
        "development" => Environment::Development,
        "test" => Environment::Test,
        "production" => Environment::Production,
        other => {
            errs.push(format!(
                "APP_ENV: {other:?} is not one of development, test, production"
            ));
            Environment::Development
        }
    };

    let log_level = s("LOG_LEVEL", "info").to_lowercase();
    if !matches!(log_level.as_str(), "debug" | "info" | "warn" | "warning" | "error") {
        errs.push(format!(
            "LOG_LEVEL: {log_level:?} is not one of debug, info, warn, error"
        ));
    }

    let port = {
        let raw = s("PORT", "__PORT__");
        match raw.parse::<u16>() {
            Ok(p) if p >= 1 => p,
            _ => {
                errs.push(format!("PORT: {raw:?} is not a port number (1-65535)"));
                0
            }
        }
    };

    let mut dur = |key: &str, def: Duration| -> Duration {
        match opt(key) {
            None => def,
            Some(raw) => match parse_duration(&raw) {
                Some(d) => d,
                None => {
                    errs.push(format!(
                        "{key}: {raw:?} is not a non-negative duration (e.g. 15s)"
                    ));
                    def
                }
            },
        }
    };

    let shutdown_timeout = dur("SHUTDOWN_TIMEOUT", Duration::from_secs(15));
    let drain_delay = dur("DRAIN_DELAY", Duration::from_secs(0));
    let cache_ttl = dur("CACHE_TTL", Duration::from_secs(300));

    let max_body_bytes = match opt("HTTP_MAX_BODY_BYTES") {
        None => 1 << 20,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => {
                errs.push(format!("HTTP_MAX_BODY_BYTES: {raw:?} is not a positive integer"));
                1 << 20
            }
        },
    };

    let outbound_webhook_url = opt("WEBHOOK_TARGET_URL");
    let outbound_webhook_secret = opt("WEBHOOK_TARGET_SECRET");
    if let Some(url) = &outbound_webhook_url {
        let https = url.starts_with("https://");
        if !https && !url.starts_with("http://") {
            errs.push(format!("WEBHOOK_TARGET_URL: {url:?} is not an http(s) URL"));
        } else if env == Environment::Production && !https {
            errs.push("WEBHOOK_TARGET_URL: production sends signed events over https only".into());
        }
        if outbound_webhook_secret.is_none() {
            errs.push("WEBHOOK_TARGET_SECRET: required when WEBHOOK_TARGET_URL is set".into());
        }
    }

    if errs.is_empty() {
        Ok(Config {
            service_name: s("SERVICE_NAME", "__NAME__"),
            version: version.to_string(),
            env,
            log_level,
            host: s("HTTP_HOST", "0.0.0.0"),
            port,
            max_body_bytes,
            shutdown_timeout,
            drain_delay,
            inbound_webhook_secret: opt("WEBHOOK_INBOUND_SECRET"),
            outbound_webhook_url,
            outbound_webhook_secret,
            redis_url: opt("REDIS_URL"),
            cache_ttl,
            rabbitmq_url: opt("RABBITMQ_URL"),
        })
    } else {
        Err(errs)
    }
}

/// Parses `15`, `15s`, `500ms`, `2m` or `1h` into a `Duration`. A bare number is
/// seconds. Returns `None` on anything else (including a negative value).
pub fn parse_duration(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (num, unit) = if let Some(n) = raw.strip_suffix("ms") {
        (n, "ms")
    } else if let Some(n) = raw.strip_suffix('s') {
        (n, "s")
    } else if let Some(n) = raw.strip_suffix('m') {
        (n, "m")
    } else if let Some(n) = raw.strip_suffix('h') {
        (n, "h")
    } else {
        (raw, "s")
    };
    let value: f64 = num.trim().parse().ok()?;
    if value < 0.0 || !value.is_finite() {
        return None;
    }
    let secs = match unit {
        "ms" => value / 1000.0,
        "s" => value,
        "m" => value * 60.0,
        "h" => value * 3600.0,
        _ => return None,
    };
    Some(Duration::from_secs_f64(secs))
}
