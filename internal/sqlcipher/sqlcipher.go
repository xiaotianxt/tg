// Package sqlcipher is a minimal cgo binding to the vendored SQLCipher
// amalgamation. It exposes only what tg needs: open a keyed database, run
// prepared statements, and alias the upstream FTS tokenizer. A Conn is not
// safe for concurrent use; open one per goroutine.
package sqlcipher

/*
#cgo CFLAGS: -DSQLITE_HAS_CODEC -DSQLITE_TEMP_STORE=2 -DSQLITE_THREADSAFE=2
#cgo CFLAGS: -DSQLITE_ENABLE_FTS5 -DSQLITE_DEFAULT_MEMSTATUS=0 -DSQLITE_OMIT_LOAD_EXTENSION
#cgo CFLAGS: -DSQLITE_DQS=0 -DHAVE_USLEEP=1 -DHAVE_STDINT_H=1
#cgo CFLAGS: -DSQLITE_EXTRA_INIT=sqlcipher_extra_init -DSQLITE_EXTRA_SHUTDOWN=sqlcipher_extra_shutdown
#cgo CFLAGS: -O2 -Wno-deprecated-declarations
#cgo darwin CFLAGS: -DSQLCIPHER_CRYPTO_CC
#cgo darwin LDFLAGS: -framework Security -framework CoreFoundation
#cgo linux CFLAGS: -DSQLCIPHER_CRYPTO_OPENSSL
#cgo linux LDFLAGS: -Wl,-Bstatic -lcrypto -Wl,-Bdynamic -ldl -lpthread -lm

#include <stdlib.h>
#include "sqlite3.h"

int tg_register_tokenizer(sqlite3 *db);

static int tg_bind_text(sqlite3_stmt *s, int i, _GoString_ v) {
	// An empty Go string has a nil data pointer, which SQLite binds as NULL.
	const char *p = _GoStringLen(v) > 0 ? _GoStringPtr(v) : "";
	return sqlite3_bind_text(s, i, p, (int)_GoStringLen(v), SQLITE_TRANSIENT);
}
typedef struct {
	int type;
	int n;
	sqlite3_int64 i;
	const void *p;
} tg_value;

// tg_row reads n columns of the current row, so a row costs one cgo call
// instead of two or three per column.
static void tg_row(sqlite3_stmt *s, tg_value *out, int n) {
	for (int c = 0; c < n; c++) {
		tg_value *v = &out[c];
		v->type = sqlite3_column_type(s, c);
		v->n = 0;
		v->i = 0;
		v->p = 0;
		switch (v->type) {
		case SQLITE_INTEGER:
			v->i = sqlite3_column_int64(s, c);
			break;
		case SQLITE_TEXT:
			v->p = sqlite3_column_text(s, c);
			v->n = sqlite3_column_bytes(s, c);
			break;
		case SQLITE_BLOB:
			v->p = sqlite3_column_blob(s, c);
			v->n = sqlite3_column_bytes(s, c);
			break;
		}
	}
}
static int tg_bind_blob(sqlite3_stmt *s, int i, const void *p, int n) {
	return sqlite3_bind_blob(s, i, p, n, SQLITE_TRANSIENT);
}
*/
import "C"

import (
	"errors"
	"fmt"
	"unsafe"
)

// Error is a SQLite result code with the connection's message.
type Error struct {
	Code int
	// Op is the failed operation ("" when Msg already names it); Msg is
	// SQLite's message, which user-facing errors show alone.
	Op  string
	Msg string
}

func (e *Error) Error() string {
	if e.Op != "" {
		return fmt.Sprintf("%s: %s (SQLite code %d)", e.Op, e.Msg, e.Code)
	}
	return fmt.Sprintf("%s (SQLite code %d)", e.Msg, e.Code)
}

// Result codes callers branch on.
const (
	codeOK      = 0
	codeRow     = 100
	codeDone    = 101
	CodeNotADB  = 26
	CodeCorrupt = 11
)

// IsCode reports whether err is a SQLite error with the given primary code.
func IsCode(err error, code int) bool {
	var e *Error
	return errors.As(err, &e) && e.Code&0xff == code
}

// Conn is one open SQLite connection.
type Conn struct {
	db *C.sqlite3
}

// OpenOptions selects how a database is opened.
type OpenOptions struct {
	// ReadOnly opens with SQLITE_OPEN_READONLY and sets query_only.
	ReadOnly bool
	// Key is a SQLCipher key spec such as x'<64 hex key><32 hex salt>'. Empty
	// opens a plaintext database.
	Key []byte
}

