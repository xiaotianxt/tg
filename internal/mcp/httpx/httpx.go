// Package httpx is the Streamable HTTP transport (JSON responses only, no
// sessions, no SSE) plus the OAuth endpoints.
//
// TLS terminates at the public reverse proxy; the proxy-to-tg hop runs over
// the tailnet. Checks run in this order: Host (421), body size (413), then
// routing; POST /mcp further checks Origin (403), the bearer token (401) and
// MCP-Protocol-Version (400). Every response carries Cache-Control: no-store
// and X-Content-Type-Options: nosniff.
package httpx

import (
	"context"
	"encoding/base64"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/netip"
	"os"
	"slices"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/xiaotianxt/tg/internal/logx"
	"github.com/xiaotianxt/tg/internal/mcp/config"
	"github.com/xiaotianxt/tg/internal/mcp/oauth"
	"github.com/xiaotianxt/tg/internal/mcp/rpc"
)

// MaxBodyBytes caps every request body.
const MaxBodyBytes = 1024 * 1024

const consentCSP = "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'"

// Handler serves /mcp, the OAuth endpoints and their metadata.
type Handler struct {
	cfg       config.Config
	auth      *oauth.Server
	rpc       *rpc.Server
	auditPath string
	now       func() time.Time

	// toolMu runs one MCP request at a time: there is one owner, and the
	// heavy reads already parallelize internally.
	toolMu sync.Mutex
	// auditMu keeps audit lines whole.
	auditMu sync.Mutex
}

// NewHandler builds the handler; auditPath is appended to for every tool call.
func NewHandler(cfg config.Config, auth *oauth.Server, server *rpc.Server, auditPath string, now func() time.Time) *Handler {
	return &Handler{cfg: cfg, auth: auth, rpc: server, auditPath: auditPath, now: now}
}

// Serve listens on cfg.Listen until ctx ends.
func Serve(ctx context.Context, h *Handler) error {
	listener, err := net.Listen("tcp", h.cfg.Listen.String())
	if err != nil {
		return fmt.Errorf("Cannot listen on %s: %w", h.cfg.Listen, err)
	}
	logx.Infof("tg mcp listening on http://%s for %s", h.cfg.Listen, h.cfg.ResourceURL())
	srv := &http.Server{Handler: h, ReadHeaderTimeout: 10 * time.Second}
	errc := make(chan error, 1)
	go func() { errc <- srv.Serve(listener) }()
	select {
	case err := <-errc:
		return fmt.Errorf("HTTP server failed: %w", err)
	case <-ctx.Done():
		shutdown, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := srv.Shutdown(shutdown); err != nil && !errors.Is(err, context.DeadlineExceeded) {
			return err
		}
		return nil
	}
}

type reply struct {
	status      int
	contentType string
	body        []byte
	headers     [][2]string
}

func jsonReply(status int, v any) reply {
	b, err := rpc.Marshal(v)
	if err != nil {
		panic(fmt.Sprintf("httpx: encode %T: %v", v, err))
	}
	return reply{status: status, contentType: "application/json", body: b}
}

func textReply(status int, text string) reply {
	return reply{status: status, contentType: "text/plain; charset=utf-8", body: []byte(text)}
}

func fromOAuth(r oauth.Reply) reply {
	switch r.Kind {
	case oauth.ReplyJSON:
		return reply{status: r.Status, contentType: "application/json", body: r.Body}
	case oauth.ReplyHTML:
		return reply{status: r.Status, contentType: "text/html; charset=utf-8", body: r.Body, headers: [][2]string{
			{"X-Frame-Options", "DENY"},
			{"Content-Security-Policy", consentCSP},
			{"Referrer-Policy", "no-referrer"},
		}}
	case oauth.ReplyRedirect:
		return reply{status: r.Status, contentType: "text/plain; charset=utf-8", headers: [][2]string{{"Location", r.Location}}}
	}
	panic(fmt.Sprintf("httpx: unknown oauth reply kind %d", r.Kind))
}

// request is an incoming request whose body has been read within the cap.
type request struct {
	*http.Request
	body     []byte
	clientIP netip.Addr
	started  time.Time
}

func (h *Handler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	started := h.now()
	path := r.URL.EscapedPath()
	ip := h.clientIP(r)
	var out reply
	if !h.hostAllowed(r.Host) {
		out = textReply(421, "Misdirected request")
	} else if body, err := io.ReadAll(io.LimitReader(r.Body, MaxBodyBytes+1)); err != nil || len(body) > MaxBodyBytes {
		out = textReply(413, "Request body too large")
	} else {
		out = h.route(&request{Request: r, body: body, clientIP: ip, started: started})
	}
	// Never log query strings: they carry authorization codes.
	logx.Infof("%s %s -> %d (%s)", r.Method, path, out.status, ip)
	header := w.Header()
	header.Set("Content-Type", out.contentType)
	header.Set("Cache-Control", "no-store")
	header.Set("X-Content-Type-Options", "nosniff")
	for _, kv := range out.headers {
		header.Set(kv[0], kv[1])
	}
	w.WriteHeader(out.status)
	if _, err := w.Write(out.body); err != nil {
		logx.Warnf("Failed to send response: %v", err)
	}
}

