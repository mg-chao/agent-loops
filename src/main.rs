use agent_loops::{orchestrate, print_plan, run_codex};
use clap::Parser;
use std::path::Path;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(name = "agent-loops", about = "Orchestrate codex CLI tasks with cyclic execution")]
struct Cli {
    /// Prompts to execute sequentially, each in its own codex conversation.
    #[arg(short, long, required = true, num_args = 1..)]
    prompts: Vec<String>,

    /// Number of times to loop through the full prompt list.
    #[arg(short, long, default_value_t = 1)]
    loops: usize,

    /// Working directory for codex to operate in.
    #[arg(short = 'C', long = "cd")]
    work_dir: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.loops == 0 {
        println!("Loop count is 0 — nothing to do.");
        return ExitCode::SUCCESS;
    }

    if cli.prompts.is_empty() {
        println!("No prompts provided — nothing to do.");
        return ExitCode::SUCCESS;
    }

    print_plan(&cli.prompts, cli.loops, cli.work_dir.as_deref());

    let work_dir = cli.work_dir.clone();
    let results = orchestrate(&cli.prompts, cli.loops, |prompt| {
        let dir = work_dir.clone();
        async move {
            run_codex(&prompt, dir.as_deref().map(Path::new)).await
        }
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
