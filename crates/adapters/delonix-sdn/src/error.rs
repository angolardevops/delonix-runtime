//! `delonix-sdn`'s own failures, grouped by what went wrong, and the
//! dictionary number of each group (ADR-0043) — the first crate to populate
//! the `Network` domain (digit 3) of the `DX-CDNN` table.
//!
//! **The messages are a contract with what was there before.** Each variant
//! carries the text the call site used to build by hand and converts into the
//! same shared class the crate always used — so the CLI prints byte for byte
//! what it printed and exits with the same code, now with a number attached.
//! Two exceptions, both deliberate, both matching what a syscall-shaped
//! failure could always exit with anyway: a subnet already claimed by another
//! network (`BaseOctetTaken`) and a network whose subnet cannot be changed in
//! place (`NetworkSubnetImmutable`) moved from `Invalid` to `Conflict` — the
//! shared class's own doc-comment names exactly this shape ("a resource with
//! the same name but a different state already exists") as what `Conflict`
//! is for.
//!
//! **`Command` is one number for every host-tool/syscall failure this crate
//! makes**, the same shape [`delonix-vm`](../../delonix_vm/enum.Error.html)'s
//! own `Command` variant already uses: the holder shells out to `ip`, `nft`,
//! `wg`, `bridge`, and a dozen other tools, always with the same
//! `context`+`message` shape the shared [`delonix_model::Error::Runtime`]
//! already carried. Giving each `context` string its own number would be a
//! number per invocation site, not per failure MEANING — the same call
//! (`ip link add`, say) failing for two different reasons is still "a system
//! call failed", which is what the number means.

use thiserror::Error;

/// A failure of `delonix-sdn` — the rootless SDN, the ingress firewall, CNI,
/// IPAM, and the WireGuard overlay.
#[derive(Debug, Error)]
pub enum Error {
    // ---- invalid argument --------------------------------------------------
    /// A string is not a WireGuard base64 public key (`wg::remove_peer`,
    /// `infra::del_wg_peer`).
    #[error("{0}")]
    InvalidWgKey(String),

    /// A CNI config/conflist/plugin-input JSON does not parse, or a conflist
    /// has no plugins.
    #[error("{0}")]
    CniConfigInvalid(String),

    /// Building the JSON a CNI plugin gets on stdin failed.
    #[error("{0}")]
    CniEncodeFailed(String),

    /// A CNI plugin's own stdout does not parse as its result.
    #[error("{0}")]
    CniResultInvalid(String),

    /// A CNI plugin named in the chain is not in `CNI_PATH`.
    #[error("{0}")]
    CniPluginNotFound(String),

    /// A netns sysctl key given to `set_netns_sysctls` is not a plain `net.*`
    /// name.
    #[error("{0}")]
    CniSysctlKeyInvalid(String),

    /// A `-p`/`--publish` spec: bad protocol, bad port, or a host address that
    /// is not an IPv4 literal.
    #[error("{0}")]
    InvalidPublishSpec(String),

    /// A `-p` port RANGE (`8000-8010:8000-8010`) with mismatched width, a
    /// non-numeric bound, or an out-of-range port.
    #[error("{0}")]
    InvalidPortRange(String),

    /// A `--net-bps`/`--net-burst` value that does not parse, is zero, or is
    /// out of range.
    #[error("{0}")]
    InvalidNetRate(String),

    /// A `NetworkStore` record on disk is missing a field its driver needs
    /// (parent, subnet, vni, base octet).
    #[error("{0}")]
    NetworkRecordCorrupted(String),

    /// The name asked for is `bridge` (the implicit default) or a reserved
    /// driver name (`host`/`none`).
    #[error("{0}")]
    ReservedNetworkName(String),

    /// A network name outside `[A-Za-z0-9_-]`.
    #[error("{0}")]
    InvalidNetworkName(String),

    /// A `--subnet` that is not a `10.<lo-hi>.0.0/16` this engine's bridge
    /// driver can realize.
    #[error("{0}")]
    SubnetNotSupported(String),

    /// An arbitrary `--subnet` CIDR that is not a private prefix of a usable
    /// length.
    #[error("{0}")]
    SubnetInvalid(String),

    /// A declared gateway outside the network's prefix, or the network/
    /// broadcast address of it.
    #[error("{0}")]
    InvalidGateway(String),

    /// A `/16` base octet outside the workload address space.
    #[error("{0}")]
    InvalidBaseOctet(String),

