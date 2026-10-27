//! Child-process ownership for the real worker and host service.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::util::{free_port, run_dir, wait_until, workspace_root};

/// Public configuration injected into the local worker.
pub struct WorkerOpts {
    /// Enables the worker's development authentication mode.
    pub dev_insecure: bool,
    /// HOST_KEYS JSON, e.g. {"test-host": "<base64url public key>"}.
    pub host_keys: String,
    /// Host selected when the browser does not specify one.
    pub default_host: String,
    /// More `[vars]`, e.g. `VAPID_PRIVATE_KEY` for push tests.
    pub extra_vars: Vec<(String, String)>,
    /// Workspace to run the worker from (default: this one); the upgrade
    /// test points it at a checkout of an older release.
    pub root: Option<PathBuf>,
    /// Durable Object state directory to reuse (default: a fresh one).
    pub state: Option<PathBuf>,
}

impl Default for WorkerOpts {
    fn default() -> Self {
        Self {
            dev_insecure: false,
            host_keys: "{}".into(),
            default_host: String::new(),
            extra_vars: Vec::new(),
            root: None,
            state: None,
        }
    }
}

/// A wrangler process group with isolated state and captured output.
pub struct Worker {
    /// Loopback HTTP and WebSocket port.
    pub port: u16,
    child: Option<Child>,
    config: PathBuf,
    log_path: PathBuf,
}

