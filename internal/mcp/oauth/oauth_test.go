package oauth

import (
	"encoding/hex"
	"encoding/json"
	"errors"
	"net/netip"
	"net/url"
	"path/filepath"
	"strings"
	"testing"

	"github.com/xiaotianxt/tg/internal/mcp/config"
)

const (
	redirect   = "https://chatgpt.com/connector/oauth/abc_123"
	passphrase = "correct horse battery staple"
	t0         = int64(1_800_000_000)
)

func TestRedirectAllowed(t *testing.T) {
	extra := []string{"http://localhost:6274/oauth/callback"}
	for _, ok := range []string{
		"https://chatgpt.com/connector_platform_oauth_redirect",
		"https://chatgpt.com/connector/oauth/abc_123",
		"https://chatgpt.com/connector/oauth/A-b_9",
		"https://oauth-redirect.googleusercontent.com/r/user_bound_custom-mcp-6a7ca2",
		"https://oauth-redirect-sandbox.googleusercontent.com/a/x.y",
		"https://oauth-redirect-test.googleusercontent.com/r/x",
		"http://localhost:6274/oauth/callback",
	} {
		if !RedirectAllowed(ok, extra) {
			t.Errorf("%s should be allowed", ok)
		}
	}
	for _, bad := range []string{
		"https://evil.example/cb",
		"https://chatgpt.com/connector/oauth/a/b",
		"https://chatgpt.com/connector/oauth/",
		"https://chatgpt.com/connector/oauth/a%2Fb",
		"http://chatgpt.com/connector_platform_oauth_redirect",
		"https://chatgpt.com/connector_platform_oauth_redirect?x=1",
		"https://oauth-redirect.googleusercontent.com/",
		"https://oauth-redirect.googleusercontent.com.evil.example/cb",
		"https://oauth-redirect-evil.googleusercontent.com/cb",
		"https://oauth-redirect.googleusercontent.com/a/../cb",
		"https://oauth-redirect.googleusercontent.com/./cb",
		"https://oauth-redirect.googleusercontent.com/cb?x=1",
		"https://oauth-redirect.googleusercontent.com/cb#f",
		"https://oauth-redirect.googleusercontent.com/c%62",
		"http://oauth-redirect.googleusercontent.com/cb",
		"http://localhost:6274/oauth/callback/",
	} {
		if RedirectAllowed(bad, extra) {
			t.Errorf("%s should be rejected", bad)
		}
	}
}

func TestPKCE(t *testing.T) {
	// RFC 7636 appendix B.
	verifier := "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
	challenge := "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
	if got := PKCEChallenge(verifier); got != challenge {
		t.Fatalf("challenge = %s", got)
	}
	if !VerifyPKCE(verifier, challenge) || VerifyPKCE(verifier+"x", challenge) || VerifyPKCE(verifier, "") {
		t.Error("VerifyPKCE accepted a mismatch or rejected the vector")
	}
}

func TestTokenStateAndRefreshRotation(t *testing.T) {
	g := Grants{}.Issue("c1", "g1", "a1", "r1", t0)
	if id, err := g.Authenticate("a1", t0+AccessTTL-1); err != nil || id != "c1" {
		t.Fatalf("authenticate = %q, %v", id, err)
	}
	if _, err := g.Authenticate("a1", t0+AccessTTL); !errors.Is(err, ErrInvalidToken) {
		t.Errorf("expired access token: %v", err)
	}
	if _, err := g.Authenticate("r1", t0); !errors.Is(err, ErrInvalidToken) {
		t.Errorf("a refresh token must not authenticate: %v", err)
	}

	// A wrong client cannot use another client's refresh token.
	if _, _, err := g.Rotate("c2", "r1", t0); !errors.Is(err, ErrRefreshInvalid) {
		t.Errorf("wrong client: %v", err)
	}
	rotated, grant, err := g.Rotate("c1", "r1", t0+10)
	if err != nil || grant != "g1" {
		t.Fatalf("rotate = %q, %v", grant, err)
	}
	if g.Tokens[1].StateAt(t0) != TokenActive {
		t.Error("Rotate modified its receiver")
	}
	if rotated.Tokens[1].StateAt(t0+10) != TokenRotated {
		t.Error("refresh token not marked rotated")
	}
	reissued := rotated.Issue("c1", grant, "a2", "r2", t0+10)
	other := reissued.Issue("c1", "g2", "a3", "r3", t0+10)

	// Reuse of the rotated token ends grant g1 and nothing else.
	after, _, err := other.Rotate("c1", "r1", t0+20)
	if !errors.Is(err, ErrRefreshReused) {
		t.Fatalf("reuse: %v", err)
	}
	for _, tok := range []string{"a1", "a2"} {
		if _, err := after.Authenticate(tok, t0+20); err == nil {
			t.Errorf("%s survived reuse of its grant's refresh token", tok)
		}
	}
	if _, err := after.Authenticate("a3", t0+20); err != nil {
		t.Errorf("other grant was revoked: %v", err)
	}

	if _, _, err := reissued.Rotate("c1", "r2", t0+10+RefreshTTL); !errors.Is(err, ErrRefreshExpired) {
		t.Errorf("expired refresh: %v", err)
	}
	if n := len(reissued.Prune(t0 + 10 + AccessTTL).Tokens); n != 2 {
		t.Errorf("prune kept %d tokens, want the two refresh tokens", n)
	}
	if s := reissued.Summarize(t0 + 10); s.ActiveGrants != 1 {
		t.Errorf("active grants = %d", s.ActiveGrants)
	}
}

