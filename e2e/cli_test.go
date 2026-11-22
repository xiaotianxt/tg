package e2e

import (
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
)

func TestArgv(t *testing.T) {
	t.Parallel()
	e := newEnv(t)

	r := e.run(t, "--version").mustSucceed(t)
	if !regexp.MustCompile(`^tg \d+\.\d+\.\d+\n$`).MatchString(r.Stdout) {
		t.Errorf("--version prints %q, want \"tg X.Y.Z\"", r.Stdout)
	}
	for _, args := range [][]string{{"--help"}, {"help"}} {
		r := e.run(t, args...).mustSucceed(t)
		r.stdoutHas(t, "Usage: tg", "sessions", "unread", "members", "messages", "search",
			"query", "media", "export", "doctor", "keys", "mcp", "skill", "completions")
	}
	r = e.run(t, "messages", "--help").mustSucceed(t)
	r.stdoutHas(t, "--limit", "--offset", "--since", "--all-time", "--head", "--search",
		"--sender", "--time-bucket", "--anonymous", "--db-dir", "--keys")

	// No command is a usage error.
	e.run(t).mustFail(t, 2)
	// Retired commands are unknown subcommands, not session names.
	for _, retired := range []string{"decrypt", "refresh", "image", "file", "sticker", "emoji", "voice", "sql"} {
		e.run(t, retired, "x").mustFail(t, 2, retired)
	}
	e.run(t, "sessions", "--bogus").mustFail(t, 2, "--bogus")
	e.run(t, "completions", "powershell").mustFail(t, 2)
}

// An unknown first argument is a session for `messages`, flags included.
func TestDefaultMessagesCommand(t *testing.T) {
	t.Parallel()
	r := tg(t, "tgid_bob", "--limit", "1", "--time-bucket", "none").mustSucceed(t)
	if r.Stdout != tg(t, "messages", "tgid_bob", "--limit", "1", "--time-bucket", "none").Stdout {
		t.Errorf("`tg <session>` differs from `tg messages <session>`:\n%s", r.Stdout)
	}
	r.stdoutHas(t, "Bob: bob message 55")
}

func TestSourceErrors(t *testing.T) {
	t.Parallel()
	fx := fixture()
	e := newEnv(t)

	missingDir := filepath.Join(e.Tmp, "no-such-db_storage")
	r := e.run(t, "sessions", "--db-dir", missingDir, "--keys", fx.Keys)
	r.mustFail(t, 1, "Source database directory does not exist: "+missingDir)

	missingKeys := filepath.Join(e.Tmp, "keys.json")
	for _, args := range [][]string{
		{"sessions"}, {"unread"}, {"messages", "tgid_bob"}, {"search", "message"},
		{"query", "--contains", "x"}, {"members", "team@chatroom"},
	} {
		r := e.run(t, append(args, "--db-dir", fx.DBDir, "--keys", missingKeys)...)
		r.mustFail(t, 1, "Database keys are missing at "+missingKeys, "tg keys --db-dir")
	}
	// Without --keys the default lives under HOME.
	r = e.run(t, "sessions", "--db-dir", fx.DBDir)
	r.mustFail(t, 1, "Database keys are missing at "+filepath.Join(e.Home, ".tg", "all_keys.json"))

	bad := filepath.Join(e.Tmp, "bad.json")
	if err := os.WriteFile(bad, []byte("{not json"), 0o600); err != nil {
		t.Fatal(err)
	}
	e.run(t, "sessions", "--db-dir", fx.DBDir, "--keys", bad).mustFail(t, 1)
}

// wrongKeys writes a keys file whose entry for rel is valid hex but wrong.
func wrongKeys(t *testing.T, dir, rel string) string {
	t.Helper()
	keys := map[string]string{}
	for k, v := range fixture().KeyHex {
		keys[k] = v
	}
	if _, ok := keys[rel]; !ok {
		t.Fatalf("fixture has no key for %s", rel)
	}
	keys[rel] = strings.Repeat("ee", 32)
	path := filepath.Join(dir, "wrong_keys.json")
	if err := writeKeys(path, keys); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestWrongKey(t *testing.T) {
	t.Parallel()
	fx := fixture()
	e := newEnv(t)

	keys := wrongKeys(t, e.Tmp, "session/session.db")
	r := e.run(t, "sessions", "--db-dir", fx.DBDir, "--keys", keys)
	r.mustFail(t, 1, "session.db", "tg keys")

	e2 := newEnv(t)
	keys = wrongKeys(t, e2.Tmp, "message/message_1.db")
	r = e2.run(t, "messages", "tgid_bob", "--db-dir", fx.DBDir, "--keys", keys)
	r.mustFail(t, 1, "message_1.db", "tg keys")
	// Session listing does not need the message shards.
	e2.run(t, "sessions", "--db-dir", fx.DBDir, "--keys", keys).mustSucceed(t)
}

// doctorEnv pins the external tool paths doctor reports.
func doctorEnv(t *testing.T) *Env {
	e := newEnv(t)
	e.Extra = []string{"TG_FFMPEG=/opt/e2e/bin/ffmpeg", "TG_VOICE_DECODER=/opt/e2e/bin/voice-decoder"}
	return e
}

// doctorOutput drops the desktop-app process line, which depends on what is
// running on the test machine.
func doctorOutput(r Result) Result {
	var kept []string
	for _, line := range strings.SplitAfter(r.Stdout, "\n") {
		if strings.Contains(line, " process: ") {
			continue
		}
		kept = append(kept, line)
	}
	r.Stdout = strings.Join(kept, "")
	return r
}

func TestDoctor(t *testing.T) {
	t.Parallel()
	fx := fixture()
	cases := []struct {
		name string
		args []string
	}{
		{"doctor", fixtureArgs("doctor")},
		{"doctor_session", fixtureArgs("doctor", "爱丽丝")},
		{"doctor_group", fixtureArgs("doctor", "team@chatroom")},
		{"doctor_unknown_session", fixtureArgs("doctor", "nobody-at-all")},
		{"doctor_missing_keys", []string{"doctor", "--db-dir", fx.DBDir, "--keys", "/nonexistent/e2e/keys.json"}},
		{"doctor_missing_db_dir", []string{"doctor", "--db-dir", "/nonexistent/e2e/db_storage", "--keys", fx.Keys}},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			t.Parallel()
			r := doctorOutput(doctorEnv(t).run(t, c.args...))
			goldenRun(t, c.name, r)
		})
	}
}

