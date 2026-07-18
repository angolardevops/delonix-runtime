//! Construção de imagens: um `Dockerfile` mínimo (`FROM`/`RUN`/`CMD`) e a
//! captura do *diff* de uma construção como um novo *layer* content-addressed.

use crate::cas::strip;
use crate::image::{now_unix, Image, ImageConfig, ImageStore};
use delonix_runtime_core::{Error, Result};
use std::path::PathBuf;

/// Um passo do build, por ordem (a ordem importa: `COPY` antes do `RUN` que usa).
#[derive(Debug, Clone)]
pub enum Step {
    /// `RUN <cmd>` — executa um comando (com o `ENV`/`WORKDIR` acumulados).
    Run(String),
    /// `COPY [--from=<stage>] <src> <dst>` — copia do *contexto* de build (ou de
    /// um estágio anterior, se `from` estiver presente — build multi-stage).
    Copy { src: String, dst: String, from: Option<String> },
    /// `ENV K=V` (ou `ENV K V`) — define uma variável (afecta os `RUN` seguintes).
    Env { key: String, val: String },
    /// `WORKDIR <dir>` — directório de trabalho dos `RUN` seguintes.
    Workdir(String),
}

/// Um estágio intermédio de um build multi-stage (`FROM x AS nome`).
#[derive(Debug, Clone)]
pub struct Stage {
    /// Nome do estágio (`AS <nome>`), ou `None`.
    pub name: Option<String>,
    /// A imagem base do estágio (`FROM`).
    pub from: String,
    /// Os passos do estágio.
    pub steps: Vec<Step>,
}

/// Um `Dockerfile` analisado — compatível com o Docker, com extensões Delonix.
/// Os campos `from`/`steps`/`cmd`/… descrevem o **estágio final** (a imagem
/// resultante); `stages` tem os estágios intermédios (build multi-stage).
#[derive(Debug, Default)]
pub struct Dockerfile {
    /// Estágios intermédios (todos menos o último), por ordem.
    pub stages: Vec<Stage>,
    /// A imagem base do estágio final (`FROM`).
    pub from: String,
    /// Os passos do estágio final, por ordem.
    pub steps: Vec<Step>,
    /// O comando por omissão (`CMD`).
    pub cmd: Vec<String>,
    /// O ponto de entrada (`ENTRYPOINT`).
    pub entrypoint: Vec<String>,
    /// O `ENV` acumulado (para o config da imagem).
    pub env: Vec<String>,
    /// O `WORKDIR` final (para o config da imagem).
    pub workdir: Option<String>,
    // --- extensões Delonix (que o Dockerfile NÃO tem) ---
    /// `SCAN fail-on=<sev>` — porta de vulnerabilidades antes do build.
    pub scan_fail_on: Option<String>,
    /// `CPUS <n>` — limite de CPU embebido na imagem (obrigatório no Delonix).
    pub cpus: Option<String>,
    /// `MEMORY <n>` — limite de memória embebido na imagem.
    pub memory: Option<String>,
    /// `SECURITY <opção>...` — postura de segurança por omissão (ex.: `userns`).
    pub security: Vec<String>,
    /// `HEALTHCHECK ... CMD <cmd>` — comando de saúde (a parte após `CMD`).
    pub healthcheck: Option<String>,
}

/// Analisa um Dockerfile. **Compatível com o Docker** (FROM/RUN/CMD/ENTRYPOINT/
/// ENV/WORKDIR/COPY; LABEL/EXPOSE/USER/ARG/ADD/MAINTAINER/VOLUME aceites e
/// ignorados) **mais extensões Delonix** (SCAN/CPUS/MEMORY/SECURITY).
/// Junta linhas físicas que terminam em `\` numa só linha lógica (continuações,
/// como o Docker) — devolve `(índice 0-based da 1.ª linha física, linha lógica)`.
/// Uma linha de continuação é concatenada com um espaço no lugar do `\<newline>`.
fn join_continuations(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0usize;
    for (i, raw) in text.lines().enumerate() {
        if cur.is_empty() {
            start = i;
        }
        let t = raw.trim_end();
        if let Some(head) = t.strip_suffix('\\') {
            cur.push_str(head);
            cur.push(' ');
        } else {
            cur.push_str(t);
            out.push((start, std::mem::take(&mut cur)));
        }
    }
    if !cur.is_empty() {
        out.push((start, cur)); // último `\` sem linha a seguir — não perder o conteúdo
    }
    out
}

