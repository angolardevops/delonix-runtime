//! Each refusal of the ADR-0021 threat list has a test here that fails with the
//! refusal reverted. The ones that need a repository build one with the
//! system's `git`, in a temporary directory, over `file://` — no network.

use super::*;
use crate::cmd::reconcile::{Action, Change};

fn spec(yaml: &str) -> GitOpsSourceSpec {
    serde_yaml::from_str(yaml).unwrap()
}

fn base_spec() -> GitOpsSourceSpec {
    spec("source:\n  url: file:///srv/repo.git\n")
}

fn doc(yaml: &str) -> ManifestDoc {
    serde_yaml::from_str(yaml).unwrap()
}

fn number(e: &Error) -> u16 {
    e.number()
}

// --- the URL --------------------------------------------------------------

#[test]
fn only_three_transports_are_fetched() {
    assert_eq!(
        validate_url("file:///srv/repo.git").unwrap(),
        Transport::File(PathBuf::from("/srv/repo.git"))
    );
    assert_eq!(
        validate_url("https://git.example/org/repo.git").unwrap(),
        Transport::Https
    );
    assert_eq!(
        validate_url("ssh://git@git.example/org/repo.git").unwrap(),
        Transport::Ssh
    );
    for bad in [
        // git would read it as an option
        "-uhelp",
        "--upload-pack=touch /tmp/x",
        // a helper transport runs a command
        "ext::sh -c touch% /tmp/x",
        "fd::3",
        // no integrity
        "http://git.example/repo.git",
        "git://git.example/repo.git",
        // a bare path is not a URL, and a relative file:// is not a place
        "/srv/repo.git",
        "file://srv/repo.git",
        "file:///srv/../etc",
        // credentials in the URL
        "https://user:pass@git.example/repo.git",
        "https://token@git.example/repo.git",
        "ssh://git:pass@git.example/repo.git",
        // shapes that change meaning on the way through a shell or git
        "https://git.example/a b",
        "https://git.example/a\nb",
        "ssh://-oProxyCommand=x/repo",
        "",
    ] {
        let e = validate_url(bad).unwrap_err();
        assert_eq!(number(&e), 1601, "{bad:?} → {e}");
    }
}

// --- the ref and the path ---------------------------------------------------

#[test]
fn a_ref_is_a_branch_or_a_tag_and_nothing_a_shell_or_git_reads_otherwise() {
    for ok in [
        "refs/heads/main",
        "refs/tags/v1.2.3",
        "refs/heads/team/a_b-c",
    ] {
        validate_ref(ok).unwrap();
    }
    for bad in [
        "main",
        "HEAD",
        "refs/heads/../main",
        "refs/heads/a..b",
        "refs/heads/-x",
        "refs/heads/.hidden",
        "refs/heads/main@{1}",
        "refs/heads/main.lock",
        "refs/heads/a b",
        "refs/heads//main",
        "refs/heads/main/",
        "refs/remotes/origin/main",
        "refs/heads/$(touch)",
    ] {
        assert_eq!(number(&validate_ref(bad).unwrap_err()), 1601, "{bad}");
    }
}

#[test]
fn a_path_stays_inside_the_repository() {
    for ok in ["", "environments/production", "a/b-c/d_e", "prod/"] {
        validate_repo_path(ok).unwrap();
    }
    for bad in [
        "/etc", "../x", "a/../b", "a/./b", "-x", "a b", "a:b", "a//b",
    ] {
        assert_eq!(number(&validate_repo_path(bad).unwrap_err()), 1601, "{bad}");
    }
}

#[test]
fn an_interval_has_a_floor_and_a_ceiling() {
    assert_eq!(parse_interval("60s").unwrap(), 60);
    assert_eq!(parse_interval("5m").unwrap(), 300);
    assert_eq!(parse_interval("1h").unwrap(), 3600);
    assert_eq!(parse_interval("45").unwrap(), 45);
    for bad in ["1s", "14s", "25h", "0", "x", "-5m", "5d", ""] {
        assert!(parse_interval(bad).is_err(), "{bad}");
    }
}

