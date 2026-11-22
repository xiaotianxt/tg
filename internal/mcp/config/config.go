// Package config owns remote-mode MCP configuration and the private state
// files under ~/.tg/mcp/: config.json, owner.json, oauth.json and audit.log,
// each mode 0600 inside a 0700 directory.
package config

import (
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"net/netip"
	"os"
	"path/filepath"
	"strings"

	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/paths"
)

const (
	DefaultMaxResultBytes  = 16 * 1024 * 1024
	DefaultMaxLookbackDays = 90
)

// Config is config.json.
type Config struct {
	// PublicURL is the public HTTPS origin without a trailing slash: the
	// OAuth issuer and the base of the protected resource URL.
	PublicURL string `json:"public_url"`
	// Listen must be a loopback or tailnet address (IsPrivateListenAddress).
	Listen netip.AddrPort `json:"listen"`
	// TrustedProxy is the peer whose X-Forwarded-For is trusted for the audit
	// log; nil trusts no peer.
	TrustedProxy *netip.Addr  `json:"trusted_proxy"`
	Remote       RemotePolicy `json:"remote"`
	// ExtraRedirectURIs are accepted at registration in addition to the
	// built-in allowlist.
	ExtraRedirectURIs []string `json:"extra_redirect_uris"`
}

// RemotePolicy limits what remote clients may read. Missing fields in the
// file take the DefaultRemotePolicy values.
type RemotePolicy struct {
	// AllowSessions empty means every session not in DenySessions.
	AllowSessions      []string `json:"allow_sessions"`
	DenySessions       []string `json:"deny_sessions"`
	MaxLookbackDays    int64    `json:"max_lookback_days"`
	ShowContactRemarks bool     `json:"show_contact_remarks"`
	MaxResultBytes     int      `json:"max_result_bytes"`
	// MaxTextChars caps each message; nil keeps every message whole.
	MaxTextChars *int `json:"max_text_chars,omitempty"`
	// AllowMedia lets remote clients view cached images. Off by default:
	// images can show faces, documents and places the text never mentions.
	AllowMedia bool `json:"allow_media"`
}

// DefaultRemotePolicy is the policy used when the file sets none.
func DefaultRemotePolicy() RemotePolicy {
	return RemotePolicy{
		AllowSessions:   []string{},
		DenySessions:    []string{},
		MaxLookbackDays: DefaultMaxLookbackDays,
		MaxResultBytes:  DefaultMaxResultBytes,
	}
}

// Validate checks the invariants every loaded or saved config must hold.
func (c Config) Validate() error {
	host, ok := strings.CutPrefix(c.PublicURL, "https://")
	if !ok {
		return errors.New("public_url must start with https://")
	}
	if host == "" || strings.ContainsAny(host, "/?#") {
		return errors.New("public_url must be an origin such as https://mcp.example.com")
	}
	if !IsPrivateListenAddress(c.Listen.Addr()) {
		return fmt.Errorf("listen address %s must be loopback or a Tailscale address", c.Listen.Addr())
	}
	p := c.Remote
	if p.MaxLookbackDays <= 0 {
		return errors.New("remote.max_lookback_days must be positive")
	}
	if p.MaxResultBytes < 1024 || (p.MaxTextChars != nil && *p.MaxTextChars <= 0) {
		return errors.New("remote.max_result_bytes must be >= 1024 and max_text_chars > 0")
	}
	return nil
}

// PublicHost is the Host header value the public proxy forwards.
func (c Config) PublicHost() string { return strings.TrimPrefix(c.PublicURL, "https://") }

// ResourceURL is the protected resource: the public /mcp endpoint.
func (c Config) ResourceURL() string { return c.PublicURL + "/mcp" }

// IsPrivateListenAddress reports loopback or Tailscale (CGNAT 100.64.0.0/10,
// ULA fd7a:115c:a1e0::/48) addresses: the HTTP server must never listen on
// LAN or wildcard interfaces. IPv4-mapped IPv6 addresses are not private.
func IsPrivateListenAddress(ip netip.Addr) bool {
	if ip.Is4() {
		b := ip.As4()
		return b[0] == 127 || (b[0] == 100 && b[1] >= 64 && b[1] < 128)
	}
	if !ip.Is6() || ip.Zone() != "" {
		return false
	}
	b := ip.As16()
	return ip == netip.IPv6Loopback() || bytes.Equal(b[:6], []byte{0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0})
}

