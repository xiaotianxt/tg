package httpx

import (
	"context"
	"crypto/pbkdf2"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"net/http/httptest"
	"net/netip"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/xiaotianxt/tg/internal/mcp/config"
	"github.com/xiaotianxt/tg/internal/mcp/oauth"
	"github.com/xiaotianxt/tg/internal/mcp/rpc"
)

const (
	passphrase = "correct horse battery staple"
	redirect   = "https://chatgpt.com/connector_platform_oauth_redirect"
	host       = "mcp.example.com"
)

func TestParseForm(t *testing.T) {
	form := ParseForm("a=1+2&b=%E4%BD%A0%3D&c&bad=%zz&trunc=%4&utf=%FF&a=3&&k%20x=v")
	want := map[string]string{"a": "3", "b": "你=", "c": "", "k x": "v"}
	if len(form) != len(want) {
		t.Errorf("form = %v", form)
	}
	for k, v := range want {
		if form[k] != v {
			t.Errorf("%s = %q, want %q", k, form[k], v)
		}
	}
}

func TestParseBasic(t *testing.T) {
	c, ok := ParseBasic("Basic " + base64.StdEncoding.EncodeToString([]byte("tgc_a%3Ab:s+3:x")))
	if !ok || c.ID != "tgc_a:b" || c.Secret != "s 3:x" {
		t.Errorf("got %+v %v", c, ok)
	}
	for _, bad := range []string{"Bearer x", "Basic !!!", "Basic " + base64.StdEncoding.EncodeToString([]byte("nocolon")), ""} {
		if _, ok := ParseBasic(bad); ok {
			t.Errorf("%q accepted", bad)
		}
	}
}

type harness struct {
	t     *testing.T
	h     *Handler
	dir   string
	clock time.Time
}

