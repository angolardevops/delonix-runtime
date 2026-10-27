# ADR-0081: Native Ceph support — four surfaces of one cluster, read and served over the node API

- **Status:** Proposed (2026-10-27). Implemented on branch `feat/ceph-native-storage` and proved live
  against a single-node MicroCeph (Ceph Squid 19.2.3) stood up on the test host — all four surfaces
  (`reports/ceph-live/`). Awaiting the owner's integration GO.
- **Date:** 2026-10-27
- **Deciders:** Walter Angolar (owner)
- **Relates to:** ADR-0067 (the storage-pool driver port and the administrator's allowlist), ADR-0040
  (the node API: gRPC and HTTP/JSON of `delonix.node.v1` on one unix socket), ADR-0042 (the REST/OpenAPI
  encoding generated from the contract), ADR-0008 (registering a driver does no I/O), and guardrails 1
  (daemonless) and 6 (no silent failure).

## Context

The cloud needs the storage a Ceph cluster gives, in four shapes that are not interchangeable:

| Surface | What it is | Used for |
|---|---|---|
| **RADOS** | the distributed object store that is the substrate | the base every other surface sits on |
| **RBD** | block devices | VM disks, persistent block volumes |
| **CephFS** | a shared POSIX file system | directories shared between applications |
| **RGW** | S3/Swift object storage | tenant buckets, application files, backup targets |

The runtime had no Ceph of any kind. Two of the four shapes map cleanly onto the existing storage
abstraction: a `StoragePoolDriver` (ADR-0067) hands out either a **block** volume or a **filesystem**
volume, which is exactly RBD and CephFS. The other two do not: RADOS is a substrate, not a volume, and
RGW object storage has no volume shape at all — a bucket is neither a disk nor a directory.

## Decision

**1. One provider crate, `delonix-provider-ceph`, reaching the cluster through the storage `Run` port
— never librados.** Every existing block/filesystem driver (zfs, lvm-thin, btrfs) shells out to its
backend's native tools through `delonix_storage::exec::Run`, which the tests replace with a recorder so
each command is checked as data without a disk. Ceph is reached the same way: `rbd`, `ceph`, `rados`,
`radosgw-admin`, `mount`, `umount` — the `ceph-common` tools a node integrating Ceph already carries.
No C dependency, no build-time `librados`, and the whole crate is unit-tested offline against the
scripted `Run`.

**2. RBD and CephFS are `StoragePoolDriver`s (`ceph-rbd`, `cephfs`).** They register alongside the local
drivers in the composition root and the privileged helper; a pool's RADOS pool / CephFS name and its
client identity (`cephPool`, `cephFs`, `cephUser`, `cephConf`) come from the administrator's allowlist
and from there only (ADR-0067 D3) — a manifest still carries only a pool name. A volume is adopted or
released by a **stamp** written on the object itself (RBD image metadata, CephFS subvolume metadata),
never by its name alone (ADR-0067 D5). Making/mapping/mounting is root and goes through the helper;
reading (`probe`, `usage`) needs only a readable cephx keyring and runs in-process. **Pool/file-system
creation stays the cluster administrator's** — the helper has no `admin_for` arm for these, the same way
it never creates a volume group.

**3. RGW is its own port, not a pool driver.** Object storage has no volume shape, so `Rgw` is a plain
type over `radosgw-admin`: it provisions (or reads back) a tenant's S3 identity and administers the
tenant's buckets (list, stats, remove). A bucket is **created by the tenant over S3** — the normal
object-storage model — so the engine never holds a tenant's data on a command line.

**4. The node API gains a `StorageService` (`proto/delonix/node/v1/storage.proto`).** gRPC and
HTTP/JSON from the one contract (ADR-0040), the REST routes and the OpenAPI document generated from the
`google.api.http` annotations (ADR-0042):
- `GetCephStatus` → `GET /v1/storage/ceph/status` (RADOS health: status, osds, pools).
- `ListRbdImages` → `GET /v1/storage/pools/{pool}/rbd-images`.
- `ListCephfsSubvolumes` → `GET /v1/storage/pools/{pool}/cephfs-subvolumes`.
- `ListObjectStoreBuckets` / `GetObjectStoreBucket` / `CreateObjectStoreUser` → `/v1/storage/objectstore/…`.

Making and destroying RBD/CephFS volumes is NOT here — that is the storage-pool path (`VolumeService`
+ the helper). This service is the node's read window onto the cluster and the administration RGW owns.

## Consequences

- A node that carries `ceph-common` and an `/etc/ceph` can hand out RBD block volumes and CephFS shares
  through the same `delonix volume` / manifest path the local drivers use, and expose the cluster's state
  and its tenants' object storage over the node API.
- The contract grows by one service and six RPCs; `buf breaking` against v5.1.1 is clean (additive).
- **Out of scope, by decision:** managing the Ceph cluster's lifecycle from Delonix (bootstrap/expand/
  destroy), RBD-mirror / RGW-multisite / CephFS-mirror, and production HA tuning. Integration into
  delonix-paas / delonix-console / delonix-desktop consumes this `node.v1` contract later.

## How it was proved (live)

A single-node MicroCeph (Squid 19.2.3) was stood up on the test host with all four surfaces enabled.
The crate's own code drove each one (`cargo test -p delonix-provider-ceph --test live`,
`reports/ceph-live/crate-live-tests.log`): RADOS `HEALTH_OK` with 3 OSDs/11 pools; an RBD block volume
through its full `allocate → /dev/rbdN → adopt-by-stamp → release` lifecycle; the CephFS pool probing
Available with the stamp boundary enforced; the RGW tenant, its bucket and the bucket's stats. The REST
encoding was then driven through the node API's own router against the live cluster
(`reports/ceph-live/api-rest-live.log`): `GET /v1/storage/ceph/status` → `HEALTH_OK`,
`GET /v1/storage/objectstore/buckets` → the tenant's bucket.
