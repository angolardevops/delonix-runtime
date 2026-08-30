//! `delonix-mcp` — a local, tenancy-free AI control surface over Delonix Runtime.
//!
//! See `docs/adr/0025-mcp-local-ai-control-surface.md`. Stdio-only in this pass:
//! `delonix mcp serve` is a foreground process, spawned as a child of the AI
//! client for one session, and exits when its stdin closes — the same category
//! as any other CLI invocation, never a persistent daemon. The single principal
//! is the local uid running the process, the same trust boundary `delonix-mgmt`
//! already uses for its unix socket (`SO_PEERCRED` uid-equality). No tenant, no
//! OAuth/OIDC, no IAM scopes — those stay out per ADR-0010/guardrail #2.
//!
//! Tool outputs are pretty-printed JSON **text**, not MCP structured content: the
//! domain types reused here (`Container`/`Vm`/`Volume`) do not derive
//! `schemars::JsonSchema` (only the CLI's manifest spec structs do, per
//! ADR-0007), and duplicating them into a second schema would violate "one
//! canonical domain schema." Tool *inputs* are fully typed and schema-validated
//! (`Parameters<T: JsonSchema>`), which is the half that matters for keeping an
//! LLM from sending a malformed or injected call.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use delonix_runtime_core::{Container, Error as EngineError, Store};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    ErrorCode, ErrorData, ReadResourceRequestParams, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::io::stdio;
use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

pub mod audit;
pub mod risk;
pub mod tasks;

use audit::{hash_args, now_unix, AuditEvent, AuditLog};
use risk::{risk_of, TOOL_RISK};
use tasks::TaskRegistry;

/// `$DELONIX_ROOT`, or the same rootless/root default every other store crate in
/// this workspace derives (mirrors `Store::default_root` minus the `containers`
/// join, since this crate needs the shared root, not one store's subdirectory).
pub fn state_root() -> PathBuf {
    if let Some(root) = std::env::var_os("DELONIX_ROOT") {
        return PathBuf::from(root);
    }
    // SAFETY: geteuid() is always safe and does not fail.
    if unsafe { libc::geteuid() } != 0 {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."));
        return base.join("delonix");
    }
    PathBuf::from("/var/lib/delonix")
}

fn principal() -> String {
    // SAFETY: geteuid() is always safe and does not fail.
    format!("uid:{}", unsafe { libc::geteuid() })
}

fn typed_err(kind: &'static str, message: impl Into<String>) -> ErrorData {
    let code = match kind {
        "NOT_FOUND" => ErrorCode::RESOURCE_NOT_FOUND,
        "VALIDATION_FAILED" | "INVALID_MANIFEST" => ErrorCode::INVALID_PARAMS,
        "POLICY_DENIED" | "NOT_IMPLEMENTED" | "CONFLICT" => ErrorCode::INVALID_REQUEST,
        _ => ErrorCode::INTERNAL_ERROR,
    };
    ErrorData::new(code, message.into(), Some(json!({ "delonix_error": kind })))
}

fn from_engine_error(e: EngineError) -> ErrorData {
    let kind = match &e {
        EngineError::NotFound(_) | EngineError::VmNotFound(_) => "NOT_FOUND",
        EngineError::Invalid(_) => "VALIDATION_FAILED",
        EngineError::NotRunning(_) => "CONFLICT",
        _ => "INTERNAL",
    };
    typed_err(kind, e.to_string())
}

/// `resource.{list,get,describe}`'s `kind` argument. `Workload` is deliberately
/// absent: a `kind: Workload` document lowers into a synthetic Container/Vm at
/// manifest-load time and never exists as its own runtime object (see
/// `AGENTS.md` "kind: Workload") — listing it as a resource kind here would
/// always return empty and read as a bug rather than the documented behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceKind {
    Container,
    Vm,
    Volume,
}

