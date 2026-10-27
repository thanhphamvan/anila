use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anila_proto::{HostId, api::PairRequest};

use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand};

use anila_service::{
    api,
    config::{Config, default_key_path},
    keys::ServiceKey,
    link::{self, Timings},
    pair,
    tmux::Tmux,
};

#[derive(Parser)]
#[command(
    name = "anila-service",
    version,
    about = "Serves this machine's tmux sessions to the anila web app"
)]
struct Cli {
    /// Config file. Defaults to ~/.config/anila/service.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair this machine with your account using a code from the web app.
    Pair {
        /// The code shown by "Add machine" in the web app.
        code: String,
        /// Machine name; defaults to this computer's hostname.
        #[arg(long)]
        name: Option<String>,
        /// Worker URL, e.g. https://anila.example.workers.dev. Required when
        /// there is no service.toml yet.
        #[arg(long)]
        worker: Option<String>,
    },
    /// Generate a private signing key and print the worker configuration.
    Keygen,
    /// Generate a VAPID key pair for worker push notifications.
    VapidKeygen,
    /// Send a push notification to subscribed devices.
    Notify {
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        session: Option<String>,
    },
    /// Dial out to the host room and serve sessions until stopped.
    Run,
    /// Print a one-time code for registering a passkey in the web app.
    Enroll,
    /// End every signed-in browser session.
    RevokeAll,
    /// List tmux sessions the way the web app will see them.
    Sessions,
    /// Parse the config file and print the result.
    CheckConfig,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let path = cli
        .config
        .unwrap_or_else(anila_service::config::default_path);
    let load = || Config::load(&path).with_context(|| format!("reading {}", path.display()));

    match cli.cmd {
        Cmd::Pair { code, name, worker } => pair_machine(&path, code, name, worker)?,
        Cmd::Keygen => {
            // Use the config's key_file and host when a config exists.
            let config = if path.try_exists()? {
                Some(load()?)
            } else {
                None
            };
            let key_path = config
                .as_ref()
                .map(Config::key_path)
                .unwrap_or_else(default_key_path);
            let host = config.as_ref().map(|c| c.host.as_str()).unwrap_or("my-mac");
            let key = ServiceKey::save_new(&key_path)?;
            let public_key = key.public_key_b64();
            println!("Key file: {}", key_path.display());
            println!("Public key: {public_key}");
            println!(
                "To add this machine to your account, create a pairing code in the web app (\"Add machine\") and run: anila-service pair <code>"
            );
            println!("For a brand-new deployment's first machine, the worker variable is:");
            println!("HOST_KEYS = '{{\"{host}\": \"{public_key}\"}}'");
        }
        Cmd::VapidKeygen => {
            let key = p256::ecdsa::SigningKey::random(&mut rand_core::OsRng);
            let private = URL_SAFE_NO_PAD.encode(key.to_bytes());
            let public =
                URL_SAFE_NO_PAD.encode(key.verifying_key().to_encoded_point(false).as_bytes());
            println!("Private key: {private}");
            println!("Public key: {public}");
            println!("From worker/, run: npx wrangler secret put VAPID_PRIVATE_KEY");
            println!("For local development, add to worker/.dev.vars:");
            println!("VAPID_PRIVATE_KEY=\"{private}\"");
        }
        Cmd::Notify {
            title,
            body,
            session,
        } => {
            let config = load()?;
            let key = load_key(&config)?;
            let session = match session {
                Some(session) => Some(session),
                None if std::env::var_os("TMUX").is_some() => {
                    let output = std::process::Command::new("tmux")
                        .args(["display-message", "-p", "#S"])
                        .output()
                        .context("reading current tmux session")?;
                    anyhow::ensure!(
                        output.status.success(),
                        "tmux display-message failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    Some(
                        String::from_utf8(output.stdout)
                            .context("tmux session is not UTF-8")?
                            .trim_end()
                            .to_owned(),
                    )
                }
                None => None,
            };
            let result = api::notify(&config, &key, &title, &body, session.as_deref())?;
            if result.sent == 0 {
                println!("No devices subscribed. Open the web app and turn on notifications.");
            } else {
                println!(
                    "Sent to {} {}.",
                    result.sent,
                    if result.sent == 1 {
                        "device"
                    } else {
                        "devices"
                    }
                );
            }
        }
        Cmd::CheckConfig => {
            println!("{:#?}", load()?);
        }
        Cmd::Sessions => {
            let config = load()?;
            let tmux = Tmux::new(config.tmux_socket.clone());
            let sessions = tmux.list_sessions()?;
            if sessions.is_empty() {
                println!("no tmux sessions");
            }
            for s in sessions {
                println!(
                    "{:<32} {:>2} windows  {} attached  {}",
                    s.name, s.windows, s.attached, s.cmd
                );
            }
        }
        Cmd::Run => {
            let config = load()?;
            let key = load_key(&config)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            tokio::task::LocalSet::new().block_on(&runtime, async move {
                let (stop, stopped) = tokio::sync::watch::channel(false);
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
                let signals = async move {
                    tokio::select! {
                        result = tokio::signal::ctrl_c() => result?,
                        _ = terminate.recv() => {},
                    }
                    let _ = stop.send(true);
                    Ok::<(), anyhow::Error>(())
                };
                tokio::try_join!(link::run(config, key, Timings::default(), stopped), signals,)?;
                Ok::<(), anyhow::Error>(())
            })?;
        }
        Cmd::Enroll => {
            let config = load()?;
            let key = load_key(&config)?;
            let code = api::enroll(&config, &key)?;
            println!("Enroll code: {}", code.code);
            println!(
                "Valid for 10 minutes, once. Open {} in your browser, choose",
                config.worker
            );
            println!("\"First time on this device\", and enter the code.");
        }
        Cmd::RevokeAll => {
            let config = load()?;
            let key = load_key(&config)?;
            api::revoke_all(&config, &key)?;
            println!("Signed out every browser.");
        }
    }
    Ok(())
}

