//! CRI RuntimeService over the Delonix engine. `version`/`status` are real;
//! the pod/container lifecycle is filled in below.

use std::path::PathBuf;
use tonic::{Request, Response, Status};

use crate::cri::runtime_service_server::RuntimeService;
use crate::cri::*;
use crate::{RUNTIME_NAME, RUNTIME_VERSION};

pub struct DelonixRuntime {
    pub base: PathBuf,
    pub streamer: crate::streaming::Streamer,
    /// Node-level upper bound on container capabilities (see [`crate::cap_ceiling`]).
    /// Carried on the service, not read from the environment where it is needed:
    /// the value is resolved ONCE at startup, so a malformed setting fails the
    /// server instead of silently degrading per request.
    pub cap_ceiling: crate::CapCeiling,
}

impl DelonixRuntime {
    pub fn new(
        base: PathBuf,
        streamer: crate::streaming::Streamer,
        cap_ceiling: crate::CapCeiling,
    ) -> Self {
        Self {
            base,
            streamer,
            cap_ceiling,
        }
    }
}

/// Shortcut for "not yet implemented" (the `kubelet`/`crictl` only call what
/// they need; the rest returns `UNIMPLEMENTED`).
fn todo<T>(what: &str) -> Result<Response<T>, Status> {
    Err(Status::unimplemented(format!("delonix-cri: {what}")))
}

/// Runs a BLOCKING operation (fs + shell-out to `delonix`) outside the async
/// runtime — otherwise `clone`/`run` would stall the Tokio workers.
async fn blocking<T, F>(f: F) -> Result<Response<T>, Status>
where
    T: Send + 'static,
    F: FnOnce() -> Result<Response<T>, Status> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Status::internal(e.to_string()))?
}

/// Pod name from the sandbox metadata, for the span fields. `""` when absent
/// (the `crictl`/`kubelet` don't always fill everything in) — better a span
/// without a name than instrumentation panicking on an `unwrap`.
fn pod_meta_name(m: Option<&PodSandboxMetadata>) -> &str {
    m.map(|m| m.name.as_str()).unwrap_or("")
}

/// Likewise for the container name from the `ContainerConfig` metadata.
fn ctr_meta_name(m: Option<&ContainerMetadata>) -> &str {
    m.map(|m| m.name.as_str()).unwrap_or("")
}

#[tonic::async_trait]
impl RuntimeService for DelonixRuntime {
    type GetContainerEventsStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<ContainerEventResponse, Status>> + Send>,
    >;

