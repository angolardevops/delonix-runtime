set -u
D=$(mktemp -d /tmp/dlx65.XXXX)
unshare --user --map-root-user --net --mount sleep 150 & NSPID=$!
sleep 0.5
J() { nsenter -t $NSPID -U -n -m --preserve-credentials -- "$@"; }
J sh -c 'mount -t tmpfs none /run && mkdir -p /run/netns; sysctl -qw net.ipv6.conf.all.forwarding=1 net.ipv6.conf.default.accept_ra=2'
slirp4netns --configure --mtu=65520 --disable-host-loopback --enable-ipv6 --api-socket=$D/api.sock $NSPID tap0 > $D/slirp.log 2>&1 & SPID=$!
sleep 6
echo "## holder (forwarding=1, accept_ra=2 on tap0=$(J cat /proc/sys/net/ipv6/conf/tap0/accept_ra)):"
J ip -6 -o addr show dev tap0 scope global | awk '{print "  addr", $4}'
J ip -6 route show default | sed 's/^/  /'
J sh -c '
ip link add br0 type bridge; ip link set br0 up; ip -6 addr add fd65:1::1/64 dev br0 nodad
ip netns add a; ip link add vh-a type veth peer name eth0 netns a; ip link set vh-a master br0 up
ip netns exec a sh -c "sysctl -qw net.ipv6.conf.all.accept_dad=0 net.ipv6.conf.eth0.accept_dad=0; ip link set lo up; ip link set eth0 up; ip -6 addr add fd65:1::a/64 dev eth0 nodad; ip -6 route add default via fd65:1::1"
sleep 1
ip netns exec a ping -6 -c2 -W2 fd00::2 >/dev/null && echo "container fd65:1::a -> slirp gw fd00::2 WITHOUT nat66: OK" || echo "container fd65:1::a -> slirp gw fd00::2 WITHOUT nat66: FAIL"
nft -f - <<X
table inet n {
 chain post {
  type nat hook postrouting priority 100;
  oifname "tap0" ip6 saddr fd65::/16 counter masquerade
 }
}
X
ip netns exec a ping -6 -c2 -W2 fd00::2 >/dev/null && echo "container -> fd00::2 WITH inet masquerade on tap0: OK" || echo "container -> fd00::2 WITH inet masquerade: FAIL"
echo "  masquerade $(nft list chain inet n post | grep -o "packets [0-9]*")"
'
kill $SPID $NSPID 2>/dev/null; wait 2>/dev/null; rm -rf $D
