set -u
D=$(mktemp -d /tmp/dlx65.XXXX)
unshare --user --map-root-user --net sleep 120 & NSPID=$!
sleep 0.5
slirp4netns --configure --mtu=65520 --disable-host-loopback --enable-ipv6 --api-socket=$D/api.sock $NSPID tap0 > $D/slirp.log 2>&1 & SPID=$!
sleep 6
J() { nsenter -t $NSPID -U -n --preserve-credentials -- "$@"; }
echo "## slirp log:"; cat $D/slirp.log
echo "## addresses:"; J ip -6 -o addr show dev tap0 | awk '{print $4, $5, $6, $7}'
J ip -o -4 addr show dev tap0 | awk '{print $4}'
echo "## v6 routes:"; J ip -6 route
echo "## accept_ra tap0=$(J cat /proc/sys/net/ipv6/conf/tap0/accept_ra) forwarding all=$(J cat /proc/sys/net/ipv6/conf/all/forwarding)"
J ping -6 -c2 -W1 fd00::2 >/dev/null && echo "ping6 slirp gw fd00::2: OK" || echo "ping6 slirp gw fd00::2: FAIL"
J python3 -c "
import socket,struct
q=struct.pack('!HHHHHH',0x1234,0x0100,1,0,0,0)+b'\x07example\x03com\x00'+struct.pack('!HH',28,1)
for srv in ('fd00::3','10.0.2.3'):
  fam=socket.AF_INET6 if ':' in srv else socket.AF_INET
  s=socket.socket(fam,socket.SOCK_DGRAM); s.settimeout(3)
  try:
    s.sendto(q,(srv,53)); r=s.recv(512); print('DNS AAAA via',srv,': rcode',r[3]&15,'answers',struct.unpack('!H',r[6:8])[0])
  except Exception as e: print('DNS via',srv,':',e)
"
J ping -6 -c2 -W2 2606:4700:4700::1111 >/dev/null 2>$D/p6.err && echo "ping6 Internet: OK" || echo "ping6 Internet: FAIL ($(head -1 $D/p6.err))"
J python3 -c "
import socket
s=socket.socket(socket.AF_INET6,socket.SOCK_STREAM); s.settimeout(4)
try: s.connect(('2606:4700:4700::1111',443)); print('tcp6 Internet 443: OK')
except Exception as e: print('tcp6 Internet 443:',e)
"
J python3 -c "
import socket
s=socket.socket(socket.AF_INET,socket.SOCK_STREAM); s.settimeout(4)
try: s.connect(('1.1.1.1',443)); print('tcp4 Internet 443: OK')
except Exception as e: print('tcp4 Internet 443:',e)
"
echo "## hostfwd via api with v6 guest_addr:"
for req in '{"execute":"add_hostfwd","arguments":{"proto":"tcp","host_addr":"127.0.0.1","host_port":18765,"guest_addr":"fd00::100","guest_port":80}}' '{"execute":"add_hostfwd","arguments":{"proto":"tcp","host_addr":"::1","host_port":18766,"guest_port":80}}' '{"execute":"list_hostfwd"}'; do
  printf '%s' "$req" | python3 -c "
import socket,sys
s=socket.socket(socket.AF_UNIX); s.connect('$D/api.sock'); s.sendall(sys.stdin.read().encode()); s.shutdown(socket.SHUT_WR)
print('  ', s.recv(4096).decode())"
done
kill $SPID $NSPID 2>/dev/null; wait 2>/dev/null; rm -rf $D
