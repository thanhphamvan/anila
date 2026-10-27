//! Minimal, session-aware Chrome DevTools Protocol driver for real-browser tests.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    sync::oneshot,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;
type Logs = Arc<Mutex<HashMap<String, Vec<String>>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct PendingCommand {
    id: u64,
    pending: Pending,
}

impl Drop for PendingCommand {
    fn drop(&mut self) {
        lock(&self.pending).remove(&self.id);
    }
}

struct Connection {
    writer: tokio::sync::Mutex<SplitSink<Socket, Message>>,
    pending: Pending,
    logs: Logs,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
}

impl Connection {
    async fn command(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = lock(&self.pending);
            if self.closed.load(Ordering::Acquire) {
                bail!("CDP connection closed");
            }
            pending.insert(id, tx);
        }
        let _pending = PendingCommand {
            id,
            pending: self.pending.clone(),
        };
        let mut message = json!({"id":id,"method":method,"params":params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        timeout(COMMAND_TIMEOUT, async {
            self.writer
                .lock()
                .await
                .send(Message::Text(message.to_string().into()))
                .await?;
            let response = rx.await.context("CDP connection closed")?;
            if let Some(error) = response.get("error") {
                bail!(
                    "{method}: {}",
                    error["message"].as_str().unwrap_or("unknown CDP error")
                );
            }
            response
                .get("result")
                .cloned()
                .context("CDP response has no result")
        })
        .await
        .with_context(|| format!("{method} timed out after 30 s"))?
    }
}

/// A headless Chromium process and its single browser CDP connection.
pub struct Browser {
    _process: Process,
    connection: Arc<Connection>,
    reader: JoinHandle<()>,
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.connection.closed.store(true, Ordering::Release);
        self.reader.abort();
        lock(&self.connection.pending).clear();
    }
}

fn binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ANILA_BROWSER") {
        return Ok(path.into());
    }
    for name in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "google-chrome",
        "chromium",
    ] {
        let path = PathBuf::from(name);
        if path.is_absolute() {
            if path.is_file() {
                return Ok(path);
            }
        } else if let Some(paths) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&paths) {
                let path = directory.join(name);
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
    }
    bail!("no browser binary found; set ANILA_BROWSER")
}

fn description(value: &Value) -> String {
    value["description"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| value["value"].as_str().map(str::to_owned))
        .unwrap_or_else(|| value.get("value").unwrap_or(value).to_string())
}

fn exception(details: &Value) -> String {
    details
        .get("exception")
        .map(description)
        .unwrap_or_else(|| {
            details["text"]
                .as_str()
                .unwrap_or("JavaScript exception")
                .to_owned()
        })
}

