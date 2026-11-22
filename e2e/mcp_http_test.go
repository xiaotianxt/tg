package e2e

import (
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
	"time"
)

const (
	publicHost = "mcp.example.test"
	publicURL  = "https://" + publicHost
	// Inspector-style loopback callback, allowlisted with --redirect-uri.
	redirectURI = "http://127.0.0.1:6274/oauth/callback"
	passphrase  = "correct horse battery staple e2e"
)

func freePort(t *testing.T) int {
	t.Helper()
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port
}

// httpServer is one running `tg mcp serve`.
type httpServer struct {
	t      *testing.T
	listen string
	client *http.Client
}

// startHTTPServer initialises remote mode in e's HOME and serves it. The
// lookback is widened so the 2024 fixture is visible.
func startHTTPServer(t *testing.T, e *Env) *httpServer {
	t.Helper()
	listen := fmt.Sprintf("127.0.0.1:%d", freePort(t))
	e.Stdin = passphrase + "\n"
	r := e.run(t, "mcp", "init", "--public-url", publicURL+"/", "--listen", listen,
		"--redirect-uri", redirectURI, "--passphrase-stdin").mustSucceed(t)
	if r.Stdout != "MCP config saved. Connector URL: "+publicURL+"/mcp\n" {
		t.Errorf("init stdout: %q", r.Stdout)
	}
	e.Stdin = ""
	configPath := filepath.Join(e.Home, ".tg", "mcp", "config.json")
	var config map[string]any
	data, err := os.ReadFile(configPath)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(data, &config); err != nil {
		t.Fatal(err)
	}
	config["remote"] = map[string]any{"max_lookback_days": 100000, "show_contact_remarks": true}
	if err := os.WriteFile(configPath, []byte(mustJSON(t, config)), 0o600); err != nil {
		t.Fatal(err)
	}

	cmd := exec.Command(tgBin, fixtureArgs("mcp", "serve")...)
	cmd.Env = e.environ()
	cmd.Dir = e.Tmp
	logFile, err := os.Create(filepath.Join(e.Tmp, "serve.log"))
	if err != nil {
		t.Fatal(err)
	}
	cmd.Stdout, cmd.Stderr = logFile, logFile
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		cmd.Process.Kill()
		cmd.Wait()
		logFile.Close()
	})
	deadline := time.Now().Add(10 * time.Second)
	for {
		conn, err := net.Dial("tcp", listen)
		if err == nil {
			conn.Close()
			break
		}
		if time.Now().After(deadline) {
			log, _ := os.ReadFile(logFile.Name())
			t.Fatalf("tg mcp serve did not listen on %s:\n%s", listen, log)
		}
		time.Sleep(20 * time.Millisecond)
	}
	return &httpServer{t: t, listen: listen, client: &http.Client{
		Timeout:       10 * time.Second,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}}
}

type httpReply struct {
	Status int
	Header http.Header
	Body   string
}

func (r httpReply) json(t *testing.T) map[string]any {
	t.Helper()
	var value map[string]any
	if err := json.Unmarshal([]byte(r.Body), &value); err != nil {
		t.Fatalf("body is not JSON (status %d): %q", r.Status, r.Body)
	}
	return value
}