// --- the registration -------------------------------------------------------

#[test]
fn an_ssh_source_needs_a_key_and_a_pinned_host() {
    let s = spec("source:\n  url: ssh://git@h/r.git\n  secretRef: deploy\n");
    assert_eq!(
        number(&validate_spec(&s, "default", "web").unwrap_err()),
        1601
    );
    let s =
        spec("source:\n  url: ssh://git@h/r.git\n  secretRef: deploy\n  knownHostsRef: hosts\n");
    validate_spec(&s, "default", "web").unwrap();
}

#[test]
fn a_credential_is_not_accepted_where_it_would_not_be_read() {
    let s = spec("source:\n  url: https://h/r.git\n  secretRef: token\n");
    assert_eq!(
        number(&validate_spec(&s, "default", "web").unwrap_err()),
        1601
    );
}

#[test]
fn signed_commits_and_trusted_keys_go_together() {
    let s = spec("source:\n  url: file:///r.git\n  signedCommitsRequired: true\n");
    assert!(validate_spec(&s, "default", "web").is_err());
    let s = spec("source:\n  url: file:///r.git\n  trustedKeysRef: signers\n");
    assert!(validate_spec(&s, "default", "web").is_err());
}

#[test]
fn the_safety_policy_speaks_the_reconcilers_own_names() {
    for bad in ["Delete", "Conflict", "NoOp", "HotUpdate", "create"] {
        let s = spec(&format!(
            "source:\n  url: file:///r.git\nsafety:\n  allow: [{bad}]\n"
        ));
        assert!(validate_spec(&s, "default", "web").is_err(), "{bad}");
    }
    let s = spec("source:\n  url: file:///r.git\nsafety:\n  allow: [Create, Update, Adopt]\n");
    validate_spec(&s, "default", "web").unwrap();
}

#[test]
fn safety_kinds_can_never_let_in_the_hard_refused_ones() {
    for bad in ["Secret", "RuntimePolicy", "App", "GitOpsSource", "Nope"] {
        let s = spec(&format!(
            "source:\n  url: file:///r.git\nsafety:\n  kinds: [{bad}]\n"
        ));
        assert!(validate_spec(&s, "default", "web").is_err(), "{bad}");
    }
}

#[test]
fn a_name_becomes_a_directory_and_a_unit_so_it_is_a_dns_label() {
    for bad in ["../x", "a.b", "A", "-a", "a-", "", "a b"] {
        assert!(
            validate_spec(&base_spec(), "default", bad).is_err(),
            "{bad}"
        );
        assert!(validate_spec(&base_spec(), bad, "web").is_err(), "{bad}");
    }
}

#[test]
fn a_spec_refuses_an_unknown_field_rather_than_ignoring_it() {
    // `selfHeal`/`wait` are named by the ADR and not built: accepting them
    // would be the accepted-and-ignored flag this engine refuses everywhere.
    assert!(serde_yaml::from_str::<GitOpsSourceSpec>(
        "source:\n  url: file:///r.git\nsync:\n  selfHeal: true\n"
    )
    .is_err());
    assert!(serde_yaml::from_str::<GitOpsSourceSpec>(
        "source:\n  url: file:///r.git\n  depth: 0\n"
    )
    .is_err());
}

// --- reading a commit -------------------------------------------------------

fn entry(mode: &str, kind: &str, size: Option<u64>, path: &str) -> TreeEntry {
    TreeEntry {
        mode: mode.into(),
        kind: kind.into(),
        oid: "a".repeat(40),
        size,
        path: path.into(),
    }
}

