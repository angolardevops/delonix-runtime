// delonix_flow — per-IP traffic accounting for SDN observability.
//
// Two tc/clsact classifiers share one hash map keyed by the container's IPv4:
//   - `count_tx` (attach at delonix0 INGRESS): a packet entering the bridge came
//     FROM a container → key = saddr, bump tx.
//   - `count_rx` (attach at delonix0 EGRESS): a packet leaving the bridge goes
//     TO a container → key = daddr, bump rx.
// Never drops or mangles: returns TC_ACT_UNSPEC so the normal path (nft) decides.
#include <linux/bpf.h>
#include <linux/pkt_cls.h>
#include <linux/if_ether.h>
#include <linux/ip.h>
#include <bpf/bpf_helpers.h>

struct flow {
    __u64 rx_packets;
    __u64 rx_bytes;
    __u64 tx_packets;
    __u64 tx_bytes;
};

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 8192);
    __type(key, __u32);      // IPv4 address (network byte order)
    __type(value, struct flow);
    __uint(pinning, LIBBPF_PIN_BY_NAME);
} delonix_flows SEC(".maps");

static __always_inline int ipv4_of(struct __sk_buff *skb, int want_src, __u32 *out, __u32 *len)
{
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    if ((void *)(eth + 1) > data_end)
        return -1;
    if (eth->h_proto != __constant_htons(ETH_P_IP))
        return -1;
    struct iphdr *ip = (void *)(eth + 1);
    if ((void *)(ip + 1) > data_end)
        return -1;
    *out = want_src ? ip->saddr : ip->daddr;
    *len = skb->len;
    return 0;
}

SEC("tc/tx")
int count_tx(struct __sk_buff *skb)
{
    __u32 ip, len;
    if (ipv4_of(skb, 1, &ip, &len) < 0)
        return TC_ACT_UNSPEC;
    struct flow *f = bpf_map_lookup_elem(&delonix_flows, &ip);
    if (f) {
        __sync_fetch_and_add(&f->tx_packets, 1);
        __sync_fetch_and_add(&f->tx_bytes, len);
    } else {
        struct flow init = {.tx_packets = 1, .tx_bytes = len};
        bpf_map_update_elem(&delonix_flows, &ip, &init, BPF_NOEXIST);
    }
    return TC_ACT_UNSPEC;
}

SEC("tc/rx")
int count_rx(struct __sk_buff *skb)
{
    __u32 ip, len;
    if (ipv4_of(skb, 0, &ip, &len) < 0)
        return TC_ACT_UNSPEC;
    struct flow *f = bpf_map_lookup_elem(&delonix_flows, &ip);
    if (f) {
        __sync_fetch_and_add(&f->rx_packets, 1);
        __sync_fetch_and_add(&f->rx_bytes, len);
    } else {
        struct flow init = {.rx_packets = 1, .rx_bytes = len};
        bpf_map_update_elem(&delonix_flows, &ip, &init, BPF_NOEXIST);
    }
    return TC_ACT_UNSPEC;
}

char _license[] SEC("license") = "GPL";
