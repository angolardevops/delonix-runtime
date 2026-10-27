//! The RGW object-store port (ADR-0081): tenant S3 identities and the
//! administration of their buckets, on the Ceph Object Gateway.
//!
//! Object storage has no volume shape — a bucket is not a disk and not a
//! directory — so it is not a [`StoragePoolDriver`](delonix_storage::pool::StoragePoolDriver).
//! The platform's native surface is what `radosgw-admin` owns: it provisions a
//! tenant's S3 user (its keys and quota) and administers the tenant's buckets
//! (lists, measures, removes). A bucket is CREATED by the tenant over S3, the
//! normal object-storage model; the engine never holds a tenant's data on a
//! command line.

use std::path::PathBuf;
use std::sync::Arc;

use delonix_storage::exec::Run;

use crate::ceph::{self, Answer};

/// A tenant's S3 identity on the gateway.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RgwUser {
    pub uid: String,
    pub display_name: String,
    pub access_key: String,
    pub secret_key: String,
}

/// What a bucket holds, as the gateway measures it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct BucketInfo {
    pub name: String,
    pub owner: String,
    pub objects: u64,
    pub size_bytes: u64,
}

/// A failure of the object-store port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RgwError {
    /// `radosgw-admin` is not installed on this node.
    Unavailable(String),
    /// The gateway or cluster could not be reached, or the caller may not.
    Undetermined(String),
    /// The bucket or user is not there.
    NotFound(String),
}

pub type RgwResult<T> = Result<T, RgwError>;

fn map_fail(a: Answer) -> RgwError {
    match a {
        Answer::Unavailable(m) => RgwError::Unavailable(m),
        Answer::Missing(m) => RgwError::NotFound(m),
        Answer::Undetermined(m) => RgwError::Undetermined(m),
        // `map_fail` is only ever handed a non-success answer; a success here
        // would be a caller's mistake, not the gateway's.
        Answer::Ok(_) => RgwError::Undetermined("radosgw-admin returned no failure".into()),
    }
}

/// The RGW object store, reached through `radosgw-admin`.
pub struct Rgw {
    run: Arc<dyn Run>,
    conf: Option<PathBuf>,
}

impl Rgw {
    pub fn new(run: Arc<dyn Run>, conf: Option<PathBuf>) -> Self {
        Self { run, conf }
    }

    /// `radosgw-admin <-c conf> <parts…>`. The tool authenticates as
    /// `client.admin` from the keyring; no `--id` is forced on it.
    fn argv(&self, parts: &[&str]) -> Vec<String> {
        let mut v = vec!["radosgw-admin".to_string()];
        if let Some(conf) = &self.conf {
            v.push("-c".into());
            v.push(conf.display().to_string());
        }
        v.extend(parts.iter().map(|s| s.to_string()));
        v
    }

    fn call(&self, parts: &[&str]) -> Answer {
        ceph::run(&*self.run, &self.argv(parts))
    }

    /// Can this node administer the gateway at all?
    pub fn probe(&self) -> RgwResult<()> {
        match self.call(&["bucket", "list"]) {
            Answer::Ok(_) => Ok(()),
            other => Err(map_fail(other)),
        }
    }

    /// The tenant's S3 identity: made if it is not there, read back if it is.
    pub fn ensure_user(&self, uid: &str, display_name: &str) -> RgwResult<RgwUser> {
        // Already there?
        if let Answer::Ok(out) = self.call(&["user", "info", "--uid", uid]) {
            if let Some(u) = parse_user(&out.stdout) {
                return Ok(u);
            }
        }
        match self.call(&[
            "user",
            "create",
            "--uid",
            uid,
            "--display-name",
            display_name,
        ]) {
            Answer::Ok(out) => parse_user(&out.stdout).ok_or_else(|| {
                RgwError::Undetermined("created the user but could not read its keys".into())
            }),
            other => Err(map_fail(other)),
        }
    }

    /// The buckets the gateway holds, or those of one tenant.
    pub fn list_buckets(&self, uid: Option<&str>) -> RgwResult<Vec<String>> {
        let mut parts = vec!["bucket", "list"];
        if let Some(uid) = uid {
            parts.push("--uid");
            parts.push(uid);
        }
        match self.call(&parts) {
            Answer::Ok(out) => {
                Ok(serde_json::from_str::<Vec<String>>(out.stdout.trim()).unwrap_or_default())
            }
            other => Err(map_fail(other)),
        }
    }

    /// How full one bucket is.
    pub fn bucket_stats(&self, bucket: &str) -> RgwResult<BucketInfo> {
        match self.call(&["bucket", "stats", "--bucket", bucket]) {
            Answer::Ok(out) => parse_bucket_stats(&out.stdout).ok_or_else(|| {
                RgwError::Undetermined(format!("could not read stats for '{bucket}'"))
            }),
            other => Err(map_fail(other)),
        }
    }