#[test]
fn ls_tree_output_is_parsed_with_its_padded_size() {
    let raw = format!(
        "100644 blob {}     123\tapp.yaml\0160000 commit {}       -\tsub\0",
        "a".repeat(40),
        "b".repeat(40)
    );
    let e = parse_ls_tree(raw.as_bytes()).unwrap();
    assert_eq!(e.len(), 2);
    assert_eq!(e[0].size, Some(123));
    assert_eq!(e[0].path, "app.yaml");
    assert_eq!(e[1].size, None);
}

#[test]
fn a_symlink_in_the_tree_refuses_the_commit() {
    let e = select_manifests(&[
        entry("100644", "blob", Some(10), "a.yaml"),
        entry("120000", "blob", Some(10), "b.yaml"),
    ])
    .unwrap_err();
    assert_eq!(number(&e), 1602, "{e}");
}

#[test]
fn a_symlink_that_is_not_yaml_still_refuses_the_commit() {
    // Not read is not enough: a link pointing out of the tree is a statement
    // of intent, and the next version that reads more files would follow it.
    let e = select_manifests(&[entry("120000", "blob", Some(10), "data/etc")]).unwrap_err();
    assert_eq!(number(&e), 1602);
}

#[test]
fn a_submodule_refuses_the_commit() {
    let e = select_manifests(&[entry("160000", "commit", None, "vendor/lib")]).unwrap_err();
    assert_eq!(number(&e), 1602);
}

#[test]
fn the_tree_is_bounded_per_file_in_count_and_in_total() {
    let big = select_manifests(&[entry("100644", "blob", Some(MAX_FILE_BYTES + 1), "a.yaml")]);
    assert_eq!(number(&big.unwrap_err()), 1603);
    let many: Vec<TreeEntry> = (0..=MAX_MANIFEST_FILES)
        .map(|i| entry("100644", "blob", Some(10), &format!("f{i}.yaml")))
        .collect();
    assert_eq!(number(&select_manifests(&many).unwrap_err()), 1603);
    let heavy: Vec<TreeEntry> = (0..9)
        .map(|i| {
            entry(
                "100644",
                "blob",
                Some(MAX_FILE_BYTES),
                &format!("f{i}.yaml"),
            )
        })
        .collect();
    assert_eq!(number(&select_manifests(&heavy).unwrap_err()), 1603);
    let listing: Vec<TreeEntry> = (0..=MAX_TREE_ENTRIES)
        .map(|i| entry("100644", "blob", Some(1), &format!("f{i}.txt")))
        .collect();
    assert_eq!(number(&select_manifests(&listing).unwrap_err()), 1603);
}

#[test]
fn hidden_and_non_yaml_files_are_not_manifests() {
    let got = select_manifests(&[
        entry("100644", "blob", Some(10), ".github/workflows/ci.yaml"),
        entry("100644", "blob", Some(10), "README.md"),
        entry("100755", "blob", Some(10), "b.yml"),
        entry("100644", "blob", Some(10), "a.yaml"),
    ])
    .unwrap();
    let paths: Vec<&str> = got.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["a.yaml", "b.yml"]);
}

#[test]
fn an_lfs_pointer_or_a_private_key_refuses_the_commit() {
    let e = scan_file(
        "a.yaml",
        "version https://git-lfs.github.com/spec/v1\noid sha256:00\nsize 1\n",
    )
    .unwrap_err();
    assert_eq!(number(&e), 1602);
    let e = scan_file(
        "a.yaml",
        "kind: Secret\nstringData:\n  k: |\n    -----BEGIN OPENSSH PRIVATE KEY-----\n    x\n",
    )
    .unwrap_err();
    assert_eq!(number(&e), 1602);
    scan_file("a.yaml", "kind: Volume\n").unwrap();
}

// --- the guard --------------------------------------------------------------

fn guard(docs_yaml: &[&str], s: &GitOpsSourceSpec) -> Vec<String> {
    let docs: Vec<ManifestDoc> = docs_yaml.iter().map(|y| doc(y)).collect();
    guard_documents(&docs, "default", "web", s)
}

const VOLUME: &str = "apiVersion: delonix.io/v1\nkind: Volume\nmetadata: {name: data}\nspec: {}\n";

