mod store;
mod runner;
mod scheduler;
mod tui;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

#[derive(Parser)]
#[command(name = "fractal", about = "A persistent task tree executed by ephemeral agents")]
struct Cli {
    #[arg(short, long, default_value = ".")]
    project: PathBuf,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a project and run it immediately
    Init {
        /// The goal to achieve
        goal: Vec<String>,
    },
    /// Resume a paused or incomplete project
    Run,
    /// Print the tree with statuses
    Status,
    /// Generate a digest (done / blocked / next)
    Digest,
}

fn main() {
    let cli = Cli::parse();
    let project = cli.project.canonicalize().unwrap_or_else(|_| cli.project.clone());
    let s = store::Store::new(&project);

    match cli.command {
        Commands::Init { goal } => {
            let goal = goal.join(" ");
            if goal.trim().is_empty() {
                eprintln!("fractal: a goal is required: fractal init <goal>");
                std::process::exit(2);
            }
            match s.init(&goal) {
                Ok(node) => {
                    println!("initialised tree/root ({})", node.status);
                    println!("goal: {goal}");
                    run_project(&s, &project);
                }
                Err(e) => {
                    eprintln!("fractal: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Run => {
            if let Err(e) = s.require_initialised() {
                eprintln!("fractal: {e}");
                std::process::exit(2);
            }
            run_project(&s, &project);
        }
        Commands::Status => {
            if let Err(e) = s.require_initialised() {
                eprintln!("fractal: {e}");
                std::process::exit(2);
            }
            let _ = s.reconcile();
            match s.walk() {
                Ok(nodes) => {
                    for n in &nodes {
                        let indent = "  ".repeat((n.depth - 1) as usize);
                        let goal = n.goal.lines().next().unwrap_or(&n.goal);
                        let g = if goal.len() > 68 { format!("{}…", &goal[..67]) } else { goal.to_string() };
                        println!("{indent}{}  [{}]  {g}", n.id, n.status);
                    }
                }
                Err(e) => { eprintln!("fractal: {e}"); std::process::exit(1); }
            }
        }
        Commands::Digest => {
            if let Err(e) = s.require_initialised() {
                eprintln!("fractal: {e}");
                std::process::exit(2);
            }
            match s.generate_digest() {
                Ok(text) => {
                    let path = project.join("digest.md");
                    let _ = std::fs::write(&path, &text);
                    println!("{text}");
                    println!("written: {}", path.display());
                }
                Err(e) => { eprintln!("fractal: {e}"); std::process::exit(1); }
            }
        }
    }
}

fn run_project(store: &store::Store, project: &PathBuf) {
    let _ = ctrlc::set_handler(move || {
        scheduler::INTERRUPTED.store(true, Ordering::SeqCst);
    });

    let root = store.get("root").unwrap();
    let goal = root.goal.clone();
    let mut t = tui::Tui::new("root", &goal).unwrap();
    let result = scheduler::run(store, &mut t);
    let _ = t.close();

    match result {
        Ok(report) => {
            eprintln!();
            eprintln!("{} steps · {} completed · {} split · {} failed · verify {}/{}",
                report.steps, report.completed, report.split, report.failed,
                report.verifications - report.verify_failures, report.verifications);
            let trace = project.join("trace.json");
            report.write_trace(&trace.to_string_lossy());
            std::process::exit(if report.ok() { 0 } else { 1 });
        }
        Err(e) => {
            eprintln!("\n  fractal: {e}");
            std::process::exit(1);
        }
    }
}
