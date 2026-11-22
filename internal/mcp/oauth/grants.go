package oauth

import (
	"encoding/json"
	"errors"
	"fmt"
	"slices"

	"github.com/xiaotianxt/tg/internal/mcp/config"
)

// ClientKind says how a client authenticates at the token endpoint.
type ClientKind uint8

const (
	// PublicClient presents only its client_id (token_endpoint_auth_method
	// "none") and relies on PKCE.
	PublicClient ClientKind = iota
	// ConfidentialClient also presents a client secret, by form post or
	// HTTP Basic.
	ConfidentialClient
)

// Client is one registered OAuth client.
type Client struct {
	ID   string
	Kind ClientKind
	// SecretHash is the SHA-256 hex of the secret; set only for
	// ConfidentialClient.
	SecretHash   string
	RedirectURIs []string
	// Name is the client-supplied display name, "" when none was given.
	Name    string
	Created int64
}

// clientFile is the on-disk shape shared with tg 2.x.
type clientFile struct {
	ClientID     string   `json:"client_id"`
	SecretHash   *string  `json:"secret_hash"`
	RedirectURIs []string `json:"redirect_uris"`
	ClientName   *string  `json:"client_name"`
	Created      int64    `json:"created"`
}

func (c Client) MarshalJSON() ([]byte, error) {
	f := clientFile{ClientID: c.ID, RedirectURIs: c.RedirectURIs, Created: c.Created}
	if c.Kind == ConfidentialClient {
		f.SecretHash = &c.SecretHash
	}
	if c.Name != "" {
		f.ClientName = &c.Name
	}
	return json.Marshal(f)
}

func (c *Client) UnmarshalJSON(data []byte) error {
	var f clientFile
	if err := json.Unmarshal(data, &f); err != nil {
		return err
	}
	*c = Client{ID: f.ClientID, Kind: PublicClient, RedirectURIs: f.RedirectURIs, Created: f.Created}
	if f.SecretHash != nil {
		c.Kind, c.SecretHash = ConfidentialClient, *f.SecretHash
	}
	if f.ClientName != nil {
		c.Name = *f.ClientName
	}
	return nil
}

// TokenKind distinguishes access from refresh tokens.
type TokenKind uint8

const (
	AccessToken TokenKind = iota
	RefreshToken
)

func (k TokenKind) MarshalText() ([]byte, error) {
	switch k {
	case AccessToken:
		return []byte("access"), nil
	case RefreshToken:
		return []byte("refresh"), nil
	}
	return nil, fmt.Errorf("unknown token kind %d", k)
}

func (k *TokenKind) UnmarshalText(b []byte) error {
	switch string(b) {
	case "access":
		*k = AccessToken
	case "refresh":
		*k = RefreshToken
	default:
		return fmt.Errorf("unknown token kind %q", b)
	}
	return nil
}

// TokenState is where a token is in its life. Active and Rotated are stored;
// Expired is derived from the clock (Token.StateAt).
type TokenState uint8

const (
	TokenActive TokenState = iota
	// TokenRotated is a refresh token already exchanged once. Presenting it
	// again is the sign of a stolen token and revokes its whole grant.
	TokenRotated
	TokenExpired
)

// Token is one issued token, stored only by hash.
type Token struct {
	Hash     string    `json:"hash"`
	Kind     TokenKind `json:"kind"`
	ClientID string    `json:"client_id"`
	GrantID  string    `json:"grant_id"`
	Expires  int64     `json:"expires"`
	// Rotated is the stored half of TokenState (file field "used").
	Rotated bool `json:"used"`
}

// StateAt is the token's state at now. A rotated token stays Rotated after
// expiry, so its reuse is still detected until it is pruned.
func (t Token) StateAt(now int64) TokenState {
	switch {
	case t.Rotated:
		return TokenRotated
	case t.Expires <= now:
		return TokenExpired
	default:
		return TokenActive
	}
}

// Grants is the persistent authorization state (oauth.json). Methods are
// pure: they return a new value and never touch the receiver's slices.
type Grants struct {
	Clients []Client `json:"clients"`
	Tokens  []Token  `json:"tokens"`
}

// Summary describes Grants for `tg mcp status`.
type Summary struct {
	Clients []Client
	// ActiveGrants counts distinct grants holding an active token.
	ActiveGrants int
}

var (
	ErrClientLimit    = errors.New("client limit reached; run `tg mcp revoke`")
	ErrInvalidToken   = errors.New("invalid or expired access token")
	ErrRefreshInvalid = errors.New("refresh token is invalid")
	ErrRefreshReused  = errors.New("refresh token was already used; sign in again")
	ErrRefreshExpired = errors.New("refresh token expired")
)

// Client returns the registered client with id.
func (g Grants) Client(id string) (Client, bool) {
	for _, c := range g.Clients {
		if c.ID == id {
			return c, true
		}
	}
	return Client{}, false
}

