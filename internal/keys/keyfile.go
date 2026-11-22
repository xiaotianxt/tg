// Package keys owns all_keys.json (read and atomic private write), the
// page-1 HMAC key check, the PBKDF2-HMAC-SHA512 database key derivation
// grouped by salt, and key_material.bin. It never opens a database and never
// depends on cgo, so it compiles on every target.
package keys

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
)

// PageSize is the SQLCipher page size of every source database.
const PageSize = 4096

var sqliteHeader = []byte("SQLite format 3\x00")

// KeyFile is all_keys.json: relative path -> {"enc_key": hex, "salt": hex}.
type KeyFile map[string]map[string]string

// SourceDatabase is one cataloged *.db file under db_storage.
type SourceDatabase struct {
	RelPath  string
	FullPath string
}

// CollectSourceDatabases lists every *.db file under root, sorted by
// relative path.
func CollectSourceDatabases(root string) ([]SourceDatabase, error) {
	if st, err := os.Stat(root); err != nil || !st.IsDir() {
		return nil, errors.New("Source database directory does not exist")
	}
	var out []SourceDatabase
	err := filepath.WalkDir(root, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			rel, _ := filepath.Rel(root, p)
			return fmt.Errorf("Cannot read source directory %s: %w", rel, err)
		}
		if d.Type().IsRegular() && filepath.Ext(p) == ".db" {
			rel, _ := filepath.Rel(root, p)
			out = append(out, SourceDatabase{RelPath: filepath.ToSlash(rel), FullPath: p})
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	sort.Slice(out, func(i, j int) bool { return out[i].RelPath < out[j].RelPath })
	return out, nil
}

// ReadPage1 reads the first page of a database file.
func ReadPage1(path, label string) ([]byte, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("Cannot open source database %s: %w", label, err)
	}
	defer f.Close()
	page := make([]byte, PageSize)
	if _, err := io.ReadFull(f, page); err != nil {
		return nil, fmt.Errorf("Cannot read page 1 from %s: %w", label, err)
	}
	return page, nil
}

// LoadKeyFile reads path. It returns nil, nil when the file does not exist,
// and rejects non-regular files and files readable by group or others.
func LoadKeyFile(path string) (KeyFile, error) {
	st, err := os.Lstat(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("Cannot inspect keys file %s: %w", path, err)
	}
	if !st.Mode().IsRegular() {
		return nil, fmt.Errorf("Keys path is not a regular file: %s", path)
	}
	if st.Mode().Perm()&0o077 != 0 {
		return nil, fmt.Errorf("Keys file must not be accessible by group or others: %s", path)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("Cannot read keys file %s: %w", path, err)
	}
	defer clear(data)
	var kf KeyFile
	if err := json.Unmarshal(data, &kf); err != nil {
		return nil, fmt.Errorf("Invalid keys file %s: %w", path, err)
	}
	if kf == nil {
		kf = KeyFile{}
	}
	return kf, nil
}

// WriteKeyFile writes kf atomically: a temp file in the same directory with
// mode 0600, pretty JSON with a trailing newline (matching the Rust output),
// then rename over path. A rename replaces the previous file as one step, so
// a crash never leaves a truncated keys file.
func WriteKeyFile(path string, kf KeyFile) error {
	if err := rejectIrregular(path); err != nil {
		return err
	}
	content, err := json.MarshalIndent(kf, "", "  ")
	if err != nil {
		return fmt.Errorf("Cannot encode database keys: %w", err)
	}
	content = append(content, '\n')
	defer clear(content)
	return writeAtomicPrivate(path, content)
}

// rejectIrregular fails when path exists and is not a regular file, so the
// write never follows a symlink or clobbers a directory.
func rejectIrregular(path string) error {
	st, err := os.Lstat(path)
	switch {
	case errors.Is(err, fs.ErrNotExist):
		return nil
	case err != nil:
		return fmt.Errorf("Cannot inspect keys file %s: %w", path, err)
	case !st.Mode().IsRegular():
		return fmt.Errorf("Keys path is not a regular file: %s", path)
	}
	return nil
}
