//! RuntimeService do CRI sobre o engine Delonix. `version`/`status` são reais;
//! o ciclo de vida de pods/containers é preenchido a seguir.

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

/// Atalho para "ainda não implementado" (o `kubelet`/`crictl` só chamam o que
/// precisam; o resto devolve `UNIMPLEMENTED`).
fn todo<T>(what: &str) -> Result<Response<T>, Status> {
    Err(Status::unimplemented(format!("delonix-cri: {what}")))
}

/// Corre uma operação BLOQUEANTE (fs + shell-out ao `delonix`) fora do runtime
/// async — senão o `clone`/`run` paralisava os workers do Tokio.
async fn blocking<T, F>(f: F) -> Result<Response<T>, Status>
where
    T: Send + 'static,
    F: FnOnce() -> Result<Response<T>, Status> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Status::internal(e.to_string()))?
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
        // `RuntimeReady`: chegar até aqui já prova que o servidor CRI está vivo
        // e a responder — não há mais nada a verificar sem inventar um estado
        // que não temos.
        let runtime_ready = cond("RuntimeReady", true, "", "");
        // `NetworkReady`: ANTES disto era sempre `true` fixo — mascarava
        // avarias reais da SDN (bridge/slirp/holder em baixo), fazendo o node
        // ficar `Ready` no K8s mesmo sem rede a funcionar. Agora verifica de
        // facto, nos DOIS modos (rootless: holder+slirp vivos via pidfiles;
        // root: existência do bridge `delonix0` via sysfs — leitura, sem
        // privilégio nenhum).
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
                        "netns de infra rootless em baixo (holder={:?}, slirp={:?})",
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
                        "bridge '{}' não existe em /sys/class/net",
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

    async fn run_pod_sandbox(
        &self,
        r: Request<RunPodSandboxRequest>,
    ) -> Result<Response<RunPodSandboxResponse>, Status> {
        let (base, req) = (self.base.clone(), r.into_inner());
        blocking(move || lifecycle::run_pod_sandbox(&base, req)).await
    }
    async fn stop_pod_sandbox(
        &self,
        r: Request<StopPodSandboxRequest>,
    ) -> Result<Response<StopPodSandboxResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().pod_sandbox_id);
        blocking(move || lifecycle::stop_pod_sandbox(&base, id)).await
    }
    async fn remove_pod_sandbox(
        &self,
        r: Request<RemovePodSandboxRequest>,
    ) -> Result<Response<RemovePodSandboxResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().pod_sandbox_id);
        blocking(move || lifecycle::remove_pod_sandbox(&base, id)).await
    }
    async fn pod_sandbox_status(
        &self,
        r: Request<PodSandboxStatusRequest>,
    ) -> Result<Response<PodSandboxStatusResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().pod_sandbox_id);
        blocking(move || lifecycle::pod_sandbox_status(&base, id)).await
    }
    async fn list_pod_sandbox(
        &self,
        _r: Request<ListPodSandboxRequest>,
    ) -> Result<Response<ListPodSandboxResponse>, Status> {
        let base = self.base.clone();
        blocking(move || lifecycle::list_pod_sandbox(&base)).await
    }
    async fn create_container(
        &self,
        r: Request<CreateContainerRequest>,
    ) -> Result<Response<CreateContainerResponse>, Status> {
        let (base, req) = (self.base.clone(), r.into_inner());
        blocking(move || lifecycle::create_container(&base, req)).await
    }
    async fn start_container(
        &self,
        r: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().container_id);
        blocking(move || lifecycle::start_container(&base, id)).await
    }
    async fn stop_container(
        &self,
        r: Request<StopContainerRequest>,
    ) -> Result<Response<StopContainerResponse>, Status> {
        let req = r.into_inner();
        let (base, id, timeout) = (self.base.clone(), req.container_id, req.timeout);
        blocking(move || lifecycle::stop_container(&base, id, timeout)).await
    }
    async fn remove_container(
        &self,
        r: Request<RemoveContainerRequest>,
    ) -> Result<Response<RemoveContainerResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().container_id);
        blocking(move || lifecycle::remove_container(&base, id)).await
    }
    async fn list_containers(
        &self,
        _r: Request<ListContainersRequest>,
    ) -> Result<Response<ListContainersResponse>, Status> {
        let base = self.base.clone();
        blocking(move || lifecycle::list_containers(&base)).await
    }
    async fn container_status(
        &self,
        r: Request<ContainerStatusRequest>,
    ) -> Result<Response<ContainerStatusResponse>, Status> {
        let (base, id) = (self.base.clone(), r.into_inner().container_id);
        blocking(move || lifecycle::container_status(&base, id)).await
    }

    // --- não exercitadas pelo fluxo base do crictl/kubelet → UNIMPLEMENTED ---
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
            return Err(Status::invalid_argument("exec sem comando"));
        }
        // Regista o pedido e devolve a URL do servidor de streaming. O cliente
        // (kubelet/crictl) faz upgrade (SPDY ou WebSocket) lá e nós corremos
        // `delonix exec`, ligando stdin/stdout/stderr às streams.
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
        // Attach = transmite o output (stdout/stderr) do container ao vivo. O
        // stdio do processo principal de um container detached vai para o log,
        // logo o servidor de streaming corre `delonix logs -f`. (Enviar stdin ao
        // PID 1 de um container detached não é suportado — usa `exec`.)
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
        // Encaminha portas do host para dentro do netns do pod (proxy TCP via
        // setns). Devolve a URL de streaming; o cliente abre uma stream por porta.
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

    /// O achado corrigido: `NetworkReady` deixou de ser `true` fixo. Neste
    /// ambiente de teste não há nenhuma infra rootless (`holder`/`slirp`) a
    /// correr — por isso `NetworkReady` TEM de vir `false` (com razão
    /// "InfraDown"), nunca `true`. Antes da correção, este teste falharia
    /// (a condição vinha sempre `true`, mascarando exactamente este cenário).
    #[tokio::test]
    async fn network_ready_reflecte_infra_rootless_real_nao_fabricada() {
        if !delonix_runtime::is_rootless() {
            eprintln!("SKIP: teste assume ambiente rootless (uid != 0)");
            return;
        }
        // `status()` sonda a infra rootless GLOBAL (`delonix_net::infra::status()`
        // lê `<base_root>/ingress/holder.pid`, resolvido por `DELONIX_ROOT`/
        // `XDG_DATA_HOME`, NÃO pelo `base` temporário deste teste). Se o operador
        // tiver infra REAL a correr (ex.: um holder de sessões anteriores neste
        // dev box), `NetworkReady` vem `true` com razão — e não há como forçar
        // "InfraDown" sem DERRUBAR essa infra viva, o que um teste unitário nunca
        // pode fazer. Neste caso salta-se; num runner limpo (infra em baixo, o caso
        // que importa para a regressão) o teste corre e valida o caminho `false`.
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
