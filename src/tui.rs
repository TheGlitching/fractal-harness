use crate::store::Node;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH_MS: u64 = 150;
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const ICONS: &[(&str, &str)] = &[
    ("complete", "●"),
    ("running", "◉"),
    ("split", "◇"),
    ("failed", "✗"),
    ("suspended", "⊗"),
    ("pending", "○"),
];

fn status_icon(status: &str) -> &'static str {
    for (s, icon) in ICONS { if *s == status { return icon; } }
    "?"
}

fn status_color(status: &str) -> Color {
    match status {
        "complete" => Color::Green,
        "running" => Color::LightYellow,
        "split" => Color::LightBlue,
        "failed" => Color::Red,
        "suspended" => Color::Magenta,
        _ => Color::DarkGray,
    }
}

pub struct StatsSnapshot {
    pub steps: usize,
    pub completed: usize,
    pub split: usize,
    pub failed: usize,
    pub refused: usize,
    pub refused_goals: Vec<String>,
    pub failed_goals: Vec<String>,
}

pub struct TuiState {
    pub nodes: Vec<Node>,
    pub stats: StatsSnapshot,
    pub log_lines: Vec<String>,
    pub status_line: String,
    pub node_id: String,
    pub node_goal: String,
    pub node_started_at: Instant,
    pub last_activity: String,
    pub node_activities: HashMap<String, String>,
    pub done: bool,
    pub error: Option<String>,
    pub model: String,
}

pub struct Tui {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
    pub state: Arc<Mutex<TuiState>>,
    started_at: Instant,
}

impl Tui {
    pub fn with_state(_goal: &str, _model: &str, state: Arc<Mutex<TuiState>>) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stderr = io::stderr();
        execute!(stderr, EnterAlternateScreen, crossterm::cursor::Hide)?;

        let backend = CrosstermBackend::new(stderr);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;

