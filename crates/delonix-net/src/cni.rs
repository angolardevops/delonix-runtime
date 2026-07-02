//! Camada de conformidade **CNI** (Container Network Interface, spec 0.4.0/1.0.0).
//!
//! Permite ao Delonix delegar a configuração de rede de uma netns a **plugins CNI
//! reais** (`/etc/cni/net.d/*.conflist` + binários em `/opt/cni/bin`), de modo a
//! correr Calico/Flannel/Cilium tal como o containerd/CRI-O — mantendo o SDN nativo
//! (bridge `delonix0`) como *provider* interno alternativo.
//!
//! Este módulo é a **camada de protocolo**, pura e testável: descobre/parseia a
//! config, resolve o binário do plugin no `CNI_PATH`, constrói o ambiente e o stdin
//! JSON, invoca `ADD`/`DEL` encadeando o `prevResult`, e parseia o resultado/erro.
//! O *wiring* aos caminhos de attach (root via `Net`, rootless via holder) é feito
//! por quem chama `add`/`del`.

use delonix_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Diretório padrão dos plugins CNI (o mesmo do containerd/CRI-O).
pub const DEFAULT_PLUGIN_DIR: &str = "/opt/cni/bin";
/// Diretório padrão das configs de rede CNI.
pub const DEFAULT_CONF_DIR: &str = "/etc/cni/net.d";
/// Interface por omissão dentro do container (paridade com o resto do Delonix).
pub const DEFAULT_IFNAME: &str = "eth0";

/// O comando CNI (vai para a env var `CNI_COMMAND`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command_ {
    Add,
    Del,
    Check,
}

impl Command_ {
    fn as_str(self) -> &'static str {
        match self {
            Command_::Add => "ADD",
            Command_::Del => "DEL",
            Command_::Check => "CHECK",
        }
    }
}

/// Identidade do container/interface para uma invocação CNI.
#[derive(Clone, Copy)]
pub struct Target<'a> {
    /// Id do container (`CNI_CONTAINERID`).
    pub container_id: &'a str,
    /// Caminho da netns (`CNI_NETNS`), ex. `/proc/<pid>/ns/net` ou `/run/netns/<n>`.
    pub netns: &'a str,
    /// Nome da interface dentro do container (`CNI_IFNAME`).
    pub ifname: &'a str,
}

/// Uma lista de configuração de rede CNI (`*.conflist`): uma cadeia de plugins que
/// partilham `cniVersion`/`name`. Um `*.conf` de plugin único é normalizado nisto.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetConfList {
    #[serde(rename = "cniVersion")]
    pub cni_version: String,
    pub name: String,
    /// Cada plugin é um objeto arbitrário; só o campo `type` é obrigatório. Guardamos
    /// como `Value` para preservar campos específicos de cada plugin no round-trip.
    pub plugins: Vec<Value>,
}

/// Resultado de um plugin CNI (spec 1.0.0 — campos irrelevantes ignorados).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CniResult {
    #[serde(rename = "cniVersion", default)]
    pub cni_version: String,
    #[serde(default)]
    pub interfaces: Vec<Interface>,
    #[serde(default)]
    pub ips: Vec<IpConf>,
    #[serde(default)]
    pub routes: Vec<Value>,
    #[serde(default)]
    pub dns: Value,
}

/// Uma interface no resultado CNI.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Interface {
    pub name: String,
    #[serde(default)]
    pub mac: String,
    #[serde(default)]
    pub sandbox: String,
}

/// Um IP atribuído no resultado CNI (o IPAM do plugin substitui o `alloc_ip` nativo).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IpConf {
    /// CIDR, ex. `10.244.1.7/24`.
    pub address: String,
    #[serde(default)]
    pub gateway: String,
    /// Índice em `interfaces[]` a que este IP pertence.
    #[serde(default)]
    pub interface: Option<i64>,
}