func TestDoctorWrongKey(t *testing.T) {
	t.Parallel()
	e := doctorEnv(t)
	keys := wrongKeys(t, e.Tmp, "session/session.db")
	r := doctorOutput(e.run(t, "doctor", "--db-dir", fixture().DBDir, "--keys", keys))
	goldenRun(t, "doctor_wrong_key", r, keys, "<WRONG_KEYS>")
}

func TestSkillInstall(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	template, err := os.ReadFile("../SKILL.md")
	if err != nil {
		t.Fatal(err)
	}
	want := realAppNames(string(template))

	dir := filepath.Join(e.Tmp, "skills", "tg")
	r := e.run(t, "skill", "install", "--dir", dir).mustSucceed(t)
	r.stdoutHas(t, filepath.Join(dir, "SKILL.md"))
	got, err := os.ReadFile(filepath.Join(dir, "SKILL.md"))
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != want {
		t.Errorf("installed SKILL.md is not the template with the app's real names")
	}

	// Default location: $CODEX_HOME/skills/tg, else ~/.codex/skills/tg.
	e.run(t, "skill", "install").mustSucceed(t)
	if _, err := os.Stat(filepath.Join(e.Home, ".codex", "skills", "tg", "SKILL.md")); err != nil {
		t.Error(err)
	}
	codexHome := filepath.Join(e.Tmp, "codex")
	e.Extra = []string{"CODEX_HOME=" + codexHome}
	e.run(t, "skill", "install").mustSucceed(t)
	if _, err := os.Stat(filepath.Join(codexHome, "skills", "tg", "SKILL.md")); err != nil {
		t.Error(err)
	}

	// A symlinked skill directory is refused rather than written through.
	target := filepath.Join(e.Tmp, "elsewhere")
	link := filepath.Join(e.Tmp, "link")
	if err := os.Mkdir(target, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(target, link); err != nil {
		t.Fatal(err)
	}
	e.run(t, "skill", "install", "--dir", link).mustFail(t, 1, "is a symlink")
	if _, err := os.Stat(filepath.Join(target, "SKILL.md")); err == nil {
		t.Error("SKILL.md was written through the symlink")
	}
}

func TestCompletions(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	for _, shell := range []string{"bash", "zsh", "fish"} {
		r := e.run(t, "completions", shell).mustSucceed(t)
		r.stdoutHas(t, "sessions", "messages", "export", "time-bucket")
		// The script must at least parse in its shell when that shell exists.
		if path, err := exec.LookPath(shell); err == nil {
			script := filepath.Join(e.Tmp, "tg."+shell)
			if err := os.WriteFile(script, []byte(r.Stdout), 0o600); err != nil {
				t.Fatal(err)
			}
			if out, err := exec.Command(path, "-n", script).CombinedOutput(); err != nil {
				t.Errorf("%s completion does not parse: %v\n%s", shell, err, out)
			}
		}
	}
}

// keys is exercised only on paths that fail before touching any process:
// nothing here may attach to or read a running desktop app.
func TestKeysErrors(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	missing := filepath.Join(e.Tmp, "no-such-db_storage")
	out := filepath.Join(e.Tmp, "keys.json")
	r := e.run(t, "keys", "--db-dir", missing, "--output", out)
	r.mustFail(t, 1, "No usable cached account key material", "tg keys --db-dir", "--method login")
	if _, err := os.Stat(out); err == nil {
		t.Error("a failed keys run wrote the output file")
	}
	e.run(t, "keys", "--method", "bogus").mustFail(t, 2, "bogus")
}

// realAppNames renders the public cover name the way installed skills and
// MCP metadata show it; coverAppNames undoes that for snapshots.
func realAppNames(text string) string {
	app, localized := appName(), localizedAppName()
	for _, suffix := range []string{"聊天", "群", "里"} {
		text = strings.ReplaceAll(text, "Telegram"+suffix, localized+suffix)
	}
	text = strings.ReplaceAll(text, "Telegram", app)
	return strings.ReplaceAll(text, "telegram", strings.ToLower(app))
}

func coverAppNames(text string) string {
	app, localized := appName(), localizedAppName()
	text = strings.ReplaceAll(text, localized, "Telegram")
	text = strings.ReplaceAll(text, app, "Telegram")
	return strings.ReplaceAll(text, strings.ToLower(app), "telegram")
}

func appName() string          { return masked(13, 63, 25, 50, 59, 46) }
func localizedAppName() string { return masked(191, 228, 244, 190, 229, 251) }
