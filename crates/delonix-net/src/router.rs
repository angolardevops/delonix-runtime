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

    /// Cria o router de uma **stack** (Fase 2), ancorado no **VIP estável** do
    /// serviço (`crate::service_vip`). É o ÚNICO ponto de entrada externo da stack:
    /// o ingress publica no VIP e o VIP balanceia para as réplicas (`set_lb`). Toda
    /// a comunicação stack↔exterior passa por aqui (modelo broker).
    pub fn for_stack(stack: &str, vip: &str) -> Self {
        Router { owner: format!("stack:{stack}"), ip: vip.to_string() }
    }

    /// **(Re)programa o balanceador L4** do router para as `backends`
    /// (`ip:port`), via nftables (round-robin por-ligação). É como o router da
    /// stack distribui o tráfego que entra pelo seu VIP. No modo root usa o
    /// `numgen`/conntrack do `Net`; no rootless o roteamento de serviço do ingress
    /// é tratado à parte (no-op aqui). Idempotente.
    pub fn set_lb(&self, backends: &[String], rootless: bool) -> Result<()> {
        if rootless {
            return Ok(());
        }
        Net.set_service_lb(&self.ip, backends)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_and_ip_per_kind() {
        let p = Router::for_pod("web", "10.200.1.5");
        assert_eq!(p.owner, "pod:web");
        assert_eq!(p.ip, "10.200.1.5");

        let c = Router::for_container("nginx", "10.200.2.7");
        assert_eq!(c.owner, "container:nginx");
        assert_eq!(c.ip, "10.200.2.7");

        // O router da stack ancora no VIP estável do serviço.
        let vip = crate::service_vip("shop_web");
        let s = Router::for_stack("shop", &vip);
        assert_eq!(s.owner, "stack:shop");
        assert_eq!(s.ip, vip);
        assert!(s.ip.starts_with("10.90."), "VIP fora do espaço de serviço: {}", s.ip);
    }

    #[test]
    fn set_lb_rootless_is_noop() {
        // No rootless o roteamento de serviço é tratado à parte → não falha.
        let r = Router::for_stack("s", &crate::service_vip("s_svc"));
        assert!(r.set_lb(&["10.200.1.2:80".into()], true).is_ok());
    }
}
