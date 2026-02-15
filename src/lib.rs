use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Maximum display length for a single task description in the summary.
pub const MAX_DISPLAY_LEN: usize = 60;
/// Maximum display length for the current-task header.
pub const MAX_CURRENT_TASK_LEN: usize = 120;
/// Keep a bounded amount of task output in memory while redrawing.
const MAX_RENDERED_OUTPUT_LINES: usize = 4000;

/// Truncate a string for display, appending "..." if it exceeds `max_len`.
pub fn truncate_display(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

/// Print the execution plan before running.
pub fn print_plan(prompts: &[String], loops: usize, work_dir: Option<&str>) {
    println!("=== Agent Loops Plan ===");
    if let Some(dir) = work_dir {
        println!("Work dir: {dir}");
    }
    println!(
        "Loops: {loops} | Tasks: {} | Total runs: {}",
        prompts.len(),
        prompts.len() * loops
    );
    println!("Task list:");
    for (i, prompt) in prompts.iter().enumerate() {
        println!("  {}. {}", i + 1, truncate_display(prompt, MAX_DISPLAY_LEN));
    }
    println!();
    println!("========================\n");
}

fn task_header_slot() -> &'static Mutex<Option<Vec<String>>> {
    static TASK_HEADER: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();
    TASK_HEADER.get_or_init(|| Mutex::new(None))
}

fn set_current_task_header(lines: Option<Vec<String>>) {
    let mut guard = match task_header_slot().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = lines;
}

fn current_task_header() -> Option<Vec<String>> {
    let guard = match task_header_slot().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.clone()
}

fn current_task_header_or_default(prompt: &str) -> Vec<String> {
    current_task_header().unwrap_or_else(|| {
        vec![
            "=== Agent Loops ===".to_string(),
            format!(
                "Current task: {}",
                truncate_display(prompt, MAX_CURRENT_TASK_LEN)
            ),
            "----------------------------------------".to_string(),
        ]
    })
}

struct CurrentTaskHeaderGuard;

impl CurrentTaskHeaderGuard {
    fn new(lines: Vec<String>) -> Self {
        set_current_task_header(Some(lines));
        Self
    }
}

impl Drop for CurrentTaskHeaderGuard {
    fn drop(&mut self) {
        set_current_task_header(None);
    }
}

/// Run a single codex conversation with the given prompt.
/// Uses `codex exec --dangerously-bypass-approvals-and-sandbox` for full access.
/// If `work_dir` is provided, passes `-C <dir>` to codex to set its working directory.
/// Returns `Ok(true)` on success, `Ok(false)` on non-zero exit.
pub async fn run_codex(
    prompt: &str,
    work_dir: Option<&Path>,
    codex_bin: &str,
) -> std::io::Result<bool> {
    let mut args: Vec<String> = vec![
        "exec".to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
    ];
    if let Some(dir) = work_dir {
        args.extend(["-C".to_string(), dir.to_string_lossy().to_string()]);
    }
    args.push(prompt.to_string());

    let pinned_header = current_task_header_or_default(prompt);
    let status = if cfg!(windows) {
        let mut cmd_args = vec!["/C".to_string(), codex_bin.to_string()];
        cmd_args.extend(args.clone());
        let mut cmd = Command::new("cmd");
        cmd.args(&cmd_args);
        run_command_with_forwarded_output(cmd, Some(pinned_header.clone())).await?
    } else {
        let mut direct_cmd = Command::new(codex_bin);
        direct_cmd.args(&args);
        match run_command_with_forwarded_output(direct_cmd, Some(pinned_header.clone())).await {
            Ok(status) => status,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match run_codex_via_shell(codex_bin, &args, pinned_header.as_slice()).await {
                    Ok(status) => {
                        if status.code() == Some(127) {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                format!(
                                    "could not execute `{codex_bin}`; it was not found in PATH or shell startup configuration"
                                ),
                            ));
                        }
                        status
                    }
                    Err(shell_e) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!(
                                "could not execute `{codex_bin}`; direct launch failed ({e}); shell fallback failed ({shell_e})"
                            ),
                        ));
                    }
                }
            }
            Err(e) => return Err(e),
        }
    };
    Ok(status.success())
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