// Open opens path. With a key it applies SQLCipher 4 defaults before any
// schema read, so a wrong key surfaces on the first query as CodeNotADB.
func Open(path string, opts OpenOptions) (*Conn, error) {
	flags := C.int(C.SQLITE_OPEN_NOMUTEX | C.SQLITE_OPEN_URI)
	if opts.ReadOnly {
		flags |= C.SQLITE_OPEN_READONLY
	} else {
		flags |= C.SQLITE_OPEN_READWRITE | C.SQLITE_OPEN_CREATE
	}
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))
	var db *C.sqlite3
	if rc := C.sqlite3_open_v2(cpath, &db, flags, nil); rc != codeOK {
		err := connError(db, rc, "open "+path)
		C.sqlite3_close_v2(db)
		return nil, err
	}
	c := &Conn{db: db}
	if len(opts.Key) > 0 {
		rc := C.sqlite3_key(db, unsafe.Pointer(&opts.Key[0]), C.int(len(opts.Key)))
		if rc != codeOK {
			c.Close()
			return nil, &Error{Code: int(rc), Msg: "configure database key"}
		}
		if err := c.Exec("PRAGMA cipher_compatibility = 4"); err != nil {
			c.Close()
			return nil, err
		}
	}
	pragmas := "PRAGMA temp_store = MEMORY"
	if opts.ReadOnly {
		pragmas += "; PRAGMA query_only = true"
	}
	if err := c.Exec(pragmas); err != nil {
		c.Close()
		return nil, err
	}
	return c, nil
}

// Close releases the connection. Unfinalized statements keep it alive until
// they are finalized.
func (c *Conn) Close() error {
	if c.db == nil {
		return nil
	}
	rc := C.sqlite3_close_v2(c.db)
	c.db = nil
	if rc != codeOK {
		return &Error{Code: int(rc), Msg: "close database"}
	}
	return nil
}

// Interrupt aborts the statement running on c from another goroutine.
func (c *Conn) Interrupt() { C.sqlite3_interrupt(c.db) }

// Exec runs one or more statements that return no rows.
func (c *Conn) Exec(sql string) error {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	if rc := C.sqlite3_exec(c.db, csql, nil, nil, nil); rc != codeOK {
		return connError(c.db, rc, "exec")
	}
	return nil
}

// RegisterCompatibleTokenizer registers the upstream FTS tokenizer name as an
// alias of unicode61 so FTS5 MATCH works on upstream tables.
func (c *Conn) RegisterCompatibleTokenizer() error {
	if rc := C.tg_register_tokenizer(c.db); rc != codeOK {
		return connError(c.db, rc, "register compatible FTS tokenizer")
	}
	return nil
}

// Stmt is a prepared statement.
type Stmt struct {
	conn *Conn
	s    *C.sqlite3_stmt
}

// Prepare compiles one statement.
func (c *Conn) Prepare(sql string) (*Stmt, error) {
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	var s *C.sqlite3_stmt
	if rc := C.sqlite3_prepare_v2(c.db, csql, -1, &s, nil); rc != codeOK {
		return nil, connError(c.db, rc, "prepare")
	}
	return &Stmt{conn: c, s: s}, nil
}

// Bind binds positional args (1-based in SQL). Supported: nil, int, int64,
// string, []byte, bool.
func (st *Stmt) Bind(args ...any) error {
	for i, arg := range args {
		idx := C.int(i + 1)
		var rc C.int
		switch v := arg.(type) {
		case nil:
			rc = C.sqlite3_bind_null(st.s, idx)
		case int:
			rc = C.sqlite3_bind_int64(st.s, idx, C.sqlite3_int64(v))
		case int64:
			rc = C.sqlite3_bind_int64(st.s, idx, C.sqlite3_int64(v))
		case bool:
			b := 0
			if v {
				b = 1
			}
			rc = C.sqlite3_bind_int64(st.s, idx, C.sqlite3_int64(b))
		case string:
			rc = C.tg_bind_text(st.s, idx, v)
		case []byte:
			var p unsafe.Pointer
			if len(v) > 0 {
				p = unsafe.Pointer(&v[0])
			}
			rc = C.tg_bind_blob(st.s, idx, p, C.int(len(v)))
		default:
			panic(fmt.Sprintf("sqlcipher: unsupported bind type %T", arg))
		}
		if rc != codeOK {
			return connError(st.conn.db, rc, "bind")
		}
	}
	return nil
}

// Step advances to the next row; it returns false when the statement is done.
func (st *Stmt) Step() (bool, error) {
	switch rc := C.sqlite3_step(st.s); rc {
	case codeRow:
		return true, nil
	case codeDone:
		return false, nil
	default:
		return false, connError(st.conn.db, rc, "step")
	}
}

