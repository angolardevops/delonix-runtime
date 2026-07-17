//! `delonix secret` — cofre de segredos do runtime (Secret Manager, estilo
//! docker/k8s). Wrapper fino sobre `delonix_runtime_core::SecretStore`, que já
//! cifra em repouso (XChaCha20-Poly1305 sob uma chave-mestra local).
//!
//! É o produtor dos segredos que o `container run --secret <nome>` consome.
//! **Os valores nunca são impressos** por omissão (`inspect` redige-os; `--reveal`
//! é opt-in explícito) — um `secret` é rotineiramente colado em issues/chats.

use std::path::PathBuf;

use clap::Subcommand;
use delonix_runtime_core::secret::{parse_env_file, valid_name};
use delonix_runtime_core::{Error, Result, Secret, SecretStore};

use super::output;
use super::util::state_root;

#[derive(Subcommand)]
pub enum SecretCmd {
    /// Cria/substitui um segredo a partir de literais e/ou um ficheiro `.env`.
    Create {
        name: String,
        /// Par `KEY=value`. Repetível.
        #[arg(long = "from-literal")]
        from_literal: Vec<String>,
        /// Carrega linhas `KEY=value` de um ficheiro (ex.: `.env`).
        #[arg(long = "from-env-file")]
        from_env_file: Option<PathBuf>,
    },
    /// Lista os segredos (nome + nº de chaves; valores NUNCA mostrados).
    Ls,
    /// Mostra as chaves de um segredo (valores redigidos, salvo `--reveal`).
    Inspect {
        name: String,
        /// Revela os VALORES em claro (perigoso — evita em terminais partilhados).
        #[arg(long)]
        reveal: bool,
    },
    /// Define/actualiza chaves num segredo (cria-o se não existir).
    Set {
        name: String,
        /// Pares `KEY=value`.
        pairs: Vec<String>,
    },
    /// Remove uma chave de um segredo (ou o segredo todo com `--all`).
    Unset {
        name: String,
        key: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Remove um segredo.
    Rm { name: String },
    /// Roda a chave-mestra do host: re-cifra TODOS os segredos com uma chave
    /// nova. Os valores são preservados.
    RotateKey,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Separa `KEY=value` (o `=` do PRIMEIRO sinal; o valor pode conter `=`).
fn parse_kv(s: &str) -> Option<(String, String)> {
    let (k, v) = s.split_once('=')?;
    if k.is_empty() {
        return None;
    }
    Some((k.to_string(), v.to_string()))
}

pub fn run(action: SecretCmd) -> Result<()> {
    let mut store = SecretStore::open(state_root())?;
    match action {
        SecretCmd::Create { name, from_literal, from_env_file } => {
            if !valid_name(&name) {
                return Err(Error::Invalid(format!("nome de segredo inválido: {name:?}")));
            }
            let mut data = std::collections::BTreeMap::new();
            if let Some(f) = from_env_file {
                let content = std::fs::read_to_string(&f).map_err(|e| Error::Invalid(format!("--from-env-file {}: {e}", f.display())))?;
                data.extend(parse_env_file(&content));
            }
            for lit in &from_literal {
                let (k, v) = parse_kv(lit).ok_or_else(|| Error::Invalid(format!("--from-literal inválido: {lit:?} (usa KEY=value)")))?;
                data.insert(k, v);
            }
            if data.is_empty() {
                return Err(Error::Invalid("segredo vazio — usa --from-literal KEY=value e/ou --from-env-file".into()));
            }
            let n = data.len();
            store.save(&Secret { name: name.clone(), data, updated_unix: now_unix() })?;
            println!("segredo '{name}' criado ({n} chave(s))");
        }
        SecretCmd::Ls => {
            let mut t = output::Table::new(&["NAME", "KEYS", "NAMES"]).right_align(1);
            for s in store.list() {
                let keys: Vec<&str> = s.data.keys().map(String::as_str).collect();
                t.row(vec![s.name.clone(), s.data.len().to_string(), keys.join(", ")]);
            }
            t.print();
        }
        SecretCmd::Inspect { name, reveal } => {
            let s = store.load(&name)?;
            println!("Name:  {}", s.name);
            for (k, v) in &s.data {
                // Redacção por omissão — o valor só sai com --reveal explícito.
                println!("  {k}={}", if reveal { v.clone() } else { "••••••".into() });
            }
            if !reveal && !s.data.is_empty() {
                println!("{}", output::secundario("(valores ocultos — usa --reveal para os mostrar)"));
            }
        }
        SecretCmd::Set { name, pairs } => {
            if pairs.is_empty() {
                return Err(Error::Invalid("indica pelo menos um KEY=value".into()));
            }
            let mut s = store.load(&name).unwrap_or_else(|_| Secret { name: name.clone(), ..Default::default() });
            s.name = name.clone();
            for p in &pairs {
                let (k, v) = parse_kv(p).ok_or_else(|| Error::Invalid(format!("par inválido: {p:?} (usa KEY=value)")))?;
                s.data.insert(k, v);
            }
            s.updated_unix = now_unix();
            store.save(&s)?;
            println!("segredo '{name}' actualizado ({} chave(s))", s.data.len());
        }
        SecretCmd::Unset { name, key, all } => {
            if all {
                store.remove(&name)?;
                println!("segredo '{name}' removido");
                return Ok(());
            }
            let k = key.ok_or_else(|| Error::Invalid("indica a chave a remover (ou --all)".into()))?;
            let mut s = store.load(&name)?;
            if s.data.remove(&k).is_none() {
                return Err(Error::Invalid(format!("chave '{k}' não existe em '{name}'")));
            }
            s.updated_unix = now_unix();
            store.save(&s)?;
            println!("chave '{k}' removida de '{name}'");
        }
        SecretCmd::Rm { name } => {
            store.remove(&name)?;
            println!("segredo '{name}' removido");
        }
        SecretCmd::RotateKey => {
            store.rotate_key()?;
            println!("chave-mestra rodada — todos os segredos re-cifrados com a nova chave");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_kv;

    #[test]
    fn parse_kv_corta_no_primeiro_igual() {
        assert_eq!(parse_kv("K=v"), Some(("K".into(), "v".into())));
        // O valor pode conter '=' (ex.: um token base64 com padding).
        assert_eq!(parse_kv("TOKEN=ab==cd"), Some(("TOKEN".into(), "ab==cd".into())));
        // Chave vazia não é válida.
        assert_eq!(parse_kv("=v"), None);
        assert_eq!(parse_kv("semigual"), None);
    }
}