async fn run_command_with_forwarded_output(
    mut cmd: Command,
    pinned_header: Option<Vec<String>>,
) -> io::Result<ExitStatus> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to capture child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("failed to capture child stderr"))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<(OutputStream, Vec<u8>)>();
    let stdout_task = spawn_output_reader(stdout, OutputStream::Stdout, tx.clone());
    let stderr_task = spawn_output_reader(stderr, OutputStream::Stderr, tx.clone());
    drop(tx);

    if let Some(header_lines) = pinned_header.filter(|_| io::stdout().is_terminal()) {
        let mut renderer = PinnedOutputRenderer::new(header_lines)?;
        while let Some((_stream, chunk)) = rx.recv().await {
            renderer.push_chunk(&chunk)?;
        }
        renderer.finish()?;
    } else {
        let mut out = tokio::io::stdout();
        let mut err = tokio::io::stderr();
        while let Some((stream, chunk)) = rx.recv().await {
            match stream {
                OutputStream::Stdout => out.write_all(&chunk).await?,
                OutputStream::Stderr => err.write_all(&chunk).await?,
            }
        }
        out.flush().await?;
        err.flush().await?;
    }

    await_reader_task(stdout_task, "stdout").await?;
    await_reader_task(stderr_task, "stderr").await?;

    child.wait().await
}

fn spawn_output_reader<R>(
    mut reader: R,
    stream: OutputStream,
    tx: mpsc::UnboundedSender<(OutputStream, Vec<u8>)>,
) -> tokio::task::JoinHandle<io::Result<()>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0_u8; 8192];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            if tx.send((stream, buf[..n].to_vec())).is_err() {
                break;
            }
        }
        Ok(())
    })
}

async fn await_reader_task(
    handle: tokio::task::JoinHandle<io::Result<()>>,
    label: &str,
) -> io::Result<()> {
    handle
        .await
        .map_err(join_error_to_io)?
        .map_err(|e| io::Error::new(e.kind(), format!("failed reading child {label}: {e}")))
}

fn join_error_to_io(err: tokio::task::JoinError) -> io::Error {
    io::Error::other(format!("output forwarder task failed: {err}"))
}

#[cfg(not(windows))]
async fn run_codex_via_shell(
    codex_bin: &str,
    args: &[String],
    pinned_header: &[String],
) -> std::io::Result<std::process::ExitStatus> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let shell_name = Path::new(&shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    let mut cmd = Command::new(&shell);
    if shell_name == "zsh" || shell_name == "bash" {
        // Interactive mode makes alias/function-based codex definitions available.
        cmd.arg("-i");
    }

    cmd.arg("-lc")
        .arg("\"$0\" \"$@\"")
        .arg(codex_bin)
        .args(args);
    run_command_with_forwarded_output(cmd, Some(pinned_header.to_vec())).await
}

fn task_header_lines(
    run_idx: usize,
    total_runs: usize,
    loop_idx: usize,
    loops: usize,
    task_idx: usize,
    task_total: usize,
    prompt: &str,
) -> [String; 4] {
    [
        "=== Agent Loops ===".to_string(),
        format!(
            "Run {run_idx}/{total_runs} | Loop {}/{loops} | Task {}/{}",
            loop_idx + 1,
            task_idx + 1,
            task_total
        ),
        format!(
            "Current task: {}",
            truncate_display(prompt, MAX_CURRENT_TASK_LEN)
        ),
        "----------------------------------------".to_string(),
    ]
}

#[derive(Clone, Copy)]
enum AnsiParseState {
    Normal,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

struct PinnedOutputRenderer {
    header_lines: Vec<String>,
    output_lines: VecDeque<String>,
    current_line: String,
    ansi_state: AnsiParseState,
}

impl PinnedOutputRenderer {
    fn new(header_lines: Vec<String>) -> io::Result<Self> {
        let renderer = Self {
            header_lines,
            output_lines: VecDeque::new(),
            current_line: String::new(),
            ansi_state: AnsiParseState::Normal,
        };

        let mut out = io::stdout();
        // Hide cursor and clear screen before entering redraw mode.
        write!(out, "\x1b[?25l\x1b[2J\x1b[H")?;
        out.flush()?;

        renderer.render()?;
        Ok(renderer)
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        let mut sanitized = Vec::with_capacity(chunk.len());
        for &b in chunk {
            self.consume_byte(b, &mut sanitized);
        }
        if sanitized.is_empty() {
            return Ok(());
        }

        let text = String::from_utf8_lossy(&sanitized);
        for ch in text.chars() {
            match ch {
                '\n' => self.push_current_line(),
                _ => self.current_line.push(ch),
            }
        }

        self.render()
    }

    fn finish(&mut self) -> io::Result<()> {
        if !self.current_line.is_empty() {
            self.push_current_line();
        }
        self.render()?;

        let mut out = io::stdout();
        write!(out, "\x1b[?25h")?;
        out.flush()
    }