#[test]
fn a_plain_volume_passes_the_guard() {
    assert!(guard(&[VOLUME], &base_spec()).is_empty());
}

#[test]
fn the_hard_refused_kinds_never_pass() {
    for kind in ["RuntimePolicy", "Secret", "App"] {
        let y =
            format!("apiVersion: delonix.io/v1\nkind: {kind}\nmetadata: {{name: x}}\nspec: {{}}\n");
        let mut s = base_spec();
        assert!(!guard(&[&y], &s).is_empty(), "{kind}");
        // Not even listed — `validate_spec` already refuses the listing, and
        // the guard does not trust that it ran.
        s.safety.kinds = vec![kind.to_string()];
        assert!(!guard(&[&y], &s).is_empty(), "{kind} with kinds");
    }
}

#[test]
fn an_opt_in_kind_passes_only_when_named() {
    let y =
        "apiVersion: delonix.io/v1\nkind: VirtualMachine\nmetadata: {name: vm}\nspec: {disk: x}\n";
    let mut s = base_spec();
    assert!(!guard(&[y], &s).is_empty());
    s.safety.kinds = vec!["VirtualMachine".into(), "Volume".into()];
    assert!(
        guard(&[y, VOLUME], &s).is_empty(),
        "{:?}",
        guard(&[y, VOLUME], &s)
    );
    // and the list NARROWS: a Kind not named is refused once there is a list
    s.safety.kinds = vec!["VirtualMachine".into()];
    assert!(!guard(&[VOLUME], &s).is_empty());
}

fn container(spec_yaml: &str) -> String {
    format!("apiVersion: delonix.io/v1\nkind: Container\nmetadata: {{name: c}}\nspec:\n{spec_yaml}")
}

#[test]
fn a_container_asking_for_the_host_is_refused() {
    for host in [
        "  image: alpine\n  privileged: true\n",
        "  image: alpine\n  network: host\n",
        "  image: alpine\n  volumes: [\"/etc:/x\"]\n",
        "  image: alpine\n  volumes: [\"./data:/x\"]\n",
        "  image: alpine\n  devices: [\"/dev/kvm\"]\n",
        "  image: alpine\n  capAdd: [NET_ADMIN]\n",
        "  image: alpine\n  envFile: [\"/etc/environment\"]\n",
        "  image: alpine\n  hostPid: true\n",
    ] {
        let y = container(host);
        assert!(!guard(&[&y], &base_spec()).is_empty(), "{host}");
    }
    let plain = container(
        "  image: alpine\n  network: appnet\n  volumes: [\"data:/x\"]\n  ports: [\"8080:80\"]\n",
    );
    assert!(
        guard(&[&plain], &base_spec()).is_empty(),
        "{:?}",
        guard(&[&plain], &base_spec())
    );
}

#[test]
fn a_pod_mounting_a_host_path_is_refused() {
    let y = "apiVersion: delonix.io/v1\nkind: Pod\nmetadata: {name: p}\nspec:\n  containers:\n  - name: a\n    image: alpine\n    volumeMounts: [{name: h, mountPath: /h}]\n  volumes:\n  - name: h\n    hostPath: {path: /}\n";
    assert!(!guard(&[y], &base_spec()).is_empty());
}

#[test]
fn an_image_build_and_a_network_volume_are_refused() {
    let build = "apiVersion: delonix.io/v1\nkind: Image\nmetadata: {name: i}\nspec:\n  build: {context: .}\n";
    assert!(!guard(&[build], &base_spec()).is_empty());
    let pull = "apiVersion: delonix.io/v1\nkind: Image\nmetadata: {name: alpine:3.20}\nspec:\n  pull: alpine:3.20\n";
    assert!(guard(&[pull], &base_spec()).is_empty());
    let nfs = "apiVersion: delonix.io/v1\nkind: Volume\nmetadata: {name: v}\nspec:\n  nfs: {server: 10.0.0.1, share: /x}\n";
    assert!(!guard(&[nfs], &base_spec()).is_empty());
    let overlay =
        "apiVersion: delonix.io/v1\nkind: Network\nmetadata: {name: n}\nspec:\n  driver: overlay\n";
    assert!(!guard(&[overlay], &base_spec()).is_empty());
}