impl ResourceKind {
    fn parse(s: &str) -> Result<Self, ErrorData> {
        match s.to_ascii_lowercase().trim_end_matches('s') {
            "container" => Ok(Self::Container),
            "vm" => Ok(Self::Vm),
            "volume" => Ok(Self::Volume),
            _ => Err(typed_err(
                "VALIDATION_FAILED",
                format!("unknown resource kind '{s}' — expected one of: container, vm, volume"),
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Vm => "vm",
            Self::Volume => "volume",
        }
    }
}

fn pretty(value: impl serde::Serialize) -> String {
    serde_json::to_string_pretty(&value).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn list_containers(base: &Path) -> Result<Vec<Container>, ErrorData> {
    Store::open(base.join("containers"))
        .and_then(|s| s.list())
        .map_err(from_engine_error)
}

fn list_vms(base: &Path) -> Result<Vec<delonix_runtime_core::Vm>, ErrorData> {
    delonix_vm::list(base).map_err(from_engine_error)
}

fn list_volumes(base: &Path) -> Result<Vec<delonix_volume::Volume>, ErrorData> {
    delonix_volume::VolumeStore::open(base)
        .and_then(|s| s.list())
        .map_err(from_engine_error)
}

fn list_kind(base: &Path, kind: ResourceKind) -> Result<serde_json::Value, ErrorData> {
    Ok(match kind {
        ResourceKind::Container => serde_json::to_value(list_containers(base)?),
        ResourceKind::Vm => serde_json::to_value(list_vms(base)?),
        ResourceKind::Volume => serde_json::to_value(list_volumes(base)?),
    }
    .unwrap_or(serde_json::Value::Null))
}

fn get_kind(base: &Path, kind: ResourceKind, name: &str) -> Result<serde_json::Value, ErrorData> {
    match kind {
        ResourceKind::Container => Store::open(base.join("containers"))
            .and_then(|s| s.load(name))
            .map(|c| serde_json::to_value(c).unwrap_or(serde_json::Value::Null))
            .map_err(from_engine_error),
        ResourceKind::Vm => delonix_vm::status(base, name)
            .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null))
            .map_err(from_engine_error),
        ResourceKind::Volume => list_volumes(base)?
            .into_iter()
            .find(|v| v.name == name)
            .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null))
            .ok_or_else(|| typed_err("NOT_FOUND", format!("no such volume: {name}"))),
    }
}

