package sqlcipher

import (
	"path/filepath"
	"testing"
)

const keySpec = "x'000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f" + "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf'"

func TestEncryptedRoundTripAndTokenizerAlias(t *testing.T) {
	path := filepath.Join(t.TempDir(), "x.db")
	w, err := Open(path, OpenOptions{Key: []byte(keySpec)})
	if err != nil {
		t.Fatal(err)
	}
	if err := w.RegisterCompatibleTokenizer(); err != nil {
		t.Fatal(err)
	}
	if err := w.Exec(`CREATE VIRTUAL TABLE f USING fts5(tokenize = 'MMFtsTokenizer disable_pinyin', acontent);
		INSERT INTO f VALUES ('今天 吃饭 hello world');`); err != nil {
		t.Fatal(err)
	}
	w.Close()

	r, err := Open(path, OpenOptions{Key: []byte(keySpec), ReadOnly: true})
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	if err := r.RegisterCompatibleTokenizer(); err != nil {
		t.Fatal(err)
	}
	var got string
	ok, err := r.QueryRow("SELECT acontent FROM f WHERE f MATCH ?1", []any{"吃饭"}, func(s *Stmt) error {
		got = s.ColumnText(0)
		return nil
	})
	if err != nil || !ok || got == "" {
		t.Fatalf("match: ok=%v err=%v got=%q", ok, err, got)
	}
	if err := r.Exec("CREATE TABLE y(a)"); err == nil {
		t.Fatal("read-only connection accepted a write")
	}

	bad, err := Open(path, OpenOptions{Key: []byte("x'" + string(make([]byte, 0)) + "ff" + keySpec[4:]), ReadOnly: true})
	if err == nil {
		_, err = bad.TableExists("f")
		bad.Close()
	}
	if !IsCode(err, CodeNotADB) {
		t.Fatalf("wrong key: got %v, want SQLITE_NOTADB", err)
	}
}

func TestEmptyStringBindsAsText(t *testing.T) {
	c, err := Open(":memory:", OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	var typ string
	if _, err := c.QueryRow("SELECT typeof(?1)", []any{""}, func(s *Stmt) error { typ = s.ColumnText(0); return nil }); err != nil {
		t.Fatal(err)
	}
	if typ != "text" {
		t.Fatalf("empty string bound as %s", typ)
	}
}