#[test]
fn a_workload_never_consumes_the_sources_own_credential() {
    let s = spec(
        "source:\n  url: ssh://git@h/r.git\n  secretRef: deploy-key\n  knownHostsRef: hosts\n",
    );
    let y = container("  image: alpine\n  network: appnet\n  secret: [deploy-key]\n");
    assert!(!guard(&[&y], &s).is_empty());
    let other = container("  image: alpine\n  network: appnet\n  secret: [app-config]\n");
    assert!(guard(&[&other], &s).is_empty());
}

fn source_doc(name: &str, ns: &str, extra: &str) -> String {
    format!(
        "apiVersion: gitops.delonix.io/v1alpha1\nkind: GitOpsSource\nmetadata: {{name: {name}, namespace: {ns}}}\nspec:\n  source:\n    url: file:///srv/repo.git\n{extra}"
    )
}

#[test]
fn a_source_declares_only_itself() {
    let other = source_doc("other", "default", "");
    assert!(!guard(&[&other], &base_spec()).is_empty());
    let elsewhere = source_doc("web", "team-b", "");
    assert!(!guard(&[&elsewhere], &base_spec()).is_empty());
    let me = source_doc("web", "default", "");
    assert!(
        guard(&[&me], &base_spec()).is_empty(),
        "{:?}",
        guard(&[&me], &base_spec())
    );
}

#[test]
fn a_commit_may_move_its_own_ref_but_never_loosen_its_policy() {
    let moved = source_doc(
        "web",
        "default",
        "    ref: refs/heads/next\n    interval: 5m\n",
    );
    assert!(guard(&[&moved], &base_spec()).is_empty());
    for loosen in [
        "  safety:\n    allow: [Create, Update, Replace]\n",
        "  safety:\n    prune: true\n",
        "  sync:\n    automated: true\n",
    ] {
        let y = source_doc("web", "default", "") + loosen;
        let r = guard(&[&y], &base_spec());
        assert!(!r.is_empty(), "{loosen}");
    }
    let repointed = "apiVersion: gitops.delonix.io/v1alpha1\nkind: GitOpsSource\nmetadata: {name: web}\nspec:\n  source:\n    url: file:///srv/attacker.git\n";
    assert!(!guard(&[repointed], &base_spec()).is_empty());
}

#[test]
fn documents_land_in_the_sources_namespace_and_never_in_another() {
    let mut docs = vec![
        doc(&container("  image: alpine\n")),
        doc("apiVersion: delonix.io/v1\nkind: Container\nmetadata: {name: d, namespace: team-b}\nspec: {image: alpine}\n"),
        doc(VOLUME),
    ];
    let reasons = scope_documents(&mut docs, "team-a");
    assert_eq!(docs[0].metadata.namespace.as_deref(), Some("team-a"));
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(reasons[0].contains("team-b"));
    // A Kind with no namespace of its own is left alone.
    assert_eq!(docs[2].metadata.namespace, None);
}

// --- the safety policy over a plan ------------------------------------------

fn change(kind: &str, name: &str, action: Action) -> Change {
    Change {
        kind: kind.into(),
        name: name.into(),
        action,
        reason: None,
        cold_fields: Vec::new(),
        reboot_fields: Vec::new(),
        owner: None,
        conditions: Vec::new(),
        diffs: Vec::new(),
        changed: action.is_change(),
        plan_digest: None,
    }
}

