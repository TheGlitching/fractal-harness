use crate::store::Node;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
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
    for (s, icon) in ICONS {
        if *s == status {
            return icon;
        }
    }
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
    #[allow(dead_code)]
    pub refused_goals: Vec<String>,
    pub failed_goals: Vec<String>,
}

#[derive(Debug, PartialEq, Clone)]
pub enum TuiMode {
    Normal,
    Inspect,
    InputPrompt { title: String, input: String, command: String },
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
    pub selected_idx: usize,
    pub mode: TuiMode,
    pub prompt_message: Option<String>,
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

        Ok(Tui {
            terminal,
            state,
            started_at: Instant::now(),
        })
    }

    pub fn run(&mut self, store: &crate::store::Store) -> io::Result<()> {
        let tick = Duration::from_millis(REFRESH_MS);

        loop {
            self.terminal
                .draw(|f| Self::render(f, &self.state.lock().unwrap(), self.started_at))?;

            if event::poll(tick)? {
                if let Event::Key(key) = event::read()? {
                    let mut s = self.state.lock().unwrap();
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        crate::scheduler::INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }

                    match &s.mode.clone() {
                        TuiMode::Normal => match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Up | KeyCode::Char('k') => {
                                if s.selected_idx > 0 {
                                    s.selected_idx -= 1;
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if !s.nodes.is_empty() && s.selected_idx + 1 < s.nodes.len() {
                                    s.selected_idx += 1;
                                }
                            }
                            KeyCode::Char('i') | KeyCode::Char('?') | KeyCode::Enter => {
                                s.mode = TuiMode::Inspect;
                            }
                            KeyCode::Char('s') | KeyCode::Char('m') => {
                                if let Some(selected_node) = s.nodes.get(s.selected_idx) {
                                    let nid = selected_node.id.clone();
                                    s.mode = TuiMode::InputPrompt {
                                        title: format!("Add constraint to {nid} & descendants"),
                                        input: String::new(),
                                        command: format!("constraint:{nid}"),
                                    };
                                }
                            }
                            KeyCode::Char('r') => {
                                if let Some(selected_node) = s.nodes.get(s.selected_idx) {
                                    let nid = selected_node.id.clone();
                                    let _ = store.enqueue_steer("retry", &nid);
                                    s.prompt_message = Some(format!("Queued retry for node {nid}"));
                                }
                            }
                            _ => {}
                        },
                        TuiMode::Inspect => match key.code {
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => {
                                s.mode = TuiMode::Normal;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if s.selected_idx > 0 {
                                    s.selected_idx -= 1;
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if !s.nodes.is_empty() && s.selected_idx + 1 < s.nodes.len() {
                                    s.selected_idx += 1;
                                }
                            }
                            KeyCode::Char('s') | KeyCode::Char('m') => {
                                if let Some(selected_node) = s.nodes.get(s.selected_idx) {
                                    let nid = selected_node.id.clone();
                                    s.mode = TuiMode::InputPrompt {
                                        title: format!("Add constraint to {nid} & descendants"),
                                        input: String::new(),
                                        command: format!("constraint:{nid}"),
                                    };
                                }
                            }
                            KeyCode::Char('r') => {
                                if let Some(selected_node) = s.nodes.get(s.selected_idx) {
                                    let nid = selected_node.id.clone();
                                    let _ = store.enqueue_steer("retry", &nid);
                                    s.prompt_message = Some(format!("Queued retry for node {nid}"));
                                }
                            }
                            _ => {}
                        },
                        TuiMode::InputPrompt { title, input, command } => match key.code {
                            KeyCode::Esc => {
                                s.mode = TuiMode::Normal;
                            }
                            KeyCode::Enter => {
                                if !input.trim().is_empty() {
                                    if command.starts_with("constraint:") {
                                        let nid = command.trim_start_matches("constraint:");
                                        let _ = store.enqueue_steer("constraint", &format!("{nid}:{input}"));
                                        s.prompt_message = Some(format!("Constraint applied: \"{input}\" on {nid}"));
                                    }
                                }
                                s.mode = TuiMode::Normal;
                            }
                            KeyCode::Backspace => {
                                let mut new_input = input.clone();
                                new_input.pop();
                                s.mode = TuiMode::InputPrompt {
                                    title: title.clone(),
                                    input: new_input,
                                    command: command.clone(),
                                };
                            }
                            KeyCode::Char(c) => {
                                let mut new_input = input.clone();
                                new_input.push(c);
                                s.mode = TuiMode::InputPrompt {
                                    title: title.clone(),
                                    input: new_input,
                                    command: command.clone(),
                                };
                            }
                            _ => {}
                        },
                    }
                }
            }

            let done = self.state.lock().unwrap().done;
            if !done {
                continue;
            }

            // Done — keep drawing until user presses q
            self.terminal
                .draw(|f| Self::render(f, &self.state.lock().unwrap(), self.started_at))?;
            loop {
                if event::poll(tick)? {
                    if let Event::Key(key) = event::read()? {
                        if (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL))
                            || key.code == KeyCode::Char('q')
                        {
                            return self.cleanup();
                        }
                    }
                }
                self.terminal
                    .draw(|f| Self::render(f, &self.state.lock().unwrap(), self.started_at))?;
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
                Span::styled(
                    &state.model,
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  active: {}", state.node_id),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    "   [↑/↓: select, i: inspect, s: steer/constraint, r: retry, q: quit]",
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(Span::styled(
                format!(" goal: {}", truncate(&root_goal, 72)),
                Style::default().fg(Color::White),
            )),
        ];
        let header_text = Paragraph::new(header_body).block(header);
        frame.render_widget(header_text, chunks[0]);

        // Main area: tree + stats/inspect
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(chunks[1]);

        // Tree panel
        let tree_block = Block::default()
            .borders(Borders::ALL)
            .title(" Task Tree ")
            .border_style(Style::default().fg(Color::DarkGray));
        let spinner_idx = (started_at.elapsed().as_millis() as usize / 150) % SPINNER.len();
        let tree_lines = build_tree_lines(state, spinner_idx);
        let tree_list = List::new(tree_lines).block(tree_block);
        frame.render_widget(tree_list, main_chunks[0]);

        // Right panel: Inspect or Stats
        if state.mode == TuiMode::Inspect {
            let inspect_block = Block::default()
                .borders(Borders::ALL)
                .title(" Node Inspector ")
                .border_style(Style::default().fg(Color::Cyan));

            let mut inspect_lines: Vec<Line> = Vec::new();
            if let Some(node) = state.nodes.get(state.selected_idx) {
                inspect_lines.push(Line::from(vec![
                    Span::styled("ID:     ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&node.id, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  [{}]", node.status), Style::default().fg(status_color(&node.status))),
                ]));
                inspect_lines.push(Line::from(vec![
                    Span::styled("Goal:   ", Style::default().fg(Color::DarkGray)),
                    Span::styled(truncate(&node.goal, 40), Style::default().fg(Color::White)),
                ]));
                if let Some(ref p) = node.parent {
                    inspect_lines.push(Line::from(vec![
                        Span::styled("Parent: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(p, Style::default().fg(Color::Gray)),
                    ]));
                }
                inspect_lines.push(Line::from(""));
                let contract = node.contract();
                if !contract.constraints.is_empty() {
                    inspect_lines.push(Line::from(Span::styled(
                        "Constraints:",
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    )));
                    for c in &contract.constraints {
                        inspect_lines.push(Line::from(Span::styled(
                            format!(" • {}", truncate(c, 35)),
                            Style::default().fg(Color::LightYellow),
                        )));
                    }
                    inspect_lines.push(Line::from(""));
                }

                if let Ok(decisions) = std::fs::read_to_string(node.decisions_path()) {
                    inspect_lines.push(Line::from(Span::styled(
                        "Decisions:",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )));
                    for l in decisions.lines().filter(|l| l.starts_with("- ")).rev().take(5) {
                        inspect_lines.push(Line::from(Span::styled(
                            truncate(l, 38),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
            } else {
                inspect_lines.push(Line::from(Span::styled("No node selected", Style::default().fg(Color::DarkGray))));
            }

            let inspect_p = Paragraph::new(Text::from(inspect_lines)).block(inspect_block);
            frame.render_widget(inspect_p, main_chunks[1]);
        } else {
            // Stats panel
            let elapsed = started_at.elapsed().as_secs();
            let mins = elapsed / 60;
            let secs = elapsed % 60;
            let time_str = if mins > 0 {
                format!("{}m {}s", mins, secs)
            } else {
                format!("{}s", secs)
            };
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

            if let Some(ref msg) = state.prompt_message {
                stats_lines.push(Line::from(vec![
                    Span::styled("  Notice:", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
                ]));
                stats_lines.push(Line::from(vec![
                    Span::styled(format!("    {}", truncate(msg, 32)), Style::default().fg(Color::LightGreen)),
                ]));
                stats_lines.push(Line::from(""));
            }

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

            let stats_block = Block::default()
                .borders(Borders::ALL)
                .title(" Progress ")
                .border_style(Style::default().fg(Color::DarkGray));
            let stats_p = Paragraph::new(Text::from(stats_lines)).block(stats_block);
            frame.render_widget(stats_p, main_chunks[1]);
        }

        // Status bar
        let spun = SPINNER[(started_at.elapsed().as_millis() as usize / 150) % SPINNER.len()];
        let status_text = if state.done {
            if let Some(ref err) = state.error {
                Line::from(vec![
                    Span::styled(
                        " ✗ ",
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(err.as_str(), Style::default().fg(Color::Red)),
                ])
            } else {
                Line::from(vec![
                    Span::styled(
                        " ✓ ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
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
            let action = if state.status_line.is_empty() {
                "..."
            } else {
                &state.status_line
            };
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

        // Input Modal if in InputPrompt mode
        if let TuiMode::InputPrompt { ref title, ref input, .. } = state.mode {
            let area = centered_rect(60, 25, frame.area());
            frame.render_widget(Clear, area);

            let modal_block = Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} "))
                .border_style(Style::default().fg(Color::LightYellow));

            let input_lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                    Span::styled(input, Style::default().fg(Color::White)),
                    Span::styled("█", Style::default().fg(Color::Yellow)),
                ]),
                Line::from(""),
                Line::from(Span::styled("[Enter: Confirm, Esc: Cancel]", Style::default().fg(Color::DarkGray))),
            ];

            let p = Paragraph::new(input_lines).block(modal_block);
            frame.render_widget(p, area);
        }
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn build_tree_lines(state: &TuiState, spinner_idx: usize) -> Vec<ListItem<'_>> {
    if state.nodes.is_empty() {
        return vec![ListItem::new(Span::styled(
            "  ...",
            Style::default().fg(Color::DarkGray),
        ))];
    }
    let mut lines: Vec<ListItem> = Vec::new();
    let mut flat_idx = 0;
    let root = state.nodes.iter().find(|n| n.parent.is_none());
    if let Some(r) = root {
        render_node(
            r,
            &state.nodes,
            "",
            true,
            &mut lines,
            spinner_idx,
            &state.node_activities,
            state.selected_idx,
            &mut flat_idx,
        );
    }
    lines
}

fn render_node(
    node: &Node,
    all: &[Node],
    prefix: &str,
    _is_last: bool,
    lines: &mut Vec<ListItem>,
    spinner_idx: usize,
    activities: &HashMap<String, String>,
    selected_idx: usize,
    flat_idx: &mut usize,
) {
    let is_selected = *flat_idx == selected_idx;
    *flat_idx += 1;

    let icon = status_icon(&node.status);
    let color = status_color(&node.status);
    let goal = node.goal.lines().next().unwrap_or(&node.goal);
    let goal = truncate(goal, 50);

    let running = node.status == "running";
    let spin = if running {
        format!(" {}", SPINNER[spinner_idx])
    } else {
        String::new()
    };

    let select_marker = if is_selected { "▶ " } else { "  " };
    let mut style = Style::default().fg(color);
    if is_selected {
        style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
    }

    let span = Span::styled(
        format!("{select_marker}{prefix}{icon}{spin} {goal}"),
        style,
    );
    lines.push(ListItem::new(span));

    if running {
        if let Some(act) = activities.get(&node.id) {
            if !act.is_empty() {
                let indent = format!("  {prefix}   ");
                let act_text = truncate(act, 45);
                lines.push(ListItem::new(Span::styled(
                    format!("{indent}{act_text}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }

    let children: Vec<&Node> = all
        .iter()
        .filter(|n| n.parent.as_deref() == Some(&node.id))
        .collect();
    for (i, child) in children.iter().enumerate() {
        let is_last_child = i == children.len() - 1;
        let child_prefix = if is_last_child {
            format!("{}   ", prefix)
        } else {
            format!("{}│  ", prefix)
        };
        render_node(
            child,
            all,
            &child_prefix,
            is_last_child,
            lines,
            spinner_idx,
            activities,
            selected_idx,
            flat_idx,
        );
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
