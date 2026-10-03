//! `VolumeService`, the read half: `GetVolume` and `ListVolumes`
//! (ADR-0042 step E).
//!
//! **Namespaces.** The store has two places: the unscoped root, where every
//! `delonix volume create` lands and which a workload of any namespace
//! resolves, and one sub-tree per namespace, where a share declared in that
//! namespace lives. The contract has no "no namespace", so the root is
//! reported in `default`, together with whatever the `default` sub-tree holds;
//! every other namespace reports its own sub-tree; `*` lists all of them.
//! When the root and the `default` sub-tree both have a volume of one name,
//! the list shows both and `GetVolume` refuses — the same refusal a workload
//! gets when it mounts that name.
//!
//! **Usage.** Measuring a volume walks its data. `GetVolume` does it;
//! `ListVolumes` does not (a list of every volume of a node would walk every
//! byte the node stores). `used_bytes` is set only when the walk was complete,
//! and the `UsageMeasured` condition says which of the three it was: measured,
//! not readable by this uid, or not measured in a list. Unknown is never zero.

use std::path::Path;

use delonix_volume::{OwnedVolume, VolumeStore};
use tonic::Status;

use crate::page;
use crate::proto::v1::{
    volume_spec, Condition, ConditionStatus, GetVolumeRequest, ListVolumesRequest,
    ListVolumesResponse, LocalVolume, PageResponse, RemoteProtocol, RemoteShare, ResourceMeta,
    ShareOf, Volume, VolumeSpec,
};
use crate::selector::Selector;

/// The namespace the unscoped root is reported in.
pub const DEFAULT_NAMESPACE: &str = "default";

/// One volume with the namespace it is reported in and whether it comes from
/// the unscoped root.
struct Entry {
    namespace: String,
    scoped: bool,
    volume: delonix_volume::Volume,
}

impl Entry {
    /// The order of a list, and what a page token carries: namespace, name,
    /// and the root before the sub-tree for a name both have.
    fn key(&self) -> String {
        format!(
            "{}/{}/{}",
            self.namespace,
            self.volume.name,
            u8::from(self.scoped)
        )
    }
}

/// Every volume under `root`, as reported. A state root that never had a
/// volume has no `volumes/` directory: an empty list, and reading does not
/// create it.
fn entries(root: &Path) -> Result<Vec<Entry>, Status> {
    if !root.join("volumes").is_dir() {
        return Ok(Vec::new());
    }
    let store = VolumeStore::open(root)
        .map_err(|e| Status::internal(format!("opening the volume store: {e}")))?;
    let all = store
        .list_all()
        .map_err(|e| Status::internal(format!("listing volumes: {e}")))?;
    let mut out: Vec<Entry> = all
        .into_iter()
        .map(|OwnedVolume { namespace, volume }| Entry {
            scoped: namespace.is_some(),
            namespace: namespace.unwrap_or_else(|| DEFAULT_NAMESPACE.to_string()),
            volume,
        })
        .collect();
    out.sort_by_key(Entry::key);
    Ok(out)
}

/// What the caller asked to see: `""` is `default`, `*` is every namespace.
fn wanted(namespace: &str) -> Option<&str> {
    match namespace {
        "*" => None,
        "" => Some(DEFAULT_NAMESPACE),
        ns => Some(ns),
    }
}

/// How the usage of a volume is known.
enum Measured {
    /// The walk read every directory.
    Yes(u64),
    /// Some directories are not readable by this uid (data written under a
    /// mapped subuid); the bytes seen are a floor, not the usage.
    Unreadable(u64),
    /// Not asked: a list does not walk the data.
    NotInList,
}

fn condition(kind: &str, status: ConditionStatus, reason: &str, message: String) -> Condition {
    Condition {
        r#type: kind.into(),
        status: status as i32,
        reason: reason.into(),
        message,
        last_transition_time: None,
    }
}

/// The contract's source for a record: a share of another volume, a remote
/// share, or local data.
fn source(v: &delonix_volume::Volume) -> volume_spec::Source {
    if let Some(parent) = &v.parent {
        return volume_spec::Source::ShareOf(ShareOf {
            parent_volume: parent.clone(),
        });
    }
    let device = v.device.as_deref().unwrap_or_default();
    let (protocol, server, share) = match v.driver.as_str() {
        // `server:/path`
        "nfs" => {
            let (server, share) = device.split_once(':').unwrap_or(("", device));
            (RemoteProtocol::Nfs, server, share)
        }
        // `//server/share`
        "cifs" => {
            let rest = device.trim_start_matches('/');
            let (server, share) = rest.split_once('/').unwrap_or((rest, ""));
            (RemoteProtocol::Smb, server, share)
        }
        // A URL: the whole of it is where the share is.
        "davfs" => (RemoteProtocol::Webdav, "", device),
        _ => return volume_spec::Source::Local(LocalVolume {}),
    };
    let read_only = v
        .options
        .as_deref()
        .is_some_and(|o| o.split(',').any(|opt| opt.trim() == "ro"));
    volume_spec::Source::Remote(RemoteShare {
        protocol: protocol as i32,
        server: server.to_string(),
        share: share.to_string(),
        // The record keeps a credentials FILE for the mount, not the name of
        // the secret it was written from.
        credentials_secret: String::new(),
        read_only,
    })
}

