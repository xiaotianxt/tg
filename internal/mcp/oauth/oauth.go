// Package oauth is a single-user OAuth 2.1 authorization server for remote
// MCP clients.
//
// The owner proves presence with a passphrase on the consent page; clients
// never see it. Tokens are opaque random strings stored only as SHA-256
// hashes (Grants, in oauth.json). Pending consents, authorization codes and
// the failure limiter live in memory, so a restart only interrupts a sign-in
// in progress. Every operation takes `now` in unix seconds.
package oauth

import (
	"bytes"
	"crypto/pbkdf2"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strings"
	"sync"
	"unicode/utf8"

	"github.com/xiaotianxt/tg/internal/logx"
	"github.com/xiaotianxt/tg/internal/mcp/config"
)

const (
	Scope              = "tg.read"
	AccessTTL          = 3600
	RefreshTTL         = 30 * 86400
	CodeTTL            = 60
	ConsentTTL         = 600
	PassphraseMinChars = 16
	PBKDF2Rounds       = 600_000
	MaxClients         = 10
	FailureWindow      = 15 * 60
	MaxFailures        = 5
)

// Token prefixes make leaked values recognizable.
const (
	clientIDPrefix = "tgc_"
	secretPrefix   = "tgs_"
	codePrefix     = "tgcode_"
	accessPrefix   = "tgat_"
	refreshPrefix  = "tgrt_"
)

// Owner is owner.json: the PBKDF2-HMAC-SHA256 hash of the owner passphrase.
type Owner struct {
	Salt   string `json:"salt"`
	Rounds int    `json:"rounds"`
	Hash   string `json:"hash"`
}

// NewOwner hashes a passphrase of at least PassphraseMinChars characters
// with a random salt.
func NewOwner(passphrase string) (Owner, error) {
	if utf8.RuneCountInString(passphrase) < PassphraseMinChars {
		return Owner{}, fmt.Errorf("passphrase must have at least %d characters", PassphraseMinChars)
	}
	salt := make([]byte, 16)
	_, _ = rand.Read(salt) // crypto/rand.Read never fails
	return Owner{
		Salt:   hex.EncodeToString(salt),
		Rounds: PBKDF2Rounds,
		Hash:   hex.EncodeToString(derive(passphrase, salt, PBKDF2Rounds)),
	}, nil
}

// Verify reports whether passphrase matches, in constant time.
func (o Owner) Verify(passphrase string) bool {
	salt, err1 := hex.DecodeString(o.Salt)
	want, err2 := hex.DecodeString(o.Hash)
	if err1 != nil || err2 != nil || o.Rounds <= 0 {
		return false
	}
	return subtle.ConstantTimeCompare(derive(passphrase, salt, o.Rounds), want) == 1
}

func derive(passphrase string, salt []byte, rounds int) []byte {
	key, err := pbkdf2.Key(sha256.New, passphrase, salt, rounds, 32)
	if err != nil {
		// Only FIPS-mode parameter checks fail, and these parameters pass.
		panic(fmt.Sprintf("pbkdf2: %v", err))
	}
	return key
}

// ReplyKind is the shape of an OAuth response.
type ReplyKind uint8

const (
	ReplyJSON ReplyKind = iota
	ReplyHTML
	ReplyRedirect
)

// Reply is an HTTP-independent OAuth response; the transport renders it.
type Reply struct {
	Kind   ReplyKind
	Status int
	// Body is JSON for ReplyJSON and HTML for ReplyHTML.
	Body []byte
	// Location is set for ReplyRedirect (status 302).
	Location string
}

func jsonReply(status int, v any) Reply {
	return Reply{Kind: ReplyJSON, Status: status, Body: marshal(v)}
}

func htmlReply(status int, html string) Reply {
	return Reply{Kind: ReplyHTML, Status: status, Body: []byte(html)}
}

func redirectReply(location string) Reply {
	return Reply{Kind: ReplyRedirect, Status: 302, Location: location}
}

func oauthError(status int, code, description string) Reply {
	return jsonReply(status, map[string]string{"error": code, "error_description": description})
}

func serverError(err error) Reply {
	logx.Errorf("oauth: %v", err)
	return oauthError(500, "server_error", "internal error")
}

