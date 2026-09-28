//go:build windows

// Mouse input diagnostic for Windows terminals.
//
// Purpose: determine empirically whether mouse events reach a process at all
// in a given terminal, and through which channel. Bubble Tea picks one of two
// mutually exclusive input paths on Windows (see key_windows.go, readInputs):
//
//   - stdin is a usable console handle -> readConInputs, mouse arrives as
//     MOUSE_EVENT_RECORD via ReadConsoleInput. FocusMsg is never produced on
//     this path, and ANSI mouse sequences are irrelevant.
//   - otherwise -> readAnsiInputs, mouse arrives as SGR/X10 escape sequences
//     on stdin, and focus events are parsed from \x1b[I / \x1b[O.
//
// This tool mirrors that decision, enables mouse reporting on whichever path
// applies, and logs every event received. If clicking produces nothing here,
// the terminal is not delivering mouse input to child processes at all and no
// library-level fix can recover it.
package main

import (
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"strings"
	"time"

	"github.com/erikgeiser/coninput"
	"golang.org/x/sys/windows"
)

const runFor = 30 * time.Second

// Same set Bubble Tea applies in newInputReader (inputreader_windows.go).
var bubbleteaModes = []uint32{
	windows.ENABLE_WINDOW_INPUT,
	windows.ENABLE_EXTENDED_FLAGS,
	windows.ENABLE_MOUSE_INPUT,
}

// ANSI mouse enables, matching what OpenTUI's native core emits
// (packages/native/src/terminal.zig, setMouseMode).
const (
	ansiEnableMouse = "\x1b[?1003l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h"
	ansiResetMouse  = "\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1004l"
)

type logger struct {
	file *os.File
}

func (l *logger) logf(format string, args ...any) {
	line := fmt.Sprintf(format, args...)
	fmt.Println(line)
	if l.file != nil {
		fmt.Fprintln(l.file, line)
	}
}

func main() {
	f, err := os.Create("diag.log")
	if err != nil {
		fmt.Fprintln(os.Stderr, "cannot create diag.log:", err)
		os.Exit(1)
	}
	defer f.Close()
	log := &logger{file: f}

	log.logf("=== mouse input diagnostic ===")
	log.logf("time        : %s", time.Now().Format(time.RFC3339))
	log.logf("TERM        : %q", os.Getenv("TERM"))
	log.logf("TERM_PROGRAM: %q", os.Getenv("TERM_PROGRAM"))
	log.logf("WT_SESSION  : %q", os.Getenv("WT_SESSION"))
	log.logf("")

	conin, handleErr := coninput.NewStdinHandle()
	if handleErr != nil {
		log.logf("coninput.NewStdinHandle: FAILED (%v)", handleErr)
		log.logf("=> Bubble Tea would use the ANSI path (readAnsiInputs).")
		log.logf("")
		runAnsiPath(log)
		return
	}

	log.logf("coninput.NewStdinHandle: ok (handle %v)", conin)
	log.logf("=> Bubble Tea would use the CONSOLE path (readConInputs).")
	log.logf("   On this path mouse comes from MOUSE_EVENT_RECORD only;")
	log.logf("   ANSI mouse sequences and FocusMsg do not apply.")
	log.logf("")
	runConsolePath(log, conin)
}