fn message(e: &Entry, measured: Measured) -> Volume {
    let v = &e.volume;
    let collection = format!("/v1/namespaces/{}/volumes", page::escape(&e.namespace));
    let (used_bytes, usage) = match measured {
        Measured::Yes(bytes) => (
            Some(bytes as i64),
            condition(
                "UsageMeasured",
                ConditionStatus::True,
                "Measured",
                "every directory of the volume was read".into(),
            ),
        ),
        Measured::Unreadable(floor) => (
            None,
            condition(
                "UsageMeasured",
                ConditionStatus::False,
                "UnreadableDirectories",
                format!(
                    "some directories are not readable by the uid serving this API; at least \
                     {floor} bytes are in use (`delonix volume describe` measures from inside \
                     the user namespace that owns them)"
                ),
            ),
        ),
        Measured::NotInList => (
            None,
            condition(
                "UsageMeasured",
                ConditionStatus::Unknown,
                "NotMeasuredInList",
                "a list does not walk the data; read the volume by name".into(),
            ),
        ),
    };
    Volume {
        meta: Some(ResourceMeta {
            name: v.name.clone(),
            namespace: e.namespace.clone(),
            labels: v.labels.clone().into_iter().collect(),
            annotations: v.annotations.clone().into_iter().collect(),
            create_time: Some(pbjson_types::Timestamp {
                seconds: v.created_unix as i64,
                nanos: 0,
            }),
            // Of the record, not of the usage: data written to a volume does
            // not change what the volume is.
            etag: page::fnv(&format!("{}|{v:?}", e.scoped)),
            ..Default::default()
        }),
        spec: Some(VolumeSpec {
            source: Some(source(v)),
            quota_bytes: v.quota_bytes.map(|q| q as i64).unwrap_or_default(),
            alert_percent: v.alert_pct.map(i32::from).unwrap_or_default(),
            provision: None,
        }),
        used_bytes,
        conditions: vec![usage],
        links: vec![
            crate::node::link("self", &format!("{collection}/{}", page::escape(&v.name))),
            crate::node::link("collection", &collection),
        ],
    }
}

/// `GetVolume` under `root`, with its usage measured.
pub fn get_in(root: &Path, req: &GetVolumeRequest) -> Result<Volume, Status> {
    let ns = wanted(&req.namespace).unwrap_or("*");
    let missing = || Status::not_found(format!("volume '{}' in namespace '{ns}'", req.name));
    if ns == "*" {
        return Err(missing());
    }
    let mut found: Vec<Entry> = entries(root)?
        .into_iter()
        .filter(|e| e.namespace == ns && e.volume.name == req.name)
        .collect();
    if found.len() > 1 {
        return Err(Status::failed_precondition(format!(
            "volume '{}' exists twice in namespace '{ns}': once in the node's unscoped store \
             and once in the namespace's own — remove or rename one of them (`delonix volume \
             ls -A` shows both)",
            req.name
        )));
    }
    let entry = found.pop().ok_or_else(missing)?;
    let usage = delonix_volume::measure(Path::new(&entry.volume.mountpoint));
    let measured = if usage.is_complete() {
        Measured::Yes(usage.bytes)
    } else {
        Measured::Unreadable(usage.bytes)
    };
    Ok(message(&entry, measured))
}

