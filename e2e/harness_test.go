// Package e2e is tg's black-box contract suite. It runs the tg binary against
// an encrypted fixture database directory with an isolated HOME and TMPDIR,
// and asserts only what users, scripts and MCP clients observe: stdout,
// essential stderr text, exit codes, files written and JSON shapes. Internals
// can change freely underneath it.
package e2e

import (
	"bytes"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"regexp"
	"strings"
	"testing"
)

var update = flag.Bool("update", false, "rewrite e2e/testdata snapshots")

var tgBin = os.Getenv("TG_BIN")

// TestMain runs the contracts against TG_BIN, or against a fresh build of
// ./cmd/tg once that package exists. Without either it skips with a note, so
// `go test ./...` stays green before the Go command lands.
func TestMain(m *testing.M) {
	flag.Parse()
	os.Exit(run(m))
}

func run(m *testing.M) int {
	if tgBin == "" {
		if !hasGoFiles("../cmd/tg") {
			fmt.Fprintln(os.Stderr, "e2e: TG_BIN is unset and ../cmd/tg has no Go files; skipping the contract suite")
			return 0
		}
		dir, err := os.MkdirTemp("", "tg-e2e-bin-")
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			return 2
		}
		defer os.RemoveAll(dir)
		tgBin = filepath.Join(dir, "tg")
		build := exec.Command("go", "build", "-o", tgBin, "../cmd/tg")
		build.Stdout, build.Stderr = os.Stderr, os.Stderr
		if err := build.Run(); err != nil {
			fmt.Fprintln(os.Stderr, "build tg:", err)
			return 2
		}
	}
	abs, err := filepath.Abs(tgBin)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 2
	}
	tgBin = abs
	code := m.Run()
	cleanupFixture()
	return code
}

func hasGoFiles(dir string) bool {
	matches, _ := filepath.Glob(filepath.Join(dir, "*.go"))
	return len(matches) > 0
}

// Env is one isolated process environment: its own HOME and TMPDIR, UTC, and
// nothing inherited from the developer's shell except PATH.
type Env struct {
	Home  string
	Tmp   string
	Extra []string
	Stdin string
}

func newEnv(t *testing.T) *Env {
	t.Helper()
	root := t.TempDir()
	e := &Env{Home: filepath.Join(root, "home"), Tmp: filepath.Join(root, "tmp")}
	for _, dir := range []string{e.Home, e.Tmp} {
		if err := os.MkdirAll(dir, 0o700); err != nil {
			t.Fatal(err)
		}
	}
	return e
}

func (e *Env) environ() []string {
	env := []string{
		"HOME=" + e.Home,
		"TMPDIR=" + e.Tmp,
		"TZ=UTC",
		"LANG=C.UTF-8",
		"PATH=" + os.Getenv("PATH"),
	}
	return append(env, e.Extra...)
}

// Result is what one tg run showed the caller.
type Result struct {
	Args   []string
	Stdout string
	Stderr string
	Code   int
}

func (e *Env) run(t *testing.T, args ...string) Result {
	t.Helper()
	cmd := exec.Command(tgBin, args...)
	cmd.Env = e.environ()
	cmd.Dir = e.Tmp
	if e.Stdin != "" {
		cmd.Stdin = strings.NewReader(e.Stdin)
	}
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err := cmd.Run()
	code := 0
	if err != nil {
		exit, ok := err.(*exec.ExitError)
		if !ok {
			t.Fatalf("run tg %v: %v", args, err)
		}
		code = exit.ExitCode()
	}
	return Result{Args: args, Stdout: stdout.String(), Stderr: stderr.String(), Code: code}
}

// tg runs tg against the shared fixture with --db-dir and --keys appended.
func tg(t *testing.T, args ...string) Result {
	t.Helper()
	return newEnv(t).run(t, fixtureArgs(args...)...)
}

// fixtureArgs appends the source flags every data command needs.
func fixtureArgs(args ...string) []string {
	fx := fixture()
	return append(append([]string{}, args...), "--db-dir", fx.DBDir, "--keys", fx.Keys)
}

func (r Result) mustSucceed(t *testing.T) Result {
	t.Helper()
	if r.Code != 0 {
		t.Fatalf("tg %v exited %d\nstdout:\n%s\nstderr:\n%s", r.Args, r.Code, r.Stdout, r.Stderr)
	}
	return r
}

func (r Result) mustFail(t *testing.T, code int, stderrHas ...string) Result {
	t.Helper()
	if r.Code != code {
		t.Fatalf("tg %v exited %d, want %d\nstdout:\n%s\nstderr:\n%s", r.Args, r.Code, code, r.Stdout, r.Stderr)
	}
	for _, want := range stderrHas {
		if !strings.Contains(r.Stderr, want) {
			t.Errorf("tg %v stderr lacks %q:\n%s", r.Args, want, r.Stderr)
		}
	}
	return r
}

func (r Result) stdoutHas(t *testing.T, wants ...string) {
	t.Helper()
	for _, want := range wants {
		if !strings.Contains(r.Stdout, want) {
			t.Errorf("tg %v stdout lacks %q:\n%s", r.Args, want, r.Stdout)
		}
	}
}

// normalize replaces machine-specific paths with stable placeholders.
func normalize(text string, extra ...string) string {
	fx := fixture()
	pairs := append([]string{fx.Keys, "<KEYS>", fx.DBDir, "<DB_DIR>", fx.Root, "<FIXTURE>"}, extra...)
	for i := 0; i+1 < len(pairs); i += 2 {
		if pairs[i] != "" {
			text = strings.ReplaceAll(text, pairs[i], pairs[i+1])
		}
	}
	return text
}

// golden compares got with testdata/<name>, or rewrites it under -update.
func golden(t *testing.T, name, got string) {
	t.Helper()
	path := filepath.Join("testdata", name)
	if *update {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(got), 0o644); err != nil {
			t.Fatal(err)
		}
		return
	}
	want, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("%v (run with -update to create it)", err)
	}
	if got != string(want) {
		t.Errorf("output differs from %s (rerun with -update after review)\n--- got\n%s\n--- want\n%s", path, got, want)
	}
}

// goldenRun freezes a run's exit code and normalized stdout.
func goldenRun(t *testing.T, name string, r Result, extra ...string) {
	t.Helper()
	got := fmt.Sprintf("exit: %d\n%s", r.Code, normalize(r.Stdout, extra...))
	golden(t, filepath.Join("cli", name+".golden"), got)
	if t.Failed() && r.Stderr != "" {
		t.Logf("stderr:\n%s", r.Stderr)
	}
}

var ansi = regexp.MustCompile("\x1b\\[[0-9;]*m")

func stripANSI(s string) string { return ansi.ReplaceAllString(s, "") }

// assertJSONEqual decodes got and compares it with want after a JSON round
// trip, so key order and number formatting do not matter.
func assertJSONEqual(t *testing.T, got string, want any) {
	t.Helper()
	var gotValue, wantValue any
	if err := json.Unmarshal([]byte(got), &gotValue); err != nil {
		t.Fatalf("invalid JSON %q: %v", got, err)
	}
	encoded, err := json.Marshal(want)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(encoded, &wantValue); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(gotValue, wantValue) {
		t.Errorf("JSON differs\n got: %s\nwant: %s", got, encoded)
	}
}
