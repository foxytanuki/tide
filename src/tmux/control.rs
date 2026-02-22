use std::collections::VecDeque;
use std::os::unix::io::FromRawFd;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};

use super::parser::{parse_control_marker, parse_line, ControlMarker};
use super::WindowInfo;

/// Guard that kills the child process on drop unless disarmed via `take()`.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn take(&mut self) -> Child {
        self.0.take().expect("ChildGuard already taken")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.0 {
            let _ = child.start_kill();
        }
    }
}

struct WaiterState {
    queue: VecDeque<oneshot::Sender<Result<String>>>,
    /// Number of responses to silently discard (from timed-out commands whose
    /// %end has not yet arrived).
    skip_responses: usize,
}

pub struct TmuxControl {
    stdin: tokio::fs::File,
    events: mpsc::Receiver<super::TmuxEvent>,
    waiters: Arc<Mutex<WaiterState>>,
    child: Child,
    reader_task: tokio::task::JoinHandle<()>,
}

impl TmuxControl {
    pub async fn new(session: &str) -> Result<Self> {
        let (pty_master, pty_slave) =
            open_pty().context("failed to allocate pty for tmux control mode")?;

        // tmux gets the pty slave for stdin, stdout, and stderr
        // so control mode output goes through the tty
        let slave_stdin = pty_slave.try_clone().context("dup pty slave for stdin")?;
        let slave_stdout = pty_slave.try_clone().context("dup pty slave for stdout")?;

        let mut guard = ChildGuard(Some(
            Command::new("tmux")
                .arg("-CC")
                .arg("attach")
                .arg("-t")
                .arg(session)
                .env_remove("TMUX")
                .stdin(Stdio::from(slave_stdin))
                .stdout(Stdio::from(slave_stdout))
                .stderr(Stdio::from(pty_slave))
                .spawn()
                .with_context(|| format!("failed to spawn tmux -CC attach -t {session}"))?,
        ));

        // Give the child a moment to fail (e.g. bad session name)
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Some(status) = guard.0.as_mut().unwrap().try_wait()? {
            // Try to read error output from pty master
            let mut err_buf = [0u8; 1024];
            let err_msg = {
                use std::io::Read;
                use std::os::unix::io::AsRawFd;
                match pty_master.try_clone() {
                    Ok(mut master_clone) => {
                        unsafe {
                            let flags = libc::fcntl(master_clone.as_raw_fd(), libc::F_GETFL);
                            libc::fcntl(
                                master_clone.as_raw_fd(),
                                libc::F_SETFL,
                                flags | libc::O_NONBLOCK,
                            );
                        }
                        match master_clone.read(&mut err_buf) {
                            Ok(n) => {
                                String::from_utf8_lossy(&err_buf[..n]).trim().to_string()
                            }
                            Err(_) => String::new(),
                        }
                    }
                    Err(_) => String::new(),
                }
            };
            return Err(anyhow!(
                "tmux -CC exited immediately ({}): {}",
                status,
                if err_msg.is_empty() {
                    "no output"
                } else {
                    &err_msg
                }
            ));
        }

        // Split pty master into reader and writer
        let master_for_write = pty_master.try_clone().context("dup pty master for write")?;
        let master_for_read = pty_master;

        let stdin = tokio::fs::File::from_std(master_for_write);

        // Spawn a blocking thread to read lines from the pty master
        let (line_tx, mut line_rx) = mpsc::channel::<String>(512);
        std::thread::Builder::new()
            .name("tmux-reader".into())
            .spawn(move || {
                use std::io::BufRead;
                let reader = std::io::BufReader::new(master_for_read);
                let mut reader = reader;
                let mut buf = Vec::<u8>::new();

                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf) {
                        Ok(0) => break,
                        Ok(_) => {
                            if matches!(buf.last(), Some(b'\n')) {
                                buf.pop();
                            }
                            if matches!(buf.last(), Some(b'\r')) {
                                buf.pop();
                            }

                            let line = String::from_utf8_lossy(&buf).into_owned();
                            if line_tx.blocking_send(line).is_err() {
                                break;
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .context("failed to spawn tmux reader thread")?;

        let (event_tx, event_rx) = mpsc::channel::<super::TmuxEvent>(256);
        let waiters: Arc<Mutex<WaiterState>> =
            Arc::new(Mutex::new(WaiterState {
                queue: VecDeque::new(),
                skip_responses: 0,
            }));

        let waiters_for_task = Arc::clone(&waiters);
        let reader_task = tokio::spawn(async move {
            let mut in_progress: Option<String> = None;

            loop {
                let line = match line_rx.recv().await {
                    Some(line) => line,
                    None => break,
                };

                if let Some(marker) = parse_control_marker(&line) {
                    match marker {
                        ControlMarker::Begin => {
                            if let Some(old_data) = in_progress.take() {
                                let mut state = waiters_for_task.lock().await;
                                if state.skip_responses > 0 {
                                    state.skip_responses -= 1;
                                } else if let Some(waiter) = state.queue.pop_front() {
                                    let _ = waiter.send(Err(anyhow!(
                                        "response block interrupted by new %begin (data: {})",
                                        old_data.trim()
                                    )));
                                }
                            }
                            in_progress = Some(String::new());
                        }
                        ControlMarker::End | ControlMarker::ErrorEnd => {
                            let is_error = matches!(marker, ControlMarker::ErrorEnd);
                            match in_progress.take() {
                                Some(data) => {
                                    let mut state = waiters_for_task.lock().await;
                                    if state.skip_responses > 0 {
                                        state.skip_responses -= 1;
                                    } else {
                                        while let Some(waiter) = state.queue.pop_front() {
                                            let result = if is_error {
                                                let err_msg = data.trim().to_string();
                                                Err(anyhow!(
                                                    "tmux command error: {}",
                                                    if err_msg.is_empty() {
                                                        "unknown error".to_string()
                                                    } else {
                                                        err_msg
                                                    }
                                                ))
                                            } else {
                                                Ok(data.clone())
                                            };

                                            if waiter.send(result).is_ok() {
                                                break;
                                            }
                                        }
                                    }
                                }
                                None => {
                                    // Unsolicited %end/%error (e.g., from attach init)
                                }
                            }
                        }
                    }
                    continue;
                }

                // If inside a response block, non-marker lines are response data.
                // %-prefixed lines that match a known notification are emitted as
                // events and NOT appended to the response.  Unrecognized %-lines
                // (e.g. pane IDs like %0, %1) ARE kept as response data.
                if let Some(data) = in_progress.as_mut() {
                    if line.starts_with('%') {
                        if let Some(event) = parse_line(&line) {
                            if !send_event(&event_tx, event).await {
                                break;
                            }
                            continue;
                        }
                        // Unrecognized %-line: fall through to append as response data
                    }
                    data.push_str(&line);
                    data.push('\n');
                    continue;
                }

                // Outside a response block, %-prefixed lines are events
                if line.starts_with('%') {
                    if let Some(event) = parse_line(&line) {
                        if !send_event(&event_tx, event).await {
                            break;
                        }
                    }
                }
            }

            let mut state = waiters_for_task.lock().await;
            while let Some(waiter) = state.queue.pop_front() {
                let _ = waiter.send(Err(anyhow!("tmux control stream closed")));
            }

            let _ = event_tx
                .send(super::TmuxEvent::Error(
                    "tmux control process stdout closed".to_string(),
                ))
                .await;
        });

        Ok(Self {
            stdin,
            events: event_rx,
            waiters,
            child: guard.take(),
            reader_task,
        })
    }

    pub async fn send_command(&mut self, cmd: &str) -> Result<String> {
        let cmd = cmd.trim_end_matches('\n');
        if cmd.contains('\n') {
            return Err(anyhow!(
                "tmux command must not contain embedded newlines: {:?}",
                cmd
            ));
        }

        let (tx, rx) = oneshot::channel::<Result<String>>();
        self.waiters.lock().await.queue.push_back(tx);

        tracing::trace!(cmd, "tmux send");
        if let Err(err) = self.stdin.write_all(cmd.as_bytes()).await {
            self.waiters.lock().await.queue.pop_back();
            return Err(anyhow!("failed to write tmux command: {err}"));
        }

        if let Err(err) = self.stdin.write_all(b"\n").await {
            self.waiters.lock().await.queue.pop_back();
            return Err(anyhow!("failed to write tmux command newline: {err}"));
        }

        if let Err(err) = self.stdin.flush().await {
            self.waiters.lock().await.queue.pop_back();
            return Err(anyhow!("failed to flush tmux stdin: {err}"));
        }

        let result = match timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(anyhow!("tmux command response channel closed")),
            Err(_) => {
                let mut state = self.waiters.lock().await;
                // Only increment skip_responses if our waiter is still in the
                // queue.  If the reader already consumed it (race between
                // response arrival and timeout), the queue is empty and we must
                // not bump skip_responses — doing so would discard the next
                // legitimate response.
                if state.queue.pop_back().is_some() {
                    state.skip_responses += 1;
                }
                Err(anyhow!("timed out waiting for tmux response"))
            }
        };

        if let Err(ref err) = result {
            tracing::warn!(cmd, %err, "tmux command failed");
        }

        result
    }

    pub fn event_stream(&mut self) -> &mut mpsc::Receiver<super::TmuxEvent> {
        &mut self.events
    }

    pub async fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        let response = self
            .send_command(
                "list-windows -F '#{window_id} #{window_index} #{window_name} #{window_active}'",
            )
            .await?;

        let mut windows = Vec::new();
        for line in response.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let window = parse_window_line(line)
                .with_context(|| format!("failed to parse list-windows line: {line}"))?;
            windows.push(window);
        }
        Ok(windows)
    }

    pub async fn shutdown(&mut self) {
        let _ = self.stdin.write_all(b"detach-client\n").await;
        let _ = self.stdin.flush().await;
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.reader_task.abort();
    }
}

impl Drop for TmuxControl {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        // Give the child a brief window to exit after SIGKILL so we
        // don't leave a zombie.  Can't use async wait in Drop.
        if matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(50));
            let _ = self.child.try_wait();
        }
        self.reader_task.abort();
    }
}

