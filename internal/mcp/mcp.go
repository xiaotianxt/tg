// Package mcp implements the `tg mcp` subcommands: serve tg's read-only
// tools over stdio for local clients, or over Streamable HTTP with OAuth for
// remote ones (docs/mcp-server-design.md). cmd/tg parses flags and calls
// these functions; the tool registry comes from internal/mcp/tools through
// a BuildRegistry function.
package mcp

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/netip"
	"os"
	"slices"
	"strings"
	"time"

	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/mcp/config"
	"github.com/xiaotianxt/tg/internal/mcp/httpx"
	"github.com/xiaotianxt/tg/internal/mcp/oauth"
	"github.com/xiaotianxt/tg/internal/mcp/rpc"
)

// BuildRegistry builds the tools for one server. Size caps in policy apply
// in every mode; session and lookback rules only when remote is set.
type BuildRegistry func(policy config.RemotePolicy, remote bool) (*rpc.Registry, error)

const timestampLayout = "2006-01-02 15:04:05"

// StdioOptions configures `tg mcp stdio`.
type StdioOptions struct {
	// RemotePolicy applies the policy from config.json (--remote-policy).
	RemotePolicy bool
	Version      string
	Build        BuildRegistry
}

// Stdio serves newline-delimited JSON-RPC on in/out until in ends.
func Stdio(ctx context.Context, e env.Env, opts StdioOptions, in io.Reader, out io.Writer) error {
	policy := config.DefaultRemotePolicy()
	if opts.RemotePolicy {
		cfg, err := config.Load(config.Dir(e))
		if err != nil {
			return err
		}
		policy = cfg.Remote
	}
	registry, err := opts.Build(policy, opts.RemotePolicy)
	if err != nil {
		return err
	}
	return rpc.NewServer(registry, opts.Version).ServeStdio(ctx, in, out)
}

// InitOptions configures `tg mcp init`. Zero values leave the existing
// setting unchanged.
type InitOptions struct {
	// PublicURL is the public HTTPS origin (--public-url); required on the
	// first init.
	PublicURL string
	// Listen is the loopback or tailnet address (--listen); required on the
	// first init.
	Listen netip.AddrPort
	// TrustedProxy is the reverse proxy whose X-Forwarded-For is trusted
	// (--trusted-proxy).
	TrustedProxy netip.Addr
	// RedirectURIs are extra OAuth redirect URIs to accept (--redirect-uri).
	RedirectURIs []string
	// PassphraseStdin reads a new owner passphrase from the first line of
	// stdin (--passphrase-stdin).
	PassphraseStdin bool
}

// Init creates or updates config.json and, with PassphraseStdin, owner.json.
func Init(e env.Env, opts InitOptions, stdin io.Reader, out io.Writer) error {
	dir := config.Dir(e)
	cfg, found, err := config.Read(dir)
	if err != nil {
		return err
	}
	if !found {
		if opts.PublicURL == "" {
			return errors.New("--public-url is required for the first init")
		}
		if !opts.Listen.IsValid() {
			return errors.New("--listen is required for the first init")
		}
	}
	if opts.PublicURL != "" {
		cfg.PublicURL = strings.TrimRight(opts.PublicURL, "/")
	}
	if opts.Listen.IsValid() {
		cfg.Listen = opts.Listen
	}
	if opts.TrustedProxy.IsValid() {
		proxy := opts.TrustedProxy
		cfg.TrustedProxy = &proxy
	}
	for _, uri := range opts.RedirectURIs {
		if !slices.Contains(cfg.ExtraRedirectURIs, uri) {
			cfg.ExtraRedirectURIs = append(cfg.ExtraRedirectURIs, uri)
		}
	}
	if err := cfg.Validate(); err != nil {
		return err
	}
	ownerPath := config.OwnerPath(dir)
	if opts.PassphraseStdin {
		line, err := bufio.NewReader(stdin).ReadString('\n')
		if err != nil && !errors.Is(err, io.EOF) {
			return fmt.Errorf("Read passphrase: %w", err)
		}
		owner, err := oauth.NewOwner(strings.TrimRight(line, "\r\n"))
		if err != nil {
			return err
		}
		if err := config.WritePrivateJSON(ownerPath, owner); err != nil {
			return err
		}
	} else {
		var owner oauth.Owner
		found, err := config.ReadPrivateJSON(ownerPath, &owner)
		if err != nil {
			return err
		}
		if !found {
			return errors.New("no owner passphrase yet; rerun with --passphrase-stdin")
		}
	}
	if err := config.WritePrivateJSON(config.ConfigPath(dir), cfg); err != nil {
		return err
	}
	_, err = fmt.Fprintf(out, "MCP config saved. Connector URL: %s\n", cfg.ResourceURL())
	return err
}

// ServeOptions configures `tg mcp serve`.
type ServeOptions struct {
	Version string
	Build   BuildRegistry
	// Probe opens the local history once so that a broken setup fails at
	// startup, not on the first remote request.
	Probe func() error
}