// Dir is the MCP state directory, <state dir>/mcp.
func Dir(e env.Env) string { return filepath.Join(e.StateDir, "mcp") }

func ConfigPath(dir string) string { return filepath.Join(dir, "config.json") }
func OwnerPath(dir string) string  { return filepath.Join(dir, "owner.json") }
func OAuthPath(dir string) string  { return filepath.Join(dir, "oauth.json") }
func AuditPath(dir string) string  { return filepath.Join(dir, "audit.log") }

// Read reads config.json; found is false when the file does not exist.
func Read(dir string) (cfg Config, found bool, err error) {
	cfg = Config{Remote: DefaultRemotePolicy(), ExtraRedirectURIs: []string{}}
	found, err = ReadPrivateJSON(ConfigPath(dir), &cfg)
	return cfg, found, err
}

// Load reads and validates config.json, which must exist.
func Load(dir string) (Config, error) {
	cfg, found, err := Read(dir)
	if err != nil {
		return Config{}, err
	}
	if !found {
		return Config{}, fmt.Errorf("%s not found; run `tg mcp init` first", ConfigPath(dir))
	}
	if err := cfg.Validate(); err != nil {
		return Config{}, err
	}
	return cfg, nil
}

// ReadPrivateJSON decodes path into v, rejecting unknown fields and files
// other users could read or write. found is false when path does not exist,
// in which case v is untouched.
func ReadPrivateJSON(path string, v any) (found bool, err error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("Cannot read %s: %w", path, err)
	}
	if err := CheckPrivate(path); err != nil {
		return false, err
	}
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	if err := dec.Decode(v); err != nil {
		return false, fmt.Errorf("Invalid %s: %w", path, err)
	}
	if dec.More() {
		return false, fmt.Errorf("Invalid %s: trailing data after JSON value", path)
	}
	return true, nil
}

// CheckPrivate fails when path has permission bits beyond 0600 or its
// directory beyond 0700.
func CheckPrivate(path string) error {
	check := func(p string, max fs.FileMode) error {
		st, err := os.Stat(p)
		if err != nil {
			return fmt.Errorf("Cannot stat %s: %w", p, err)
		}
		if mode := st.Mode().Perm(); mode&^max != 0 {
			return fmt.Errorf("%s has mode %o; expected at most %o", p, mode, max)
		}
		return nil
	}
	if err := check(path, 0o600); err != nil {
		return err
	}
	return check(filepath.Dir(path), 0o700)
}

// WritePrivateJSON atomically replaces path with v as indented JSON, mode
// 0600 inside a 0700 directory.
func WritePrivateJSON(path string, v any) error {
	dir := filepath.Dir(path)
	if err := paths.EnsurePrivateDir(dir); err != nil {
		return fmt.Errorf("Cannot create %s: %w", dir, err)
	}
	data, err := MarshalIndent(v)
	if err != nil {
		return err
	}
	nonce := make([]byte, 8)
	_, _ = rand.Read(nonce) // crypto/rand.Read never fails
	temp := filepath.Join(dir, "."+hex.EncodeToString(nonce)+".tmp")
	if err := writeNew(temp, data); err != nil {
		_ = os.Remove(temp)
		return err
	}
	if err := os.Rename(temp, path); err != nil {
		_ = os.Remove(temp)
		return fmt.Errorf("Cannot install %s: %w", path, err)
	}
	return nil
}

func writeNew(path string, data []byte) error {
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return fmt.Errorf("Cannot create %s: %w", path, err)
	}
	_, err = f.Write(data)
	if err == nil {
		err = f.Sync()
	}
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		return fmt.Errorf("Cannot write %s: %w", path, err)
	}
	return nil
}

// MarshalIndent encodes v with two-space indentation and without HTML
// escaping, so text stays as written.
func MarshalIndent(v any) ([]byte, error) {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	if err := enc.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buf.Bytes(), []byte("\n")), nil
}