pub fn parse_dockerfile(text: &str) -> Result<Dockerfile> {
    let mut df = Dockerfile::default();
    let mut stages: Vec<Stage> = Vec::new(); // todos os estágios, na ordem
    for (n, line) in join_continuations(text) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (instr, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let instr_up = instr.to_ascii_uppercase();
        // Os passos vão para o ESTÁGIO actual (o último de `stages`); FROM abre um novo.
        if instr_up != "FROM" && stages.is_empty() {
            // permite extensões/metadados antes do 1.º FROM serem ignorados? Não:
            // qualquer passo antes de FROM é erro (como no Docker).
            return Err(Error::Invalid(format!(
                "Dockerfile linha {}: `{instr}` antes de qualquer FROM",
                n + 1
            )));
        }
        match instr_up.as_str() {
            "FROM" => {
                // `FROM <img> [AS <nome>]`
                let mut it = rest.split_whitespace();
                let from = it.next().unwrap_or("").to_string();
                let name = match (it.next(), it.next()) {
                    (Some(kw), Some(nm)) if kw.eq_ignore_ascii_case("as") => Some(nm.to_string()),
                    _ => None,
                };
                stages.push(Stage { name, from, steps: Vec::new() });
            }
            "RUN" => stages.last_mut().unwrap().steps.push(Step::Run(rest.to_string())),
            "CMD" => df.cmd = parse_cmd(rest),
            "ENTRYPOINT" => df.entrypoint = parse_cmd(rest),
            "ENV" => {
                // `ENV k1=v1 k2="v 2" …` (múltiplas vars) OU o legado `ENV k v`.
                for (key, val) in parse_env_pairs(rest) {
                    stages.last_mut().unwrap().steps.push(Step::Env { key, val });
                }
            }
            "WORKDIR" => {
                stages.last_mut().unwrap().steps.push(Step::Workdir(rest.to_string()));
            }
            "COPY" | "ADD" => {
                // `COPY [--from=<estágio>] <src> <dst>`
                let mut from_stage: Option<String> = None;
                let parts: Vec<&str> = rest
                    .split_whitespace()
                    .filter(|t| {
                        if let Some(f) = t.strip_prefix("--from=") {
                            from_stage = Some(f.to_string());
                            false
                        } else {
                            true
                        }
                    })
                    .collect();
                if parts.len() < 2 {
                    return Err(Error::Invalid(format!("linha {}: {instr} precisa de src e dst", n + 1)));
                }
                stages.last_mut().unwrap().steps.push(Step::Copy {
                    src: parts[0].to_string(),
                    dst: parts[parts.len() - 1].to_string(),
                    from: from_stage,
                });
            }
            // --- extensões Delonix (aplicam-se à imagem final) ---
            "SCAN" => {
                df.scan_fail_on = rest
                    .split_whitespace()
                    .find_map(|t| t.strip_prefix("fail-on=").map(|s| s.to_string()))
                    .or(Some("high".to_string()));
            }
            "CPUS" => df.cpus = Some(rest.to_string()),
            "MEMORY" => df.memory = Some(rest.to_string()),
            "SECURITY" => df.security = rest.split_whitespace().map(|s| s.to_string()).collect(),
            // HEALTHCHECK [opções] CMD <cmd> | HEALTHCHECK NONE (A17).
            "HEALTHCHECK" => {
                let r = rest.trim();
                if r.eq_ignore_ascii_case("NONE") {
                    df.healthcheck = None;
                } else if let Some(idx) = r.find("CMD ").or_else(|| r.find("cmd ")) {
                    df.healthcheck = Some(r[idx + 4..].trim().to_string());
                }
            }
            // compatibilidade: aceites mas sem efeito de build (metadados)
            "LABEL" | "EXPOSE" | "USER" | "ARG" | "MAINTAINER" | "VOLUME" | "STOPSIGNAL"
            | "SHELL" | "ONBUILD" => {}
            other => {
                return Err(Error::Invalid(format!(
                    "Dockerfile linha {}: instrução desconhecida `{other}`",
                    n + 1
                )))
            }
        }
    }
    // O último estágio é o final (a imagem resultante); os anteriores são intermédios.
    let last = stages.pop().ok_or_else(|| Error::Invalid("Dockerfile sem instrução FROM".into()))?;
    df.from = last.from;
    df.steps = last.steps;
    df.stages = stages;
    // ENV/WORKDIR do estágio final → config da imagem.
    for s in &df.steps {
        match s {
            Step::Env { key, val } => df.env.push(format!("{key}={val}")),
            Step::Workdir(d) => df.workdir = Some(d.clone()),
            _ => {}
        }
    }
    Ok(df)
}

