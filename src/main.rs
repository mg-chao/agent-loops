use agent_loops::{orchestrate, print_plan, run_codex};
use clap::Parser;
use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(name = "agent-loops", about = "Orchestrate codex CLI tasks with cyclic execution")]
struct Cli {
    /// Prompts to execute sequentially, each in its own codex conversation.
    #[arg(short, long, num_args = 1.., required_unless_present = "prompts_file")]
    prompts: Vec<String>,

    /// Load prompts from a UTF-8 text file, one prompt per non-empty line.
    #[arg(long = "prompts-file", value_name = "FILE")]
    prompts_file: Option<String>,

    /// Number of times to loop through the full prompt list.
    #[arg(short, long, default_value_t = 1)]
    loops: usize,

    /// Working directory for codex to operate in.
    #[arg(short = 'C', long = "cd")]
    work_dir: Option<String>,

    /// Codex executable path or command name. Defaults to `codex`.
    #[arg(long = "codex-bin")]
    codex_bin: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut prompts = cli.prompts.clone();

    if let Some(prompts_file) = cli.prompts_file.as_deref() {
        match load_prompts_file(Path::new(prompts_file)) {
            Ok(mut file_prompts) => prompts.append(&mut file_prompts),
            Err(e) => {
                eprintln!("Failed to read prompts file `{prompts_file}`: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if cli.loops == 0 {
        println!("Loop count is 0 — nothing to do.");
        return ExitCode::SUCCESS;
    }

    if prompts.is_empty() {
        println!("No prompts provided — nothing to do.");
        return ExitCode::SUCCESS;
    }

    if let Some(dir) = cli.work_dir.as_deref() {
        let path = Path::new(dir);
        if !path.exists() {
            eprintln!("Working directory does not exist: {dir}");
            return ExitCode::FAILURE;
        }
        if !path.is_dir() {
            eprintln!("Working directory is not a directory: {dir}");
            return ExitCode::FAILURE;
        }
    }

    print_plan(&prompts, cli.loops, cli.work_dir.as_deref());

    let work_dir = cli.work_dir.clone();
    let codex_bin = cli
        .codex_bin
        .clone()
        .or_else(|| std::env::var("AGENT_LOOPS_CODEX_BIN").ok())
        .unwrap_or_else(|| "codex".to_string());
    let results = orchestrate(&prompts, cli.loops, |prompt| {
        let dir = work_dir.clone();
        let codex_bin = codex_bin.clone();
        async move { run_codex(&prompt, dir.as_deref().map(Path::new), &codex_bin).await }
    })
    .await;

    let failures: Vec<_> = results.iter().filter(|(_, _, ok)| !ok).collect();
    if failures.is_empty() {
        println!("All tasks completed successfully.");
        ExitCode::SUCCESS
    } else {
        eprintln!("{} task(s) failed.", failures.len());
        ExitCode::FAILURE
    }
}

fn load_prompts_file(path: &Path) -> io::Result<Vec<String>> {
    let content = fs::read_to_string(path)?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}