    /// `set_metadata` on the implicit default `bridge` network, which has no
    /// record to annotate.
    #[error("{0}")]
    NoRecordToAnnotate(String),

    /// A label/annotation key or value that cannot go into the record's
    /// `key=value` line format (contains `=`, a newline, or is empty).
    #[error("{0}")]
    InvalidMetadataKey(String),

    /// A VNI outside `1..16777215`, or one that does not parse.
    #[error("{0}")]
    InvalidVni(String),

    /// An overlay-peer operation on a network whose driver is not `overlay`.
    #[error("{0}")]
    NotAnOverlay(String),

    /// An overlay peer spec with no `node_ip`.
    #[error("{0}")]
    InvalidOverlayPeer(String),

    /// A `create_lan` driver that is neither `macvlan` nor `ipvlan`.
    #[error("{0}")]
    UnknownDriver(String),

    /// A `macvlan`/`ipvlan` parent NIC that does not exist on the host.
    #[error("{0}")]
    ParentNicMissing(String),

    /// A `macvlan`/`ipvlan` subnet that does not parse as a CIDR.
    #[error("{0}")]
    InvalidLanSubnet(String),

    /// A line on the holder's control socket this build does not recognise.
    #[error("{0}")]
    InvalidControlCommand(String),

    /// A hex payload on the control socket (a CNI conflist, a firewall spec)
    /// does not decode.
    #[error("{0}")]
    InvalidHex(String),

    /// An FDB/overlay-peer destination that is not a bare IP — refused before
    /// it reaches an `nft`/`bridge` argv, where it could smuggle a second
    /// command.
    #[error("{0}")]
    InvalidFdbDst(String),

    /// An attempt to remove the default ingress bridge.
    #[error("{0}")]
    IngressBridgeProtected(String),

    /// A load-balancer VIP outside the ingress address space.
    #[error("{0}")]
    VipOutsideIngressSpace(String),

    /// An `lbset` backend list: a bad weight, a backend with no port, or an
    /// empty list.
    #[error("{0}")]
    InvalidLbSpec(String),

    /// A port string (publish/unpublish/lbset) that is not `1..65535`.
    #[error("{0}")]
    InvalidPort(String),

    /// A protocol other than `tcp`/`udp` on a publish/unpublish.
    #[error("{0}")]
    InvalidProto(String),

    /// An `egress`/`egress net` policy other than `allow`/`deny`/
    /// `allowlist:<cidrs>`.
    #[error("{0}")]
    InvalidEgressPolicy(String),

    /// An `egress host` suffix that is not a plain DNS name.
    #[error("{0}")]
    InvalidEgressHostname(String),

    /// A container/publish IP outside the ingress address space
    /// (`10.200-254.x`).
    #[error("{0}")]
    IpOutsideIngressSpace(String),

    /// `apply_firewall`/`apply_firewall_all` called with no IP.
    #[error("{0}")]
    FirewallNoIp(String),

    /// A firewall spec (hex-decoded JSON) that does not parse as
    /// `ContainerFw`.
    #[error("{0}")]
    FirewallJsonInvalid(String),

    /// Encoding a `ContainerFw` to send over the control socket failed.
    #[error("{0}")]
    FirewallEncodeFailed(String),

    /// No free `/16` prefix left for a new ingress `NetDef`.
    #[error("{0}")]
    NoFreeIngressPrefix(String),

    /// An `attach_container_on_ip` IP outside the target network's subnet.
    #[error("{0}")]
    IpNotInSubnet(String),

    /// A [`crate::network_zone::NetworkZoneProvider`] registration refused —
    /// an empty id, or one whose id/alias already belongs to a different
    /// provider (ADR-0049 addendum, mirrors `GatewayProviderRegistrationRefused`).
    #[error("{0}")]
    NetworkZoneProviderRegistrationRefused(String),

    /// `kind: NetworkZone` was applied but nothing registered a
    /// [`crate::network_zone::NetworkZoneProvider`].
    #[error("{0}")]
    NoNetworkZoneProviderConfigured(String),

    /// More than one [`crate::network_zone::NetworkZoneProvider`] is
    /// registered — `kind: NetworkZone` has no field to disambiguate.
    #[error("{0}")]
    AmbiguousNetworkZoneProvider(String),

    // ---- not found ----------------------------------------------------
    /// No `NetworkRoute` between the given pair.
    #[error("{0}")]
    RouteNotFound(String),

    /// No `Service` of that name in that namespace.
    #[error("{0}")]
    ServiceNotFound(String),