/// `ENV K=V` ou `ENV K V` → (chave, valor).
/// Faz o parse de um `ENV` num ou mais pares `(chave, valor)`, à Docker:
/// - **legado** `ENV chave valor com espaços` (sem `=`): um só par, o resto é o valor.
/// - **multi-var** `ENV k1=v1 k2="v 2" k3=v3`: tokeniza por espaços RESPEITANDO
///   aspas (para valores com espaços), cada token parte no 1.º `=`.
fn parse_env_pairs(rest: &str) -> Vec<(String, String)> {
    let rest = rest.trim();
    if !rest.contains('=') {
        return match rest.split_once(char::is_whitespace) {
            Some((k, v)) => vec![(k.trim().to_string(), v.trim().to_string())],
            None => vec![(rest.to_string(), String::new())],
        };
    }
    let mut out = Vec::new();
    let mut tok = String::new();
    let mut in_quote = false;
    let mut push = |t: &mut String| {
        if let Some((k, v)) = t.split_once('=') {
            out.push((k.trim().to_string(), v.to_string()));
        }
        t.clear();
    };
    for ch in rest.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !tok.is_empty() {
                    push(&mut tok);
                }
            }
            c => tok.push(c),
        }
    }
    if !tok.is_empty() {
        push(&mut tok);
    }
    out
}

/// `CMD ["a","b"]` (JSON) ou `CMD a b` (shell) → vector de argumentos.
fn parse_cmd(rest: &str) -> Vec<String> {
    if rest.starts_with('[') {
        serde_json::from_str::<Vec<String>>(rest).unwrap_or_default()
    } else {
        rest.split_whitespace().map(|s| s.to_string()).collect()
    }
}

/// A arquitectura no vocabulário OCI (`amd64`, `arm64`, ...).
fn oci_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    }
}

