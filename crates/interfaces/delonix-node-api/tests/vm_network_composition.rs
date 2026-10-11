//! The node API registers the node's VM network before it serves (plan 68
//! item N3). Its own test binary, so nothing else in the process registers
//! a network first: the only registration this process can see is the one
//! `serve_blocking` made.
//!
//! Nothing here creates a VM or touches the dataplane: registering the port
//! is a struct in a `OnceLock`, and the test only asks the registry whether
//! it is there — the question the Cloud Hypervisor backend asks before it
//! attaches a guest to a network.

/// A SHORT socket path (`sun_path` is 108 bytes), removed on every exit.
fn short_sock() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir_in("/tmp").expect("a directory under /tmp");
    let sock = dir.path().join("x.sock").to_str().unwrap().to_owned();
    (dir, sock)
}

#[test]
fn serving_registers_the_vm_network_a_networked_create_needs() {
    // Before the server runs, nothing in this process registered one: the
    // assertion below can only pass because of what serving did.
    assert!(
        delonix_compute::vm_registry::network().is_err(),
        "a VM network was registered before the server ran; this test proves nothing"
    );
    let (_sock_dir, sock) = short_sock();
    let s = sock.clone();
    std::thread::spawn(move || {
        let _ = delonix_node_api::serve_blocking(&format!("unix://{s}"));
    });
    for _ in 0..200 {
        if std::path::Path::new(&sock).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        std::path::Path::new(&sock).exists(),
        "the server never bound"
    );
    if let Err(e) = delonix_compute::vm_registry::network() {
        panic!("the node API serves without a VM network: {e}");
    }
}
