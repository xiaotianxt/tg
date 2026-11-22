// Package store opens the client's encrypted source databases read-only. It
// owns key loading, database discovery, and per-command snapshots; it never
// writes under db_storage.
package store

import (
	"bytes"
	"encoding/hex"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"

	"github.com/xiaotianxt/tg/internal/keys"
	"github.com/xiaotianxt/tg/internal/paths"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
)

var sqliteHeader = []byte("SQLite format 3\x00")

// Store is one command's view of the source databases.
type Store struct {
	Root           string
	databases      map[string]entry
	keyRecovery    string
	accountUser    string
	snapshotMu     sync.Mutex
	snapshotDir    string
	snapshotSource map[string]string // rel path -> file to read; "" means the live file
}

type entry struct {
	path string
	key  *[32]byte // nil when all_keys.json has no key for this database
}

// Discover resolves the database directory (auto-detected under $HOME when
// dbDir is empty), loads keys from keysPath, and catalogs every *.db file.
func Discover(dbDir, keysPath string) (*Store, error) {
	root, err := resolveRoot(dbDir)
	if err != nil {
		return nil, err
	}
	recovery := paths.KeyRecoveryCommand(root, keysPath)
	raw, err := LoadKeyFile(keysPath)
	if err != nil {
		return nil, fmt.Errorf("%w; repair with: %s", err, recovery)
	}
	if raw == nil {
		return nil, fmt.Errorf("Database keys are missing at %s; run: %s", keysPath, recovery)
	}
	keys, err := decodeKeys(raw)
	if err != nil {
		return nil, fmt.Errorf("%w; repair with: %s", err, recovery)
	}
	sources, err := CollectSourceDatabases(root)
	if err != nil {
		return nil, err
	}
	s := &Store{
		Root:           root,
		databases:      make(map[string]entry, len(sources)),
		keyRecovery:    recovery,
		snapshotSource: map[string]string{},
	}
	for _, src := range sources {
		s.databases[src.RelPath] = entry{path: src.FullPath, key: keys[src.RelPath]}
	}
	// The account directory holds db_storage directly or one level up.
	for dir, i := filepath.Dir(root), 0; i < 2; dir, i = filepath.Dir(dir), i+1 {
		if id := paths.AccountID(dir); id != "" {
			s.accountUser = id
			break
		}
	}
	return s, nil
}

func resolveRoot(dbDir string) (string, error) {
	if dbDir != "" {
		if st, err := os.Stat(dbDir); err == nil && st.IsDir() {
			return dbDir, nil
		}
		return "", fmt.Errorf("Source database directory does not exist: %s", dbDir)
	}
	home := os.Getenv("HOME")
	if home == "" {
		return "", errors.New("Cannot find home directory for source discovery")
	}
	root := paths.FindDBStorageDir(home)
	if root == "" {
		return "", errors.New("Cannot auto-detect the source database directory; use --db-dir")
	}
	return root, nil
}

// AccountUsername is the signed-in account's username from the account
// directory name, or "" when the name does not carry one.
func (s *Store) AccountUsername() string { return s.accountUser }

// KeyRecovery is the command that rebuilds keys for this store.
func (s *Store) KeyRecovery() string { return s.keyRecovery }

// Has reports whether rel is a cataloged database.
func (s *Store) Has(rel string) bool {
	_, ok := s.databases[rel]
	return ok
}

// Databases lists cataloged relative paths in sorted order.
func (s *Store) Databases() []string {
	out := make([]string, 0, len(s.databases))
	for rel := range s.databases {
		out = append(out, rel)
	}
	sort.Strings(out)
	return out
}

// MessageShards lists message/message_N.db in numeric order.
func (s *Store) MessageShards() []string {
	var out []string
	for rel := range s.databases {
		if n, ok := ShardNumber(rel); ok && n >= 0 {
			out = append(out, rel)
		}
	}
	sort.Slice(out, func(i, j int) bool {
		a, _ := ShardNumber(out[i])
		b, _ := ShardNumber(out[j])
		return a < b
	})
	return out
}

