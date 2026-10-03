set -u
P=$(cd "$(dirname "$0")" && pwd)
mount -t tmpfs none /run && mkdir -p /run/netns
sysctl -qw net.ipv6.conf.all.forwarding=1 net.ipv4.ip_forward=1
for br in br0 br1; do ip link add $br type bridge; ip link set $br up; done
ip -6 addr add fd65:1::1/64 dev br0 nodad; ip -6 addr add fd65:2::1/64 dev br1 nodad
ip addr add 10.9.0.1/24 dev br0
mk() { # name bridge v6 v4
  ip netns add $1
  ip link add vh-$1 type veth peer name eth0 netns $1
  ip link set vh-$1 master $2 up
  ip netns exec $1 sh -c "sysctl -qw net.ipv6.conf.all.accept_dad=0 net.ipv6.conf.default.accept_dad=0 net.ipv6.conf.eth0.accept_dad=0; ip link set lo up; ip link set eth0 up; ip -6 addr add $3/64 dev eth0 nodad; ip -6 route add default via ${3%::*}::1"
  [ -n "${4:-}" ] && ip netns exec $1 ip addr add $4/24 dev eth0
}
mk a br0 fd65:1::a 10.9.0.2; mk b br0 fd65:1::b 10.9.0.3; mk c br1 fd65:2::c
sleep 2
echo "## holder netns bridge-nf-call-ip6tables before: $(cat /proc/sys/net/bridge/bridge-nf-call-ip6tables 2>&1)  iptables: $(cat /proc/sys/net/bridge/bridge-nf-call-iptables 2>&1)"
nft -f - <<X
table inet t {
  set dlxbr { type ifname; elements = { "br0", "br1" } }
  chain fw { type filter hook forward priority -10;
    meta l4proto ipv6-icmp icmpv6 type echo-request ip6 saddr fd65:1::a ip6 daddr fd65:1::b counter meta nftrace set 0 comment "a2b-seen"
    ip6 saddr fd65:1::99 counter comment "spoof6-seen-any-iif"
    iifname "vh-a" ip6 saddr fd65:1::99 counter comment "spoof6-seen-iif-veth"
    iifname "br0" ip6 saddr fd65:1::99 counter comment "spoof6-seen-iif-bridge"
    ip saddr 10.9.0.99 counter comment "spoof4-seen-any-iif"
    iifname "vh-a" ip saddr 10.9.0.99 counter comment "spoof4-seen-iif-veth"
    icmpv6 type nd-router-advert counter comment "ra-seen"
    icmpv6 type { nd-neighbor-solicit, nd-neighbor-advert } counter comment "nd-seen"
    iifname "br0" oifname "br1" counter drop comment "inter-bridge-drop"
    iifname "br0" oifname "br0" counter comment "intra-br0-seen"
  }
}
X
run() { echo "## $1"; }
for v in 0 1; do
  echo "### bridge-nf-call-ip6tables=$v (write: $(echo $v > /proc/sys/net/bridge/bridge-nf-call-ip6tables 2>&1 && echo ok)) iptables=$v ($(echo $v > /proc/sys/net/bridge/bridge-nf-call-iptables 2>&1 && echo ok))"
  nft reset counters table inet t >/dev/null
  ip netns exec a ping -6 -c2 -W1 fd65:1::b >/dev/null && echo "a->b v6 bridged: OK" || echo "a->b v6 bridged: FAIL"
  ip netns exec a ping -6 -c2 -W1 fd65:2::c >/dev/null && echo "a->c v6 routed: OK" || echo "a->c v6 routed: FAIL"
  ip netns exec a ip -6 addr add fd65:1::99/64 dev eth0 nodad
  ip netns exec a ping -6 -c2 -W1 -I fd65:1::99 fd65:1::b >/dev/null && echo "a(spoof ::99)->b: replies" || echo "a(spoof ::99)->b: no reply"
  ip netns exec a ip -6 addr del fd65:1::99/64 dev eth0
  ip netns exec a ip addr add 10.9.0.99/24 dev eth0
  ip netns exec a ping -c2 -W1 -I 10.9.0.99 10.9.0.3 >/dev/null && echo "a(spoof .99)->b v4: replies" || echo "a(spoof .99)->b v4: no reply"
  ip netns exec a ip addr del 10.9.0.99/24 dev eth0
  ip netns exec a python3 $P/ra.py eth0 fd77:$v:: >/dev/null; sleep 1
  echo "b addrs from RA: $(ip netns exec b ip -6 -o addr show dev eth0 | awk '{print $4}' | grep fd77 | tr '\n' ' ')"
  nft list chain inet t fw | grep -E 'counter packets' | sed -E 's/^\s+//; s/.*counter (packets [0-9]+).*comment "([^"]+)".*/  \2: \1/'
done
echo "### table bridge (native, no br_netfilter needed): bridge-nf-call-ip6tables=0"
echo 0 > /proc/sys/net/bridge/bridge-nf-call-ip6tables
nft -f - <<X
table bridge bt {
  chain brfw { type filter hook forward priority -200;
    iifname "vh-a" ether type ip6 ip6 saddr != fd65:1::a ip6 saddr != fe80::/10 counter drop comment "spoof6-drop"
    iifname "vh-a" icmpv6 type nd-router-advert counter drop comment "ra-drop"
    iifname "vh-a" icmpv6 type nd-neighbor-advert icmpv6 taddr != fd65:1::a icmpv6 taddr != fe80::/10 counter drop comment "na-spoof-drop"
  }
}
X
ip netns exec a ip -6 addr add fd65:1::99/64 dev eth0 nodad
ip netns exec a ping -6 -c2 -W1 -I fd65:1::99 fd65:1::b >/dev/null && echo "a(spoof ::99)->b: replies" || echo "a(spoof ::99)->b: no reply"
ip netns exec a ip -6 addr del fd65:1::99/64 dev eth0
ip netns exec a ping -6 -c2 -W1 fd65:1::b >/dev/null && echo "a->b legit: OK" || echo "a->b legit: FAIL"
ip netns exec a python3 $P/ra.py eth0 fd77:b:: >/dev/null; sleep 1
echo "b addrs from RA fd77:b: $(ip netns exec b ip -6 -o addr show dev eth0 | awk '{print $4}' | grep 'fd77:b' | tr '\n' ' ')"
nft list table bridge bt | grep 'counter packets' | sed -E 's/^\s+//; s/.*counter (packets [0-9]+).*comment "([^"]+)".*/  \2: \1/'
