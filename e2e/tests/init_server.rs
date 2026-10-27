//! `scripts/init-server.sh` on a fresh Ubuntu 24.04 server with systemd (a
//! privileged Docker container): it creates `cloud-user`, installs tmux and
//! the given Linux build of anila-service, pairs with a pairing code from a
//! local worker, and starts the systemd unit. Then a session opens as
//! `cloud-user` with the `su` hint, and `su - alice` lands in Alice's shell.
//!
//! Needs Docker and a Linux anila-service for the Docker host's CPU (e.g. a
//! release archive's binary). Skipped unless `ANILA_LINUX_BINARY` is set:
//! `ANILA_LINUX_BINARY=/path/to/anila-service cargo test -p anila-e2e --test init_server -- --nocapture`

mod common;

use std::process::Command;
use std::time::Duration;

use anila_e2e::authn::SoftAuthenticator;
use anila_e2e::tmux::TmuxServer;
use anila_e2e::util::{run_dir, workspace_root};
use common::*;
use serde_json::json;

const T: Duration = Duration::from_secs(15);
const IMAGE: &str = "anila-init-server-test:ubuntu24.04";

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .output()
        .expect("docker runs");
    assert!(
        out.status.success(),
        "docker {args:?} failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

#[test]
fn init_server_script() {
    let Some(binary) = std::env::var_os("ANILA_LINUX_BINARY") else {
        eprintln!("ANILA_LINUX_BINARY not set, skipping");
        return;
    };
    let binary = std::path::PathBuf::from(binary);
    assert!(binary.exists(), "{binary:?}");

    eprintln!("\n=== a worker, an admin account and a pairing code");
    let worker = start_worker(false);
    let url = worker.http_base();
    let tmux = TmuxServer::new("e2e-init");
    let _service = start_service(&url, &tmux);
    let code = enroll_code(&url, &tmux);
    let mut key = SoftAuthenticator::new(21);
    let (status, body, mut admin) = try_register(&worker, &code, &mut key, Some("Admin"));
    assert_eq!(status, 201, "{body}");
    let (status, body) = admin.post("/api/pair-codes", json!({}));
    assert_eq!(status, 200, "{body}");
    let pair_code = body["code"].as_str().unwrap().to_string();

    eprintln!("\n=== a fresh Ubuntu server with systemd");
    let dir = run_dir("init-server");
    std::fs::write(
        dir.join("Dockerfile"),
        "FROM ubuntu:24.04\n\
         RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq systemd systemd-sysv socat >/dev/null\n\
         CMD [\"/lib/systemd/systemd\"]\n",
    )
    .unwrap();
    docker(&["build", "-q", "-t", IMAGE, dir.to_str().unwrap()]);
    let name = format!("anila-init-{}", std::process::id());
    let script = workspace_root().join("scripts/init-server.sh");
    docker(&[
        "run",
        "-d",
        "--name",
        &name,
        "--privileged",
        "--cgroupns=host",
        "-v",
        "/sys/fs/cgroup:/sys/fs/cgroup:rw",
        // On Linux the container shares the host's network and reaches the
        // worker on 127.0.0.1 directly; Docker Desktop needs a forwarder.
        if cfg!(target_os = "linux") {
            "--network=host"
        } else {
            "--add-host=host.docker.internal:host-gateway"
        },
        "-v",
        &format!("{}:/root/init-server.sh:ro", script.display()),
        "-v",
        &format!("{}:/root/anila-service:ro", binary.display()),
        IMAGE,
    ]);
    let container = Container(name.clone());
    let exec = |cmd: &str| docker(&["exec", &container.0, "sh", "-c", cmd]);
    // The local worker listens on the Docker host's loopback; forward it.
    for _ in 0..50 {
        if Command::new("docker")
            .args(["exec", &name, "systemctl", "is-system-running"])
            .output()
            .is_ok_and(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                s.contains("running") || s.contains("degraded")
            })
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let worker_url = if cfg!(target_os = "linux") {
        format!("http://localhost:{}", worker.port)
    } else {
        docker(&[
            "exec",
            "-d",
            &name,
            "socat",
            "TCP-LISTEN:8787,fork,reuseaddr",
            &format!("TCP:host.docker.internal:{}", worker.port),
        ]);
        "http://localhost:8787".to_string()
    };
    exec("useradd -m -s /bin/bash alice && echo 'alice:alice-pw-1' | chpasswd");

    eprintln!("\n=== init-server.sh as root");
    let out = exec(&format!(
        "sh /root/init-server.sh --code {pair_code} --binary /root/anila-service \
         --worker {worker_url} --name ubuntu-box 2>&1"
    ));
    eprintln!("{out}");
    assert!(out.contains("creating user cloud-user"), "{out}");
    assert!(out.contains("Paired as ubuntu-box"), "{out}");
    assert!(
        out.contains("anila.service is running as cloud-user"),
        "{out}"
    );
    assert_eq!(exec("systemctl is-enabled anila.service").trim(), "enabled");
    assert_eq!(
        exec("stat -c %U /home/cloud-user/.config/anila/service.toml").trim(),
        "cloud-user"
    );
    assert!(
        exec("passwd -S cloud-user").contains(" L "),
        "cloud-user has no usable password"
    );
    assert!(
        !exec("id cloud-user").contains("sudo"),
        "cloud-user is not a sudoer"
    );
    // A second run without a code keeps the pairing and restarts cleanly.
    let again = exec("sh /root/init-server.sh 2>&1");
    assert!(again.contains("already paired"), "{again}");

    eprintln!("\n=== the machine is online in the account");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let id = loop {
        let (_, hosts) = admin.get("/api/hosts");
        if let Some(h) = hosts
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["name"] == "ubuntu-box" && h["online"] == true)
        {
            break h["id"].as_str().unwrap().to_string();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ubuntu-box online: {hosts}\n{}",
            exec("journalctl -u anila --no-pager -n 30 2>&1")
        );
        std::thread::sleep(Duration::from_millis(300));
    };

    eprintln!("\n=== a new session lands as cloud-user, then su to alice");
    let cookie = admin.cookie().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let p = probe_host(&worker.ws_base(), &worker.origin(), Some(cookie), 1, &id).await;
        wait_connected(&p, 15).await;
        let (_, presets) = p.list(T).await.expect("list");
        assert_eq!(presets.len(), 1, "{presets:?}");
        let ch = p
            .open("work", Some(presets[0].id.as_str()), 100, 30, T)
            .await
            .expect("create a session");
        p.wait_output(ch, "su - <your-user>", T)
            .await
            .expect("the su hint");
        assert!(p.type_str(ch, "echo me=$(id -un)\r").await);
        p.wait_output(ch, "me=cloud-user", T)
            .await
            .expect("lands as cloud-user");
        assert!(p.type_str(ch, "su - alice\r").await);
        p.wait_output(ch, "Password:", T)
            .await
            .expect("su asks for alice's password");
        assert!(p.type_str(ch, "alice-pw-1\r").await);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(p.type_str(ch, "echo now=$(id -un)\r").await);
        p.wait_output(ch, "now=alice", T)
            .await
            .expect("in alice's shell");
        p.shutdown().await;
    });
    eprintln!("\ninit-server suite passed");
}
