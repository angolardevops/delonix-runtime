//! The REST encoding of the contract, from its `google.api.http` annotations
//! (ADR-0042 step C).
//!
//! `build.rs` reads the descriptor set and writes the route table and one
//! dispatcher per service ([`ROUTES`], `dispatch_<service>`); this module is
//! the part that is the same for every RPC: matching a request against a path
//! template, building the request message from the path, the query string and
//! the body, and calling the service method the gRPC encoding calls.
//!
//! The rules are the ones of `google/api/http.proto`:
//!
//! - a `{field}` in the template binds that request field, and it wins over
//!   nothing: a body that names the same field with another value is refused;
//! - `body: "*"` — the body is the request; no query parameter is accepted;
//! - `body: "<field>"` — the body is that field; the rest may come by query;
//! - no `body` — every other field may come by query, and a body is refused.
//!
//! A query parameter the request message does not have is an error, never
//! dropped: an option the caller wrote and the server ignored is the failure
//! this engine refuses everywhere else.

// A `tonic::Status` is large by nature, and it is the error every service
// method already returns (the CRI crate silences this for the same reason).
#![allow(clippy::result_large_err)]

use std::future::Future;

use serde_json::{Map, Value};
use tonic::{Request, Response, Status};

include!(concat!(env!("OUT_DIR"), "/rest_routes.rs"));

/// What a query-string value becomes in the request's JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A string, bytes or enum field: the text as written.
    Text,
    /// `true` or `false`; anything else is refused.
    Bool,
    /// A number; the text has to parse as one.
    Number,
}

/// One RPC's REST mapping, as the contract declares it.
#[derive(Debug)]
pub struct Route {
    pub service: &'static str,
    pub rpc: &'static str,
    pub method: &'static str,
    pub template: &'static str,
    pub body: &'static str,
    pub server_streaming: bool,
    /// The request fields a query string may name: dotted path, kind, repeated.
    pub fields: &'static [(&'static str, Kind, bool)],
}

/// What a method and a path resolve to.
#[derive(Debug)]
pub enum Resolved {
    /// The route, and the value of each `{variable}` of its template.
    Route(&'static Route, Vec<(&'static str, String)>),
    /// The path is in the contract under other methods (for `Allow`).
    OtherMethods(Vec<&'static str>),
    /// The contract has no such path.
    Unknown,
}

/// The contract's route for `method path`.
pub fn resolve(method: &str, path: &str) -> Resolved {
    let mut others: Vec<&'static str> = Vec::new();
    for route in ROUTES {
        let Some(vars) = match_template(route.template, path) else {
            continue;
        };
        if route.method == method {
            return Resolved::Route(route, vars);
        }
        if !others.contains(&route.method) {
            others.push(route.method);
        }
    }
    if others.is_empty() {
        Resolved::Unknown
    } else {
        others.sort_unstable();
        Resolved::OtherMethods(others)
    }
}

/// `path` against one template; the variables' values on a match.
///
/// A segment is a literal (`images:pull` included), `{field}`, or
/// `{field}:verb`. A plain `{field}` does not take a value with a `:` in it:
/// `/v1/operations/x:cancel` is the `:cancel` route of `x`, never the
/// operation named `x:cancel` — and no name this engine accepts has a colon.
pub fn match_template(template: &'static str, path: &str) -> Option<Vec<(&'static str, String)>> {
    let mut vars = Vec::new();
    let mut want = template.split('/');
    let mut have = path.split('/');
    loop {
        match (want.next(), have.next()) {
            (None, None) => return Some(vars),
            (Some(w), Some(h)) => {
                let Some(rest) = w.strip_prefix('{') else {
                    if w != h {
                        return None;
                    }
                    continue;
                };
                let (name, verb) = rest.split_once('}')?;
                let value = if verb.is_empty() {
                    if h.contains(':') {
                        return None;
                    }
                    h
                } else {
                    h.strip_suffix(verb)?
                };
                if value.is_empty() {
                    return None;
                }
                vars.push((name, percent_decode(value)?));
            }
            _ => return None,
        }
    }
}

