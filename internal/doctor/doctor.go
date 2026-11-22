// Package doctor builds the step-by-step setup diagnosis `tg doctor` prints
// and the MCP diagnose tool returns: the desktop process, external tools,
// the key file, the source directory, opening the key databases, and an
// optional chat probe. Every step reports its outcome instead of stopping,
// so one report shows everything that is wrong.
package doctor

import (
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/history"
	"github.com/xiaotianxt/tg/internal/keys"
	"github.com/xiaotianxt/tg/internal/media"
	"github.com/xiaotianxt/tg/internal/paths"
	"github.com/xiaotianxt/tg/internal/render"
	"github.com/xiaotianxt/tg/internal/resolve"
	"github.com/xiaotianxt/tg/internal/store"
	"github.com/xiaotianxt/tg/internal/voice"
)

// Options selects what to diagnose.
type Options struct {
	// Session is a chat to probe; "" for none.
	Session string
	// DBDir is the requested source directory; "" auto-detects under Home.
	DBDir    string
	KeysPath string
	Jobs     int
}

// Run diagnoses the setup. It never fails: each problem is a check line.
func Run(e env.Env, o Options) render.DoctorReport {
	var r report
	r.process()
	ffmpeg, err := media.FFmpegPath(e)
	r.tool("ffmpeg", ffmpeg, err)
	decoder, err := voice.ResolveDecoder(e)
	r.tool("voice decoder", decoder.Path, err)
	kf, haveKeys := r.keys(o.KeysPath)
	root := r.source(e, o.DBDir, kf, haveKeys)

	var st *store.Store
	if root != "" {
		st = r.direct(root, o.KeysPath)
	}
	if st != nil {
		defer st.Close()
	}
	if o.Session != "" {
		d := &render.SessionDiagnosis{Query: o.Session, Kind: render.SessionUnavailable}
		if st != nil {
			probe, err := history.ProbeSession(st, o.Session, resolve.Options{Activity: true, Jobs: o.Jobs, Loc: e.Loc})
			if err != nil {
				d.Kind, d.Error = render.SessionFailed, err.Error()
			} else {
				d.Kind, d.Probe = render.SessionResolved, probe
			}
		}
		r.out.Session = d
	}
	return r.out
}

type report struct{ out render.DoctorReport }

func (r *report) status(label string, s render.CheckStatus, detail string) {
	r.out.Checks = append(r.out.Checks, render.DoctorCheck{Label: label, Status: s, Detail: detail})
}

func (r *report) fact(label, detail string) { r.status(label, render.NoStatus, detail) }

func (r *report) process() {
	const label = "Telegram process"
	out, err := exec.Command("pgrep", "-x", dict.DesktopAppProcess()).Output()
	if err != nil {
		r.status(label, render.StatusMissing, "Telegram is not running.")
		return
	}
	line, _, _ := strings.Cut(string(out), "\n")
	pid, err := strconv.Atoi(strings.TrimSpace(line))
	if err != nil {
		r.status(label, render.StatusMissing, fmt.Sprintf("Invalid PID: %v", err))
		return
	}
	r.status(label, render.StatusOK, fmt.Sprintf("pid %d", pid))
}

// tool reports an external tool through the same lookup media conversion
// uses, so a service's PATH problem shows up before a conversion fails.
func (r *report) tool(label, path string, err error) {
	if err != nil {
		r.status(label, render.StatusMissing, err.Error())
		return
	}
	r.status(label, render.StatusOK, path)
}

func (r *report) keys(path string) (keys.KeyFile, bool) {
	kf, err := keys.LoadKeyFile(path)
	switch {
	case err != nil:
		r.status("keys", render.StatusError, err.Error())
		return nil, false
	case kf == nil:
		r.status("keys", render.StatusMissing, path)
		return nil, false
	}
	usable := 0
	for rel := range kf {
		if hasUsableKey(kf, rel) {
			usable++
		}
	}
	if usable == 0 {
		r.status("keys", render.StatusError, fmt.Sprintf("0 usable keys in %s", path))
	} else {
		r.status("keys", render.StatusOK, fmt.Sprintf("%d keys in %s", usable, path))
	}
	return kf, true
}

// source reports the source directory and its message shards, returning
// the directory or "" when there is none.
func (r *report) source(e env.Env, requested string, kf keys.KeyFile, haveKeys bool) string {
	root := requested
	if requested != "" {
		if st, err := os.Stat(requested); err != nil || !st.IsDir() {
			r.status("source db dir", render.StatusMissing, requested)
			return ""
		}
	} else if e.Home != "" {
		root = paths.FindDBStorageDir(e.Home)
	}
	if root == "" {
		r.status("source db dir", render.StatusMissing, "auto-detection failed")
		return ""
	}
	r.status("source db dir", render.StatusOK, root)

	files, err := store.CollectSourceDatabases(root)
	if err != nil {
		r.status("source catalog", render.StatusError, err.Error())
		return root
	}
	var shards []store.SourceDatabase
	for _, f := range files {
		if n, ok := store.ShardNumber(f.RelPath); ok && n >= 0 {
			shards = append(shards, f)
		}
	}
	r.fact("source numbered message dbs", strconv.Itoa(len(shards)))
	coverage := "unknown"
	if haveKeys {
		covered := 0
		for _, f := range shards {
			if hasUsableKey(kf, f.RelPath) {
				covered++
			}
		}
		coverage = fmt.Sprintf("%d/%d numbered dbs", covered, len(shards))
	}
	r.fact("message key coverage", coverage)
	hot := 0
	for _, f := range shards {
		if st, err := os.Stat(f.FullPath + "-wal"); err == nil && st.Size() > 0 {
			hot++
		}
	}
	r.fact("live message WAL files", strconv.Itoa(hot))
	return root
}

// direct opens the source store and the databases every command needs. The
// caller closes the returned store.
func (r *report) direct(root, keysPath string) *store.Store {
	st, err := store.Discover(root, keysPath)
	if err != nil {
		r.status("direct source", render.StatusError, err.Error())
		return nil
	}
	r.status("direct source", render.StatusOK, "")
	r.database(st, resolve.SessionDB, "session db")
	r.database(st, history.FTSDB, "message FTS")
	return st
}

func (r *report) database(st *store.Store, rel, label string) {
	if !st.Has(rel) {
		r.status(label, render.StatusMissing, "")
		return
	}
	conn, err := st.Open(rel)
	if err != nil {
		r.status(label, render.StatusError, err.Error())
		return
	}
	conn.Close()
	r.status(label, render.StatusOK, "")
}

// hasUsableKey reports whether rel has a 64-hex-digit enc_key.
func hasUsableKey(kf keys.KeyFile, rel string) bool {
	key := kf[rel]["enc_key"]
	if len(key) != 64 {
		return false
	}
	for i := 0; i < len(key); i++ {
		c := key[i]
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f' || c >= 'A' && c <= 'F') {
			return false
		}
	}
	return true
}
