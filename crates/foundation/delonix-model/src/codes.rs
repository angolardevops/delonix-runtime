//! The dictionary of numbered codes, `DX-CDNN` (ADR-0043).
//!
//! A code is four digits: the thousands digit is the [`Class`] — what the caller
//! does next —, the hundreds digit is the [`Domain`] — where it happened —, and the
//! last two number the failure within that class and domain (`00` is the class
//! itself). `DX-4201` reads «not found, storage» before anyone opens this table.
//!
//! **A number never changes meaning** and is never reused. The message may be
//! reworded and is translated; the number is the contract. Everything that shows a
//! code — the CLI error line, `delonix explain`, the generated page, `-o json`, the
//! node API — reads this one table.

use crate::Error;

/// What the caller does next. The thousands digit of a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// It worked, or a notice that is not a failure.
    Success,
    /// An argument is wrong: fix it and run again.
    InvalidArgument,
    /// The command line itself is wrong, or a plan has changes pending.
    Usage,
    /// The resource exists but is not running.
    NotRunning,
    /// There is no such resource.
    NotFound,
    /// The name is already taken.
    Conflict,
    /// A capability this host does not have.
    Unavailable,
    /// The operating system refused on a permission.
    PermissionDenied,
    /// The deadline passed with the work unfinished.
    Timeout,
    /// The kernel, the filesystem, the state records or a registry failed.
    SystemFailure,
}

impl Class {
    /// Every class, in digit order.
    pub const ALL: [Class; 10] = [
        Class::Success,
        Class::InvalidArgument,
        Class::Usage,
        Class::NotRunning,
        Class::NotFound,
        Class::Conflict,
        Class::Unavailable,
        Class::PermissionDenied,
        Class::Timeout,
        Class::SystemFailure,
    ];

    /// The thousands digit.
    pub fn digit(self) -> u16 {
        Class::ALL.iter().position(|c| *c == self).unwrap_or(0) as u16
    }

    /// The exit code a failure of this class answers. For [`Class::SystemFailure`]
    /// it is the default; an entry may say `74` (I/O) instead.
    pub fn exit_code(self) -> i32 {
        use crate::exitcode as x;
        match self {
            Class::Success => 0,
            Class::InvalidArgument => x::GENERIC,
            Class::Usage => 2,
            Class::NotRunning => x::NOT_RUNNING,
            Class::NotFound => x::NOT_FOUND,
            Class::Conflict => x::CONFLICT,
            Class::Unavailable => x::UNAVAILABLE,
            Class::PermissionDenied => x::NO_PERMISSION,
            Class::Timeout => x::TIMEOUT,
            Class::SystemFailure => x::GENERIC,
        }
    }

    /// The class name, as printed by `delonix explain`.
    pub fn name(self) -> &'static str {
        match self {
            Class::Success => "success",
            Class::InvalidArgument => "invalid argument",
            Class::Usage => "invalid usage",
            Class::NotRunning => "not running",
            Class::NotFound => "not found",
            Class::Conflict => "conflict",
            Class::Unavailable => "unavailable",
            Class::PermissionDenied => "permission denied",
            Class::Timeout => "timeout",
            Class::SystemFailure => "system failure",
        }
    }
}

/// Where it happened. The hundreds digit of a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// The engine as a whole, or no domain in particular.
    Engine,
    /// Containers and pods.
    Container,
    /// Volumes and storage.
    Volume,
    /// Networks, routes, policies, ingress.
    Network,
    /// Images, builds and scans.
    Image,
    /// Virtual machines.
    Vm,
    /// Stacks, manifests and compose files.
    Stack,
    /// Clusters and the CRI.
    Cluster,
    /// Secrets and security policy.
    Security,
    /// The host, the CLI and external tools.
    Host,
}

impl Domain {
    /// Every domain, in digit order.
    pub const ALL: [Domain; 10] = [
        Domain::Engine,
        Domain::Container,
        Domain::Volume,
        Domain::Network,
        Domain::Image,
        Domain::Vm,
        Domain::Stack,
        Domain::Cluster,
        Domain::Security,
        Domain::Host,
    ];

    /// The hundreds digit.
    pub fn digit(self) -> u16 {
        Domain::ALL.iter().position(|d| *d == self).unwrap_or(0) as u16
    }

    /// The domain name, as printed by `delonix explain`.
    pub fn name(self) -> &'static str {
        match self {
            Domain::Engine => "engine",
            Domain::Container => "container",
            Domain::Volume => "volume",
            Domain::Network => "network",
            Domain::Image => "image",
            Domain::Vm => "vm",
            Domain::Stack => "stack",
            Domain::Cluster => "cluster",
            Domain::Security => "security",
            Domain::Host => "host",
        }
    }
}

/// One entry of the dictionary.
#[derive(Debug)]
pub struct Code {
    /// The number, `CDNN`.
    pub number: u16,
    /// A stable, readable id (`volume.not_found`).
    pub id: &'static str,
    /// What the caller does next.
    pub class: Class,
    /// Where it happened.
    pub domain: Domain,
    /// The exit code the CLI answers with it.
    pub exit: i32,
    /// The message, in English (translated by the CLI's catalogue).
    pub message: &'static str,
    /// What it means.
    pub meaning: &'static str,
    /// What to do about it.
    pub remedy: &'static str,
}

impl Code {
    /// `DX-4201`.
    pub fn label(&self) -> String {
        label(self.number)
    }
}

/// `DX-4201` for `4201`.
pub fn label(number: u16) -> String {
    format!("DX-{number:04}")
}

/// Reads `DX-4201`, `dx-4201`, `DX4201` or `4201`. `None` for anything else.
pub fn parse(text: &str) -> Option<u16> {
    let t = text.trim();
    let digits = t
        .strip_prefix("DX-")
        .or_else(|| t.strip_prefix("dx-"))
        .or_else(|| t.strip_prefix("DX"))
        .or_else(|| t.strip_prefix("dx"))
        .unwrap_or(t);
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The entry for a number, if the dictionary has one.
pub fn lookup(number: u16) -> Option<&'static Code> {
    CATALOG.iter().find(|c| c.number == number)
}

macro_rules! code {
    ($n:literal, $id:literal, $class:ident, $domain:ident, $exit:expr, $msg:literal, $meaning:literal, $remedy:literal) => {
        Code {
            number: $n,
            id: $id,
            class: Class::$class,
            domain: Domain::$domain,
            exit: $exit,
            message: $msg,
            meaning: $meaning,
            remedy: $remedy,
        }
    };
}