/// `%41` → `A`. `None` for a truncated escape or bytes that are not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The request message as JSON, from the three places REST spreads it over.
pub fn bind(
    route: &Route,
    vars: &[(&'static str, String)],
    query: Option<&str>,
    body: &[u8],
) -> Result<Value, Status> {
    let refuse = |why: String| Status::invalid_argument(format!("{}: {why}", route.rpc));
    let has_body = body.iter().any(|b| !b.is_ascii_whitespace());
    let parsed = || {
        serde_json::from_slice::<Value>(body)
            .map_err(|e| refuse(format!("the request body is not JSON: {e}")))
    };
    let mut msg = Map::new();
    match route.body {
        "" => {
            if has_body {
                return Err(refuse(format!(
                    "{} {} takes no request body",
                    route.method, route.template
                )));
            }
        }
        "*" => {
            if has_body {
                match parsed()? {
                    Value::Object(o) => msg = o,
                    _ => return Err(refuse("the request body has to be a JSON object".into())),
                }
            }
        }
        field => {
            if has_body {
                msg.insert(field.to_string(), parsed()?);
            }
        }
    }
    for (key, value) in query_pairs(query).map_err(&refuse)? {
        if route.body == "*" {
            return Err(refuse(format!(
                "query parameter '{key}' is not accepted: the request is the body"
            )));
        }
        let Some((path, kind, repeated)) = route.fields.iter().find(|(p, _, _)| *p == key) else {
            return Err(refuse(format!("unknown query parameter '{key}'")));
        };
        if vars.iter().any(|(v, _)| v == path) {
            return Err(refuse(format!(
                "'{key}' is part of the path and cannot also be a query parameter"
            )));
        }
        let json = match kind {
            Kind::Text => Value::String(value),
            Kind::Bool => match value.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => {
                    return Err(refuse(format!(
                        "'{key}' has to be true or false, got '{value}'"
                    )))
                }
            },
            Kind::Number => {
                if value.parse::<f64>().is_err() {
                    return Err(refuse(format!("'{key}' has to be a number, got '{value}'")));
                }
                // As text: proto3 JSON reads a number from a string, and a
                // 64-bit value survives that where a JSON number would not.
                Value::String(value)
            }
        };
        let slot = slot(&mut msg, path).map_err(&refuse)?;
        match (repeated, slot) {
            (true, Value::Array(items)) => items.push(json),
            (true, empty @ Value::Null) => *empty = Value::Array(vec![json]),
            (false, empty @ Value::Null) => *empty = json,
            _ => return Err(refuse(format!("'{key}' is given more than once"))),
        }
    }
    for (path, value) in vars {
        let slot = slot(&mut msg, path).map_err(&refuse)?;
        match slot {
            Value::Null => *slot = Value::String(value.clone()),
            Value::String(same) if same == value => {}
            _ => {
                return Err(refuse(format!(
                    "'{path}' in the body disagrees with the path ('{value}')"
                )))
            }
        }
    }
    Ok(Value::Object(msg))
}

/// The JSON value at a dotted field path, creating the objects on the way.
fn slot<'a>(msg: &'a mut Map<String, Value>, path: &str) -> Result<&'a mut Value, String> {
    let mut at = msg;
    let mut parts = path.split('.').peekable();
    loop {
        let part = parts.next().unwrap_or_default();
        let entry = at.entry(part.to_string()).or_insert(Value::Null);
        if parts.peek().is_none() {
            return Ok(entry);
        }
        if entry.is_null() {
            *entry = Value::Object(Map::new());
        }
        at = entry
            .as_object_mut()
            .ok_or_else(|| format!("'{path}' is not a field of an object"))?;
    }
}

/// `a=1&b=x%20y` as pairs, percent-decoded, `+` read as a space.
fn query_pairs(query: Option<&str>) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let decode = |s: &str| {
            percent_decode(&s.replace('+', " "))
                .ok_or_else(|| format!("the query string has a malformed escape in '{pair}'"))
        };
        out.push((decode(k)?, decode(v)?));
    }
    Ok(out)
}

/// One unary RPC: the bound JSON into the request message, the service method,
/// the response message back to JSON.
pub async fn unary<Req, Res, F, Fut>(input: Value, call: F) -> Result<Value, Status>
where
    Req: serde::de::DeserializeOwned,
    Res: serde::Serialize,
    F: FnOnce(Request<Req>) -> Fut,
    Fut: Future<Output = Result<Response<Res>, Status>>,
{
    let req: Req = serde_json::from_value(input).map_err(|e| {
        Status::invalid_argument(format!("the request does not match the contract: {e}"))
    })?;
    let res = call(Request::new(req)).await?.into_inner();
    serde_json::to_value(res).map_err(|e| Status::internal(format!("encoding the answer: {e}")))
}

/// A server-streaming RPC has no REST answer yet: the stream is what the gRPC
/// encoding carries.
pub fn streams_over_grpc_only(service: &str, rpc: &str) -> Status {
    Status::unimplemented(format!(
        "{service}.{rpc} is a stream, and the REST encoding does not carry streams yet — call it over gRPC on this socket"
    ))
}

