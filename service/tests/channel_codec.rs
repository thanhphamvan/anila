use std::time::Instant;

use anila_proto::{
    Ch, Codec, Epoch, SessionInfo, SessionName, Tab, ToAgent, ToBrowser, codec::Decoder,
};
use anila_service::channel::{CreateError, Limits, Manager, Outbound, Pty, Spawner};

struct FakePty;
impl Pty for FakePty {
    fn write(&mut self, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn resize(&mut self, _: u16, _: u16) -> std::io::Result<()> {
        Ok(())
    }
    fn kill(&mut self) {}
}
struct FakeSpawner;
impl Spawner for FakeSpawner {
    fn spawn(
        &mut self,
        _: Tab,
        _: Ch,
        _: Epoch,
        _: &SessionName,
        _: u16,
        _: u16,
    ) -> Result<Box<dyn Pty>, String> {
        Ok(Box::new(FakePty))
    }
    fn session_exists(&mut self, _: &SessionName) -> bool {
        true
    }
    fn create(&mut self, _: &SessionName, _: &str) -> Result<(), CreateError> {
        Ok(())
    }
    fn list(&mut self) -> Result<Vec<SessionInfo>, String> {
        Ok(Vec::new())
    }
}

fn name() -> SessionName {
    SessionName::new("shell").unwrap()
}

fn opened(codec: Codec, limits: Limits) -> Manager {
    let mut epoch = 0;
    let mut manager = Manager::new(
        Box::new(FakeSpawner),
        vec![],
        limits,
        Box::new(move || {
            epoch += 1;
            epoch
        }),
    );
    assert!(matches!(
        manager
            .handle(
                ToAgent::Open {
                    tab: 7,
                    ch: 1,
                    req: 1,
                    session: name(),
                    create: None,
                    cols: 80,
                    rows: 24,
                    codec,
                },
                Instant::now()
            )
            .as_slice(),
        [Outbound::Text(ToBrowser::Opened { epoch: 1, .. })]
    ));
    manager
}

fn resume(manager: &mut Manager, from: u64, codec: Codec) -> Vec<Outbound> {
    manager.handle(
        ToAgent::Resume {
            tab: 7,
            ch: 1,
            session: name(),
            epoch: 1,
            from,
            cols: 80,
            rows: 24,
            codec,
        },
        Instant::now(),
    )
}

fn decode(frames: &[Outbound], decoder: &mut Decoder, from: u64, cap: usize) -> Vec<u8> {
    let mut output = Vec::new();
    for frame in frames {
        if let Outbound::Out {
            off,
            bytes,
            compressed,
            ..
        } = frame
        {
            assert!(*compressed);
            assert_eq!(*off, from + output.len() as u64);
            assert!(bytes.len() <= cap);
            output.extend(decoder.decode(bytes).unwrap());
        }
    }
    output
}

#[test]
fn compressed_output_offsets_and_resume_use_uncompressed_bytes() {
    let limits = Limits {
        window_bytes: 48 * 1024,
        ..Limits::default()
    };
    let cap = limits.max_out_payload;
    let mut manager = opened(Codec::DeflateStream, limits);
    let input = b"\x1b[32m Compiling terminal output\x1b[0m\r\n".repeat(4000);
    let frames = manager.on_pty_output(7, 1, 1, &input);
    let mut decoder = Decoder::new(Codec::DeflateStream);
    let first = decode(&frames, &mut decoder, 0, cap);
    assert_eq!(first, input[..48 * 1024]);
    let next = manager.handle(
        ToAgent::Ack {
            tab: 7,
            ch: 1,
            epoch: 1,
            off: first.len() as u64,
        },
        Instant::now(),
    );
    let second = decode(&next, &mut decoder, first.len() as u64, cap);
    assert_eq!(second, input[first.len()..first.len() + second.len()]);
    let replay = resume(&mut manager, 1000, Codec::DeflateStream);
    assert!(matches!(
        replay[0],
        Outbound::Text(ToBrowser::Resumed { from: 1000, .. })
    ));
    let replayed = decode(&replay, &mut Decoder::new(Codec::DeflateStream), 1000, cap);
    assert_eq!(replayed, input[1000..1000 + 48 * 1024]);
}

#[test]
fn incompressible_output_retries_without_advancing_stream() {
    let limits = Limits::default();
    let cap = limits.max_out_payload;
    let mut manager = opened(Codec::DeflateStream, limits);
    let mut seed = 0x9e3779b97f4a7c15_u64;
    let input: Vec<u8> = (0..40 * 1024)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 32) as u8
        })
        .collect();
    let frames = manager.on_pty_output(7, 1, 1, &input);
    assert_eq!(
        decode(&frames, &mut Decoder::new(Codec::DeflateStream), 0, cap),
        input
    );
    let frames = resume(&mut manager, 0, Codec::DeflateStream);
    assert_eq!(
        decode(&frames, &mut Decoder::new(Codec::DeflateStream), 0, cap),
        input
    );
}

#[test]
fn reset_restarts_compression_and_resume_can_change_codec() {
    let mut manager = opened(Codec::DeflateStream, Limits::default());
    manager.on_pty_output(7, 1, 1, b"old output");
    let reset = manager.on_pty_exit(7, 1, 1, Instant::now());
    assert!(matches!(
        reset.as_slice(),
        [Outbound::Text(ToBrowser::Reset { epoch: 2, .. })]
    ));
    let frames = manager.on_pty_output(7, 1, 2, b"fresh output");
    assert_eq!(
        decode(&frames, &mut Decoder::new(Codec::DeflateStream), 0, 32768),
        b"fresh output"
    );
    let mut manager = opened(Codec::None, Limits::default());
    manager.on_pty_output(7, 1, 1, b"replay");
    let frames = resume(&mut manager, 0, Codec::DeflateStream);
    assert_eq!(
        decode(&frames, &mut Decoder::new(Codec::DeflateStream), 0, 32768),
        b"replay"
    );
}

#[test]
fn unsupported_codecs_fall_back_on_open_and_resume() {
    // ZstdStream is now supported by the service (feature zstd-encode).
    for codec in [Codec::Unknown, Codec::Lz4, Codec::Zstd] {
        let mut manager = opened(codec, Limits::default());
        let frames = manager.on_pty_output(7, 1, 1, b"plain");
        assert!(
            matches!(frames.as_slice(), [Outbound::Out { compressed: false, bytes, .. }] if bytes == b"plain")
        );
        let frames = resume(&mut manager, 0, codec);
        assert!(
            matches!(&frames[1], Outbound::Out { compressed: false, bytes, .. } if bytes == b"plain")
        );
    }
}