impl Worker {
    /// Starts pinned wrangler in the worker directory and waits for HTTP readiness.
    pub fn start(opts: WorkerOpts) -> anyhow::Result<Worker> {
        let root = opts.root.clone().unwrap_or_else(workspace_root);
        // wrangler would load it next to the config and override the test vars.
        if root.join("worker/.dev.vars").exists() {
            bail!("worker/.dev.vars exists and would override the e2e vars; move it away first");
        }
        let port = free_port();
        let dir = run_dir("worker");
        let mut worker = Self {
            port,
            child: None,
            config: root.join(format!("worker/.e2e-{port}.toml")),
            log_path: dir.join("wrangler.log"),
        };
        let template = fs::read_to_string(root.join("worker/wrangler.toml"))?;
        let mut config = String::new();
        let mut in_vars = false;
        for line in template.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_vars = trimmed.split('#').next().unwrap_or_default().trim() == "[vars]";
            }
            if !in_vars {
                config.push_str(line);
                config.push('\n');
            }
        }
        config.push_str("\n[vars]\n");
        for (key, value) in [
            ("ORIGIN", worker.origin()),
            ("RP_ID", "localhost".into()),
            ("DEFAULT_HOST", opts.default_host),
            ("HOST_KEYS", opts.host_keys),
            (
                "DEV_INSECURE",
                if opts.dev_insecure { "1" } else { "0" }.into(),
            ),
        ] {
            config.push_str(&format!("{key} = {}\n", toml_string(&value)));
        }
        for (key, value) in &opts.extra_vars {
            config.push_str(&format!("{key} = {}\n", toml_string(value)));
        }
        fs::write(&worker.config, config)?;
        let index = root.join("frontend/dist/index.html");
        if !index.exists() {
            fs::create_dir_all(root.join("frontend/dist"))?;
            fs::write(index, "<!doctype html><title>anila</title>")?;
        }
        let log = append_log(&worker.log_path)?;
        worker.child = Some(
            Command::new("npx")
                .args(["-y", "wrangler@4.148.0", "dev", "-c"])
                .arg(format!(".e2e-{port}.toml"))
                .args([
                    "--port",
                    &port.to_string(),
                    "--ip",
                    "127.0.0.1",
                    "--persist-to",
                ])
                .arg(opts.state.clone().unwrap_or_else(|| dir.join("state")))
                .current_dir(root.join("worker"))
                .env("CARGO_NET_OFFLINE", "true")
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .process_group(0)
                .spawn()
                .context("start wrangler")?,
        );
        let start = Instant::now();
        loop {
            if let Some(child) = worker.child.as_mut()
                && let Some(status) = child.try_wait()?
            {
                bail!("wrangler exited ({status}):\n{}", tail(&worker.log()));
            }
            if start.elapsed() >= Duration::from_secs(300) {
                bail!("wrangler readiness timed out:\n{}", tail(&worker.log()));
            }
            if worker.log().contains("Ready on") && http_ready(port) {
                return Ok(worker);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Returns the worker's loopback HTTP base URL.
    pub fn http_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Returns the worker's loopback WebSocket base URL.
    pub fn ws_base(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    /// Returns the browser origin configured for authentication.
    pub fn origin(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Returns everything wrangler has printed so far.
    pub fn log(&self) -> String {
        read_log(&self.log_path)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let group = format!("-{}", child.id());
            signal("TERM", &group);
            std::thread::sleep(Duration::from_secs(2));
            signal("KILL", &group);
            let _ = child.wait();
        }
        let _ = fs::remove_file(&self.config);
    }
}

/// A host service process with persistent configuration, key, and log files.
pub struct Service {
    bin: PathBuf,
    child: Option<Child>,
    config: PathBuf,
    log_path: PathBuf,
    pid: u32,
}

impl Service {
    /// Builds the service once, stores the supplied seed privately, and starts it.
    /// The caller's configuration must not contain `key_file`.
    pub fn start(config_toml: &str, key_seed: [u8; 32]) -> anyhow::Result<Service> {
        Self::start_bin(None, config_toml, key_seed)
    }

    /// `start` with another build of the service (e.g. an older release);
    /// `None` builds and uses this workspace's.
    pub fn start_bin(
        bin: Option<PathBuf>,
        config_toml: &str,
        key_seed: [u8; 32],
    ) -> anyhow::Result<Service> {
        if config_toml.lines().any(|line| {
            line.split_once('=')
                .is_some_and(|(key, _)| key.trim().trim_matches(['\'', '"']) == "key_file")
        }) {
            bail!("service config must not set key_file");
        }
        static BUILD: OnceLock<Result<(), String>> = OnceLock::new();
        let built = if bin.is_some() {
            &Ok(())
        } else {
            BUILD.get_or_init(|| {
                let output = Command::new("cargo")
                    .args(["build", "-q", "-p", "anila-service"])
                    .env("CARGO_NET_OFFLINE", "true")
                    .current_dir(workspace_root())
                    .output()
                    .map_err(|error| error.to_string())?;
                if output.status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "service build failed ({}):\n{}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr)
                    ))
                }
            })
        };
        if let Err(error) = built {
            bail!("{error}");
        }
        let dir = run_dir("service");
        let key_path = dir.join("service.key");
        let mut key = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)?;
        writeln!(key, "{}", URL_SAFE_NO_PAD.encode(key_seed))?;
        let config = dir.join("service.toml");
        // Root keys must precede any [[preset]] tables in the supplied config.
        fs::write(
            &config,
            format!(
                "key_file = {}\n{config_toml}",
                toml_string(&key_path.to_string_lossy())
            ),
        )?;
        let mut service = Self {
            bin: bin.unwrap_or_else(|| workspace_root().join("target/debug/anila-service")),
            child: None,
            config,
            log_path: dir.join("service.log"),
            pid: 0,
        };
        service.restart()?;
        Ok(service)
    }

    /// Returns the most recently started child's process id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Suspends the running service.
    pub fn pause(&self) {
        if self.child.is_some() {
            signal("STOP", &self.pid.to_string());
        }
    }

    /// Resumes a suspended service.
    pub fn resume(&self) {
        if self.child.is_some() {
            signal("CONT", &self.pid.to_string());
        }
    }

    /// Kills and reaps the service immediately.
    pub fn kill9(&mut self) {
        if let Some(mut child) = self.child.take() {
            signal("KILL", &child.id().to_string());
            let _ = child.wait();
        }
    }

    /// Requests termination, allowing five seconds before killing and reaping.
    pub fn stop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                self.child = None;
                return;
            }
            signal("TERM", &child.id().to_string());
            if wait_until(Duration::from_secs(5), || {
                matches!(child.try_wait(), Ok(Some(_)))
            }) {
                self.child = None;
            } else {
                self.kill9();
            }
        }
    }

    /// Replaces the service process, reusing its configuration and key.
    pub fn restart(&mut self) -> anyhow::Result<()> {
        self.stop();
        let log = append_log(&self.log_path)?;
        let child = Command::new(&self.bin)
            .arg("--config")
            .arg(&self.config)
            .arg("run")
            .current_dir(workspace_root())
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("start anila-service")?;
        self.pid = child.id();
        self.child = Some(child);
        Ok(())
    }

    /// Checks whether the child is still running, reaping it if it exited.
    pub fn is_running(&mut self) -> bool {
        match self.child.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => true,
            Some(Ok(Some(_))) => {
                self.child = None;
                false
            }
            _ => false,
        }
    }

    /// Returns all service output, including previous starts.
    pub fn log(&self) -> String {
        read_log(&self.log_path)
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
    }
}

fn append_log(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn read_log(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

fn signal(name: &str, target: &str) {
    let _ = Command::new("kill")
        .args([&format!("-{name}"), "--", target])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn tail(log: &str) -> String {
    log.lines()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

fn toml_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            ch if ch.is_control() => output.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn http_ready(port: u16) -> bool {
    let result = (|| -> std::io::Result<bool> {
        let timeout = Duration::from_millis(200);
        let mut stream = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            timeout,
        )?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        write!(
            stream,
            "GET /api/me HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )?;
        let mut status = [0; 12];
        stream.read_exact(&mut status)?;
        Ok(status.starts_with(b"HTTP/1.")
            && status[8] == b' '
            && status[9..12].iter().all(u8::is_ascii_digit))
    })();
    result.unwrap_or(false)
}