    /// An ingress network the caller named has no `NetDef` — it was never
    /// realized (or was removed) on this holder.
    #[error("{0}")]
    IngressNetworkNotRealized(String),

    // ---- conflict -------------------------------------------------------
    /// A `NetworkStore` network with that name already exists.
    #[error("{0}")]
    NetworkAlreadyExists(String),

    /// The workload address space has no `/16` left to hand this network.
    #[error("{0}")]
    NoFreeSubnet(String),

    /// A requested CIDR overlaps an existing network's subnet.
    #[error("{0}")]
    SubnetOverlap(String),

    /// A network by this name already exists with a DIFFERENT subnet — its
    /// subnet cannot be changed in place.
    #[error("{0}")]
    NetworkSubnetImmutable(String),

    /// The `/16` a `create_with_base` was given is already used by another
    /// network.
    #[error("{0}")]
    BaseOctetTaken(String),

    /// An ingress `NetDef` is already realized on a different prefix than the
    /// registry now asks for.
    #[error("{0}")]
    NetworkPrefixConflict(String),

    // ---- unavailable ------------------------------------------------------
    /// The `wg` binary is missing from the host.
    #[error("{0}")]
    WgMissing(String),

    // ---- system failure -----------------------------------------------
    /// A host tool this crate shells out to (`ip`, `nft`, `wg`, `bridge`, a
    /// CNI plugin, `newuidmap`/`newgidmap`, a raw syscall) could not be run or
    /// exited badly.
    #[error("system call `{context}` failed: {message}")]
    Command {
        /// What was being done.
        context: &'static str,
        /// The tool's own error, or the underlying `errno`.
        message: String,
    },

    /// A failure of the layers underneath (filesystem, JSON, state), with its
    /// own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Engine(e.into())
    }
}

impl From<delonix_state::Error> for Error {
    fn from(e: delonix_state::Error) -> Self {
        Error::Engine(e.into())
    }
}

/// The shared class each `delonix-sdn` failure belongs to.
type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure (ADR-0043). A failure of the
    /// layers underneath keeps its own.
    ///
    /// The match is exhaustive on purpose, like [`delonix_model::Error::code`]'s:
    /// a variant added tomorrow stops the build here instead of being filed
    /// under a catch-all nobody ever revisits.
    pub fn number(&self) -> u16 {
        match self {
            Error::InvalidWgKey(_) => 1301,
            Error::CniConfigInvalid(_) => 1302,
            Error::CniEncodeFailed(_) => 1303,
            Error::CniResultInvalid(_) => 1304,
            Error::CniPluginNotFound(_) => 1305,
            Error::CniSysctlKeyInvalid(_) => 1306,
            Error::InvalidPublishSpec(_) => 1307,
            Error::InvalidPortRange(_) => 1308,
            Error::InvalidNetRate(_) => 1309,
            Error::NetworkRecordCorrupted(_) => 1310,
            Error::ReservedNetworkName(_) => 1311,
            Error::InvalidNetworkName(_) => 1312,
            Error::SubnetNotSupported(_) => 1313,
            Error::SubnetInvalid(_) => 1314,
            Error::InvalidGateway(_) => 1315,
            Error::InvalidBaseOctet(_) => 1316,
            Error::NoRecordToAnnotate(_) => 1317,
            Error::InvalidMetadataKey(_) => 1318,
            Error::InvalidVni(_) => 1319,
            Error::NotAnOverlay(_) => 1320,
            Error::InvalidOverlayPeer(_) => 1321,
            Error::UnknownDriver(_) => 1322,
            Error::ParentNicMissing(_) => 1323,
            Error::InvalidLanSubnet(_) => 1324,
            Error::InvalidControlCommand(_) => 1325,
            Error::InvalidHex(_) => 1326,
            Error::InvalidFdbDst(_) => 1327,
            Error::IngressBridgeProtected(_) => 1328,
            Error::VipOutsideIngressSpace(_) => 1329,
            Error::InvalidLbSpec(_) => 1330,
            Error::InvalidPort(_) => 1331,
            Error::InvalidProto(_) => 1332,
            Error::InvalidEgressPolicy(_) => 1333,
            Error::InvalidEgressHostname(_) => 1334,
            Error::IpOutsideIngressSpace(_) => 1335,
            Error::FirewallNoIp(_) => 1336,
            Error::FirewallJsonInvalid(_) => 1337,
            Error::FirewallEncodeFailed(_) => 1338,
            Error::NoFreeIngressPrefix(_) => 1339,
            Error::IpNotInSubnet(_) => 1340,
            Error::NetworkZoneProviderRegistrationRefused(_) => 1341,
            Error::NoNetworkZoneProviderConfigured(_) => 1342,
            Error::AmbiguousNetworkZoneProvider(_) => 1343,
            Error::RouteNotFound(_) => 4301,
            Error::ServiceNotFound(_) => 4302,
            Error::IngressNetworkNotRealized(_) => 4303,
            Error::NetworkAlreadyExists(_) => 5301,
            Error::NoFreeSubnet(_) => 5302,
            Error::SubnetOverlap(_) => 5303,
            Error::NetworkSubnetImmutable(_) => 5304,
            Error::BaseOctetTaken(_) => 5305,
            Error::NetworkPrefixConflict(_) => 5306,
            Error::WgMissing(_) => 6301,
            Error::Command { .. } => 9301,
            Error::Engine(e) => e.number(),
        }
    }

    /// «An argument is wrong».
    pub fn is_invalid_argument(&self) -> bool {
        self.number() / 1000 == 1
    }

    /// «No such resource».
    pub fn is_not_found(&self) -> bool {
        self.number() / 1000 == 4
    }

    /// «Already exists».
    pub fn is_conflict(&self) -> bool {
        self.number() / 1000 == 5
    }

    /// «A capability this host does not have».
    pub fn is_unavailable(&self) -> bool {
        self.number() / 1000 == 6
    }

    /// The shared class this failure converts into, for a caller that needs a
    /// variant's payload.
    pub fn into_root(self) -> Dx {
        Dx::from(self).into_root()
    }
}

