mod store;
mod runner;
mod scheduler;
mod tui;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;

#[derive(Parser)]
#[command(name = "fractal", about = "A persistent task tree executed by ephemeral agents")]
struct Cli {
    #[arg(short, long, default_value = ".", global = true)]
    project: PathBuf,
    #[arg(short, long, global = true)]
    executor: Option<String>,
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
    /// Reset a node and its children back to pending
    Retry {
        /// Node id to retry
        node_id: String,
    },
    /// Export the unified codebase from all completed nodes to a clean directory
    Export {
        /// Output directory (default: dist/)
        #[arg(short, long, default_value = "dist")]
        output: PathBuf,
    },
    /// Signal task completion with a summary (used by agents)
    Done {
        /// Summary of what was delivered/implemented
        #[arg(short, long)]
        summary: String,
        /// Node ID (defaults to current node from environment or active running node)
        #[arg(short, long)]
        node: Option<String>,
    },
    /// Split a node into subtasks (JSON passed via string, file, or stdin)
    Split {
        /// JSON array of subtasks, or path to a JSON file
        #[arg(short, long)]
        subtasks: Option<String>,
        /// Node ID (defaults to current node from environment or active running node)
        #[arg(short, long)]
        node: Option<String>,
    },
    /// Escalate an invalid constraint or assumption to the parent node
    Escalate {
        /// Assumption that proved invalid
        #[arg(short, long)]
        assumption: String,
        /// Concrete evidence or failure details
        #[arg(short, long)]
        evidence: String,
        /// Node ID (defaults to current node from environment or active running node)
        #[arg(short, long)]
        node: Option<String>,
    },
}
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use crossterm::execute;
        use crossterm::cursor::Show;
        use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
        let _ = execute!(std::io::stderr(), LeaveAlternateScreen, Show);
        let _ = disable_raw_mode();
        original(info);
    }));
}

