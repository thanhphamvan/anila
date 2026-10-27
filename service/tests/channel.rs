//! Acceptance tests for `anila_service::channel` (task T3). Do not edit.
//! The manager is sans-IO: a fake spawner records what it was asked to do.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anila_proto::{
    Ch, Create, Epoch, ErrorCode, Preset, SessionInfo, SessionName, Tab, ToAgent, ToBrowser,
};
use anila_service::channel::{CreateError, Limits, Manager, Outbound, Pty, Spawner};

#[derive(Default)]
struct World {
    sessions: BTreeSet<String>,
    spawns: Vec<(Tab, Ch, Epoch, String, u16, u16)>,
    writes: HashMap<Epoch, Vec<u8>>,
    resizes: Vec<(Epoch, u16, u16)>,
    killed: BTreeSet<Epoch>,
    creates: Vec<(String, String)>,
}

type Shared = Rc<RefCell<World>>;

struct FakePty {
    epoch: Epoch,
    world: Shared,
}

impl Pty for FakePty {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.world
            .borrow_mut()
            .writes
            .entry(self.epoch)
            .or_default()
            .extend_from_slice(bytes);
        Ok(())
    }
    fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        self.world
            .borrow_mut()
            .resizes
            .push((self.epoch, cols, rows));
        Ok(())
    }
    fn kill(&mut self) {
        self.world.borrow_mut().killed.insert(self.epoch);
    }
}

struct FakeSpawner(Shared);

impl Spawner for FakeSpawner {
    fn spawn(
        &mut self,
        tab: Tab,
        ch: Ch,
        epoch: Epoch,
        session: &SessionName,
        cols: u16,
        rows: u16,
    ) -> Result<Box<dyn Pty>, String> {
        let mut w = self.0.borrow_mut();
        if !w.sessions.contains(session.as_str()) {
            return Err("no such session".into());
        }
        w.spawns
            .push((tab, ch, epoch, session.to_string(), cols, rows));
        Ok(Box::new(FakePty {
            epoch,
            world: self.0.clone(),
        }))
    }
    fn session_exists(&mut self, session: &SessionName) -> bool {
        self.0.borrow().sessions.contains(session.as_str())
    }
    fn create(&mut self, session: &SessionName, preset: &str) -> Result<(), CreateError> {
        let mut w = self.0.borrow_mut();
        if preset != "shell" {
            return Err(CreateError::UnknownPreset);
        }
        if !w.sessions.insert(session.to_string()) {
            return Err(CreateError::Exists);
        }
        w.creates.push((session.to_string(), preset.to_string()));
        Ok(())
    }
    fn list(&mut self) -> Result<Vec<SessionInfo>, String> {
        Ok(self
            .0
            .borrow()
            .sessions
            .iter()
            .map(|s| SessionInfo {
                name: s.clone(),
                windows: 1,
                attached: 0,
                activity: 0,
                cmd: "zsh".into(),
            })
            .collect())
    }
}

const TAB: Tab = 7;
const KIB: usize = 1024;

struct Rig {
    m: Manager,
    w: Shared,
    t0: Instant,
}

fn rig_with(limits: Limits) -> Rig {
    let w: Shared = Rc::new(RefCell::new(World::default()));
    w.borrow_mut()
        .sessions
        .extend(["claude".to_string(), "notes".to_string()]);
    let mut next: Epoch = 100;
    let epochs = Box::new(move || {
        next += 1;
        next
    });
    let presets = vec![Preset {
        id: "shell".into(),
        label: "Shell".into(),
    }];
    let m = Manager::new(Box::new(FakeSpawner(w.clone())), presets, limits, epochs);
    Rig {
        m,
        w,
        t0: Instant::now(),
    }
}

fn rig() -> Rig {
    rig_with(Limits::default())
}

fn name(s: &str) -> SessionName {
    SessionName::new(s).unwrap()
}

