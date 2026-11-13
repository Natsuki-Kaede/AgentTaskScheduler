//! Holds the command until its process identity has been recorded by the daemon.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::runspec::RunSpec;

const PERMIT: u8 = 1;

pub(super) fn command(spec: &RunSpec) -> Result<(std::process::Command, tokio::net::UnixStream)> {
    let (executor, worker) = UnixStream::pair()?;
    executor.set_nonblocking(true)?;
    let executor = tokio::net::UnixStream::from_std(executor)?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("__launch")
        .arg("--spec")
        .arg(serde_json::to_string(spec)?)
        .arg("--gate")
        .arg(worker.as_raw_fd().to_string());
    // SAFETY: the captured stream owns the descriptor through spawn. fcntl is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || set_close_on_exec(worker.as_raw_fd(), false));
    }
    Ok((cmd, executor))
}

pub(super) async fn permit(mut gate: tokio::net::UnixStream) -> Result<Option<String>> {
    gate.write_all(&[PERMIT])
        .await
        .context("releasing the command")?;
    let mut failure = String::new();
    gate.read_to_string(&mut failure)
        .await
        .context("reading the command startup status")?;
    Ok((!failure.is_empty()).then_some(failure))
}

pub fn launch(spec: RunSpec, gate: i32) -> Result<ExitCode> {
    ensure!(gate > 2, "invalid launch gate descriptor");
    set_close_on_exec(gate, true).context("closing the launch gate on exec")?;
    // SAFETY: fcntl confirmed a valid descriptor, dedicated to this launch process.
    let mut gate = unsafe { std::fs::File::from_raw_fd(gate) };
    ensure!(
        gate.metadata()?.file_type().is_socket(),
        "launch gate is not a socket"
    );
    let mut permit = [0];
    if gate.read(&mut permit)? == 0 || permit[0] != PERMIT {
        return Ok(ExitCode::from(1));
    }
    let error = spec.command().exec();
    gate.write_all(error.to_string().as_bytes())?;
    Ok(ExitCode::from(127))
}

fn set_close_on_exec(fd: i32, close: bool) -> std::io::Result<()> {
    // SAFETY: this sets descriptor flags on the dedicated gate.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, if close { libc::FD_CLOEXEC } else { 0 }) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