impl ImageStore {
    /// Os `diff_ids` (digests NÃO-comprimidos) dos layers da base, lidos do seu
    /// config ORIGINAL no CAS (uma imagem puxada traz-nos correctos). Recorre aos
    /// digests guardados se o config não os tiver (ex.: base `scratch`).
    fn base_diff_ids(&self, base: &Image) -> Vec<String> {
        if let Ok(bytes) = self.cas().read(&base.id) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(ids) = v
                    .get("rootfs")
                    .and_then(|r| r.get("diff_ids"))
                    .and_then(|d| d.as_array())
                {
                    let out: Vec<String> = ids
                        .iter()
                        .filter_map(|x| x.as_str().map(|s| format!("sha256:{}", strip(s))))
                        .collect();
                    if out.len() == base.layers.len() {
                        return out;
                    }
                }
            }
        }
        base.layers.iter().map(|l| format!("sha256:{}", strip(l))).collect()
    }

    /// Empacota o `upperdir` de um container de construção como tar -> CAS.
    pub fn commit_upper(&self, container_id: &str) -> Result<String> {
        let upper: PathBuf = self
            .root()
            .join("containers")
            .join(container_id)
            .join("upper");
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            builder
                .append_dir_all(".", &upper)
                .map_err(|e| Error::Invalid(format!("falha a empacotar o diff: {e}")))?;
            builder
                .finish()
                .map_err(|e| Error::Invalid(format!("falha a fechar o tar: {e}")))?;
        }
        self.cas().write(&buf)
    }

    /// Cria a imagem final: layers(base) + novo layer, com o config derivado do
    /// Dockerfile (Cmd, Env e — extensões Delonix — limites de CPU/memória).
    pub fn build_image(
        &self,
        base: &Image,
        new_layer: String,
        df: &Dockerfile,
        tag: &str,
    ) -> Result<Image> {
        let mut layers = base.layers.clone();
        layers.push(new_layer);
        let cmd = if df.cmd.is_empty() { base.config.cmd.clone() } else { df.cmd.clone() };
        let entrypoint = if df.entrypoint.is_empty() {
            base.config.entrypoint.clone()
        } else {
            df.entrypoint.clone()
        };
        // Env = o da base + o do Dockerfile.
        let mut env = base.config.env.clone();
        env.extend(df.env.iter().cloned());
        // herda os limites da base se o Dockerfile não os redefinir.
        let cpus = df.cpus.clone().or_else(|| base.config.cpus.clone());
        let memory = df.memory.clone().or_else(|| base.config.memory.clone());

        let created = now_unix();
        // `diff_ids` VÁLIDOS p/ Docker/OCI: os layers da base vêm do config
        // ORIGINAL da base (digests NÃO-comprimidos, que o Docker valida ao
        // descomprimir cada blob); o novo layer é um tar não-comprimido, por isso
        // o seu diff_id = o próprio digest. Assim uma imagem construída pelo
        // Delonix é puxável pelo Docker (não só pelo Delonix). Ver A1 (push).
        let mut diff_ids: Vec<String> = self.base_diff_ids(base);
        diff_ids.push(format!("sha256:{}", strip(layers.last().unwrap())));
        let security = if df.security.is_empty() {
            base.config.security.clone()
        } else {
            df.security.clone()
        };
        let healthcheck = df.healthcheck.clone().or_else(|| base.config.healthcheck.clone());
        let workdir = df.workdir.clone().unwrap_or_default();
        let config_json = serde_json::json!({
            // Campos standard do config de imagem OCI/Docker (interop).
            "architecture": oci_arch(),
            "os": "linux",
            "config": { "Cmd": cmd, "Entrypoint": entrypoint, "Env": env, "Cpus": cpus, "Memory": memory, "Security": security },
            "rootfs": { "type": "layers", "diff_ids": diff_ids },
            // Extensão Delonix (ignorada pelo Docker):
            "created_unix": created,
        });
        let id = self.cas().write(&serde_json::to_vec(&config_json)?)?;

        let repo_tags = self.merged_tags(&id, tag);
        let img = Image {
            id,
            repo_tags,
            layers,
            config: ImageConfig { cmd, entrypoint, env, cpus, memory, security, healthcheck, user: String::new(), working_dir: workdir.clone() },
            created_unix: created,
        };
        self.enforce_tag_uniqueness(&img)?;
        self.save(&img)?;
        Ok(img)
    }

    /// Cria uma imagem a partir de um rootfs FLAT (modo *rootless*/vfs): empacota
    /// TODO o directório como **um único layer** (squash) — não há overlay, logo
    /// não há diff. Config OCI válido (1 diff_id). Usado pelo `build` rootless.
    pub fn commit_flat_rootfs(
        &self,
        rootfs: &std::path::Path,
        cmd: Vec<String>,
        env: Vec<String>,
        workdir: String,
        tag: &str,
    ) -> Result<Image> {
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            b.follow_symlinks(false);
            b.append_dir_all(".", rootfs)
                .map_err(|e| Error::Invalid(format!("empacotar rootfs: {e}")))?;
            b.finish().map_err(|e| Error::Invalid(format!("fechar tar: {e}")))?;
        }
        self.commit_flat_rootfs_from_tar(buf, cmd, env, workdir, tag)
    }

    /// Como [`commit_flat_rootfs`], mas recebe o tar do rootfs **já construído**
    /// (não-comprimido). Existe porque, em rootless com subuid, o rootfs de um
    /// `RUN` que corra `apt`/`dpkg` tem ficheiros de modo restrito que o
    /// utilizador real não lê — o tar tem de ser feito DENTRO do userns mapeado
    /// (`delonix __buildtar`, ver `cmd::mapped::buildtar`) e o resultado entregue
    /// aqui. O caminho in-process de [`commit_flat_rootfs`] fica para quando não
    /// há subuid (rootless single-uid) ou para root.
    pub fn commit_flat_rootfs_from_tar(
        &self,
        tar_bytes: Vec<u8>,
        cmd: Vec<String>,
        env: Vec<String>,
        workdir: String,
        tag: &str,
    ) -> Result<Image> {
        let layer = self.cas().write(&tar_bytes)?; // tar não-comprimido → diff_id = digest
        let diff_ids = vec![format!("sha256:{}", strip(&layer))];
        let created = now_unix();
        let config_json = serde_json::json!({
            "architecture": oci_arch(),
            "os": "linux",
            "config": { "Cmd": cmd, "Env": env, "WorkingDir": workdir },
            "rootfs": { "type": "layers", "diff_ids": diff_ids },
            "created_unix": created,
        });
        let id = self.cas().write(&serde_json::to_vec(&config_json)?)?;
        let repo_tags = self.merged_tags(&id, tag);
        let img = Image {
            id,
            repo_tags,
            layers: vec![layer],
            config: ImageConfig { cmd, entrypoint: Vec::new(), env, cpus: None, memory: None, security: Vec::new(), healthcheck: None, user: String::new(), working_dir: workdir },
            created_unix: created,
        };
        self.enforce_tag_uniqueness(&img)?;
        self.save(&img)?;
        Ok(img)
    }

    /// Cria uma imagem a partir do estado de um container (`docker commit`): o
    /// `new_layer` é o diff (upperdir) já empacotado por [`commit_upper`], sobre
    /// os layers da `base`; `cmd`/`env` são os do container. Config OCI válido
    /// (puxável pelo Docker), tal como o `build`.
    pub fn commit_container(
        &self,
        base: &Image,
        new_layer: String,
        cmd: Vec<String>,
        env: Vec<String>,
        tag: &str,
    ) -> Result<Image> {
        let mut layers = base.layers.clone();
        layers.push(new_layer);
        let mut diff_ids: Vec<String> = self.base_diff_ids(base);
        diff_ids.push(format!("sha256:{}", strip(layers.last().unwrap())));
        let entrypoint = base.config.entrypoint.clone();
        let cpus = base.config.cpus.clone();
        let memory = base.config.memory.clone();
        let security = base.config.security.clone();
        let healthcheck = base.config.healthcheck.clone();
        let workdir = base.config.working_dir.clone();
        let created = now_unix();
        let config_json = serde_json::json!({
            "architecture": oci_arch(),
            "os": "linux",
            "config": { "Cmd": cmd, "Entrypoint": entrypoint, "Env": env, "Cpus": cpus, "Memory": memory, "Security": security },
            "rootfs": { "type": "layers", "diff_ids": diff_ids },
            "created_unix": created,
        });
        let id = self.cas().write(&serde_json::to_vec(&config_json)?)?;
        let repo_tags = self.merged_tags(&id, tag);
        let img = Image {
            id,
            repo_tags,
            layers,
            config: ImageConfig { cmd, entrypoint, env, cpus, memory, security, healthcheck, user: String::new(), working_dir: workdir.clone() },
            created_unix: created,
        };
        self.enforce_tag_uniqueness(&img)?;
        self.save(&img)?;
        Ok(img)
    }
}