    /// Removes a bucket. `purge` also deletes the objects it still holds; a
    /// non-empty bucket is refused without it.
    pub fn remove_bucket(&self, bucket: &str, purge: bool) -> RgwResult<()> {
        let mut parts = vec!["bucket", "rm", "--bucket", bucket];
        if purge {
            parts.push("--purge-objects");
        }
        match self.call(&parts) {
            Answer::Ok(_) | Answer::Missing(_) => Ok(()),
            other => Err(map_fail(other)),
        }
    }

    /// Removes a tenant's S3 identity. `purge` also deletes the buckets and
    /// objects it still owns; WITHOUT it the gateway refuses a user that still
    /// owns data — so a caller never destroys a tenant's objects by accident.
    /// A user that is already gone is success (idempotent teardown).
    pub fn remove_user(&self, uid: &str, purge: bool) -> RgwResult<()> {
        let mut parts = vec!["user", "rm", "--uid", uid];
        if purge {
            parts.push("--purge-data");
        }
        match self.call(&parts) {
            Answer::Ok(_) | Answer::Missing(_) => Ok(()),
            other => Err(map_fail(other)),
        }
    }
}

/// The first key pair of a `radosgw-admin user {create,info}` document.
fn parse_user(json: &str) -> Option<RgwUser> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let key = v.get("keys")?.as_array()?.first()?;
    Some(RgwUser {
        uid: v.get("user_id")?.as_str()?.to_string(),
        display_name: v
            .get("display_name")
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string(),
        access_key: key.get("access_key")?.as_str()?.to_string(),
        secret_key: key.get("secret_key")?.as_str()?.to_string(),
    })
}

/// The object count and the size from a `radosgw-admin bucket stats` document.
fn parse_bucket_stats(json: &str) -> Option<BucketInfo> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let name = v.get("bucket")?.as_str()?.to_string();
    let owner = v
        .get("owner")
        .and_then(|o| o.as_str())
        .unwrap_or_default()
        .to_string();
    // under usage -> "rgw.main" -> the object count and the actual size in bytes
    let main = v.get("usage").and_then(|u| u.get("rgw.main"));
    let objects = main
        .and_then(|m| m.get("num_objects"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let size_bytes = main
        .and_then(|m| m.get("size_actual").or_else(|| m.get("size")))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    Some(BucketInfo {
        name,
        owner,
        objects,
        size_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use delonix_storage::exec::testing::Script;

    #[test]
    fn argv_carries_the_conf_but_forces_no_id() {
        let rgw = Rgw::new(Arc::new(Script::new()), Some("/etc/ceph/ceph.conf".into()));
        assert_eq!(
            rgw.argv(&["bucket", "list"]),
            vec![
                "radosgw-admin",
                "-c",
                "/etc/ceph/ceph.conf",
                "bucket",
                "list"
            ]
        );
    }

    #[test]
    fn ensure_user_reads_an_existing_identity_before_creating() {
        let script = Arc::new(Script::new());
        script.on(
            &["radosgw-admin", "-c", "/etc/ceph/ceph.conf", "user", "info"],
            0,
            r#"{"user_id":"delonix","display_name":"Delonix","keys":[{"access_key":"AK","secret_key":"SK"}]}"#,
            "",
        );
        let rgw = Rgw::new(script.clone(), Some("/etc/ceph/ceph.conf".into()));
        let u = rgw.ensure_user("delonix", "Delonix").unwrap();
        assert_eq!(u.access_key, "AK");
        assert_eq!(u.secret_key, "SK");
        // It did not create — only `user info` was run.
        assert!(script
            .calls()
            .iter()
            .all(|c| !c.contains(&"create".to_string())));
    }

    #[test]
    fn list_buckets_parses_the_json_array() {
        let script = Arc::new(Script::new());
        script.on(&["radosgw-admin"], 0, r#"["a","b"]"#, "");
        let rgw = Rgw::new(script, None);
        assert_eq!(rgw.list_buckets(None).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn bucket_stats_reads_objects_and_size() {
        let json = r#"{"bucket":"delonix-bucket","owner":"delonix","usage":{"rgw.main":{"num_objects":3,"size_actual":4096}}}"#;
        let b = parse_bucket_stats(json).unwrap();
        assert_eq!(b.name, "delonix-bucket");
        assert_eq!(b.objects, 3);
        assert_eq!(b.size_bytes, 4096);
    }

    #[test]
    fn a_missing_tool_is_unavailable() {
        let rgw = Rgw::new(Arc::new(Script::new()), None);
        // No rule → NotFound(start) → Unavailable.
        assert!(matches!(rgw.probe(), Err(RgwError::Unavailable(_))));
    }
}