/// The whole dictionary, sorted by number.
pub static CATALOG: &[Code] = &[
    code!(0, "success", Success, Engine, 0,
        "success",
        "The command did what it was asked.",
        "Nothing."),
    code!(1000, "invalid_argument", InvalidArgument, Engine, 1,
        "invalid argument",
        "An argument, a flag value or a manifest field is not acceptable. The message names which.",
        "Correct the argument the message names and run the command again."),
    code!(1101, "container.ambiguous_name", InvalidArgument, Container, 1,
        "ambiguous container name",
        "The name exists in more than one namespace, so it does not say which container.",
        "Qualify it as <namespace>/<name>."),
    code!(1102, "container.invalid_memory_limit", InvalidArgument, Container, 1,
        "invalid memory limit",
        "The --memory value, or a Kubernetes resources.limits.memory, is not a byte count this engine understands.",
        "Use bytes (67108864) or a suffix (64M, 64Mi, 1G, 1Gi)."),
    code!(1103, "container.invalid_cpu_limit", InvalidArgument, Container, 1,
        "invalid CPU limit",
        "The --cpus value, or a Kubernetes resources.limits.cpu, is not a number of cores this engine understands.",
        "Use a decimal number of cores (0.5, 2); millicores (500m) are a Kubernetes spelling this flag does not take."),
    code!(1104, "container.invalid_cgroup_weight", InvalidArgument, Container, 1,
        "invalid cgroup weight",
        "A weight value (--cpu-weight, --io-weight) is outside the cgroup v2 range.",
        "Pass a weight between 1 and 10000."),
    code!(1105, "container.invalid_cpuset", InvalidArgument, Container, 1,
        "invalid CPU set",
        "The --cpuset value does not parse as a CPU list (e.g. 0-3,6).",
        "Pass a comma-separated list of CPU numbers or ranges."),
    code!(1106, "container.invalid_command_argv", InvalidArgument, Container, 1,
        "invalid command argument",
        "A command or entrypoint argument contains a NUL byte, which cannot be passed to execve.",
        "Remove the embedded NUL byte from the argument the message names."),
    code!(1107, "container.empty_command", InvalidArgument, Container, 1,
        "empty command",
        "The container has no command to run: neither the image's ENTRYPOINT/CMD nor an explicit command was given.",
        "Pass a command, or use an image with an ENTRYPOINT or CMD."),
    code!(1108, "container.no_processes", InvalidArgument, Container, 1,
        "no processes in the container",
        "An operation needing at least one live process (renice) found none.",
        "Check the container is actually running (`delonix container ps`)."),
    code!(1109, "container.unsafe_mount_path", InvalidArgument, Container, 1,
        "unsafe live-mount path",
        "A container update --volume-add/--volume-rm target is refused by the same path-safety check bind mounts use.",
        "Use a target path that stays inside the container's own root filesystem."),
    code!(1110, "container.mount_source_missing", InvalidArgument, Container, 1,
        "live-mount source does not exist",
        "The host path given to a live bind-mount (container update --volume-add) does not exist.",
        "Check the source path on the host exists before mounting it live."),
    code!(1111, "container.shares_host_mount_namespace", InvalidArgument, Container, 1,
        "container shares the host mount namespace",
        "A live mount/unmount was asked of a container with no private mount namespace to change.",
        "This container cannot take a live volume change; stop and recreate it if one is needed."),
    code!(1112, "container.live_mount_failed", InvalidArgument, Container, 1,
        "live mount failed",
        "The forked helper that performs a live bind-mount inside the running container's namespaces exited with a failure, or was interrupted.",
        "Read the message for the exit code; retry the volume-add, or restart the container."),
    code!(1113, "container.live_unmount_failed", InvalidArgument, Container, 1,
        "live unmount failed",
        "The forked helper that performs a live unmount inside the running container's namespaces did not succeed.",
        "Retry the volume-rm, or restart the container."),
    code!(1114, "container.invalid_cdi_device_name", InvalidArgument, Container, 1,
        "invalid CDI device name",
        "A --device value naming a CDI-qualified device is not vendor.com/class=name.",
        "Use the vendor.com/class=name form (e.g. nvidia.com/gpu=0)."),
    code!(1115, "container.kube_cgroup_needs_root", InvalidArgument, Container, 1,
        "kubelet cgroup placement needs the root runtime",
        "Placing a container in the cgroup hierarchy a CRI RunPodSandbox names needs privileges rootless, or an already-namespaced process, does not have.",
        "Run the CRI server as root, or do not request a kube cgroup parent in rootless."),
    code!(1201, "volume.invalid_name", InvalidArgument, Volume, 1,
        "invalid volume name",
        "A volume name may only use letters, digits, dot, dash and underscore, and cannot start with a dot.",
        "Pick a name made of [A-Za-z0-9._-] that does not start with a dot."),
    code!(1202, "volume.missing_device", InvalidArgument, Volume, 1,
        "network volume without a device",
        "An nfs, cifs or webdav volume was asked for without the share to mount.",
        "Pass the share: --opt server=<host> --opt share=<path>."),
    code!(1203, "volume.no_device", InvalidArgument, Volume, 1,
        "network volume record without a device",
        "A network volume is registered but its record lost the device to mount.",
        "Recreate the volume with its server and share."),
    code!(1204, "volume.invalid_snapshot_name", InvalidArgument, Volume, 1,
        "invalid snapshot name",
        "A snapshot name may only use letters, digits, dot, dash and underscore, with no slash or double dot.",
        "Pick a name made of [A-Za-z0-9._-]."),
    code!(1205, "volume.quota_on_non_empty", InvalidArgument, Volume, 1,
        "hard quota on a volume with data",
        "A hard quota (loopback) can only be set on an empty volume: it would hide the data already there.",
        "Create the volume with --quota, or empty it first."),
    code!(1206, "volume.quota_below_usage", InvalidArgument, Volume, 1,
        "quota below current usage",
        "The new quota is smaller than what the volume already holds.",
        "Free space in the volume first, or ask for a larger quota."),
    code!(1207, "volume.in_use", InvalidArgument, Volume, 1,
        "volume in use",
        "Shrinking a quota needs the volume unmounted, and a container is using it.",
        "Stop the containers that mount it, then change the quota."),
    code!(1208, "volume.ambiguous", InvalidArgument, Volume, 1,
        "ambiguous volume name",
        "The name is both a volume in the workload's namespace and a global volume.",
        "Rename one of the two."),
    code!(1209, "volume.invalid_spec", InvalidArgument, Volume, 1,
        "invalid volume spec",
        "A -v value is not source:/target[:options].",
        "Write it as source:/target, optionally with :ro."),
    code!(1210, "volume.unsupported_bind_option", InvalidArgument, Volume, 1,
        "unsupported bind option",
        "The third field of -v names an option this engine does not implement (SELinux :z/:Z, :U).",
        "Use :ro, :rw or a propagation option (:rprivate, :rslave, :rshared)."),
    code!(1211, "volume.relative_target", InvalidArgument, Volume, 1,
        "mount target is not absolute",
        "The path inside the container must start with a slash.",
        "Write the target as an absolute path, like /data."),
    code!(1212, "volume.truenas_quota_too_small", InvalidArgument, Volume, 1,
        "quota below TrueNAS' minimum",
        "TrueNAS refuses a quota under 1 GiB on a dataset; asking for less does not get rounded up, it gets refused.",
        "Ask for at least 1 GiB, or drop the quota to leave the dataset unlimited."),
    code!(1213, "volume.truenas_invalid_url", InvalidArgument, Volume, 1,
        "invalid TrueNAS url",
        "The provisioning target's url has no scheme, an unsupported scheme, or no host.",
        "Write it as https://<host>[:port], with no path."),
    code!(1214, "volume.truenas_credential_in_url", InvalidArgument, Volume, 1,
        "credential in the TrueNAS url",
        "The url carries a username or password (userinfo), which is a credential hidden in the manifest instead of a kind: Secret.",
        "Remove the userinfo and pass the credential as apiKeySecret or passwordSecret."),
    code!(1215, "volume.truenas_insecure_credential", InvalidArgument, Volume, 1,
        "credential over plain http",
        "Sending an API key or password to a plain http:// TrueNAS target would put it on the wire in the clear.",
        "Use https://, with insecureTLS: true if the appliance serves its own self-signed certificate."),
    code!(1216, "volume.truenas_invalid_dataset_name", InvalidArgument, Volume, 1,
        "invalid TrueNAS dataset name",
        "A ZFS dataset name has to be <pool>/<name>, with no '..', no empty component and no leading '-'.",
        "Write the dataset as <pool>/<name>, e.g. tank/projects."),
    code!(1301, "network.invalid_wg_key", InvalidArgument, Network, 1,
        "invalid WireGuard key",
        "A string given as a WireGuard key is not 44 characters of base64 ending in '='.",
        "Pass a key exactly as `wg genkey`/`wg pubkey` produced it."),
    code!(1302, "network.cni_config_invalid", InvalidArgument, Network, 1,
        "invalid CNI configuration",
        "A CNI config/conflist file, or a plugin's input built from it, is not valid JSON or has the wrong shape.",
        "Check the `*.conflist`/`*.conf` file the message names; validate it with a JSON linter."),
    code!(1303, "network.cni_encode_failed", InvalidArgument, Network, 1,
        "could not encode a CNI plugin's input",
        "Building the JSON a CNI plugin reads on stdin failed.",
        "Check the plugin entry in the conflist for a value JSON cannot represent."),
    code!(1304, "network.cni_result_invalid", InvalidArgument, Network, 1,
        "invalid CNI plugin result",
        "A CNI plugin's own stdout did not parse as the result the CNI spec expects.",
        "Check the plugin binary the message names is the version this host's conflist expects."),
    code!(1305, "network.cni_plugin_not_found", InvalidArgument, Network, 1,
        "CNI plugin not found",
        "A plugin named in the CNI chain (or its `ipam.type`) is not a binary in CNI_PATH.",
        "Install the plugin binary the message names into CNI_PATH (default /opt/cni/bin)."),
    code!(1306, "network.cni_sysctl_key_invalid", InvalidArgument, Network, 1,
        "invalid netns sysctl key",
        "A sysctl key given to be set inside a netns is not a plain `net.*` name.",
        "Use a dotted `net.*` key, as `sysctl` itself would accept."),
    code!(1307, "network.invalid_publish_spec", InvalidArgument, Network, 1,
        "invalid publish spec",
        "A `-p`/`--publish` spec has a protocol other than tcp/udp, a non-numeric port, or a host address that is not an IPv4 literal.",
        "Write it as [hostIp:]hostPort:contPort[/tcp|udp], e.g. 8080:80 or 0.0.0.0:8080:80/tcp."),
    code!(1308, "network.invalid_port_range", InvalidArgument, Network, 1,
        "invalid publish port range",
        "A `-p` port RANGE has a non-numeric bound, an out-of-order/out-of-range port, or mismatched host/container widths.",
        "Use hostStart-hostEnd:contStart-contEnd with both sides the same width, within 1-65535."),
    code!(1309, "network.invalid_net_rate", InvalidArgument, Network, 1,
        "invalid network rate spec",
        "A `--net-bps`/`--net-burst` value does not parse, is zero, or is out of range.",
        "Use a number with an optional k/m/g/t suffix, e.g. 10mbit or 512k."),
    code!(1310, "network.record_corrupted", InvalidArgument, Network, 1,
        "network record is corrupted",
        "A NetworkStore record on disk is missing a field its driver needs (parent, subnet, vni, or the base octet).",
        "Remove the network and recreate it (`delonix network rm`, then `delonix network create`)."),
    code!(1311, "network.reserved_network_name", InvalidArgument, Network, 1,
        "reserved network name",
        "The name asked for is `bridge` (the implicit default network) or a reserved driver name (`host`/`none`).",
        "Pick a different name."),
    code!(1312, "network.invalid_network_name", InvalidArgument, Network, 1,
        "invalid network name",
        "A network name may only use letters, digits, dash and underscore.",
        "Pick a name made of [A-Za-z0-9_-]."),
    code!(1313, "network.subnet_not_supported", InvalidArgument, Network, 1,
        "unsupported bridge subnet",
        "A --subnet for the bridge driver is not a 10.<workload-range>.0.0/16 this engine's IPAM can realize.",
        "Pass a 10.<lo>-<hi>.0.0/16 in the workload range, or omit --subnet to let the engine pick a free one."),
    code!(1314, "network.subnet_invalid", InvalidArgument, Network, 1,
        "invalid subnet",
        "An arbitrary --subnet CIDR is not a private prefix (RFC 1918) of a usable length.",
        "Use a private range (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16) with a prefix between /8 and /28."),
    code!(1315, "network.invalid_gateway", InvalidArgument, Network, 1,
        "invalid network gateway",
        "A declared gateway is outside the network's prefix, or is the network/broadcast address.",
        "Pick an address inside the network's prefix that is not the network or broadcast address."),
    code!(1316, "network.invalid_base_octet", InvalidArgument, Network, 1,
        "invalid /16 base octet",
        "A /16 base octet for a bridge network is outside the workload address space.",
        "Pick a base octet inside the workload range this engine reports."),
    code!(1317, "network.no_record_to_annotate", InvalidArgument, Network, 1,
        "no record to annotate",
        "The implicit default `bridge` network has no record on disk, so there is nothing to label/annotate.",
        "Annotate a user network instead; the default network cannot carry labels."),
    code!(1318, "network.invalid_metadata_key", InvalidArgument, Network, 1,
        "invalid network metadata key or value",
        "A label/annotation key is empty or contains '=' or a newline, or its value contains a newline.",
        "Use a key/value with no '=' and no newline."),
    code!(1319, "network.invalid_vni", InvalidArgument, Network, 1,
        "invalid VNI",
        "An overlay network's VNI does not parse, or is outside 1..16777215.",
        "Pass a VNI between 1 and 16777215."),
    code!(1320, "network.not_an_overlay", InvalidArgument, Network, 1,
        "not an overlay network",
        "An overlay-peer operation was asked of a network whose driver is not `overlay`.",
        "Target a network created with `--driver overlay`."),
    code!(1321, "network.invalid_overlay_peer", InvalidArgument, Network, 1,
        "invalid overlay peer",
        "An overlay peer spec has no node_ip.",
        "Write the peer as <node_ip> or <node_ip>=<pubkey>=<wg_ip>."),
    code!(1322, "network.unknown_driver", InvalidArgument, Network, 1,
        "unknown network driver",
        "A `create_lan` driver is neither `macvlan` nor `ipvlan`.",
        "Use --driver macvlan or --driver ipvlan."),
    code!(1323, "network.parent_nic_missing", InvalidArgument, Network, 1,
        "parent NIC not found",
        "A macvlan/ipvlan network's parent NIC does not exist on this host.",
        "Pass a NIC that `ip link` on this host actually shows."),
    code!(1324, "network.invalid_lan_subnet", InvalidArgument, Network, 1,
        "invalid macvlan/ipvlan subnet",
        "A macvlan/ipvlan subnet does not parse as a CIDR.",
        "Write it as e.g. 192.168.1.0/24."),
    code!(1325, "network.invalid_control_command", InvalidArgument, Network, 1,
        "invalid holder control command",
        "A line sent on the holder's control socket is not one this build recognises.",
        "Check the CLI and the holder are from the same build; an old holder may need a respawn."),
    code!(1326, "network.invalid_hex", InvalidArgument, Network, 1,
        "invalid hex payload",
        "A hex-encoded payload on the holder control socket (a CNI conflist, a firewall spec) does not decode.",
        "This is an internal protocol error between the CLI and the holder; check they are the same build."),
    code!(1327, "network.invalid_fdb_dst", InvalidArgument, Network, 1,
        "invalid FDB/overlay peer destination",
        "An FDB or overlay-peer destination is not a bare IP address.",
        "Pass a plain IPv4 address, with no other characters."),
    code!(1328, "network.ingress_bridge_protected", InvalidArgument, Network, 1,
        "the default ingress bridge cannot be removed",
        "An attempt was made to remove the default ingress bridge, which every other network path depends on.",
        "Remove a user network instead; the default bridge is not removable."),
    code!(1329, "network.vip_outside_ingress_space", InvalidArgument, Network, 1,
        "VIP outside the ingress address space",
        "A load-balancer VIP is outside the ingress address space (10.200-254.x).",
        "Use an address inside the ingress range this engine reports."),
    code!(1330, "network.invalid_lb_spec", InvalidArgument, Network, 1,
        "invalid load-balancer spec",
        "An `lbset` backend list has a bad weight, a backend with no port, an invalid backend, or is empty.",
        "Write backends as ip:port[#weight], comma-separated, with at least one entry."),
    code!(1331, "network.invalid_port", InvalidArgument, Network, 1,
        "invalid port",
        "A port string given to the ingress dataplane (publish/unpublish/lbset) is not 1..65535.",
        "Pass a numeric port between 1 and 65535."),
    code!(1332, "network.invalid_proto", InvalidArgument, Network, 1,
        "invalid protocol",
        "A protocol other than tcp/udp was given to a publish/unpublish call.",
        "Use tcp or udp."),
    code!(1333, "network.invalid_egress_policy", InvalidArgument, Network, 1,
        "invalid egress policy",
        "An egress policy is not allow, deny, or allowlist:<cidrs>.",
        "Use allow, deny, or allowlist:<comma-separated CIDRs>."),
    code!(1334, "network.invalid_egress_hostname", InvalidArgument, Network, 1,
        "invalid egress hostname",
        "An `egress host` suffix is not a plain DNS name.",
        "Use a plain hostname/domain suffix, e.g. example.com."),
    code!(1335, "network.ip_outside_ingress_space", InvalidArgument, Network, 1,
        "IP outside the ingress address space",
        "A container/publish IP is outside the ingress address space (10.200-254.x).",
        "Attach the container to a network inside the ingress range this engine reports."),
    code!(1336, "network.firewall_no_ip", InvalidArgument, Network, 1,
        "firewall applied with no IP",
        "apply_firewall/apply_firewall_all was called with an empty IP list.",
        "This is an internal call error; report it if you see it from the CLI."),
    code!(1337, "network.firewall_json_invalid", InvalidArgument, Network, 1,
        "invalid firewall spec",
        "A firewall spec (hex-decoded JSON sent to the holder) does not parse as the expected structure.",
        "Check the CLI and the holder are from the same build."),
    code!(1338, "network.firewall_encode_failed", InvalidArgument, Network, 1,
        "could not encode a firewall spec",
        "Encoding a container's firewall to send over the control socket failed.",
        "This is an internal call error; report it if you see it from the CLI."),
    code!(1339, "network.no_free_ingress_prefix", InvalidArgument, Network, 1,
        "no free ingress prefix",
        "No /16 prefix is left in the ingress address space for a new network.",
        "Remove an unused network (`delonix network rm`) to free a prefix."),
    code!(1340, "network.ip_not_in_subnet", InvalidArgument, Network, 1,
        "IP does not belong to network",
        "A fixed IP given to attach a container is not inside the target network's subnet.",
        "Pass an address inside the network's own subnet (`delonix network inspect <name>`)."),
    code!(1401, "image.empty_sbom", InvalidArgument, Image, 1,
        "empty SBOM",
        "The image has no apk or dpkg package database, so there is nothing to scan for vulnerabilities.",
        "Scan an image built on a distribution with a package manager, or scan its base image instead."),
    code!(1402, "image.advisory_db_invalid", InvalidArgument, Image, 1,
        "invalid advisory database",
        "The advisory database the scanner loaded is not valid JSON.",
        "Sync it again with `delonix image scan --update --feed <url>`."),
    code!(1403, "image.osv_feed_not_json", InvalidArgument, Image, 1,
        "OSV feed is not JSON",
        "The feed given to the scanner could not be parsed as JSON.",
        "Check the --feed URL points at an OSV JSON export, not an HTML page."),
    code!(1404, "image.osv_feed_shape", InvalidArgument, Image, 1,
        "OSV feed has the wrong shape",
        "The feed is JSON but neither an array of advisories nor an object with a vulns array.",
        "Point --feed at an OSV export: an array of advisories, or an object with a vulns array."),
    code!(1405, "image.not_an_odoo_module", InvalidArgument, Image, 1,
        "not an Odoo module",
        "The directory named as a module has no __manifest__.py.",
        "Point the scan at the module's own directory, the one holding __manifest__.py."),
    code!(1406, "image.no_odoo_module", InvalidArgument, Image, 1,
        "no Odoo module found",
        "No directory under the path given has a __manifest__.py.",
        "Point the scan at the directory that contains the modules."),
    code!(1407, "image.dockerfile_invalid", InvalidArgument, Image, 1,
        "invalid Dockerfile",
        "The build file does not parse, or asks for an instruction or flag this engine does not support. The message names the line.",
        "Fix the line the message names; `delonix build --help` lists what is supported."),
    code!(1408, "image.archive_invalid", InvalidArgument, Image, 1,
        "invalid image archive",
        "The archive being loaded is incomplete or corrupt: a missing manifest, or a blob that does not hash to its name.",
        "Export the archive again from its source and load the new file."),
    code!(1409, "image.layer_failed", InvalidArgument, Image, 1,
        "a layer could not be packed or unpacked",
        "Reading, compressing or extracting a layer or a root filesystem failed. The message carries the underlying error.",
        "Read the error in the message; check free space in the state root (`delonix system df`) and try again."),
    code!(1410, "image.no_layers", InvalidArgument, Image, 1,
        "image has no layers",
        "The image record has no layers to mount as a root filesystem.",
        "Pull or build the image again."),
    code!(1411, "image.state_root_unusable", InvalidArgument, Image, 1,
        "the state root cannot back an overlay",
        "The state root path contains a character (':') that the overlay mount options cannot express.",
        "Move the state root to a path without ':' (DELONIX_ROOT)."),
    code!(1412, "image.signing_key_invalid", InvalidArgument, Image, 1,
        "invalid signing key",
        "A signing or verification key is not a valid P-256 key in PEM, or could not be generated.",
        "Pass a cosign-compatible ECDSA P-256 key in PEM."),
    code!(1413, "image.not_signed", InvalidArgument, Image, 1,
        "image not signed",
        "The registry has no cosign signature for the image's digest.",
        "Sign it with `delonix image sign`, or pull a signed image."),
    code!(1414, "image.signature_invalid", InvalidArgument, Image, 1,
        "invalid signature",
        "A signature exists but does not verify against the given key, or its manifest or payload is malformed.",
        "Check the key is the one the image was signed with; do not run an image whose signature fails."),
    code!(1415, "image.signature_unknown", InvalidArgument, Image, 1,
        "signature state unknown",
        "The registry did not answer the signature lookup, so whether the image is signed could not be decided. This is not a verdict.",
        "Retry when the registry is reachable; the message carries the registry's error."),
    code!(1416, "image.already_signed", InvalidArgument, Image, 1,
        "image already signed",
        "A signature for this digest already exists.",
        "Pass --force to sign it again."),
    code!(1417, "image.signing_failed", InvalidArgument, Image, 1,
        "could not sign",
        "Producing or uploading the signature failed.",
        "Read the error in the message and check the registry login (`delonix image login`)."),
    code!(1501, "vm.unsupported_by_backend", InvalidArgument, Vm, 1,
        "not supported on this backend",
        "The pause, unpause, snapshot, restore or delete-snapshot verb asked for is not implemented by this VM's backend.",
        "Use a backend that supports it (libvirt supports the most), or drop the verb."),
    code!(1502, "vm.backend_registration_refused", InvalidArgument, Vm, 1,
        "invalid VM backend registration",
        "A backend registration had no id, claimed auto-selection from outside this crate, or claimed a name another backend already has.",
        "Fix the registration the message describes; this is a programming error in the process that registered it, not a manifest/flag."),
    code!(1503, "vm.unknown_backend", InvalidArgument, Vm, 1,
        "unknown VM backend",
        "The --backend/DELONIX_VM_BACKEND value does not name a registered backend.",
        "Use one of the backends the message lists."),
    code!(1504, "vm.unregistered_backend_in_record", InvalidArgument, Vm, 1,
        "VM record names an unregistered backend",
        "The VM's own record names a backend this process has no registration for.",
        "Run a build/process that registers that backend, or recreate the VM with one that is registered here."),
    code!(1505, "vm.requires_libvirt_backend", InvalidArgument, Vm, 1,
        "requires the libvirt backend",
        "The VM's spec.volumes need virtio-9p, which the Cloud Hypervisor backend does not implement.",
        "Remove `backend: cloud-hypervisor`, or drop the volumes."),
    code!(1506, "vm.socket_path_too_long", InvalidArgument, Vm, 1,
        "VM socket path too long",
        "The VM's api-socket or console socket path would not fit in a UNIX socket's sun_path (108 bytes on Linux).",
        "Use a shorter VM name or a shorter DELONIX_ROOT."),
    code!(1507, "vm.invalid_name", InvalidArgument, Vm, 1,
        "invalid VM name",
        "A VM name may only use letters, digits, dot, dash and underscore, with no path traversal.",
        "Pick a name made of [A-Za-z0-9._-] with no '/', '..', or leading '-'."),
    code!(1508, "vm.namespace_unsupported", InvalidArgument, Vm, 1,
        "namespace not enforceable on this backend",
        "The VM's backend does not put its VMs on this engine's SDN, so namespace isolation cannot be enforced.",
        "Use `--backend cloud-hypervisor`, or drop `--namespace`."),
    code!(1509, "vm.disk_too_small", InvalidArgument, Vm, 1,
        "--disk-size smaller than the base image",
        "A qcow2 overlay cannot be smaller than its backing file.",
        "Ask for a --disk-size at least as large as the base image."),
    code!(1510, "vm.invalid_snapshot_name", InvalidArgument, Vm, 1,
        "invalid snapshot name",
        "A VM snapshot name may only use letters, digits, dot, dash and underscore, with no path traversal.",
        "Pick a name made of [A-Za-z0-9._-] with no '/', '..', or leading '-'."),
    code!(1511, "vm.static_ip_requires_nat", InvalidArgument, Vm, 1,
        "--ip requires the libvirt nat mode",
        "A static IP is materialized as a DHCP reservation on the libvirt network, which only the nat/network net mode has.",
        "Drop --net-mode, or set it to nat, to use --ip; on a host bridge, reserve the IP on the LAN's own DHCP instead."),
    code!(1512, "vm.invalid_static_ip", InvalidArgument, Vm, 1,
        "invalid static IP",
        "The --ip value does not parse as an IPv4 address.",
        "Pass a dotted-quad IPv4 address."),
    code!(1513, "vm.static_ip_reservation_failed", InvalidArgument, Vm, 1,
        "could not reserve the static IP",
        "virsh net-update refused the DHCP host reservation for this MAC/IP pair.",
        "Read the virsh reason in the message; the IP may be outside the network's subnet."),
    code!(1514, "vm.live_backup_needs_libvirt", InvalidArgument, Vm, 1,
        "live disk backup needs the libvirt backend",
        "A live (running-VM) disk backup uses virsh blockcommit, which only the libvirt backend has.",
        "Stop the VM and take an offline snapshot instead, or recreate it on the libvirt backend."),
    code!(1515, "vm.not_running_for_op", InvalidArgument, Vm, 1,
        "VM not in the right state for this verb",
        "pause needs a running VM and unpause needs a paused one; the VM is in neither state.",
        "Check `delonix vm status <name>` and use the verb that matches its current state."),
    code!(1516, "vm.user_data_copy_failed", InvalidArgument, Vm, 1,
        "could not copy the cloud-init user-data",
        "The file a --user-data override names could not be copied into the VM's seed directory.",
        "Check the path exists and is readable by this user."),
    code!(1517, "vm.proxmox_no_such_node", InvalidArgument, Vm, 1,
        "no such Proxmox node",
        "The node named in the target is not part of the cluster this url answers for.",
        "Point the target at a node `GET /nodes` on this cluster actually reports, or fix the url."),
    code!(1518, "vm.proxmox_invalid_url", InvalidArgument, Vm, 1,
        "invalid Proxmox url",
        "The target url has no scheme, a scheme other than https, or no host.",
        "Write it as https://<host>:8006, with no path."),
    code!(1519, "vm.proxmox_credential_in_url", InvalidArgument, Vm, 1,
        "credential in the Proxmox url",
        "The url carries a username or password (userinfo), which is a credential hidden in the manifest instead of a kind: Secret.",
        "Remove the userinfo and pass the credential as a Proxmox API token or password secret."),
    code!(1520, "vm.proxmox_invalid_bridge_name", InvalidArgument, Vm, 1,
        "invalid Proxmox bridge name",
        "A bridge name is interpolated into the VM's net0 property, and has to be a valid Linux interface name.",
        "Use a name like vmbr0: letters, digits, '-', '_' or '.', up to 15 characters."),
    code!(1521, "vm.proxmox_invalid_node_name", InvalidArgument, Vm, 1,
        "invalid Proxmox node name",
        "A node name goes into a URL path on every call, and has to be what Proxmox itself accepts.",
        "Use letters, digits, '-' and '.', matching the node's name in `pvecm status`."),
    code!(1522, "vm.proxmox_invalid_disk_spec", InvalidArgument, Vm, 1,
        "invalid Proxmox disk spec",
        "A VM's disk on this backend has to name a template to clone or a storage and size on the node; a local path has no meaning there.",
        "Write `template:<vmid>` to clone a template, or `<storage>:<size-in-GiB>` for a fresh disk, e.g. local-lvm:8."),
    code!(1523, "vm.proxmox_invalid_snapshot_name", InvalidArgument, Vm, 1,
        "invalid Proxmox snapshot name",
        "A snapshot name goes into a URL path and into Proxmox's own namespace, and 'current' is reserved for the live state.",
        "Use letters, digits, '-' and '_', and not 'current'."),
    code!(1524, "vm.proxmox_unsupported_field", InvalidArgument, Vm, 1,
        "unsupported field on the Proxmox backend",
        "The manifest asks for a local kernel/initrd/seed/device, a QEMU tuning knob the node owns, or a libvirt-only escape hatch — none of which this remote backend can honour.",
        "Remove the field the message names, or use a local backend (`--backend libvirt`)."),
    code!(1525, "vm.proxmox_no_handle", InvalidArgument, Vm, 1,
        "VM has no Proxmox handle",
        "The VM's record carries no Proxmox handle, so it was not created by this backend and this backend does not know its vmid.",
        "Manage this VM with the backend that created it."),
    code!(1526, "vm.proxmox_bad_request", InvalidArgument, Vm, 1,
        "Proxmox rejected the request",
        "The node answered HTTP 400/422: a parameter of the request is not acceptable to it; the message carries the node's own reason.",
        "Read the node's reason in the message and correct the field it names."),
    code!(1527, "vm.unknown_capability", InvalidArgument, Vm, 1,
        "unknown capability",
        "A `required_capabilities` entry (`vm create --require`, `spec.requiredCapabilities`) names something the capability catalog does not have — a typo, or a name from another catalog version. It is refused before any backend is asked.",
        "Use a name from `delonix provider ls` (the catalog version is in its header)."),
    code!(1528, "vm.proxmox_invalid_firewall_rule", InvalidArgument, Vm, 1,
        "invalid Proxmox firewall rule",
        "A rule of the node's OWN firewall (not this engine's SDN firewall) named a `type` other than 'in'/'out', or an `action` other than ACCEPT/DROP/REJECT — a firewall group's name is a different kind of value this client does not validate and does not accept.",
        "Use 'in' or 'out' for the type, and ACCEPT, DROP or REJECT for the action."),
    code!(1529, "vm.proxmox_invalid_cloudinit_kind", InvalidArgument, Vm, 1,
        "invalid Proxmox cloud-init dump type",
        "cloudinit_dump's `type` has to be one of the three the node's cloud-init route accepts, and is refused here before any request goes to the node.",
        "Ask for 'user', 'network' or 'meta'."),
    code!(1801, "security.invalid_secret_name", InvalidArgument, Security, 1,
        "invalid secret name",
        "A secret name may only use lowercase letters, digits, dot, dash and underscore, up to 64 characters.",
        "Pick a name made of [a-z0-9._-]."),
    code!(1802, "security.invalid_env_key", InvalidArgument, Security, 1,
        "invalid secret key",
        "A secret key must be a valid environment variable name: a letter or underscore, then letters, digits or underscores.",
        "Rename the key, for example DB_PASSWORD."),
    code!(1803, "security.invalid_credential_name", InvalidArgument, Security, 1,
        "invalid credential name",
        "A credential name may only use lowercase letters, digits, dot, dash, underscore and colon.",
        "Pick a name made of [a-z0-9._:-]."),
    code!(1804, "security.corrupt_master_key", InvalidArgument, Security, 1,
        "corrupted master key",
        "The host's master key file has the wrong size, so nothing encrypted with it can be read.",
        "Restore the key from a node snapshot (`delonix system snapshot restore`); without it the encrypted secrets are lost."),
    code!(1805, "security.vault_failure", InvalidArgument, Security, 1,
        "the secret vault could not encrypt or decrypt",
        "Encryption or decryption failed: a blob that is corrupt, or a key that is not the one it was sealed with.",
        "Check the master key is the host's own; recreate the secret if its file is corrupt."),
    code!(1901, "host.apparmor_unavailable", InvalidArgument, Host, 1,
        "AppArmor profile unavailable",
        "The requested AppArmor profile could not be loaded: AppArmor is not enabled on this host, apparmor_parser is not on PATH, or it refused the profile.",
        "Enable AppArmor on this host, install apparmor_parser, or pass --security-opt apparmor=unconfined."),
    code!(1902, "host.no_cdi_spec", InvalidArgument, Host, 1,
        "no CDI spec found",
        "A --gpus/--device request named a CDI-qualified device, but this host has no generated CDI spec at all.",
        "Generate one with `nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml` (installing nvidia-container-toolkit first if needed)."),
    code!(2000, "usage", Usage, Engine, 2,
        "invalid usage",
        "The command line does not parse: an unknown subcommand, a missing argument or a flag this command does not take.",
        "Run the command with --help and follow its usage line."),
    code!(2601, "stack.changes_pending", Usage, Stack, 2,
        "the plan has changes",
        "`stack plan --detailed-exitcode` found differences between the manifest and what is running. Not a failure.",
        "Review the plan, then run `delonix stack apply` if the changes are wanted."),
    code!(3000, "not_running", NotRunning, Engine, 3,
        "not running",
        "The resource exists but is not running, and the operation needs it running.",
        "Start it (`delonix container start <name>`, `delonix vm start <name>`) and try again."),
    code!(3101, "container.not_running", NotRunning, Container, 3,
        "container is not running",
        "The container exists, but the operation needs it running.",
        "Start it first (`delonix container start <name>`)."),
    code!(4000, "not_found", NotFound, Engine, 4,
        "no such resource",
        "Nothing with that name or id exists in this scope.",
        "Check the name and the namespace with the matching `ls`, or create the resource first."),
    code!(4101, "container.not_found", NotFound, Container, 4,
        "no such container",
        "No container answers to that id, id prefix or name in this state root.",
        "List them with `delonix container ps -a`."),
    code!(4102, "container.cdi_device_not_found", NotFound, Container, 4,
        "CDI device not found",
        "The device name is not declared by any discovered CDI spec.",
        "Check the device name against the spec (`nvidia-ctk cdi list`), or regenerate it."),
    code!(4201, "volume.not_found", NotFound, Volume, 4,
        "no such volume",
        "There is no volume with that name in this scope.",
        "List them with `delonix volume ls` (-A for every namespace), or create it with `delonix volume create`."),
    code!(4202, "volume.snapshot_not_found", NotFound, Volume, 4,
        "no such snapshot",
        "The volume has no snapshot with that name.",
        "List its snapshots with `delonix volume snapshot ls <volume>`."),
    code!(4301, "network.route_not_found", NotFound, Network, 4,
        "no such NetworkRoute",
        "There is no NetworkRoute between the given pair of networks.",
        "List routes with `delonix network route ls`, or create one with `delonix network route <from> <to>`."),
    code!(4302, "network.service_not_found", NotFound, Network, 4,
        "no such Service",
        "There is no Service of that name in that namespace.",
        "List services with `delonix get services`."),
    code!(4303, "network.ingress_network_not_realized", NotFound, Network, 4,
        "ingress network does not exist",
        "The named network has no NetDef on this holder — it was never realized here, or was removed.",
        "Create it with `delonix network create <name>`, or use the default network."),
    code!(4401, "image.not_found", NotFound, Image, 4,
        "no such image",
        "The image is not in the local store and the registry has no such tag.",
        "Check the reference, or pull it first (`delonix image pull`)."),
    code!(4402, "image.not_visible", NotFound, Image, 4,
        "image or repository not visible",
        "The registry does not show the repository or image to these credentials: it does not exist, or it is private.",
        "Log in with credentials that can see it (`delonix image login <registry>`)."),
    code!(4501, "vm.not_found", NotFound, Vm, 4,
        "no such VM",
        "There is no virtual machine with that name in this state root.",
        "List them with `delonix vm ls`, or create it with `delonix vm create`."),
    code!(4502, "vm.snapshot_not_found", NotFound, Vm, 4,
        "no such snapshot",
        "The VM has no snapshot with that name.",
        "List its snapshots with `delonix vm snapshot ls <vm>`."),
    code!(4503, "vm.proxmox_snapshot_not_found", NotFound, Vm, 4,
        "no such snapshot",
        "The VM has no snapshot with that name on the Proxmox node.",
        "List its snapshots with `delonix vm snapshot ls <vm>`."),
    code!(4504, "vm.proxmox_not_found", NotFound, Vm, 4,
        "no such resource on the Proxmox node",
        "The node answered HTTP 404, or reported that the VM's configuration file does not exist (which it says with HTTP 500).",
        "Check the VM id in the record against `qm list` on the node; a VM removed on the node side leaves a record with nothing behind it."),
    code!(4801, "security.secret_not_found", NotFound, Security, 4,
        "no such secret",
        "There is no secret with that name.",
        "List them with `delonix secret ls`, or create it with `delonix secret create`."),
    code!(5000, "conflict", Conflict, Engine, 5,
        "conflict: already exists",
        "The name is already taken, or the desired state conflicts with what is there.",
        "Pick another name, remove the existing resource, or use the command's --force/--replace if it has one."),
    code!(5101, "container.already_running", Conflict, Container, 5,
        "container already running",
        "The container is running, and the operation would remove or replace it without --force.",
        "Stop it first, or pass --force."),
    code!(5301, "network.already_exists", Conflict, Network, 5,
        "network already exists",
        "A NetworkStore network with that name already exists.",
        "Pick another name, or remove the existing one first (`delonix network rm <name>`)."),
    code!(5302, "network.no_free_subnet", Conflict, Network, 5,
        "no free subnet left",
        "The workload address space has no /16 left to hand a new network.",
        "Remove an unused network (`delonix network rm <name>`) to free a /16."),
    code!(5303, "network.subnet_overlap", Conflict, Network, 5,
        "subnet overlaps an existing network",
        "The requested CIDR overlaps an existing network's subnet.",
        "Pick a CIDR that does not overlap any network `delonix network ls` already shows."),
    code!(5304, "network.subnet_immutable", Conflict, Network, 5,
        "network subnet cannot be changed in place",
        "A network by this name already exists with a different subnet than the one asked for; workloads may already be addressed on it.",
        "Remove the network (`delonix network rm <name>`) and create it again with the new subnet."),
    code!(5305, "network.base_octet_taken", Conflict, Network, 5,
        "base octet already used by another network",
        "The /16 a create_with_base call was given is already used by a different network.",
        "Pick a different base octet, or remove the network that already has it."),
    code!(5306, "network.prefix_conflict", Conflict, Network, 5,
        "ingress network already realized on a different prefix",
        "An ingress NetDef is already realized on a prefix different from the one the registry now asks for.",
        "Remove the network (`delonix network rm <name>`) and create it again, or keep the recorded subnet."),
    code!(5501, "vm.record_conflict", Conflict, Vm, 5,
        "VM name already used by another VM subsystem",
        "A VM record under this name already exists, created by the other (direct-QEMU) VM subsystem that shares the same vms/ folder.",
        "Remove it first (`delonix vm rm <name>`), or pick another name."),
    code!(5502, "vm.snapshot_taken", Conflict, Vm, 5,
        "VM snapshot name already used",
        "The VM already has a snapshot with that name.",
        "Pick another name, or remove the existing snapshot (`delonix vm snapshot rm`)."),
    code!(5503, "vm.proxmox_snapshot_taken", Conflict, Vm, 5,
        "snapshot name already used",
        "The VM already has a snapshot with that name on the Proxmox node.",
        "Pick another name, or remove the existing snapshot first."),
    code!(5504, "vm.proxmox_conflict", Conflict, Vm, 5,
        "the Proxmox node reports a conflict",
        "The node answered HTTP 409, or reported that the resource already exists (which it says with HTTP 500).",
        "Pick another name or id, or remove what is already there."),
    code!(6000, "unavailable", Unavailable, Engine, 69,
        "unavailable on this host",
        "A tool, a backend or a kernel feature the operation needs is not present. Nothing about the arguments is wrong.",
        "Install what the message names (`delonix system info` shows what this host has) and run the command again."),
    code!(6201, "volume.truenas_client_unbuildable", Unavailable, Volume, 69,
        "could not build the TrueNAS HTTP client",
        "The local TLS/HTTP stack could not build a client for the request. Nothing about the manifest is wrong.",
        "Check the host's TLS libraries; `delonix system info` reports what this host has."),
    code!(6202, "volume.truenas_unsupported_version", Unavailable, Volume, 69,
        "unsupported TrueNAS version",
        "This build only speaks one TrueNAS REST API major; the surface moves between majors.",
        "Point the manifest at a TrueNAS SCALE appliance on the supported major, or update this engine."),
    code!(6301, "network.wg_missing", Unavailable, Network, 69,
        "'wg' is not available on this host",
        "WireGuard node keys and encrypted overlay networks need the `wg` binary, which is not on PATH.",
        "Install wireguard-tools (Debian/Ubuntu: apt install wireguard-tools; Fedora/RHEL: dnf install wireguard-tools; Arch: pacman -S wireguard-tools)."),
    code!(6501, "vm.backend_not_configured", Unavailable, Vm, 69,
        "VM backend not configured",
        "This build knows about the named backend, but nothing in this process has configured it (e.g. Proxmox with no target set).",
        "Configure it as the message describes, or use a backend that is already available."),
    code!(6502, "vm.no_backend_available", Unavailable, Vm, 69,
        "no VM backend available",
        "Auto-detection found neither Cloud Hypervisor nor libvirt+qemu installed.",
        "Install cloud-hypervisor or libvirt+qemu."),
    code!(6503, "vm.no_firmware", Unavailable, Vm, 69,
        "no VM firmware found",
        "Booting a cloud image on Cloud Hypervisor without an explicit kernel needs a firmware, and none was given or found bundled.",
        "Reinstall (curl install.sh) to fetch rust-hypervisor-fw, pass --firmware <path>, or use --backend libvirt."),
    code!(6504, "vm.cloud_localds_missing", Unavailable, Vm, 69,
        "cloud-localds not found",
        "Generating a cloud-init NoCloud seed needs the cloud-localds tool, which is not on PATH.",
        "Install it (Debian/Ubuntu: cloud-image-utils, Fedora/Rocky: cloud-utils), or pass a ready-made seed."),
    code!(6505, "vm.proxmox_client_unbuildable", Unavailable, Vm, 69,
        "could not build the Proxmox HTTP client",
        "The local TLS/HTTP stack could not build a client for the request. Nothing about the manifest is wrong.",
        "Check the host's TLS libraries; `delonix system info` reports what this host has."),
    code!(6506, "vm.proxmox_unavailable", Unavailable, Vm, 69,
        "Proxmox node unavailable",
        "The node answered HTTP 502/503/504: the API is up but cannot serve the request right now (pveproxy without a backend, node restarting).",
        "Retry later; check the node's services (`pveproxy`, `pvedaemon`) and the cluster's quorum."),
    code!(6507, "vm.capability_not_supported", Unavailable, Vm, 69,
        "capability not supported by the backend",
        "The backend this VM would run on does not mark a required capability usable on this host — or, with auto-detection, no available backend does. The message lists each unmet entry with the provider's own state and reason. It is the node contract's FAILED_PRECONDITION with reason CapabilityNotSupported (ADR-0050 D6).",
        "Pick a backend that has it (`delonix provider ls`, `--backend`), fix what the host lacks when the state is `unavailable-on-host`, or drop the requirement."),
    code!(7000, "permission_denied", PermissionDenied, Engine, 77,
        "permission denied",
        "The operating system refused on a permission: a file, a directory or a capability.",
        "Fix the permission on the path the message names, or run from a session that has it, and repeat."),
    code!(8000, "timeout", Timeout, Engine, 124,
        "timed out",
        "The deadline passed with the work unfinished. Nothing said no; it may still be finishing.",
        "Wait and ask again, or raise the --timeout. Do not recreate the resource on top of one still coming up."),
    code!(8201, "volume.truenas_job_timeout", Timeout, Volume, 124,
        "TrueNAS job timed out",
        "An asynchronous job on the appliance (permissions, dataset work) did not reach a terminal state before the deadline.",
        "It may still be finishing on the appliance; check it there before retrying."),
    code!(8501, "vm.proxmox_task_timeout", Timeout, Vm, 124,
        "Proxmox task timed out",
        "A task on the node (create, start, snapshot, destroy) did not reach a terminal state before the deadline.",
        "It may still be finishing on the node; check the node's task log before retrying."),
    code!(8502, "vm.proxmox_lock_timeout", Timeout, Vm, 124,
        "Proxmox VM config lock stayed busy",
        "The node kept refusing because the VM's config lock was held (usually qmeventd cleaning up after a stop), past the retry window.",
        "Check the node's task log for this VM; something is still holding the lock."),
    code!(9000, "system_call_failed", SystemFailure, Engine, 1,
        "a system call failed",
        "The kernel refused an operation (clone, mount, setns…). The message carries the operation and the errno.",
        "Read the errno in the message; `delonix system info` checks the host requirements (user namespaces, cgroup delegation)."),
    code!(9001, "io", SystemFailure, Engine, 74,
        "I/O error",
        "The filesystem said no on a path the engine needs: a full disk, a missing directory, a bad mount.",
        "Check the path in the message and the free space of the state root (`delonix system df`)."),
    code!(9002, "invalid_state", SystemFailure, Engine, 1,
        "invalid state record",
        "A state record on disk could not be read or written as JSON: it is corrupt or from an incompatible version.",
        "Inspect the record the message names; restore it from a snapshot (`delonix system snapshot restore`) if it is corrupt."),
    code!(9003, "state_lock_failed", SystemFailure, Engine, 1,
        "the state lock could not be taken",
        "The lock that serialises a read-modify-write of a state record could not be opened or taken, so the write was refused rather than risk losing a concurrent one.",
        "Check the state root is writable by this user and not on a filesystem without flock."),
    code!(9004, "entropy_unavailable", SystemFailure, Engine, 1,
        "no entropy from the kernel",
        "The kernel did not provide random bytes, which encryption and generated values need.",
        "Check getrandom(2) works on this host (a very old kernel or a restricted seccomp profile)."),
    code!(9201, "volume.host_tool_failed", SystemFailure, Volume, 1,
        "a storage tool failed",
        "A host tool the volume needs (mount, mkfs.ext4, resize2fs, losetup…) failed or is missing. The message carries its own error.",
        "Read the tool's error in the message; mounting network volumes and hard quotas need CAP_SYS_ADMIN."),
    code!(9202, "volume.truenas_request_failed", SystemFailure, Volume, 1,
        "TrueNAS request failed",
        "The HTTP request to the appliance could not be sent or answered (network, TLS, DNS).",
        "Check the target url and that the appliance is reachable from this host."),
    code!(9203, "volume.truenas_http_error", SystemFailure, Volume, 1,
        "TrueNAS answered with an HTTP error",
        "The appliance's REST API returned a non-2xx status; the message carries its own error body.",
        "Read the body in the message — TrueNAS puts the actionable detail there."),
    code!(9204, "volume.truenas_decode_failed", SystemFailure, Volume, 1,
        "could not read the TrueNAS response",
        "The appliance's answer was not the JSON this client expected.",
        "Check the appliance is on the supported major; the message carries the start of the body."),
    code!(9205, "volume.truenas_job_failed", SystemFailure, Volume, 1,
        "TrueNAS job failed",
        "An asynchronous job on the appliance finished FAILED or ABORTED; the message carries its own reason.",
        "Read the appliance's reason in the message."),
    code!(9206, "volume.truenas_job_vanished", SystemFailure, Volume, 1,
        "TrueNAS job vanished",
        "The appliance stopped reporting a job before it reached a terminal state, so whether the work happened is unknown.",
        "Check the dataset/permissions on the appliance directly."),
    code!(9207, "volume.truenas_no_mountpoint", SystemFailure, Volume, 1,
        "TrueNAS dataset has no mountpoint",
        "The appliance reports the dataset with no mountpoint, so it cannot be shared or mounted.",
        "Check the dataset on the appliance; a mountpoint may need to be set explicitly."),
    code!(9208, "volume.truenas_inconsistent", SystemFailure, Volume, 1,
        "TrueNAS state disagreed with its own report",
        "The appliance reported an action as done, but a read-back immediately after disagrees (dataset missing after create, or still there after delete).",
        "Check the dataset on the appliance directly; nothing here was rolled back."),
    code!(9301, "network.command_failed", SystemFailure, Network, 1,
        "a network system call failed",
        "A host tool this engine shells out to for networking (ip, nft, wg, bridge, a CNI plugin, newuidmap/newgidmap, or a raw syscall) could not be run or exited badly. The message names the operation and carries the tool's own error.",
        "Read the operation and error in the message; check the tool is installed and the host's network state is sane."),
    code!(9401, "image.registry", SystemFailure, Image, 1,
        "registry error",
        "An OCI registry answered with an error, or could not be reached.",
        "Check the image reference, your login (`delonix image login`) and the network path to the registry."),
    code!(9402, "image.module_scan_failed", SystemFailure, Image, 1,
        "a module tree could not be read",
        "Reading the module directory failed: a permission, a missing path or an I/O error.",
        "Check the path in the message is readable by this user."),
    code!(9403, "image.digest_mismatch", SystemFailure, Image, 1,
        "content digest mismatch",
        "What the registry served does not hash to the digest that was pinned or declared. The content was refused.",
        "Do not work around it: the registry, a mirror or the network may be tampering. Pull again from a trusted source."),
    code!(9404, "image.overlay_mount_failed", SystemFailure, Image, 1,
        "overlay mount failed",
        "Mounting the image's overlay root filesystem failed.",
        "Read the errno in the message; rootless overlay needs a kernel with unprivileged overlayfs (`delonix system info`)."),
    code!(9501, "vm.tool_failed", SystemFailure, Vm, 1,
        "VM tool failed",
        "An external tool this VM's lifecycle depends on (virsh, qemu-img, the cloud-hypervisor process) could not be run or exited badly.",
        "Read the tool's own error in the message."),
    code!(9502, "vm.cloud_hypervisor_api_failed", SystemFailure, Vm, 1,
        "Cloud Hypervisor API failed",
        "The running VM's Cloud Hypervisor VMM could not be reached over its local api-socket, or answered with a non-2xx status.",
        "Check the VM is still alive (`delonix vm status`); a vmm that crashed needs `delonix vm start`."),
    code!(9503, "vm.disk_corrupted", SystemFailure, Vm, 1,
        "VM disk corrupted after stop",
        "qemu-img check found a stopped Cloud Hypervisor VM's disk corrupted (BUG-VM-001) — a post-stop check, not the stop itself failing.",
        "Run `qemu-img check -r all` on the disk the message names; consider --backend libvirt for VMs that need reliable disk snapshots."),
    code!(9504, "vm.snapshot_needs_stopped", SystemFailure, Vm, 1,
        "snapshot needs a stopped VM",
        "The Cloud Hypervisor backend cannot snapshot/restore/delete-snapshot a running VM's disk: the vmm holds it exclusively.",
        "Stop the VM first (`delonix vm stop`), or use --backend libvirt for live checkpoints."),
    code!(9505, "vm.no_stopped_domain_xml", SystemFailure, Vm, 1,
        "no libvirt domain description on disk",
        "A stopped VM has no <name>.xml recorded, so there is nothing to redefine for a snapshot/restore verb.",
        "Start the VM once (`delonix vm start <name>`); the description is written then and stays from then on."),
    code!(9506, "vm.admission_refused", SystemFailure, Vm, 1,
        "VM admission refused",
        "The host-protection check refused to boot a VM whose requested memory does not fit in what the host has available.",
        "Stop other VMs/containers, reduce the memory, or lower DELONIX_VM_RESERVE_MIB (at your own risk)."),
    code!(9507, "vm.live_backup_failed", SystemFailure, Vm, 1,
        "live disk backup failed",
        "A step of the live-disk-backup pipeline (list disks, stage the overlay, snapshot, copy, pivot) failed.",
        "Read the step's own error in the message; the VM is left running either way."),
    code!(9508, "vm.cloud_localds_spawn_failed", SystemFailure, Vm, 1,
        "could not run cloud-localds",
        "The cloud-localds tool could not be started, for a reason other than being missing.",
        "Read the error in the message; check the host's permissions and free space."),
    code!(9509, "vm.cloud_localds_failed", SystemFailure, Vm, 1,
        "cloud-localds failed",
        "The cloud-localds tool ran and exited with a non-zero status while building the seed ISO.",
        "Check the cloud-init user-data/meta-data/network-config it was given are well-formed."),
    code!(9510, "vm.proxmox_request_failed", SystemFailure, Vm, 1,
        "Proxmox request failed",
        "The HTTP request to the node could not be sent or answered (network, TLS, DNS).",
        "Check the target url and that the node is reachable from this host."),
    code!(9511, "vm.proxmox_http_error", SystemFailure, Vm, 1,
        "Proxmox answered with an HTTP error",
        "The node's REST API returned a non-2xx status; the message carries its own error body.",
        "Read the body in the message — Proxmox puts the actionable detail there."),
    code!(9512, "vm.proxmox_unexpected_answer", SystemFailure, Vm, 1,
        "Proxmox answer had the wrong shape",
        "The node's response parsed as JSON but did not carry what a specific call needed (a VM id, a task id).",
        "Check the node's Proxmox VE version matches what this build expects."),
    code!(9513, "vm.proxmox_task_failed", SystemFailure, Vm, 1,
        "Proxmox task failed",
        "A task on the node (create, start, snapshot, destroy) finished with a verdict other than OK; the message carries the node's own reason.",
        "Read the node's reason in the message, or check its task log."),
    code!(9514, "vm.proxmox_decode_failed", SystemFailure, Vm, 1,
        "could not read the Proxmox response",
        "The node's answer was not the JSON this client expected.",
        "Check the node's Proxmox VE version matches what this build expects; the message carries the start of the body."),
    code!(9515, "vm.proxmox_unauthorized", SystemFailure, Vm, 1,
        "Proxmox refused the credential",
        "The node answered HTTP 401: the API token is wrong or revoked, or the ticket could not be renewed.",
        "Check the token id and secret (or the account), and that the token is not expired on the node."),
    code!(9516, "vm.proxmox_forbidden", SystemFailure, Vm, 1,
        "Proxmox denied the operation",
        "The node answered HTTP 403: the credential is valid but lacks the privilege this route needs (the message names the route).",
        "Grant the role the route needs to the token or account on the node — least privilege, one role per use."),
    code!(9517, "vm.proxmox_response_too_large", SystemFailure, Vm, 1,
        "Proxmox response exceeded the size bound",
        "The node's answer was larger than this client is willing to read into memory.",
        "Check what the node is serving on that route; an answer this large is not one the engine expects."),
    code!(9901, "host.syscall_failed", SystemFailure, Host, 1,
        "a container-runtime system call failed",
        "A kernel operation this crate performs to build or manage a container (clone, mount, setns, a cgroup or /proc write) failed. The message carries the operation and the errno.",
        "Read the errno in the message; `delonix system info` checks the host requirements (user namespaces, cgroup delegation)."),
];

