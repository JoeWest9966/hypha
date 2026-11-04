//! Ptys. These belong to the machine, not the connection: closing the browser
//! does not kill the shell, it just stops the relay.

use crate::exec::Ctx;
use anyhow::{Context, Result};
use hypha_proto::*;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::Arc;

pub struct Pty {
    pub master: Box<dyn MasterPty + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

fn shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
}

pub fn open(ctx: &Arc<Ctx>, id: String, term: Option<String>, cols: u16, rows: u16) -> Result<()> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let mut cmd = CommandBuilder::new(shell());
    cmd.arg("-l");
    cmd.env("TERM", term.unwrap_or_else(|| "xterm-256color".into()));
    cmd.env("COLORTERM", "truecolor");
    cmd.env("HYPHA_SESSION", &id);
    if let Ok(home) = std::env::var("HOME") {
        cmd.cwd(home);
    }

    let child = pair.slave.spawn_command(cmd).context("spawn pty child")?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("pty reader")?;
    let writer = pair.master.take_writer().context("pty writer")?;

    let out = ctx.out.clone();
    let pty_id = id.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if out
                        .send(AgentMsg::PtyChunk {
                            pty: pty_id.clone(),
                            seq: 0,
                            data: b64(&buf[..n]),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = out.send(AgentMsg::PtyExit {
            pty: pty_id,
            code: None,
        });
    });

    ctx.ptys.lock().unwrap().insert(
        id,
        Pty {
            master: pair.master,
            writer,
            child,
        },
    );
    Ok(())
}

pub fn input(ctx: &Ctx, id: &str, data_b64: &str) {
    let Ok(bytes) = b64d(data_b64) else {
        return;
    };
    let mut ptys = ctx.ptys.lock().unwrap();
    if let Some(p) = ptys.get_mut(id) {
        let _ = p.writer.write_all(&bytes);
        let _ = p.writer.flush();
    }
}

pub fn resize(ctx: &Ctx, id: &str, cols: u16, rows: u16) {
    let mut ptys = ctx.ptys.lock().unwrap();
    if let Some(p) = ptys.get_mut(id) {
        let _ = p.master.resize(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}

pub fn close(ctx: &Ctx, id: &str) {
    if let Some(mut p) = ctx.ptys.lock().unwrap().remove(id) {
        let _ = p.child.kill();
        let _ = p.child.wait();
    }
}

pub fn close_all(ctx: &Ctx) {
    let ids: Vec<String> = ctx.ptys.lock().unwrap().keys().cloned().collect();
    for id in ids {
        close(ctx, &id);
    }
}