/// Allocate a pty pair with raw mode (no echo, no output processing).
/// Returns (master, slave) as std Files.
fn open_pty() -> Result<(std::fs::File, std::fs::File)> {
    unsafe {
        let master_fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master_fd < 0 {
            anyhow::bail!("posix_openpt: {}", std::io::Error::last_os_error());
        }

        if libc::grantpt(master_fd) != 0 {
            libc::close(master_fd);
            anyhow::bail!("grantpt: {}", std::io::Error::last_os_error());
        }

        if libc::unlockpt(master_fd) != 0 {
            libc::close(master_fd);
            anyhow::bail!("unlockpt: {}", std::io::Error::last_os_error());
        }

        let slave_name = libc::ptsname(master_fd);
        if slave_name.is_null() {
            libc::close(master_fd);
            anyhow::bail!("ptsname: {}", std::io::Error::last_os_error());
        }

        let slave_fd = libc::open(slave_name, libc::O_RDWR | libc::O_NOCTTY);
        if slave_fd < 0 {
            libc::close(master_fd);
            anyhow::bail!("open pty slave: {}", std::io::Error::last_os_error());
        }

        // Set raw mode: no echo, no signals, no output processing
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave_fd, &mut tio) == 0 {
            libc::cfmakeraw(&mut tio);
            libc::tcsetattr(slave_fd, libc::TCSANOW, &tio);
        }

        Ok((
            std::fs::File::from_raw_fd(master_fd),
            std::fs::File::from_raw_fd(slave_fd),
        ))
    }
}