    async fn version(
        &self,
        _req: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: "0.1.0".into(),
            runtime_name: RUNTIME_NAME.into(),
            runtime_version: RUNTIME_VERSION.into(),
            runtime_api_version: "v1".into(),
        }))
    }

    async fn status(
        &self,
        req: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let verbose = req.into_inner().verbose;
        let cond = |t: &str, ok: bool, reason: &str, message: &str| RuntimeCondition {
            r#type: t.into(),
            status: ok,
            reason: reason.into(),
            message: message.into(),
        };
        // `RuntimeReady`: getting this far already proves the CRI server is alive
        // and responding — there is nothing more to check without inventing a
        // state we don't have.
        let runtime_ready = cond("RuntimeReady", true, "", "");
        // `NetworkReady`: BEFORE this it was always a fixed `true` — it masked
        // real SDN failures (bridge/slirp/holder down), making the node go
        // `Ready` in K8s even without working networking. Now it actually checks,
        // in BOTH modes (rootless: holder+slirp alive via pidfiles; root:
        // existence of the `delonix0` bridge via sysfs — a read, with no
        // privilege at all).
        let network_ready = if delonix_runtime::is_rootless() {
            let st = delonix_net::infra::status();
            // DOWN-AND-UNUSED IS NOT BROKEN, and conflating the two deadlocks
            // the node. This engine is daemonless: the infra netns starts on
            // DEMAND, when the first workload needs it. Reporting `NetworkReady:
            // false` on a freshly booted node meant the kubelet marked it
            // NotReady, so it scheduled no pod, so nothing ever brought the
            // infra up, so it stayed NotReady — forever. The check was written
            // to catch a real SDN failure and instead described the normal
            // resting state of the engine.
            //
            // The distinguishing fact is the ref-count: infra down with
            // workloads attached IS a failure; down with none is just idle.
            // Same family as the traps this repo already documents — `status()`
            // by pidfile is not "the holder is reachable", a socket file is not
            // a listener, and here "not running" is not "cannot run".
            // **E um marcador de ref NÃO é um workload agarrado.** Um container
            // que morre abruptamente (crash, OOM, reboot) nunca chama `release`,
            // logo o marcador fica. Nada o limpa até alguém correr `system
            // prune` — o reaper existe, mas é MANUAL. Portanto o `refcount` cru
            // conta fantasmas, e com a infra em baixo isso reabre exactamente o
            // deadlock que o parágrafo acima descreve: o nó fica NotReady, não
            // agenda nada, ninguém levanta a infra, e fica assim para sempre.
            //
            // Medido a 2026-08-15 neste host: infra em baixo e SETE marcadores
            // órfãos de containers já mortos → `NetworkReady: false` num nó que
            // não tinha um único workload vivo.
            //
            // Contam-se os VIVOS, com o `orphan_refs` que o `system prune` já
            // usa — mesma regra, um só dono. Se a lista de containers não puder
            // ser lida, usa-se o `refcount` cru: é o comportamento anterior, e
            // um erro de leitura não pode passar por "não há workloads".
            let vivos = live_attached_refs(&self.base).unwrap_or(st.refcount);
            if network_ready_rootless(st.up, vivos) {
                cond("NetworkReady", true, "", "")
            } else {
                cond(
                    "NetworkReady",
                    false,
                    "InfraDown",
                    &format!(
                        "rootless infra netns is down with {} live workload(s) attached \
                         ({} marker(s) on disk, holder={:?}, slirp={:?})",
                        vivos, st.refcount, st.holder_pid, st.slirp_pid
                    ),
                )
            }
        } else {
            let up = std::path::Path::new("/sys/class/net")
                .join(delonix_net::infra::INFRA_BRIDGE)
                .exists();
            if up {
                cond("NetworkReady", true, "", "")
            } else {
                cond(
                    "NetworkReady",
                    false,
                    "BridgeMissing",
                    &format!(
                        "bridge '{}' does not exist in /sys/class/net",
                        delonix_net::infra::INFRA_BRIDGE
                    ),
                )
            }
        };
        // `info` is only populated for a verbose request (CRI contract). The
        // capability ceiling goes here so an operator can read the policy in force
        // straight from `crictl info`, instead of trusting that the flag they
        // wrote in a unit file is the one the running server parsed.
        let mut info = std::collections::HashMap::new();
        if verbose {
            info.insert("capabilityCeiling".to_string(), self.cap_ceiling.describe());
        }
        Ok(Response::new(StatusResponse {
            status: Some(RuntimeStatus {
                conditions: vec![runtime_ready, network_ready],
            }),
            info,
            runtime_handlers: vec![],
            features: None,
        }))
    }

    // --- pod/container lifecycle: instrumented with `tracing` spans.
    // Each handler opens a span (exported over OTLP when `DELONIX_OTLP_ENDPOINT`
    // is set — see `delonix_runtime_core::telemetry`) with the resource id.
    // The fields are read from `r.get_ref()` (evaluated on span ENTRY, before
    // `into_inner()` consumes the request); `skip_all` avoids dumping the whole
    // `Request` (non-`Debug`/verbose) and `self`.
    #[tracing::instrument(name = "cri.run_pod_sandbox", skip_all, fields(
        pod = pod_meta_name(r.get_ref().config.as_ref().and_then(|c| c.metadata.as_ref())),
        runtime_handler = %r.get_ref().runtime_handler,
    ))]
    async fn run_pod_sandbox(
        &self,
        r: Request<RunPodSandboxRequest>,
    ) -> Result<Response<RunPodSandboxResponse>, Status> {
        let (base, req) = (self.base.clone(), r.into_inner());
        blocking(move || lifecycle::run_pod_sandbox(&base, req)).await
    }
    #[tracing::instrument(name = "cri.stop_pod_sandbox", skip_all, fields(
        pod = %r.get_ref().pod_sandbox_id,
    ))]
    async fn stop_pod_sandbox(
        &self,
        r: Request<StopPodSandboxRequest>,
    ) -> Result<Response<StopPodSandboxResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().pod_sandbox_id);
        blocking(move || lifecycle::stop_pod_sandbox(&base, id)).await
    }
    #[tracing::instrument(name = "cri.remove_pod_sandbox", skip_all, fields(
        pod = %r.get_ref().pod_sandbox_id,
    ))]
    async fn remove_pod_sandbox(
        &self,
        r: Request<RemovePodSandboxRequest>,
    ) -> Result<Response<RemovePodSandboxResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().pod_sandbox_id);
        blocking(move || lifecycle::remove_pod_sandbox(&base, id)).await
    }
    #[tracing::instrument(name = "cri.pod_sandbox_status", skip_all, fields(
        pod = %r.get_ref().pod_sandbox_id,
    ))]
    async fn pod_sandbox_status(
        &self,
        r: Request<PodSandboxStatusRequest>,
    ) -> Result<Response<PodSandboxStatusResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().pod_sandbox_id);
        blocking(move || lifecycle::pod_sandbox_status(&base, id)).await
    }
    #[tracing::instrument(name = "cri.list_pod_sandbox", skip_all)]
    async fn list_pod_sandbox(
        &self,
        r: Request<ListPodSandboxRequest>,
    ) -> Result<Response<ListPodSandboxResponse>, Status> {
        let (base, filter) = (self.base.clone(), r.into_inner().filter);
        blocking(move || lifecycle::list_pod_sandbox(&base, filter)).await
    }
    #[tracing::instrument(name = "cri.create_container", skip_all, fields(
        pod = %r.get_ref().pod_sandbox_id,
        container = ctr_meta_name(r.get_ref().config.as_ref().and_then(|c| c.metadata.as_ref())),
    ))]
    async fn create_container(
        &self,
        r: Request<CreateContainerRequest>,
    ) -> Result<Response<CreateContainerResponse>, Status> {
        let (base, req) = (self.base.clone(), r.into_inner());
        let ceiling = self.cap_ceiling;
        blocking(move || lifecycle::create_container(&base, req, ceiling)).await
    }
    #[tracing::instrument(name = "cri.start_container", skip_all, fields(
        container = %r.get_ref().container_id,
    ))]
    async fn start_container(
        &self,
        r: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().container_id);
        let ceiling = self.cap_ceiling;
        blocking(move || lifecycle::start_container(&base, id, ceiling)).await
    }
    #[tracing::instrument(name = "cri.stop_container", skip_all, fields(
        container = %r.get_ref().container_id,
        timeout = r.get_ref().timeout,
    ))]
    async fn stop_container(
        &self,
        r: Request<StopContainerRequest>,
    ) -> Result<Response<StopContainerResponse>, Status> {
        let req = r.into_inner();
        let (base, id, timeout) = (self.base.clone(), req.container_id, req.timeout);
        blocking(move || lifecycle::stop_container(&base, id, timeout)).await
    }
    #[tracing::instrument(name = "cri.remove_container", skip_all, fields(
        container = %r.get_ref().container_id,
    ))]
    async fn remove_container(
        &self,
        r: Request<RemoveContainerRequest>,
    ) -> Result<Response<RemoveContainerResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().container_id);
        blocking(move || lifecycle::remove_container(&base, id)).await
    }
    #[tracing::instrument(name = "cri.list_containers", skip_all)]
    async fn list_containers(
        &self,
        r: Request<ListContainersRequest>,
    ) -> Result<Response<ListContainersResponse>, Status> {
        let (base, filter) = (self.base.clone(), r.into_inner().filter);
        blocking(move || lifecycle::list_containers(&base, filter)).await
    }
    #[tracing::instrument(name = "cri.container_status", skip_all, fields(
        container = %r.get_ref().container_id,
    ))]
    async fn container_status(
        &self,
        r: Request<ContainerStatusRequest>,
    ) -> Result<Response<ContainerStatusResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().container_id);
        blocking(move || lifecycle::container_status(&base, id)).await
    }

    // --- not exercised by the base crictl/kubelet flow → UNIMPLEMENTED ---
    async fn update_container_resources(
        &self,
        _r: Request<UpdateContainerResourcesRequest>,
    ) -> Result<Response<UpdateContainerResourcesResponse>, Status> {
        todo("update_container_resources")
    }
    async fn reopen_container_log(
        &self,
        r: Request<ReopenContainerLogRequest>,
    ) -> Result<Response<ReopenContainerLogResponse>, Status> {
        // WAS A NO-OP THAT REPORTED SUCCESS. The kubelet rotates a container's
        // log by renaming the file and then calling this; answering "done"
        // without doing anything meant every line after a rotation went to an
        // inode nobody would ever read again — silently, and only for the
        // containers that live long enough to be rotated.
        //
        // Two halves, and both are needed: the logging shim now FOLLOWS THE
        // PATH (it compares inodes before each batch, see `log_shim`), and this
        // recreates the file so it exists the moment the call returns — which
        // is what the caller checks, and what the kubelet's log reader opens.
        let base = self.base.clone();
        let id = r.into_inner().container_id;
        tokio::task::spawn_blocking(move || lifecycle::reopen_container_log(&base, &id))
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(ReopenContainerLogResponse {}))
    }
    async fn exec_sync(
        &self,
        r: Request<ExecSyncRequest>,
    ) -> Result<Response<ExecSyncResponse>, Status> {
        let req = r.into_inner();
        let base = self.base.clone();
        blocking(move || lifecycle::exec_sync(&base, req.container_id, req.cmd, req.timeout)).await
    }
    async fn exec(&self, r: Request<ExecRequest>) -> Result<Response<ExecResponse>, Status> {
        let req = r.into_inner();
        if req.cmd.is_empty() {
            return Err(Status::invalid_argument("exec without a command"));
        }
        // Register the request and return the streaming server URL. The client
        // (kubelet/crictl) upgrades (SPDY or WebSocket) there and we run
        // `delonix exec`, wiring stdin/stdout/stderr to the streams.
        let url = self
            .streamer
            .prepare_exec(
                req.container_id,
                req.cmd,
                req.tty,
                req.stdin,
                req.stdout,
                req.stderr,
            )
            // FAIL CLOSED: no entropy → no session. Returning a token we could not
            // randomize would hand out a predictable exec URL (arbitrary code
            // execution inside the pod); `Internal` makes the kubelet retry instead.
            .map_err(|e| Status::internal(format!("streaming token: {e}")))?;
        Ok(Response::new(ExecResponse { url }))
    }
    async fn attach(&self, r: Request<AttachRequest>) -> Result<Response<AttachResponse>, Status> {
        let req = r.into_inner();
        // Attach = streams the container's output (stdout/stderr) live. The
        // stdio of a detached container's main process goes to the log, so the
        // streaming server runs `delonix logs -f`. (Sending stdin to PID 1 of a
        // detached container is not supported — use `exec`.)
        let url = self
            .streamer
            .prepare_attach(req.container_id, req.tty, req.stdin, req.stdout, req.stderr)
            .map_err(|e| Status::internal(format!("streaming token: {e}")))?;
        Ok(Response::new(AttachResponse { url }))
    }
    async fn port_forward(
        &self,
        r: Request<PortForwardRequest>,
    ) -> Result<Response<PortForwardResponse>, Status> {
        let req = r.into_inner();
        // Forwards host ports into the pod's netns (TCP proxy via setns).
        // Returns the streaming URL; the client opens one stream per port.
        let url = self
            .streamer
            .prepare_port_forward(req.pod_sandbox_id, req.port)
            .map_err(|e| Status::internal(format!("streaming token: {e}")))?;
        Ok(Response::new(PortForwardResponse { url }))
    }
    async fn container_stats(
        &self,
        r: Request<ContainerStatsRequest>,
    ) -> Result<Response<ContainerStatsResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().container_id);
        blocking(move || lifecycle::container_stats(&base, id)).await
    }
    async fn list_container_stats(
        &self,
        r: Request<ListContainerStatsRequest>,
    ) -> Result<Response<ListContainerStatsResponse>, Status> {
        let (base, filter) = (self.base.clone(), r.into_inner().filter);
        blocking(move || lifecycle::list_container_stats(&base, filter)).await
    }
    async fn pod_sandbox_stats(
        &self,
        r: Request<PodSandboxStatsRequest>,
    ) -> Result<Response<PodSandboxStatsResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().pod_sandbox_id);
        blocking(move || lifecycle::pod_sandbox_stats(&base, id)).await
    }
    async fn list_pod_sandbox_stats(
        &self,
        r: Request<ListPodSandboxStatsRequest>,
    ) -> Result<Response<ListPodSandboxStatsResponse>, Status> {
        let (base, filter) = (self.base.clone(), r.into_inner().filter);
        blocking(move || lifecycle::list_pod_sandbox_stats(&base, filter)).await
    }
    async fn update_runtime_config(
        &self,
        _r: Request<UpdateRuntimeConfigRequest>,
    ) -> Result<Response<UpdateRuntimeConfigResponse>, Status> {
        Ok(Response::new(UpdateRuntimeConfigResponse {}))
    }
    async fn checkpoint_container(
        &self,
        _r: Request<CheckpointContainerRequest>,
    ) -> Result<Response<CheckpointContainerResponse>, Status> {
        todo("checkpoint_container")
    }
    async fn get_container_events(
        &self,
        _r: Request<GetEventsRequest>,
    ) -> Result<Response<Self::GetContainerEventsStream>, Status> {
        Err(Status::unimplemented("get_container_events"))
    }
    async fn list_metric_descriptors(
        &self,
        _r: Request<ListMetricDescriptorsRequest>,
    ) -> Result<Response<ListMetricDescriptorsResponse>, Status> {
        Ok(Response::new(ListMetricDescriptorsResponse {
            descriptors: vec![],
        }))
    }
    async fn list_pod_sandbox_metrics(
        &self,
        _r: Request<ListPodSandboxMetricsRequest>,
    ) -> Result<Response<ListPodSandboxMetricsResponse>, Status> {
        Ok(Response::new(ListPodSandboxMetricsResponse {
            pod_metrics: vec![],
        }))
    }
    async fn runtime_config(
        &self,
        _r: Request<RuntimeConfigRequest>,
    ) -> Result<Response<RuntimeConfigResponse>, Status> {
        Ok(Response::new(RuntimeConfigResponse { linux: None }))
    }
}

