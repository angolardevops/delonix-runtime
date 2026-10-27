//! `PodService`, the read half: `ListPods` and `GetPod`.
//!
//! A pod has no store of its own: it is a group of containers that share a
//! netns, each carrying the pod's name on its record (`container.pod`). The
//! read half DERIVES pods from the container records — grouping the members by
//! that name, and reusing `containers::message` for each member (the same
//! `Container` the ContainerService reports). The pod is `Ready` when every
//! member is running.
//!
//! The write half (`CreatePod` from a `kind: Pod` manifest, and a delete that
//! removes every member) is still in the contract and not served here yet —
//! `delonix pod create` / removing the members does it.

use std::collections::BTreeMap;
use std::path::Path;

use tonic::Status;

use crate::containers::{message, records};
use crate::proto::v1::{
    Condition, ConditionStatus, Container, ContainerPhase, GetPodRequest, ListPodsRequest,
    ListPodsResponse, NamedContainer, PageResponse, Pod, PodSpec, ResourceMeta,
};
use crate::selector::Selector;

/// Container record (same alias the ContainerService reads with).
type Record = delonix_compute::Container;

fn ns_wanted(namespace: &str) -> &str {
    if namespace.is_empty() {
        "default"
    } else {
        namespace
    }
}

/// A Pod built from its member records (all sharing the pod name). `members` is
/// the full `Container` of each; `spec.containers` the name+spec pair.
fn pod_message(name: &str, namespace: &str, members: &[&Record]) -> Pod {
    let containers: Vec<Container> = members.iter().map(|m| message(m)).collect();
    let all_running = containers.iter().all(|c| {
        c.status
            .as_ref()
            .is_some_and(|s| s.phase == ContainerPhase::Running as i32)
    });
    let named: Vec<NamedContainer> = containers
        .iter()
        .map(|c| NamedContainer {
            name: c.meta.as_ref().map(|m| m.name.clone()).unwrap_or_default(),
            spec: c.spec.clone(),
        })
        .collect();
    let ready = if all_running {
        Condition {
            r#type: "Ready".into(),
            status: ConditionStatus::True as i32,
            reason: "AllMembersRunning".into(),
            message: "every member of the pod is running".into(),
            last_transition_time: None,
        }
    } else {
        Condition {
            r#type: "Ready".into(),
            status: ConditionStatus::False as i32,
            reason: "MemberNotRunning".into(),
            message: "at least one member of the pod is not running".into(),
            last_transition_time: None,
        }
    };
    Pod {
        meta: Some(ResourceMeta {
            name: name.to_string(),
            namespace: namespace.to_string(),
            ..Default::default()
        }),
        spec: Some(PodSpec {
            containers: named,
            ..Default::default()
        }),
        members: containers,
        conditions: vec![ready],
    }
}

/// Groups the records that belong to a pod (non-empty `pod` name) by that name,
/// in name order, after the namespace and label filters.
fn grouped<'a>(
    records: &'a [Record],
    ns: &str,
    selector: &Selector,
) -> BTreeMap<String, Vec<&'a Record>> {
    let mut by_pod: BTreeMap<String, Vec<&Record>> = BTreeMap::new();
    for c in records {
        let pod = c.pod.clone().unwrap_or_default();
        if pod.is_empty() {
            continue;
        }
        if ns != "*" && c.namespace != ns {
            continue;
        }
        if !selector.matches(&c.labels) {
            continue;
        }
        by_pod.entry(pod).or_default().push(c);
    }
    by_pod
}

/// `ListPods` — every pod of the namespace, derived from the container records.
pub fn list_in(root: &Path, req: &ListPodsRequest) -> Result<ListPodsResponse, Status> {
    let selector = Selector::parse(&req.label_selector).map_err(Status::invalid_argument)?;
    let page = req.page.clone().unwrap_or_default();
    let size = crate::page::size(&page)?;
    let after = crate::page::after(&page)?;
    let ns = ns_wanted(&req.namespace);

    let all = records(root)?;
    let by_pod = grouped(&all, ns, &selector);
    let mut names: Vec<&String> = by_pod
        .keys()
        .filter(|n| after.as_ref().is_none_or(|a| n.as_str() > a.as_str()))
        .collect();
    names.sort();

    let shown: Vec<&String> = names.iter().take(size).copied().collect();
    let next = match (names.get(size), shown.last()) {
        (Some(_), Some(last)) => crate::page::encode_token(last),
        _ => String::new(),
    };
    let pods = shown
        .into_iter()
        .map(|name| {
            let members = &by_pod[name];
            let ns = members
                .first()
                .map(|m| m.namespace.as_str())
                .unwrap_or("default");
            pod_message(name, ns, members)
        })
        .collect();

    Ok(ListPodsResponse {
        pods,
        page: Some(PageResponse {
            next_page_token: next,
        }),
    })
}

/// `GetPod` — one pod by name, derived from its member records.
pub fn get_in(root: &Path, req: &GetPodRequest) -> Result<Pod, Status> {
    if req.name.is_empty() {
        return Err(Status::invalid_argument("GetPod: name is required"));
    }
    let ns = ns_wanted(&req.namespace);
    let all = records(root)?;
    let members: Vec<&Record> = all
        .iter()
        .filter(|c| c.pod.as_deref() == Some(req.name.as_str()))
        .filter(|c| ns == "*" || c.namespace == ns)
        .collect();
    if members.is_empty() {
        return Err(Status::not_found(format!(
            "pod '{}' in namespace '{ns}'",
            req.name
        )));
    }
    let pod_ns = members
        .first()
        .map(|m| m.namespace.as_str())
        .unwrap_or("default");
    Ok(pod_message(&req.name, pod_ns, &members))
}
