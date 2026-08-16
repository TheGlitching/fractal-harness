use crate::store::Node;
use crate::scheduler::RunReport;
use std::io::{self, Write};

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BLUE: &str = "\x1b[34m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";

/// Render the full tree animation to stderr.
pub fn render(report: &RunReport, nodes: &[Node]) {
    let _ = write!(io::stderr(), "\x1b[2J\x1b[H"); // clear screen
    let _ = writeln!(io::stderr(), "{BOLD}{BLUE}┌─ fractal ──────────────────────────────{RESET}");

    // Tree
    let root = nodes.iter().find(|n| n.parent.is_none());
    if let Some(r) = root {
        print_tree(r, nodes, "", true);
    }

    let _ = writeln!(io::stderr(), "{BOLD}{BLUE}├─────────────────────────────────────{RESET}");
    // Stats
    let _ = writeln!(io::stderr(),
        "{BOLD}  steps: {steps}  {GREEN}✓{RESET}{GREEN}{completed}{RESET}  {YELLOW}◇{RESET}{split}  {RED}✗{RESET}{failed}  ⤴{escalations}  verify: {v_ok}/{v_total}{RESET}",
        steps = report.steps, completed = report.completed, split = report.split,
        failed = report.failed, escalations = report.escalations,
        v_ok = report.verifications - report.verify_failures, v_total = report.verifications,
    );

    // Running spinners
    for n in nodes {
        if n.status == "running" {
            let spin = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"][report.steps % 10];
            let goal = if n.goal.len() > 50 { &n.goal[..47] } else { &n.goal };
            let _ = writeln!(io::stderr(), "  {CYAN}{spin}{RESET} {BOLD}{goal}...{RESET}{DIM}  [{id}]{RESET}", id = n.id);
        }
    }

    let _ = writeln!(io::stderr(), "{BOLD}{BLUE}└─────────────────────────────────────{RESET}");
    let _ = io::stderr().flush();
}

fn print_tree(node: &Node, nodes: &[Node], prefix: &str, is_last: bool) {
    let _ = writeln!(io::stderr(), "{}", tree_line(node, prefix, is_last));
    let children: Vec<&Node> = nodes.iter().filter(|n| n.parent.as_deref() == Some(&node.id)).collect();
    for (i, child) in children.iter().enumerate() {
        let child_prefix = format!("{prefix}{}", if is_last { "   " } else { "│  " });
        print_tree(child, nodes, &child_prefix, i == children.len() - 1);
    }
}

fn tree_line(node: &Node, prefix: &str, is_last: bool) -> String {
    let branch = if node.parent.is_none() { "" } else if is_last { "└─ " } else { "├─ " };
    let icon = match node.status.as_str() {
        "complete" => format!("{GREEN}●{RESET}"),
        "running" => format!("{CYAN}◉{RESET}"),
        "failed" => format!("{RED}✗{RESET}"),
        "split" => format!("{YELLOW}◇{RESET}"),
        "suspended" => format!("{DIM}⊗{RESET}"),
        _ => format!("{DIM}○{RESET}"),
    };
    let goal = format_goal(&node.goal, 50);
    format!("{prefix}{branch}{icon} {BOLD}{id}{RESET}{DIM}  {goal}{RESET}", id = node.id)
}

fn format_goal(goal: &str, max: usize) -> String {
    let first_line = goal.lines().next().unwrap_or(goal);
    let s = first_line.replace('\n', " ").trim().to_string();
    if s.len() > max { format!("{}…", &s[..max - 1]) } else { s }
}
