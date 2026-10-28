# MicroCeph test cluster — access facts (session feat/ceph-native-storage)

- fsid: 8ce5d316-a462-411e-8297-9ecb986608f1
- mon addr: 10.3.31.35:3300 (v2) / :6789 (v1) — **anchored on `lo` as /32** (host DHCP IP churns)
- conf: /etc/ceph/ceph.conf  (+ snap: /var/snap/microceph/current/conf/ceph.conf)
- admin keyring: /etc/ceph/ceph.client.admin.keyring  (client.admin)
- native client tools: ceph/rbd/rados/radosgw-admin 19.2.3 (apt ceph-common), unconfined
- snap tools (confined, private /tmp): microceph.ceph / microceph.rbd / microceph.rados

## Surfaces live
- RADOS pool: testpool (app=rados); obj `obj1`
- RBD pool: rbdpool (inited), image `testimg` 256MiB
- CephFS: fs `delonixfs` (pools cephfs.delonixfs.{meta,data}); mount: `mount -t ceph admin@.delonixfs=/ MNT -o secret=<key>,mon_addr=10.3.31.35`
- RGW/S3: endpoint http://10.3.31.35:80 ; user `delonix` ak=delonixkey sk=delonixsecret ; bucket `delonix-bucket`

## FRAGILE / must re-apply after host reboot
1. `sudo ip addr add 10.3.31.35/32 dev lo`   (else mon cannot bind: errno 99)
2. mon avail crit/warn lowered IN ceph.conf ([global] mon data avail crit=1, warn=2) — host disk ~94%; `ceph config set` does NOT help (mon dies before reading centralized config)
3. snap auto-refresh HELD (`snap refresh --hold microceph`) — an auto-refresh mid-bootstrap tore the cluster down once
4. If services down after reboot: `sudo snap restart microceph.daemon microceph.mon microceph.osd microceph.mgr microceph.mds`
