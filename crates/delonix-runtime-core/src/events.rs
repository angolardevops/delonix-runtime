//! Registo de eventos do motor — o `docker events` de um runtime **sem daemon**.
//!
//! # Porquê um ficheiro e não um daemon
//!
//! O `docker events` funciona porque há um `dockerd` sempre vivo a multiplexar um
//! stream para os clientes. Aqui não há daemon nenhum: cada comando é um processo
//! efémero que nasce, faz o seu trabalho e morre. A resposta daemonless é o
//! inverso — um **log append-only** partilhado (`<root>/events.jsonl`): quem
//! produz acrescenta uma linha e sai; quem lê faz `tail`. O ficheiro É o bus.
//!
//! # Porque não precisa de trinco
//!
//! Um `write` em `O_APPEND` de menos de `PIPE_BUF` (4 KiB) é **atómico** em
//! filesystems locais: o kernel serializa o posicionamento e a escrita. Cada
//! evento é uma linha curta, muito abaixo desse limite — logo N processos
//! concorrentes acrescentam sem se entrelaçarem e sem `flock`. (Um evento que
//! passasse os 4 KiB perderia a garantia; por isso os campos são fixos e curtos,
//! nunca conteúdo arbitrário como logs ou env.)
//!
//! # Rotação
//!
//! Sem daemon não há quem limpe em background. A rotação é oportunista: quem
//! escreve verifica o tamanho e, se passou o tecto, roda para `.1` (uma geração
//! só — histórico não é a função disto; para auditoria de longo prazo, exporta).

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Tecto do ficheiro antes de rodar (~4 MiB ≈ dezenas de milhar de eventos).
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Um evento do ciclo de vida. Campos deliberadamente poucos e curtos — ver a
/// nota sobre `PIPE_BUF` no topo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Instante unix (segundos).
    pub ts: u64,
    /// `container` | `image` | `network` | `volume` | `vm`.
    pub kind: String,
    /// `create`|`start`|`stop`|`die`|`remove`|`pull`|…
    pub action: String,
    /// Id do objecto (curto).
    pub id: String,
    /// Nome legível.
    pub name: String,
    /// Detalhe opcional (ex.: exit code no `die`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Event {
    /// Linha para consumo humano (`system events`).
    pub fn to_line(&self) -> String {
        let when = crate::fmt_local_ts(self.ts);
        let detail = self
            .detail
            .as_deref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        format!(
            "{when}  {:<9} {:<7} {}  {}{}",
            self.kind,
            self.action,
            self.short_id(),
            self.name,
            detail
        )
    }

    fn short_id(&self) -> &str {
        &self.id[..12.min(self.id.len())]
    }
}

fn path(root: &Path) -> PathBuf {
    root.join("events.jsonl")
}

/// Acrescenta um evento. **Best-effort e infalível por desenho**: um erro a
/// registar um evento nunca pode fazer falhar a operação que o gerou (não se
/// recusa um `container stop` porque o log de eventos está cheio).
pub fn emit(root: &Path, kind: &str, action: &str, id: &str, name: &str, detail: Option<&str>) {
    let ev = Event {
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        kind: kind.to_string(),
        action: action.to_string(),
        id: id.to_string(),
        name: name.to_string(),
        detail: detail.map(str::to_string),
    };
    let Ok(mut line) = serde_json::to_string(&ev) else {
        return;
    };
    line.push('\n');
    let p = path(root);
    rotate_if_needed(&p);
    let _ = std::fs::create_dir_all(root);
    // `O_APPEND`: a atomicidade vem do kernel, não de um trinco nosso.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Roda quando passa o tecto. Oportunista (quem escreve limpa) — sem daemon não
/// há outra altura em que isto possa acontecer.
fn rotate_if_needed(p: &Path) {
    if std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) <= MAX_BYTES {
        return;
    }
    let _ = std::fs::rename(p, p.with_extension("jsonl.1"));
}

/// Lê os eventos registados (do mais antigo para o mais recente). Linhas
/// corrompidas são saltadas em silêncio: um evento ilegível não pode esconder
/// os outros.
pub fn read(root: &Path) -> Vec<Event> {
    let Ok(data) = std::fs::read_to_string(path(root)) else {
        return Vec::new();
    };
    data.lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .collect()
}

/// O tamanho actual do log (para o `-f` saber onde continuar).
pub fn size(root: &Path) -> u64 {
    std::fs::metadata(path(root)).map(|m| m.len()).unwrap_or(0)
}

/// Lê a partir de um offset (para o `follow`). Devolve os eventos e o offset novo.
pub fn read_from(root: &Path, offset: u64) -> (Vec<Event>, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let p = path(root);
    let Ok(mut f) = std::fs::File::open(&p) else {
        return (Vec::new(), offset);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // Encolheu = rodou; recomeça do princípio para não perder o ficheiro novo.
    let start = if len < offset { 0 } else { offset };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (Vec::new(), offset);
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return (Vec::new(), offset);
    }
    let evs = buf
        .lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .collect();
    (evs, start + buf.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("delonix-events-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn emit_e_read_fazem_round_trip() {
        let root = tmp("rt");
        emit(&root, "container", "create", "abc123def456", "web", None);
        emit(
            &root,
            "container",
            "die",
            "abc123def456",
            "web",
            Some("exit=42"),
        );
        let evs = read(&root);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].action, "create");
        assert_eq!(evs[1].detail.as_deref(), Some("exit=42"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A garantia que sustenta o desenho sem trincos: N processos (aqui threads,
    /// cada uma com o seu `OpenOptions`) acrescentam SEM se entrelaçarem — cada
    /// linha continua a ser um JSON válido e nenhuma se perde.
    #[test]
    fn emits_concorrentes_nao_se_entrelacam() {
        let root = tmp("race");
        const N: usize = 32;
        std::thread::scope(|sc| {
            for i in 0..N {
                let root = root.clone();
                sc.spawn(move || {
                    emit(
                        &root,
                        "container",
                        "start",
                        &format!("id{i:04}"),
                        &format!("nome-{i}"),
                        None,
                    );
                });
            }
        });
        let evs = read(&root);
        assert_eq!(
            evs.len(),
            N,
            "perderam-se ou corromperam-se eventos: {} de {N}",
            evs.len()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_from_continua_do_offset() {
        let root = tmp("off");
        emit(&root, "container", "create", "a1", "um", None);
        let (first, off) = read_from(&root, 0);
        assert_eq!(first.len(), 1);
        // Sem eventos novos, não devolve nada (é isto que o `-f` precisa).
        let (none, off2) = read_from(&root, off);
        assert!(none.is_empty());
        emit(&root, "container", "die", "a1", "um", None);
        let (novos, _) = read_from(&root, off2);
        assert_eq!(novos.len(), 1);
        assert_eq!(novos[0].action, "die");
        let _ = std::fs::remove_dir_all(&root);
    }
}
