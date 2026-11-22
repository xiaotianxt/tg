package capture

import (
	"os"
	"path/filepath"
	"testing"
)

func TestParseMethodAcceptsLoginAliases(t *testing.T) {
	for _, s := range []string{"", "auto"} {
		if m, err := ParseMethod(s); err != nil || m != MethodAuto {
			t.Fatalf("ParseMethod(%q) = (%v, %v)", s, m, err)
		}
	}
	for _, s := range []string{"login", "lldb-login", "lldb-cold", "gdb-login"} {
		if m, err := ParseMethod(s); err != nil || m != MethodLogin {
			t.Fatalf("ParseMethod(%q) = (%v, %v)", s, m, err)
		}
	}
	if _, err := ParseMethod("nope"); err == nil {
		t.Fatal("unknown method must error")
	}
}

func TestDefaultTimeoutPerMethod(t *testing.T) {
	if DefaultTimeout(MethodAuto) != 30 {
		t.Fatalf("auto default = %d, want 30", DefaultTimeout(MethodAuto))
	}
	if DefaultTimeout(MethodLogin) != 180 {
		t.Fatalf("login default = %d, want 180", DefaultTimeout(MethodLogin))
	}
}

func TestLoadExistingKeysReportsMalformedFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "all_keys.json")
	if err := os.WriteFile(path, []byte("not json"), 0o600); err != nil {
		t.Fatal(err)
	}
	// Deliberate change from the Rust flow, which silently replaced a corrupt
	// keys file: a malformed existing file is reported instead.
	if _, err := loadExistingKeys(path); err == nil {
		t.Fatal("malformed existing keys file must be reported")
	}
}

func TestLoadExistingKeysAbsentIsEmpty(t *testing.T) {
	kf, err := loadExistingKeys(filepath.Join(t.TempDir(), "missing.json"))
	if err != nil || kf == nil || len(kf) != 0 {
		t.Fatalf("absent keys = (%v, %v)", kf, err)
	}
}
