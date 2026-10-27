//! `ImageService`, the read half: `ListImages` and `GetImage`.
//!
//! Images are node-wide, not namespaced (the routes are `/v1/images`, not
//! `/v1/namespaces/{ns}/images`). The source is the OCI image store
//! (`delonix_oci::ImageStore`), read in-process — the same store the CLI
//! `image ls` reads; no re-exec of `delonix` for a read.
//!
//! **One entry per reference.** A stored image can carry several tags; the
//! contract's `Image` is reference-centric (one `reference` + a digest), so a
//! multi-tag image is reported once per tag — the same choice `image ls` made
//! (a multi-tag image used to show only the first, hiding the other names it
//! can be pulled by). An untagged (dangling) image is reported once with an
//! empty `reference` and its digest.
//!
//! **Size.** The list sums the on-disk size of the image's layer blobs in the
//! CAS (`stat`, never reading them). It is 0 when no layer blob is present —
//! the same "unmeasurable" the CLI reports as `-`.

use std::path::Path;

use delonix_oci::ImageStore;
use tonic::Status;

use crate::proto::v1::{
    GetImageRequest, Image, ListImagesRequest, ListImagesResponse, PageResponse,
};

/// On-disk size of an image: the sum of its layer blobs in the CAS. 0 when no
/// layer blob is present (unmeasurable), mirroring the CLI's `-`.
fn size_bytes(store: &ImageStore, img: &delonix_oci::Image) -> i64 {
    let mut total: u64 = 0;
    for layer in &img.layers {
        if let Ok(meta) = std::fs::metadata(store.cas().path(layer)) {
            total += meta.len();
        }
    }
    total as i64
}

/// A proto `Image` for one reference of a stored image (shared digest/size).
fn message(reference: String, digest: &str, size: i64) -> Image {
    Image {
        reference,
        digest: digest.to_string(),
        repo_digests: Vec::new(),
        size_bytes: size,
        os: String::new(),
        architecture: String::new(),
        labels: Default::default(),
    }
}

/// Every reference of a stored image: one per tag, or one empty reference when
/// the image is untagged (dangling).
fn messages(store: &ImageStore, img: &delonix_oci::Image) -> Vec<Image> {
    let size = size_bytes(store, img);
    if img.repo_tags.is_empty() {
        vec![message(String::new(), &img.id, size)]
    } else {
        img.repo_tags
            .iter()
            .map(|tag| message(tag.clone(), &img.id, size))
            .collect()
    }
}

/// `ListImages` — every stored image, newest first, one entry per reference.
/// `reference_prefix` filters by a leading substring of the reference.
pub fn list_in(root: &Path, req: &ListImagesRequest) -> Result<ListImagesResponse, Status> {
    let store = ImageStore::open(root).map_err(|e| Status::internal(e.to_string()))?;
    let mut imgs = store.list().map_err(|e| Status::internal(e.to_string()))?;
    // Newest first, as in `image ls` / `docker images`.
    imgs.sort_by_key(|i| std::cmp::Reverse(i.created_unix));

    let mut images: Vec<Image> = imgs.iter().flat_map(|img| messages(&store, img)).collect();
    if !req.reference_prefix.is_empty() {
        images.retain(|i| i.reference.starts_with(req.reference_prefix.as_str()));
    }

    Ok(ListImagesResponse {
        images,
        page: Some(PageResponse {
            next_page_token: String::new(),
        }),
    })
}

/// `GetImage` — the image a reference resolves to (a tag, an id or a digest).
pub fn get_in(root: &Path, req: &GetImageRequest) -> Result<Image, Status> {
    if req.reference.is_empty() {
        return Err(Status::invalid_argument("GetImage: reference is required"));
    }
    let store = ImageStore::open(root).map_err(|e| Status::internal(e.to_string()))?;
    let imgs = store.list().map_err(|e| Status::internal(e.to_string()))?;
    let want = &req.reference;
    let want_stripped = delonix_oci::cas::strip(want);
    for img in &imgs {
        let by_tag = img.repo_tags.iter().any(|t| t == want);
        let by_id = img.id == *want || delonix_oci::cas::strip(&img.id) == want_stripped;
        if by_tag || by_id {
            // The reference the caller asked for when it was a tag; otherwise
            // the first tag (or empty for a dangling image).
            let reference = if by_tag {
                want.clone()
            } else {
                img.repo_tags.first().cloned().unwrap_or_default()
            };
            return Ok(message(reference, &img.id, size_bytes(&store, img)));
        }
    }
    Err(Status::not_found(format!(
        "GetImage: no image for reference {want:?}"
    )))
}