// ShardNumber parses N from message/message_N.db.
func ShardNumber(rel string) (int, bool) {
	name, ok := strings.CutPrefix(filepath.ToSlash(rel), "message/message_")
	if !ok {
		return 0, false
	}
	digits, ok := strings.CutSuffix(name, ".db")
	if !ok || digits == "" {
		return 0, false
	}
	n := 0
	for _, c := range digits {
		if c < '0' || c > '9' {
			return 0, false
		}
		n = n*10 + int(c-'0')
	}
	return n, true
}

// Open returns a read-only connection to rel inside a read transaction, so
// every query on it sees one snapshot.
func (s *Store) Open(rel string) (*sqlcipher.Conn, error) {
	e, ok := s.databases[rel]
	if !ok {
		return nil, fmt.Errorf("Source database is not present in this command snapshot: %s", rel)
	}
	if e.key == nil {
		return nil, fmt.Errorf("No key for source database %s; run: %s", rel, s.keyRecovery)
	}
	page1, err := ReadPage1(e.path, rel)
	if err != nil {
		return nil, err
	}
	if bytes.HasPrefix(page1, sqliteHeader) {
		return nil, fmt.Errorf("Source database is not encrypted: %s", rel)
	}
	var salt [16]byte
	copy(salt[:], page1[:16])
	src, err := s.readSource(rel, e.path, e.key, &salt)
	if err != nil {
		return nil, err
	}
	return s.openEncrypted(src, e.key, &salt, rel)
}

// OpenOptional opens rel when it is cataloged, returning nil otherwise.
func (s *Store) OpenOptional(rel string) (*sqlcipher.Conn, error) {
	if !s.Has(rel) {
		return nil, nil
	}
	return s.Open(rel)
}

func (s *Store) openEncrypted(path string, key *[32]byte, salt *[16]byte, rel string) (*sqlcipher.Conn, error) {
	spec := keySpec(key, salt)
	conn, err := sqlcipher.Open(path, sqlcipher.OpenOptions{ReadOnly: true, Key: spec})
	clear(spec)
	if err != nil {
		return nil, fmt.Errorf("Cannot open %s read-only: %w", rel, err)
	}
	if err := conn.Exec("BEGIN"); err != nil {
		conn.Close()
		return nil, fmt.Errorf("Cannot begin read snapshot for %s: %w", rel, err)
	}
	if _, err := conn.QueryRow("SELECT EXISTS(SELECT 1 FROM sqlite_schema LIMIT 1)", nil, func(*sqlcipher.Stmt) error { return nil }); err != nil {
		conn.Close()
		// A wrong key reads as SQLite's own message ("file is not a
		// database"), as the 2.x output showed it.
		detail := err.Error()
		var se *sqlcipher.Error
		if errors.As(err, &se) {
			detail = se.Msg
		}
		return nil, fmt.Errorf("Cannot decrypt or read schema for %s: %s; run: %s", rel, detail, s.keyRecovery)
	}
	return conn, nil
}

// readSource returns the file this command reads rel from: one copy-on-write
// clone per database per command, so connections share a snapshot, or the
// live file where cloning is unsupported.
//
// A long read transaction on the live file keeps the client's WAL
// checkpoints from completing, and the client was observed to drop incoming
// messages while back-to-back tg runs held one almost continuously. A clone
// moves that read off the live file after a lock held only for the clone.
func (s *Store) readSource(rel, live string, key *[32]byte, salt *[16]byte) (string, error) {
	s.snapshotMu.Lock()
	defer s.snapshotMu.Unlock()
	if src, ok := s.snapshotSource[rel]; ok {
		if src == "" {
			return live, nil
		}
		return src, nil
	}
	if !canClone {
		s.snapshotSource[rel] = ""
		return live, nil
	}
	// The pinned read keeps checkpoints from backfilling past this snapshot
	// or restarting the WAL, so the main file and WAL, cloned at two
	// instants, still replay to one committed state.
	pin, err := s.openEncrypted(live, key, salt, rel)
	if err != nil {
		return "", err
	}
	defer pin.Close()
	if s.snapshotDir == "" {
		dir, err := os.MkdirTemp("", "tg-snapshot-")
		if err != nil {
			return "", fmt.Errorf("Cannot create a snapshot directory: %w", err)
		}
		s.snapshotDir = dir
	}
	clone, err := cloneDatabase(live, s.snapshotDir, rel)
	if err != nil {
		return "", err
	}
	s.snapshotSource[rel] = clone
	if clone == "" {
		return live, nil
	}
	return clone, nil
}