fn main() {
    install_panic_hook();
    let cli = Cli::parse();
    let project = cli.project.canonicalize().unwrap_or_else(|_| cli.project.clone());

    if let Some(exec) = cli.executor {
        std::env::set_var("FRACTAL_EXECUTOR", exec);
    }

    match cli.command {
        Commands::Init { goal } => {
            let goal = goal.join(" ");
            if goal.trim().is_empty() {
                eprintln!("fractal: a goal is required: fractal init <goal>");
                std::process::exit(2);
            }
            let s = store::Store::new(&project);
            match s.init(&goal) {
                Ok(node) => {
                    println!("initialised tree/root ({})", node.status);
                    println!("goal: {goal}");
                    run_project(&project, &goal);
                }
                Err(e) => {
                    eprintln!("fractal: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Run => {
            let s = store::Store::new(&project);
            if let Err(e) = s.require_initialised() {
                eprintln!("fractal: {e}");
                std::process::exit(2);
            }
            let goal = s.get("root").map(|n| n.goal).unwrap_or_default();
            run_project(&project, &goal);
        }
        Commands::Status => {
            let s = store::Store::new(&project);
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
            let s = store::Store::new(&project);
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
        Commands::Retry { node_id } => {
            let s = store::Store::new(&project);
            if let Err(e) = s.require_initialised() {
                eprintln!("fractal: {e}");
                std::process::exit(2);
            }
            match s.retry(&node_id) {
                Ok(n) => {
                    println!("retried {} nodes from '{}' and below — run `fractal run -p {}` to resume",
                        n, node_id, project.display());
                }
                Err(e) => { eprintln!("fractal: {e}"); std::process::exit(1); }
            }
        }
        Commands::Export { output } => {
            let s = store::Store::new(&project);
            if let Err(e) = s.require_initialised() {
                eprintln!("fractal: {e}");
                std::process::exit(2);
            }
            let target = if output.is_absolute() { output } else { project.join(output) };
            match s.export_workspace(&target) {
                Ok(count) => {
                    println!("Successfully exported {} unified code artifacts to: {}", count, target.display());
                }
                Err(e) => { eprintln!("fractal: {e}"); std::process::exit(1); }
            }
        }
        Commands::Done { summary, node } => {
            let s = store::Store::new(&project);
            let target_node_id = node.or_else(|| std::env::var("FRACTAL_NODE_ID").ok())
                .unwrap_or_else(|| {
                    s.walk().ok().and_then(|nodes| nodes.iter().find(|n| n.status == store::RUNNING).map(|n| n.id.clone()))
                        .unwrap_or_default()
                });
            if target_node_id.is_empty() {
                eprintln!("fractal: no active or specified node found for 'done'");
                std::process::exit(1);
            }
            let decision = serde_json::json!({
                "verb": "complete",
                "summary": summary,
                "deliverable": summary
            });
            let decision_file = project.join(format!(".fractal_decision_{}", target_node_id));
            let _ = std::fs::write(&decision_file, serde_json::to_string(&decision).unwrap());
            println!("Node '{}' marked DONE. Decision recorded.", target_node_id);
        }
        Commands::Split { subtasks, node } => {
            let s = store::Store::new(&project);
            let target_node_id = node.or_else(|| std::env::var("FRACTAL_NODE_ID").ok())
                .unwrap_or_else(|| {
                    s.walk().ok().and_then(|nodes| nodes.iter().find(|n| n.status == store::RUNNING).map(|n| n.id.clone()))
                        .unwrap_or_default()
                });
            if target_node_id.is_empty() {
                eprintln!("fractal: no active or specified node found for 'split'");
                std::process::exit(1);
            }
            let raw_json = match subtasks {
                Some(s) if std::path::Path::new(&s).exists() => std::fs::read_to_string(&s).unwrap_or_default(),
                Some(s) => s,
                None => {
                    use std::io::Read;
                    let mut buffer = String::new();
                    let _ = std::io::stdin().read_to_string(&mut buffer);
                    buffer
                }
            };
            let subtasks_val: serde_json::Value = serde_json::from_str(&raw_json).unwrap_or_else(|e| {
                eprintln!("fractal: invalid JSON subtasks: {e}");
                std::process::exit(1);
            });
            let subtasks_arr = if subtasks_val.is_array() {
                subtasks_val
            } else if let Some(arr) = subtasks_val.get("subtasks") {
                arr.clone()
            } else {
                eprintln!("fractal: subtasks must be a JSON array of subtask objects");
                std::process::exit(1);
            };
            let decision = serde_json::json!({
                "verb": "split",
                "subtasks": subtasks_arr
            });
            let decision_file = project.join(format!(".fractal_decision_{}", target_node_id));
            let _ = std::fs::write(&decision_file, serde_json::to_string(&decision).unwrap());
            println!("Node '{}' marked SPLIT into {} subtasks.", target_node_id, subtasks_arr.as_array().map(|a| a.len()).unwrap_or(0));
        }
        Commands::Escalate { assumption, evidence, node } => {
            let s = store::Store::new(&project);
            let target_node_id = node.or_else(|| std::env::var("FRACTAL_NODE_ID").ok())
                .unwrap_or_else(|| {
                    s.walk().ok().and_then(|nodes| nodes.iter().find(|n| n.status == store::RUNNING).map(|n| n.id.clone()))
                        .unwrap_or_default()
                });
            if target_node_id.is_empty() {
                eprintln!("fractal: no active or specified node found for 'escalate'");
                std::process::exit(1);
            }
            let decision = serde_json::json!({
                "verb": "escalate",
                "assumption": assumption,
                "evidence": evidence
            });
            let decision_file = project.join(format!(".fractal_decision_{}", target_node_id));
            let _ = std::fs::write(&decision_file, serde_json::to_string(&decision).unwrap());
            println!("Node '{}' recorded ESCALATION.", target_node_id);
        }
    }
}

fn pick_model() -> String {
    let default = "default";
    if let Ok(m) = std::env::var("FRACTAL_MODEL") { if !m.is_empty() { return m; } }

    let models = list_models(default);
    if models.is_empty() { return default.to_string(); }

    let default_idx = models.iter().position(|m| m == default).unwrap_or(0);

    use crossterm::{
        cursor::{Hide, Show},
        event::{self, Event, KeyCode, KeyEventKind},
        execute,
        terminal::{Clear, ClearType},
    };

    let mut stdout = std::io::stdout();
    if crossterm::terminal::enable_raw_mode().is_err() {
        eprintln!("Model [{default}]: (non-TTY, using default)");
        return default.to_string();
    }
    let _ = execute!(stdout, Hide);

    let term_h = crossterm::terminal::size().unwrap_or((80, 40)).1 as usize;
    let view_h = term_h.saturating_sub(4);
    let mut selected = default_idx;
    let mut scroll = selected.saturating_sub(view_h / 2);
    let header = format!(" Choose model (↑↓ move, enter/space confirm, q quit)  [{} models] ", models.len());

    draw_picker_view(&mut stdout, &header, &models, selected, scroll, view_h, None);

    loop {
        if let Ok(Event::Key(key)) = event::read() {
            if key.kind != KeyEventKind::Press { continue; }
            let old = selected;
            let mut redraw = false;
            match key.code {
                KeyCode::Up if selected > 0 => { selected -= 1; }
                KeyCode::Down if selected + 1 < models.len() => { selected += 1; }
                KeyCode::PageUp => { selected = selected.saturating_sub(view_h); }
                KeyCode::PageDown => { selected = (selected + view_h).min(models.len() - 1); }
                KeyCode::Home => { selected = 0; }
                KeyCode::End => { selected = models.len() - 1; }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    let _ = execute!(stdout, Clear(ClearType::All), Show);
                    let _ = crossterm::terminal::disable_raw_mode();
                    return models[selected].clone();
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    let _ = execute!(stdout, Clear(ClearType::All), Show);
                    let _ = crossterm::terminal::disable_raw_mode();
                    return models[default_idx].clone();
                }
                _ => {}
            }
            if selected < scroll { scroll = selected; redraw = true; }
            if selected >= scroll + view_h { scroll = selected.saturating_sub(view_h - 1); redraw = true; }
            let old_scroll = if old >= scroll && old < scroll + view_h { Some(old) } else { None };
            let new_scroll = if selected >= scroll && selected < scroll + view_h { Some(selected) } else { None };
            if new_scroll != old_scroll { redraw = true; }

            if selected != old || redraw {
                if redraw || old_scroll.is_none() || new_scroll.is_none() {
                    draw_picker_view(&mut stdout, &header, &models, selected, scroll, view_h, Some(old));
                } else {
                    patch_picker_lines(&mut stdout, &models, old, selected, scroll, view_h);
                }
            }
        }
    }
}

fn draw_picker_view(stdout: &mut std::io::Stdout, header: &str, models: &[String], selected: usize, scroll: usize, view_h: usize, old_scroll_off: Option<usize>) {
    use crossterm::{
        execute,
        style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor, ResetColor},
        terminal::{Clear, ClearType},
    };
    let redraw_header = old_scroll_off.map_or(true, |os| os < scroll || os >= scroll + view_h);
    if redraw_header {
        let _ = execute!(stdout, Clear(ClearType::All),
            SetForegroundColor(Color::Cyan), SetAttribute(Attribute::Bold),
            Print(header), ResetColor, SetAttribute(Attribute::Reset), Print("\n\n"));
    }
    let end = (scroll + view_h).min(models.len());
    for i in scroll..end {
        let marker = if i == selected { ">" } else { " " };
        let _ = execute!(stdout, Clear(ClearType::CurrentLine));
        if i == selected {
            let _ = execute!(stdout, SetForegroundColor(Color::White), SetBackgroundColor(Color::DarkGrey), Print(format!("{marker} {}", models[i])), ResetColor, Print("\r\n"));
        } else {
            let _ = execute!(stdout, SetForegroundColor(Color::White), Print(format!("{marker} {}", models[i])), ResetColor, Print("\r\n"));
        }
    }
    let _ = std::io::Write::flush(stdout);
}

fn patch_picker_lines(stdout: &mut std::io::Stdout, models: &[String], old: usize, selected: usize, scroll: usize, view_h: usize) {
    use crossterm::{cursor::MoveTo, execute, style::{Color, Print, SetBackgroundColor, SetForegroundColor, ResetColor}, terminal::{Clear, ClearType}};
    let line_row = |idx: usize| (idx.wrapping_sub(scroll) + 2) as u16;
    let end = (scroll + view_h).min(models.len());
    if old >= scroll && old < end {
        let _ = execute!(stdout, MoveTo(0, line_row(old)), Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::White), Print(format!("  {}", models[old])), ResetColor, Print("\r\n"));
    }
    if selected >= scroll && selected < end {
        let _ = execute!(stdout, MoveTo(0, line_row(selected)), Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::White), SetBackgroundColor(Color::DarkGrey), Print(format!("> {}", models[selected])), ResetColor, Print("\r\n"));
    }
    let _ = std::io::Write::flush(stdout);
}

