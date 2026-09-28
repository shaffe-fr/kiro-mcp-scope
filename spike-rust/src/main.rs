use ratatui::{
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
            KeyModifiers, MouseButton, MouseEventKind,
        },
        execute,
    },
    style::{Color, Style},
    text::Line,
    widgets::Paragraph,
    DefaultTerminal, Frame,
};

const ITEMS: [&str; 5] = ["alpha", "bravo", "charlie", "delta", "echo"];

/// Rows occupied by the header before the first item: title, then a blank line.
const HEADER_ROWS: u16 = 2;

struct App {
    checked: [bool; ITEMS.len()],
    cursor: usize,
}

impl App {
    fn new() -> Self {
        Self {
            checked: [false; ITEMS.len()],
            cursor: 0,
        }
    }

    fn toggle(&mut self, index: usize) {
        self.checked[index] = !self.checked[index];
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = ITEMS.len() as isize;
        let next = (self.cursor as isize + delta).rem_euclid(len);
        self.cursor = next as usize;
    }

    fn render(&self, frame: &mut Frame) {
        let mut lines = vec![
            Line::styled(
                "Spike ratatui — click to check, wheel to move, arrows + space, q to quit",
                Style::new().fg(Color::Rgb(125, 211, 252)),
            ),
            Line::raw(""),
        ];

        for (i, label) in ITEMS.iter().enumerate() {
            let checkbox = if self.checked[i] { "[x]" } else { "[ ]" };
            let style = if i == self.cursor {
                Style::new()
                    .bg(Color::Rgb(30, 58, 95))
                    .fg(Color::Rgb(255, 255, 255))
            } else {
                Style::new().fg(Color::Rgb(203, 213, 225))
            };
            lines.push(Line::styled(format!(" {checkbox} {label}"), style));
        }

        let count = self.checked.iter().filter(|checked| **checked).count();
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("checked: {count}/{}", ITEMS.len()),
            Style::new().fg(Color::Rgb(148, 163, 184)),
        ));

        frame.render_widget(Paragraph::new(lines), frame.area());
    }
}

fn run(terminal: &mut DefaultTerminal) -> std::io::Result<()> {
    let mut app = App::new();
    loop {
        terminal.draw(|frame| app.render(frame))?;

        match event::read()? {
            // Windows reports both press and release; acting on either would
            // apply every keystroke twice.
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(())
                }
                KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
                KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
                KeyCode::Char(' ') | KeyCode::Enter => {
                    let cursor = app.cursor;
                    app.toggle(cursor);
                }
                _ => {}
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    // The alternate screen makes row 0 the top of the rendered
                    // view, so this maps straight onto the layout above.
                    if let Some(row) = mouse.row.checked_sub(HEADER_ROWS) {
                        let row = row as usize;
                        if row < ITEMS.len() {
                            app.cursor = row;
                            app.toggle(row);
                        }
                    }
                }
                MouseEventKind::ScrollUp => app.move_cursor(-1),
                MouseEventKind::ScrollDown => app.move_cursor(1),
                _ => {}
            },
            _ => {}
        }
    }
}

fn main() -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture)?;

    let result = run(&mut terminal);

    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}