/// The answer for a route of a service this engine does not serve yet.
pub fn not_served(route: &Route) -> Status {
    Status::unimplemented(format!(
        "{}.{} is in the contract and this engine does not serve it yet",
        route.service, route.rpc
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(rpc: &str) -> &'static Route {
        ROUTES
            .iter()
            .find(|r| r.rpc == rpc)
            .unwrap_or_else(|| panic!("no route for {rpc}"))
    }

    #[test]
    fn a_template_matches_literals_variables_and_verbs() {
        assert_eq!(match_template("/v1", "/v1"), Some(vec![]));
        assert_eq!(match_template("/v1", "/v1/"), None);
        assert_eq!(
            match_template("/v1/images:pull", "/v1/images:pull"),
            Some(vec![])
        );
        assert_eq!(match_template("/v1/images:pull", "/v1/images"), None);
        let t = "/v1/namespaces/{namespace}/containers/{name}:start";
        assert_eq!(
            match_template(t, "/v1/namespaces/prod/containers/web:start"),
            Some(vec![("namespace", "prod".into()), ("name", "web".into())])
        );
        assert_eq!(
            match_template(t, "/v1/namespaces/prod/containers/web"),
            None
        );
        assert_eq!(
            match_template(t, "/v1/namespaces/prod/containers/:start"),
            None
        );
        // A plain variable never swallows another route's verb.
        let plain = "/v1/namespaces/{namespace}/containers/{name}";
        assert_eq!(
            match_template(plain, "/v1/namespaces/prod/containers/web:start"),
            None
        );
        assert_eq!(
            match_template(plain, "/v1/namespaces/a%20b/containers/web"),
            Some(vec![("namespace", "a b".into()), ("name", "web".into())])
        );
        assert_eq!(
            match_template(plain, "/v1/namespaces/%zz/containers/web"),
            None
        );
        assert_eq!(
            match_template(plain, "/v1/namespaces//containers/web"),
            None
        );
    }

    #[test]
    fn a_path_under_other_methods_names_them() {
        match resolve("DELETE", "/v1/providers") {
            Resolved::OtherMethods(m) => assert_eq!(m, ["GET"]),
            other => panic!("{other:?}"),
        }
        match resolve("GET", "/v1/namespaces/a/containers/b:start") {
            Resolved::OtherMethods(m) => assert_eq!(m, ["POST"]),
            other => panic!("{other:?}"),
        }
        assert!(matches!(resolve("GET", "/v1/nothing"), Resolved::Unknown));
        assert!(matches!(resolve("GET", "/v1"), Resolved::Route(r, _) if r.rpc == "GetApiRoot"));
    }

    #[test]
    fn the_query_binds_only_what_the_request_has() {
        let r = route("ListProviders");
        assert_eq!(
            bind(r, &[], Some("kind=network"), b"").unwrap(),
            serde_json::json!({"kind": "network"})
        );
        assert_eq!(bind(r, &[], None, b"").unwrap(), serde_json::json!({}));
        let e = bind(r, &[], Some("kindd=network"), b"").unwrap_err();
        assert!(
            e.message().contains("unknown query parameter 'kindd'"),
            "{e}"
        );
        let e = bind(r, &[], Some("kind=a&kind=b"), b"").unwrap_err();
        assert!(e.message().contains("more than once"), "{e}");
        let e = bind(r, &[], None, b"{}").unwrap_err();
        assert!(e.message().contains("takes no request body"), "{e}");
    }

    #[test]
    fn the_path_binds_its_variables_and_a_disagreeing_body_is_refused() {
        let r = route("StartContainer");
        let vars = [
            ("namespace", "prod".to_string()),
            ("name", "web".to_string()),
        ];
        let v = bind(r, &vars, None, b"").unwrap();
        assert_eq!(v["namespace"], "prod");
        assert_eq!(v["name"], "web");
        let v = bind(r, &vars, None, br#"{"name": "web"}"#).unwrap();
        assert_eq!(v["name"], "web");
        let e = bind(r, &vars, None, br#"{"name": "db"}"#).unwrap_err();
        assert!(e.message().contains("disagrees with the path"), "{e}");
        let e = bind(r, &vars, Some("x=1"), b"").unwrap_err();
        assert!(e.message().contains("the request is the body"), "{e}");
        let e = bind(r, &vars, None, b"[1]").unwrap_err();
        assert!(e.message().contains("JSON object"), "{e}");
    }

    #[test]
    fn typed_query_values_are_checked_and_decode_into_the_request() {
        // Every bindable field of every route: a value of its kind binds, and
        // the wrong kind is refused — over the generated table, so a new
        // request field is covered the day it is added.
        let mut bools = 0;
        let mut numbers = 0;
        for r in ROUTES.iter().filter(|r| r.body != "*") {
            for (path, kind, _) in r.fields {
                let (good, bad) = match kind {
                    Kind::Text => continue,
                    Kind::Bool => {
                        bools += 1;
                        ("true", "yes")
                    }
                    Kind::Number => {
                        numbers += 1;
                        ("7", "seven")
                    }
                };
                let v = bind(r, &[], Some(&format!("{path}={good}")), b"")
                    .unwrap_or_else(|e| panic!("{} {path}: {e}", r.rpc));
                assert!(v.pointer(&format!("/{}", path.replace('.', "/"))).is_some());
                assert!(bind(r, &[], Some(&format!("{path}={bad}")), b"").is_err());
            }
        }
        assert!(bools + numbers > 0, "the contract has typed query fields");
    }
}
