// Package logx writes tg's diagnostics to stderr. Level comes from TG_LOG
// (error, warn, info, debug, trace, off; the last comma-separated directive
// wins, and `target=level` uses the level).
package logx

import (
	"fmt"
	"os"
	"strings"
	"sync/atomic"
)

type Level int32

const (
	Off Level = iota
	Error
	Warn
	Info
	Debug
	Trace
)

var level atomic.Int32

func init() { level.Store(int32(parseEnv(os.Getenv("TG_LOG")))) }

func parseEnv(v string) Level {
	parts := strings.Split(v, ",")
	for i := len(parts) - 1; i >= 0; i-- {
		p := strings.TrimSpace(parts[i])
		if j := strings.LastIndexByte(p, '='); j >= 0 {
			p = strings.TrimSpace(p[j+1:])
		}
		switch strings.ToLower(p) {
		case "off":
			return Off
		case "error":
			return Error
		case "warn", "warning":
			return Warn
		case "info":
			return Info
		case "debug":
			return Debug
		case "trace":
			return Trace
		}
	}
	return Info
}

// Enabled reports whether messages at l are written.
func Enabled(l Level) bool { return Level(level.Load()) >= l }

func Errorf(format string, args ...any) { emit(Error, "error: ", format, args) }
func Warnf(format string, args ...any)  { emit(Warn, "warning: ", format, args) }
func Infof(format string, args ...any)  { emit(Info, "", format, args) }
func Debugf(format string, args ...any) { emit(Debug, "debug: ", format, args) }

func emit(l Level, prefix, format string, args []any) {
	if Enabled(l) {
		fmt.Fprintf(os.Stderr, prefix+format+"\n", args...)
	}
}