fn list_models(default: &str) -> Vec<String> {
    let executor = runner::get_executor();
    if executor == "opencode" {
        let bin = which::which("opencode").unwrap_or_else(|_| std::path::Path::new("opencode").to_path_buf());
        if let Ok(o) = std::process::Command::new(&bin).args(["models"]).output() {
            if o.status.success() {
                let text = String::from_utf8_lossy(&o.stdout);
                let models: Vec<String> = text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                if !models.is_empty() { return models; }
            }
        }
    }
    vec![
        default.to_string(),
        "smol".into(),
        "slow".into(),
        "openrouter/google/gemini-3.7-flash".into(),
        "anthropic/claude-3-7-sonnet".into(),
        "openai/gpt-4o-mini".into(),
    ]
}

fn run_scheduler(project: &PathBuf, state: &std::sync::Arc<std::sync::Mutex<tui::TuiState>>, model: &str) {
    let s = store::Store::new(project);
    match scheduler::run(&s, state, model) {
        Ok(report) => {
            let mut s = state.lock().unwrap();
            s.done = true;
            if !report.ok() {
                s.status_line = format!("failed — root: {}", report.root_status);
            }
        }
        Err(e) => {
            let mut s = state.lock().unwrap();
            s.done = true;
            s.error = Some(format!("{}", e));
            s.status_line = format!("error: {}", e);
        }
    }
}

