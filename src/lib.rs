use std::path::Path;
use tokio::process::Command;

/// Maximum display length for a single task description in the summary.
pub const MAX_DISPLAY_LEN: usize = 60;

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
    println!("=== Agent Loops Execution Plan ===");
    if let Some(dir) = work_dir {
        println!("Working directory: {dir}");
    }
    println!("Loop count: {loops}");
    println!("Tasks ({}):", prompts.len());
    for (i, prompt) in prompts.iter().enumerate() {
        println!("  {}. {}", i + 1, truncate_display(prompt, MAX_DISPLAY_LEN));
    }
    println!();
    println!(
        "Total executions: {} ({} task(s) x {} loop(s))",
        prompts.len() * loops,
        prompts.len(),
        loops,
    );
    println!("==================================\n");
}

/// Run a single codex conversation with the given prompt.
/// Uses `codex exec --dangerously-bypass-approvals-and-sandbox` for full access.
/// If `work_dir` is provided, passes `-C <dir>` to codex to set its working directory.
/// Returns `Ok(true)` on success, `Ok(false)` on non-zero exit.
pub async fn run_codex(prompt: &str, work_dir: Option<&Path>) -> std::io::Result<bool> {
    let mut args: Vec<&str> = vec!["exec", "--dangerously-bypass-approvals-and-sandbox"];
    let dir_string;
    if let Some(dir) = work_dir {
        dir_string = dir.to_string_lossy().to_string();
        args.extend(["-C", dir_string.as_str()]);
    }
    args.push(prompt);

    let status = if cfg!(windows) {
        let mut cmd_args = vec!["/C", "codex"];
        cmd_args.extend(&args);
        Command::new("cmd").args(&cmd_args).status().await?
    } else {
        Command::new("codex").args(&args).status().await?
    };
    Ok(status.success())
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

    for loop_idx in 0..loops {
        println!("--- Loop {}/{} ---", loop_idx + 1, loops);
        for (task_idx, prompt) in prompts.iter().enumerate() {
            println!(
                "[Loop {}/{}, Task {}/{}] Running: {}",
                loop_idx + 1,
                loops,
                task_idx + 1,
                prompts.len(),
                truncate_display(prompt, MAX_DISPLAY_LEN),
            );

            let success = match runner(prompt.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("  Error launching codex: {e}");
                    false
                }
            };

            let status_label = if success { "OK" } else { "FAILED" };
            println!("  Result: {status_label}\n");
            results.push((loop_idx, task_idx, success));
        }
    }

    println!("=== All loops completed ===");
    results
}
