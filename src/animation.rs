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

pub fn render(report: &RunReport, nodes: &[Node]) {
    let utf8 = is_utf8();

    let _ = write!(io::stderr(), "\x1b[2J\x1b[H");
    let top = if utf8 { "┌─ fractal ──────────────────────────────" } else { "+- fractal ------------------------------" };
    let mid = if utf8 { "├─────────────────────────────────────" } else { "+-------------------------------------" };
    let bot = if utf8 { "└─────────────────────────────────────" } else { "+-------------------------------------" };

    let _ = writeln!(io::stderr(), "{BOLD}{BLUE}{top}{RESET}");

    let root = nodes.iter().find(|n| n.parent.is_none());
    if let Some(r) = root {
        print_tree(r, nodes, "", true, utf8);
    }

    let _ = writeln!(io::stderr(), "{BOLD}{BLUE}{mid}{RESET}");
    let _ = writeln!(io::stderr(),
        "{BOLD}  steps: {steps}  {GREEN}*{RESET}{completed}  {YELLOW}+{RESET}{split}  {RED}X{RESET}{failed}  ^{escalations}  verify: {v_ok}/{v_total}{RESET}",
        steps = report.steps, completed = report.completed, split = report.split,
        failed = report.failed, escalations = report.escalations,
        v_ok = report.verifications - report.verify_failures, v_total = report.verifications,
    );

    for n in nodes {
        if n.status == "running" {
            let spin = if utf8 {
                ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"][report.steps % 10]
            } else {
                ["|","/","-","\\","|","/","-","\\","|","/"][report.steps % 10]
            };
            let goal = if n.goal.len() > 50 { &n.goal[..47] } else { &n.goal };
            let _ = writeln!(io::stderr(), "  {CYAN}{spin}{RESET} {BOLD}{goal}...{RESET}{DIM}  [{id}]{RESET}", id = n.id);
        }
    }

    let _ = writeln!(io::stderr(), "{BOLD}{BLUE}{bot}{RESET}");
    let _ = io::stderr().flush();
}

fn is_utf8() -> bool {
    std::env::var("LC_ALL").map(|v| v.contains("UTF") || v.contains("utf")).unwrap_or(false)
        || std::env::var("LANG").map(|v| v.contains("UTF") || v.contains("utf")).unwrap_or(false)
        || std::env::var("LC_CTYPE").map(|v| v.contains("UTF") || v.contains("utf")).unwrap_or(false)
}

fn print_tree(node: &Node, nodes: &[Node], prefix: &str, is_last: bool, utf8: bool) {
    let _ = writeln!(io::stderr(), "{}", tree_line(node, prefix, is_last, utf8));
    let children: Vec<&Node> = nodes.iter().filter(|n| n.parent.as_deref() == Some(&node.id)).collect();
    for (i, child) in children.iter().enumerate() {
        let sep = if utf8 { if is_last { "   " } else { "│  " } } else { "   " };
        let child_prefix = format!("{prefix}{sep}");
        print_tree(child, nodes, &child_prefix, i == children.len() - 1, utf8);
    }
}

fn tree_line(node: &Node, prefix: &str, is_last: bool, utf8: bool) -> String {
    let branch = if node.parent.is_none() { "" }
        else if is_last { if utf8 { "`- " } else { "`- " } }
        else { if utf8 { "|- " } else { "|- " } };
    let (complete, running, failed, split, suspended, pending) = if utf8 {
        ("●", "◉", "✗", "◇", "⊗", "○")
    } else {
        ("*", "@", "X", "+", "-", ".")
    };
    let icon = match node.status.as_str() {
        "complete" => format!("{GREEN}{complete}{RESET}"),
        "running" => format!("{CYAN}{running}{RESET}"),
        "failed" => format!("{RED}{failed}{RESET}"),
        "split" => format!("{YELLOW}{split}{RESET}"),
        "suspended" => format!("{DIM}{suspended}{RESET}"),
        _ => format!("{DIM}{pending}{RESET}"),
    };
    let goal = format_goal(&node.goal, 50);
    format!("{prefix}{branch}{icon} {BOLD}{id}{RESET}{DIM}  {goal}{RESET}", id = node.id)
}

fn format_goal(goal: &str, max: usize) -> String {
    let first_line = goal.lines().next().unwrap_or(goal);
    let s = first_line.replace('\n', " ").trim().to_string();
    if s.len() > max { format!("{}...", &s[..max - 1]) } else { s }
}
