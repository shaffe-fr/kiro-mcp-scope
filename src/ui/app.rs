//! ratatui event loop for both TUI views.
//!
//! `ratatui::init()` gives raw mode, the alternate screen (mandatory, or mouse
//! coordinates are offset by scrollback), and a panic hook; mouse capture is
//! enabled explicitly and disabled on exit. `KeyEventKind::Press` is filtered
//! because Windows emits both press and release. crossterm classifies mouse
//! buttons itself.
//!
//! Both views are held together and switched with Tab. Their edited state lives
//! in memory, so switching before saving does not lose pending checks.

use std::io::stdout;

use ratatui::{
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
            KeyModifiers, MouseButton, MouseEventKind,
        },
        execute,
    },
    style::{Color, Modifier, Style},
    text::Line,
    widgets::Paragraph,
    DefaultTerminal, Frame,
};

use crate::core::merge::{OnDiverge, ServerState};
use crate::core::types::ServerKind;
use crate::core::workspace::WriteError;
use crate::ui::matrix_view::{MatrixView, CELL_STEP, NAME_COL_WIDTH};
use crate::ui::project_view::ProjectView;

/// Rows before the first server row in the project view: title, project line, blank.
const PROJECT_HEADER_ROWS: u16 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Active {
    Project,
    Matrix,
}

/// Both views, plus which one is shown. A matrix view is optional: running from
/// a project without discovery still gives the project view.
pub struct App {
    project: ProjectView,
    matrix: Option<MatrixView>,
    active: Active,
}

impl App {
    pub fn new(project: ProjectView, matrix: Option<MatrixView>) -> Self {
        Self {
            project,
            matrix,
            active: Active::Project,
        }
    }

    /// Start on the matrix view instead of the project view.
    pub fn starting_on_matrix(mut self) -> Self {
        if self.matrix.is_some() {
            self.active = Active::Matrix;
        }
        self
    }
}

/// Run the app to completion, setting up and tearing down the terminal.
pub fn run(mut app: App) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    let result = event_loop(&mut terminal, &mut app);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    loop {
        terminal.draw(|frame| render(frame, app))?;

        let event = event::read()?;
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    KeyCode::Tab => {
                        toggle_active(app);
                        continue;
                    }
                    _ => {}
                }
            }
        }

        match app.active {
            Active::Project => handle_project(&mut app.project, &event),
            Active::Matrix => {
                if let Some(matrix) = app.matrix.as_mut() {
                    handle_matrix(matrix, &event);
                }
            }
        }
    }
}

fn toggle_active(app: &mut App) {
    app.active = match app.active {
        Active::Project => {
            if app.matrix.is_some() {
                Active::Matrix
            } else {
                Active::Project
            }
        }
        Active::Matrix => Active::Project,
    };
}

fn handle_project(view: &mut ProjectView, event: &Event) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Up | KeyCode::Char('k') => view.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => view.move_cursor(1),
            KeyCode::Char(' ') | KeyCode::Enter => view.toggle(),
            KeyCode::Char('d') => view.cycle_divergence(),
            KeyCode::Char('s') => {
                if let Err(err) = view.save() {
                    view.message = Some(write_error_message(err));
                }
            }
            _ => {}
        },
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = mouse.row.checked_sub(PROJECT_HEADER_ROWS) {
                    let row = row as usize;
                    if row < view.len() {
                        view.set_cursor(row);
                        view.toggle();
                    }
                }
            }
            MouseEventKind::ScrollUp => view.move_cursor(-1),
            MouseEventKind::ScrollDown => view.move_cursor(1),
            _ => {}
        },
        _ => {}
    }
}

fn handle_matrix(view: &mut MatrixView, event: &Event) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Up | KeyCode::Char('k') => view.move_project(-1),
            KeyCode::Down | KeyCode::Char('j') => view.move_project(1),
            KeyCode::Left | KeyCode::Char('h') => view.move_server(-1),
            KeyCode::Right | KeyCode::Char('l') => view.move_server(1),
            KeyCode::Char(' ') | KeyCode::Enter => view.toggle_cursor(),
            KeyCode::Char('s') => {
                if let Err((project, err)) = view.save() {
                    view.message = Some(format!(
                        "{}: {}",
                        project.display(),
                        write_error_message(err)
                    ));
                }
            }
            _ => {}
        },
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => view.click(mouse.column, mouse.row),
            MouseEventKind::ScrollUp => view.move_project(-1),
            MouseEventKind::ScrollDown => view.move_project(1),
            _ => {}
        },
        _ => {}
    }
}

fn write_error_message(err: WriteError) -> String {
    match err {
        WriteError::Secret(secret) => secret.to_string(),
        WriteError::Io(io) => format!("Write failed: {io}"),
    }
}

fn kind_label(kind: ServerKind) -> &'static str {
    match kind {
        ServerKind::Local => "local",
        ServerKind::Remote => "remote",
    }
}