// ---------------------------------------------------------------------------
// Tool parameter types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct ResourceListParams {
    /// Resource kind: `container`, `vm`, or `volume`.
    kind: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ResourceGetParams {
    /// Resource kind: `container`, `vm`, or `volume`.
    kind: String,
    /// Resource name (or container id/prefix).
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LogsQueryParams {
    /// Container name or id.
    name: String,
    /// Number of trailing lines (only meaningful for containers run with
    /// `--log-cri`; see `delonix container logs --help`).
    #[serde(default)]
    tail: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StorageInspectParams {
    /// Optional volume name; omit to list all volumes.
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ContainerRestartParams {
    /// Container name or id to restart.
    name: String,
    /// Must be `true` — this is a DISRUPTIVE operation and is refused otherwise.
    #[serde(default)]
    confirm: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskIdParams {
    task_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AuditQueryParams {
    /// Maximum number of most-recent audit events to return (default 50).
    #[serde(default)]
    limit: Option<usize>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct DelonixMcp {
    base: PathBuf,
    principal: String,
    tasks: TaskRegistry,
    audit: Arc<AuditLog>,
}

impl DelonixMcp {
    pub fn new(base: PathBuf) -> std::io::Result<Self> {
        let audit = AuditLog::open(&base)?;
        Ok(Self {
            base,
            principal: principal(),
            tasks: TaskRegistry::default(),
            audit: Arc::new(audit),
        })
    }

    /// `outcome` is one of `"ok"`, `"error"`, or `"denied"` — the policy decision
    /// and result are both derived from it, since every call site's pair is one
    /// of exactly those three combinations (this is what keeps the parameter
    /// count under clippy's `too_many_arguments`, not just working around it).
    fn log(
        &self,
        tool: &str,
        outcome: &str,
        args: &serde_json::Value,
        resource: Option<String>,
        task_id: Option<String>,
        message: Option<String>,
    ) {
        let (decision, result) = if outcome == "denied" {
            ("denied", "error")
        } else {
            ("allowed", outcome)
        };
        self.audit.append(&AuditEvent {
            timestamp_unix: now_unix(),
            principal: &self.principal,
            tool,
            risk: risk_of(tool).as_str(),
            policy_decision: decision,
            result,
            arguments_hash: hash_args(args),
            resource,
            task_id,
            message,
        });
    }
}

#[tool_router]
impl DelonixMcp {
    #[tool(
        name = "runtime.info",
        description = "Delonix Runtime version, state root, and cheap resource counts.",
        annotations(read_only_hint = true)
    )]
    fn runtime_info(&self) -> String {
        let summary = delonix_mgmt::dashstats::collect(&self.base, false, false);
        self.log("runtime.info", "ok", &json!({}), None, None, None);
        pretty(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "state_root": self.base.display().to_string(),
            "principal": self.principal,
            "summary": summary,
        }))
    }

    #[tool(
        name = "runtime.health",
        description = "Whether this node's stores are reachable (container/VM/volume stores openable).",
        annotations(read_only_hint = true)
    )]
    fn runtime_health(&self) -> String {
        let containers_ok = Store::open(self.base.join("containers")).is_ok();
        let volumes_ok = delonix_volume::VolumeStore::open(&self.base).is_ok();
        let ok = containers_ok && volumes_ok;
        self.log("runtime.health", "ok", &json!({}), None, None, None);
        pretty(json!({
            "ok": ok,
            "checks": {
                "container_store": containers_ok,
                "volume_store": volumes_ok,
            }
        }))
    }

    #[tool(
        name = "resources.get",
        description = "Host capacity, which cgroup controllers this node can actually enforce, \
                       current PSI pressure, and the findings that follow — each with a stable \
                       DLX-RES-nnn id. Read-only.",
        annotations(read_only_hint = true)
    )]
    fn resources_get(&self) -> String {
        use delonix_runtime::resource_advice as advice;
        // The rules are NOT re-implemented here. An agent that disagrees with
        // `delonix system resources` about the same host is worse than no
        // agent: whoever reads the two has no way to tell which one is lying.
        let snap = advice::collect(&self.base);
        let findings = advice::advise(&snap);
        let inference = advice::local_inference(&snap);
        // Built before the `json!`: the macro reads a `[` in value position as
        // a JSON array literal and chokes on the `.iter()` that follows.
        let pressure: Vec<_> = [
            ("cpu", snap.psi_cpu),
            ("memory", snap.psi_memory),
            ("io", snap.psi_io),
        ]
        .iter()
        .map(|(r, p)| {
            json!({
                "resource": r,
                "avg10": p.map(|p| p.avg10),
                "avg60": p.map(|p| p.avg60),
                "avg300": p.map(|p| p.avg300),
            })
        })
        .collect();
        self.log("resources.get", "ok", &json!({}), None, None, None);
        pretty(json!({
            "host": {
                "cpus": snap.cpus,
                "memory_bytes": snap.mem_total,
                "memory_available_bytes": snap.mem_available,
                "swap_bytes": snap.swap_total,
                "swap_used_bytes": snap.swap_used,
                "state_root_free_bytes": snap.disk_free,
                "cpu_temperature_c": snap.cpu_temp_c,
                "gpu": snap.gpu.as_ref().map(|g| json!({
                    "name": g.name,
                    "vram_total_mib": g.vram_total_mib,
                    "vram_free_mib": g.vram_free_mib,
                    "cdi_spec": g.cdi_spec,
                    "drives_display": g.drives_display,
                })),
            },
            "control": {
                "rootless": snap.rootless,
                "cgroup_base": snap.cgroup_base,
                "delegated": snap.delegated,
                "aggregate_slice": snap.aggregate_slice,
            },
            "pressure": pressure,
            "advice": findings.iter().map(|f| json!({
                "id": f.id,
                "severity": f.severity.as_str(),
                "class": f.class.as_str(),
                "finding": f.finding,
                "action": f.action,
            })).collect::<Vec<_>>(),
            // Whether this very node should be the one running the model that
            // is reading this. It usually should not, and the reasons say why.
            "local_inference": {
                "verdict": inference.verdict.as_str(),
                "largest_model_b_q4": inference.largest_model_b,
                "reasons": inference.reasons,
            },
        }))
    }

    #[tool(
        name = "resource.list",
        description = "List resources of a kind (container, vm, or volume).",
        annotations(read_only_hint = true)
    )]
    fn resource_list(
        &self,
        Parameters(ResourceListParams { kind }): Parameters<ResourceListParams>,
    ) -> Result<String, ErrorData> {
        let k = ResourceKind::parse(&kind)?;
        let args = json!({ "kind": kind });
        match list_kind(&self.base, k) {
            Ok(v) => {
                self.log(
                    "resource.list",
                    "ok",
                    &args,
                    Some(k.as_str().to_string()),
                    None,
                    None,
                );
                Ok(pretty(v))
            }
            Err(e) => {
                self.log(
                    "resource.list",
                    "error",
                    &args,
                    Some(k.as_str().to_string()),
                    None,
                    Some(e.message.to_string()),
                );
                Err(e)
            }
        }
    }

    #[tool(
        name = "resource.get",
        description = "Get the full record of one resource by kind and name.",
        annotations(read_only_hint = true)
    )]
    fn resource_get(
        &self,
        Parameters(ResourceGetParams { kind, name }): Parameters<ResourceGetParams>,
    ) -> Result<String, ErrorData> {
        let k = ResourceKind::parse(&kind)?;
        let args = json!({ "kind": kind, "name": name });
        match get_kind(&self.base, k, &name) {
            Ok(v) => {
                self.log("resource.get", "ok", &args, Some(name.clone()), None, None);
                Ok(pretty(v))
            }
            Err(e) => {
                self.log(
                    "resource.get",
                    "error",
                    &args,
                    Some(name.clone()),
                    None,
                    Some(e.message.to_string()),
                );
                Err(e)
            }
        }
    }

    #[tool(
        name = "resource.describe",
        description = "Human-oriented summary of one resource (get plus a short status line).",
        annotations(read_only_hint = true)
    )]
    fn resource_describe(
        &self,
        Parameters(ResourceGetParams { kind, name }): Parameters<ResourceGetParams>,
    ) -> Result<String, ErrorData> {
        let k = ResourceKind::parse(&kind)?;
        let record = get_kind(&self.base, k, &name)?;
        let status = record
            .get("status")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        self.log(
            "resource.describe",
            "ok",
            &json!({ "kind": kind, "name": name }),
            Some(name.clone()),
            None,
            None,
        );
        Ok(pretty(json!({
            "kind": k.as_str(),
            "name": name,
            "status": status,
            "record": record,
        })))
    }

    #[tool(
        name = "metrics.query",
        description = "Cheap runtime metrics (container/VM counts, memory). Does not scan disk/network usage.",
        annotations(read_only_hint = true)
    )]
    fn metrics_query(&self) -> String {
        let summary = delonix_mgmt::dashstats::collect(&self.base, false, false);
        self.log("metrics.query", "ok", &json!({}), None, None, None);
        pretty(summary)
    }

    #[tool(
        name = "logs.query",
        description = "Container logs (wraps `delonix container logs`).",
        annotations(read_only_hint = true)
    )]
    fn logs_query(
        &self,
        Parameters(LogsQueryParams { name, tail }): Parameters<LogsQueryParams>,
    ) -> Result<String, ErrorData> {
        let mut args = vec!["container".to_string(), "logs".to_string()];
        if let Some(t) = tail {
            args.push("--tail".to_string());
            args.push(t.to_string());
        }
        args.push(name.clone());
        let call_args = json!({ "name": name, "tail": tail });
        match run_cli(&self.base, args) {
            Ok((ok, output)) => {
                self.log(
                    "logs.query",
                    if ok { "ok" } else { "error" },
                    &call_args,
                    Some(name),
                    None,
                    None,
                );
                Ok(output)
            }
            Err(e) => {
                self.log(
                    "logs.query",
                    "error",
                    &call_args,
                    Some(name),
                    None,
                    Some(e.message.to_string()),
                );
                Err(e)
            }
        }
    }

    #[tool(
        name = "network.inspect",
        description = "SDN/holder network status (bridges, firewall, ingress).",
        annotations(read_only_hint = true)
    )]
    fn network_inspect(&self) -> String {
        let status = delonix_net::infra::status();
        self.log("network.inspect", "ok", &json!({}), None, None, None);
        pretty(status)
    }

    #[tool(
        name = "storage.inspect",
        description = "Named volumes (all, or one by name).",
        annotations(read_only_hint = true)
    )]
    fn storage_inspect(
        &self,
        Parameters(StorageInspectParams { name }): Parameters<StorageInspectParams>,
    ) -> Result<String, ErrorData> {
        let volumes = list_volumes(&self.base)?;
        let args = json!({ "name": name });
        let result = match &name {
            Some(n) => volumes
                .into_iter()
                .find(|v| &v.name == n)
                .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null))
                .ok_or_else(|| typed_err("NOT_FOUND", format!("no such volume: {n}")))?,
            None => serde_json::to_value(volumes).unwrap_or(serde_json::Value::Null),
        };
        self.log("storage.inspect", "ok", &args, name, None, None);
        Ok(pretty(result))
    }

    #[tool(
        name = "container.restart",
        description = "Restart a container. DISRUPTIVE — requires confirm: true, or is refused with POLICY_DENIED.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    fn container_restart(
        &self,
        Parameters(ContainerRestartParams { name, confirm }): Parameters<ContainerRestartParams>,
    ) -> Result<String, ErrorData> {
        let risk = risk_of("container.restart");
        let args = json!({ "name": name, "confirm": confirm });
        if risk.requires_confirm() && !confirm {
            self.log(
                "container.restart",
                "denied",
                &args,
                Some(name.clone()),
                None,
                Some("confirm:true required for a DISRUPTIVE tool".to_string()),
            );
            return Err(typed_err(
                "POLICY_DENIED",
                format!(
                    "container.restart is {} — resend the call with confirm: true",
                    risk.as_str()
                ),
            ));
        }

        let base = self.base.clone();
        let target = name.clone();
        let task_id = self
            .tasks
            .spawn("container.restart", risk.as_str(), move || {
                run_cli_blocking(
                    &base,
                    vec!["container".to_string(), "restart".to_string(), target],
                )
                .map(|(ok, output)| json!({ "ok": ok, "output": output }))
            });
        self.log(
            "container.restart",
            "ok",
            &args,
            Some(name),
            Some(task_id.clone()),
            None,
        );
        Ok(pretty(json!({ "task_id": task_id })))
    }

    #[tool(
        name = "task.get",
        description = "Status of a task started by a mutating tool.",
        annotations(read_only_hint = true)
    )]
    fn task_get(
        &self,
        Parameters(TaskIdParams { task_id }): Parameters<TaskIdParams>,
    ) -> Result<String, ErrorData> {
        match self.tasks.get(&task_id) {
            Some(info) => Ok(pretty(info)),
            None => Err(typed_err("NOT_FOUND", format!("no such task: {task_id}"))),
        }
    }

    #[tool(
        name = "task.cancel",
        description = "Request cancellation of a task. Only prevents a not-yet-started task from starting; a task already running is left to finish (see docs).",
        annotations(read_only_hint = false)
    )]
    fn task_cancel(
        &self,
        Parameters(TaskIdParams { task_id }): Parameters<TaskIdParams>,
    ) -> Result<String, ErrorData> {
        match self.tasks.cancel(&task_id) {
            Some(info) => Ok(pretty(info)),
            None => Err(typed_err("NOT_FOUND", format!("no such task: {task_id}"))),
        }
    }

    #[tool(
        name = "audit.query",
        description = "Most recent tool-call audit events from this session's audit log.",
        annotations(read_only_hint = true)
    )]
    fn audit_query(
        &self,
        Parameters(AuditQueryParams { limit }): Parameters<AuditQueryParams>,
    ) -> String {
        let events = self.audit.tail(limit.unwrap_or(50));
        pretty(events)
    }
}

