package keys

import (
	"crypto/hmac"
	"crypto/pbkdf2"
	"crypto/sha512"
	"crypto/subtle"
	"encoding/hex"
	"sort"

	"github.com/xiaotianxt/tg/internal/par"
)

const (
	saltSize  = 16
	ivSize    = 16
	hmacSize  = 64
	keySize   = 32
	kdfRounds = 256_000
	// deriveAutoCap bounds parallel KDF derivation when --jobs is 0. Each
	// group runs 256k PBKDF2 rounds, so this is CPU bound, not I/O bound.
	deriveAutoCap = 8
)

// KeyStatus is one database's key outcome.
type KeyStatus int

const (
	// StatusMissing means no verified key is available for the database.
	StatusMissing KeyStatus = iota
	// StatusReused means an existing all_keys.json key still verifies.
	StatusReused
	// StatusDerived means the key was derived from account material now.
	StatusDerived
)

func (s KeyStatus) String() string {
	switch s {
	case StatusReused:
		return "reused"
	case StatusDerived:
		return "derived"
	default:
		return "missing"
	}
}

// DatabaseOutcome is the per-database result of a derivation.
type DatabaseOutcome struct {
	RelPath string
	Status  KeyStatus
}

// Derivation is the result of deriving keys for one db_storage tree. Counts
// come from the per-database outcomes, never from subtraction.
type Derivation struct {
	// Outcomes are in sorted relative-path order, one per encrypted database.
	Outcomes []DatabaseOutcome
	// Keys holds the verified keys to write to all_keys.json.
	Keys KeyFile
	// DerivedSalts is how many distinct salts the KDF ran for and matched.
	DerivedSalts int
}

// Total is the number of encrypted databases considered.
func (d Derivation) Total() int { return len(d.Outcomes) }

// Reused counts databases whose existing key was kept.
func (d Derivation) Reused() int { return d.count(StatusReused) }

// Derived counts databases that received a freshly derived key.
func (d Derivation) Derived() int { return d.count(StatusDerived) }

func (d Derivation) count(status KeyStatus) int {
	n := 0
	for _, o := range d.Outcomes {
		if o.Status == status {
			n++
		}
	}
	return n
}

// Missing lists databases without a verified key, in sorted order.
func (d Derivation) Missing() []string {
	var out []string
	for _, o := range d.Outcomes {
		if o.Status == StatusMissing {
			out = append(out, o.RelPath)
		}
	}
	return out
}

// IsComplete reports whether every considered database has a verified key.
func (d Derivation) IsComplete() bool {
	return d.Total() > 0 && len(d.Missing()) == 0
}

type encryptedPage struct {
	rel   string
	page1 []byte
	salt  [16]byte
}

// DeriveFromStorage reads every encrypted database under dbStorage and
// returns a verified key for each one it can, reusing a still-valid existing
// key when present and otherwise running the database KDF once per distinct
// salt across up to jobs goroutines.
func DeriveFromStorage(material *[32]byte, dbStorage string, existing KeyFile, jobs int) (Derivation, error) {
	pages, err := loadEncryptedPages(dbStorage)
	if err != nil {
		return Derivation{}, err
	}
	return deriveForPages(material, pages, existing, jobs), nil
}

// ValidatesAgainstStorage reports whether material derives a key that
// verifies against at least one encrypted database under dbStorage.
func ValidatesAgainstStorage(material *[32]byte, dbStorage string) (bool, error) {
	pages, err := loadEncryptedPages(dbStorage)
	if err != nil {
		return false, err
	}
	for _, p := range pages {
		key := deriveEncryptionKey(material, &p.salt)
		ok := keyMatchesPage1(key[:], p.page1)
		clear(key[:])
		if ok {
			return true, nil
		}
	}
	return false, nil
}

func loadEncryptedPages(dbStorage string) ([]encryptedPage, error) {
	sources, err := CollectSourceDatabases(dbStorage)
	if err != nil {
		return nil, err
	}
	var pages []encryptedPage
	for _, src := range sources {
		page1, err := ReadPage1(src.FullPath, src.RelPath)
		if err != nil {
			return nil, err
		}
		if len(page1) >= len(sqliteHeader) &&
			subtle.ConstantTimeCompare(page1[:len(sqliteHeader)], sqliteHeader) == 1 {
			// A plaintext SQLite database needs no key.
			continue
		}
		var p encryptedPage
		p.rel = src.RelPath
		p.page1 = page1
		copy(p.salt[:], page1[:saltSize])
		pages = append(pages, p)
	}
	return pages, nil
}

type saltGroup struct {
	salt    [16]byte
	sources []encryptedPage
}