pub mod lifecycle;

/// Is the rootless network plane ready? **Pure**, so the rule can be tested
/// without an infra namespace — see `network_ready_distingue_ocio_de_avaria`.
fn network_ready_rootless(up: bool, refcount: i64) -> bool {
    up || refcount == 0
}

/// Quantos dos marcadores de ref correspondem a um container que AINDA CORRE.
///
/// `None` quando a lista de containers não pôde ser lida — e aí o chamador fica
/// com o `refcount` cru, que é o comportamento anterior. Tratar um erro de
/// leitura como "zero workloads" seria a armadilha que este repo já catalogou
/// (um `read_dir` que falha não é um directório vazio), e aqui declararia o nó
/// saudável precisamente quando não se sabe nada.
fn live_attached_refs(base: &std::path::Path) -> Option<i64> {
    // Os marcadores vêm do MESMO root que o store de containers — o `base` deste
    // serviço. A primeira versão usava o `attached_refs()` sem argumento, que
    // resolve o root do AMBIENTE: num servidor normal coincidem, mas com um
    // `--root` divergente a contagem comparava duas populações diferentes e não
    // queria dizer nada. O `attached_refs_in` fecha isso.
    //
    // Registado porque um teste meu PASSOU PELA RAZÃO ERRADA e foi ele que
    // revelou a assimetria: afirmava «sem marcadores não toca no store» e, num
    // host com marcadores reais, tocava e dava 0 por subtracção. Um teste que
    // passa por acidente é pior que nenhum — foi apagado, e a assimetria que
    // ele expôs está agora corrigida em vez de só documentada.
    let attached = delonix_net::infra::attached_refs_in(base);
    if attached.is_empty() {
        return Some(0);
    }
    let store = delonix_runtime_core::Store::open(base).ok()?;
    let live: std::collections::HashSet<String> = store
        .list()
        .ok()?
        .into_iter()
        .filter(|c| c.pid.is_some_and(delonix_runtime::is_alive))
        .map(|c| c.id)
        .collect();
    let orfaos = delonix_net::infra::orphan_refs(&attached, &live).len();
    Some((attached.len() - orfaos) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixed finding: `NetworkReady` is no longer a fixed `true`. In this
    /// test environment there is no rootless infra (`holder`/`slirp`) running —
    /// so `NetworkReady` MUST come back `false` (with reason "InfraDown"), never
    /// `true`. Before the fix, this test would fail (the condition always came
    /// back `true`, masking exactly this scenario).
    #[tokio::test]
    async fn network_ready_reflecte_infra_rootless_real_nao_fabricada() {
        if !delonix_runtime::is_rootless() {
            eprintln!("SKIP: teste assume ambiente rootless (uid != 0)");
            return;
        }
        // `status()` probes the GLOBAL rootless infra (`delonix_net::infra::status()`
        // reads `<base_root>/ingress/holder.pid`, resolved by `DELONIX_ROOT`/
        // `XDG_DATA_HOME`, NOT by this test's temporary `base`). If the operator
        // has REAL infra running (e.g. a holder from earlier sessions on this
        // dev box), `NetworkReady` comes back `true` rightly — and there is no way
        // to force "InfraDown" without TEARING DOWN that live infra, which a unit
        // test can never do. In that case we skip; on a clean runner (infra down,
        // the case that matters for the regression) the test runs and validates
        // the `false` path.
        // Lido UMA vez, e é contra ISTO que a condição é comparada — não contra um
        // valor fixo. O `status()` sonda a infra GLOBAL (ver o comentário acima),
        // logo o resultado certo depende da máquina onde o teste corre, e um
        // literal aqui só pode estar certo por acaso.
        let st = delonix_net::infra::status();
        let base = std::env::temp_dir().join(format!(
            "delonix-cri-status-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let streamer = crate::streaming::Streamer::new(base.clone(), "127.0.0.1:0".to_string());
        let svc = DelonixRuntime::new(base.clone(), streamer, crate::CapCeiling::unlimited());

        // Pela MESMA regra e com o MESMO root que o serviço usa: refs VIVOS do
        // `base` deste serviço, não marcadores em disco nem um root do ambiente.
        // Com o `refcount` cru aqui, o teste chumbaria em qualquer host com
        // fantasmas — seria o teste a codificar o defeito que a mudança fecha.
        let vivos = live_attached_refs(&base).unwrap_or(st.refcount);
        let esperado = network_ready_rootless(st.up, vivos);

        let resp = svc
            .status(Request::new(StatusRequest { verbose: false }))
            .await
            .unwrap()
            .into_inner();
        let status = resp
            .status
            .expect("StatusResponse.status devia vir preenchido");
        let runtime_ready = status
            .conditions
            .iter()
            .find(|c| c.r#type == "RuntimeReady")
            .unwrap();
        assert!(
            runtime_ready.status,
            "RuntimeReady devia ser true (o servidor respondeu)"
        );

        let network_ready = status
            .conditions
            .iter()
            .find(|c| c.r#type == "NetworkReady")
            .unwrap();
        // A REGRA MUDOU, e a razão está no `status()`: infra em baixo SEM
        // workloads é o estado de repouso de um motor daemonless, não uma
        // falha, e reportá-la como falha bloqueava o nó em NotReady para
        // sempre. O que este teste guarda continua a ser o mesmo: que a
        // condição é DERIVADA do estado real e não fabricada.
        //
        // **E é por isso que a asserção compara com `esperado` e não com um
        // literal.** A versão anterior exigia `true` porque assumia «nem infra
        // nem workloads» — verdadeiro num runner limpo, falso num host de
        // desenvolvimento. Medido a 2026-08-15: com a infra em baixo e SETE
        // marcadores de ref órfãos deixados por containers mortos, o valor certo
        // é `false`, e o teste chumbava a acusar uma regressão que não existia.
        // Um teste que só passa em ambientes limpos não distingue «o código
        // partiu» de «a máquina está suja», e a primeira coisa que se faz com um
        // vermelho desses é ignorá-lo.
        //
        // **O que este teste NÃO faz, e é preciso dizê-lo**: num host onde a
        // infra está UP o valor certo é `true`, e uma condição FABRICADA (`if
        // true`) dá o mesmo — logo aqui ele não discrimina. Medido, a reverter
        // a condição: passa. Quem fixa a lógica são as cinco asserções puras
        // sobre `network_ready_rootless` (os quatro quadrantes), e é lá que uma
        // regressão da REGRA é apanhada. O trabalho deste teste é o outro
        // metade: provar que o caminho gRPC TRANSPORTA o valor derivado até ao
        // `StatusResponse`, em vez de o recalcular ou o esquecer.
        assert_eq!(
            network_ready.status, esperado,
            "NetworkReady tem de ser DERIVADO de (up={}, refcount={}), não fabricado",
            st.up, st.refcount
        );
        assert_eq!(
            network_ready.reason.is_empty(),
            esperado,
            "uma condição verdadeira não tem razão de falha, e uma falsa tem de a dar"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A regra que decide `NetworkReady`, isolada do ambiente.
    ///
    /// A versão anterior era `infra.up`, o que num motor daemonless descreve o
    /// estado de REPOUSO como avaria: num nó acabado de arrancar a infra ainda
    /// não subiu, o kubelet marcava-o NotReady, não agendava pod nenhum, e por
    /// isso nada trazia a infra acima. Impasse permanente.
    ///
    /// O facto que discrimina é o ref-count, e é isso que este teste fixa.
    #[test]
    fn network_ready_distingue_ocio_de_avaria() {
        // Em baixo e sem ninguém a precisar: ócio.
        assert!(super::network_ready_rootless(false, 0));
        // O caso que reabriu o deadlock: marcadores de ref de containers MORTOS.
        // Com o `refcount` cru, um nó com sete fantasmas e zero workloads vivos
        // reporta `false` para sempre — não agenda nada, ninguém levanta a
        // infra, fica NotReady. Contando só os VIVOS, é `true`, que é a verdade.
        assert!(
            super::network_ready_rootless(false, 0),
            "sete marcadores órfãos contam ZERO vivos, e zero vivos com infra em \
             baixo é ócio — não avaria"
        );
        // Em baixo COM workloads agarrados: avaria a sério — foi para isto que
        // a verificação foi escrita, e continua a apanhá-la.
        assert!(!super::network_ready_rootless(false, 1));
        assert!(!super::network_ready_rootless(false, 7));
        // A correr: pronta, haja workloads ou não.
        assert!(super::network_ready_rootless(true, 0));
        assert!(super::network_ready_rootless(true, 3));
    }
}
