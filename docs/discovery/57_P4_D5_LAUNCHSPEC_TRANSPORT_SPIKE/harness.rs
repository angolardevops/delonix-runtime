// D5 IPC-transport spike (ADR-0044) — standalone, no delonix crates involved.
// Measures the file-based `__apirun` precedent (0700 dir + `create_new`+0600,
// unique name, unlink on both sides — copied verbatim from
// `bins/delonix-runtime-bin/src/cmd/dockerapi.rs`/`delonix-cri`'s
// `write_run_spec`) against an inherited-fd transport (`memfd_create`, no
// `MFD_CLOEXEC`, fd number passed as an argv string, inherited across a real
// `execve` into a *different* binary — not just a `fork()` within one
// process).
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

fn self_exe() -> PathBuf {
    env::current_exe().expect("current_exe")
}

fn unique_name() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        // a per-call counter would be nicer; nanos are enough for this spike
        rand_u32()
    )
}

fn rand_u32() -> u32 {
    // no `rand` crate on purpose (keep this spike dependency-free besides libc) —
    // ASLR-seeded stack address is good enough entropy for a unique filename
    let x = &0u8 as *const u8 as usize;
    (x as u32) ^ (std::process::id())
}

// ---------- file-based transport (the existing precedent) ----------

fn write_file(dir: &Path, payload: &[u8], die_after_write: bool) {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .expect("mkdir 0700");
    let path = dir.join(unique_name());
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("create_new 0600");
        f.write_all(payload).expect("write");
    }
    println!("WROTE {}", path.display());
    if die_after_write {
        // Simulates the writer process crashing right after the write, before
        // it ever spawns a reader and before its own defensive unlink runs —
        // the exact residual window this spike exists to measure.
        std::process::exit(0);
    }
    let exe = self_exe();
    let out = Command::new(&exe)
        .arg("read-file")
        .arg(&path)
        .output()
        .expect("spawn reader");
    assert!(out.status.success(), "reader failed: {:?}", out.status);
    // Defensive cleanup, same as the real precedent: "so a child that died
    // before reading does not leave the spec behind".
    let _ = fs::remove_file(&path);
    verify(payload, &out.stdout, "FILE");
}

fn read_file(path: &Path) {
    let data = fs::read(path).expect("read spec file");
    let sum = checksum(&data);
    println!("READ-FILE bytes={} sum={sum:08x}", data.len());
    let _ = fs::remove_file(path);
}

// ---------- fd-based transport (memfd, inherited across execve) ----------

fn write_fd(payload: &[u8], die_after_write: bool) {
    // memfd_create WITHOUT MFD_CLOEXEC: the fd survives execve by construction,
    // no dup2-to-a-fixed-number dance needed (its number is just passed as an
    // argv string). Nothing here ever calls open()/openat() on a real path.
    let name = std::ffi::CString::new("delonix-launchspec").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    assert!(fd >= 0, "memfd_create failed: {}", std::io::Error::last_os_error());
    let mut f = unsafe { fs::File::from_raw_fd(fd) };
    f.write_all(payload).expect("write memfd");
    // Rewind: the child inherits the SAME open-file-description (shared via
    // fork), including the write cursor — without this the child would read
    // from EOF and get nothing.
    use std::io::{Seek, SeekFrom};
    f.seek(SeekFrom::Start(0)).expect("seek");
    println!("WROTE fd={fd} (memfd, no path)");
    if die_after_write {
        std::process::exit(0);
    }
    let raw: RawFd = f.as_raw_fd();
    let exe = self_exe();
    // `Command` does not close arbitrary inherited fds on exec unless told to
    // (`CLOEXEC` on the fd itself is what would do that, and we deliberately
    // did not set it) — so the child's fd table already has `raw` open when
    // its `execve` happens; we only need to tell it the number.
    let out = Command::new(&exe)
        .arg("read-fd")
        .arg(raw.to_string())
        .output()
        .expect("spawn reader");
    assert!(out.status.success(), "reader failed: {:?}", out.status);
    // `f` drops here, closing our own copy; the child's copy (if it hasn't
    // exited yet) keeps the memfd alive independently. No unlink anywhere —
    // there is no path to unlink.
    verify(payload, &out.stdout, "FD");
}

fn verify(payload: &[u8], reader_stdout: &[u8], tag: &str) {
    let text = String::from_utf8_lossy(reader_stdout);
    let line = text.lines().find(|l| l.starts_with("READ-")).unwrap_or("");
    let got_sum = line
        .rsplit("sum=")
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let want_sum = format!("{:08x}", checksum(payload));
    if got_sum == want_sum {
        println!("VERIFY {tag} ok pid={} sum={want_sum}", std::process::id());
    } else {
        println!(
            "VERIFY {tag} MISMATCH pid={} want={want_sum} got={got_sum} reader_line={line:?}",
            std::process::id()
        );
    }
}

fn read_fd(raw: RawFd) {
    let mut f = unsafe { fs::File::from_raw_fd(raw) };
    let mut data = Vec::new();
    f.read_to_end(&mut data).expect("read memfd");
    let sum = checksum(&data);
    println!("READ-FD bytes={} sum={sum:08x}", data.len());
}

fn checksum(data: &[u8]) -> u32 {
    // Not cryptographic — just enough to catch cross-talk/truncation between
    // concurrent pairs in the concurrency test.
    let mut h: u32 = 2166136261;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

fn scan_leftovers(dir: &Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            n += 1;
            let p = e.path();
            let contents = fs::read_to_string(&p).unwrap_or_default();
            println!(
                "LEFTOVER {} ({} bytes) content={:?}",
                p.display(),
                contents.len(),
                &contents[..contents.len().min(40)]
            );
        }
    }
    n
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("write-file") => {
            let dir = PathBuf::from(&args[2]);
            let payload = args[3].as_bytes();
            let die = args.get(4).map(|s| s == "--die-after-write").unwrap_or(false);
            write_file(&dir, payload, die);
        }
        Some("read-file") => {
            read_file(Path::new(&args[2]));
        }
        Some("write-fd") => {
            let payload = args[2].as_bytes();
            let die = args.get(3).map(|s| s == "--die-after-write").unwrap_or(false);
            write_fd(payload, die);
        }
        Some("read-fd") => {
            let n: RawFd = args[2].parse().expect("fd number");
            read_fd(n);
        }
        Some("scan") => {
            let dir = PathBuf::from(&args[2]);
            let n = scan_leftovers(&dir);
            println!("SCAN found={n}");
        }
        _ => {
            eprintln!("usage: d5spike <write-file|read-file|write-fd|read-fd|scan> ...");
            std::process::exit(2);
        }
    }
}