impl Error {
    /// The dictionary number of this failure (ADR-0043). A variant built with free
    /// text reports its class's generic entry until it has its own.
    ///
    /// The match is exhaustive on purpose, like [`Error::code`]'s.
    pub fn number(&self) -> u16 {
        match self {
            Error::NotFound(_) => 4000,
            Error::VmNotFound(_) => 4501,
            Error::NotRunning(_) => 3000,
            Error::Conflict(_) => 5000,
            Error::Unavailable(_) => 6000,
            Error::Timeout(_) => 8000,
            Error::Invalid(_) => 1000,
            Error::Registry(_) => 9401,
            Error::Runtime { .. } => 9000,
            Error::Json(_) => 9002,
            Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 7000,
            Error::Io(_) => 9001,
            Error::Coded { number, .. } => *number,
        }
    }

    /// `inner`, carrying its specific dictionary number (ADR-0043 D4).
    ///
    /// # Panics
    ///
    /// When the number's class digit is not `inner`'s class: a code that says
    /// «not found» on a failure that exits as «invalid argument» would make the
    /// number and the exit code tell two stories. Every crate's conversion has a
    /// test that walks all its variants through here, so this fires in tests,
    /// not in front of an operator.
    pub fn coded(number: u16, inner: Error) -> Error {
        assert_eq!(
            number / 1000,
            inner.class().digit(),
            "DX-{number:04} does not belong to the class of «{inner}»"
        );
        Error::Coded {
            number,
            inner: Box::new(inner),
        }
    }

