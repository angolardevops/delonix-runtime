import socket, struct, sys
ifname, prefix = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_INET6, socket.SOCK_RAW, socket.IPPROTO_ICMPV6)
s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_HOPS, 255)
idx = socket.if_nametoindex(ifname)
s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_IF, idx)
ra = struct.pack('!BBHBBHII', 134, 0, 0, 64, 0, 1800, 0, 0)
pio = struct.pack('!BBBBIII', 3, 4, 64, 0xC0, 86400, 14400, 0) + socket.inet_pton(socket.AF_INET6, prefix)
s.sendto(ra + pio, ('ff02::1', 0, 0, idx))
print('ra sent', prefix)
