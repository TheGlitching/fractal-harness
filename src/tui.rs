use crossterm::{
    cursor, execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use std::io::{self, Write};

const WIDTH: u16 = 80;
const OUTPUT_HEIGHT: u16 = 12;

pub struct Tui {
    out: io::Stderr,
    pub output_lines: Vec<String>,
    pub status_line: String,
    pub node_id: String,
    pub node_goal: String,
    pub elapsed: u64,
    pub steps: usize,
    pub completed: usize,
    pub failed: usize,
    pub spin_idx: usize,
}

impl Tui {
    pub fn new(node_id: &str, node_goal: &str) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = io::stderr();
        execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
        Ok(Tui {
            out,
            output_lines: Vec::new(),
            status_line: String::new(),
            node_id: node_id.to_string(),
            node_goal: node_goal.to_string(),
            elapsed: 0,
            steps: 0,
            completed: 0,
            failed: 0,
            spin_idx: 0,
        })
    }

    pub fn set_stats(&mut self, elapsed: u64, steps: usize, completed: usize, failed: usize) {
        self.elapsed = elapsed;
        self.steps = steps;
        self.completed = completed;
        self.failed = failed;
    }

    pub fn add_output(&mut self, line: &str) {
        self.output_lines.push(line.to_string());
        if self.output_lines.len() > OUTPUT_HEIGHT as usize * 4 {
            self.output_lines.remove(0);
        }
    }

    pub fn draw(&mut self) -> io::Result<()> {
        let spin = ["|", "/", "-", "\\"][self.spin_idx % 4];
        self.spin_idx += 1;
        let w = WIDTH as usize;

        // Top border
        queue!(self.out, cursor::MoveTo(0, 0), SetForegroundColor(Color::Cyan))?;
        let title = format!(" fractal · {}", self.node_id);
        let pad = w.saturating_sub(title.len()).saturating_sub(2);
        let top = format!("{}{}", title, "─".repeat(pad));
        queue!(self.out, Print(top), ResetColor)?;

        // Goal line
        queue!(self.out, cursor::MoveTo(0, 1))?;
        let goal = if self.node_goal.len() > w - 2 { &self.node_goal[..w - 5] } else { &self.node_goal };
        queue!(self.out, Print(format!(" {}", goal)))?;

        // Separator
        queue!(self.out, cursor::MoveTo(0, 2), Print("─".repeat(w)))?;

        // Output panel
        let start = self.output_lines.len().saturating_sub(OUTPUT_HEIGHT as usize);
        for (i, line) in self.output_lines.iter().skip(start).take(OUTPUT_HEIGHT as usize).enumerate() {
            queue!(self.out, cursor::MoveTo(0, 3 + i as u16), Clear(ClearType::UntilNewLine))?;
            let clipped: String = line.chars().take(w - 2).collect();
            queue!(self.out, Print(format!("  {}", clipped)))?;
        }
        // Clear remaining output rows
        let shown = (self.output_lines.len().saturating_sub(start)).min(OUTPUT_HEIGHT as usize);
        for i in shown..OUTPUT_HEIGHT as usize {
            queue!(self.out, cursor::MoveTo(0, 3 + i as u16), Clear(ClearType::UntilNewLine))?;
        }

        // Separator above status
        let status_y = 3 + OUTPUT_HEIGHT;
        queue!(self.out, cursor::MoveTo(0, status_y), Print("─".repeat(w)))?;

        // Status bar
        queue!(self.out, cursor::MoveTo(0, status_y + 1), SetForegroundColor(Color::Yellow))?;
        let status = format!(
            "  {} steps · {} done · {} fail · {}s · verify {}/{}",
            self.steps, self.completed, self.failed, self.elapsed, 0, 0
        );
        queue!(self.out, Print(status), ResetColor)?;

        // Spinner + latest action
        queue!(self.out, cursor::MoveTo(0, status_y + 2), SetForegroundColor(Color::Green))?;
        let action = if self.status_line.is_empty() { "..." } else { &self.status_line };
        let clipped: String = action.chars().take(w - 6).collect();
        queue!(self.out, Print(format!("  {spin} {}", clipped)), ResetColor)?;

        // Bottom border
        queue!(self.out, cursor::MoveTo(0, status_y + 3), SetForegroundColor(Color::Cyan))?;
        queue!(self.out, Print("─".repeat(w)), ResetColor)?;

        self.out.flush()
    }

    pub fn close(mut self) -> io::Result<()> {
        execute!(self.out, cursor::Show, terminal::LeaveAlternateScreen)?;
        terminal::disable_raw_mode()
    }
}