func runConsolePath(log *logger, conin windows.Handle) {
	var originalMode uint32
	if err := windows.GetConsoleMode(conin, &originalMode); err != nil {
		log.logf("GetConsoleMode: FAILED (%v)", err)
		return
	}
	log.logf("console mode before: 0x%08x", originalMode)
	for _, name := range coninput.ListInputModeNames(originalMode) {
		log.logf("  - %s", name)
	}

	// Bubble Tea replaces the mode wholesale, starting from 0, rather than
	// OR-ing onto the existing one. Reproduced here so the observed behaviour
	// matches what Bubble Tea actually gets.
	newMode := coninput.AddInputModes(0, bubbleteaModes...)
	if err := windows.SetConsoleMode(conin, newMode); err != nil {
		log.logf("SetConsoleMode: FAILED (%v)", err)
		return
	}
	defer func() {
		_ = windows.SetConsoleMode(conin, originalMode)
	}()

	var applied uint32
	if err := windows.GetConsoleMode(conin, &applied); err == nil {
		log.logf("console mode after : 0x%08x (requested 0x%08x)", applied, newMode)
		for _, name := range coninput.ListInputModeNames(applied) {
			log.logf("  - %s", name)
		}
		if applied&windows.ENABLE_MOUSE_INPUT == 0 {
			log.logf("  !! ENABLE_MOUSE_INPUT did NOT stick after SetConsoleMode")
		}
	}

	log.logf("")
	log.logf("Now click and scroll in this terminal. Press q to stop (max %s).", runFor)
	log.logf("")

	deadline := time.Now().Add(runFor)
	mouseSeen, keySeen, otherSeen := 0, 0, 0

	for time.Now().Before(deadline) {
		n, err := coninput.GetNumberOfConsoleInputEvents(conin)
		if err != nil {
			log.logf("GetNumberOfConsoleInputEvents: FAILED (%v)", err)
			return
		}
		if n == 0 {
			time.Sleep(20 * time.Millisecond)
			continue
		}
		records, err := coninput.ReadNConsoleInputs(conin, 16)
		if err != nil {
			log.logf("ReadNConsoleInputs: FAILED (%v)", err)
			return
		}
		for _, record := range records {
			switch e := record.Unwrap().(type) {
			case coninput.MouseEventRecord:
				mouseSeen++
				log.logf("MOUSE  x=%d y=%d buttons=%s flags=%v",
					e.MousePositon.X, e.MousePositon.Y, e.ButtonState, e.EventFlags)
			case coninput.KeyEventRecord:
				keySeen++
				if e.KeyDown && (e.Char == 'q' || e.Char == 'Q') {
					log.logf("KEY    q pressed -> stopping")
					summary(log, mouseSeen, keySeen, otherSeen)
					return
				}
				if e.KeyDown {
					log.logf("KEY    char=%q vk=%d", e.Char, e.VirtualKeyCode)
				}
			default:
				otherSeen++
				log.logf("OTHER  %T", e)
			}
		}
	}
	summary(log, mouseSeen, keySeen, otherSeen)
}

func runAnsiPath(log *logger) {
	fmt.Print(ansiEnableMouse)
	defer fmt.Print(ansiResetMouse)

	log.logf("Wrote ANSI mouse enables: ?1000h ?1002h ?1006h ?1004h")
	log.logf("")
	log.logf("Now click and scroll in this terminal. Press q to stop (max %s).", runFor)
	log.logf("")

	done := make(chan struct{})
	bytesSeen := 0

	go func() {
		defer close(done)
		buf := make([]byte, 256)
		for {
			n, err := os.Stdin.Read(buf)
			if n > 0 {
				chunk := buf[:n]
				bytesSeen += n
				log.logf("STDIN  %d bytes: %s | %q",
					n, hex.EncodeToString(chunk), sanitize(chunk))
				if strings.ContainsAny(string(chunk), "qQ") {
					log.logf("q pressed -> stopping")
					return
				}
			}
			if err != nil {
				if err != io.EOF {
					log.logf("stdin read error: %v", err)
				}
				return
			}
		}
	}()

	select {
	case <-done:
	case <-time.After(runFor):
		log.logf("timeout reached")
	}

	log.logf("")
	log.logf("total stdin bytes: %d", bytesSeen)
	if bytesSeen == 0 {
		log.logf("=> nothing arrived at all: the terminal is not forwarding input.")
	}
}

func sanitize(b []byte) string {
	var sb strings.Builder
	for _, c := range b {
		if c == 0x1b {
			sb.WriteString("<ESC>")
			continue
		}
		if c < 0x20 || c > 0x7e {
			fmt.Fprintf(&sb, "<%02x>", c)
			continue
		}
		sb.WriteByte(c)
	}
	return sb.String()
}

func summary(log *logger, mouse, key, other int) {
	log.logf("")
	log.logf("--- summary ---")
	log.logf("mouse records : %d", mouse)
	log.logf("key records   : %d", key)
	log.logf("other records : %d", other)
	if mouse == 0 {
		log.logf("")
		log.logf("=> NO mouse events were delivered to this process.")
		log.logf("   ENABLE_MOUSE_INPUT was set, so the terminal (or the pty")
		log.logf("   between it and this process) is dropping them. No change")
		log.logf("   inside the Go program can recover mouse input here.")
	}
}