fn open(r: &mut Rig, ch: Ch, session: &str) -> Vec<Outbound> {
    r.m.handle(
        ToAgent::Open {
            tab: TAB,
            ch,
            req: 1,
            session: name(session),
            create: None,
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    )
}

fn opened_epoch(out: &[Outbound]) -> Epoch {
    match out.first() {
        Some(Outbound::Text(ToBrowser::Opened { epoch, .. })) => *epoch,
        other => panic!("expected opened first, got {other:?}"),
    }
}

/// Concatenated OUT payload and the offset of its first byte.
fn data(out: &[Outbound]) -> (Option<u64>, Vec<u8>) {
    let mut first = None;
    let mut bytes = Vec::new();
    let mut expected_off = None;
    for o in out {
        if let Outbound::Out { off, bytes: b, .. } = o {
            if let Some(e) = expected_off {
                assert_eq!(*off, e, "OUT frames must have contiguous offsets");
            }
            first.get_or_insert(*off);
            expected_off = Some(*off + b.len() as u64);
            bytes.extend_from_slice(b);
        }
    }
    (first, bytes)
}

fn texts(out: &[Outbound]) -> Vec<ToBrowser> {
    out.iter()
        .filter_map(|o| match o {
            Outbound::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn error_code(out: &[Outbound]) -> Option<ErrorCode> {
    texts(out).into_iter().find_map(|t| match t {
        ToBrowser::Error { code, .. } => Some(code),
        _ => None,
    })
}

#[test]
fn list_returns_sessions_and_presets() {
    let mut r = rig();
    let out = r.m.handle(ToAgent::List { tab: TAB, req: 9 }, r.t0);
    match texts(&out).as_slice() {
        [
            ToBrowser::Sessions {
                tab,
                req,
                items,
                presets,
            },
        ] => {
            assert_eq!((*tab, *req), (TAB, 9));
            assert_eq!(
                items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
                ["claude", "notes"]
            );
            assert_eq!(presets[0].id, "shell");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn open_existing_session_then_stream_output() {
    let mut r = rig();
    let out = open(&mut r, 1, "claude");
    let epoch = opened_epoch(&out);
    assert_ne!(epoch, 0);
    assert_eq!(
        r.w.borrow().spawns,
        vec![(TAB, 1, epoch, "claude".to_string(), 80, 24)]
    );

    let out = r.m.on_pty_output(TAB, 1, epoch, b"hello ");
    assert_eq!(data(&out), (Some(0), b"hello ".to_vec()));
    let out = r.m.on_pty_output(TAB, 1, epoch, b"world");
    assert_eq!(data(&out), (Some(6), b"world".to_vec()));
    for o in &out {
        if let Outbound::Out {
            tab, ch, epoch: e, ..
        } = o
        {
            assert_eq!((*tab, *ch, *e), (TAB, 1, epoch));
        }
    }
}

#[test]
fn open_missing_session_is_not_found_with_req() {
    let mut r = rig();
    let out = r.m.handle(
        ToAgent::Open {
            tab: TAB,
            ch: 1,
            req: 42,
            session: name("nope"),
            create: None,
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    );
    match texts(&out).as_slice() {
        [
            ToBrowser::Error {
                tab, ch, req, code, ..
            },
        ] => {
            assert_eq!(
                (*tab, *ch, *req, *code),
                (TAB, Some(1), Some(42), ErrorCode::NotFound)
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(r.m.channel_count(), 0);
}

#[test]
fn open_with_create_uses_preset_and_reports_conflicts() {
    let mut r = rig();
    let create = |preset: &str| {
        Some(Create {
            preset: preset.into(),
        })
    };
    let out = r.m.handle(
        ToAgent::Open {
            tab: TAB,
            ch: 1,
            req: 1,
            session: name("fresh"),
            create: create("shell"),
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    );
    opened_epoch(&out);
    assert_eq!(
        r.w.borrow().creates,
        vec![("fresh".to_string(), "shell".to_string())]
    );

    let out = r.m.handle(
        ToAgent::Open {
            tab: TAB,
            ch: 2,
            req: 2,
            session: name("fresh"),
            create: create("shell"),
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    );
    assert_eq!(error_code(&out), Some(ErrorCode::Exists));

    let out = r.m.handle(
        ToAgent::Open {
            tab: TAB,
            ch: 3,
            req: 3,
            session: name("other"),
            create: create("rocket"),
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    );
    assert_eq!(error_code(&out), Some(ErrorCode::NotFound));
    assert_eq!(r.m.channel_count(), 1);
}

#[test]
fn channel_limit_per_tab() {
    let mut r = rig_with(Limits {
        max_channels_per_tab: 2,
        ..Limits::default()
    });
    opened_epoch(&open(&mut r, 1, "claude"));
    opened_epoch(&open(&mut r, 2, "claude"));
    assert_eq!(
        error_code(&open(&mut r, 3, "claude")),
        Some(ErrorCode::Limit)
    );
    // Another tab has its own budget.
    let out = r.m.handle(
        ToAgent::Open {
            tab: 8,
            ch: 1,
            req: 1,
            session: name("claude"),
            create: None,
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    );
    opened_epoch(&out);
}

#[test]
fn input_is_applied_once_in_order_and_acked_on_tick() {
    let mut r = rig();
    let epoch = opened_epoch(&open(&mut r, 1, "claude"));
    assert!(r.m.input(TAB, 1, 1, b"ls", r.t0).is_empty());
    r.m.input(TAB, 1, 2, b"\r", r.t0);
    r.m.input(TAB, 1, 2, b"\r", r.t0); // duplicate after a resend
    r.m.input(TAB, 1, 1, b"ls", r.t0); // stale
    assert_eq!(r.w.borrow().writes[&epoch], b"ls\r");

    let out = r.m.tick(r.t0 + Duration::from_millis(100));
    assert_eq!(
        texts(&out),
        vec![ToBrowser::InAck {
            tab: TAB,
            ch: 1,
            id: 2
        }]
    );
    assert!(
        texts(&r.m.tick(r.t0 + Duration::from_millis(200))).is_empty(),
        "no new input, no new in_ack"
    );
}

#[test]
fn input_to_unknown_channel_is_not_found() {
    let mut r = rig();
    let out = r.m.input(TAB, 5, 1, b"x", r.t0);
    assert_eq!(error_code(&out), Some(ErrorCode::NotFound));
}

#[test]
fn flow_control_caps_unacked_output_and_frame_size() {
    let mut r = rig_with(Limits {
        window_bytes: 64 * KIB,
        max_out_payload: 16 * KIB,
        ..Limits::default()
    });
    let epoch = opened_epoch(&open(&mut r, 1, "claude"));
    let blob: Vec<u8> = (0..200 * KIB).map(|i| (i % 251) as u8).collect();
    let out = r.m.on_pty_output(TAB, 1, epoch, &blob);
    for o in &out {
        if let Outbound::Out { bytes, .. } = o {
            assert!(bytes.len() <= 16 * KIB);
        }
    }
    let (off, sent) = data(&out);
    assert_eq!(off, Some(0));
    assert_eq!(sent.len(), 64 * KIB, "never more than the window unacked");

    let out = r.m.handle(
        ToAgent::Ack {
            tab: TAB,
            ch: 1,
            epoch,
            off: 32 * KIB as u64,
        },
        r.t0,
    );
    let (off, more) = data(&out);
    assert_eq!(off, Some(64 * KIB as u64));
    assert_eq!(more.len(), 32 * KIB);

    // A stale-epoch ack is ignored.
    assert!(
        data(&r.m.handle(
            ToAgent::Ack {
                tab: TAB,
                ch: 1,
                epoch: epoch + 999,
                off: 96 * KIB as u64
            },
            r.t0
        ))
        .1
        .is_empty()
    );

    let out = r.m.handle(
        ToAgent::Ack {
            tab: TAB,
            ch: 1,
            epoch,
            off: 96 * KIB as u64,
        },
        r.t0,
    );
    let (_, rest) = data(&out);
    assert_eq!(rest.len(), 64 * KIB);
    let mut all = sent;
    all.extend(more);
    all.extend(rest);
    assert_eq!(&all[..], &blob[..160 * KIB]);
}

#[test]
fn orphaned_channel_buffers_then_resume_replays_exactly() {
    let mut r = rig();
    let epoch = opened_epoch(&open(&mut r, 1, "claude"));
    let (_, first) = data(&r.m.on_pty_output(TAB, 1, epoch, b"0123456789"));
    assert_eq!(first, b"0123456789");
    r.m.input(TAB, 1, 1, b"a", r.t0);

    r.m.tab_gone(TAB, r.t0);
    assert!(
        data(&r.m.on_pty_output(TAB, 1, epoch, b"ABCDEF"))
            .1
            .is_empty(),
        "nothing is sent while orphaned"
    );

    // The browser rendered only the first 4 bytes before the link died.
    let out = r.m.handle(
        ToAgent::Resume {
            tab: TAB,
            ch: 1,
            session: name("claude"),
            epoch,
            from: 4,
            cols: 100,
            rows: 30,
            codec: Default::default(),
        },
        r.t0 + Duration::from_secs(5),
    );
    assert_eq!(
        texts(&out).first(),
        Some(&ToBrowser::Resumed {
            tab: TAB,
            ch: 1,
            epoch,
            from: 4,
            in_acked: 1
        }),
        "resumed comes first and reports applied input"
    );
    assert_eq!(data(&out), (Some(4), b"456789ABCDEF".to_vec()));
    assert!(r.w.borrow().resizes.contains(&(epoch, 100, 30)));
    assert_eq!(
        r.w.borrow().spawns.len(),
        1,
        "resume within grace reuses the PTY"
    );
}

#[test]
fn resume_with_wrong_epoch_resets() {
    let mut r = rig();
    let epoch = opened_epoch(&open(&mut r, 1, "claude"));
    r.m.on_pty_output(TAB, 1, epoch, b"old");
    let out = r.m.handle(
        ToAgent::Resume {
            tab: TAB,
            ch: 1,
            session: name("claude"),
            epoch: epoch + 50,
            from: 3,
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    );
    let new_epoch = match texts(&out).as_slice() {
        [
            ToBrowser::Reset {
                tab: TAB,
                ch: 1,
                epoch: e,
            },
        ] => *e,
        other => panic!("expected reset, got {other:?}"),
    };
    assert_ne!(new_epoch, epoch);
    assert!(r.w.borrow().killed.contains(&epoch), "old PTY killed");
    assert_eq!(r.w.borrow().spawns.len(), 2, "re-attached in a new PTY");
    assert!(
        data(&r.m.on_pty_output(TAB, 1, epoch, b"stale"))
            .1
            .is_empty(),
        "old epoch output ignored"
    );
    assert_eq!(
        data(&r.m.on_pty_output(TAB, 1, new_epoch, b"fresh")),
        (Some(0), b"fresh".to_vec())
    );
}

#[test]
fn resume_older_than_buffer_resets() {
    let mut r = rig_with(Limits {
        buffer_bytes: 8,
        ..Limits::default()
    });
    let epoch = opened_epoch(&open(&mut r, 1, "claude"));
    r.m.tab_gone(TAB, r.t0);
    r.m.on_pty_output(TAB, 1, epoch, b"0123456789abcdef");
    let out = r.m.handle(
        ToAgent::Resume {
            tab: TAB,
            ch: 1,
            session: name("claude"),
            epoch,
            from: 2,
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    );
    assert!(
        matches!(texts(&out).as_slice(), [ToBrowser::Reset { .. }]),
        "got {:?}",
        texts(&out)
    );
}

#[test]
fn grace_period_expiry_frees_the_pty() {
    let mut r = rig_with(Limits {
        grace: Duration::from_secs(60),
        ..Limits::default()
    });
    let epoch = opened_epoch(&open(&mut r, 1, "claude"));
    r.m.tab_gone(TAB, r.t0);
    r.m.tick(r.t0 + Duration::from_secs(59));
    assert_eq!(r.m.channel_count(), 1);
    assert!(!r.w.borrow().killed.contains(&epoch));
    r.m.tick(r.t0 + Duration::from_secs(61));
    assert_eq!(r.m.channel_count(), 0);
    assert!(r.w.borrow().killed.contains(&epoch));

    // A late resume re-attaches with a new epoch.
    let out = r.m.handle(
        ToAgent::Resume {
            tab: TAB,
            ch: 1,
            session: name("claude"),
            epoch,
            from: 0,
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0 + Duration::from_secs(70),
    );
    assert!(matches!(texts(&out).as_slice(), [ToBrowser::Reset { .. }]));
    assert_eq!(r.m.channel_count(), 1);
}

#[test]
fn resume_for_vanished_session_is_exited() {
    let mut r = rig();
    let out = r.m.handle(
        ToAgent::Resume {
            tab: TAB,
            ch: 4,
            session: name("ghost"),
            epoch: 1,
            from: 0,
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    );
    assert!(
        matches!(
            texts(&out).as_slice(),
            [ToBrowser::Exited {
                tab: TAB,
                ch: 4,
                ..
            }]
        ),
        "got {:?}",
        texts(&out)
    );
    assert_eq!(r.m.channel_count(), 0);
}

#[test]
fn pty_exit_respawns_while_session_lives_else_exits() {
    let mut r = rig();
    let epoch = opened_epoch(&open(&mut r, 1, "claude"));
    let out = r.m.on_pty_exit(TAB, 1, epoch, r.t0);
    let e2 = match texts(&out).as_slice() {
        [ToBrowser::Reset { epoch: e, .. }] => *e,
        other => panic!("expected reset, got {other:?}"),
    };
    assert_ne!(e2, epoch);
    assert!(
        r.m.on_pty_exit(TAB, 1, epoch, r.t0).is_empty(),
        "stale exit ignored"
    );

    r.w.borrow_mut().sessions.remove("claude");
    let out = r.m.on_pty_exit(TAB, 1, e2, r.t0);
    assert!(matches!(
        texts(&out).as_slice(),
        [ToBrowser::Exited {
            tab: TAB,
            ch: 1,
            ..
        }]
    ));
    assert_eq!(r.m.channel_count(), 0);
}

#[test]
fn close_kills_pty_without_reply() {
    let mut r = rig();
    let epoch = opened_epoch(&open(&mut r, 1, "claude"));
    assert!(
        r.m.handle(ToAgent::Close { tab: TAB, ch: 1 }, r.t0)
            .is_empty()
    );
    assert!(r.w.borrow().killed.contains(&epoch));
    assert_eq!(r.m.channel_count(), 0);
}

#[test]
fn resize_reaches_the_pty() {
    let mut r = rig();
    let epoch = opened_epoch(&open(&mut r, 1, "claude"));
    r.m.handle(
        ToAgent::Resize {
            tab: TAB,
            ch: 1,
            cols: 132,
            rows: 50,
        },
        r.t0,
    );
    assert!(r.w.borrow().resizes.contains(&(epoch, 132, 50)));
}

#[test]
fn link_lost_orphans_every_tab() {
    let mut r = rig_with(Limits {
        grace: Duration::from_secs(10),
        ..Limits::default()
    });
    let e1 = opened_epoch(&open(&mut r, 1, "claude"));
    let out = r.m.handle(
        ToAgent::Open {
            tab: 8,
            ch: 1,
            req: 1,
            session: name("notes"),
            create: None,
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    );
    let e2 = opened_epoch(&out);
    r.m.link_lost(r.t0);
    assert!(data(&r.m.on_pty_output(TAB, 1, e1, b"x")).1.is_empty());
    r.m.tick(r.t0 + Duration::from_secs(11));
    assert_eq!(r.m.channel_count(), 0);
    let killed = r.w.borrow().killed.clone();
    assert!(killed.contains(&e1) && killed.contains(&e2));
}

#[test]
fn reopening_a_channel_replaces_it() {
    let mut r = rig();
    let e1 = opened_epoch(&open(&mut r, 1, "claude"));
    let e2 = opened_epoch(&open(&mut r, 1, "notes"));
    assert_ne!(e1, e2);
    assert!(r.w.borrow().killed.contains(&e1));
    assert_eq!(r.m.channel_count(), 1);
}

#[test]
fn resume_at_the_same_size_does_not_resize() {
    let mut r = rig();
    let epoch = opened_epoch(&open(&mut r, 1, "claude"));
    r.m.tab_gone(TAB, r.t0);
    r.m.handle(
        ToAgent::Resume {
            tab: TAB,
            ch: 1,
            session: name("claude"),
            epoch,
            from: 0,
            cols: 80,
            rows: 24,
            codec: Default::default(),
        },
        r.t0,
    );
    assert!(
        r.w.borrow().resizes.is_empty(),
        "an unchanged size must not trigger a tmux redraw"
    );
}
