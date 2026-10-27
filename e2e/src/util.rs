//! Small synchronous helpers shared by the process harness.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

/// Returns the workspace containing this crate.
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Reserves an ephemeral loopback port briefly, then releases it.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .expect("allocate a loopback port")
        .port()
}

/// Creates a unique directory for logs and state from one harness run.
pub fn run_dir(name: &str) -> PathBuf {
    let parent = workspace_root().join("e2e/.runs");
    std::fs::create_dir_all(&parent).expect("create harness runs directory");
    loop {
        let n = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{name}-{}-{n}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create run directory {}: {error}", path.display()),
        }
    }
}

/// Polls a predicate every 20 ms until it succeeds or the timeout expires.
pub fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20).min(timeout - start.elapsed().min(timeout)));
    }
}

/// Returns seconds since the Unix epoch (negative before the epoch).
pub fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(error) => -(error.duration().as_secs() as i64),
    }
}