#[test]
fn by_default_a_pass_creates_and_updates_and_nothing_else() {
    let d = decide(
        &[
            change("Volume", "a", Action::Create),
            change("Volume", "b", Action::Update),
            change("Volume", "c", Action::NoOp),
        ],
        &base_spec(),
    );
    assert!(d.blocked.is_empty(), "{:?}", d.blocked);
    assert!(d.changes);
    for (action, word) in [
        (Action::Adopt, "Adopt"),
        (Action::Replace, "Replace"),
        (Action::Reboot, "Reboot"),
    ] {
        let d = decide(&[change("Container", "x", action)], &base_spec());
        assert_eq!(d.blocked.len(), 1, "{word}");
        assert!(d.blocked[0].contains(word), "{:?}", d.blocked);
    }
    let d = decide(&[change("Container", "x", Action::Conflict)], &base_spec());
    assert_eq!(d.blocked.len(), 1);
}

#[test]
fn an_allowed_replace_is_named_for_the_apply() {
    let mut s = base_spec();
    s.safety.allow.push("Replace".into());
    let d = decide(&[change("Container", "x", Action::Replace)], &s);
    assert!(d.blocked.is_empty());
    assert_eq!(d.replace, ["Container/x"]);
}

#[test]
fn a_delete_runs_only_with_prune_below_the_ceiling_and_off_the_protected_list() {
    let gone = [
        change("Volume", "a", Action::Delete),
        change("Volume", "b", Action::Delete),
    ];
    let d = decide(&gone, &base_spec());
    assert!(d.prune.is_empty());
    assert_eq!(d.kept.len(), 2);
    assert!(!d.changes, "a delete that does not run is not a change");

    let mut s = base_spec();
    s.safety.prune = true;
    let d = decide(&gone, &s);
    assert_eq!(d.prune.len(), 2);

    s.safety.maximum_deletes = 1;
    let d = decide(&gone, &s);
    assert_eq!(d.blocked.len(), 1, "{:?}", d.blocked);

    s.safety.maximum_deletes = 5;
    s.safety.protected_resources = vec!["Volume/*".into()];
    let d = decide(&gone, &s);
    assert!(d.prune.is_empty());
    assert_eq!(d.kept.len(), 2);
}

#[test]
fn a_pass_never_prunes_a_source_least_of_all_itself() {
    let mut s = base_spec();
    s.safety.prune = true;
    let d = decide(&[change("GitOpsSource", "web", Action::Delete)], &s);
    assert!(d.prune.is_empty());
    assert_eq!(d.kept.len(), 1);
}

// --- the timer --------------------------------------------------------------

#[test]
fn the_units_are_prefixed_and_carry_absolute_paths_only() {
    let svc = service_unit(
        Path::new("/usr/bin/delonix"),
        Path::new("/var/lib/delonix 100%"),
        None,
        "prod",
        "web",
    )
    .unwrap();
    assert!(svc.contains("Type=oneshot"), "{svc}");
    assert!(svc.contains("ExecStart=\"/usr/bin/delonix\" gitops reconcile web --namespace prod"));
    assert!(svc.contains("DELONIX_ROOT=/var/lib/delonix 100%%"), "{svc}");
    assert!(service_unit(Path::new("delonix"), Path::new("/r"), None, "a", "b").is_err());
    assert!(service_unit(Path::new("/d"), Path::new("/r\"x"), None, "a", "b").is_err());
    let t = timer_unit("prod", "web", 60);
    assert!(t.contains("OnUnitInactiveSec=60s"), "{t}");
    assert!(t.contains("Unit=delonix-gitops-prod.web.service"), "{t}");
    assert_eq!(unit_base("prod", "web"), "delonix-gitops-prod.web");
}

// --- against a real repository ------------------------------------------------

