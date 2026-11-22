// Package capture implements the `tg keys` flow: derive database keys from
// cached account key material (auto), or capture that material from the
// running desktop client with a debugger (login) and then derive. The flow
// is an explicit state machine; its platform-specific debugger steps live in
// the OS-tagged files, and all key math lives in internal/keys.
package capture

import (
	"context"
	"fmt"
	"os"
	"path/filepath"

	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/keys"
	"github.com/xiaotianxt/tg/internal/logx"
	"github.com/xiaotianxt/tg/internal/paths"
)

// Method selects how account key material is obtained.
type Method int

const (
	// MethodAuto derives from already-cached account key material.
	MethodAuto Method = iota
	// MethodLogin captures fresh material from the running client with a
	// debugger during a log-out/log-in, then derives.
	MethodLogin
)

func (m Method) String() string {
	if m == MethodLogin {
		return "login"
	}
	return "auto"
}

// ParseMethod maps a CLI method name to a Method. The login aliases name the
// debugger and cold-start variants the old CLI accepted.
func ParseMethod(s string) (Method, error) {
	switch s {
	case "", "auto":
		return MethodAuto, nil
	case "login", "lldb-login", "lldb-cold", "gdb-login":
		return MethodLogin, nil
	default:
		return MethodAuto, fmt.Errorf("Unknown key extraction method: %s", s)
	}
}

// DefaultTimeout is the per-method default used when Options.Timeout is 0.
// Auto never waits on a user, so it only bounds derivation; login must give
// the user time to log out and back in.
func DefaultTimeout(m Method) int {
	if m == MethodLogin {
		return 180
	}
	return 30
}

// Outcome is the terminal state of a run, modeled explicitly instead of as an
// optional path plus flags.
type Outcome int

const (
	// OutcomeComplete means every encrypted database now has a verified key.
	OutcomeComplete Outcome = iota
	// OutcomePartial means keys were written but some databases are still
	// uncovered.
	OutcomePartial
	// OutcomeNoCoverage means the material did not verify against any new
	// database, so nothing was written.
	OutcomeNoCoverage
	// OutcomeNoMaterial means auto found no cached material to derive from.
	OutcomeNoMaterial
	// OutcomeBadMaterial means cached material was present but malformed or
	// captured material did not validate.
	OutcomeBadMaterial
)

func (o Outcome) String() string {
	switch o {
	case OutcomeComplete:
		return "complete"
	case OutcomePartial:
		return "partial"
	case OutcomeNoCoverage:
		return "no-coverage"
	case OutcomeNoMaterial:
		return "no-material"
	case OutcomeBadMaterial:
		return "bad-material"
	default:
		return "unknown"
	}
}

// Options configures a run. Timeout 0 means "the default for the method".
type Options struct {
	Method  Method
	Timeout int
	Jobs    int
	DBDir   string // "" auto-detects under the invoking user's home
	Output  string // all_keys.json path; "" uses env.KeysPath()
}

// Result reports what a run produced. KeysPath is empty when nothing was
// written.
type Result struct {
	Outcome      Outcome
	KeysPath     string
	Databases    []keys.DatabaseOutcome
	DerivedSalts int
}

// Run executes the keys flow. On success it returns a Result whose Outcome is
// Complete or Partial and whose KeysPath names the written file; otherwise it
// returns an error and a Result whose Outcome classifies the failure.
func Run(ctx context.Context, e env.Env, opts Options) (Result, error) {
	timeout := opts.Timeout
	if timeout <= 0 {
		timeout = DefaultTimeout(opts.Method)
	}
	keysPath := opts.Output
	if keysPath == "" {
		keysPath = e.KeysPath()
	}
	switch opts.Method {
	case MethodAuto:
		return runAuto(e, opts, keysPath)
	case MethodLogin:
		return runLogin(ctx, e, opts, keysPath, timeout)
	default:
		return Result{}, fmt.Errorf("Unknown key extraction method: %d", opts.Method)
	}
}

func runAuto(e env.Env, opts Options, keysPath string) (Result, error) {
	material, present, err := keys.LoadMaterial(materialPath(e))
	if err != nil {
		// Present but malformed: report it and stop. A fresh capture via
		// --method login overwrites it.
		return Result{Outcome: OutcomeBadMaterial}, err
	}
	if !present {
		return Result{Outcome: OutcomeNoMaterial}, fmt.Errorf(
			"No usable cached account key material. Capture it once with:\n\n  sudo %s --method login --timeout 180",
			captureCommandHint(e, opts, keysPath))
	}
	defer material.Wipe()

	dbStorage, err := resolveDBStorage(e, opts.DBDir)
	if err != nil {
		return Result{Outcome: OutcomeNoCoverage}, err
	}
	existing, err := loadExistingKeys(keysPath)
	if err != nil {
		return Result{Outcome: OutcomeBadMaterial}, err
	}
	derivation, err := keys.DeriveFromStorage(material.Bytes(), dbStorage, existing, opts.Jobs)
	if err != nil {
		return Result{Outcome: OutcomeNoCoverage}, err
	}
	if derivation.IsComplete() || derivation.Derived() > 0 {
		return writeAndReport("cached account material", keysPath, derivation)
	}
	if derivation.Total() > 0 {
		logx.Warnf("Cached account key material did not cover %d database(s); a new capture is required.", len(derivation.Missing()))
	}
	return Result{Outcome: OutcomeNoCoverage, Databases: derivation.Outcomes}, fmt.Errorf(
		"No usable cached account key material. Capture it once with:\n\n  sudo %s --method login --timeout 180",
		captureCommandHint(e, opts, keysPath))
}