// Close removes this command's snapshot clones.
func (s *Store) Close() error {
	s.snapshotMu.Lock()
	defer s.snapshotMu.Unlock()
	if s.snapshotDir == "" {
		return nil
	}
	err := os.RemoveAll(s.snapshotDir)
	s.snapshotDir = ""
	return err
}

// cloneDatabase clones the main file and its WAL into dir. It returns "" when
// the filesystem cannot clone; a full copy is not a substitute, as it would
// hold the live read for the copy's duration and write the whole database on
// every command.
func cloneDatabase(live, dir, rel string) (string, error) {
	target := filepath.Join(dir, rel)
	if err := os.MkdirAll(filepath.Dir(target), 0o700); err != nil {
		return "", fmt.Errorf("Cannot create a snapshot directory for %s: %w", rel, err)
	}
	switch err := cloneFile(live, target); {
	case errors.Is(err, errCloneUnsupported):
		return "", nil
	case err != nil:
		return "", fmt.Errorf("Cannot clone %s: %w", rel, err)
	}
	// No WAL: the database is not in WAL mode, so the main file is complete.
	if err := cloneFile(live+"-wal", target+"-wal"); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return "", fmt.Errorf("Cannot clone the WAL of %s: %w", rel, err)
	}
	return target, nil
}

var errCloneUnsupported = errors.New("copy-on-write clone unsupported")

func keySpec(key *[32]byte, salt *[16]byte) []byte {
	spec := make([]byte, 0, 99)
	spec = append(spec, "x'"...)
	spec = hex.AppendEncode(spec, key[:])
	spec = hex.AppendEncode(spec, salt[:])
	return append(spec, '\'')
}

// SourceDatabase is one cataloged *.db file. The catalog and key file live in
// internal/keys, which has no cgo dependency; these aliases keep store's API.
type SourceDatabase = keys.SourceDatabase

// CollectSourceDatabases lists every *.db file under root, sorted by
// relative path.
func CollectSourceDatabases(root string) ([]SourceDatabase, error) {
	return keys.CollectSourceDatabases(root)
}

// PageSize is the SQLCipher page size of every source database.
const PageSize = keys.PageSize

// ReadPage1 reads the first page of a database file.
func ReadPage1(path, label string) ([]byte, error) {
	return keys.ReadPage1(path, label)
}

// KeyFile is all_keys.json: relative path -> {"enc_key": hex, ...}.
type KeyFile = keys.KeyFile

// LoadKeyFile reads path. It returns nil, nil when the file does not exist,
// and rejects non-regular files and files readable by group or others.
func LoadKeyFile(path string) (KeyFile, error) {
	return keys.LoadKeyFile(path)
}

func decodeKeys(raw KeyFile) (map[string]*[32]byte, error) {
	out := make(map[string]*[32]byte, len(raw))
	for rel, fields := range raw {
		v, ok := fields["enc_key"]
		if !ok {
			continue
		}
		var key [32]byte
		if hex.DecodedLen(len(v)) != 32 {
			return nil, fmt.Errorf("Invalid 32-byte database key for %s", rel)
		}
		if _, err := hex.Decode(key[:], []byte(v)); err != nil {
			return nil, fmt.Errorf("Invalid 32-byte database key for %s", rel)
		}
		out[rel] = &key
	}
	return out, nil
}