#[tool_handler]
impl ServerHandler for DelonixMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(
            "Local, tenancy-free AI control surface over the Delonix Runtime engine on this \
             host (ADR-0025). No tenant/OAuth/IAM: the principal is the local uid running this \
             process. Prefer read tools (runtime.*, resource.*, metrics.query, logs.query, \
             network.inspect, storage.inspect) to investigate before calling a mutating tool. \
             Mutating tools above SAFE_WRITE risk require confirm: true and return a task_id — \
             poll task.get to see the result, and check audit.query for the trail.",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, ErrorData> {
        let resources = vec![
            Resource::new("delonix://runtime/info", "runtime-info")
                .with_description("Version, state root, and cheap counts"),
            Resource::new("delonix://runtime/capabilities", "runtime-capabilities")
                .with_description("The tool risk table this server enforces"),
            Resource::new("delonix://resources/container", "containers")
                .with_description("All containers"),
            Resource::new("delonix://resources/vm", "vms").with_description("All VMs"),
            Resource::new("delonix://resources/volume", "volumes")
                .with_description("All named volumes"),
        ];
        Ok(rmcp::model::ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, ErrorData> {
        let uri = request.uri.clone();
        let body = self.read_delonix_uri(&uri)?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(body, uri)]).into())
    }
}

impl DelonixMcp {
    fn read_delonix_uri(&self, uri: &str) -> Result<String, ErrorData> {
        let Some(rest) = uri.strip_prefix("delonix://") else {
            return Err(typed_err(
                "NOT_FOUND",
                format!("not a delonix:// uri: {uri}"),
            ));
        };
        let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            ["runtime", "info"] => Ok(self.runtime_info()),
            ["runtime", "capabilities"] => Ok(pretty(capabilities_table())),
            ["resources", kind] => {
                let k = ResourceKind::parse(kind)?;
                Ok(pretty(list_kind(&self.base, k)?))
            }
            ["resources", kind, name] => {
                let k = ResourceKind::parse(kind)?;
                Ok(pretty(get_kind(&self.base, k, name)?))
            }
            ["schema", _kind] => Err(typed_err(
                "NOT_IMPLEMENTED",
                "delonix://schema/{kind} is not implemented in this pass: the generated JSON \
                 Schema for manifest Kinds (ADR-0007) lives in delonix-runtime-bin, which this \
                 crate cannot depend on (the bin depends on delonix-mcp, not the reverse) — \
                 reusing it here needs the spec structs promoted to a shared crate first, which \
                 is its own follow-up, not silently faked here.",
            )),
            _ => Err(typed_err("NOT_FOUND", format!("no such resource: {uri}"))),
        }
    }
}