    /// The failure itself, for the rare caller that needs a variant's payload.
    ///
    /// Today every error is its own root. When a crate's error carries its specific
    /// code inside the shared class (ADR-0043 D4), this is what looks through the
    /// carrier — so a `match e.root()` keeps matching where a `match e` would not.
    /// For «which class is this», ask [`Error::class`] instead.
    pub fn root(&self) -> &Error {
        match self {
            Error::Coded { inner, .. } => inner.root(),
            e => e,
        }
    }

    /// [`Error::root`] by value, to move a payload out.
    pub fn into_root(self) -> Error {
        match self {
            Error::Coded { inner, .. } => inner.into_root(),
            e => e,
        }
    }

    /// The class of this failure — what the caller does next.
    ///
    /// **Ask this, not the variant.** A crate's own error travels as a shared class
    /// that carries its specific code (ADR-0043 D4), and a `match` on
    /// `Err(Error::NotFound(_))` stops matching it without a word from the
    /// compiler. A class question keeps its answer.
    pub fn class(&self) -> Class {
        let root = self.root();
        Class::ALL[usize::from(root.number() / 1000)]
    }

    /// «No such resource» — the caller creates it or reports it missing.
    pub fn is_not_found(&self) -> bool {
        self.class() == Class::NotFound
    }