func TestClientLimitAndRevoke(t *testing.T) {
	g := Grants{}
	var err error
	for i := range MaxClients {
		if g, err = g.AddClient(Client{ID: string(rune('a' + i))}); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := g.AddClient(Client{ID: "z"}); !errors.Is(err, ErrClientLimit) {
		t.Errorf("err = %v", err)
	}
	g = g.Issue("a", "g", "ha", "hr", t0)
	g, n := g.RevokeClient("a")
	if n != 1 || len(g.Clients) != MaxClients-1 || len(g.Tokens) != 0 {
		t.Errorf("revoke a: n=%d clients=%d tokens=%d", n, len(g.Clients), len(g.Tokens))
	}
	if _, n := g.RevokeClient("missing"); n != 0 {
		t.Errorf("revoke missing removed %d", n)
	}
	if _, n := g.RevokeAll(); n != MaxClients-1 {
		t.Errorf("revoke all removed %d", n)
	}
}

func TestClientFileKeepsTheTwoXShape(t *testing.T) {
	in := `{"client_id":"tgc_1","secret_hash":null,"redirect_uris":["x"],"client_name":null,"created":5}`
	var c Client
	if err := json.Unmarshal([]byte(in), &c); err != nil {
		t.Fatal(err)
	}
	if c.Kind != PublicClient || c.Name != "" {
		t.Errorf("client = %+v", c)
	}
	out, _ := json.Marshal(c)
	if string(out) != in {
		t.Errorf("round trip = %s", out)
	}
	if err := json.Unmarshal([]byte(`{"client_id":"tgc_2","secret_hash":"ab","redirect_uris":[],"client_name":"n","created":1}`), &c); err != nil {
		t.Fatal(err)
	}
	if c.Kind != ConfidentialClient || c.SecretHash != "ab" || c.Name != "n" {
		t.Errorf("client = %+v", c)
	}
}

func TestLimiter(t *testing.T) {
	var l Limiter
	for i := range MaxFailures {
		if l.Blocked() {
			t.Fatalf("blocked after %d failures", i)
		}
		l = l.Record(t0 + int64(i))
	}
	if !l.Prune(t0 + FailureWindow - 1).Blocked() {
		t.Error("not blocked within the window")
	}
	if l.Prune(t0 + FailureWindow).Blocked() {
		t.Error("still blocked after the oldest failure left the window")
	}
}

func TestOwnerPassphrase(t *testing.T) {
	if _, err := NewOwner("fifteen chars!!"); err == nil {
		t.Error("15-character passphrase accepted")
	}
	if _, err := NewOwner("十六个字符十六个字符十六个字符十六"); err != nil {
		t.Errorf("16 multibyte characters rejected: %v", err)
	}
	o := testOwner()
	if !o.Verify(passphrase) || o.Verify(passphrase[:len(passphrase)-1]) || o.Verify("") {
		t.Error("Verify mismatch")
	}
	if (Owner{Salt: "zz", Rounds: 1, Hash: "00"}).Verify("") {
		t.Error("corrupt owner verified")
	}
}

func testOwner() Owner {
	// Few rounds keep tests fast; production uses PBKDF2Rounds.
	salt := make([]byte, 16)
	return Owner{Salt: hex.EncodeToString(salt), Rounds: 1000, Hash: hex.EncodeToString(derive(passphrase, salt, 1000))}
}

func testServer(t *testing.T) *Server {
	cfg := config.Config{PublicURL: "https://mcp.example.com", Listen: netip.MustParseAddrPort("127.0.0.1:8765"), Remote: config.DefaultRemotePolicy()}
	return NewServer(cfg, filepath.Join(t.TempDir(), "mcp"), testOwner())
}

func register(t *testing.T, s *Server) string {
	t.Helper()
	r := s.Register(t0, []byte(`{"redirect_uris":["`+redirect+`"],"client_name":"Chat"}`))
	if r.Status != 201 {
		t.Fatalf("register: %d %s", r.Status, r.Body)
	}
	var body struct {
		ClientID string `json:"client_id"`
	}
	_ = json.Unmarshal(r.Body, &body)
	return body.ClientID
}

func consent(t *testing.T, s *Server, now int64, clientID, verifier string) string {
	t.Helper()
	r := s.AuthorizePage(now, Params{
		"response_type": "code", "client_id": clientID, "redirect_uri": redirect,
		"code_challenge": PKCEChallenge(verifier), "code_challenge_method": "S256",
		"state": "xyz", "resource": "https://mcp.example.com/mcp",
	})
	if r.Kind != ReplyHTML || r.Status != 200 {
		t.Fatalf("authorize page: %d %s", r.Status, r.Body)
	}
	_, rest, _ := strings.Cut(string(r.Body), `name=request value="`)
	csrf, _, _ := strings.Cut(rest, `"`)
	return csrf
}

func TestConsentFailureLimiting(t *testing.T) {
	s := testServer(t)
	id := register(t, s)
	csrf := consent(t, s, t0, id, strings.Repeat("v", 50))
	submit := func(now int64, pass string) Reply {
		return s.AuthorizeSubmit(now, Params{"request": csrf, "passphrase": pass, "decision": "allow"})
	}
	for i := range MaxFailures {
		if r := submit(t0+int64(i), "wrong"); r.Status != 401 || !strings.Contains(string(r.Body), "Wrong passphrase.") {
			t.Fatalf("attempt %d: %d", i, r.Status)
		}
	}
	// Blocked even with the right passphrase; the consent survives.
	if r := submit(t0+10, passphrase); r.Status != 429 {
		t.Fatalf("expected 429, got %d", r.Status)
	}
	// The consent itself expires after ConsentTTL, so restart it once the
	// failure window has passed.
	later := t0 + FailureWindow + 5
	if r := submit(later, passphrase); r.Status != 400 {
		t.Fatalf("expired consent: %d", r.Status)
	}
	csrf = consent(t, s, later, id, strings.Repeat("v", 50))
	if r := submit(later, passphrase); r.Kind != ReplyRedirect {
		t.Fatalf("after the window: %d %s", r.Status, r.Body)
	}
}

func TestDenyRedirectsWithError(t *testing.T) {
	s := testServer(t)
	csrf := consent(t, s, t0, register(t, s), strings.Repeat("v", 50))
	r := s.AuthorizeSubmit(t0, Params{"request": csrf, "decision": "deny"})
	want := redirect + "?error=access_denied&state=xyz&iss=https%3A%2F%2Fmcp.example.com"
	if r.Kind != ReplyRedirect || r.Location != want {
		t.Fatalf("location = %q", r.Location)
	}
	if again := s.AuthorizeSubmit(t0, Params{"request": csrf, "decision": "deny"}); again.Status != 400 {
		t.Errorf("denied consent reused: %d", again.Status)
	}
}

func TestAuthorizePageErrorsGoBackToTheClient(t *testing.T) {
	s := testServer(t)
	id := register(t, s)
	base := Params{"response_type": "code", "client_id": id, "redirect_uri": redirect,
		"code_challenge": strings.Repeat("c", 43), "code_challenge_method": "S256"}
	with := func(k, v string) Params {
		p := Params{}
		for key, val := range base {
			p[key] = val
		}
		p[k] = v
		return p
	}
	for _, tc := range []struct {
		params Params
		error  string
	}{
		{with("response_type", "token"), "unsupported_response_type"},
		{with("code_challenge_method", "plain"), "invalid_request"},
		{with("code_challenge", strings.Repeat("c", 42)), "invalid_request"},
		{with("code_challenge", strings.Repeat("c", 129)), "invalid_request"},
		{with("resource", "https://other.example/mcp"), "invalid_target"},
	} {
		r := s.AuthorizePage(t0, tc.params)
		if r.Kind != ReplyRedirect || !strings.Contains(r.Location, "error="+tc.error) || strings.Contains(r.Location, "state=") {
			t.Errorf("%v: %d %q", tc.params, r.Status, r.Location)
		}
	}
	if r := s.AuthorizePage(t0, with("redirect_uri", redirect+"x")); r.Kind != ReplyHTML || r.Status != 400 {
		t.Errorf("unregistered redirect must not redirect: %d", r.Status)
	}
	if r := s.AuthorizePage(t0, with("client_id", "tgc_unknown")); r.Status != 400 {
		t.Errorf("unknown client: %d", r.Status)
	}
	// A trailing slash on the resource is accepted.
	if r := s.AuthorizePage(t0, with("resource", "https://mcp.example.com/")); r.Kind != ReplyHTML || r.Status != 200 {
		t.Errorf("issuer resource: %d", r.Status)
	}
}

func TestCodeIsSingleUseAndExpires(t *testing.T) {
	s := testServer(t)
	id := register(t, s)
	verifier := strings.Repeat("v", 50)
	issue := func(now int64) string {
		csrf := consent(t, s, now, id, verifier)
		r := s.AuthorizeSubmit(now, Params{"request": csrf, "passphrase": passphrase})
		u, err := url.Parse(r.Location)
		if err != nil || r.Kind != ReplyRedirect {
			t.Fatalf("submit: %d %v", r.Status, err)
		}
		return u.Query().Get("code")
	}
	exchange := func(now int64, code, verifier, redirectURI string) Reply {
		return s.Token(now, Params{"grant_type": "authorization_code", "code": code, "client_id": id,
			"redirect_uri": redirectURI, "code_verifier": verifier}, nil)
	}
	code := issue(t0)
	if !strings.HasPrefix(code, codePrefix) {
		t.Errorf("code prefix: %q", code)
	}
	// A wrong verifier consumes the code too.
	if r := exchange(t0, code, verifier+"x", redirect); r.Status != 400 {
		t.Fatalf("wrong verifier: %d", r.Status)
	}
	if r := exchange(t0, code, verifier, redirect); r.Status != 400 {
		t.Fatalf("consumed code accepted: %d", r.Status)
	}
	code = issue(t0)
	if r := exchange(t0+CodeTTL, code, verifier, redirect); r.Status != 400 {
		t.Fatalf("expired code accepted: %d", r.Status)
	}
	code = issue(t0)
	if r := exchange(t0, code, verifier, redirect+"x"); r.Status != 400 {
		t.Fatalf("redirect mismatch accepted: %d", r.Status)
	}
	code = issue(t0)
	r := exchange(t0+CodeTTL-1, code, verifier, redirect)
	var tokens map[string]any
	_ = json.Unmarshal(r.Body, &tokens)
	if r.Status != 200 || !strings.HasPrefix(tokens["access_token"].(string), accessPrefix) || !strings.HasPrefix(tokens["refresh_token"].(string), refreshPrefix) {
		t.Fatalf("exchange: %d %s", r.Status, r.Body)
	}
}

func TestConfidentialClientsMustAuthenticate(t *testing.T) {
	s := testServer(t)
	r := s.Register(t0, []byte(`{"redirect_uris":["`+redirect+`"],"token_endpoint_auth_method":"client_secret_basic"}`))
	var body map[string]any
	_ = json.Unmarshal(r.Body, &body)
	id, secret := body["client_id"].(string), body["client_secret"].(string)
	if r.Status != 201 || !strings.HasPrefix(id, clientIDPrefix) || !strings.HasPrefix(secret, secretPrefix) {
		t.Fatalf("register: %d %s", r.Status, r.Body)
	}
	form := Params{"grant_type": "refresh_token", "refresh_token": "tgrt_x"}
	for _, creds := range []*Credentials{nil, {ID: id, Secret: "wrong"}} {
		if r := s.Token(t0, Params{"client_id": id, "grant_type": "refresh_token"}, creds); r.Status != 401 {
			t.Errorf("creds %v: %d", creds, r.Status)
		}
	}
	// Authenticated, the request reaches grant validation.
	if r := s.Token(t0, form, &Credentials{ID: id, Secret: secret}); r.Status != 400 || !strings.Contains(string(r.Body), "invalid_grant") {
		t.Errorf("authenticated: %d %s", r.Status, r.Body)
	}
	if r := s.Register(t0, []byte(`{"redirect_uris":["`+redirect+`"],"token_endpoint_auth_method":"private_key_jwt"}`)); r.Status != 400 {
		t.Errorf("unsupported auth method: %d", r.Status)
	}
	if r := s.Register(t0, []byte(`[1]`)); r.Status != 400 || !strings.Contains(string(r.Body), "invalid_redirect_uri") {
		t.Errorf("non-object body: %d %s", r.Status, r.Body)
	}
}
