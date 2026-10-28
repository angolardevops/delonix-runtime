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

use base64::Engine as _;
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

    /// Creates the bucket `bucket`, owned by the identity whose keys are given,
    /// with an S3 `PUT /{bucket}` against `endpoint`. A bucket is not shaped like
    /// a disk — it is made over S3, not by `radosgw-admin` — so this is the one
    /// call that speaks the gateway's data plane. Creating an EMPTY bucket holds
    /// no tenant data, so it stays on the right side of ADR-0081's boundary.
    ///
    /// Idempotent: a bucket this identity already owns is success (the gateway's
    /// `BucketAlreadyOwnedByYou`), so a re-apply does not fail.
    pub fn create_bucket(
        &self,
        bucket: &str,
        access_key: &str,
        secret_key: &str,
        endpoint: &str,
    ) -> RgwResult<()> {
        let date = httpdate::fmt_http_date(std::time::SystemTime::now());
        // Path-style: the bucket is the first path segment, so the signed
        // resource and the URL path are both `/{bucket}`.
        let resource = format!("/{bucket}");
        let authorization = sign_v2("PUT", &resource, &date, access_key, secret_key);
        let url = format!("{}{resource}", endpoint.trim_end_matches('/'));

        let resp = reqwest::blocking::Client::new()
            .put(&url)
            .header(reqwest::header::DATE, &date)
            .header(reqwest::header::AUTHORIZATION, &authorization)
            // An empty body: Content-Length 0 and — crucially — NO Content-Type,
            // which the StringToSign leaves blank; a stray type would mismatch.
            .body(Vec::<u8>::new())
            .send()
            .map_err(|e| {
                RgwError::Undetermined(format!("S3 PUT bucket '{bucket}' at {url}: {e}"))
            })?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().unwrap_or_default();
        // The gateway reports an already-owned bucket as 409; that is the
        // idempotent case, not a failure.
        if body.contains("BucketAlreadyOwnedByYou") {
            return Ok(());
        }
        Err(RgwError::Undetermined(format!(
            "S3 PUT bucket '{bucket}': {status} {body}"
        )))
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

    /// Sets (and enables) a user quota on the tenant's S3 identity — a ceiling
    /// on the total bytes and/or object count across all its buckets. A `None`
    /// leaves that dimension unset; both `None` is a no-op. `max_size` takes the
    /// gateway's own spelling (`100G`, `1T`, or a byte count); a negative value
    /// is the gateway's "unlimited". Idempotent: re-setting the same ceiling is
    /// not an error.
    pub fn set_user_quota(
        &self,
        uid: &str,
        max_size: Option<&str>,
        max_objects: Option<i64>,
    ) -> RgwResult<()> {
        if max_size.is_none() && max_objects.is_none() {
            return Ok(());
        }
        let mut parts: Vec<String> = ["quota", "set", "--quota-scope", "user", "--uid", uid]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(s) = max_size {
            parts.push("--max-size".into());
            parts.push(s.to_string());
        }
        if let Some(n) = max_objects {
            parts.push("--max-objects".into());
            parts.push(n.to_string());
        }
        let argv: Vec<&str> = parts.iter().map(String::as_str).collect();
        match self.call(&argv) {
            Answer::Ok(_) => {}
            other => return Err(map_fail(other)),
        }
        // A quota that is set but not enabled is silently not enforced — the
        // enable is what makes the ceiling real.
        match self.call(&["quota", "enable", "--quota-scope", "user", "--uid", uid]) {
            Answer::Ok(_) => Ok(()),
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

/// The AWS Signature Version 2 `Authorization` header value for a request, as
/// RGW's S3 endpoint accepts it (`AWS <access-key>:<signature>`).
///
/// The StringToSign is `VERB \n Content-MD5 \n Content-Type \n Date \n
/// CanonicalizedResource`; an empty body carries no Content-MD5 and no
/// Content-Type, so two lines are blank. The signature is
/// `base64(HMAC-SHA1(secret, StringToSign))`.
fn sign_v2(
    method: &str,
    canonical_resource: &str,
    date: &str,
    access_key: &str,
    secret_key: &str,
) -> String {
    let string_to_sign = format!("{method}\n\n\n{date}\n{canonical_resource}");
    let key = ring::hmac::Key::new(
        ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        secret_key.as_bytes(),
    );
    let tag = ring::hmac::sign(&key, string_to_sign.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(tag.as_ref());
    format!("AWS {access_key}:{signature}")
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
    fn set_user_quota_sets_then_enables() {
        let script = Arc::new(Script::new());
        script.on(&["radosgw-admin"], 0, "", "");
        let rgw = Rgw::new(script.clone(), None);
        rgw.set_user_quota("delonix", Some("100G"), Some(10_000))
            .unwrap();
        let calls = script.calls();
        // The set carries both ceilings and the scope...
        assert!(calls.iter().any(|c| c.contains(&"set".to_string())
            && c.contains(&"--max-size".to_string())
            && c.contains(&"100G".to_string())
            && c.contains(&"--max-objects".to_string())
            && c.contains(&"10000".to_string())));
        // ...and a separate enable makes it enforced (a set alone is silent).
        assert!(calls
            .iter()
            .any(|c| c.contains(&"enable".to_string()) && c.contains(&"user".to_string())));
    }

    #[test]
    fn set_user_quota_with_nothing_is_a_noop() {
        let script = Arc::new(Script::new());
        let rgw = Rgw::new(script.clone(), None);
        rgw.set_user_quota("delonix", None, None).unwrap();
        assert!(script.calls().is_empty(), "no quota fields → no call");
    }

    #[test]
    fn sign_v2_matches_the_aws_reference_vector() {
        // The canonical example from the AWS S3 REST authentication reference
        // (GET /johnsmith/photos/puppy.jpg) — if our HMAC-SHA1 + base64 and the
        // StringToSign layout are right, we reproduce its published signature.
        let auth = sign_v2(
            "GET",
            "/johnsmith/photos/puppy.jpg",
            "Tue, 27 Mar 2007 19:36:42 +0000",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        assert_eq!(auth, "AWS AKIAIOSFODNN7EXAMPLE:bWq2s1WEIj+Ydj0vQ697zp+IXMU=");
    }

    #[test]
    fn sign_v2_signs_a_bucket_put_with_an_empty_body() {
        // A bucket PUT: empty Content-MD5 and Content-Type, the resource is the
        // bucket. Pinned so a change to the StringToSign layout is caught.
        let auth = sign_v2("PUT", "/media", "Wed, 28 Oct 2026 12:00:00 GMT", "AK", "SK");
        assert_eq!(auth, "AWS AK:TMe1pk3zdCs9Wh2IPsyU3LU/Khw=");
    }

    #[test]
    fn a_missing_tool_is_unavailable() {
        let rgw = Rgw::new(Arc::new(Script::new()), None);
        // No rule → NotFound(start) → Unavailable.
        assert!(matches!(rgw.probe(), Err(RgwError::Unavailable(_))));
    }
}