/// Send an event through the channel.  PaneOutput events are best-effort
/// (try_send) to avoid backpressure from high-frequency streaming output
/// stalling %begin/%end processing and command responses.  All other
/// events use blocking send.  Returns false when the channel is closed.
async fn send_event(
    tx: &mpsc::Sender<super::TmuxEvent>,
    event: super::TmuxEvent,
) -> bool {
    if matches!(event, super::TmuxEvent::PaneOutput(_)) {
        // Best-effort: drop if channel is full rather than blocking.
        // Closed channel → return false to break the reader loop.
        match tx.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => true, // drop, keep going
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    } else {
        tx.send(event).await.is_ok()
    }
}

fn parse_window_line(line: &str) -> Result<WindowInfo> {
    let mut left = line.splitn(3, ' ');
    let id = left
        .next()
        .ok_or_else(|| anyhow!("missing window_id"))?
        .to_string();

    let index = left
        .next()
        .ok_or_else(|| anyhow!("missing window_index"))?
        .parse::<usize>()
        .context("invalid window_index")?;

    let rest = left
        .next()
        .ok_or_else(|| anyhow!("missing window_name/window_active"))?;

    let mut right = rest.rsplitn(2, ' ');
    let active_raw = right
        .next()
        .ok_or_else(|| anyhow!("missing window_active"))?;
    let name = right.next().unwrap_or("").to_string();

    let active = match active_raw {
        "1" => true,
        "0" => false,
        other => return Err(anyhow!("invalid window_active value: {other}")),
    };

    Ok(WindowInfo {
        id,
        index,
        name,
        active,
    })
}