impl From<Error> for Dx {
    fn from(e: Error) -> Self {
        let number = e.number();
        let class = match e {
            Error::RouteNotFound(text)
            | Error::ServiceNotFound(text)
            | Error::IngressNetworkNotRealized(text) => Dx::NotFound(text),
            Error::NetworkAlreadyExists(text)
            | Error::NoFreeSubnet(text)
            | Error::SubnetOverlap(text)
            | Error::NetworkSubnetImmutable(text)
            | Error::BaseOctetTaken(text)
            | Error::NetworkPrefixConflict(text) => Dx::Conflict(text),
            Error::WgMissing(text) => Dx::Unavailable(text),
            Error::Command { context, message } => Dx::Runtime { context, message },
            Error::Engine(e) => return e,
            e => Dx::Invalid(e.to_string()),
        };
        Dx::coded(number, class)
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    fn every_variant() -> Vec<Error> {
        vec![
            Error::InvalidWgKey("not a WireGuard public key: 'x'".into()),
            Error::CniConfigInvalid("invalid CNI config: x".into()),
            Error::CniEncodeFailed("serialize CNI config: x".into()),
            Error::CniResultInvalid("invalid CNI result: x".into()),
            Error::CniPluginNotFound("CNI plugin `bridge` not found in CNI_PATH".into()),
            Error::CniSysctlKeyInvalid("not a net.* sysctl: \"x\"".into()),
            Error::InvalidPublishSpec("invalid protocol in 'x' (tcp|udp)".into()),
            Error::InvalidPortRange("invalid port range in 'x' (ports must be numbers)".into()),
            Error::InvalidNetRate("invalid --net-bps: 'x'".into()),
            Error::NetworkRecordCorrupted("network 'x' is corrupted".into()),
            Error::ReservedNetworkName("'bridge' is the default network (reserved)".into()),
            Error::InvalidNetworkName("invalid network name: 'x'".into()),
            Error::SubnetNotSupported("subnet 'x': no prefix length".into()),
            Error::SubnetInvalid("subnet 'x': not an IPv4 prefix".into()),
            Error::InvalidGateway("gateway 'x': not an IPv4 address".into()),
            Error::InvalidBaseOctet("invalid /16 base octet: 5".into()),
            Error::NoRecordToAnnotate(
                "the default 'bridge' network has no record to annotate".into(),
            ),
            Error::InvalidMetadataKey("invalid metadata key: \"a=b\"".into()),
            Error::InvalidVni("invalid VNI (1..16777215)".into()),
            Error::NotAnOverlay("'x' is not an overlay".into()),
            Error::InvalidOverlayPeer("invalid peer (missing node_ip)".into()),
            Error::UnknownDriver("unknown driver: 'x'".into()),
            Error::ParentNicMissing("parent NIC 'eno1' does not exist on the host".into()),
            Error::InvalidLanSubnet("invalid subnet: 'x' (e.g. 192.168.1.0/24)".into()),
            Error::InvalidControlCommand("invalid control command: \"x\"".into()),
            Error::InvalidHex("invalid hex".into()),
            Error::InvalidFdbDst("invalid FDB dst: 'x'".into()),
            Error::IngressBridgeProtected("the default ingress bridge cannot be removed".into()),
            Error::VipOutsideIngressSpace("VIP fora do espaço de ingress: x".into()),
            Error::InvalidLbSpec("lbset sem backends".into()),
            Error::InvalidPort("invalid port".into()),
            Error::InvalidProto("invalid proto: x".into()),
            Error::InvalidEgressPolicy("invalid egress policy: x".into()),
            Error::InvalidEgressHostname("invalid hostname: \"x\"".into()),
            Error::IpOutsideIngressSpace("IP x outside the ingress space (10.200-254.x)".into()),
            Error::FirewallNoIp("firewall: no IP given".into()),
            Error::FirewallJsonInvalid("firewall JSON: x".into()),
            Error::FirewallEncodeFailed("x".into()),
            Error::NoFreeIngressPrefix("no free /16 prefixes for ingress networks".into()),
            Error::IpNotInSubnet("IP x does not belong to network y (10.201.0.0/16)".into()),
            Error::NetworkZoneProviderRegistrationRefused(
                "network zone provider 'x' cannot claim the name 'y': it already belongs to 'z'"
                    .into(),
            ),
            Error::NoNetworkZoneProviderConfigured(
                "kind: NetworkZone has no registered provider".into(),
            ),
            Error::AmbiguousNetworkZoneProvider(
                "kind: NetworkZone has 2 registered providers (a, b)".into(),
            ),
            Error::RouteNotFound("route: a -> b".into()),
            Error::ServiceNotFound("service: default/web".into()),
            Error::IngressNetworkNotRealized("ingress network 'x' does not exist".into()),
            Error::NetworkAlreadyExists("network 'x' already exists".into()),
            Error::NoFreeSubnet("no free /16 left for network 'x'".into()),
            Error::SubnetOverlap("subnet 10.50.0.0/16 overlaps network 'x' (10.50.0.0/16)".into()),
            Error::NetworkSubnetImmutable("network 'x' already exists as 10.50.0.0/16".into()),
            Error::BaseOctetTaken("10.50.0.0/16 is already used by network 'x'".into()),
            Error::NetworkPrefixConflict("network 'x' is already realized on 10.50".into()),
            Error::WgMissing("'wg' is not available on this host".into()),
            Error::Command {
                context: "spawn",
                message: "No such file or directory".into(),
            },
            Error::Engine(delonix_model::Error::Conflict("x".into())),
        ]
    }

    #[test]
    fn every_failure_keeps_its_number_through_the_conversion() {
        for e in every_variant() {
            let number = e.number();
            let shown = e.to_string();
            let converted = delonix_model::Error::from(e);
            assert_eq!(converted.number(), number, "{shown}");
            assert!(
                delonix_model::codes::lookup(number).is_some(),
                "DX-{number:04} ({shown}) has no dictionary entry"
            );
        }
    }

    /// The text the CLI printed before this type existed: the shared class
    /// wraps the local variant's message verbatim.
    #[test]
    fn the_converted_message_is_the_one_printed_before() {
        let cases = [
            (
                Error::InvalidNetworkName("invalid network name: 'a b'".into()),
                "invalid argument: invalid network name: 'a b'",
            ),
            (
                Error::NetworkAlreadyExists("network 'db' already exists".into()),
                "conflict: network 'db' already exists",
            ),
            (
                Error::RouteNotFound("route: a -> b".into()),
                "no such route: a -> b",
            ),
            (
                Error::WgMissing("'wg' is not available on this host".into()),
                "unavailable: 'wg' is not available on this host",
            ),
            (
                Error::Command {
                    context: "netns pin",
                    message: "the pin exited before creating its namespaces".into(),
                },
                "system call `netns pin` failed: the pin exited before creating its namespaces",
            ),
            (
                Error::CniPluginNotFound("CNI plugin `bridge` not found in CNI_PATH".into()),
                "invalid argument: CNI plugin `bridge` not found in CNI_PATH",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(delonix_model::Error::from(e).to_string(), want);
        }
    }
}