// runLogin resolves db_storage, captures material with the platform debugger,
// confirms it validates, caches it, then derives and writes keys.
func runLogin(ctx context.Context, e env.Env, opts Options, keysPath string, timeout int) (Result, error) {
	dbStorage, err := resolveDBStorage(e, opts.DBDir)
	if err != nil {
		return Result{Outcome: OutcomeNoCoverage}, err
	}
	logx.Infof("Waiting for account key derivation in the desktop client.")
	logx.Infof("Log out of the account, then log back in within %d seconds.", timeout)
	material, err := loginCapture(ctx, e, dbStorage, timeout)
	if err != nil {
		return Result{Outcome: OutcomeBadMaterial}, err
	}
	defer material.Wipe()

	existing, err := loadExistingKeys(keysPath)
	if err != nil {
		return Result{Outcome: OutcomeBadMaterial}, err
	}
	derivation, err := keys.DeriveFromStorage(material.Bytes(), dbStorage, existing, opts.Jobs)
	if err != nil {
		return Result{Outcome: OutcomeBadMaterial}, err
	}
	if derivation.Derived() == 0 {
		valid, err := keys.ValidatesAgainstStorage(material.Bytes(), dbStorage)
		if err != nil {
			return Result{Outcome: OutcomeBadMaterial}, err
		}
		if !valid {
			return Result{Outcome: OutcomeBadMaterial}, fmt.Errorf(
				"Captured account key material did not validate against any local database.")
		}
	}
	if err := keys.StoreMaterial(materialPath(e), material); err != nil {
		return Result{Outcome: OutcomeBadMaterial}, err
	}
	return writeAndReport("captured account material", keysPath, derivation)
}

func writeAndReport(label, keysPath string, d keys.Derivation) (Result, error) {
	logDerivation(label, d)
	if err := keys.WriteKeyFile(keysPath, d.Keys); err != nil {
		return Result{Outcome: OutcomeNoCoverage, Databases: d.Outcomes}, err
	}
	outcome := OutcomeComplete
	if !d.IsComplete() {
		outcome = OutcomePartial
	}
	return Result{
		Outcome:      outcome,
		KeysPath:     keysPath,
		Databases:    d.Outcomes,
		DerivedSalts: d.DerivedSalts,
	}, nil
}

func logDerivation(label string, d keys.Derivation) {
	logx.Infof("Database keys from %s: %d reused, %d derived across %d new salt(s), %d total.",
		label, d.Reused(), d.Derived(), d.DerivedSalts, d.Total())
	if missing := d.Missing(); len(missing) > 0 {
		logx.Warnf("No verified key for %d database(s). Run with debug logging for source details.", len(missing))
		logx.Debugf("Databases without a verified key: %v", missing)
	}
}

// loadExistingKeys reads all_keys.json for key reuse. Unlike the Rust flow,
// which silently discarded a malformed keys file, a corrupt existing file is
// reported so the user can inspect it before it is replaced.
func loadExistingKeys(path string) (keys.KeyFile, error) {
	kf, err := keys.LoadKeyFile(path)
	if err != nil {
		return nil, fmt.Errorf("%w; remove %s and retry", err, path)
	}
	if kf == nil {
		kf = keys.KeyFile{}
	}
	return kf, nil
}

func resolveDBStorage(e env.Env, dbDir string) (string, error) {
	if dbDir != "" {
		if st, err := os.Stat(dbDir); err == nil && st.IsDir() {
			return dbDir, nil
		}
		return "", fmt.Errorf("Source database directory does not exist: %s", dbDir)
	}
	if e.UserHome == "" {
		return "", fmt.Errorf("Cannot find home directory for source discovery")
	}
	root := paths.FindDBStorageDir(e.UserHome)
	if root == "" {
		return "", fmt.Errorf("Cannot auto-detect the source database directory; use --db-dir")
	}
	return root, nil
}

// materialPath follows the configured state directory rather than a fixed
// ~/.tg, so a custom TG_HOME caches material in the same place as its keys.
func materialPath(e env.Env) string {
	return filepath.Join(e.StateDir, filepath.Base(paths.KeyMaterialPath()))
}

func captureCommandHint(e env.Env, opts Options, keysPath string) string {
	dbDir := opts.DBDir
	if dbDir == "" {
		if resolved, err := resolveDBStorage(e, ""); err == nil {
			dbDir = resolved
		}
	}
	if dbDir == "" {
		return "tg keys"
	}
	return paths.KeyRecoveryCommand(dbDir, keysPath)
}