/// `(tool_name, risk, requires_confirm)` — what `delonix mcp capabilities` prints
/// and what `delonix://runtime/capabilities` serves.
pub fn capabilities_table() -> Vec<serde_json::Value> {
    TOOL_RISK
        .iter()
        .map(|(name, risk)| {
            json!({
                "tool": name,
                "risk": risk.as_str(),
                "requires_confirm": risk.requires_confirm(),
            })
        })
        .collect()
}

/// Doctor checks for `delonix mcp doctor` — no IAM/OAuth to check, there is none
/// in this design (ADR-0025). `(check_name, ok, detail)`.
pub fn doctor_checks(base: &Path) -> Vec<(&'static str, bool, String)> {
    let mut checks = Vec::new();

    let containers_ok = Store::open(base.join("containers"));
    checks.push((
        "container_store",
        containers_ok.is_ok(),
        match &containers_ok {
            Ok(_) => "reachable".to_string(),
            Err(e) => e.to_string(),
        },
    ));

    let volumes_ok = delonix_volume::VolumeStore::open(base);
    checks.push((
        "volume_store",
        volumes_ok.is_ok(),
        match &volumes_ok {
            Ok(_) => "reachable".to_string(),
            Err(e) => e.to_string(),
        },
    ));

    let mcp_dir = base.join("mcp");
    let dir_ok = std::fs::create_dir_all(&mcp_dir).is_ok()
        && std::fs::metadata(&mcp_dir)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false);
    checks.push((
        "mcp_state_dir_writable",
        dir_ok,
        mcp_dir.display().to_string(),
    ));

    let bin_ok = std::env::current_exe().is_ok();
    checks.push((
        "runtime_binary_resolvable",
        bin_ok,
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| e.to_string()),
    ));

    checks
}