fn run_project(project: &PathBuf, goal: &str) {
    let model = pick_model();

    let _ = ctrlc::set_handler(move || {
        scheduler::INTERRUPTED.store(true, Ordering::SeqCst);
    });

    let state = std::sync::Arc::new(std::sync::Mutex::new(tui::TuiState {
        nodes: vec![],
        stats: tui::StatsSnapshot { steps: 0, completed: 0, split: 0, failed: 0, refused: 0, refused_goals: vec![], failed_goals: vec![] },
        log_lines: vec![],
        status_line: String::new(),
        node_id: "root".into(),
        node_goal: goal.to_string(),
        node_started_at: std::time::Instant::now(),
        last_activity: String::new(),
        node_activities: std::collections::HashMap::new(),
        done: false,
        error: None,
        model: model.clone(),
        selected_idx: 0,
        mode: tui::TuiMode::Normal,
        prompt_message: None,
        inspect_scroll: 0,
    }));

    let project_path = project.clone();
    let s = store::Store::new(project);

    if let Ok(mut tui) = tui::Tui::with_state(goal, &model, state.clone()) {
        let state2 = state.clone();
        thread::spawn(move || run_scheduler(&project_path, &state2, &model));
        match tui.run(&s) {
            Ok(()) => {}
            Err(e) => eprintln!("\nfractal: TUI error: {e}"),
        }
    } else {
        eprintln!("(no TUI — running headless)");
        run_scheduler(&project_path, &state, &model);
    }

    let final_state = state.lock().unwrap();
    if let Some(ref err) = final_state.error {
        eprintln!("\n  fractal: {err}");
    }
    let nodes = match s.walk() {
        Ok(n) => n,
        Err(e) => { eprintln!("\n  fractal: {e}"); std::process::exit(1); }
    };
    let root_status = nodes.first().map(|n| n.status.as_str()).unwrap_or("pending");
    let model = final_state.model.clone();
    drop(final_state);

    let summary = if root_status == store::COMPLETE {
        generate_summary(project, &s, &nodes, &model)
    } else {
        String::new()
    };

    let completed = nodes.iter().filter(|n| n.status == store::COMPLETE).count();
    let failed = nodes.iter().filter(|n| n.status == store::FAILED).count();
    let pending = nodes.iter().filter(|n| n.status == "pending" || n.status == store::RUNNING).count();

    eprintln!("\n Summary ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    if let Some(root) = nodes.first() {
        let g = root.goal.lines().next().unwrap_or(&root.goal);
        let g = if g.len() > 68 { format!("{}…", &g[..67]) } else { g.to_string() };
        eprintln!("  goal:    {g}");
    }
    if !summary.is_empty() {
        for line in summary.lines() {
            eprintln!("  {line}");
        }
    }
    eprintln!("  result:  root {root_status}");
    eprintln!("  {completed} completed · {failed} failed · {pending} pending · {} total",
        nodes.len());
    eprintln!("  artifacts: {}/artifacts/", project.join("tree/root").display());
    eprintln!("  trace:     {}/trace.json", project.display());
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    let code = if root_status == store::COMPLETE { 0 } else { 1 };
    std::process::exit(code);
}

