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
}

impl DelonixRuntime {
    pub fn new(base: PathBuf, streamer: crate::streaming::Streamer) -> Self {
        Self { base, streamer }
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
        _req: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
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
            if st.up {
                cond("NetworkReady", true, "", "")
            } else {
                cond(
                    "NetworkReady",
                    false,
                    "InfraDown",
                    &format!(
                        "rootless infra netns is down (holder={:?}, slirp={:?})",
                        st.holder_pid, st.slirp_pid
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
        Ok(Response::new(StatusResponse {
            status: Some(RuntimeStatus {
                conditions: vec![runtime_ready, network_ready],
            }),
            info: Default::default(),
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
        _r: Request<ListPodSandboxRequest>,
    ) -> Result<Response<ListPodSandboxResponse>, Status> {
        let base = self.base.clone();
        blocking(move || lifecycle::list_pod_sandbox(&base)).await
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
        blocking(move || lifecycle::create_container(&base, req)).await
    }
    #[tracing::instrument(name = "cri.start_container", skip_all, fields(
        container = %r.get_ref().container_id,
    ))]
    async fn start_container(
        &self,
        r: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().container_id);
        blocking(move || lifecycle::start_container(&base, id)).await
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
        _r: Request<ListContainersRequest>,
    ) -> Result<Response<ListContainersResponse>, Status> {
        let base = self.base.clone();
        blocking(move || lifecycle::list_containers(&base)).await
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
        _r: Request<ReopenContainerLogRequest>,
    ) -> Result<Response<ReopenContainerLogResponse>, Status> {
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
        let url = self.streamer.prepare_exec(
            req.container_id,
            req.cmd,
            req.tty,
            req.stdin,
            req.stdout,
            req.stderr,
        );
        Ok(Response::new(ExecResponse { url }))
    }
    async fn attach(&self, r: Request<AttachRequest>) -> Result<Response<AttachResponse>, Status> {
        let req = r.into_inner();
        // Attach = streams the container's output (stdout/stderr) live. The
        // stdio of a detached container's main process goes to the log, so the
        // streaming server runs `delonix logs -f`. (Sending stdin to PID 1 of a
        // detached container is not supported — use `exec`.)
        let url = self.streamer.prepare_attach(
            req.container_id,
            req.tty,
            req.stdin,
            req.stdout,
            req.stderr,
        );
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
            .prepare_port_forward(req.pod_sandbox_id, req.port);
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
        if delonix_net::infra::status().up {
            eprintln!("SKIP: infra rootless ambiente a correr — não se pode provar InfraDown sem a derrubar");
            return;
        }
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
        let svc = DelonixRuntime::new(base.clone(), streamer);

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
        assert!(
            !network_ready.status,
            "NetworkReady devia ser FALSE (sem infra rootless a correr neste teste) — \
             se vier true sem verificação real, é a regressão que corrigimos"
        );
        assert_eq!(network_ready.reason, "InfraDown");
        assert!(
            !network_ready.message.is_empty(),
            "devia explicar a causa concreta"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