    /// «Already exists» — the caller adopts it, skips, or picks another name.
    pub fn is_conflict(&self) -> bool {
        self.class() == Class::Conflict
    }

    /// «Exists but is not running».
    pub fn is_not_running(&self) -> bool {
        self.class() == Class::NotRunning
    }

    /// «A capability this host does not have».
    pub fn is_unavailable(&self) -> bool {
        self.class() == Class::Unavailable
    }

    /// «The deadline passed».
    pub fn is_timeout(&self) -> bool {
        self.class() == Class::Timeout
    }

    /// «An argument is wrong».
    pub fn is_invalid_argument(&self) -> bool {
        self.class() == Class::InvalidArgument
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_unique_and_sorted() {
        for w in CATALOG.windows(2) {
            assert!(
                w[0].number < w[1].number,
                "{} before {}",
                w[0].label(),
                w[1].label()
            );
        }
    }

    #[test]
    fn the_digits_are_the_class_and_the_domain() {
        for c in CATALOG {
            assert!(c.number <= 9999, "{}", c.label());
            assert_eq!(
                c.number / 1000,
                c.class.digit(),
                "{}: class digit",
                c.label()
            );
            assert_eq!(
                c.number / 100 % 10,
                c.domain.digit(),
                "{}: domain digit",
                c.label()
            );
            if c.number % 100 == 0 {
                assert_eq!(
                    c.domain,
                    Domain::Engine,
                    "{}: only a class entry ends in 00",
                    c.label()
                );
            }
        }
    }

    #[test]
    fn every_class_has_its_generic_entry() {
        for class in Class::ALL {
            let n = class.digit() * 1000;
            let c = lookup(n).unwrap_or_else(|| panic!("no entry {}", label(n)));
            assert_eq!(c.class, class);
        }
    }

    #[test]
    fn the_exit_code_of_an_entry_is_its_class() {
        for c in CATALOG {
            if c.class == Class::SystemFailure {
                assert!(
                    c.exit == 1 || c.exit == crate::exitcode::IO,
                    "{}",
                    c.label()
                );
            } else {
                assert_eq!(c.exit, c.class.exit_code(), "{}", c.label());
            }
        }
    }

    #[test]
    fn ids_are_unique_and_every_text_is_written() {
        let mut ids: Vec<&str> = CATALOG.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), CATALOG.len());
        for c in CATALOG {
            assert!(
                !c.message.is_empty() && !c.meaning.is_empty() && !c.remedy.is_empty(),
                "{}",
                c.label()
            );
        }
    }

    fn one_of_each() -> Vec<Error> {
        vec![
            Error::NotFound("volume x".into()),
            Error::VmNotFound("dev".into()),
            Error::NotRunning("web".into()),
            Error::Conflict("x".into()),
            Error::Unavailable("wg".into()),
            Error::Timeout("x".into()),
            Error::Invalid("x".into()),
            Error::Registry("x".into()),
            Error::Runtime {
                context: "clone",
                message: "EPERM".into(),
            },
            Error::Json(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
            Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            Error::Io(std::io::Error::other("x")),
        ]
    }

    /// The number, the exit code and the `DX_*` identity of one failure can never
    /// tell three different stories.
    #[test]
    fn the_number_agrees_with_the_exit_code_of_the_same_error() {
        for e in one_of_each() {
            let entry =
                lookup(e.number()).unwrap_or_else(|| panic!("{} has no entry", label(e.number())));
            assert_eq!(entry.exit, crate::exitcode::for_error(&e), "{e}");
        }
    }

    #[test]
    fn the_class_question_answers_what_the_variant_did() {
        assert!(Error::NotFound("x".into()).is_not_found());
        assert!(Error::VmNotFound("x".into()).is_not_found());
        assert!(Error::Conflict("x".into()).is_conflict());
        assert!(Error::NotRunning("x".into()).is_not_running());
        assert!(Error::Unavailable("x".into()).is_unavailable());
        assert!(Error::Timeout("x".into()).is_timeout());
        assert!(Error::Invalid("x".into()).is_invalid_argument());
        assert!(!Error::Registry("x".into()).is_not_found());
        for e in one_of_each() {
            assert!(
                e.class().exit_code() == crate::exitcode::for_error(&e)
                    || e.class() == Class::SystemFailure,
                "{e}"
            );
        }
    }

    /// The carrier changes the number and nothing else: same message, same class,
    /// same exit code, same `DX_*` identity — and a `root()` match still matches.
    #[test]
    fn a_coded_error_is_its_inner_error_with_a_finer_number() {
        let plain = Error::NotFound("volume db".into());
        let coded = Error::coded(4201, Error::NotFound("volume db".into()));
        assert_eq!(coded.number(), 4201);
        assert_eq!(coded.to_string(), plain.to_string());
        assert_eq!(coded.class(), plain.class());
        assert_eq!(coded.code(), plain.code());
        assert_eq!(
            crate::exitcode::for_error(&coded),
            crate::exitcode::for_error(&plain)
        );
        assert!(coded.is_not_found());
        assert!(matches!(coded.root(), Error::NotFound(m) if m == "volume db"));
    }

    #[test]
    #[should_panic(expected = "does not belong to the class")]
    fn a_number_from_another_class_is_refused() {
        let _ = Error::coded(1201, Error::NotFound("volume db".into()));
    }

    #[test]
    fn a_code_is_read_in_the_spellings_people_write() {
        for s in ["DX-4201", "dx-4201", "DX4201", "4201", " 4201 "] {
            assert_eq!(parse(s), Some(4201), "{s}");
        }
        for s in [
            "",
            "42",
            "DX-42010",
            "Container",
            "Pod.image",
            "DX-42a1",
            "-4201",
        ] {
            assert_eq!(parse(s), None, "{s}");
        }
        assert_eq!(label(7), "DX-0007");
    }
}