/// Same pattern `delonix-mgmt::run_cli` already ships for its own mutations:
/// invoke the `delonix` binary itself (this process IS `delonix mcp serve`) with
/// a fixed, non-shell-interpolated argv. Not a new mechanism (ADR-0025 §6).
fn run_cli_blocking(base: &Path, args: Vec<String>) -> Result<(bool, String), String> {
    let bin = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = std::process::Command::new(&bin)
        .env("DELONIX_ROOT", base)
        .args(&args)
        .output()
        .map_err(|e| e.to_string())?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), s.trim().to_string()))
}

fn run_cli(base: &Path, args: Vec<String>) -> Result<(bool, String), ErrorData> {
    run_cli_blocking(base, args).map_err(|e| typed_err("INTERNAL", e))
}

/// Starts the MCP server on stdio and runs until the peer disconnects.
pub async fn serve_stdio(base: PathBuf) -> Result<(), String> {
    let server = DelonixMcp::new(base).map_err(|e| e.to_string())?;
    let running = server.serve(stdio()).await.map_err(|e| e.to_string())?;
    running.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_kind_accepts_singular_and_plural_and_rejects_unknown() {
        assert_eq!(
            ResourceKind::parse("container").unwrap(),
            ResourceKind::Container
        );
        assert_eq!(
            ResourceKind::parse("containers").unwrap(),
            ResourceKind::Container
        );
        assert_eq!(ResourceKind::parse("VM").unwrap(), ResourceKind::Vm);
        assert_eq!(
            ResourceKind::parse("volumes").unwrap(),
            ResourceKind::Volume
        );
        assert!(ResourceKind::parse("workload").is_err());
        assert!(ResourceKind::parse("secret").is_err());
    }

    #[test]
    fn typed_err_maps_known_kinds_to_the_right_json_rpc_code() {
        assert_eq!(
            typed_err("NOT_FOUND", "x").code,
            ErrorCode::RESOURCE_NOT_FOUND
        );
        assert_eq!(
            typed_err("VALIDATION_FAILED", "x").code,
            ErrorCode::INVALID_PARAMS
        );
        assert_eq!(
            typed_err("POLICY_DENIED", "x").code,
            ErrorCode::INVALID_REQUEST
        );
        assert_eq!(typed_err("INTERNAL", "x").code, ErrorCode::INTERNAL_ERROR);
    }

    #[test]
    fn every_tool_parameter_type_generates_a_valid_json_schema() {
        // Reused by rmcp for each tool's input schema — a struct that fails to
        // derive a sane schema would silently break `tools/list` for an LLM
        // client, not just a doc string.
        for schema in [
            schemars::schema_for!(ResourceListParams),
            schemars::schema_for!(ResourceGetParams),
            schemars::schema_for!(LogsQueryParams),
            schemars::schema_for!(StorageInspectParams),
            schemars::schema_for!(ContainerRestartParams),
            schemars::schema_for!(TaskIdParams),
            schemars::schema_for!(AuditQueryParams),
        ] {
            let value = serde_json::to_value(&schema).unwrap();
            assert!(
                value.get("properties").is_some(),
                "schema has no properties: {value}"
            );
        }
    }

    #[test]
    fn capabilities_table_matches_the_risk_table_length() {
        assert_eq!(capabilities_table().len(), risk::TOOL_RISK.len());
    }
}
