set -u
mount -t tmpfs none /run && mkdir -p /run/netns
sysctl -qw net.ipv4.ip_forward=1
for br in br0 br1; do ip link add $br type bridge; ip link set $br up; done
ip addr add 10.9.0.1/24 dev br0; ip addr add 10.9.1.1/24 dev br1
mk() { ip netns add $1; ip link add vh-$1 type veth peer name eth0 netns $1; ip link set vh-$1 master $2 up
  ip netns exec $1 sh -c "ip link set lo up; ip link set eth0 up; ip addr add $3/24 dev eth0; ip route add default via ${3%.*}.1"; }
mk a br0 10.9.0.2;
ip link add vx type dummy; ip link set vx up
 mk b br0 10.9.0.3; mk c br1 10.9.1.2
# the production form: table ip dlxing, chain fwdeny, `insert rule ... iifname <veth> ip saddr != <ip> drop`
nft -f - <<X
table ip dlxing {
 chain fwdeny {
  type filter hook forward priority -10;
 }
}
X
nft insert rule ip dlxing fwdeny iifname vh-a ip saddr != 10.9.0.2 counter drop
for n in b c; do ip netns exec $n nft -f - <<X
table ip t {
 chain in {
  type filter hook input priority 0;
  ip saddr 10.9.0.99 icmp type echo-request counter comment "spoofed-arrived"
 }
}
X
done
ip netns exec a ip addr add 10.9.0.99/24 dev eth0
echo "bridge-nf-call-iptables=$(cat /proc/sys/net/bridge/bridge-nf-call-iptables)"
ip netns exec a ping -c3 -W1 -I 10.9.0.99 10.9.0.3 >/dev/null; echo "spoofed a->b (same bridge) rc=$?"
ip netns exec a ping -c3 -W1 -I 10.9.0.99 10.9.1.2 >/dev/null; echo "spoofed a->c (routed br0->br1) rc=$?"
echo "holder antispoof rule: $(nft list chain ip dlxing fwdeny | grep -o 'counter packets [0-9]*')"
for n in b c; do echo "$n received spoofed: $(ip netns exec $n nft list chain ip t in | grep -o 'packets [0-9]*')"; done