// AddClient registers c, refusing more than MaxClients.
func (g Grants) AddClient(c Client) (Grants, error) {
	if len(g.Clients) >= MaxClients {
		return g, ErrClientLimit
	}
	return Grants{Clients: append(slices.Clip(g.Clients), c), Tokens: g.Tokens}, nil
}

// Authenticate returns the client an active access token was issued to.
func (g Grants) Authenticate(accessHash string, now int64) (string, error) {
	for _, t := range g.Tokens {
		if t.Kind == AccessToken && t.Hash == accessHash && t.StateAt(now) == TokenActive {
			return t.ClientID, nil
		}
	}
	return "", ErrInvalidToken
}

// Issue adds an access and a refresh token for grantID.
func (g Grants) Issue(clientID, grantID, accessHash, refreshHash string, now int64) Grants {
	tokens := append(slices.Clip(g.Tokens),
		Token{Hash: accessHash, Kind: AccessToken, ClientID: clientID, GrantID: grantID, Expires: now + AccessTTL},
		Token{Hash: refreshHash, Kind: RefreshToken, ClientID: clientID, GrantID: grantID, Expires: now + RefreshTTL},
	)
	return Grants{Clients: g.Clients, Tokens: tokens}
}

// Rotate marks clientID's refresh token used and returns its grant, which
// the caller then reissues with Issue. Presenting a rotated token returns
// ErrRefreshReused together with Grants that no longer hold any token of
// that grant; the caller must save them.
func (g Grants) Rotate(clientID, refreshHash string, now int64) (Grants, string, error) {
	i := slices.IndexFunc(g.Tokens, func(t Token) bool {
		return t.Kind == RefreshToken && t.Hash == refreshHash && t.ClientID == clientID
	})
	if i < 0 {
		return g, "", ErrRefreshInvalid
	}
	t := g.Tokens[i]
	switch t.StateAt(now) {
	case TokenRotated:
		kept := slices.DeleteFunc(slices.Clone(g.Tokens), func(x Token) bool { return x.GrantID == t.GrantID })
		return Grants{Clients: g.Clients, Tokens: kept}, "", ErrRefreshReused
	case TokenExpired:
		return g, "", ErrRefreshExpired
	}
	tokens := slices.Clone(g.Tokens)
	tokens[i].Rotated = true
	return Grants{Clients: g.Clients, Tokens: tokens}, t.GrantID, nil
}

// Prune drops expired tokens; Store.Save applies it.
func (g Grants) Prune(now int64) Grants {
	kept := slices.DeleteFunc(slices.Clone(g.Tokens), func(t Token) bool { return t.Expires <= now })
	return Grants{Clients: g.Clients, Tokens: kept}
}

// RevokeClient removes one client and its tokens; removed is 0 or 1.
func (g Grants) RevokeClient(id string) (out Grants, removed int) {
	clients := slices.DeleteFunc(slices.Clone(g.Clients), func(c Client) bool { return c.ID == id })
	tokens := slices.DeleteFunc(slices.Clone(g.Tokens), func(t Token) bool { return t.ClientID == id })
	return Grants{Clients: clients, Tokens: tokens}, len(g.Clients) - len(clients)
}

// RevokeAll removes every client and token.
func (g Grants) RevokeAll() (out Grants, removed int) {
	return Grants{Clients: []Client{}, Tokens: []Token{}}, len(g.Clients)
}

// Summarize counts registered clients and grants with an active token.
func (g Grants) Summarize(now int64) Summary {
	grants := map[string]bool{}
	for _, t := range g.Tokens {
		if t.StateAt(now) == TokenActive {
			grants[t.GrantID] = true
		}
	}
	return Summary{Clients: g.Clients, ActiveGrants: len(grants)}
}

// Store is the file-backed Grants, re-read on every operation so that
// `tg mcp revoke` takes effect immediately on a running server.
type Store struct{ path string }

func NewStore(dir string) Store { return Store{path: config.OAuthPath(dir)} }

func (s Store) Load() (Grants, error) {
	g := Grants{Clients: []Client{}, Tokens: []Token{}}
	if _, err := config.ReadPrivateJSON(s.path, &g); err != nil {
		return Grants{}, err
	}
	return g, nil
}

// Save writes g without its expired tokens.
func (s Store) Save(g Grants, now int64) error {
	g = g.Prune(now)
	if g.Clients == nil {
		g.Clients = []Client{}
	}
	if g.Tokens == nil {
		g.Tokens = []Token{}
	}
	return config.WritePrivateJSON(s.path, g)
}

// RevokeClient removes one client and its tokens from the file.
func (s Store) RevokeClient(id string, now int64) (int, error) {
	return s.revoke(func(g Grants) (Grants, int) { return g.RevokeClient(id) }, now)
}

// RevokeAll removes every client and token from the file.
func (s Store) RevokeAll(now int64) (int, error) {
	return s.revoke(Grants.RevokeAll, now)
}

func (s Store) revoke(f func(Grants) (Grants, int), now int64) (int, error) {
	g, err := s.Load()
	if err != nil {
		return 0, err
	}
	g, removed := f(g)
	return removed, s.Save(g, now)
}
