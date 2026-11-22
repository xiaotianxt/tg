package keys

import (
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func TestWriteKeyFileIsAtomicPrivateAndPretty(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "all_keys.json")
	kf := KeyFile{"session/session.db": {"enc_key": knownKeyHex, "salt": "00"}}
	if err := WriteKeyFile(path, kf); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasSuffix(string(data), "}\n") {
		t.Fatalf("keys file must end with a newline: %q", string(data))
	}
	if !strings.Contains(string(data), "\n  \"session/session.db\"") {
		t.Fatalf("keys file must be pretty-printed: %q", string(data))
	}
	back, err := LoadKeyFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if back["session/session.db"]["enc_key"] != knownKeyHex {
		t.Fatalf("round-trip mismatch: %v", back)
	}
	if runtime.GOOS != "windows" {
		st, _ := os.Stat(path)
		if st.Mode().Perm() != 0o600 {
			t.Fatalf("keys mode = %o, want 600", st.Mode().Perm())
		}
	}
}

func TestLoadKeyFileAbsentIsNil(t *testing.T) {
	kf, err := LoadKeyFile(filepath.Join(t.TempDir(), "missing.json"))
	if err != nil || kf != nil {
		t.Fatalf("absent keys file = %v, %v", kf, err)
	}
}

func TestLoadKeyFileRejectsGroupReadable(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("permission bits are POSIX")
	}
	dir := t.TempDir()
	path := filepath.Join(dir, "all_keys.json")
	if err := os.WriteFile(path, []byte("{}"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := LoadKeyFile(path); err == nil {
		t.Fatal("group/other-readable keys file must be rejected")
	}
}
