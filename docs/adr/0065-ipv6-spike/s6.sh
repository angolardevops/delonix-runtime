set -u
mount -t tmpfs none /run && mkdir -p /run/netns
sysctl -qw net.ipv6.conf.all.forwarding=1 net.ipv4.ip_forward=1
ip link add br0 type bridge; ip link set br0 up; ip -6 addr add fd65:1::1/64 dev br0 nodad; ip addr add 10.9.0.1/24 dev br0
mk() { ip netns add $1; ip link add vh-$1 type veth peer name eth0 netns $1; ip link set vh-$1 master br0 up
  ip netns exec $1 sh -c "sysctl -qw net.ipv6.conf.all.accept_dad=0 net.ipv6.conf.eth0.accept_dad=0; ip link set lo up; ip link set eth0 up; ip -6 addr add $2/64 dev eth0 nodad; ip addr add $3/24 dev eth0"; }
mk a fd65:1::a 10.9.0.2; mk b fd65:1::b 10.9.0.3; mk c fd65:1::c 10.9.0.4
# a: ns X ; b: ns Y ; c: ns Y   -- b's chain = the production shape (ns accept, then @dlxall ct new drop), dual-stack
nft -f - <<X
table inet dlxing {
 set dlxall4 { type ipv4_addr; elements = { 10.9.0.2, 10.9.0.3, 10.9.0.4 } }
 set dlxall6 { type ipv6_addr; elements = { fd65:1::a, fd65:1::b, fd65:1::c } }
 set nsY4 { type ipv4_addr; elements = { 10.9.0.3, 10.9.0.4 } }
 set nsY6 { type ipv6_addr; elements = { fd65:1::b, fd65:1::c } }
 map fwmap4 { type ipv4_addr : verdict; }
 map fwmap6 { type ipv6_addr : verdict; }
 chain fwb {
  ct state invalid counter drop
  ct state established,related counter accept
  ip daddr 10.9.0.3 ip saddr @nsY4 counter accept
  ip daddr 10.9.0.3 ip saddr @dlxall4 ct state new counter drop
  ip6 daddr fd65:1::b ip6 saddr @nsY6 counter accept
  ip6 daddr fd65:1::b ip6 saddr @dlxall6 ct state new counter drop
 }
 chain fwout {
  type filter hook forward priority -6;
  ip saddr vmap @fwmap4
  ip6 saddr vmap @fwmap6
 }
 chain fwcont {
  type filter hook forward priority -5;
  ip daddr vmap @fwmap4
  ip6 daddr vmap @fwmap6
 }
}
X
nft add element inet dlxing fwmap4 '{ 10.9.0.3 : jump fwb }'
nft add element inet dlxing fwmap6 '{ fd65:1::b : jump fwb }'
p() { ip netns exec $1 ping $2 -c2 -W1 $3 >/dev/null && echo "$4: OPEN" || echo "$4: BLOCKED"; }
p a -4 10.9.0.3 "v4 a(X)->b(Y)"; p a -6 fd65:1::b "v6 a(X)->b(Y)"
p c -4 10.9.0.3 "v4 c(Y)->b(Y)"; p c -6 fd65:1::b "v6 c(Y)->b(Y)"
p b -4 10.9.0.2 "v4 b(Y)->a(X) (return via established)"; p b -6 fd65:1::a "v6 b(Y)->a(X) (return via established)"
nft list chain inet dlxing fwb | grep -o 'ip6\? .*counter packets [0-9]*' | sed 's/^/  /'