/// Runs the system `git` for the TEST's own setup, with nothing of the
/// caller's configuration (a hook or a signing default would change what is
/// being tested).
fn sh_git(home: &Path, cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git is installed");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Repo {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    bare: PathBuf,
    state: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let work = tmp.path().join("work");
        let bare = tmp.path().join("repo.git");
        let state = tmp.path().join("state");
        for d in [&home, &work, &state] {
            std::fs::create_dir_all(d).unwrap();
        }
        sh_git(
            &home,
            tmp.path(),
            &["init", "--quiet", "--bare", bare.to_str().unwrap()],
        );
        sh_git(&home, &work, &["init", "--quiet", "-b", "main"]);
        Repo {
            _tmp: tmp,
            home,
            work,
            bare,
            state,
        }
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.work.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn commit(&self, extra: &[&str]) -> String {
        sh_git(&self.home, &self.work, &["add", "-A"]);
        let mut args = vec!["commit", "--quiet", "--allow-empty", "-m", "c"];
        args.extend_from_slice(extra);
        sh_git(&self.home, &self.work, &args);
        sh_git(
            &self.home,
            &self.work,
            &[
                "push",
                "--quiet",
                "--force",
                self.bare.to_str().unwrap(),
                "main",
            ],
        );
        sh_git(&self.home, &self.work, &["rev-parse", "HEAD"])
    }

    fn record(&self, extra: &str) -> SourceRecord {
        let s = spec(&format!(
            "source:\n  url: file://{}\n  path: env/prod\n{extra}",
            self.bare.display()
        ));
        SourceRecord {
            namespace: "default".into(),
            name: "web".into(),
            spec: s,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        }
    }

    fn dir(&self) -> PathBuf {
        let d = self.state.join("gitops/default/web");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The pass up to (not including) the plan: fetch, resolve, verify, read.
    fn read(&self, rec: &SourceRecord) -> Result<(String, String)> {
        let dir = self.dir();
        let (git, _files) = prepare_git(&self.state, &dir, rec)?;
        let commit = fetch_and_resolve(&self.state, &git, &dir, rec)?;
        let path = read_commit(&git, &dir, &commit, &rec.spec)?;
        Ok((commit, std::fs::read_to_string(path).unwrap()))
    }
}

#[test]
fn a_commit_is_read_from_the_tree_without_a_checkout() {
    let r = Repo::new();
    r.write("env/prod/vol.yaml", VOLUME);
    r.write("env/prod/README.md", "not a manifest\n");
    r.write("env/staging/x.yaml", "kind: Nope\n");
    let sha = r.commit(&[]);
    let (commit, text) = r.read(&r.record("")).unwrap();
    assert_eq!(commit, sha);
    assert!(text.contains("kind: Volume"), "{text}");
    assert!(!text.contains("Nope"), "a sibling path leaked in");
    assert!(!text.contains("not a manifest"));
    // Nothing was checked out next to the mirror.
    let dir = r.dir();
    assert!(!dir.join("mirror.git/env").exists());
}

#[test]
fn a_symlink_committed_under_the_path_refuses_the_commit() {
    let r = Repo::new();
    r.write("env/prod/vol.yaml", VOLUME);
    std::os::unix::fs::symlink("/etc/passwd", r.work.join("env/prod/link.yaml")).unwrap();
    r.commit(&[]);
    let e = r.read(&r.record("")).unwrap_err();
    assert_eq!(number(&e), 1602, "{e}");
}

#[test]
fn a_submodule_committed_under_the_path_refuses_the_commit() {
    let r = Repo::new();
    r.write("env/prod/vol.yaml", VOLUME);
    r.commit(&[]);
    let sha = sh_git(&r.home, &r.work, &["rev-parse", "HEAD"]);
    sh_git(
        &r.home,
        &r.work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sha},env/prod/vendor"),
        ],
    );
    sh_git(&r.home, &r.work, &["commit", "--quiet", "-m", "sub"]);
    sh_git(
        &r.home,
        &r.work,
        &[
            "push",
            "--quiet",
            "--force",
            r.bare.to_str().unwrap(),
            "main",
        ],
    );
    let e = r.read(&r.record("")).unwrap_err();
    assert_eq!(number(&e), 1602, "{e}");
}

