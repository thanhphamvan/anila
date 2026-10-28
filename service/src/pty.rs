//! Real PTYs for the channel manager, built on `portable-pty`.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use anila_proto::{Ch, Epoch, Tab, limits::MAX_OUT_PAYLOAD};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc::UnboundedSender;

use crate::channel::Pty;

/// Reported from the PTY's reader thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    /// At most 32 KiB per event.
    Output {
        tab: Tab,
        ch: Ch,
        epoch: Epoch,
        bytes: Vec<u8>,
    },
    /// Sent exactly once, after the last `Output`, when the child exits or
    /// the PTY is killed.
    Exit { tab: Tab, ch: Ch, epoch: Epoch },
}

/// A running child and the master handles for its terminal.
pub struct PtyProcess {
    master: Box<dyn MasterPty + Send>,
    /// Shared with the reaper thread, which drops the last reference only
    /// after the child exits: portable-pty's writer types "\n" + EOF into
    /// the terminal when dropped, and a live tmux client would forward
    /// those keys to the user's pane.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

/// Starts `program args..` in a new PTY of `cols`×`rows`, with the parent's
/// environment plus `TERM=xterm-256color`. A reader thread forwards output
/// and the final exit to `events`.
pub fn spawn(
    program: &str,
    args: &[String],
    cols: u16,
    rows: u16,
    key: (Tab, Ch, Epoch),
    events: UnboundedSender<PtyEvent>,
) -> std::io::Result<PtyProcess> {
    let mut command = CommandBuilder::new(program);
    command.args(args);
    spawn_command(command, cols, rows, key, events)
}

/// Same PTY lifetime rules, with a caller-prepared child environment.
pub(crate) fn spawn_command(
    mut cmd: CommandBuilder,
    cols: u16,
    rows: u16,
    key: (Tab, Ch, Epoch),
    events: UnboundedSender<PtyEvent>,
) -> std::io::Result<PtyProcess> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(io::Error::other)?;
    let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
    let writer = pair.master.take_writer().map_err(io::Error::other)?;
    cmd.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(cmd).map_err(io::Error::other)?;
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(writer));
    let process = PtyProcess {
        master: pair.master,
        writer: writer.clone(),
        killer: child.clone_killer(),
    };
    thread::spawn(move || {
        let _ = child.wait();
        drop(writer);
    });

    let (tab, ch, epoch) = key;
    thread::Builder::new()
        .name(format!("pty-{tab}-{ch}-{epoch}"))
        .spawn(move || {
            let mut buffer = [0; MAX_OUT_PAYLOAD];
            loop {
                match reader.read(&mut buffer) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Ok(0) | Err(_) => break,
                    Ok(len) => {
                        let _ = events.send(PtyEvent::Output {
                            tab,
                            ch,
                            epoch,
                            bytes: buffer[..len].to_vec(),
                        });
                    }
                }
            }
            let _ = events.send(PtyEvent::Exit { tab, ch, epoch });
        })?;
    Ok(process)
}

impl Pty for PtyProcess {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("pty writer poisoned"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }

    fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        self.kill();
    }
}