/// Erro estruturado devolvido por um plugin (JSON no stdout com saída ≠ 0).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CniError {
    pub code: i32,
    pub msg: String,
    #[serde(default)]
    pub details: String,
}

// ---------------------------------------------------------------------------
//  Descoberta e parsing de config (puro)
// ---------------------------------------------------------------------------

/// Lista os ficheiros de config CNI num diretório, por ordem alfabética (o CNI
/// escolhe o primeiro). Aceita `*.conflist` e `*.conf`. Diretório ausente → vazio.
pub fn list_conf_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("conflist") | Some("conf")
            )
        })
        .collect();
    out.sort();
    out
}

/// Parseia uma config CNI, aceitando tanto `*.conflist` (com `plugins[]`) como um
/// `*.conf` de plugin único (normalizado para uma lista de um só plugin).
pub fn parse_config(text: &str) -> Result<NetConfList> {
    let v: Value = serde_json::from_str(text).map_err(|e| Error::Invalid(format!("config CNI inválida: {e}")))?;
    if v.get("plugins").is_some() {
        return serde_json::from_value(v).map_err(|e| Error::Invalid(format!("conflist CNI inválida: {e}")));
    }
    // `*.conf` de plugin único: o próprio objeto é o plugin.
    let cni_version = v.get("cniVersion").and_then(|s| s.as_str()).unwrap_or("1.0.0").to_string();
    let name = v.get("name").and_then(|s| s.as_str()).unwrap_or("cni").to_string();
    Ok(NetConfList { cni_version, name, plugins: vec![v] })
}

/// Descobre e carrega a primeira config CNI do diretório dado. `None` se não houver.
pub fn load_default(conf_dir: &Path) -> Result<Option<NetConfList>> {
    let Some(first) = list_conf_files(conf_dir).into_iter().next() else { return Ok(None) };
    let text = std::fs::read_to_string(&first)
        .map_err(|e| Error::Invalid(format!("ler {}: {e}", first.display())))?;
    Ok(Some(parse_config(&text)?))
}

/// Resolve o binário de um plugin (`type`) no `CNI_PATH` (primeiro que exista).
pub fn resolve_plugin(cni_path: &[PathBuf], typ: &str) -> Option<PathBuf> {
    cni_path.iter().map(|d| d.join(typ)).find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
//  Construção de ambiente e stdin (puro)
// ---------------------------------------------------------------------------

/// Constrói as variáveis de ambiente do protocolo CNI para uma invocação.
fn build_env(cmd: Command_, t: Target, cni_path: &[PathBuf]) -> Vec<(String, String)> {
    let path = cni_path.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":");
    vec![
        ("CNI_COMMAND".into(), cmd.as_str().into()),
        ("CNI_CONTAINERID".into(), t.container_id.into()),
        ("CNI_NETNS".into(), t.netns.into()),
        ("CNI_IFNAME".into(), t.ifname.into()),
        ("CNI_PATH".into(), path),
    ]
}

/// Constrói o JSON de stdin para um plugin: a sua config, com `cniVersion`/`name`
/// da lista injetados e o `prevResult` do plugin anterior (encadeamento CNI).
fn plugin_input(plugin: &Value, name: &str, cni_version: &str, prev: Option<&CniResult>) -> Result<String> {
    let mut obj = plugin.clone();
    let map = obj
        .as_object_mut()
        .ok_or_else(|| Error::Invalid("plugin CNI não é um objeto JSON".into()))?;
    map.insert("cniVersion".into(), json!(cni_version));
    map.insert("name".into(), json!(name));
    if let Some(p) = prev {
        map.insert("prevResult".into(), serde_json::to_value(p).unwrap_or(Value::Null));
    }
    serde_json::to_string(&obj).map_err(|e| Error::Invalid(format!("serializar config CNI: {e}")))
}