func (h *Handler) route(r *request) reply {
	now := h.now().Unix()
	switch path := r.URL.EscapedPath(); {
	case r.Method == http.MethodGet && (path == "/.well-known/oauth-protected-resource" || path == "/.well-known/oauth-protected-resource/mcp"):
		return fromOAuth(h.auth.ProtectedResourceMetadata())
	case r.Method == http.MethodGet && (path == "/.well-known/oauth-authorization-server" || path == "/.well-known/openid-configuration"):
		return fromOAuth(h.auth.AuthorizationServerMetadata())
	case r.Method == http.MethodPost && path == "/oauth/register":
		return fromOAuth(h.auth.Register(now, r.body))
	case r.Method == http.MethodGet && path == "/oauth/authorize":
		return fromOAuth(h.auth.AuthorizePage(now, ParseForm(r.URL.RawQuery)))
	case r.Method == http.MethodPost && path == "/oauth/authorize":
		return fromOAuth(h.auth.AuthorizeSubmit(now, ParseForm(lossy(r.body))))
	case r.Method == http.MethodPost && path == "/oauth/token":
		var basic *oauth.Credentials
		if c, ok := ParseBasic(r.Header.Get("Authorization")); ok {
			basic = &c
		}
		return fromOAuth(h.auth.Token(now, ParseForm(lossy(r.body)), basic))
	case r.Method == http.MethodPost && path == "/mcp":
		return h.mcp(r, now)
	case (r.Method == http.MethodGet || r.Method == http.MethodDelete) && path == "/mcp":
		// No server-initiated streams and no sessions to end.
		out := textReply(405, "Method not allowed")
		out.headers = append(out.headers, [2]string{"Allow", "POST"})
		return out
	}
	return textReply(404, "Not found")
}

func (h *Handler) mcp(r *request, now int64) reply {
	if origin := r.Header.Values("Origin"); len(origin) > 0 && !h.originAllowed(origin[0]) {
		return textReply(403, "Forbidden origin")
	}
	bearer, ok := bearerToken(r.Header.Get("Authorization"))
	if !ok {
		return h.unauthorized("")
	}
	client, err := h.auth.Authenticate(now, bearer)
	switch {
	case errors.Is(err, oauth.ErrInvalidToken):
		return h.unauthorized("invalid_token")
	case err != nil:
		logx.Errorf("oauth: %v", err)
		return textReply(500, "Internal server error")
	}
	if version := r.Header.Values("Mcp-Protocol-Version"); len(version) > 0 && !slices.Contains(rpc.SupportedVersions, version[0]) {
		return jsonReply(400, map[string]string{"error": "Unsupported MCP-Protocol-Version: " + version[0]})
	}
	h.toolMu.Lock()
	handled := h.rpc.Handle(r.Context(), r.body)
	h.toolMu.Unlock()
	if handled.Call != nil {
		h.audit(client, r.clientIP, handled.Call, r.started)
	}
	if handled.Response == nil {
		return textReply(202, "")
	}
	return reply{status: 200, contentType: "application/json", body: handled.Response}
}

func bearerToken(header string) (string, bool) {
	token, ok := strings.CutPrefix(header, "Bearer ")
	if !ok {
		token, ok = strings.CutPrefix(header, "bearer ")
	}
	return strings.TrimSpace(token), ok
}

func (h *Handler) unauthorized(code string) reply {
	challenge := fmt.Sprintf(`Bearer resource_metadata="%s/.well-known/oauth-protected-resource", scope="%s"`, h.cfg.PublicURL, oauth.Scope)
	body := "unauthorized"
	if code != "" {
		challenge += fmt.Sprintf(`, error="%s"`, code)
		body = code
	}
	out := jsonReply(401, map[string]string{"error": body})
	out.headers = append(out.headers, [2]string{"WWW-Authenticate", challenge})
	return out
}

// hostAllowed admits the public Host, or the listen address for direct local
// checks. A DNS rebinding page would carry its own hostname.
func (h *Handler) hostAllowed(host string) bool {
	return host != "" && (strings.EqualFold(host, h.cfg.PublicHost()) || host == h.cfg.Listen.String())
}

// originAllowed admits only the configured public origin. Browsers send
// Origin on cross-site requests; server-side clients send none.
func (h *Handler) originAllowed(origin string) bool {
	return strings.EqualFold(strings.TrimRight(origin, "/"), h.cfg.PublicURL)
}

