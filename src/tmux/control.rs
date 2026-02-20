use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};

use super::parser::{parse_control_marker, parse_line, ControlMarker};
use super::WindowInfo;

pub struct TmuxControl {
    stdin: ChildStdin,
    events: mpsc::Receiver<super::TmuxEvent>,
    waiters: Arc<Mutex<VecDeque<oneshot::Sender<Result<String>>>>>,
    child: Child,
    reader_task: tokio::task::JoinHandle<()>,
}

impl TmuxControl {
    pub async fn new(session: &str) -> Result<Self> {
        let mut child = Command::new("tmux")
            .arg("-CC")
            .arg("attach")
            .arg("-t")
            .arg(session)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn tmux -CC attach -t {session}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to capture tmux stdin"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture tmux stdout"))?;

        let (event_tx, event_rx) = mpsc::channel::<super::TmuxEvent>(256);
        let waiters: Arc<Mutex<VecDeque<oneshot::Sender<Result<String>>>>> =
            Arc::new(Mutex::new(VecDeque::new()));

        let waiters_for_task = Arc::clone(&waiters);
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            let mut in_progress: Option<String> = None;

            loop {
                let line = match reader.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(err) => {
                        let _ = event_tx
                            .send(super::TmuxEvent::Error(format!(
                                "failed to read tmux control output: {err}"
                            )))
                            .await;
                        break;
                    }
                };

                if let Some(marker) = parse_control_marker(&line) {
                    match marker {
                        ControlMarker::Begin => {
                            // If already in a block, resolve the old waiter with error
                            if let Some(old_data) = in_progress.take() {
                                let mut waiters = waiters_for_task.lock().await;
                                if let Some(waiter) = waiters.pop_front() {
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
                                    // Resolve the oldest pending waiter (FIFO)
                                    // Skip cancelled waiters (rx dropped due to timeout)
                                    let mut waiters = waiters_for_task.lock().await;
                                    while let Some(waiter) = waiters.pop_front() {
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

                                        // If send succeeds, the waiter was alive. Done.
                                        // If send fails, rx was dropped (timeout). Skip to next.
                                        if waiter.send(result).is_ok() {
                                            break;
                                        }
                                    }
                                }
                                None => {
                                    // Unsolicited %end/%error (e.g., from attach init)
                                    // Silently discard
                                }
                            }
                        }
                    }
                    continue;
                }

                if line.starts_with('%') {
                    if let Some(event) = parse_line(&line) {
                        if event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    continue;
                }

                // Non-% lines: accumulate inside a begin/end block
                if let Some(data) = in_progress.as_mut() {
                    data.push_str(&line);
                    data.push('\n');
                }
            }

            // Resolve any remaining waiters with errors
            let mut waiters = waiters_for_task.lock().await;
            while let Some(waiter) = waiters.pop_front() {
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
            child,
            reader_task,
        })
    }

    pub async fn send_command(&mut self, cmd: &str) -> Result<String> {
        let (tx, rx) = oneshot::channel::<Result<String>>();
        self.waiters.lock().await.push_back(tx);

        let cmd = cmd.trim_end_matches('\n');
        if let Err(err) = self.stdin.write_all(cmd.as_bytes()).await {
            self.waiters.lock().await.pop_back();
            return Err(anyhow!("failed to write tmux command: {err}"));
        }

        if let Err(err) = self.stdin.write_all(b"\n").await {
            self.waiters.lock().await.pop_back();
            return Err(anyhow!("failed to write tmux command newline: {err}"));
        }

        if let Err(err) = self.stdin.flush().await {
            self.waiters.lock().await.pop_back();
            return Err(anyhow!("failed to flush tmux stdin: {err}"));
        }

        match timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(anyhow!("tmux command response channel closed")),
            Err(_) => {
                // Timeout — rx is dropped, reader will skip this waiter's tx
                // (send will fail → reader pops next live waiter)
                Err(anyhow!("timed out waiting for tmux response"))
            }
        }
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
        self.reader_task.abort();
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