// Serve runs the HTTP server until ctx ends.
func Serve(ctx context.Context, e env.Env, opts ServeOptions) error {
	dir := config.Dir(e)
	cfg, err := config.Load(dir)
	if err != nil {
		return err
	}
	var owner oauth.Owner
	found, err := config.ReadPrivateJSON(config.OwnerPath(dir), &owner)
	if err != nil {
		return err
	}
	if !found {
		return errors.New("no owner passphrase; run `tg mcp init --passphrase-stdin`")
	}
	registry, err := opts.Build(cfg.Remote, true)
	if err != nil {
		return err
	}
	if err := opts.Probe(); err != nil {
		return fmt.Errorf("Cannot open the local history: %w", err)
	}
	h := httpx.NewHandler(cfg, oauth.NewServer(cfg, dir, owner), rpc.NewServer(registry, opts.Version), config.AuditPath(dir), e.Now)
	return httpx.Serve(ctx, h)
}

// Status prints the remote config and registered clients.
func Status(e env.Env, out io.Writer) error {
	dir := config.Dir(e)
	cfg, err := config.Load(dir)
	if err != nil {
		return err
	}
	g, err := oauth.NewStore(dir).Load()
	if err != nil {
		return err
	}
	summary := g.Summarize(e.Now().Unix())
	var b strings.Builder
	fmt.Fprintf(&b, "Connector URL: %s\n", cfg.ResourceURL())
	fmt.Fprintf(&b, "Listening on: %s\n", cfg.Listen)
	fmt.Fprintf(&b, "Active grants: %d; registered clients: %d\n", summary.ActiveGrants, len(summary.Clients))
	for _, c := range summary.Clients {
		name := c.Name
		if name == "" {
			name = "-"
		}
		fmt.Fprintf(&b, "  %s  %s  registered %s\n", c.ID, name, time.Unix(c.Created, 0).In(e.Loc).Format(timestampLayout))
	}
	_, err = io.WriteString(out, b.String())
	return err
}

// RevokeOptions selects what `tg mcp revoke` removes: one client (--client)
// or, with All, every client (--all). Exactly one must be set.
type RevokeOptions struct {
	Client string
	All    bool
}

// Revoke removes OAuth clients and their tokens. A running server re-reads
// the store on every request, so this takes effect immediately.
func Revoke(e env.Env, opts RevokeOptions, out io.Writer) error {
	store := oauth.NewStore(config.Dir(e))
	now := e.Now().Unix()
	var removed int
	var err error
	switch {
	case opts.All && opts.Client == "":
		removed, err = store.RevokeAll(now)
	case !opts.All && opts.Client != "":
		removed, err = store.RevokeClient(opts.Client, now)
	default:
		return errors.New("pass exactly one of --client or --all")
	}
	if err != nil {
		return err
	}
	_, err = fmt.Fprintf(out, "Revoked %d client(s) and their tokens.\n", removed)
	return err
}

// AuditOptions configures `tg mcp audit`.
type AuditOptions struct {
	// Since is the earliest entry shown, in unix seconds (cmd/tg parses
	// --since, default 1d).
	Since int64
}

// Audit prints audit-log entries at or after Since. Unparseable lines are
// skipped; no message content is ever logged.
func Audit(e env.Env, opts AuditOptions, out io.Writer) error {
	path := config.AuditPath(config.Dir(e))
	data, err := os.ReadFile(path)
	if err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("Cannot read %s: %w", path, err)
	}
	w := bufio.NewWriter(out)
	for _, line := range strings.Split(string(data), "\n") {
		entry, ok := parseAuditLine(line)
		if !ok {
			continue
		}
		ts := entry.int("ts")
		if ts < opts.Since {
			continue
		}
		fmt.Fprintf(w, "%s %s %s args=%s rows=%s ok=%s %sms\n",
			time.Unix(ts, 0).In(e.Loc).Format(timestampLayout),
			entry.str("ip"), entry.str("tool"),
			entry.raw("args"), entry.raw("rows"), entry.raw("ok"), entry.raw("ms"))
	}
	return w.Flush()
}

// auditEntry is one decoded audit line; a line that is valid JSON but not an
// object has no fields.
type auditEntry map[string]json.RawMessage

func parseAuditLine(line string) (auditEntry, bool) {
	line = strings.TrimSpace(line)
	if line == "" || !json.Valid([]byte(line)) {
		return nil, false
	}
	var entry auditEntry
	if json.Unmarshal([]byte(line), &entry) != nil {
		return auditEntry{}, true
	}
	return entry, true
}

func (a auditEntry) int(key string) int64 {
	var n int64
	if json.Unmarshal(a[key], &n) != nil {
		return 0
	}
	return n
}

func (a auditEntry) str(key string) string {
	var s string
	if v := a[key]; len(v) == 0 || v[0] != '"' || json.Unmarshal(v, &s) != nil {
		return "-"
	}
	return s
}

func (a auditEntry) raw(key string) string {
	v, ok := a[key]
	if !ok {
		return "null"
	}
	var buf bytes.Buffer
	if json.Compact(&buf, v) != nil {
		return "null"
	}
	return buf.String()
}