// Params is a decoded query string or form body; a repeated key keeps the
// last value.
type Params map[string]string

func (p Params) get(key string) (string, bool) { v, ok := p[key]; return v, ok }

// Credentials are decoded HTTP Basic client credentials.
type Credentials struct{ ID, Secret string }

// PendingConsent is an authorization request waiting for the owner's
// decision on the consent page, keyed by its CSRF token.
type PendingConsent struct {
	ClientID   string
	ClientName string
	// RedirectURI is one of the client's registered URIs, matched exactly.
	RedirectURI string
	// State is echoed back to the client; nil when the request had none.
	State         *string
	CodeChallenge string
	Expires       int64
}

// AuthCode is an issued authorization code, keyed by its hash. It is
// removed on first presentation, whether or not the exchange succeeds.
type AuthCode struct {
	ClientID      string
	RedirectURI   string
	CodeChallenge string
	Expires       int64
}

// Limiter counts wrong passphrases within FailureWindow. It is a value the
// server owns, not process state.
type Limiter struct{ failures []int64 }

// Prune forgets failures older than FailureWindow.
func (l Limiter) Prune(now int64) Limiter {
	return Limiter{failures: slices.DeleteFunc(slices.Clone(l.failures), func(at int64) bool { return now-at >= FailureWindow })}
}

// Record adds a failure at now.
func (l Limiter) Record(now int64) Limiter {
	return Limiter{failures: append(slices.Clip(l.failures), now)}
}

// Blocked reports whether MaxFailures were reached; call Prune first.
func (l Limiter) Blocked() bool { return len(l.failures) >= MaxFailures }

// Server is the authorization server. Its methods are safe for concurrent
// use; each runs under one lock, including the file read-modify-write.
type Server struct {
	publicURL   string
	resourceURL string
	extraURIs   []string
	store       Store
	owner       Owner

	mu       sync.Mutex
	consents map[string]PendingConsent
	codes    map[string]AuthCode
	limiter  Limiter
}

// NewServer serves cfg's issuer with grants stored under dir.
func NewServer(cfg config.Config, dir string, owner Owner) *Server {
	return &Server{
		publicURL:   cfg.PublicURL,
		resourceURL: cfg.ResourceURL(),
		extraURIs:   slices.Clone(cfg.ExtraRedirectURIs),
		store:       NewStore(dir),
		owner:       owner,
		consents:    map[string]PendingConsent{},
		codes:       map[string]AuthCode{},
	}
}

// Store is the server's grant file.
func (s *Server) Store() Store { return s.store }

// Authenticate validates a bearer access token and returns its client id.
// It returns ErrInvalidToken for an unknown or expired token and other
// errors for an unreadable store.
func (s *Server) Authenticate(now int64, bearer string) (string, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	g, err := s.store.Load()
	if err != nil {
		return "", err
	}
	return g.Authenticate(hashToken(bearer), now)
}

// ProtectedResourceMetadata is RFC 9728 metadata for /mcp.
func (s *Server) ProtectedResourceMetadata() Reply {
	return jsonReply(200, map[string]any{
		"resource":                 s.resourceURL,
		"authorization_servers":    []string{s.publicURL},
		"scopes_supported":         []string{Scope},
		"bearer_methods_supported": []string{"header"},
		"resource_name":            "tg",
	})
}

// AuthorizationServerMetadata is RFC 8414 metadata.
func (s *Server) AuthorizationServerMetadata() Reply {
	base := s.publicURL
	return jsonReply(200, map[string]any{
		"issuer":                                         base,
		"authorization_endpoint":                         base + "/oauth/authorize",
		"token_endpoint":                                 base + "/oauth/token",
		"registration_endpoint":                          base + "/oauth/register",
		"response_types_supported":                       []string{"code"},
		"response_modes_supported":                       []string{"query"},
		"grant_types_supported":                          []string{"authorization_code", "refresh_token"},
		"code_challenge_methods_supported":               []string{"S256"},
		"token_endpoint_auth_methods_supported":          []string{"none", "client_secret_post", "client_secret_basic"},
		"scopes_supported":                               []string{Scope},
		"authorization_response_iss_parameter_supported": true,
	})
}