#[cfg(test)]
mod tests {
    use super::{join_continuations, parse_env_pairs};

    #[test]
    fn join_continuations_coalesces_backslash_lines() {
        let df = "RUN apt install \\\n    a \\\n    b\nENV X=1";
        let lines: Vec<String> = join_continuations(df).into_iter().map(|(_, l)| l).collect();
        assert_eq!(lines.len(), 2, "3 linhas físicas do RUN → 1 lógica, + o ENV");
        assert!(lines[0].starts_with("RUN apt install") && lines[0].contains(" a ") && lines[0].contains(" b"));
        assert_eq!(lines[1], "ENV X=1");
    }

    #[test]
    fn parse_env_pairs_handles_multi_var_and_quotes() {
        // Multi-var numa linha — o bug que perdia o PATH.
        let p = parse_env_pairs("A=1 B=2 PATH=/app/.venv/bin:$PATH");
        assert_eq!(p, vec![
            ("A".into(), "1".into()),
            ("B".into(), "2".into()),
            ("PATH".into(), "/app/.venv/bin:$PATH".into()),
        ]);
        // Valor com espaços entre aspas.
        assert_eq!(parse_env_pairs(r#"MSG="hello world" K=v"#), vec![
            ("MSG".into(), "hello world".into()),
            ("K".into(), "v".into()),
        ]);
        // Legado `ENV chave valor` (sem `=`).
        assert_eq!(parse_env_pairs("GREETING olá mundo"), vec![("GREETING".into(), "olá mundo".into())]);
    }
}
