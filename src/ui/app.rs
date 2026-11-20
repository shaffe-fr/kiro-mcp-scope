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
    text::{Line, Span},
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
            KeyCode::Char('s') => match view.save() {
                Ok(_) => record_history(view.catalog()),
                Err(err) => view.message = Some(write_error_message(err)),
            },
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
            KeyCode::Char('e') => view.toggle_fold(),
            KeyCode::Char('i') => view.init_worktree(),
            KeyCode::Char('s') => match view.save() {
                Ok(_) => record_history(view.catalog()),
                Err((project, err)) => {
                    view.message = Some(format!(
                        "{}: {}",
                        project.display(),
                        write_error_message(err)
                    ));
                }
            },
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

/// Remember what the catalog says now, so a later sync can tell a copy that
/// merely lags from one that was customized. Best effort: a saved selection
/// never fails because of it.
fn record_history(catalog: &crate::core::types::Catalog) {
    crate::core::state::record_best_effort(catalog);
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

/// `text` cut to `width` characters, with an ellipsis when something was lost.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// The one cell the keyboard acts on: bright, impossible to miss.
fn active_cell_style() -> Style {
    Style::new()
        .bg(Color::Rgb(125, 211, 252))
        .fg(Color::Rgb(15, 23, 42))
        .add_modifier(Modifier::BOLD)
}

/// A faint band down the cursor column, to follow it across rows.
fn column_style() -> Style {
    Style::new().bg(Color::Rgb(22, 38, 62))
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

/// `1 name  2 name ...`, wrapped to the width, the cursor column highlighted.
fn legend_lines(view: &MatrixView, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (index, name) in view.matrix.server_names.iter().enumerate() {
        let item = format!("{} {}", index + 1, name);
        let cost = item.chars().count() + 2;
        if used > 0 && used + cost > width {
            lines.push(Line::from(std::mem::take(&mut current)));
            used = 0;
        }
        let style = if index == view.server_cursor {
            cursor_style()
        } else {
            dim()
        };
        current.push(Span::styled(item, style));
        current.push(Span::raw("  "));
        used += cost;
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
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

    // Header row: one number per column, explained by the legend below the
    // grid. Truncated names were ambiguous (three "Rovo", two "play"). The
    // cursor column's number is lit, so the active column is visible from the
    // header down to the grid.
    let mut header = vec![Span::styled(
        format!(
            "{:<width$}",
            format!("projects{dirty}"),
            width = NAME_COL_WIDTH as usize
        ),
        dim(),
    )];
    for number in 1..=view.matrix.server_count() {
        let style = if number - 1 == view.server_cursor {
            active_cell_style()
        } else {
            dim()
        };
        // Centered over the three characters of its `[ ]`.
        header.push(Span::styled(format!("{number:^3}"), style));
        header.push(Span::raw(" ".repeat(CELL_STEP as usize - 3)));
    }
    lines.push(Line::from(header));
    lines.push(Line::raw(""));

    for &p in view.visible_rows() {
        let project = &view.matrix.projects[p];
        let is_worktree = project.parent.is_some();
        let has_children = !view.matrix.worktrees_of(p).is_empty();
        let prefix = if is_worktree {
            "    "
        } else if has_children && view.is_expanded(p) {
            "- "
        } else if has_children {
            "+ "
        } else {
            "  "
        };
        let name = fit(
            &format!("{prefix}{}", project.name),
            NAME_COL_WIDTH as usize - 2,
        );
        let on_cursor_row = p == view.project_cursor;
        let row_style = if on_cursor_row {
            cursor_style()
        } else if is_worktree && !project.has_config {
            dim()
        } else {
            Style::new()
        };

        let mut spans = vec![Span::styled(
            format!("{:<width$}", name, width = NAME_COL_WIDTH as usize),
            row_style,
        )];
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
            let on_cursor_column = s == view.server_cursor;
            let cell_style = if on_cursor_row && on_cursor_column {
                active_cell_style()
            } else if on_cursor_column {
                row_style.patch(column_style())
            } else {
                row_style
            };
            spans.push(Span::styled(mark, cell_style));
            // The gap between cells never takes the band: it is as wide as the
            // `[ ]` and the header number above it, no wider.
            spans.push(Span::styled(
                " ".repeat(CELL_STEP as usize - mark.len()),
                row_style,
            ));
        }
        if is_worktree && !project.has_config {
            spans.push(Span::styled("  no config (i)", row_style));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::raw(""));
    lines.extend(legend_lines(view, frame.area().width as usize));
    if let Some(hint) = view.row_hint() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(hint, accent()));
    }

    for outside in &view.outside {
        lines.push(Line::styled(outside.clone(), dim()));
    }
    if let Some(message) = &view.message {
        lines.push(Line::styled(message.clone(), dim()));
    }

    frame.render_widget(Paragraph::new(lines), frame.area());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::discovery::DiscoveredProject;
    use crate::core::matrix::Matrix;
    use crate::core::merge::compute_statuses;
    use crate::core::types::Catalog;
    use ratatui::{backend::TestBackend, Terminal};
    use serde_json::Map;
    use std::path::PathBuf;

    fn draw(view: &MatrixView) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();
        terminal.draw(|frame| render_matrix(frame, view)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen(view: &MatrixView) -> String {
        let buffer = draw(view);
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn similar_server_names_are_told_apart_and_keys_are_explained_in_place() {
        let mut servers = Map::new();
        for name in ["Rovo-MCP-Jira3", "Rovo-MCP-Septime", "Rovo-MCP-Versailles"] {
            servers.insert(name.to_string(), serde_json::json!({ "command": "x" }));
        }
        let catalog = Catalog { servers };
        let project = |dir: &str, parent: Option<&str>, has_config: bool| DiscoveredProject {
            name: dir.rsplit('/').next().unwrap().to_string(),
            dir: PathBuf::from(dir),
            statuses: compute_statuses(&catalog, &Map::new()),
            active_count: 0,
            parent: parent.map(PathBuf::from),
            has_config,
        };
        let projects = vec![
            project("/sites/germineo", None, true),
            project(
                "/sites/germineo/.worktrees/feat",
                Some("/sites/germineo"),
                false,
            ),
        ];
        let matrix = Matrix::build(&catalog, &projects);
        let mut view = MatrixView::new(catalog, matrix);
        view.server_cursor = 2;

        let shown = screen(&view);
        assert!(shown.contains("1    2    3"), "numbered header:\n{shown}");
        assert!(shown.contains("1 Rovo-MCP-Jira3"), "legend:\n{shown}");
        assert!(shown.contains("3 Rovo-MCP-Versailles"), "legend:\n{shown}");
        assert!(shown.contains("- germineo"), "{shown}");
        assert!(shown.contains("no config"), "{shown}");

        view.project_cursor = 1;
        let shown = screen(&view);
        assert!(
            shown.contains("no config — i: write it from germineo"),
            "{shown}"
        );
    }

    #[test]
    fn the_active_cell_column_and_header_follow_the_keyboard() {
        let mut servers = Map::new();
        for name in ["a", "b", "c"] {
            servers.insert(name.to_string(), serde_json::json!({ "command": "x" }));
        }
        let catalog = Catalog { servers };
        let project = |dir: &str| DiscoveredProject {
            name: dir.trim_start_matches('/').to_string(),
            dir: PathBuf::from(dir),
            statuses: compute_statuses(&catalog, &Map::new()),
            active_count: 0,
            parent: None,
            has_config: true,
        };
        let matrix = Matrix::build(&catalog, &[project("/p1"), project("/p2")]);
        let mut view = MatrixView::new(catalog, matrix);
        let active = active_cell_style().bg;
        let band = column_style().bg;

        let at = |buffer: &ratatui::buffer::Buffer, column: u16, line: u16| {
            // First character of the cell: the "[" of "[ ]" or the header digit.
            buffer[(NAME_COL_WIDTH + column * CELL_STEP, line)]
                .style()
                .bg
        };
        let (header, row1, row2) = (2, 4, 5);

        view.project_cursor = 1;
        view.server_cursor = 2;
        let buffer = draw(&view);
        assert_eq!(at(&buffer, 2, row2), active, "the cell under the cursor");
        assert_eq!(at(&buffer, 2, header), active, "its column number");
        assert_eq!(at(&buffer, 2, row1), band, "the column band on other rows");
        assert_ne!(at(&buffer, 0, row2), active, "no other cell is active");
        let gap = buffer[(NAME_COL_WIDTH + 2 * CELL_STEP + 3, row1)]
            .style()
            .bg;
        assert_ne!(gap, band, "the band stops at the cell's three characters");
        let last = buffer[(NAME_COL_WIDTH + 2 * CELL_STEP + 2, row1)]
            .style()
            .bg;
        assert_eq!(last, band, "and covers all three");
        assert_ne!(at(&buffer, 1, header), active);

        view.move_server(-1);
        view.move_project(-1);
        let buffer = draw(&view);
        assert_eq!(at(&buffer, 1, row1), active, "it moves with the keys");
        assert_eq!(at(&buffer, 1, header), active);
        assert_ne!(at(&buffer, 2, row2), active);
    }

    #[test]
    fn header_numbers_sit_over_the_middle_of_their_cell_and_long_names_end_in_an_ellipsis() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("germineo-fix-security-audit", 12), "germineo-fi…");

        let mut servers = Map::new();
        servers.insert("a".to_string(), serde_json::json!({ "command": "x" }));
        let catalog = Catalog { servers };
        let project = DiscoveredProject {
            name: "p".to_string(),
            dir: PathBuf::from("/p"),
            statuses: compute_statuses(&catalog, &Map::new()),
            active_count: 0,
            parent: None,
            has_config: true,
        };
        let matrix = Matrix::build(&catalog, &[project]);
        let view = MatrixView::new(catalog, matrix);
        let buffer = draw(&view);
        let x = NAME_COL_WIDTH;
        // Header line 2, first project line 4: the digit above the space of `[ ]`.
        assert_eq!(buffer[(x + 1, 2)].symbol(), "1");
        assert_eq!(buffer[(x, 4)].symbol(), "[");
        assert_eq!(buffer[(x + 1, 4)].symbol(), " ");
    }
}