        Ok(Tui { terminal, state, started_at: Instant::now() })
    }

    pub fn run(&mut self) -> io::Result<()> {
        let tick = Duration::from_millis(REFRESH_MS);

        loop {
            self.terminal.draw(|f| Self::render(f, &self.state.lock().unwrap(), self.started_at))?;

            if event::poll(tick)? {
                if let Event::Key(key) = event::read()? {
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        break;
                    }
                    if key.code == KeyCode::Char('q') { break; }
                }
            }

            let done = self.state.lock().unwrap().done;
            if !done { continue; }

            // Done — keep drawing until user presses q
            self.terminal.draw(|f| Self::render(f, &self.state.lock().unwrap(), self.started_at))?;
            loop {
                if event::poll(tick)? {
                    if let Event::Key(key) = event::read()? {
                        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                            return self.cleanup();
                        }
                        if key.code == KeyCode::Char('q') {
                            return self.cleanup();
                        }
                    }
                }
                self.terminal.draw(|f| Self::render(f, &self.state.lock().unwrap(), self.started_at))?;
            }
        }

        self.cleanup()
    }

    pub fn cleanup(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(io::stderr(), LeaveAlternateScreen, crossterm::cursor::Show)
    }

    fn render(frame: &mut Frame, state: &TuiState, started_at: Instant) {
        let root_goal = if state.node_id == "root" {
            truncate(&state.node_goal, 70)
        } else {
            state.node_goal.clone()
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(2),
            ])
            .split(frame.area());

        // Header
        let header = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let header_body = vec![
            Line::from(vec![
                Span::styled(" model: ", Style::default().fg(Color::DarkGray)),
                Span::styled(&state.model, Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  node: {}", state.node_id), Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(Span::styled(format!(" goal: {}", truncate(&root_goal, 72)), Style::default().fg(Color::White))),
        ];
        let header_text = Paragraph::new(header_body).block(header);
        frame.render_widget(header_text, chunks[0]);

        // Main area: tree + stats
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(chunks[1]);

        // Tree panel
        let tree_block = Block::default()
            .borders(Borders::ALL)
            .title(" Tree ")
            .border_style(Style::default().fg(Color::DarkGray));
        let spinner_idx = (started_at.elapsed().as_millis() as usize / 150) % SPINNER.len();
        let tree_lines = build_tree_lines(state, spinner_idx);
        let tree_list = List::new(tree_lines).block(tree_block);
        frame.render_widget(tree_list, main_chunks[0]);

        // Stats panel
        let elapsed = started_at.elapsed().as_secs();
        let mins = elapsed / 60;
        let secs = elapsed % 60;
        let time_str = if mins > 0 { format!("{}m {}s", mins, secs) } else { format!("{}s", secs) };
        let mut stats_lines: Vec<Line> = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Steps:     ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{}", state.stats.steps), Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("  Completed: ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{}", state.stats.completed), Style::default().fg(Color::Green)),
            ]),
            Line::from(vec![
                Span::styled("  Split:     ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{}", state.stats.split), Style::default().fg(Color::LightBlue)),
            ]),
            Line::from(vec![
                Span::styled("  Failed:    ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{}", state.stats.failed), Style::default().fg(Color::Red)),
            ]),
            Line::from(vec![
                Span::styled("  Refused:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{}", state.stats.refused), Style::default().fg(Color::Yellow)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Elapsed:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(time_str, Style::default().fg(Color::White)),
            ]),
            Line::from(""),
        ];

        if !state.stats.failed_goals.is_empty() {
            stats_lines.push(Line::from(vec![
                Span::styled("  Failed:", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            ]));
            for g in &state.stats.failed_goals {
                stats_lines.push(Line::from(vec![
                    Span::styled(format!("    ✗ {}", truncate(g, 25)), Style::default().fg(Color::Red)),
                ]));
            }
            stats_lines.push(Line::from(""));
        }
        if !state.stats.refused_goals.is_empty() {
            stats_lines.push(Line::from(vec![
                Span::styled("  Refused:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            ]));
            for g in &state.stats.refused_goals {
                stats_lines.push(Line::from(vec![
                    Span::styled(format!("    ⊘ {}", truncate(g, 25)), Style::default().fg(Color::Yellow)),
                ]));
            }
        }

        let stats_block = Block::default()
            .borders(Borders::ALL)
            .title(" Progress ")
            .border_style(Style::default().fg(Color::DarkGray));
        let stats_p = Paragraph::new(Text::from(stats_lines)).block(stats_block);
        frame.render_widget(stats_p, main_chunks[1]);

        // Status bar
        let spun = SPINNER[(started_at.elapsed().as_millis() as usize / 150) % SPINNER.len()];
        let status_text = if state.done {
            if let Some(ref err) = state.error {
                Line::from(vec![
                    Span::styled(" ✗ ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::styled(err.as_str(), Style::default().fg(Color::Red)),
                ])
            } else {
                Line::from(vec![
                    Span::styled(" ✓ ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                    Span::styled("Done", Style::default().fg(Color::Green)),
                    Span::styled(" — press q to exit", Style::default().fg(Color::DarkGray)),
                ])
            }
        } else {
            let node_elapsed = state.node_started_at.elapsed().as_secs();
            let node_time = if node_elapsed >= 60 {
                format!("{}m{}s", node_elapsed / 60, node_elapsed % 60)
            } else {
                format!("{}s", node_elapsed)
            };
            let action = if state.status_line.is_empty() { "..." } else { &state.status_line };
            let activity = if state.last_activity.is_empty() {
                "".into()
            } else {
                format!(" | {}", truncate(&state.last_activity, 40))
            };
            Line::from(vec![
                Span::styled(format!(" {spun} "), Style::default().fg(Color::Cyan)),
                Span::styled(truncate(action, 35), Style::default().fg(Color::White)),
                Span::styled(format!(" [{node_time}]"), Style::default().fg(Color::DarkGray)),
                Span::styled(activity, Style::default().fg(Color::Gray)),
            ])
        };

        let status_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let status_p = Paragraph::new(status_text)
            .block(status_block)
            .wrap(Wrap { trim: false });
        frame.render_widget(status_p, chunks[2]);
    }
}

fn build_tree_lines(state: &TuiState, spinner_idx: usize) -> Vec<ListItem<'_>> {
    if state.nodes.is_empty() {
        return vec![ListItem::new(Span::styled("  ...", Style::default().fg(Color::DarkGray)))];
    }
    let mut lines: Vec<ListItem> = Vec::new();
    let root = state.nodes.iter().find(|n| n.parent.is_none());
    if let Some(r) = root {
        render_node(r, &state.nodes, "", true, &mut lines, spinner_idx, &state.node_activities);
    }
    lines
}

fn render_node(node: &Node, all: &[Node], prefix: &str, _is_last: bool, lines: &mut Vec<ListItem>, spinner_idx: usize, activities: &HashMap<String, String>) {
    let icon = status_icon(&node.status);
    let color = status_color(&node.status);
    let goal = node.goal.lines().next().unwrap_or(&node.goal);
    let goal = truncate(goal, 55);

    let running = node.status == "running";
    let spin = if running { format!(" {}", SPINNER[spinner_idx]) } else { String::new() };

    let span = Span::styled(
        format!("{prefix}{icon}{spin} {goal}"),
        Style::default().fg(color),
    );
    lines.push(ListItem::new(span));

    if running {
        if let Some(act) = activities.get(&node.id) {
            if !act.is_empty() {
                let indent = format!("{prefix}   ");
                let act_text = truncate(act, 50);
                lines.push(ListItem::new(Span::styled(
                    format!("{indent}{act_text}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }

    let children: Vec<&Node> = all.iter().filter(|n| n.parent.as_deref() == Some(&node.id)).collect();
    for (i, child) in children.iter().enumerate() {
        let is_last_child = i == children.len() - 1;
        let child_prefix = if is_last_child {
            format!("{}   ", prefix)
        } else {
            format!("{}│  ", prefix)
        };
        render_node(child, all, &child_prefix, is_last_child, lines, spinner_idx, activities);
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ").trim().to_string();
    if s.len() > max {
        format!("{}…", &s[..max.saturating_sub(1)])
    } else {
        s
    }
}