// clientIP is the TCP peer, or the first X-Forwarded-For address when the
// peer is the trusted proxy.
func (h *Handler) clientIP(r *http.Request) netip.Addr {
	peer := netip.IPv4Unspecified()
	if ap, err := netip.ParseAddrPort(r.RemoteAddr); err == nil {
		peer = ap.Addr().Unmap()
	}
	if h.cfg.TrustedProxy == nil || peer != h.cfg.TrustedProxy.Unmap() {
		return peer
	}
	first, _, _ := strings.Cut(r.Header.Get("X-Forwarded-For"), ",")
	if ip, err := netip.ParseAddr(strings.TrimSpace(first)); err == nil {
		return ip
	}
	return peer
}

// audit appends one line per tool call: argument names only, never values.
func (h *Handler) audit(client string, ip netip.Addr, call *rpc.CallRecord, started time.Time) {
	args := call.ArgumentKeys
	if args == nil {
		args = []string{}
	}
	line, err := rpc.Marshal(map[string]any{
		"ts":     h.now().Unix(),
		"client": client,
		"ip":     ip.String(),
		"tool":   call.Tool,
		"args":   args,
		"rows":   call.Rows,
		"ok":     call.OK,
		"ms":     h.now().Sub(started).Milliseconds(),
	})
	if err != nil {
		panic(fmt.Sprintf("httpx: encode audit line: %v", err))
	}
	h.auditMu.Lock()
	defer h.auditMu.Unlock()
	f, err := os.OpenFile(h.auditPath, os.O_WRONLY|os.O_CREATE|os.O_APPEND, 0o600)
	if err == nil {
		_, err = f.Write(append(line, '\n'))
		if cerr := f.Close(); err == nil {
			err = cerr
		}
	}
	if err != nil {
		logx.Errorf("Cannot write audit log %s: %v", h.auditPath, err)
	}
}

// lossy replaces invalid UTF-8 so form decoding sees text.
func lossy(b []byte) string {
	if utf8.Valid(b) {
		return string(b)
	}
	return strings.ToValidUTF8(string(b), "�")
}

// ParseForm decodes an application/x-www-form-urlencoded string. '+' is a
// space; a pair with a bad escape or invalid UTF-8 is dropped; a repeated
// key keeps the last value.
func ParseForm(input string) oauth.Params {
	form := oauth.Params{}
	for _, pair := range strings.Split(input, "&") {
		if pair == "" {
			continue
		}
		k, v, _ := strings.Cut(pair, "=")
		key, ok1 := percentDecode(k)
		value, ok2 := percentDecode(v)
		if ok1 && ok2 {
			form[key] = value
		}
	}
	return form
}

func percentDecode(s string) (string, bool) {
	out := make([]byte, 0, len(s))
	for i := 0; i < len(s); i++ {
		switch c := s[i]; c {
		case '+':
			out = append(out, ' ')
		case '%':
			if i+2 >= len(s) {
				return "", false
			}
			hi, ok1 := unhex(s[i+1])
			lo, ok2 := unhex(s[i+2])
			if !ok1 || !ok2 {
				return "", false
			}
			out = append(out, hi<<4|lo)
			i += 2
		default:
			out = append(out, c)
		}
	}
	if !utf8.Valid(out) {
		return "", false
	}
	return string(out), true
}

func unhex(c byte) (byte, bool) {
	switch {
	case '0' <= c && c <= '9':
		return c - '0', true
	case 'a' <= c && c <= 'f':
		return c - 'a' + 10, true
	case 'A' <= c && c <= 'F':
		return c - 'A' + 10, true
	}
	return 0, false
}

// ParseBasic decodes HTTP Basic client credentials. Per RFC 6749 §2.3.1 the
// id and secret are form-encoded before base64.
func ParseBasic(header string) (oauth.Credentials, bool) {
	encoded, ok := strings.CutPrefix(header, "Basic ")
	if !ok {
		encoded, ok = strings.CutPrefix(header, "basic ")
	}
	if !ok {
		return oauth.Credentials{}, false
	}
	decoded, err := base64.StdEncoding.DecodeString(strings.TrimSpace(encoded))
	if err != nil || !utf8.Valid(decoded) {
		return oauth.Credentials{}, false
	}
	rawID, rawSecret, ok := strings.Cut(string(decoded), ":")
	if !ok {
		return oauth.Credentials{}, false
	}
	id, ok1 := percentDecode(rawID)
	secret, ok2 := percentDecode(rawSecret)
	if !ok1 || !ok2 {
		return oauth.Credentials{}, false
	}
	return oauth.Credentials{ID: id, Secret: secret}, true
}