/// Parseia o resultado (stdout) de um plugin bem-sucedido.
fn parse_result(stdout: &str) -> Result<CniResult> {
    if stdout.trim().is_empty() {
        return Ok(CniResult::default());
    }
    serde_json::from_str(stdout).map_err(|e| Error::Invalid(format!("resultado CNI inválido: {e}")))
}

/// Tenta extrair o erro estruturado que um plugin escreve no stdout ao falhar.
fn parse_error(stdout: &str) -> Option<CniError> {
    serde_json::from_str(stdout).ok()
}

// ---------------------------------------------------------------------------
//  Execução (impuro) + orquestração da cadeia
// ---------------------------------------------------------------------------

/// Invoca um binário de plugin com a config por stdin e o ambiente CNI. Devolve
/// `(sucesso, stdout, stderr)`.
fn invoke(plugin: &Path, envs: &[(String, String)], stdin_json: &str) -> Result<(bool, String, String)> {
    use std::io::Write;
    let mut child = Command::new(plugin)
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Runtime { context: "cni-spawn", message: format!("{}: {e}", plugin.display()) })?;
    child
        .stdin
        .take()
        .ok_or_else(|| Error::Runtime { context: "cni-stdin", message: "sem stdin".into() })?
        .write_all(stdin_json.as_bytes())
        .map_err(|e| Error::Runtime { context: "cni-stdin", message: e.to_string() })?;
    let out = child
        .wait_with_output()
        .map_err(|e| Error::Runtime { context: "cni-wait", message: e.to_string() })?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Executa um plugin da cadeia com um dado comando, devolvendo o resultado parseado.
fn run_one(
    cmd: Command_,
    plugin: &Value,
    net: &NetConfList,
    cni_path: &[PathBuf],
    target: Target,
    prev: Option<&CniResult>,
) -> Result<CniResult> {
    let typ = plugin
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or_else(|| Error::Invalid("plugin CNI sem campo `type`".into()))?;
    let bin = resolve_plugin(cni_path, typ)
        .ok_or_else(|| Error::Invalid(format!("plugin CNI `{typ}` não encontrado no CNI_PATH")))?;
    let envs = build_env(cmd, target, cni_path);
    let stdin_json = plugin_input(plugin, &net.name, &net.cni_version, prev)?;
    let (ok, stdout, stderr) = invoke(&bin, &envs, &stdin_json)?;
    if !ok {
        let detail = parse_error(&stdout)
            .map(|e| format!("code {} — {}", e.code, e.msg))
            .unwrap_or_else(|| stderr.trim().to_string());
        return Err(Error::Runtime {
            context: "cni-plugin",
            message: format!("`{typ}` {}: {detail}", cmd.as_str()),
        });
    }
    parse_result(&stdout)
}

/// `ADD`: corre a cadeia de plugins por ordem, encadeando o `prevResult`, e devolve
/// o resultado do último plugin (que contém as interfaces/IPs finais).
pub fn add(
    net: &NetConfList,
    cni_path: &[PathBuf],
    container_id: &str,
    netns: &str,
    ifname: &str,
) -> Result<CniResult> {
    let target = Target { container_id, netns, ifname };
    let mut prev: Option<CniResult> = None;
    for plugin in &net.plugins {
        let r = run_one(Command_::Add, plugin, net, cni_path, target, prev.as_ref())?;
        prev = Some(r);
    }
    prev.ok_or_else(|| Error::Invalid("conflist CNI sem plugins".into()))
}

