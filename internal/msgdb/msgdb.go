// Package msgdb reads the message shards (chat and official-account; see
// store.ShardKind): table naming, schema probes, Name2Id, shard iteration,
// and the single raw-row reader that messages, query and export build on.
// It returns raw rows; names and decoding belong to contacts, resolve and
// decode.
package msgdb

import (
	"crypto/md5"
	"encoding/hex"
	"fmt"
	"strings"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/par"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
	"github.com/xiaotianxt/tg/internal/store"
)

// TableName is the message table of a session: "Msg_" + md5(username) hex.
func TableName(username string) string {
	sum := md5.Sum([]byte(username))
	return "Msg_" + hex.EncodeToString(sum[:])
}

// QuoteIdent quotes an SQL identifier, doubling embedded quotes.
func QuoteIdent(name string) string {
	return `"` + strings.ReplaceAll(name, `"`, `""`) + `"`
}

// Quoted column names of a message table.
var (
	BodyCol   = QuoteIdent(dict.MsgBodyColumn)
	MarkerCol = QuoteIdent(dict.MsgCompressionMarkerColumn)
	SenderCol = QuoteIdent(dict.MsgSenderColumn)
	PackedCol = QuoteIdent(dict.MsgPackedMetaColumn)
)

// compressedMarker is the compression marker value of a compressed body.
const compressedMarker = 4

// Schema records the optional columns of one message table that change how
// it is read.
type Schema struct {
	HasPackedMeta bool
	HasLocalID    bool
	HasSortSeq    bool
}

// OrderColumn is the column display order follows: sort_seq when present,
// else create_time.
func (s Schema) OrderColumn() string {
	if s.HasSortSeq {
		return "sort_seq"
	}
	return "create_time"
}

// Table is a message table with its schema.
type Table struct {
	Name   string
	Schema Schema
}

// TableSchemas lists every Msg_* table with its schema in schema-scan order,
// from one sqlite_schema × pragma_table_info scan.
func TableSchemas(conn *sqlcipher.Conn) ([]Table, error) {
	return collectSchemas(conn, "m.name LIKE 'Msg_%'", nil)
}

// TableSchema returns the schema of one table; ok is false when the table
// is absent.
func TableSchema(conn *sqlcipher.Conn, table string) (s Schema, ok bool, err error) {
	tables, err := collectSchemas(conn, "m.name = ?1", []any{table})
	if err != nil || len(tables) == 0 {
		return Schema{}, false, err
	}
	return tables[len(tables)-1].Schema, true, nil
}