fn render(frame: &mut Frame, app: &App) {
    match app.active {
        Active::Project => render_project(frame, &app.project, app.matrix.is_some()),
        Active::Matrix => {
            if let Some(matrix) = &app.matrix {
                render_matrix(frame, matrix);
            }
        }
    }
}

fn accent() -> Style {
    Style::new().fg(Color::Rgb(125, 211, 252))
}

fn dim() -> Style {
    Style::new().fg(Color::Rgb(148, 163, 184))
}

fn cursor_style() -> Style {
    Style::new()
        .bg(Color::Rgb(30, 58, 95))
        .fg(Color::Rgb(255, 255, 255))
}

fn render_project(frame: &mut Frame, view: &ProjectView, has_matrix: bool) {
    let hint = if has_matrix {
        "kms — space/click toggle, d resolve divergence, s save, Tab matrix, q quit"
    } else {
        "kms — space/click toggle, d resolve divergence, s save, q quit"
    };
    let dirty = if view.is_dirty() { " *unsaved*" } else { "" };
    let mut lines = vec![
        Line::styled(hint, accent()),
        Line::styled(
            format!("project: {}{}", view.project_dir().display(), dirty),
            dim(),
        ),
        Line::raw(""),
    ];

    if view.is_empty() {
        lines.push(Line::styled("Catalog is empty.", dim()));
    }

    for (i, row) in view.rows.iter().enumerate() {
        let checkbox = if row.selected { "[x]" } else { "[ ]" };
        let mut label = format!(
            " {checkbox} {} ({})",
            row.status.name,
            kind_label(row.status.kind)
        );
        if row.status.state == ServerState::Diverged {
            let resolution = match row.on_diverge {
                OnDiverge::Keep => "keep local",
                OnDiverge::Catalog => "use catalog",
            };
            label.push_str(&format!(
                "  ! diverges: {} — d: {resolution}",
                row.status.diverging_fields.join(", ")
            ));
        }
        if row.status.disabled_in_kiro == Some(true) {
            label.push_str("  (off in Kiro)");
        }
        let mut style = if i == view.cursor {
            cursor_style()
        } else {
            Style::new()
        };
        if row.status.state == ServerState::Diverged {
            style = style.add_modifier(Modifier::BOLD);
        }
        lines.push(Line::styled(label, style));
    }

    if let Some(message) = &view.message {
        lines.push(Line::raw(""));
        lines.push(Line::styled(message.clone(), dim()));
    }

    frame.render_widget(Paragraph::new(lines), frame.area());
}

fn render_matrix(frame: &mut Frame, view: &MatrixView) {
    let dirty = if view.is_dirty() { " *unsaved*" } else { "" };
    let mut lines = vec![
        Line::styled(
            "kms matrix — arrows move, space/click toggle, s save, Tab project, q quit",
            accent(),
        ),
        Line::raw(""),
    ];

    if view.is_empty() {
        lines.push(Line::styled(
            "No projects or no catalog servers to show.",
            dim(),
        ));
        frame.render_widget(Paragraph::new(lines), frame.area());
        return;
    }

    // Header row: server initials, one per column, aligned to the grid.
    let mut header = format!(
        "{:<width$}",
        format!("projects{dirty}"),
        width = NAME_COL_WIDTH as usize
    );
    for name in &view.matrix.server_names {
        let short: String = name.chars().take(CELL_STEP as usize - 1).collect();
        header.push_str(&format!("{:<width$}", short, width = CELL_STEP as usize));
    }
    lines.push(Line::styled(header, dim()));
    lines.push(Line::raw(""));

    for (p, project) in view.matrix.projects.iter().enumerate() {
        let name: String = project
            .name
            .chars()
            .take(NAME_COL_WIDTH as usize - 1)
            .collect();
        let mut line = format!("{:<width$}", name, width = NAME_COL_WIDTH as usize);
        for s in 0..view.matrix.server_count() {
            let checked = view.matrix.checked[p][s];
            let diverged = view.matrix.diverged[p][s];
            let mark = if diverged {
                "[!]"
            } else if checked {
                "[x]"
            } else {
                "[ ]"
            };
            line.push_str(&format!("{:<width$}", mark, width = CELL_STEP as usize));
        }
        let is_cursor_row = p == view.project_cursor;
        let style = if is_cursor_row {
            cursor_style()
        } else {
            Style::new()
        };
        lines.push(Line::styled(line, style));
    }

    let selected = &view.matrix.server_names[view.server_cursor];
    lines.push(Line::raw(""));
    lines.push(Line::styled(format!("column: {selected}"), dim()));

    if let Some(message) = &view.message {
        lines.push(Line::styled(message.clone(), dim()));
    }

    frame.render_widget(Paragraph::new(lines), frame.area());
}
