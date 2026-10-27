use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use anila_e2e::browser::Browser;
use anyhow::{Context, Result};
use serde_json::json;

const HTML: &str = r#"<!doctype html><html><body>
<input id="name" value="old"><button id="go">Copy</button><p id="out"></p>
<textarea id="ta"></textarea>
<script>
document.querySelector('#go').onclick = event => {
    const value = document.querySelector('#name').value;
    document.querySelector('#out').innerText = value;
    console.log('copied', value, event.isTrusted);
};
</script></body></html>"#;

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("bind browser test HTTP server")?;
        let url = format!("http://{}", listener.local_addr()?);
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("accept HTTP request: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    request.push(byte[0]);
                }
                if !request.ends_with(b"\r\n\r\n") {
                    continue;
                }
                // Chromium can cancel speculative requests while navigating.
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML}",
                    HTML.len()
                );
            }
        });
        Ok(Self {
            url,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[tokio::test]
async fn real_browser_driver() -> Result<()> {
    let browser = match Browser::launch().await {
        Ok(browser) => browser,
        Err(error) if error.to_string() == "no browser binary found; set ANILA_BROWSER" => {
            eprintln!("skipping real_browser_driver: no browser binary found");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let server = Server::start()?;
    let page = browser.new_page().await?;
    page.goto(&server.url).await?;
    assert_eq!(page.eval("document.readyState").await?, "complete");
    let name = "Ada 'quoted' \"name\" \\ 😀";
    page.fill("#name", name).await?;
    page.click("#go").await?;
    assert_eq!(page.text("#out").await?, name);
    assert_eq!(page.text("#absent").await?, "");
    assert!(page.click("#absent").await.is_err());
    page.fill("#ta", "replace me").await?;
    page.fill("#ta", "first").await?;
    page.press("Enter").await?;
    page.insert_text("second").await?;
    assert_eq!(
        page.eval("document.querySelector('#ta').value").await?,
        "first\nsecond"
    );
    assert_eq!(
        page.eval("Promise.resolve({answer:42})").await?,
        json!({"answer":42})
    );
    let error = page
        .eval("throw new Error('expected evaluation failure')")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expected evaluation failure"));
    page.wait_for("({truthy:true})", Duration::from_secs(1))
        .await?;
    let start = std::time::Instant::now();
    assert!(
        page.wait_for("false", Duration::from_millis(250))
            .await
            .is_err()
    );
    assert!(start.elapsed() >= Duration::from_millis(250));
    assert!(start.elapsed() < Duration::from_secs(2));
    page.eval("setTimeout(() => { throw new Error('uncaught test exception'); }, 0)")
        .await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let logs = page.console();
            if logs
                .iter()
                .any(|line| line.contains("copied") && line.contains(name) && line.contains("true"))
                && logs
                    .iter()
                    .any(|line| line.contains("uncaught test exception"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("console capture: {:?}", page.console()))?;
    let fetch = "fetch('/probe', {cache:'no-store'}).then(r => r.ok).catch(() => false)";
    assert_eq!(page.eval(fetch).await?, true);
    page.set_offline(true).await?;
    assert_eq!(page.eval(fetch).await?, false);
    page.set_offline(false).await?;
    assert_eq!(page.eval(fetch).await?, true);
    assert!(!page.add_virtual_authenticator().await?.is_empty());
    page.eval("document.cookie='browser_test=value; path=/'")
        .await?;
    assert_eq!(
        page.eval("document.cookie.includes('browser_test=value')")
            .await?,
        true
    );
    page.clear_cookies().await?;
    assert_eq!(page.eval("document.cookie").await?, "");

    let second = browser.new_page().await?;
    let clone = page.clone();
    let (first_value, second_value) = tokio::join!(clone.eval("21 * 2"), second.eval("6 * 9"));
    assert_eq!(first_value?, 42);
    assert_eq!(second_value?, 54);
    assert!(second.console().is_empty());
    drop(browser);
    assert!(page.eval("1").await.is_err());
    Ok(())
}