// do sends a request through the public host name unless headers set Host.
func (s *httpServer) do(method, path, contentType, body string, headers map[string]string) httpReply {
	s.t.Helper()
	req, err := http.NewRequest(method, "http://"+s.listen+path, strings.NewReader(body))
	if err != nil {
		s.t.Fatal(err)
	}
	req.Host = publicHost
	if contentType != "" {
		req.Header.Set("Content-Type", contentType)
	}
	for k, v := range headers {
		if k == "Host" {
			req.Host = v
			continue
		}
		req.Header.Set(k, v)
	}
	resp, err := s.client.Do(req)
	if err != nil {
		s.t.Fatal(err)
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(resp.Body)
	if err != nil {
		s.t.Fatal(err)
	}
	return httpReply{Status: resp.StatusCode, Header: resp.Header, Body: string(data)}
}

func (s *httpServer) form(path string, values url.Values) httpReply {
	s.t.Helper()
	return s.do("POST", path, "application/x-www-form-urlencoded", values.Encode(), nil)
}

func (s *httpServer) rpc(token string, body any, headers map[string]string) httpReply {
	s.t.Helper()
	h := map[string]string{"Accept": "application/json, text/event-stream"}
	if token != "" {
		h["Authorization"] = "Bearer " + token
	}
	for k, v := range headers {
		h[k] = v
	}
	return s.do("POST", "/mcp", "application/json", mustJSON(s.t, body), h)
}

func wantStatus(t *testing.T, what string, r httpReply, status int) {
	t.Helper()
	if r.Status != status {
		t.Fatalf("%s: status %d, want %d\n%s", what, r.Status, status, r.Body)
	}
}

var requestField = regexp.MustCompile(`<input[^>]*name=["']?request["']?[^>]*>`)
var valueAttr = regexp.MustCompile(`value=["']?([^"' >]+)`)

// consentRequest extracts the hidden request token from the consent form.
func consentRequest(t *testing.T, page string) string {
	t.Helper()
	field := requestField.FindString(page)
	match := valueAttr.FindStringSubmatch(field)
	if match == nil {
		t.Fatalf("consent page has no request field:\n%s", page)
	}
	return match[1]
}

type pkce struct{ verifier, challenge string }

func newPKCE(seed string) pkce {
	verifier := strings.Repeat(seed, 50)[:64]
	sum := sha256.Sum256([]byte(verifier))
	return pkce{verifier, base64.RawURLEncoding.EncodeToString(sum[:])}
}

func (s *httpServer) register(t *testing.T) string {
	t.Helper()
	r := s.do("POST", "/oauth/register", "application/json", mustJSON(t, map[string]any{
		"redirect_uris": []string{redirectURI}, "client_name": "e2e client", "token_endpoint_auth_method": "none",
	}), nil)
	wantStatus(t, "register", r, 201)
	reg := r.json(t)
	clientID, _ := reg["client_id"].(string)
	if clientID == "" || reg["token_endpoint_auth_method"] != "none" || reg["client_secret"] != nil {
		t.Fatalf("registration: %v", reg)
	}
	return clientID
}

func authorizeQuery(clientID string, p pkce, state string) string {
	return "/oauth/authorize?" + url.Values{
		"response_type": {"code"}, "client_id": {clientID}, "redirect_uri": {redirectURI},
		"code_challenge": {p.challenge}, "code_challenge_method": {"S256"},
		"state": {state}, "scope": {"tg.read"}, "resource": {publicURL + "/mcp"},
	}.Encode()
}

// authorize runs the consent step and returns the authorization code.
func (s *httpServer) authorize(t *testing.T, clientID string, p pkce, state string) string {
	t.Helper()
	page := s.do("GET", authorizeQuery(clientID, p, state), "", "", nil)
	wantStatus(t, "consent page", page, 200)
	r := s.form("/oauth/authorize", url.Values{"request": {consentRequest(t, page.Body)}, "passphrase": {passphrase}, "decision": {"allow"}})
	wantStatus(t, "consent", r, 302)
	location, err := url.Parse(r.Header.Get("Location"))
	if err != nil {
		t.Fatal(err)
	}
	q := location.Query()
	if got := location.Scheme + "://" + location.Host + location.Path; got != redirectURI ||
		q.Get("state") != state || q.Get("iss") != publicURL || q.Get("code") == "" {
		t.Fatalf("redirect %s", location)
	}
	return q.Get("code")
}

func (s *httpServer) exchange(t *testing.T, clientID, code string, p pkce) map[string]any {
	t.Helper()
	r := s.form("/oauth/token", url.Values{
		"grant_type": {"authorization_code"}, "code": {code}, "code_verifier": {p.verifier},
		"redirect_uri": {redirectURI}, "client_id": {clientID}, "resource": {publicURL + "/mcp"},
	})
	wantStatus(t, "token", r, 200)
	tokens := r.json(t)
	if tokens["token_type"] != "Bearer" || tokens["access_token"] == "" || tokens["refresh_token"] == "" || tokens["scope"] != "tg.read" {
		t.Fatalf("token response: %v", tokens)
	}
	return tokens
}

func TestMCPHTTPMetadataAndGuards(t *testing.T) {
	t.Parallel()
	s := startHTTPServer(t, newEnv(t))

	for _, path := range []string{"/.well-known/oauth-protected-resource", "/.well-known/oauth-protected-resource/mcp"} {
		r := s.do("GET", path, "", "", nil)
		wantStatus(t, path, r, 200)
		assertJSONEqual(t, r.Body, map[string]any{
			"resource": publicURL + "/mcp", "authorization_servers": []string{publicURL},
			"scopes_supported": []string{"tg.read"}, "bearer_methods_supported": []string{"header"},
			"resource_name": "tg",
		})
	}
	for _, path := range []string{"/.well-known/oauth-authorization-server", "/.well-known/openid-configuration"} {
		r := s.do("GET", path, "", "", nil)
		wantStatus(t, path, r, 200)
		assertJSONEqual(t, r.Body, map[string]any{
			"issuer":                                         publicURL,
			"authorization_endpoint":                         publicURL + "/oauth/authorize",
			"token_endpoint":                                 publicURL + "/oauth/token",
			"registration_endpoint":                          publicURL + "/oauth/register",
			"response_types_supported":                       []string{"code"},
			"response_modes_supported":                       []string{"query"},
			"grant_types_supported":                          []string{"authorization_code", "refresh_token"},
			"code_challenge_methods_supported":               []string{"S256"},
			"token_endpoint_auth_methods_supported":          []string{"none", "client_secret_post", "client_secret_basic"},
			"scopes_supported":                               []string{"tg.read"},
			"authorization_response_iss_parameter_supported": true,
		})
	}

	// Unauthenticated MCP requests get the discovery challenge.
	challenge := `Bearer resource_metadata="` + publicURL + `/.well-known/oauth-protected-resource", scope="tg.read"`
	r := s.rpc("", rpcRequest(1, "ping", nil), nil)
	wantStatus(t, "no token", r, 401)
	if got := r.Header.Get("WWW-Authenticate"); got != challenge {
		t.Errorf("WWW-Authenticate %q, want %q", got, challenge)
	}
	r = s.rpc("tgat_not-a-real-token", rpcRequest(1, "ping", nil), nil)
	wantStatus(t, "bad token", r, 401)
	if got := r.Header.Get("WWW-Authenticate"); got != challenge+`, error="invalid_token"` {
		t.Errorf("WWW-Authenticate %q", got)
	}

	// Only the public host name and the listen address are served.
	wantStatus(t, "foreign host", s.do("GET", "/.well-known/oauth-protected-resource", "", "", map[string]string{"Host": "evil.example"}), 421)
	wantStatus(t, "listen address host", s.do("GET", "/.well-known/oauth-protected-resource", "", "", map[string]string{"Host": s.listen}), 200)
	wantStatus(t, "case-insensitive host", s.do("GET", "/.well-known/oauth-protected-resource", "", "", map[string]string{"Host": strings.ToUpper(publicHost)}), 200)

	r = s.do("GET", "/mcp", "", "", nil)
	wantStatus(t, "GET /mcp", r, 405)
	if r.Header.Get("Allow") != "POST" {
		t.Errorf("Allow: %q", r.Header.Get("Allow"))
	}
	wantStatus(t, "DELETE /mcp", s.do("DELETE", "/mcp", "", "", nil), 405)
	wantStatus(t, "unknown path", s.do("GET", "/admin", "", "", nil), 404)
	wantStatus(t, "oversized body", s.do("POST", "/oauth/register", "application/json", strings.Repeat("x", 1024*1024+10), nil), 413)

	// Registration only accepts allowlisted redirects.
	for _, body := range []string{
		`{"redirect_uris":["https://evil.example/callback"]}`,
		`{"redirect_uris":[]}`,
	} {
		r := s.do("POST", "/oauth/register", "application/json", body, nil)
		wantStatus(t, "register "+body, r, 400)
		if r.json(t)["error"] != "invalid_redirect_uri" {
			t.Errorf("register %s: %s", body, r.Body)
		}
	}
	r = s.do("POST", "/oauth/register", "application/json", "{bad", nil)
	wantStatus(t, "register bad JSON", r, 400)
	if r.json(t)["error"] != "invalid_client_metadata" {
		t.Errorf("register bad JSON: %s", r.Body)
	}
}

func TestMCPHTTPOriginCheck(t *testing.T) {
	t.Parallel()
	s := startHTTPServer(t, newEnv(t))
	r := s.do("POST", "/mcp", "application/json", mustJSON(t, rpcRequest(1, "ping", nil)), map[string]string{"Origin": "https://evil.example"})
	wantStatus(t, "foreign origin", r, 403)
	r = s.do("GET", "/.well-known/oauth-protected-resource", "", "", map[string]string{"Origin": publicURL})
	wantStatus(t, "own origin", r, 200)
}

func TestMCPHTTPOAuthFlow(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	s := startHTTPServer(t, e)
	clientID := s.register(t)
	p := newPKCE("verifier-")

	// Consent page errors before a redirect target is trusted stay on the page.
	wantStatus(t, "unknown client", s.do("GET", authorizeQuery("tgc_unknown", p, "s"), "", "", nil), 400)
	bad := strings.Replace(authorizeQuery(clientID, p, "s"), url.QueryEscape(redirectURI), url.QueryEscape("http://127.0.0.1:6274/other"), 1)
	wantStatus(t, "unregistered redirect", s.do("GET", bad, "", "", nil), 400)
	// Later errors go back to the client.
	noPKCE := strings.Replace(authorizeQuery(clientID, p, "s1"), "code_challenge_method=S256", "code_challenge_method=plain", 1)
	r := s.do("GET", noPKCE, "", "", nil)
	wantStatus(t, "plain PKCE", r, 302)
	if loc := r.Header.Get("Location"); !strings.HasPrefix(loc, redirectURI+"?error=invalid_request") || !strings.Contains(loc, "state=s1") {
		t.Errorf("plain PKCE redirect: %s", loc)
	}

	// Wrong passphrase: the form is shown again; deny returns access_denied.
	page := s.do("GET", authorizeQuery(clientID, p, "s2"), "", "", nil)
	wantStatus(t, "consent page", page, 200)
	if page.Header.Get("X-Frame-Options") != "DENY" || !strings.Contains(page.Header.Get("Content-Security-Policy"), "frame-ancestors 'none'") {
		t.Errorf("consent page must not be frameable: %v", page.Header)
	}
	if !strings.Contains(page.Body, "e2e client") {
		t.Error("consent page does not name the client")
	}
	request := consentRequest(t, page.Body)
	r = s.form("/oauth/authorize", url.Values{"request": {request}, "passphrase": {"wrong passphrase!!"}, "decision": {"allow"}})
	wantStatus(t, "wrong passphrase", r, 401)
	r = s.form("/oauth/authorize", url.Values{"request": {consentRequest(t, r.Body)}, "decision": {"deny"}})
	wantStatus(t, "deny", r, 302)
	if loc := r.Header.Get("Location"); !strings.HasPrefix(loc, redirectURI+"?error=access_denied") {
		t.Errorf("deny redirect: %s", loc)
	}
	// A consent request is single use.
	wantStatus(t, "reused consent", s.form("/oauth/authorize", url.Values{"request": {request}, "passphrase": {passphrase}, "decision": {"allow"}}), 400)

	code := s.authorize(t, clientID, p, "s3")
	// The code is bound to its PKCE verifier.
	r = s.form("/oauth/token", url.Values{
		"grant_type": {"authorization_code"}, "code": {code}, "code_verifier": {newPKCE("other-").verifier},
		"redirect_uri": {redirectURI}, "client_id": {clientID},
	})
	wantStatus(t, "wrong verifier", r, 400)
	if r.json(t)["error"] != "invalid_grant" {
		t.Errorf("wrong verifier: %s", r.Body)
	}
	code = s.authorize(t, clientID, p, "s4")
	tokens := s.exchange(t, clientID, code, p)
	access := tokens["access_token"].(string)
	// Codes are single use.
	r = s.form("/oauth/token", url.Values{
		"grant_type": {"authorization_code"}, "code": {code}, "code_verifier": {p.verifier},
		"redirect_uri": {redirectURI}, "client_id": {clientID},
	})
	wantStatus(t, "reused code", r, 400)
	wantStatus(t, "unknown client token", s.form("/oauth/token", url.Values{"grant_type": {"authorization_code"}, "client_id": {"tgc_nope"}}), 401)
	r = s.form("/oauth/token", url.Values{"grant_type": {"password"}, "client_id": {clientID}})
	wantStatus(t, "unsupported grant", r, 400)
	if r.json(t)["error"] != "unsupported_grant_type" {
		t.Errorf("unsupported grant: %s", r.Body)
	}

	// The token works on /mcp.
	r = s.rpc(access, rpcRequest(1, "initialize", map[string]any{"protocolVersion": "2025-06-18"}), nil)
	wantStatus(t, "initialize", r, 200)
	if v := r.json(t)["result"].(map[string]any)["protocolVersion"]; v != "2025-06-18" {
		t.Errorf("initialize negotiated %v", v)
	}
	r = s.rpc(access, toolCall(2, "read_messages", map[string]any{"session": "team@chatroom", "since": "2024-06-04"}), map[string]string{"MCP-Protocol-Version": "2025-06-18"})
	wantStatus(t, "tools/call", r, 200)
	text := r.json(t)["result"].(map[string]any)["content"].([]any)[0].(map[string]any)["text"].(string)
	if !strings.Contains(text, "12:05 Bob: 明天开会 meeting tomorrow") {
		t.Errorf("read_messages over HTTP:\n%s", text)
	}
	wantStatus(t, "notification", s.rpc(access, map[string]any{"jsonrpc": "2.0", "method": "notifications/initialized"}, nil), 202)
	wantStatus(t, "bad protocol version", s.rpc(access, rpcRequest(3, "ping", nil), map[string]string{"MCP-Protocol-Version": "2000-01-01"}), 400)

	// Refresh rotates; reusing a rotated refresh token ends the whole grant.
	refresh := tokens["refresh_token"].(string)
	r = s.form("/oauth/token", url.Values{"grant_type": {"refresh_token"}, "refresh_token": {refresh}, "client_id": {clientID}})
	wantStatus(t, "refresh", r, 200)
	rotated := r.json(t)
	if rotated["refresh_token"] == refresh || rotated["access_token"] == access {
		t.Error("refresh did not rotate tokens")
	}
	wantStatus(t, "rotated access token", s.rpc(rotated["access_token"].(string), rpcRequest(4, "ping", nil), nil), 200)
	r = s.form("/oauth/token", url.Values{"grant_type": {"refresh_token"}, "refresh_token": {refresh}, "client_id": {clientID}})
	wantStatus(t, "refresh reuse", r, 400)
	if r.json(t)["error"] != "invalid_grant" {
		t.Errorf("refresh reuse: %s", r.Body)
	}
	wantStatus(t, "token after reuse", s.rpc(rotated["access_token"].(string), rpcRequest(5, "ping", nil), nil), 401)

	// Owner commands: status, audit, revoke.
	st := e.run(t, "mcp", "status").mustSucceed(t)
	st.stdoutHas(t, "Connector URL: "+publicURL+"/mcp\n", "Listening on: "+s.listen+"\n",
		"registered clients: 1\n", "  "+clientID+"  e2e client  registered ")
	audit := e.run(t, "mcp", "audit").mustSucceed(t)
	audit.stdoutHas(t, " read_messages ", "ok=true")
	if strings.Contains(audit.Stdout, "明天开会") || strings.Contains(audit.Stdout, "team@chatroom") {
		t.Error("the audit log must not contain message content or argument values")
	}

	code = s.authorize(t, clientID, p, "s5")
	fresh := s.exchange(t, clientID, code, p)["access_token"].(string)
	wantStatus(t, "fresh token", s.rpc(fresh, rpcRequest(6, "ping", nil), nil), 200)
	e.run(t, "mcp", "revoke", "--client", clientID).mustSucceed(t).stdoutHas(t, "Revoked 1 client(s) and their tokens.")
	wantStatus(t, "token after revoke", s.rpc(fresh, rpcRequest(7, "ping", nil), nil), 401)
	e.run(t, "mcp", "revoke", "--all").mustSucceed(t).stdoutHas(t, "Revoked 0 client(s) and their tokens.")
}

func TestMCPInit(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	dir := filepath.Join(e.Home, ".tg", "mcp")

	e.run(t, "mcp", "status").mustFail(t, 1, "config.json not found", "tg mcp init")
	e.run(t, "mcp", "serve", "--db-dir", fixture().DBDir, "--keys", fixture().Keys).mustFail(t, 1, "config.json not found")
	e.run(t, "mcp", "init", "--listen", "127.0.0.1:18080").mustFail(t, 1, "--public-url is required")
	e.run(t, "mcp", "init", "--public-url", publicURL).mustFail(t, 1, "--listen is required")
	e.run(t, "mcp", "init", "--public-url", "http://plain.example", "--listen", "127.0.0.1:18080").mustFail(t, 1, "https://")
	e.run(t, "mcp", "init", "--public-url", publicURL, "--listen", "0.0.0.0:18080").mustFail(t, 1, "loopback")
	e.run(t, "mcp", "init", "--public-url", publicURL, "--listen", "127.0.0.1:18080").mustFail(t, 1, "--passphrase-stdin")
	e.Stdin = "short\n"
	e.run(t, "mcp", "init", "--public-url", publicURL, "--listen", "127.0.0.1:18080", "--passphrase-stdin").mustFail(t, 1)
	if _, err := os.Stat(filepath.Join(dir, "config.json")); err == nil {
		t.Error("a failed init wrote config.json")
	}

	e.Stdin = passphrase + "\n"
	e.run(t, "mcp", "init", "--public-url", publicURL, "--listen", "100.64.0.5:18080", "--passphrase-stdin").mustSucceed(t)
	assertMode(t, filepath.Join(dir, "config.json"), 0o600)
	assertMode(t, filepath.Join(dir, "owner.json"), 0o600)
	owner, err := os.ReadFile(filepath.Join(dir, "owner.json"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(owner), passphrase) {
		t.Error("owner.json stores the passphrase in clear")
	}
	// A later init keeps the passphrase and edits the config in place.
	e.Stdin = ""
	e.run(t, "mcp", "init", "--listen", "127.0.0.1:18081").mustSucceed(t)
	st := e.run(t, "mcp", "status").mustSucceed(t)
	if st.Stdout != "Connector URL: "+publicURL+"/mcp\nListening on: 127.0.0.1:18081\nActive grants: 0; registered clients: 0\n" {
		t.Errorf("status:\n%s", st.Stdout)
	}
	e.run(t, "mcp", "audit").mustSucceed(t)

	// State files other users can read are refused.
	if err := os.Chmod(filepath.Join(dir, "config.json"), 0o644); err != nil {
		t.Fatal(err)
	}
	e.run(t, "mcp", "status").mustFail(t, 1)
}
