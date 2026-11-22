package config

import (
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestListenAddressesAreLimitedToLoopbackAndTailnet(t *testing.T) {
	for _, ok := range []string{"127.0.0.1", "127.8.0.1", "100.87.128.22", "100.64.0.1", "100.127.255.255", "::1", "fd7a:115c:a1e0::5"} {
		if !IsPrivateListenAddress(netip.MustParseAddr(ok)) {
			t.Errorf("%s should be allowed", ok)
		}
	}
	for _, bad := range []string{"0.0.0.0", "192.168.1.5", "100.63.255.255", "100.128.0.1", "::", "fd00::1", "fd7a:115c:a1e1::1", "::ffff:127.0.0.1", "fe80::1%en0"} {
		if IsPrivateListenAddress(netip.MustParseAddr(bad)) {
			t.Errorf("%s should be rejected", bad)
		}
	}
	if IsPrivateListenAddress(netip.Addr{}) {
		t.Error("the zero address should be rejected")
	}
}

func validConfig() Config {
	return Config{PublicURL: "https://mcp.example.com", Listen: netip.MustParseAddrPort("127.0.0.1:8765"), Remote: DefaultRemotePolicy()}
}

func TestValidate(t *testing.T) {
	if err := validConfig().Validate(); err != nil {
		t.Fatal(err)
	}
	for _, bad := range []string{"http://mcp.example.com", "https://mcp.example.com/mcp", "https://", "https://a.com?x", "https://a.com#f"} {
		c := validConfig()
		c.PublicURL = bad
		if c.Validate() == nil {
			t.Errorf("public_url %q should be rejected", bad)
		}
	}
	zero := 0
	for name, mutate := range map[string]func(*Config){
		"wildcard listen": func(c *Config) { c.Listen = netip.MustParseAddrPort("0.0.0.0:8765") },
		"zero lookback":   func(c *Config) { c.Remote.MaxLookbackDays = 0 },
		"tiny result cap": func(c *Config) { c.Remote.MaxResultBytes = 1023 },
		"zero text chars": func(c *Config) { c.Remote.MaxTextChars = &zero },
	} {
		c := validConfig()
		mutate(&c)
		if c.Validate() == nil {
			t.Errorf("%s should be rejected", name)
		}
	}
}

func TestReadFillsPolicyDefaultsAndRejectsUnknownFields(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "mcp")
	if err := WritePrivateJSON(ConfigPath(dir), validConfig()); err != nil {
		t.Fatal(err)
	}
	write := func(body string) {
		t.Helper()
		if err := os.WriteFile(ConfigPath(dir), []byte(body), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	write(`{"public_url":"https://mcp.example.com","listen":"[::1]:8765","remote":{"allow_media":true}}`)
	cfg, err := Load(dir)
	if err != nil {
		t.Fatal(err)
	}
	if !cfg.Remote.AllowMedia || cfg.Remote.MaxLookbackDays != DefaultMaxLookbackDays || cfg.Remote.MaxResultBytes != DefaultMaxResultBytes {
		t.Errorf("policy = %+v", cfg.Remote)
	}
	if cfg.TrustedProxy != nil || cfg.Listen.String() != "[::1]:8765" {
		t.Errorf("config = %+v", cfg)
	}

	write(`{"public_url":"https://mcp.example.com","listen":"127.0.0.1:1","remote":{"bogus":1}}`)
	if _, err := Load(dir); err == nil || !strings.Contains(err.Error(), "bogus") {
		t.Errorf("unknown nested field: err = %v", err)
	}
}

func TestPrivateFilesRejectLooseModes(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "mcp")
	path := ConfigPath(dir)
	if err := WritePrivateJSON(path, validConfig()); err != nil {
		t.Fatal(err)
	}
	if st, _ := os.Stat(path); st.Mode().Perm() != 0o600 {
		t.Fatalf("mode = %o", st.Mode().Perm())
	}
	if st, _ := os.Stat(dir); st.Mode().Perm() != 0o700 {
		t.Fatalf("dir mode = %o", st.Mode().Perm())
	}
	if err := os.Chmod(path, 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(dir); err == nil || !strings.Contains(err.Error(), "has mode 644; expected at most 600") {
		t.Errorf("err = %v", err)
	}
	if err := os.Chmod(path, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(dir); err == nil || !strings.Contains(err.Error(), "expected at most 700") {
		t.Errorf("err = %v", err)
	}
	if _, found, err := Read(filepath.Join(dir, "missing")); found || err != nil {
		t.Errorf("missing dir: found=%v err=%v", found, err)
	}
}