func newHarness(t *testing.T) *harness {
	dir := filepath.Join(t.TempDir(), "mcp")
	proxy := netip.MustParseAddr("100.70.26.98")
	cfg := config.Config{PublicURL: "https://" + host, Listen: netip.MustParseAddrPort("100.87.128.22:8765"), TrustedProxy: &proxy, Remote: config.DefaultRemotePolicy()}
	salt := make([]byte, 16)
	key, err := pbkdf2.Key(sha256.New, passphrase, salt, 1000, 32)
	if err != nil {
		t.Fatal(err)
	}
	owner := oauth.Owner{Salt: hex.EncodeToString(salt), Rounds: 1000, Hash: hex.EncodeToString(key)}
	registry, err := rpc.NewRegistry(rpc.Tool{
		Name: "list_sessions", InputSchema: json.RawMessage(`{"type":"object"}`),
		Handler: func(context.Context, json.RawMessage) (rpc.Result, error) {
			return rpc.JSONResult(json.RawMessage(`{"sessions":[]}`), 3), nil
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	hs := &harness{t: t, dir: dir, clock: time.Unix(1_800_000_000, 0)}
	hs.h = NewHandler(cfg, oauth.NewServer(cfg, dir, owner), rpc.NewServer(registry, "test"), config.AuditPath(dir), func() time.Time { return hs.clock })
	return hs
}

func (hs *harness) do(method, target, body string, headers ...string) *httptest.ResponseRecorder {
	hs.t.Helper()
	r := httptest.NewRequest(method, target, strings.NewReader(body))
	r.Host = host
	for i := 0; i+1 < len(headers); i += 2 {
		if headers[i] == "Host" {
			r.Host = headers[i+1]
		} else {
			r.Header.Set(headers[i], headers[i+1])
		}
	}
	w := httptest.NewRecorder()
	hs.h.ServeHTTP(w, r)
	if w.Header().Get("Cache-Control") != "no-store" || w.Header().Get("X-Content-Type-Options") != "nosniff" {
		hs.t.Errorf("%s %s: missing no-store/nosniff headers", method, target)
	}
	return w
}

func decode(t *testing.T, w *httptest.ResponseRecorder) map[string]any {
	t.Helper()
	var out map[string]any
	if err := json.Unmarshal(w.Body.Bytes(), &out); err != nil {
		t.Fatalf("%d %s: %v", w.Code, w.Body, err)
	}
	return out
}

// signIn runs registration, consent and code exchange; it returns the
// token response.
func (hs *harness) signIn() (clientID string, tokens map[string]any) {
	t := hs.t
	t.Helper()
	w := hs.do("POST", "/oauth/register", `{"redirect_uris":["`+redirect+`"],"client_name":"ChatGPT"}`)
	if w.Code != 201 {
		t.Fatalf("register: %d %s", w.Code, w.Body)
	}
	clientID = decode(t, w)["client_id"].(string)

	verifier := strings.Repeat("v", 50)
	q := url.Values{"response_type": {"code"}, "client_id": {clientID}, "redirect_uri": {redirect},
		"code_challenge": {oauth.PKCEChallenge(verifier)}, "code_challenge_method": {"S256"},
		"state": {"xyz"}, "resource": {"https://" + host + "/mcp"}}
	w = hs.do("GET", "/oauth/authorize?"+q.Encode(), "")
	if w.Code != 200 || w.Header().Get("X-Frame-Options") != "DENY" || !strings.HasPrefix(w.Header().Get("Content-Type"), "text/html") {
		t.Fatalf("consent page: %d %v", w.Code, w.Header())
	}
	_, rest, _ := strings.Cut(w.Body.String(), `name=request value="`)
	csrf, _, _ := strings.Cut(rest, `"`)

	form := url.Values{"request": {csrf}, "passphrase": {passphrase}, "decision": {"allow"}}
	w = hs.do("POST", "/oauth/authorize", form.Encode(), "Origin", "https://"+host)
	if w.Code != 302 {
		t.Fatalf("submit: %d %s", w.Code, w.Body)
	}
	loc, err := url.Parse(w.Header().Get("Location"))
	if err != nil || loc.Query().Get("state") != "xyz" || loc.Query().Get("iss") != "https://"+host {
		t.Fatalf("location %v: %v", loc, err)
	}

	form = url.Values{"grant_type": {"authorization_code"}, "code": {loc.Query().Get("code")}, "redirect_uri": {redirect},
		"client_id": {clientID}, "code_verifier": {verifier}, "resource": {"https://" + host + "/mcp"}}
	w = hs.do("POST", "/oauth/token", form.Encode())
	if w.Code != 200 {
		t.Fatalf("token: %d %s", w.Code, w.Body)
	}
	return clientID, decode(t, w)
}

const toolCall = `{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"list_sessions","arguments":{"query":"secret"}}}`

func TestAuthorizationCodeFlowWithPKCEAndRefresh(t *testing.T) {
	hs := newHarness(t)
	clientID, tokens := hs.signIn()
	access := tokens["access_token"].(string)

	w := hs.do("POST", "/mcp", toolCall, "Authorization", "Bearer "+access, "Mcp-Protocol-Version", "2026-07-28",
		"X-Forwarded-For", "203.0.113.9, 100.70.26.98")
	if w.Code != 200 || !strings.Contains(w.Body.String(), `"structuredContent":{"sessions":[]}`) {
		t.Fatalf("tool call: %d %s", w.Code, w.Body)
	}
	// The audit line names the client and argument keys, never values. The
	// peer is not the trusted proxy, so X-Forwarded-For is ignored.
	audit, err := os.ReadFile(config.AuditPath(hs.dir))
	if err != nil {
		t.Fatal(err)
	}
	line := string(audit)
	if !strings.Contains(line, `"client":"`+clientID+`"`) || !strings.Contains(line, `"args":["query"]`) ||
		!strings.Contains(line, `"ip":"192.0.2.1"`) || !strings.Contains(line, `"rows":3`) || strings.Contains(line, "secret") {
		t.Errorf("audit line: %s", line)
	}

	refresh := url.Values{"grant_type": {"refresh_token"}, "refresh_token": {tokens["refresh_token"].(string)}, "client_id": {clientID}}.Encode()
	w = hs.do("POST", "/oauth/token", refresh)
	if w.Code != 200 {
		t.Fatalf("refresh: %d %s", w.Code, w.Body)
	}
	newAccess := decode(t, w)["access_token"].(string)
	if w := hs.do("POST", "/mcp", toolCall, "Authorization", "Bearer "+newAccess); w.Code != 200 {
		t.Fatalf("new access token: %d", w.Code)
	}
	// Replaying the rotated refresh token revokes the whole grant.
	if w := hs.do("POST", "/oauth/token", refresh); w.Code != 400 {
		t.Fatalf("replay: %d %s", w.Code, w.Body)
	}
	for _, tok := range []string{access, newAccess} {
		if w := hs.do("POST", "/mcp", toolCall, "Authorization", "Bearer "+tok); w.Code != 401 {
			t.Errorf("token survived grant revocation: %d", w.Code)
		}
	}

	// Access tokens expire after an hour.
	_, tokens = hs.signIn()
	hs.clock = hs.clock.Add(oauth.AccessTTL * time.Second)
	if w := hs.do("POST", "/mcp", toolCall, "Authorization", "Bearer "+tokens["access_token"].(string)); w.Code != 401 {
		t.Errorf("expired token: %d", w.Code)
	}
}

func TestTrustedProxyForwardedFor(t *testing.T) {
	hs := newHarness(t)
	r := httptest.NewRequest("POST", "/mcp", nil)
	r.RemoteAddr = "100.70.26.98:40000"
	r.Header.Set("X-Forwarded-For", " 203.0.113.9 , 100.70.26.98")
	if ip := hs.h.clientIP(r); ip.String() != "203.0.113.9" {
		t.Errorf("ip = %s", ip)
	}
	r.Header.Set("X-Forwarded-For", "garbage")
	if ip := hs.h.clientIP(r); ip.String() != "100.70.26.98" {
		t.Errorf("ip = %s", ip)
	}
}

func TestCheckOrder(t *testing.T) {
	hs := newHarness(t)
	_, tokens := hs.signIn()
	bearer := "Bearer " + tokens["access_token"].(string)
	big := strings.Repeat("x", MaxBodyBytes+1)
	ping := `{"jsonrpc":"2.0","id":1,"method":"ping"}`

	for _, tc := range []struct {
		name         string
		method, path string
		body         string
		headers      []string
		status       int
	}{
		{"wrong host beats body size", "POST", "/mcp", big, []string{"Host", "evil.example"}, 421},
		{"missing host", "GET", "/.well-known/oauth-authorization-server", "", []string{"Host", ""}, 421},
		{"listen address is a valid host", "GET", "/.well-known/oauth-authorization-server", "", []string{"Host", "100.87.128.22:8765"}, 200},
		{"host is case-insensitive", "GET", "/.well-known/openid-configuration", "", []string{"Host", "MCP.example.com"}, 200},
		{"body size beats auth", "POST", "/mcp", big, nil, 413},
		{"body size applies to oauth", "POST", "/oauth/token", big, nil, 413},
		{"foreign origin beats auth", "POST", "/mcp", ping, []string{"Origin", "https://evil.example"}, 403},
		{"null origin", "POST", "/mcp", ping, []string{"Origin", "null", "Authorization", bearer}, 403},
		{"no token", "POST", "/mcp", ping, nil, 401},
		{"bad token beats version", "POST", "/mcp", ping, []string{"Authorization", "Bearer tgat_forged", "Mcp-Protocol-Version", "1999-01-01"}, 401},
		{"unsupported version", "POST", "/mcp", ping, []string{"Authorization", bearer, "Mcp-Protocol-Version", "1999-01-01"}, 400},
		{"own origin", "POST", "/mcp", ping, []string{"Authorization", bearer, "Origin", "https://" + host}, 200},
		{"lowercase bearer", "POST", "/mcp", ping, []string{"Authorization", "bearer " + tokens["access_token"].(string)}, 200},
		{"notification", "POST", "/mcp", `{"jsonrpc":"2.0","method":"notifications/initialized"}`, []string{"Authorization", bearer}, 202},
		{"parse error is a JSON-RPC reply", "POST", "/mcp", `{bad`, []string{"Authorization", bearer}, 200},
		{"no streams", "GET", "/mcp", "", nil, 405},
		{"no sessions", "DELETE", "/mcp", "", nil, 405},
		{"unknown path", "GET", "/nope", "", nil, 404},
		{"wrong method on metadata", "POST", "/.well-known/oauth-protected-resource", "", nil, 404},
	} {
		w := hs.do(tc.method, tc.path, tc.body, tc.headers...)
		if w.Code != tc.status {
			t.Errorf("%s: %d, want %d (%s)", tc.name, w.Code, tc.status, w.Body)
		}
	}

	w := hs.do("POST", "/mcp", ping)
	challenge := w.Header().Get("WWW-Authenticate")
	if challenge != `Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", scope="tg.read"` ||
		decode(t, w)["error"] != "unauthorized" {
		t.Errorf("no token: %q %s", challenge, w.Body)
	}
	w = hs.do("POST", "/mcp", ping, "Authorization", "Bearer tgat_forged")
	if !strings.HasSuffix(w.Header().Get("WWW-Authenticate"), `, error="invalid_token"`) || decode(t, w)["error"] != "invalid_token" {
		t.Errorf("bad token: %q %s", w.Header().Get("WWW-Authenticate"), w.Body)
	}
	if w := hs.do("GET", "/mcp", ""); w.Header().Get("Allow") != "POST" {
		t.Errorf("405 without Allow: POST")
	}
	w = hs.do("GET", "/.well-known/oauth-protected-resource/mcp", "")
	if body := decode(t, w); body["resource"] != "https://mcp.example.com/mcp" {
		t.Errorf("resource metadata: %v", body)
	}
}