#[test]
fn an_empty_path_is_never_applied() {
    let r = Repo::new();
    r.write("env/prod/README.md", "nothing here\n");
    r.commit(&[]);
    let e = r.read(&r.record("")).unwrap_err();
    assert_eq!(number(&e), 1602, "{e}");
}

#[test]
fn an_unreachable_remote_fails_closed_naming_the_url() {
    let r = Repo::new();
    let mut rec = r.record("");
    rec.spec.source.url = format!("file://{}/missing.git", r.state.display());
    let e = r.read(&rec).unwrap_err();
    assert_eq!(number(&e), 6602, "{e}");
    assert!(e.to_string().contains("missing.git"), "{e}");
}

/// A key pair for signing, made with `ssh-keygen`.
fn keypair(dir: &Path, name: &str) -> (PathBuf, String) {
    let key = dir.join(name);
    let st = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen is installed");
    assert!(st.success());
    let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
    (key, public.trim().to_string())
}

fn store_secret(state: &Path, name: &str, key: &str, value: &str) {
    let store = delonix_state::SecretStore::open(state).unwrap();
    let mut data = BTreeMap::new();
    data.insert(key.to_string(), value.to_string());
    store
        .save(&delonix_model::secret::Secret {
            name: name.into(),
            data,
            ..Default::default()
        })
        .unwrap();
}

#[test]
fn signed_commits_are_verified_against_the_trusted_keys_only() {
    let r = Repo::new();
    let (trusted, trusted_pub) = keypair(&r.home, "trusted");
    let (_stranger, stranger_pub) = keypair(&r.home, "stranger");
    let _ = stranger_pub;
    store_secret(
        &r.state,
        "signers",
        "allowed_signers",
        &format!("t@example.invalid {trusted_pub}"),
    );
    let rec = r.record("  signedCommitsRequired: true\n  trustedKeysRef: signers\n");

    // Unsigned: refused before anything is read.
    r.write("env/prod/vol.yaml", VOLUME);
    r.commit(&[]);
    assert_eq!(number(&r.read(&rec).unwrap_err()), 7601);

    // Signed by a key the node trusts: read.
    r.write("env/prod/vol.yaml", &(VOLUME.to_string() + "# v2\n"));
    let sign = |key: &Path| {
        vec![
            "-c".to_string(),
            "gpg.format=ssh".to_string(),
            "-c".to_string(),
            format!("user.signingkey={}", key.display()),
        ]
    };
    let signed_commit = |key: &Path| {
        sh_git(&r.home, &r.work, &["add", "-A"]);
        let mut args: Vec<String> = sign(key);
        args.extend(["commit", "--quiet", "-S", "--allow-empty", "-m", "s"].map(String::from));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        sh_git(&r.home, &r.work, &refs);
        sh_git(
            &r.home,
            &r.work,
            &[
                "push",
                "--quiet",
                "--force",
                r.bare.to_str().unwrap(),
                "main",
            ],
        );
    };
    signed_commit(&trusted);
    let (_, text) = r.read(&rec).unwrap();
    assert!(text.contains("# v2"));

    // Signed, but by a stranger: refused.
    let stranger_key = r.home.join("stranger");
    signed_commit(&stranger_key);
    assert_eq!(number(&r.read(&rec).unwrap_err()), 7601);
}

#[test]
fn the_ssh_key_is_written_private_and_removed_with_the_pass() {
    let r = Repo::new();
    store_secret(&r.state, "deploy", "identity", "KEY MATERIAL");
    store_secret(&r.state, "hosts", "known_hosts", "h ssh-ed25519 AAAA");
    let mut rec = r.record("");
    rec.spec.source.url = "ssh://git@git.example/repo.git".into();
    rec.spec.source.secret_ref = Some("deploy".into());
    rec.spec.source.known_hosts_ref = Some("hosts".into());
    let dir = r.dir();
    let key = dir.join("home/identity");
    {
        let (_git, _files) = prepare_git(&r.state, &dir, &rec).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert!(!key.exists(), "the key outlived its pass");
}