impl Browser {
    /// Starts the configured or first available Chromium browser with an isolated profile.
    /// Reads DevToolsActivePort for up to 20 seconds; kills the process on drop.
    pub async fn launch() -> Result<Browser> {
        let binary = binary()?;
        let directory = crate::util::run_dir("browser");
        let stderr = std::fs::File::create(directory.join("stderr.log"))?;
        let mut process = Process(
            Command::new(&binary)
                .args(["--headless=new", "--remote-debugging-port=0"])
                .arg(format!("--user-data-dir={}", directory.display()))
                .args([
                    "--no-first-run",
                    "--no-default-browser-check",
                    "--disable-gpu",
                    "about:blank",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(stderr)
                .spawn()
                .with_context(|| format!("start {}", binary.display()))?,
        );
        let url = timeout(Duration::from_secs(20), async {
            loop {
                if let Some(status) = process.0.try_wait()? {
                    bail!(
                        "browser exited with {status}; see {}",
                        directory.join("stderr.log").display()
                    );
                }
                if let Ok(contents) = std::fs::read_to_string(directory.join("DevToolsActivePort"))
                {
                    let mut lines = contents.lines();
                    if let (Some(port), Some(path)) = (lines.next(), lines.next())
                        && let Ok(port) = port.parse::<u16>()
                        && port != 0
                        && path.starts_with('/')
                    {
                        return Ok(format!("ws://127.0.0.1:{port}{path}"));
                    }
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("timed out waiting for DevToolsActivePort")??;
        let (socket, _) = timeout(COMMAND_TIMEOUT, connect_async(url)).await??;
        let (writer, mut reader) = socket.split();
        let pending: Pending = Arc::default();
        let logs: Logs = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let connection = Arc::new(Connection {
            writer: tokio::sync::Mutex::new(writer),
            pending: pending.clone(),
            logs: logs.clone(),
            next_id: AtomicU64::new(1),
            closed: closed.clone(),
        });
        let reader = tokio::spawn(async move {
            while let Some(message) = reader.next().await {
                let text = match message {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => continue,
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = value["id"].as_u64() {
                    if let Some(tx) = lock(&pending).remove(&id) {
                        let _ = tx.send(value);
                    }
                } else if let Some(session) = value["sessionId"].as_str() {
                    let params = &value["params"];
                    let message = match value["method"].as_str() {
                        Some("Runtime.consoleAPICalled") => Some(
                            params["args"]
                                .as_array()
                                .map(|args| {
                                    args.iter().map(description).collect::<Vec<_>>().join(" ")
                                })
                                .unwrap_or_default(),
                        ),
                        Some("Runtime.exceptionThrown") => {
                            Some(exception(&params["exceptionDetails"]))
                        }
                        _ => None,
                    };
                    if let Some(message) = message {
                        lock(&logs)
                            .entry(session.to_owned())
                            .or_default()
                            .push(message);
                    }
                }
            }
            closed.store(true, Ordering::Release);
            lock(&pending).clear();
        });
        Ok(Self {
            _process: process,
            connection,
            reader,
        })
    }

    /// Creates and attaches a blank page, enabling Page, Runtime and Network events.
    pub async fn new_page(&self) -> Result<Page> {
        let target = self
            .connection
            .command(None, "Target.createTarget", json!({"url":"about:blank"}))
            .await?;
        let target = target["targetId"].as_str().context("missing targetId")?;
        let attached = self
            .connection
            .command(
                None,
                "Target.attachToTarget",
                json!({"targetId":target,"flatten":true}),
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .context("missing sessionId")?
            .to_owned();
        let page = Page {
            connection: self.connection.clone(),
            session,
        };
        for domain in ["Page.enable", "Runtime.enable", "Network.enable"] {
            page.command(domain, json!({})).await?;
        }
        Ok(page)
    }
}

/// A cloneable handle to a flattened CDP page session.
#[derive(Clone)]
pub struct Page {
    connection: Arc<Connection>,
    session: String,
}

impl Page {
    /// Sets the CPU slowdown multiplier (1 disables throttling).
    pub async fn set_cpu_throttling(&self, rate: f64) -> Result<()> {
        anyhow::ensure!(
            rate.is_finite() && rate >= 1.0,
            "CPU rate must be finite and at least 1"
        );
        self.command("Emulation.setCPUThrottlingRate", json!({"rate": rate}))
            .await?;
        Ok(())
    }

    async fn command(&self, method: &str, params: Value) -> Result<Value> {
        self.connection
            .command(Some(&self.session), method, params)
            .await
    }

    /// Navigates and waits until the document has completed loading.
    pub async fn goto(&self, url: &str) -> Result<()> {
        let result = self.command("Page.navigate", json!({"url":url})).await?;
        if let Some(error) = result["errorText"].as_str() {
            bail!("navigation failed: {error}");
        }
        self.wait_for("document.readyState === 'complete'", COMMAND_TIMEOUT)
            .await
    }

    /// Evaluates JavaScript, awaiting promises and returning values or exception descriptions.
    pub async fn eval(&self, js: &str) -> Result<Value> {
        let result = self
            .command(
                "Runtime.evaluate",
                json!({"expression":js,"awaitPromise":true,"returnByValue":true}),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            bail!("{}", exception(details));
        }
        Ok(result["result"]["value"].clone())
    }

    /// Deletes this page's signed-in passkeys labelled exactly `label`, through
    /// the web app's own API, and returns how many it deleted. Deleting the
    /// passkey that signed this page in also ends its session.
    pub async fn delete_passkeys_labelled(&self, label: &str) -> Result<u64> {
        let label = json!(label);
        let result = self
            .eval(&format!(
                "fetch('/api/passkeys').then(r => r.json()).then(async rows => {{
                   if (!Array.isArray(rows)) return 'list failed: ' + JSON.stringify(rows);
                   const mine = rows.filter(r => r.label === {label});
                   for (const r of mine) {{
                     const d = await fetch('/api/passkeys/' + encodeURIComponent(r.id),
                       {{method: 'DELETE', headers: {{'Content-Type': 'application/json'}}}});
                     if (!d.ok) return 'delete failed: ' + d.status + ' ' + await d.text();
                   }}
                   return mine.length;
                 }})"
            ))
            .await?;
        result
            .as_u64()
            .with_context(|| format!("deleting passkeys labelled {label}: {result}"))
    }

    /// Polls a JavaScript expression every 100 ms until truthy or the deadline expires.
    pub async fn wait_for(&self, js: &str, duration: Duration) -> Result<()> {
        timeout(duration, async {
            loop {
                // Resolve first: `Boolean(promise)` is always true.
                if self
                    .eval(&format!("Promise.resolve(({js})).then(Boolean)"))
                    .await?
                    == Value::Bool(true)
                {
                    return Ok(());
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .with_context(|| format!("timed out waiting for {js}"))?
    }

    /// Scrolls the first matching element into view and dispatches a real mouse click.
    pub async fn click(&self, css: &str) -> Result<()> {
        let point = self.eval(&format!("(() => {{ const e = document.querySelector({}); if (!e) throw new Error('element not found'); e.scrollIntoView({{block:'center',inline:'center',behavior:'instant'}}); const r=e.getBoundingClientRect(); return {{x:r.x+r.width/2,y:r.y+r.height/2}}; }})()", json!(css))).await?;
        let x = point["x"].as_f64().context("missing click x")?;
        let y = point["y"].as_f64().context("missing click y")?;
        for kind in ["mousePressed", "mouseReleased"] {
            self.command(
                "Input.dispatchMouseEvent",
                json!({"type":kind,"x":x,"y":y,"button":"left","clickCount":1}),
            )
            .await?;
        }
        Ok(())
    }

    /// Drag across rendered cells using real pointer events (modifiers use CDP bits).
    pub async fn drag(&self, from: (f64, f64), to: (f64, f64), modifiers: u8) -> Result<()> {
        self.command(
            "Input.dispatchMouseEvent",
            json!({"type":"mouseMoved", "x":from.0, "y":from.1, "modifiers":modifiers}),
        )
        .await?;
        self.command("Input.dispatchMouseEvent", json!({"type":"mousePressed", "x":from.0, "y":from.1, "button":"left", "buttons":1, "clickCount":1, "modifiers":modifiers})).await?;
        self.command("Input.dispatchMouseEvent", json!({"type":"mouseMoved", "x":to.0, "y":to.1, "button":"left", "buttons":1, "modifiers":modifiers})).await?;
        self.command("Input.dispatchMouseEvent", json!({"type":"mouseReleased", "x":to.0, "y":to.1, "button":"left", "clickCount":1, "modifiers":modifiers})).await?;
        Ok(())
    }

    /// Dispatch a letter shortcut with CDP modifier bits: Ctrl=2, Meta=4, Shift=8.
    pub async fn shortcut(&self, letter: char, modifiers: u8) -> Result<()> {
        anyhow::ensure!(
            letter.is_ascii_alphabetic(),
            "shortcut must be an ASCII letter"
        );
        let upper = letter.to_ascii_uppercase();
        let key = if modifiers & 8 != 0 {
            upper
        } else {
            letter.to_ascii_lowercase()
        };
        for kind in ["keyDown", "keyUp"] {
            self.command("Input.dispatchKeyEvent", json!({"type":kind, "key":key.to_string(), "code":format!("Key{upper}"), "windowsVirtualKeyCode":u32::from(upper), "modifiers":modifiers})).await?;
        }
        Ok(())
    }

    /// Use a generated local test session; never supply a production cookie.
    pub async fn set_test_session(&self, origin: &str, value: &str) -> Result<()> {
        self.command("Network.setCookie", json!({"url":origin, "name":"__Host-term", "value":value, "secure":true, "httpOnly":true, "path":"/", "sameSite":"Strict"})).await?;
        Ok(())
    }

    /// Permit clipboard assertions on this isolated test origin.
    pub async fn grant_clipboard(&self, origin: &str) -> Result<()> {
        self.connection.command(None, "Browser.grantPermissions", json!({"origin":origin, "permissions":["clipboardReadWrite", "clipboardSanitizedWrite"]})).await?;
        self.command("Page.bringToFront", json!({})).await?;
        Ok(())
    }

    /// Focuses and selects an input or textarea, then replaces its contents with user input.
    pub async fn fill(&self, css: &str, text: &str) -> Result<()> {
        self.eval(&format!("(() => {{ const e=document.querySelector({}); if (!(e instanceof HTMLInputElement || e instanceof HTMLTextAreaElement)) throw new Error('expected input or textarea'); e.focus(); e.select(); }})()", json!(css))).await?;
        self.insert_text(text).await
    }

    /// Inserts text into the focused element.
    pub async fn insert_text(&self, text: &str) -> Result<()> {
        self.command("Input.insertText", json!({"text":text}))
            .await?;
        Ok(())
    }

    /// Dispatches keyDown and keyUp for a special key, including Enter's carriage return.
    pub async fn press(&self, key: &str) -> Result<()> {
        let code = match key {
            "Backspace" => 8,
            "Tab" => 9,
            "Enter" => 13,
            "Escape" => 27,
            " " | "Space" => 32,
            "PageUp" => 33,
            "PageDown" => 34,
            "End" => 35,
            "Home" => 36,
            "ArrowLeft" => 37,
            "ArrowUp" => 38,
            "ArrowRight" => 39,
            "ArrowDown" => 40,
            "Insert" => 45,
            "Delete" => 46,
            _ => bail!("unsupported special key: {key}"),
        };
        let (key, physical) = if code == 32 {
            (" ", "Space")
        } else {
            (key, key)
        };
        for kind in ["keyDown", "keyUp"] {
            let mut params =
                json!({"type":kind,"key":key,"code":physical,"windowsVirtualKeyCode":code});
            if kind == "keyDown" && key == "Enter" {
                params["text"] = json!("\r");
            }
            self.command("Input.dispatchKeyEvent", params).await?;
        }
        Ok(())
    }

    /// Returns the first matching element's innerText, or an empty string if absent.
    pub async fn text(&self, css: &str) -> Result<String> {
        self.eval(&format!(
            "document.querySelector({})?.innerText ?? ''",
            json!(css)
        ))
        .await?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("innerText was not a string"))
    }

    /// Enables WebAuthn and adds an internal CTAP2 authenticator with resident keys and UV.
    pub async fn add_virtual_authenticator(&self) -> Result<String> {
        self.command("WebAuthn.enable", json!({})).await?;
        let result = self.command("WebAuthn.addVirtualAuthenticator", json!({"options":{
            "protocol":"ctap2","transport":"internal","hasResidentKey":true,
            "hasUserVerification":true,"isUserVerified":true,"automaticPresenceSimulation":true
        }})).await?;
        result["authenticatorId"]
            .as_str()
            .map(str::to_owned)
            .context("missing authenticatorId")
    }

    /// Toggles offline networking, restoring unrestricted throughput when online.
    pub async fn set_offline(&self, offline: bool) -> Result<()> {
        self.command(
            "Network.emulateNetworkConditions",
            json!({"offline":offline,"latency":0,"downloadThroughput":-1,"uploadThroughput":-1}),
        )
        .await?;
        Ok(())
    }

    /// Saves a PNG of the visible page to `path`.
    pub async fn screenshot_to(&self, path: &std::path::Path) -> Result<()> {
        use base64::Engine;
        let shot = self
            .command("Page.captureScreenshot", json!({ "format": "png" }))
            .await?;
        let data = shot["data"].as_str().context("missing screenshot data")?;
        std::fs::write(
            path,
            base64::engine::general_purpose::STANDARD.decode(data)?,
        )?;
        Ok(())
    }

    /// Clears browser cookies.
    /// Grants the notifications permission to `origin` (Browser.grantPermissions),
    /// as if the user accepted the prompt.
    pub async fn grant_notifications(&self, origin: &str) -> Result<()> {
        self.connection
            .command(
                None,
                "Browser.grantPermissions",
                json!({ "origin": origin, "permissions": ["notifications"] }),
            )
            .await?;
        Ok(())
    }

    /// Emulates an iPhone-sized touch device: 390x844 at 3x, mobile layout,
    /// touch events, and an iOS Safari user agent.
    pub async fn emulate_phone(&self) -> Result<()> {
        self.command(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": 390, "height": 844, "deviceScaleFactor": 3, "mobile": true }),
        )
        .await?;
        self.command(
            "Emulation.setTouchEmulationEnabled",
            json!({ "enabled": true, "maxTouchPoints": 5 }),
        )
        .await?;
        self.command(
            "Network.setUserAgentOverride",
            json!({ "userAgent": "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1" }),
        )
        .await?;
        Ok(())
    }

    /// Shapes the page's network: added latency and throughput caps in bytes
    /// per second (Chromium's emulation, applied to HTTP and websockets).
    pub async fn set_network(
        &self,
        latency_ms: u64,
        down_bytes_per_sec: u64,
        up_bytes_per_sec: u64,
    ) -> Result<()> {
        self.command(
            "Network.emulateNetworkConditions",
            json!({ "offline": false, "latency": latency_ms,
                    "downloadThroughput": down_bytes_per_sec, "uploadThroughput": up_bytes_per_sec }),
        )
        .await?;
        Ok(())
    }

    pub async fn clear_cookies(&self) -> Result<()> {
        self.command("Network.clearBrowserCookies", json!({}))
            .await?;
        Ok(())
    }

    /// Returns console messages and uncaught exceptions captured for this page so far.
    pub fn console(&self) -> Vec<String> {
        lock(&self.connection.logs)
            .get(&self.session)
            .cloned()
            .unwrap_or_default()
    }
}
