set -u
P=$(pwd)
mount -t tmpfs none /run && mkdir -p /run/netns
sysctl -qw net.ipv6.conf.all.forwarding=1 net.ipv4.ip_forward=1
echo 0 > /proc/sys/net/bridge/bridge-nf-call-ip6tables; echo 0 > /proc/sys/net/bridge/bridge-nf-call-iptables
ip link add br0 type bridge; ip link set br0 up; ip -6 addr add fd65:1::1/64 dev br0 nodad; ip addr add 10.9.0.1/24 dev br0
mk() { ip netns add $1; ip link add vh-$1 type veth peer name eth0 netns $1; ip link set vh-$1 master br0 up
  ip netns exec $1 sh -c "sysctl -qw net.ipv6.conf.all.accept_dad=0 net.ipv6.conf.eth0.accept_dad=0; ip link set eth0 address $4; ip link set lo up; ip link set eth0 up; ip -6 addr add $2/64 dev eth0 nodad; ip addr add $3/24 dev eth0"; }
mk a fd65:1::a 10.9.0.2 02:00:00:00:00:0a; mk b fd65:1::b 10.9.0.3 02:00:00:00:00:0b
LL=$(ip netns exec a ip -6 -o addr show dev eth0 scope link | awk '{print $4}' | cut -d/ -f1)
sed "s/fe80::1/$LL/" s7.nft | nft -f -
p() { ip netns exec a ping $1 -c2 -W1 $2 $3 >/dev/null && echo "$4: replies" || echo "$4: no reply"; }
echo "bridge-nf-call-ip(6)tables=0 (native bridge family only)"
p -4 "" 10.9.0.3 "legit v4 a->b"; p -6 "" fd65:1::b "legit v6 a->b"; p -6 "" fd65:1::1 "legit v6 a->holder gw"
ip netns exec a ip addr add 10.9.0.99/24 dev eth0; ip netns exec a ip -6 addr add fd65:1::99/64 dev eth0 nodad
p -4 "-I 10.9.0.99" 10.9.0.3 "spoof v4 a->b"; p -4 "-I 10.9.0.99" 10.9.0.1 "spoof v4 a->holder"
p -6 "-I fd65:1::99" fd65:1::b "spoof v6 a->b"; p -6 "-I fd65:1::99" fd65:1::1 "spoof v6 a->holder"
ip netns exec a python3 $P/ra.py eth0 fd77:c:: >/dev/null; sleep 1
echo "b got rogue RA prefix: $(ip netns exec b ip -6 -o addr | grep -c fd77:c) ; holder got: $(ip -6 -o addr | grep -c fd77:c)"
nft list chain bridge dlxspoof pre | grep -o 'counter packets [0-9]* bytes [0-9]* drop' | awk '{print "  rule", NR, $3}'