fn load_key(config: &Config) -> anyhow::Result<ServiceKey> {
    ServiceKey::load(&config.key_path()).map_err(|err| {
        if err
            .downcast_ref::<std::io::Error>()
            .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound)
        {
            err.context("key missing; run anila-service keygen")
        } else {
            err
        }
    })
}

fn pair_machine(
    path: &Path,
    code: String,
    name: Option<String>,
    worker_override: Option<String>,
) -> anyhow::Result<()> {
    let existing = if path.try_exists()? {
        Some(std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)
    } else {
        None
    };
    let config = existing.as_deref().map(Config::parse).transpose()?;
    let worker = worker_override
        .as_deref()
        .or_else(|| config.as_ref().map(|c| c.worker.as_str()))
        .context("no service.toml yet: pass --worker <https://your-worker>")?;
    anyhow::ensure!(
        worker.starts_with("https://")
            || worker.starts_with("http://localhost")
            || worker.starts_with("http://127.0.0.1"),
        "worker must be an https:// URL (plain http is allowed only for localhost)"
    );
    let name = match name {
        Some(name) => HostId::new(name)
            .context("machine name must match ^[a-z0-9-]{1,32}$ (1 to 32 lowercase letters, digits or dashes)")?,
        None => {
            let hostname = std::env::var_os("HOSTNAME").map(|s| s.to_string_lossy().into_owned())
                .or_else(|| {
                    let output = std::process::Command::new("hostname").output().ok()?;
                    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                })
                .unwrap_or_else(|| "mac".into());
            pair::default_name(&hostname)
        }
    };
    let key_path = config
        .as_ref()
        .map(Config::key_path)
        .unwrap_or_else(default_key_path);
    let key = if key_path.try_exists()? {
        ServiceKey::load(&key_path)?
    } else {
        let key = ServiceKey::save_new(&key_path)?;
        println!("Created a key at {}", key_path.display());
        key
    };
    let access = match config.as_ref() {
        Some(config) => config.access_headers()?,
        None => anila_service::config::access_headers_env(None, None)?,
    };
    let paired = api::pair(
        worker,
        &access,
        &key,
        &PairRequest {
            code,
            name,
            public_key: key.public_key_b64(),
        },
    )?;
    let text = match existing {
        Some(ref text) => pair::rewrite_config(text, &paired.host, worker_override.as_deref()),
        None => pair::new_config(&paired.host, worker),
    };
    save_config(path, &text)?;
    println!(
        "Paired as {} on {}'s account (machine id {}).",
        paired.name, paired.account_name, paired.host
    );
    println!("Saved {}.", path.display());
    println!("Start the service with: anila-service run");
    println!("If anila-service is already running, restart it to use the new id.");
    Ok(())
}

fn save_config(path: &Path, text: &str) -> anyhow::Result<()> {
    use rand_core::RngCore;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".anila-{:016x}.tmp", rand_core::OsRng.next_u64()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .context("creating temporary service config")?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.with_context(|| format!("saving {}", path.display()))
}