    fn consume_byte(&mut self, b: u8, out: &mut Vec<u8>) {
        match self.ansi_state {
            AnsiParseState::Normal => match b {
                0x1b => self.ansi_state = AnsiParseState::Esc,
                b'\r' => out.push(b'\n'),
                b'\n' | b'\t' => out.push(b),
                0x20..=0x7e | 0x80..=0xff => out.push(b),
                _ => {}
            },
            AnsiParseState::Esc => match b {
                b'[' => self.ansi_state = AnsiParseState::Csi,
                b']' => self.ansi_state = AnsiParseState::Osc,
                _ => self.ansi_state = AnsiParseState::Normal,
            },
            AnsiParseState::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    self.ansi_state = AnsiParseState::Normal;
                }
            }
            AnsiParseState::Osc => match b {
                0x07 => self.ansi_state = AnsiParseState::Normal,
                0x1b => self.ansi_state = AnsiParseState::OscEsc,
                _ => {}
            },
            AnsiParseState::OscEsc => {
                if b == b'\\' {
                    self.ansi_state = AnsiParseState::Normal;
                } else {
                    self.ansi_state = AnsiParseState::Osc;
                }
            }
        }
    }

    fn push_current_line(&mut self) {
        self.output_lines
            .push_back(std::mem::take(&mut self.current_line));
        while self.output_lines.len() > MAX_RENDERED_OUTPUT_LINES {
            self.output_lines.pop_front();
        }
    }

    fn render(&self) -> io::Result<()> {
        let rows = terminal_rows();
        let cols = terminal_cols();
        let body_rows = rows.saturating_sub(self.header_lines.len());

        let mut visible_lines: Vec<&str> = self.output_lines.iter().map(String::as_str).collect();
        if !self.current_line.is_empty() {
            visible_lines.push(self.current_line.as_str());
        }
        let start = visible_lines.len().saturating_sub(body_rows);
        let visible_tail = &visible_lines[start..];

        let mut out = io::stdout();
        write!(out, "\x1b[H")?;
        for line in &self.header_lines {
            writeln!(out, "\x1b[2K{}", fit_terminal_line(line, cols))?;
        }
        for line in visible_tail {
            writeln!(out, "\x1b[2K{}", fit_terminal_line(line, cols))?;
        }
        for _ in visible_tail.len()..body_rows {
            writeln!(out, "\x1b[2K")?;
        }
        write!(out, "\x1b[J")?;
        out.flush()
    }
}

impl Drop for PinnedOutputRenderer {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = write!(out, "\x1b[?25h");
        let _ = out.flush();
    }
}

fn fit_terminal_line(line: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }

    let total_chars = line.chars().count();
    if total_chars <= max_cols {
        return line.to_string();
    }

    if max_cols <= 3 {
        return ".".repeat(max_cols);
    }

    let mut out = String::new();
    for ch in line.chars().take(max_cols - 3) {
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn terminal_rows() -> usize {
    std::env::var("LINES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|rows| *rows > 0)
        .unwrap_or(24)
}

fn terminal_cols() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|cols| *cols > 0)
        .unwrap_or(120)
}

/// Core orchestration logic: run all prompts in order, repeating `loops` times.
/// Calls `runner` for each prompt. Returns a vec of (loop_index, task_index, success).
pub async fn orchestrate<F, Fut>(
    prompts: &[String],
    loops: usize,
    runner: F,
) -> Vec<(usize, usize, bool)>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<bool>>,
{
    let mut results = Vec::new();
    let total_runs = prompts.len() * loops;

    for loop_idx in 0..loops {
        for (task_idx, prompt) in prompts.iter().enumerate() {
            let run_idx = loop_idx * prompts.len() + task_idx + 1;
            let header = task_header_lines(
                run_idx,
                total_runs,
                loop_idx,
                loops,
                task_idx,
                prompts.len(),
                prompt,
            );
            for line in &header {
                println!("{line}");
            }
            let task_header_guard = CurrentTaskHeaderGuard::new(header.to_vec());

            let success = match runner(prompt.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Error launching codex: {e}");
                    false
                }
            };

            drop(task_header_guard);
            let status_label = if success { "OK" } else { "FAILED" };
            println!("[Run {run_idx}/{total_runs}] Result: {status_label}\n");
            results.push((loop_idx, task_idx, success));
        }
    }

    println!("=== All loops completed ===");
    results
}
