//! `router` — o **Router Controller** de uma carga (container/pod/stack), no
//! modelo **broker de control-plane**: é o ÚNICO componente que fala com o
//! Ingress/Egress em nome da carga. NÃO há um proxy no data-path — o tráfego
//! continua a fluir directamente pelo kernel (nftables/DNAT); o router é quem
//! *programa* essas regras de forma centralizada e idempotente.
//!
//! Porquê um router por carga, e não publish ad-hoc espalhado:
//! - **Pod**: os membros partilham o netns do infra ("pause"). Todas as portas
//!   dos membros têm de ser publicadas no **IP do infra** — o router do pod. Há
//!   um único ponto onde o ingress mapeia o pod, qualquer que seja o membro que
//!   escuta a porta.
//! - **Container standalone**: o router é o próprio container (o seu IP).
//! - **Stack** (Fase 2): o router será o VIP estável do serviço, com o egress
//!   central da stack. Mesmo contrato, alvo diferente.
//!
//! Aqui encapsula-se a única diferença operacional que interessa ao chamador: o
//! caminho **root** (nft no netns do host, via [`crate::Net`]) vs **rootless**
//! (nft DENTRO do netns de infra, via [`crate::infra`]). O resto da decisão (qual
//! o IP do router, qual o id da carga) é resolvido por quem constrói o router.

use crate::Net;
use delonix_core::Result;

/// O alvo lógico de um Router: identidade legível + o IP que recebe o DNAT do
/// ingress (IP do infra para um pod; IP do próprio container para standalone).
#[derive(Debug, Clone)]
pub struct Router {
    /// Identidade lógica do router (ex.: `pod:web`, `container:nginx`). Cosmético
    /// para diagnóstico/observabilidade; não entra em comandos do kernel.
    pub owner: String,
    /// IP que recebe o DNAT das portas publicadas por esta carga.
    pub ip: String,
}

impl Router {
    /// Cria o router de um **pod**, ancorado no IP do seu infra container.
    pub fn for_pod(pod: &str, infra_ip: &str) -> Self {
        Router { owner: format!("pod:{pod}"), ip: infra_ip.to_string() }
    }

    /// Cria o router de um **container standalone**, ancorado no seu próprio IP.
    pub fn for_container(name: &str, ip: &str) -> Self {
        Router { owner: format!("container:{name}"), ip: ip.to_string() }
    }

    /// **Publica** `internal_port/proto` na `host_port` através do Ingress
    /// (DNAT host → `router.ip:internal_port`). Escolhe o caminho root vs rootless.
    /// Idempotência/limpeza ficam a cargo do chamador (que mantém o registo das
    /// portas e faz [`Router::unpublish`] antes de re-publicar, se preciso).
    pub fn publish(&self, host_port: u16, internal_port: u16, proto: &str, rootless: bool) -> Result<()> {
        let spec = format!("{host_port}:{internal_port}/{proto}");
        if rootless {
            crate::infra::publish_port(&self.ip, &spec)
        } else {
            Net.publish_port(&self.ip, &spec)
        }
    }

    /// **Retira** a publicação de uma `host_port` (remove o DNAT/`hostfwd`).
    /// Best-effort, simétrico de [`Router::publish`].
    pub fn unpublish(&self, host_port: u16, rootless: bool) {
        if rootless {
            crate::infra::unpublish_port(&host_port.to_string());
        } else {
            Net.unpublish_port(&self.ip, &host_port.to_string());
        }
    }
}