fn generate_summary(project: &PathBuf, _store: &store::Store, nodes: &[store::Node], model: &str) -> String {
    let mut ctx = String::from("You are generating a final project summary. Below is a task tree with node goals, statuses, and what each completed node delivered.\n\n");
    if let Some(root) = nodes.first() {
        ctx.push_str(&format!("## Project goal\n{}\n\n", root.goal));
    }
    ctx.push_str("## Nodes\n\n");
    for n in nodes {
        let indent = "  ".repeat((n.depth - 1) as usize);
        let g = n.goal.lines().next().unwrap_or(&n.goal);
        let g = if g.len() > 72 { format!("{}…", &g[..71]) } else { g.to_string() };
        ctx.push_str(&format!("{indent}[{id}] [{status}] {goal}\n", indent = indent, id = n.id, status = n.status, goal = g));
        if n.status == store::COMPLETE {
            if let Ok(decisions) = std::fs::read_to_string(n.decisions_path()) {
                for line in decisions.lines().rev().take(2) {
                    if line.starts_with('-') && line.contains("completed") {
                        ctx.push_str(&format!("{indent}  → {}\n", line.trim_start_matches("- ")));
                        break;
                    }
                }
            }
        }
    }
    ctx.push_str("\n## Instructions\nWrite a clear, structured summary:\n1. Executive Summary: What was accomplished and built.\n2. Key Artifacts: List created files and build outputs (e.g. dist/chrome, dist/firefox).\n3. How to Install & Test: Step-by-step instructions for the developer to run, test, and use the project right now.\n");
    let executor = runner::get_executor();
    if executor == "opencode" {
        let bin = which::which("opencode").unwrap_or_else(|_| std::path::Path::new("opencode").to_path_buf());
        let iso_home = std::env::temp_dir().join(format!("fractal-summary-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&iso_home);
        let real_home = std::env::var("HOME").unwrap_or_default();
        let cmd_line = format!("cd '{}' && '{}' run --auto --model '{}' '{}'",
            project.display(),
            bin.display(),
            model,
            ctx.replace('\'', "'\\''"),
        );

        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &cmd_line])
            .env("HOME", &iso_home)
            .env("XDG_CONFIG_HOME", format!("{real_home}/.config"))
            .env("XDG_DATA_HOME", format!("{real_home}/.local/share"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();

        let result = match output {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
            Err(_) => String::new(),
        };
        let _ = std::fs::remove_dir_all(&iso_home);
        result
    } else {
        let bin = which::which("omp")
            .or_else(|_| which::which("pi"))
            .unwrap_or_else(|_| PathBuf::from("omp"));
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("-p");
        cmd.arg("--cwd").arg(project);
        if !model.is_empty() && model != "default" {
            cmd.arg(format!("--model={model}"));
        }
        cmd.arg(&ctx);
        cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        let output = cmd.output();
        match output {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
            Err(_) => String::new(),
        }
    }
}
