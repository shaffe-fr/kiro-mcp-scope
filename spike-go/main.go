package main

import (
	"fmt"
	"os"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

var items = []string{"alpha", "bravo", "charlie", "delta", "echo"}

// Rows occupied by the header before the first item: title, then a blank line.
const headerRows = 2

var (
	cursorStyle = lipgloss.NewStyle().Background(lipgloss.Color("#1e3a5f")).Foreground(lipgloss.Color("#ffffff"))
	normalStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("#cbd5e1"))
	titleStyle  = lipgloss.NewStyle().Foreground(lipgloss.Color("#7dd3fc"))
	footerStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("#94a3b8"))
)

type model struct {
	checked []bool
	cursor  int
}

func initialModel() model {
	return model{checked: make([]bool, len(items))}
}

func (m model) Init() tea.Cmd {
	return nil
}

func (m model) toggle(i int) model {
	m.checked[i] = !m.checked[i]
	return m
}

// isLeftPress reports whether a mouse event should count as a left click.
//
// Bubble Tea derives the button on Windows by XOR-ing the previous and current
// console button states (key_windows.go, mouseEventButton). Kiro's integrated
// terminal reports the right button as held during plain mouse moves, so at
// the moment of a left click the XOR yields 0x03 (left|right), which matches
// none of the single-button cases and leaves Button as MouseButtonNone. The
// press is real but arrives unlabelled, so treat an unlabelled press as a left
// click — the only button this spike cares about.
func isLeftPress(e tea.MouseEvent) bool {
	if e.Action != tea.MouseActionPress {
		return false
	}
	return e.Button == tea.MouseButtonLeft || e.Button == tea.MouseButtonNone
}

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch msg.String() {
		case "q", "ctrl+c":
			return m, tea.Quit
		case "up", "k":
			m.cursor = (m.cursor - 1 + len(items)) % len(items)
		case "down", "j":
			m.cursor = (m.cursor + 1) % len(items)
		case " ", "enter":
			m = m.toggle(m.cursor)
		}
	case tea.MouseMsg:
		event := tea.MouseEvent(msg)
		switch {
		case isLeftPress(event):
			// Y is the row within the alternate screen, so it lines up with
			// the rendered layout. Rendering inline instead would give buffer
			// coordinates offset by whatever scrollback sits above.
			row := event.Y - headerRows
			if row >= 0 && row < len(items) {
				m.cursor = row
				m = m.toggle(row)
			}
		case event.Button == tea.MouseButtonWheelUp:
			m.cursor = (m.cursor - 1 + len(items)) % len(items)
		case event.Button == tea.MouseButtonWheelDown:
			m.cursor = (m.cursor + 1) % len(items)
		}
	}
	return m, nil
}

func (m model) View() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render("Spike Bubble Tea — click to check, wheel to move, arrows + space, q to quit"))
	b.WriteString("\n\n")
	for i, label := range items {
		box := "[ ]"
		if m.checked[i] {
			box = "[x]"
		}
		line := fmt.Sprintf(" %s %s", box, label)
		if i == m.cursor {
			b.WriteString(cursorStyle.Render(line))
		} else {
			b.WriteString(normalStyle.Render(line))
		}
		b.WriteString("\n")
	}
	count := 0
	for _, v := range m.checked {
		if v {
			count++
		}
	}
	b.WriteString("\n")
	b.WriteString(footerStyle.Render(fmt.Sprintf("checked: %d/%d", count, len(items))))
	return b.String()
}

func main() {
	p := tea.NewProgram(
		initialModel(),
		tea.WithAltScreen(),
		tea.WithMouseCellMotion(),
	)
	if _, err := p.Run(); err != nil {
		fmt.Fprintln(os.Stderr, "Error:", err)
		os.Exit(1)
	}
}