// Register is RFC 7591 dynamic client registration, limited to allowlisted
// redirect URIs.
func (s *Server) Register(now int64, body []byte) Reply {
	var request any
	if err := json.Unmarshal(body, &request); err != nil {
		return oauthError(400, "invalid_client_metadata", "body must be JSON")
	}
	fields, _ := request.(map[string]any)
	uris := []string{}
	if list, ok := fields["redirect_uris"].([]any); ok {
		for _, v := range list {
			if uri, ok := v.(string); ok {
				uris = append(uris, uri)
			}
		}
	}
	if len(uris) == 0 || !allAllowed(uris, s.extraURIs) {
		// Redirect URIs are public callback addresses; logging them is what
		// lets the owner allowlist a new client with --redirect-uri.
		logx.Warnf("rejected client registration; redirect_uris: %q", uris)
		return oauthError(400, "invalid_redirect_uri", "redirect_uris are not allowed")
	}
	method := "none"
	if m, ok := fields["token_endpoint_auth_method"].(string); ok {
		method = m
	}
	kind := ConfidentialClient
	switch method {
	case "none":
		kind = PublicClient
	case "client_secret_post", "client_secret_basic":
	default:
		return oauthError(400, "invalid_client_metadata", "unsupported token_endpoint_auth_method")
	}
	name := ""
	if n, ok := fields["client_name"].(string); ok {
		name = truncateRunes(n, 80)
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	g, err := s.store.Load()
	if err != nil {
		return serverError(err)
	}
	client := Client{ID: randomToken(clientIDPrefix), Kind: kind, RedirectURIs: uris, Name: name, Created: now}
	secret := ""
	if kind == ConfidentialClient {
		secret = randomToken(secretPrefix)
		client.SecretHash = hashToken(secret)
	}
	g, err = g.AddClient(client)
	if err != nil {
		return oauthError(400, "invalid_client_metadata", err.Error())
	}
	if err := s.store.Save(g, now); err != nil {
		return serverError(err)
	}
	response := map[string]any{
		"client_id":                  client.ID,
		"client_id_issued_at":        now,
		"redirect_uris":              uris,
		"token_endpoint_auth_method": method,
		"grant_types":                []string{"authorization_code", "refresh_token"},
		"response_types":             []string{"code"},
		"scope":                      Scope,
	}
	if name != "" {
		response["client_name"] = name
	}
	if kind == ConfidentialClient {
		response["client_secret"] = secret
		response["client_secret_expires_at"] = 0
	}
	return jsonReply(201, response)
}

func allAllowed(uris, extra []string) bool {
	for _, uri := range uris {
		if !RedirectAllowed(uri, extra) {
			return false
		}
	}
	return true
}

// resourceMatches accepts a missing RFC 8707 resource, the issuer, or the
// /mcp resource, ignoring trailing slashes.
func (s *Server) resourceMatches(p Params) bool {
	resource, ok := p.get("resource")
	if !ok {
		return true
	}
	resource = strings.TrimRight(resource, "/")
	return resource == s.publicURL || resource == s.resourceURL
}

// expire drops pending consents and codes past their deadline and old
// failures. Callers hold s.mu.
func (s *Server) expire(now int64) {
	for k, c := range s.consents {
		if c.Expires <= now {
			delete(s.consents, k)
		}
	}
	for k, c := range s.codes {
		if c.Expires <= now {
			delete(s.codes, k)
		}
	}
	s.limiter = s.limiter.Prune(now)
}

// AuthorizePage handles GET /oauth/authorize: validate the request and show
// the consent page.
func (s *Server) AuthorizePage(now int64, p Params) Reply {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.expire(now)
	clientID, ok := p.get("client_id")
	if !ok {
		return consentError("Missing client_id.")
	}
	g, err := s.store.Load()
	if err != nil {
		return serverError(err)
	}
	client, ok := g.Client(clientID)
	if !ok {
		return consentError("Unknown client. Remove and re-add the connector.")
	}
	redirectURI, ok := p.get("redirect_uri")
	if !ok || !slices.Contains(client.RedirectURIs, redirectURI) {
		return consentError("redirect_uri does not match the registered client.")
	}
	// From here on errors go back to the client.
	var state *string
	if v, ok := p.get("state"); ok {
		state = &v
	}
	back := func(code string) Reply { return redirectError(redirectURI, code, state, s.publicURL) }
	if v, _ := p.get("response_type"); v != "code" {
		return back("unsupported_response_type")
	}
	challenge, hasChallenge := p.get("code_challenge")
	if m, _ := p.get("code_challenge_method"); m != "S256" || !hasChallenge || len(challenge) < 43 || len(challenge) > 128 {
		return back("invalid_request")
	}
	if !s.resourceMatches(p) {
		return back("invalid_target")
	}
	csrf := randomToken("")
	s.consents[csrf] = PendingConsent{
		ClientID:      client.ID,
		ClientName:    client.Name,
		RedirectURI:   redirectURI,
		State:         state,
		CodeChallenge: challenge,
		Expires:       now + ConsentTTL,
	}
	return htmlReply(200, consentPage(csrf, client.Name, redirectURI, ""))
}

// AuthorizeSubmit handles POST /oauth/authorize. Transitions of the pending
// consent named by the form's CSRF token:
//
//	deny                  → removed; redirect with error=access_denied
//	limiter blocked       → kept; 429
//	wrong passphrase      → kept, failure recorded; 401 consent page
//	correct passphrase    → removed; code stored by hash for CodeTTL; redirect
func (s *Server) AuthorizeSubmit(now int64, form Params) Reply {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.expire(now)
	csrf, ok := form.get("request")
	if !ok {
		return consentError("Missing request. Start the sign-in again from the client.")
	}
	pending, ok := s.consents[csrf]
	if !ok {
		return consentError("This sign-in request expired. Start it again from the client.")
	}
	if d, _ := form.get("decision"); d == "deny" {
		delete(s.consents, csrf)
		return redirectError(pending.RedirectURI, "access_denied", pending.State, s.publicURL)
	}
	if s.limiter.Blocked() {
		return htmlReply(429, simplePage("Too many attempts", "Wait 15 minutes, then try again."))
	}
	passphrase, _ := form.get("passphrase")
	if !s.owner.Verify(passphrase) {
		s.limiter = s.limiter.Record(now)
		return htmlReply(401, consentPage(csrf, pending.ClientName, pending.RedirectURI, "Wrong passphrase."))
	}
	delete(s.consents, csrf)
	code := randomToken(codePrefix)
	s.codes[hashToken(code)] = AuthCode{
		ClientID:      pending.ClientID,
		RedirectURI:   pending.RedirectURI,
		CodeChallenge: pending.CodeChallenge,
		Expires:       now + CodeTTL,
	}
	location := appendQuery(pending.RedirectURI, "code", code)
	if pending.State != nil {
		location = appendQuery(location, "state", *pending.State)
	}
	return redirectReply(appendQuery(location, "iss", s.publicURL))
}

// Token handles POST /oauth/token. basic is the decoded HTTP Basic
// credential, or nil to read client_id/client_secret from the form.
func (s *Server) Token(now int64, form Params, basic *Credentials) Reply {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.expire(now)
	var clientID, secret string
	var hasClientID, hasSecret bool
	if basic != nil {
		clientID, hasClientID, secret, hasSecret = basic.ID, true, basic.Secret, true
	} else {
		clientID, hasClientID = form.get("client_id")
		secret, hasSecret = form.get("client_secret")
	}
	if !hasClientID {
		return oauthError(401, "invalid_client", "client_id is required")
	}
	g, err := s.store.Load()
	if err != nil {
		return serverError(err)
	}
	client, ok := g.Client(clientID)
	if !ok {
		return oauthError(401, "invalid_client", "unknown client")
	}
	if client.Kind == ConfidentialClient {
		presented := ""
		if hasSecret {
			presented = hashToken(secret)
		}
		if !constantTimeEqual(presented, client.SecretHash) {
			return oauthError(401, "invalid_client", "client authentication failed")
		}
	}
	if !s.resourceMatches(form) {
		return oauthError(400, "invalid_target", "resource does not match this server")
	}
	var grantID string
	switch t, _ := form.get("grant_type"); t {
	case "authorization_code":
		code, _ := form.get("code")
		grant, ok := s.codes[hashToken(code)]
		delete(s.codes, hashToken(code))
		if !ok {
			return oauthError(400, "invalid_grant", "code is invalid, used, or expired")
		}
		verifier, _ := form.get("code_verifier")
		redirectURI, hasRedirect := form.get("redirect_uri")
		if grant.ClientID != clientID || !hasRedirect || redirectURI != grant.RedirectURI ||
			!VerifyPKCE(verifier, grant.CodeChallenge) {
			return oauthError(400, "invalid_grant", "code does not match this request")
		}
		grantID = randomToken("")
	case "refresh_token":
		refresh, _ := form.get("refresh_token")
		next, id, err := g.Rotate(clientID, hashToken(refresh), now)
		switch {
		case errors.Is(err, ErrRefreshReused):
			if err := s.store.Save(next, now); err != nil {
				return serverError(err)
			}
			return oauthError(400, "invalid_grant", err.Error())
		case err != nil:
			return oauthError(400, "invalid_grant", err.Error())
		}
		g, grantID = next, id
	default:
		return oauthError(400, "unsupported_grant_type", "use authorization_code or refresh_token")
	}
	access, refresh := randomToken(accessPrefix), randomToken(refreshPrefix)
	g = g.Issue(clientID, grantID, hashToken(access), hashToken(refresh), now)
	if err := s.store.Save(g, now); err != nil {
		return serverError(err)
	}
	return jsonReply(200, map[string]any{
		"access_token":  access,
		"token_type":    "Bearer",
		"expires_in":    AccessTTL,
		"refresh_token": refresh,
		"scope":         Scope,
	})
}

// PKCEChallenge is the S256 challenge for verifier.
func PKCEChallenge(verifier string) string {
	sum := sha256.Sum256([]byte(verifier))
	return base64.RawURLEncoding.EncodeToString(sum[:])
}

// VerifyPKCE checks verifier against an S256 challenge in constant time.
func VerifyPKCE(verifier, challenge string) bool {
	return constantTimeEqual(PKCEChallenge(verifier), challenge)
}

func randomToken(prefix string) string {
	b := make([]byte, 32)
	_, _ = rand.Read(b) // crypto/rand.Read never fails
	return prefix + base64.RawURLEncoding.EncodeToString(b)
}

func hashToken(token string) string {
	sum := sha256.Sum256([]byte(token))
	return hex.EncodeToString(sum[:])
}

func constantTimeEqual(a, b string) bool {
	return subtle.ConstantTimeCompare([]byte(a), []byte(b)) == 1
}

func truncateRunes(s string, n int) string {
	for i := range s {
		if n == 0 {
			return s[:i]
		}
		n--
	}
	return s
}

func redirectError(redirectURI, code string, state *string, issuer string) Reply {
	location := appendQuery(redirectURI, "error", code)
	if state != nil {
		location = appendQuery(location, "state", *state)
	}
	return redirectReply(appendQuery(location, "iss", issuer))
}

func consentError(message string) Reply {
	return htmlReply(400, simplePage("Cannot sign in", message))
}

// PercentEncode escapes everything except RFC 3986 unreserved characters.
func PercentEncode(value string) string {
	var b strings.Builder
	for i := 0; i < len(value); i++ {
		c := value[i]
		if 'a' <= c && c <= 'z' || 'A' <= c && c <= 'Z' || '0' <= c && c <= '9' || strings.IndexByte("-._~", c) >= 0 {
			b.WriteByte(c)
		} else {
			fmt.Fprintf(&b, "%%%02X", c)
		}
	}
	return b.String()
}

func appendQuery(uri, key, value string) string {
	sep := "?"
	if strings.Contains(uri, "?") {
		sep = "&"
	}
	return uri + sep + key + "=" + PercentEncode(value)
}

func marshal(v any) []byte {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		panic(fmt.Sprintf("oauth: encode %T: %v", v, err))
	}
	return bytes.TrimSuffix(buf.Bytes(), []byte("\n"))
}