// Reset rewinds the statement and clears its bindings for reuse.
func (st *Stmt) Reset() {
	C.sqlite3_reset(st.s)
	C.sqlite3_clear_bindings(st.s)
}

// Finalize releases the statement.
func (st *Stmt) Finalize() {
	C.sqlite3_finalize(st.s)
	st.s = nil
}

// Column types returned by ColumnType.
const (
	TypeInteger = 1
	TypeFloat   = 2
	TypeText    = 3
	TypeBlob    = 4
	TypeNull    = 5
)

func (st *Stmt) ColumnCount() int          { return int(C.sqlite3_column_count(st.s)) }
func (st *Stmt) ColumnType(i int) int      { return int(C.sqlite3_column_type(st.s, C.int(i))) }
func (st *Stmt) ColumnInt64(i int) int64   { return int64(C.sqlite3_column_int64(st.s, C.int(i))) }
func (st *Stmt) ColumnIsNull(i int) bool   { return st.ColumnType(i) == TypeNull }
func (st *Stmt) ColumnName(i int) string   { return C.GoString(C.sqlite3_column_name(st.s, C.int(i))) }
func (st *Stmt) ColumnFloat(i int) float64 { return float64(C.sqlite3_column_double(st.s, C.int(i))) }

// ColumnText returns the column as text (SQLite converts other types).
func (st *Stmt) ColumnText(i int) string {
	p := C.sqlite3_column_text(st.s, C.int(i))
	n := C.sqlite3_column_bytes(st.s, C.int(i))
	if p == nil || n == 0 {
		return ""
	}
	return C.GoStringN((*C.char)(unsafe.Pointer(p)), n)
}

// ColumnBlob returns a copy of the column bytes, nil for NULL.
func (st *Stmt) ColumnBlob(i int) []byte {
	p := C.sqlite3_column_blob(st.s, C.int(i))
	n := C.sqlite3_column_bytes(st.s, C.int(i))
	if p == nil {
		return nil
	}
	return C.GoBytes(p, n)
}

// Value is one column of the current row, filled by Stmt.Row. Text and
// blob values point into SQLite memory valid until the next Step, Reset or
// Finalize; Text and Blob copy it out.
type Value struct{ v C.tg_value }

// Type is the column's storage class (TypeInteger, TypeText, ...).
func (v *Value) Type() int { return int(v.v._type) }

// Int64 is the value of an integer column, 0 otherwise.
func (v *Value) Int64() int64 { return int64(v.v.i) }

// Text copies a text or blob column as a string, "" otherwise.
func (v *Value) Text() string {
	if v.v.n == 0 {
		return ""
	}
	return C.GoStringN((*C.char)(v.v.p), v.v.n)
}

// Blob copies a text or blob column; it is nil only for other types.
func (v *Value) Blob() []byte {
	if v.v.p == nil {
		if v.Type() == TypeBlob || v.Type() == TypeText {
			return []byte{}
		}
		return nil
	}
	return C.GoBytes(v.v.p, v.v.n)
}

// Row fills dst with the first len(dst) columns of the current row in one
// cgo call. Floats read as TypeFloat with no value.
func (st *Stmt) Row(dst []Value) {
	if len(dst) == 0 {
		return
	}
	C.tg_row(st.s, &dst[0].v, C.int(len(dst)))
}

// Query prepares sql, binds args, and calls fn for each row.
func (c *Conn) Query(sql string, args []any, fn func(*Stmt) error) error {
	st, err := c.Prepare(sql)
	if err != nil {
		return err
	}
	defer st.Finalize()
	if err := st.Bind(args...); err != nil {
		return err
	}
	for {
		ok, err := st.Step()
		if err != nil || !ok {
			return err
		}
		if err := fn(st); err != nil {
			return err
		}
	}
}

// QueryRow runs sql and calls fn for the first row. It reports whether a row
// existed.
func (c *Conn) QueryRow(sql string, args []any, fn func(*Stmt) error) (bool, error) {
	st, err := c.Prepare(sql)
	if err != nil {
		return false, err
	}
	defer st.Finalize()
	if err := st.Bind(args...); err != nil {
		return false, err
	}
	ok, err := st.Step()
	if err != nil || !ok {
		return false, err
	}
	return true, fn(st)
}

// TableExists reports whether a table or virtual table named name exists.
func (c *Conn) TableExists(name string) (bool, error) {
	return c.QueryRow("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", []any{name}, func(*Stmt) error { return nil })
}

func connError(db *C.sqlite3, rc C.int, op string) error {
	msg := C.GoString(C.sqlite3_errstr(rc))
	if db != nil {
		msg = C.GoString(C.sqlite3_errmsg(db))
	}
	return &Error{Code: int(rc), Op: op, Msg: msg}
}