/// `ListVolumes` under `root`: filtered by `label_selector`, ordered by
/// namespace and name, one page at a time, usage not measured.
pub fn list_in(root: &Path, req: &ListVolumesRequest) -> Result<ListVolumesResponse, Status> {
    let selector = Selector::parse(&req.label_selector).map_err(Status::invalid_argument)?;
    let page_req = req.page.clone().unwrap_or_default();
    let size = page::size(&page_req)?;
    let after = page::after(&page_req)?;
    let ns = wanted(&req.namespace);
    let all = entries(root)?;
    let mut matching = all
        .iter()
        .filter(|e| ns.is_none_or(|n| e.namespace == n))
        .filter(|e| selector.matches(&e.volume.labels))
        .filter(|e| after.as_ref().is_none_or(|a| e.key() > *a));
    let shown: Vec<&Entry> = matching.by_ref().take(size).collect();
    let next = match (matching.next(), shown.last()) {
        (Some(_), Some(last)) => page::encode_token(&last.key()),
        _ => String::new(),
    };
    let base = format!("/v1/namespaces/{}/volumes", page::escape(ns.unwrap_or("*")));
    let href = |token: &str| page::href(&base, &req.label_selector, &page_req, token);
    let mut links = vec![
        crate::node::link("self", &href(&page_req.page_token)),
        crate::node::link("root", "/v1"),
    ];
    if !next.is_empty() {
        links.push(crate::node::link("next", &href(&next)));
    }
    Ok(ListVolumesResponse {
        volumes: shown
            .into_iter()
            .map(|e| message(e, Measured::NotInList))
            .collect(),
        page: Some(PageResponse {
            next_page_token: next,
        }),
        links,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::v1::PageRequest;

    /// A store with: `data` and `logs` in the root (`data` labelled, with a
    /// quota and 5 bytes written), an NFS volume `nas`, and a share `db` of
    /// `data` in namespace `team-a`.
    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let store = VolumeStore::open(dir.path()).unwrap();
        let data = store.create("data").unwrap();
        std::fs::write(Path::new(&data.mountpoint).join("f"), b"12345").unwrap();
        store
            .set_metadata("data", &[("app".into(), Some("web".into()))], &[])
            .unwrap();
        store
            .set_quota("data", Some(2 << 30), Some(80), false)
            .unwrap();
        store.create("logs").unwrap();
        let write = |s: &VolumeStore, v: &delonix_volume::Volume| {
            let d = s.volume_dir(&v.name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("meta.json"), serde_json::to_vec(v).unwrap()).unwrap();
        };
        let mut nas = store.inspect("logs").unwrap();
        nas.name = "nas".into();
        nas.driver = "nfs".into();
        nas.device = Some("10.0.0.9:/export/app".into());
        nas.options = Some("vers=4,ro".into());
        write(&store, &nas);
        let team = store.scoped("team-a").unwrap();
        let mut db = store.inspect("logs").unwrap();
        db.name = "db".into();
        db.parent = Some("data".into());
        write(&team, &db);
        dir
    }

    fn list(root: &Path, ns: &str, selector: &str, size: i32, token: &str) -> ListVolumesResponse {
        list_in(
            root,
            &ListVolumesRequest {
                namespace: ns.into(),
                label_selector: selector.into(),
                page: Some(PageRequest {
                    page_size: size,
                    page_token: token.into(),
                }),
            },
        )
        .unwrap()
    }

    fn names(r: &ListVolumesResponse) -> Vec<String> {
        r.volumes
            .iter()
            .map(|v| {
                let m = v.meta.as_ref().unwrap();
                format!("{}/{}", m.namespace, m.name)
            })
            .collect()
    }

    #[test]
    fn the_root_is_default_a_namespace_has_its_own_and_star_has_all() {
        let dir = root();
        assert_eq!(
            names(&list(dir.path(), "", "", 0, "")),
            ["default/data", "default/logs", "default/nas"]
        );
        assert_eq!(names(&list(dir.path(), "team-a", "", 0, "")), ["team-a/db"]);
        assert_eq!(
            names(&list(dir.path(), "*", "", 0, "")),
            ["default/data", "default/logs", "default/nas", "team-a/db"]
        );
        assert!(list(dir.path(), "nobody", "", 0, "").volumes.is_empty());
    }

    #[test]
    fn a_get_measures_the_usage_and_a_list_says_it_did_not() {
        let dir = root();
        let got = get_in(
            dir.path(),
            &GetVolumeRequest {
                name: "data".into(),
                namespace: String::new(),
            },
        )
        .unwrap();
        // Blocks on disk, as `du` counts them: more than nothing for a
        // five-byte file, whatever the filesystem's block size.
        assert!(
            got.used_bytes.is_some_and(|b| b > 0),
            "{:?}",
            got.used_bytes
        );
        let c = &got.conditions[0];
        assert_eq!(
            (c.r#type.as_str(), c.status, c.reason.as_str()),
            ("UsageMeasured", ConditionStatus::True as i32, "Measured")
        );
        let spec = got.spec.unwrap();
        assert_eq!((spec.quota_bytes, spec.alert_percent), (2 << 30, 80));
        assert!(matches!(spec.source, Some(volume_spec::Source::Local(_))));
        assert_eq!(got.meta.as_ref().unwrap().labels["app"], "web");
        assert_eq!(got.links[0].href, "/v1/namespaces/default/volumes/data");

        let listed = list(dir.path(), "", "", 0, "");
        let data = &listed.volumes[0];
        // Unknown is not zero: no number at all, and the condition says why.
        assert_eq!(data.used_bytes, None);
        let c = &data.conditions[0];
        assert_eq!(
            (c.status, c.reason.as_str()),
            (ConditionStatus::Unknown as i32, "NotMeasuredInList")
        );
    }

    #[test]
    fn writing_data_does_not_change_the_etag_and_changing_the_record_does() {
        let dir = root();
        let get = || {
            get_in(
                dir.path(),
                &GetVolumeRequest {
                    name: "data".into(),
                    namespace: "default".into(),
                },
            )
            .unwrap()
        };
        let before = get();
        let store = VolumeStore::open(dir.path()).unwrap();
        let mp = store.inspect("data").unwrap().mountpoint;
        std::fs::write(Path::new(&mp).join("g"), b"more").unwrap();
        let after_write = get();
        assert!(after_write.used_bytes > before.used_bytes);
        assert_eq!(
            after_write.meta.as_ref().unwrap().etag,
            before.meta.as_ref().unwrap().etag
        );
        store
            .set_quota("data", Some(1 << 30), Some(80), false)
            .unwrap();
        assert_ne!(get().meta.unwrap().etag, before.meta.unwrap().etag);
    }

    #[test]
    fn a_remote_volume_and_a_share_say_what_they_are() {
        let dir = root();
        let nas = get_in(
            dir.path(),
            &GetVolumeRequest {
                name: "nas".into(),
                namespace: String::new(),
            },
        )
        .unwrap();
        match nas.spec.unwrap().source {
            Some(volume_spec::Source::Remote(r)) => {
                assert_eq!(r.protocol, RemoteProtocol::Nfs as i32);
                assert_eq!(
                    (r.server.as_str(), r.share.as_str()),
                    ("10.0.0.9", "/export/app")
                );
                assert!(r.read_only);
                assert!(r.credentials_secret.is_empty());
            }
            other => panic!("expected a remote share, got {other:?}"),
        }
        let db = get_in(
            dir.path(),
            &GetVolumeRequest {
                name: "db".into(),
                namespace: "team-a".into(),
            },
        )
        .unwrap();
        match db.spec.unwrap().source {
            Some(volume_spec::Source::ShareOf(s)) => assert_eq!(s.parent_volume, "data"),
            other => panic!("expected a share, got {other:?}"),
        }
        // A share lives in its namespace, not in default.
        let err = get_in(
            dir.path(),
            &GetVolumeRequest {
                name: "db".into(),
                namespace: String::new(),
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn a_name_in_the_root_and_in_the_default_subtree_is_listed_twice_and_refused_by_name() {
        let dir = root();
        let store = VolumeStore::open(dir.path()).unwrap();
        let scoped = store.scoped("default").unwrap();
        let mut twin = store.inspect("logs").unwrap();
        twin.parent = Some("data".into());
        let d = scoped.volume_dir("logs");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("meta.json"), serde_json::to_vec(&twin).unwrap()).unwrap();

        let listed = names(&list(dir.path(), "", "", 0, ""));
        assert_eq!(listed.iter().filter(|n| *n == "default/logs").count(), 2);
        let err = get_in(
            dir.path(),
            &GetVolumeRequest {
                name: "logs".into(),
                namespace: String::new(),
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        // And a page boundary between the two does not lose or repeat either.
        let first = list(dir.path(), "", "", 2, "");
        let token = first.page.clone().unwrap().next_page_token;
        let second = list(dir.path(), "", "", 2, &token);
        let mut all = names(&first);
        all.extend(names(&second));
        assert_eq!(
            all,
            [
                "default/data",
                "default/logs",
                "default/logs",
                "default/nas"
            ]
        );
    }

    #[test]
    fn the_selector_filters_and_the_next_link_keeps_it() {
        let dir = root();
        assert_eq!(
            names(&list(dir.path(), "*", "app=web", 0, "")),
            ["default/data"]
        );
        let first = list(dir.path(), "*", "app!=web", 1, "");
        assert_eq!(names(&first), ["default/logs"]);
        let next = first.links.iter().find(|l| l.rel == "next").unwrap();
        assert!(next.href.starts_with("/v1/namespaces/*/volumes?label_selector=app%21%3Dweb&page.page_size=1&page.page_token="), "{}", next.href);
        let err = list_in(
            dir.path(),
            &ListVolumesRequest {
                namespace: String::new(),
                label_selector: "app in (web)".into(),
                page: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_root_that_never_had_a_volume_is_an_empty_list_and_stays_untouched() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(dir.path(), "*", "", 0, "").volumes.is_empty());
        assert!(!dir.path().join("volumes").exists());
    }
}
