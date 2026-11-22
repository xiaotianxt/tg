// Package env captures the process environment once, at the top of a
// command, so packages below take explicit values instead of reading HOME,
// the clock, or the time zone themselves.
package env

import (
	"os"
	"os/user"
	"path/filepath"
	"time"

	"github.com/xiaotianxt/tg/internal/paths"
)

// Env is everything tg reads from its process environment.
type Env struct {
	// Home is $HOME of the process.
	Home string
	// UserHome is the invoking user's home: $SUDO_USER's home under sudo,
	// else Home. Client data lives here even when tg runs as root.
	UserHome string
	// StateDir is $TG_HOME or ~/.tg.
	StateDir string
	// TempDir is os.TempDir().
	TempDir string
	// Loc is the local time zone used for parsing and display.
	Loc *time.Location
	// Now returns the current time.
	Now func() time.Time
	// Getenv reads a variable; tests substitute a map.
	Getenv func(string) string
}

// FromProcess reads the real process environment.
func FromProcess() Env {
	home := os.Getenv("HOME")
	userHome := home
	if name := os.Getenv("SUDO_USER"); name != "" {
		if u, err := user.Lookup(name); err == nil && u.HomeDir != "" {
			userHome = u.HomeDir
		}
	}
	return Env{
		Home:     home,
		UserHome: userHome,
		StateDir: paths.StateDir(),
		TempDir:  os.TempDir(),
		Loc:      time.Local,
		Now:      time.Now,
		Getenv:   os.Getenv,
	}
}

// Tool finds an external tool: the override variable when set, else
// paths.FindTool(name). It returns "" when neither exists.
func (e Env) Tool(overrideVar, name string) string {
	if v := e.Getenv(overrideVar); v != "" {
		return v
	}
	return paths.FindTool(name)
}

// KeysPath is the default all_keys.json path.
func (e Env) KeysPath() string { return filepath.Join(e.StateDir, "all_keys.json") }