func deriveForPages(material *[32]byte, pages []encryptedPage, existing KeyFile, jobs int) Derivation {
	keys := KeyFile{}
	status := make(map[string]KeyStatus, len(pages))
	allPaths := make([]string, 0, len(pages))

	grouped := map[[16]byte][]encryptedPage{}
	var order [][16]byte
	for _, p := range pages {
		allPaths = append(allPaths, p.rel)
		status[p.rel] = StatusMissing
		if key := validExistingKey(existing, p); key != nil {
			keys[p.rel] = keyFields(key, &p.salt)
			status[p.rel] = StatusReused
			clear(key[:])
			continue
		}
		if _, seen := grouped[p.salt]; !seen {
			order = append(order, p.salt)
		}
		grouped[p.salt] = append(grouped[p.salt], p)
	}

	// Derive once per distinct salt, in a stable salt order so parallel and
	// serial runs produce identical results.
	sort.Slice(order, func(i, j int) bool {
		return lessBytes(order[i][:], order[j][:])
	})
	groups := make([]saltGroup, len(order))
	for i, salt := range order {
		groups[i] = saltGroup{salt: salt, sources: grouped[salt]}
	}

	type derived struct {
		group saltGroup
		key   [32]byte
	}
	results := par.Map(groups, par.Jobs(jobs, deriveAutoCap), func(g saltGroup) derived {
		return derived{group: g, key: deriveEncryptionKey(material, &g.salt)}
	})

	derivedSalts := 0
	for i := range results {
		key := results[i].key
		keyHex := hex.EncodeToString(key[:])
		saltHex := hex.EncodeToString(results[i].group.salt[:])
		matched := false
		for _, src := range results[i].group.sources {
			if !keyMatchesPage1(key[:], src.page1) {
				continue
			}
			matched = true
			keys[src.rel] = map[string]string{"enc_key": keyHex, "salt": saltHex}
			status[src.rel] = StatusDerived
		}
		if matched {
			derivedSalts++
		}
		clear(key[:])
		clear(results[i].key[:])
	}

	sort.Strings(allPaths)
	outcomes := make([]DatabaseOutcome, len(allPaths))
	for i, rel := range allPaths {
		outcomes[i] = DatabaseOutcome{RelPath: rel, Status: status[rel]}
	}
	return Derivation{Outcomes: outcomes, Keys: keys, DerivedSalts: derivedSalts}
}

func keyFields(key *[32]byte, salt *[16]byte) map[string]string {
	return map[string]string{
		"enc_key": hex.EncodeToString(key[:]),
		"salt":    hex.EncodeToString(salt[:]),
	}
}

// validExistingKey returns the existing key for a database when it decodes to
// 32 bytes and still verifies against the database's page 1, else nil.
func validExistingKey(existing KeyFile, p encryptedPage) *[32]byte {
	fields, ok := existing[p.rel]
	if !ok {
		return nil
	}
	keyHex, ok := fields["enc_key"]
	if !ok {
		return nil
	}
	key, err := decodeHexKey(keyHex)
	if err != nil {
		return nil
	}
	if keyMatchesPage1(key[:], p.page1) {
		return key
	}
	clear(key[:])
	return nil
}

func deriveEncryptionKey(material *[32]byte, salt *[16]byte) [32]byte {
	out, _ := pbkdf2.Key(sha512.New, string(material[:]), salt[:], kdfRounds, keySize)
	var key [32]byte
	copy(key[:], out)
	clear(out)
	return key
}

// keyMatchesPage1 verifies a candidate key against page 1's SQLCipher HMAC:
// the per-page MAC key is PBKDF2-HMAC-SHA512(key, salt^0x3a, 2), and the MAC
// covers the page body plus the little-endian page number.
func keyMatchesPage1(key, page1 []byte) bool {
	if len(key) != keySize || len(page1) < PageSize {
		return false
	}
	var macSalt [saltSize]byte
	for i := 0; i < saltSize; i++ {
		macSalt[i] = page1[i] ^ 0x3a
	}
	macKey, err := pbkdf2.Key(sha512.New, string(key), macSalt[:], 2, keySize)
	if err != nil {
		return false
	}
	defer clear(macKey)
	mac := hmac.New(sha512.New, macKey)
	reserve := ivSize + hmacSize
	mac.Write(page1[saltSize : PageSize-reserve+ivSize])
	mac.Write([]byte{1, 0, 0, 0}) // page number 1, little-endian
	return hmac.Equal(mac.Sum(nil), page1[PageSize-hmacSize:PageSize])
}

func lessBytes(a, b []byte) bool {
	for i := 0; i < len(a) && i < len(b); i++ {
		if a[i] != b[i] {
			return a[i] < b[i]
		}
	}
	return len(a) < len(b)
}