/// `DEL`: corre a cadeia por **ordem inversa** (spec CNI). Best-effort: continua
/// mesmo que um plugin falhe, devolvendo o primeiro erro no fim.
pub fn del(
    net: &NetConfList,
    cni_path: &[PathBuf],
    container_id: &str,
    netns: &str,
    ifname: &str,
) -> Result<()> {
    let target = Target { container_id, netns, ifname };
    let mut first_err: Option<Error> = None;
    for plugin in net.plugins.iter().rev() {
        if let Err(e) = run_one(Command_::Del, plugin, net, cni_path, target, None) {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A camada CNI está ativa? Opt-in por `DELONIX_CNI=1`. Devolve a conflist default
/// (`/etc/cni/net.d`) se ativa **e** existir uma config; senão `None` — nesse caso
/// o chamador usa o SDN nativo, sem qualquer mudança de comportamento.
pub fn enabled_conf() -> Option<NetConfList> {
    if std::env::var("DELONIX_CNI").ok().as_deref() != Some("1") {
        return None;
    }
    load_default(Path::new(DEFAULT_CONF_DIR)).ok().flatten()
}

/// Diretórios de plugins a partir da env `CNI_PATH` (`:`-separada) ou o default.
pub fn plugin_dirs() -> Vec<PathBuf> {
    match std::env::var("CNI_PATH") {
        Ok(v) if !v.is_empty() => std::env::split_paths(&v).collect(),
        _ => vec![PathBuf::from(DEFAULT_PLUGIN_DIR)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFLIST: &str = r#"{
        "cniVersion": "1.0.0",
        "name": "delonix",
        "plugins": [
            { "type": "bridge", "bridge": "cni0", "isGateway": true },
            { "type": "portmap", "capabilities": { "portMappings": true } }
        ]
    }"#;

    #[test]
    fn parse_conflist_le_a_cadeia() {
        let net = parse_config(CONFLIST).unwrap();
        assert_eq!(net.name, "delonix");
        assert_eq!(net.cni_version, "1.0.0");
        assert_eq!(net.plugins.len(), 2);
        assert_eq!(net.plugins[0]["type"], "bridge");
        assert_eq!(net.plugins[1]["type"], "portmap");
    }

    #[test]
    fn parse_conf_unico_normaliza_para_lista() {
        let conf = r#"{ "cniVersion": "0.4.0", "name": "mynet", "type": "bridge" }"#;
        let net = parse_config(conf).unwrap();
        assert_eq!(net.plugins.len(), 1);
        assert_eq!(net.plugins[0]["type"], "bridge");
        assert_eq!(net.cni_version, "0.4.0");
    }

    #[test]
    fn build_env_tem_as_5_variaveis() {
        let dirs = vec![PathBuf::from("/opt/cni/bin"), PathBuf::from("/usr/lib/cni")];
        let t = Target { container_id: "abc123", netns: "/proc/42/ns/net", ifname: "eth0" };
        let env = build_env(Command_::Add, t, &dirs);
        let get = |k: &str| env.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("CNI_COMMAND"), "ADD");
        assert_eq!(get("CNI_CONTAINERID"), "abc123");
        assert_eq!(get("CNI_NETNS"), "/proc/42/ns/net");
        assert_eq!(get("CNI_IFNAME"), "eth0");
        assert_eq!(get("CNI_PATH"), "/opt/cni/bin:/usr/lib/cni");
    }

    #[test]
    fn plugin_input_injeta_nome_versao_e_prevresult() {
        let net = parse_config(CONFLIST).unwrap();
        // 1.º plugin: sem prevResult.
        let s1 = plugin_input(&net.plugins[0], &net.name, &net.cni_version, None).unwrap();
        let v1: Value = serde_json::from_str(&s1).unwrap();
        assert_eq!(v1["name"], "delonix");
        assert_eq!(v1["cniVersion"], "1.0.0");
        assert_eq!(v1["bridge"], "cni0");
        assert!(v1.get("prevResult").is_none());
        // 2.º plugin: com prevResult do 1.º.
        let prev = CniResult { ips: vec![IpConf { address: "10.0.0.5/24".into(), ..Default::default() }], ..Default::default() };
        let s2 = plugin_input(&net.plugins[1], &net.name, &net.cni_version, Some(&prev)).unwrap();
        let v2: Value = serde_json::from_str(&s2).unwrap();
        assert_eq!(v2["prevResult"]["ips"][0]["address"], "10.0.0.5/24");
    }

    #[test]
    fn parse_result_le_ips_e_interfaces() {
        let out = r#"{
            "cniVersion": "1.0.0",
            "interfaces": [{ "name": "eth0", "mac": "0a:1b:2c:3d:4e:5f", "sandbox": "/proc/1/ns/net" }],
            "ips": [{ "address": "10.244.1.7/24", "gateway": "10.244.1.1", "interface": 0 }]
        }"#;
        let r = parse_result(out).unwrap();
        assert_eq!(r.ips.len(), 1);
        assert_eq!(r.ips[0].address, "10.244.1.7/24");
        assert_eq!(r.ips[0].gateway, "10.244.1.1");
        assert_eq!(r.interfaces[0].name, "eth0");
    }

    #[test]
    fn parse_result_vazio_e_ok() {
        assert!(parse_result("   ").unwrap().ips.is_empty());
    }

    #[test]
    fn parse_error_le_o_erro_estruturado() {
        let e = parse_error(r#"{ "cniVersion":"1.0.0", "code": 7, "msg": "sem IP livre" }"#).unwrap();
        assert_eq!(e.code, 7);
        assert_eq!(e.msg, "sem IP livre");
    }

    #[test]
    fn resolve_plugin_encontra_no_primeiro_dir() {
        let tmp = std::env::temp_dir().join(format!("dlx-cni-res-{}", std::process::id()));
        let d1 = tmp.join("a");
        let d2 = tmp.join("b");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(d2.join("bridge"), b"#!/bin/true\n").unwrap();
        let dirs = vec![d1.clone(), d2.clone()];
        assert_eq!(resolve_plugin(&dirs, "bridge"), Some(d2.join("bridge")));
        assert_eq!(resolve_plugin(&dirs, "inexistente"), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn list_conf_files_ordena_e_filtra() {
        let tmp = std::env::temp_dir().join(format!("dlx-cni-conf-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("20-b.conflist"), "{}").unwrap();
        std::fs::write(tmp.join("10-a.conf"), "{}").unwrap();
        std::fs::write(tmp.join("readme.txt"), "x").unwrap();
        let got: Vec<String> = list_conf_files(&tmp)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, vec!["10-a.conf", "20-b.conflist"]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn list_conf_files_dir_ausente_vazio() {
        assert!(list_conf_files(Path::new("/nao/existe/de/todo")).is_empty());
    }

    /// E2E do executor: um plugin falso (shell) valida a env `CNI_COMMAND`, lê a
    /// config do stdin e devolve um resultado CNI — exercita invoke/run_one/add/del.
    #[test]
    fn add_e_del_com_plugin_falso() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!("dlx-cni-e2e-{}", std::process::id()));
        let bindir = tmp.join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        // ADD → imprime resultado; DEL → nada; comando desconhecido → erro estruturado.
        let script = r#"#!/bin/sh
cat > /dev/null
case "$CNI_COMMAND" in
  ADD) echo '{"cniVersion":"1.0.0","ips":[{"address":"10.9.9.9/24","gateway":"10.9.9.1"}]}' ;;
  DEL) exit 0 ;;
  *)   echo '{"code":4,"msg":"comando nao suportado"}'; exit 4 ;;
esac
"#;
        let plugin = bindir.join("faux");
        std::fs::write(&plugin, script).unwrap();
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let net = parse_config(r#"{"cniVersion":"1.0.0","name":"t","plugins":[{"type":"faux"}]}"#).unwrap();
        let dirs = vec![bindir.clone()];
        let r = add(&net, &dirs, "cid", "/proc/1/ns/net", "eth0").unwrap();
        assert_eq!(r.ips[0].address, "10.9.9.9/24");
        assert_eq!(r.ips[0].gateway, "10.9.9.1");
        // DEL é best-effort e devolve Ok.
        del(&net, &dirs, "cid", "/proc/1/ns/net", "eth0").unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