func collectSchemas(conn *sqlcipher.Conn, filter string, args []any) ([]Table, error) {
	// sqlite_schema is the outer loop, so tables keep their schema-scan order
	// and each table's columns arrive contiguously.
	sql := "SELECT m.name, p.name FROM sqlite_schema AS m, pragma_table_info(m.name) AS p " +
		"WHERE m.type = 'table' AND " + filter
	var tables []Table
	err := conn.Query(sql, args, func(st *sqlcipher.Stmt) error {
		table, column := st.ColumnText(0), st.ColumnText(1)
		if len(tables) == 0 || tables[len(tables)-1].Name != table {
			tables = append(tables, Table{Name: table})
		}
		s := &tables[len(tables)-1].Schema
		switch column {
		case "local_id":
			s.HasLocalID = true
		case "sort_seq":
			s.HasSortSeq = true
		case dict.MsgPackedMetaColumn:
			s.HasPackedMeta = true
		}
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("Inspect message table columns: %w", err)
	}
	return tables, nil
}

// ListTables lists Msg_* table names.
func ListTables(conn *sqlcipher.Conn) ([]string, error) {
	var out []string
	err := conn.Query("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'", nil,
		func(st *sqlcipher.Stmt) error { out = append(out, st.ColumnText(0)); return nil })
	if err != nil {
		return nil, fmt.Errorf("Cannot list message tables: %w", err)
	}
	return out, nil
}

// LoadName2ID maps Name2Id rowids to usernames; empty when the shard has no
// Name2Id table. A non-text username fails the read instead of dropping it.
func LoadName2ID(conn *sqlcipher.Conn) (map[int64]string, error) {
	out := map[int64]string{}
	ok, err := conn.TableExists("Name2Id")
	if err != nil {
		return nil, fmt.Errorf("Check table Name2Id: %w", err)
	}
	if !ok {
		return out, nil
	}
	err = conn.Query("SELECT rowid, user_name FROM Name2Id", nil, func(st *sqlcipher.Stmt) error {
		name, err := Text(st, 1)
		if err != nil {
			return err
		}
		out[st.ColumnInt64(0)] = name
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("Read Name2Id: %w", err)
	}
	return out, nil
}

// SenderRowIDs returns the Name2Id rowids of usernames in one shard; empty
// when none of them has a message there.
func SenderRowIDs(conn *sqlcipher.Conn, label string, usernames []string) ([]int64, error) {
	args := make([]any, len(usernames))
	for i, u := range usernames {
		args[i] = u
	}
	sql := "SELECT rowid FROM Name2Id WHERE user_name IN (" + placeholders(len(usernames)) + ")"
	var out []int64
	err := conn.Query(sql, args, func(st *sqlcipher.Stmt) error {
		out = append(out, st.ColumnInt64(0))
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("Sender lookup in %s: %w", label, err)
	}
	return out, nil
}

func placeholders(n int) string {
	return strings.TrimSuffix(strings.Repeat("?, ", n), ", ")
}

// ShardJobs is the worker count for per-shard reads.
func ShardJobs(jobs int) int { return par.Jobs(jobs, par.SourceDBAutoCap) }

// EachShard opens every chat shard on up to ShardJobs(jobs) goroutines,
// calls f with its own connection, and returns results in shard order. The
// first error in shard order wins, as a sequential read would report it.
func EachShard[T any](st *store.Store, jobs int, f func(label string, conn *sqlcipher.Conn) (T, error)) ([]T, error) {
	return EachDB(st, st.MessageShards(), jobs, f)
}

// EachDB is EachShard over an explicit list of databases.
func EachDB[T any](st *store.Store, labels []string, jobs int, f func(label string, conn *sqlcipher.Conn) (T, error)) ([]T, error) {
	type result struct {
		v   T
		err error
	}
	results := par.Map(labels, ShardJobs(jobs), func(label string) result {
		conn, err := st.Open(label)
		if err != nil {
			return result{err: err}
		}
		defer conn.Close()
		v, err := f(label, conn)
		return result{v, err}
	})
	out := make([]T, len(results))
	for i, r := range results {
		if r.err != nil {
			return nil, r.err
		}
		out[i] = r.v
	}
	return out, nil
}

// Text reads a nullable text column: NULL is "", a non-text value is an
// error, so malformed rows fail closed instead of yielding partial data.
func Text(st *sqlcipher.Stmt, i int) (string, error) {
	switch st.ColumnType(i) {
	case sqlcipher.TypeNull:
		return "", nil
	case sqlcipher.TypeText:
		return st.ColumnText(i), nil
	default:
		return "", fmt.Errorf("column %d (%s) is not text", i, st.ColumnName(i))
	}
}

// OptInt reads a nullable integer column; a non-integer value is an error.
func OptInt(st *sqlcipher.Stmt, i int) (v int64, ok bool, err error) {
	switch st.ColumnType(i) {
	case sqlcipher.TypeNull:
		return 0, false, nil
	case sqlcipher.TypeInteger:
		return st.ColumnInt64(i), true, nil
	default:
		return 0, false, fmt.Errorf("column %d (%s) is not an integer", i, st.ColumnName(i))
	}
}

// LikeContains is a LIKE pattern matching s anywhere, with %, _ and \
// escaped for `ESCAPE '\'`.
func LikeContains(s string) string {
	var b strings.Builder
	b.Grow(len(s) + 2)
	b.WriteByte('%')
	for _, r := range s {
		if r == '%' || r == '_' || r == '\\' {
			b.WriteByte('\\')
		}
		b.WriteRune(r)
	}
	b.WriteByte('%')
	return b.String()
}
