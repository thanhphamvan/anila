//! Part 2 of the end-to-end plan: outages on purpose. A fault proxy sits on
//! the browser leg and another on the service leg; a ticker prints
//! `tick N` ten times a second, and after every fault the numbers on screen
//! must still rise by exactly one (nothing lost, nothing repeated).
//! Run with `cargo test -p anila-e2e --test chaos -- --nocapture --test-threads=1`.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anila_e2e::authn::SoftAuthenticator;
use anila_e2e::probe::Probe;
use anila_e2e::proxy::{Mode, Proxy};
use anila_e2e::tmux::TmuxServer;
use anila_proto::client::Event;
use common::*;

const T: Duration = Duration::from_secs(15);

fn step(name: &str) {
    eprintln!("\n=== {name}");
}

/// Waits until the ticker on `ch` has moved `more` ticks past where it is now.
async fn advance(p: &Probe, ch: u16, more: u64, timeout: Duration) {
    let start = ticks(&p.screen(ch)).last().copied().unwrap_or(0);
    let deadline = Instant::now() + timeout;
    loop {
        let now = ticks(&p.screen(ch)).last().copied().unwrap_or(0);
        if now >= start + more {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "ticker stalled at {now} (wanted {}); stats {:?}",
            start + more,
            p.stats()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn chaos_suite() {
    let worker = start_worker(false);
    let tmux = TmuxServer::new("e2e-chaos");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let upstream: SocketAddr = format!("127.0.0.1:{}", worker.port).parse().unwrap();
    let (browser_leg, agent_leg) = rt.block_on(async {
        (
            Proxy::start(upstream).await.unwrap(),
            Proxy::start(upstream).await.unwrap(),
        )
    });
    let agent_url = format!("http://127.0.0.1:{}", agent_leg.port());
    let browser_ws = format!("ws://127.0.0.1:{}", browser_leg.port());
    let mut service = start_service(&agent_url, &tmux);

    let code = enroll_code(&worker.http_base(), &tmux);
    let mut key = SoftAuthenticator::new(3);
    let http = register(&worker, &code, &mut key);
    let cookie = http.cookie();

    rt.block_on(async {
        let p = probe(&browser_ws, &worker.origin(), cookie.clone(), 21).await;
        wait_connected(&p, 20).await;
        let ch = p
            .open("ticks", Some("ticker"), 80, 24, T)
            .await
            .expect("ticker opens");
        advance(&p, ch, 10, T).await;

        step("X1 browser link hangs 5 s (longer than dead_ms)");
        browser_leg.set_mode(Mode::Blackhole);
        tokio::time::sleep(Duration::from_secs(5)).await;
        browser_leg.set_mode(Mode::Pass);
        advance(&p, ch, 20, T).await;
        assert_contiguous(&p.screen(ch), "X1");
        assert!(
            p.stats().resumes >= 1,
            "resumed after the hang: {:?}",
            p.stats()
        );

        step("X2 browser link cut mid-stream");
        browser_leg.cut_all();
        advance(&p, ch, 20, T).await;
        assert_contiguous(&p.screen(ch), "X2");

        step("X4 flapping browser link");
        for _ in 0..8 {
            browser_leg.set_mode(Mode::Blackhole);
            tokio::time::sleep(Duration::from_millis(1000)).await;
            browser_leg.set_mode(Mode::Pass);
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        advance(&p, ch, 20, T).await;
        assert_contiguous(&p.screen(ch), "X4");
        assert_eq!(
            tmux.clients().len(),
            1,
            "no leaked tmux clients after flapping"
        );

        step("X15 latency 150 ms each way while typing");
        browser_leg.set_latency(Duration::from_millis(150));
        tmux.new_session("typing", "cat");
        let ty = p
            .open("typing", None, 80, 24, T)
            .await
            .expect("open typing");
        assert!(anila_e2e::util::wait_until(Duration::from_secs(5), || tmux
            .clients()
            .len()
            == 2));
        p.type_str(ty, "slow-link\r").await;
        p.wait_output(ty, "slow-link", T)
            .await
            .expect("typing works over a slow link");
        browser_leg.set_latency(Duration::ZERO);
        p.close_channel(ty).await;
        advance(&p, ch, 10, T).await;
        assert_contiguous(&p.screen(ch), "X15");

        step("X6 service link cut");
        agent_leg.cut_all();
        advance(&p, ch, 20, Duration::from_secs(20)).await;
        assert_contiguous(&p.screen(ch), "X6 cut");

        step("X6 service link hangs 35 s: room times it out, service reconnects");
        let resets_before = p.stats().resets;
        agent_leg.set_mode(Mode::Blackhole);
        wait_host(&p, false, 45).await;
        agent_leg.set_mode(Mode::Pass);
        wait_host(&p, true, 30).await;
        advance(&p, ch, 20, T).await;
        assert_contiguous(&p.screen(ch), "X6 hang");
        assert_eq!(
            p.stats().resets,
            resets_before,
            "a hung link resumes exactly, no reset"
        );

        step("X7 host machine sleeps (SIGSTOP)");
        service.pause();
        wait_host(&p, false, 45).await;
        service.resume();
        wait_host(&p, true, 30).await;
        advance(&p, ch, 20, T).await;
        assert_contiguous(&p.screen(ch), "X7");
        assert_eq!(
            p.stats().resets,
            resets_before,
            "a sleeping host resumes exactly, no reset"
        );

        step("X9 tmux client killed");
        let resets_before = p.stats().resets;
        let pids = tmux.cmd(&["list-clients", "-F", "#{client_pid}"]);
        let pid = pids
            .lines()
            .next()
            .expect("a tmux client")
            .trim()
            .to_string();
        std::process::Command::new("kill")
            .arg(&pid)
            .status()
            .unwrap();
        wait_reset(&p, resets_before, 15).await;
        advance(&p, ch, 10, T).await;
        assert_contiguous(&p.screen(ch), "X9 after reset");

        step("X8 service crash (kill -9) and restart");
        let resets_before = p.stats().resets;
        service.kill9();
        wait_host(&p, false, 45).await;
        service.restart().expect("restart");
        wait_host(&p, true, 30).await;
        wait_reset(&p, resets_before, 15).await;
        advance(&p, ch, 10, T).await;
        assert_contiguous(&p.screen(ch), "X8 after reset");

        step("Monkey: 25 random browser and service faults");
        let seed: u64 = std::env::var("CHAOS_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0x5eed_a11a);
        eprintln!("chaos seed {seed:#x} (rerun with CHAOS_SEED={seed})");
        let mut x = seed;
        let mut rand = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let resets_before = p.stats().resets;
        for i in 0..25 {
            let fault = rand(4);
            let ms = 300 + rand(2500);
            eprintln!("  fault {i}: kind {fault}, {ms} ms");
            match fault {
                0 => {
                    browser_leg.set_mode(Mode::Blackhole);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    browser_leg.set_mode(Mode::Pass);
                }
                1 => browser_leg.cut_all(),
                2 => agent_leg.cut_all(),
                _ => {
                    browser_leg.set_latency(Duration::from_millis(ms / 10));
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    browser_leg.set_latency(Duration::ZERO);
                }
            }
            tokio::time::sleep(Duration::from_millis(200 + rand(800))).await;
        }
        advance(&p, ch, 20, Duration::from_secs(30)).await;
        assert_contiguous(&p.screen(ch), "monkey");
        assert_eq!(
            p.stats().resets,
            resets_before,
            "network faults alone never force a reset"
        );
        assert_eq!(tmux.clients().len(), 1, "no leaked tmux clients");
        eprintln!(
            "stats {:?}, proxy bytes browser {:?} service {:?}",
            p.stats(),
            browser_leg.bytes(),
            agent_leg.bytes()
        );

        step("X10 tmux server killed");
        expect_event(
            &p,
            15,
            |e| matches!(e, Event::Exited { ch: c, .. } if *c == ch),
            || {
                tmux.cmd(&["kill-server"]);
            },
        )
        .await;

        p.shutdown().await;
    });
    drop(service);
    eprintln!("\nchaos suite passed");
}
