package keys

import (
	"encoding/hex"
	"os"
	"path/filepath"
	"testing"
)

const knownKeyHex = "699f2ac9232d8ea2f443137cb626202ca98943df031715298614a5251ee7d973"

// storedHMAC verifies against knownKeyHex over an all-zero page body carrying
// the salt bytes 16..31; it is the fixture from the Rust KDF tests.
const storedHMACHex = "69b73d4edb08e3de8aff631726fe184a47a03973c5128b8fb9abb0d111cfd78e" +
	"3a47f06f9aab559022c82da91ad72a3e144396e808b78783a8d5244bcc6f195d"

func testMaterial() *[32]byte {
	var m [32]byte
	for i := range m {
		m[i] = byte(i)
	}
	return &m
}

func testSalt() [16]byte {
	var s [16]byte
	for i := range s {
		s[i] = byte(i + 16)
	}
	return s
}

func fixturePage(salt [16]byte) []byte {
	page := make([]byte, PageSize)
	copy(page[:16], salt[:])
	hmacBytes, _ := hex.DecodeString(storedHMACHex)
	copy(page[PageSize-64:], hmacBytes)
	return page
}

func TestDeriveEncryptionKeyMatchesKnownVector(t *testing.T) {
	salt := testSalt()
	key := deriveEncryptionKey(testMaterial(), &salt)
	if got := hex.EncodeToString(key[:]); got != knownKeyHex {
		t.Fatalf("derived key = %s, want %s", got, knownKeyHex)
	}
}

func TestKeyMatchesPage1(t *testing.T) {
	salt := testSalt()
	page := fixturePage(salt)
	key, _ := hex.DecodeString(knownKeyHex)
	if !keyMatchesPage1(key, page) {
		t.Fatal("known key should verify against fixture page 1")
	}
	key[0] ^= 0xff
	if keyMatchesPage1(key, page) {
		t.Fatal("a wrong key must not verify")
	}
}

func TestDeriveOncePerSaltMapsEveryMatchingDatabase(t *testing.T) {
	salt := testSalt()
	page := fixturePage(salt)
	pages := []encryptedPage{
		{rel: "message/message_0.db", page1: page, salt: salt},
		{rel: "session/session.db", page1: page, salt: salt},
	}
	d := deriveForPages(testMaterial(), pages, KeyFile{}, 4)

	if d.DerivedSalts != 1 {
		t.Fatalf("DerivedSalts = %d, want 1", d.DerivedSalts)
	}
	if d.Derived() != 2 || d.Reused() != 0 || d.Total() != 2 {
		t.Fatalf("counts: derived=%d reused=%d total=%d", d.Derived(), d.Reused(), d.Total())
	}
	for _, rel := range []string{"message/message_0.db", "session/session.db"} {
		if d.Keys[rel]["enc_key"] != knownKeyHex {
			t.Fatalf("%s enc_key = %q", rel, d.Keys[rel]["enc_key"])
		}
	}
	if !d.IsComplete() {
		t.Fatal("derivation should be complete")
	}
}

func TestReusesValidExistingKeyWithoutRederiving(t *testing.T) {
	salt := testSalt()
	page := fixturePage(salt)
	// A material that does not derive the known key, so a reused key is the
	// only way the database can be covered.
	var wrongMaterial [32]byte
	for i := range wrongMaterial {
		wrongMaterial[i] = 0xff
	}
	existing := KeyFile{"session/session.db": {"enc_key": knownKeyHex}}
	pages := []encryptedPage{{rel: "session/session.db", page1: page, salt: salt}}

	d := deriveForPages(&wrongMaterial, pages, existing, 4)

	if d.Reused() != 1 || d.Derived() != 0 || d.DerivedSalts != 0 {
		t.Fatalf("reused=%d derived=%d salts=%d", d.Reused(), d.Derived(), d.DerivedSalts)
	}
	if d.Keys["session/session.db"]["enc_key"] != knownKeyHex {
		t.Fatalf("reused key not preserved: %v", d.Keys["session/session.db"])
	}
	if d.Keys["session/session.db"]["salt"] != hex.EncodeToString(salt[:]) {
		t.Fatalf("reused key missing salt: %v", d.Keys["session/session.db"])
	}
}

func TestInvalidExistingKeyIsIgnored(t *testing.T) {
	salt := testSalt()
	page := fixturePage(salt)
	existing := KeyFile{"session/session.db": {"enc_key": "00"}} // wrong length/value
	pages := []encryptedPage{{rel: "session/session.db", page1: page, salt: salt}}

	d := deriveForPages(testMaterial(), pages, existing, 1)
	if d.Reused() != 0 || d.Derived() != 1 {
		t.Fatalf("a non-verifying existing key must be re-derived: reused=%d derived=%d", d.Reused(), d.Derived())
	}
}

func TestDeriveAndValidateFromStorageTree(t *testing.T) {
	dir := t.TempDir()
	salt := testSalt()
	sessionDir := filepath.Join(dir, "session")
	if err := os.MkdirAll(sessionDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(sessionDir, "session.db"), fixturePage(salt), 0o600); err != nil {
		t.Fatal(err)
	}

	d, err := DeriveFromStorage(testMaterial(), dir, KeyFile{}, 4)
	if err != nil {
		t.Fatal(err)
	}
	if !d.IsComplete() || d.Total() != 1 || d.Derived() != 1 {
		t.Fatalf("outcome: complete=%v total=%d derived=%d", d.IsComplete(), d.Total(), d.Derived())
	}
	if len(d.Missing()) != 0 {
		t.Fatalf("missing: %v", d.Missing())
	}
	valid, err := ValidatesAgainstStorage(testMaterial(), dir)
	if err != nil || !valid {
		t.Fatalf("ValidatesAgainstStorage = %v, %v", valid, err)
	}
}

func TestPlaintextDatabaseIsSkipped(t *testing.T) {
	dir := t.TempDir()
	page := make([]byte, PageSize)
	copy(page, sqliteHeader)
	if err := os.WriteFile(filepath.Join(dir, "plain.db"), page, 0o600); err != nil {
		t.Fatal(err)
	}
	d, err := DeriveFromStorage(testMaterial(), dir, KeyFile{}, 1)
	if err != nil {
		t.Fatal(err)
	}
	if d.Total() != 0 {
		t.Fatalf("plaintext database should not be considered: total=%d", d.Total())
	}
}
