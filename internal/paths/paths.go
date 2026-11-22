// Package paths locates tg's state directory, the client's database
// directory, and external tools.
package paths

import (
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/xiaotianxt/tg/internal/dict"
)

// StateDir is $TG_HOME, else ~/.tg.
func StateDir() string {
	if v := os.Getenv("TG_HOME"); v != "" {
		return v
	}
	if home := os.Getenv("HOME"); home != "" {
		return filepath.Join(home, ".tg")
	}
	return ".tg"
}

// KeysPath is the default all_keys.json location.
func KeysPath() string { return filepath.Join(StateDir(), "all_keys.json") }

// KeyMaterialPath is the default cached account key material location.
func KeyMaterialPath() string { return filepath.Join(StateDir(), "key_material.bin") }

// KeyRecoveryCommand is the shell command that rebuilds keys for dbDir.
func KeyRecoveryCommand(dbDir, keysPath string) string {
	return "tg keys --db-dir " + shellQuote(dbDir) + " --output " + shellQuote(keysPath)
}

func shellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}

// FindDBStorageDir returns the first db_storage directory under the client's
// account directories, or "" when none exists.
func FindDBStorageDir(home string) string {
	for _, root := range dict.AccountFilesCandidateDirs(home) {
		for _, account := range subdirs(root) {
			if direct := filepath.Join(account, "db_storage"); isDir(direct) {
				return direct
			}
			for _, child := range subdirs(account) {
				if nested := filepath.Join(child, "db_storage"); isDir(nested) {
					return nested
				}
			}
		}
	}
	for _, version := range subdirs(dict.AppSupportDir(home)) {
		for _, account := range subdirs(version) {
			if dir := filepath.Join(account, "db_storage"); isDir(dir) {
				return dir
			}
		}
	}
	return ""
}

func subdirs(dir string) []string {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil
	}
	var out []string
	for _, e := range entries {
		p := filepath.Join(dir, e.Name())
		if isDir(p) {
			out = append(out, p)
		}
	}
	return out
}

func isDir(p string) bool {
	st, err := os.Stat(p)
	return err == nil && st.IsDir()
}

// AccountID returns the account username encoded in an account directory
// name (`<prefix><id>_<random>` or `gh_...`), or "" when it carries none.
func AccountID(dir string) string {
	name := filepath.Base(dir)
	hasPrefix := func(s string) bool {
		return strings.HasPrefix(s, dict.AccountIDPrefix) || strings.HasPrefix(s, "gh_")
	}
	if !hasPrefix(name) {
		return ""
	}
	if i := strings.LastIndexByte(name, '_'); i >= 0 && hasPrefix(name[:i]) {
		return name[:i]
	}
	return name
}

var homebrewBinDirs = []string{"/opt/homebrew/bin", "/usr/local/bin", "/home/linuxbrew/.linuxbrew/bin"}

// FindTool finds an external tool such as ffmpeg. Services started by
// launchd or systemd get a minimal PATH without the Homebrew prefix, so after
// PATH this checks the prefix tg was installed into, then default prefixes.
func FindTool(name string) string {
	dirs := filepath.SplitList(os.Getenv("PATH"))
	if own := ownHomebrewBinDir(); own != "" {
		dirs = append(dirs, own)
	}
	dirs = append(dirs, homebrewBinDirs...)
	for _, d := range dirs {
		p := filepath.Join(d, name)
		if st, err := os.Stat(p); err == nil && st.Mode().IsRegular() {
			return p
		}
	}
	return ""
}

// ownHomebrewBinDir is <prefix>/bin when tg runs from
// <prefix>/Cellar/tg/<version>/bin/tg.
func ownHomebrewBinDir() string {
	exe, err := os.Executable()
	if err != nil {
		return ""
	}
	if exe, err = filepath.EvalSymlinks(exe); err != nil {
		return ""
	}
	for dir := filepath.Dir(exe); dir != filepath.Dir(dir); dir = filepath.Dir(dir) {
		if filepath.Base(dir) == "Cellar" {
			return filepath.Join(filepath.Dir(dir), "bin")
		}
	}
	return ""
}

// EnsurePrivateDir creates path and missing parents with mode 0700, leaving
// existing parents untouched. An existing symlink is an error. Under sudo the
// created directories are handed back to the invoking user.
func EnsurePrivateDir(path string) error {
	if path == "" {
		return nil
	}
	err := os.Mkdir(path, 0o700)
	switch {
	case err == nil:
		if err := os.Chmod(path, 0o700); err != nil {
			return err
		}
		return RestoreInvokingUserOwnership(path)
	case errors.Is(err, fs.ErrExist):
		st, lerr := os.Lstat(path)
		if lerr == nil && st.IsDir() {
			return nil
		}
		return err
	case errors.Is(err, fs.ErrNotExist):
		parent := filepath.Dir(path)
		if parent == path || parent == "." {
			return err
		}
		if err := EnsurePrivateDir(parent); err != nil {
			return err
		}
		return EnsurePrivateDir(path)
	default:
		return err
	}
}

// RestoreInvokingUserOwnership chowns path to SUDO_UID/SUDO_GID when tg runs
// under sudo, so files it creates stay readable by the user.
func RestoreInvokingUserOwnership(path string) error {
	uid, err1 := strconv.Atoi(os.Getenv("SUDO_UID"))
	gid, err2 := strconv.Atoi(os.Getenv("SUDO_GID"))
	if err1 != nil || err2 != nil {
		return nil
	}
	return os.Chown(path, uid, gid)
}
