//! What every `List` shares (ADR-0042 D2): the page size, the opaque page
//! token, and the escaping of a value that goes back out in a link.

use tonic::Status;

use crate::proto::v1::PageRequest;

/// `page_size` when the caller sends 0, and the most one page carries.
pub const DEFAULT_PAGE: usize = 100;
pub const MAX_PAGE: usize = 1000;

/// The number of items a page carries: the default for 0, capped, and a
/// negative size refused.
pub fn size(page: &PageRequest) -> Result<usize, Status> {
    match page.page_size {
        0 => Ok(DEFAULT_PAGE),
        n if n < 0 => Err(Status::invalid_argument(format!(
            "page_size {n}: has to be 0 (the default, {DEFAULT_PAGE}) or more"
        ))),
        n => Ok((n as usize).min(MAX_PAGE)),
    }
}

/// A page token is the sort key of the last item of the page before, in hex:
/// opaque to the caller, and nothing the server has to remember.
pub fn encode_token(key: &str) -> String {
    key.bytes().map(|b| format!("{b:02x}")).collect()
}

/// The sort key a token carries; `None` when this server did not issue it.
pub fn decode_token(token: &str) -> Option<String> {
    if !token.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..token.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(token.get(i..i + 2)?, 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

/// The key the request's token names, or `None` for the first page.
pub fn after(page: &PageRequest) -> Result<Option<String>, Status> {
    match page.page_token.as_str() {
        "" => Ok(None),
        token => decode_token(token).map(Some).ok_or_else(|| {
            Status::invalid_argument(format!("page_token '{token}' was not issued by this list"))
        }),
    }
}

/// Percent-encodes what a query value or a path segment cannot carry as is.
pub fn escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'*' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The href of a list's page: `base`, the caller's own filters, and the page.
pub fn href(base: &str, label_selector: &str, page: &PageRequest, token: &str) -> String {
    let mut q = Vec::new();
    if !label_selector.is_empty() {
        q.push(format!("label_selector={}", escape(label_selector)));
    }
    if page.page_size != 0 {
        q.push(format!("page.page_size={}", page.page_size));
    }
    if !token.is_empty() {
        q.push(format!("page.page_token={token}"));
    }
    if q.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", q.join("&"))
    }
}

/// FNV-1a, 64 bits: an etag is an opaque version of what the caller can see,
/// stable across processes (a `std` hasher is not).
pub fn fnv(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_round_trips_and_a_foreign_one_is_refused() {
        let token = encode_token("default/lab-a");
        assert_eq!(decode_token(&token).as_deref(), Some("default/lab-a"));
        for bad in ["zz", "abc", "ff"] {
            let page = PageRequest {
                page_size: 0,
                page_token: bad.into(),
            };
            assert_eq!(
                after(&page).unwrap_err().code(),
                tonic::Code::InvalidArgument,
                "{bad}"
            );
        }
    }

    #[test]
    fn the_size_has_a_default_a_cap_and_no_negative() {
        let of = |n| {
            size(&PageRequest {
                page_size: n,
                page_token: String::new(),
            })
        };
        assert_eq!(of(0).unwrap(), DEFAULT_PAGE);
        assert_eq!(of(7).unwrap(), 7);
        assert_eq!(of(1_000_000).unwrap(), MAX_PAGE);
        assert!(of(-1).is_err());
    }

    #[test]
    fn a_link_keeps_the_callers_filter_escaped() {
        let page = PageRequest {
            page_size: 2,
            page_token: String::new(),
        };
        assert_eq!(
            href("/v1/x", "app=web,tier!=db", &page, "6162"),
            "/v1/x?label_selector=app%3Dweb%2Ctier%21%3Ddb&page.page_size=2&page.page_token=6162"
        );
        assert_eq!(href("/v1/x", "", &PageRequest::default(), ""), "/v1/x");
    }
}
