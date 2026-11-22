package msgdb

import (
	"cmp"
	"fmt"
	"slices"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/xiaotianxt/tg/internal/decode"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
	"github.com/xiaotianxt/tg/internal/store"
)

// Order is which end of a message range comes first. Paging counts from
// that end, so it also decides which messages a page holds.
type Order int

const (
	Newest Order = iota
	Oldest
)

func (o Order) String() string {
	if o == Oldest {
		return "oldest"
	}
	return "newest"
}

func (o Order) sql() string {
	if o == Oldest {
		return "ASC"
	}
	return "DESC"
}

// OrderKey is a row's display order within one table: (order column,
// rowid). The order column is sort_seq or create_time and is not unique, so
// the rowid completes the key. Walking the single-column order index
// backwards orders ties by descending rowid, so sorting rows read in rowid
// order by this key reproduces the index walk exactly. NULL sorts first.
type OrderKey struct {
	Seq    int64
	HasSeq bool
	RowID  int64
}

// Compare orders keys ascending.
func (k OrderKey) Compare(o OrderKey) int {
	if k.HasSeq != o.HasSeq {
		if k.HasSeq {
			return 1
		}
		return -1
	}
	if c := cmp.Compare(k.Seq, o.Seq); c != 0 {
		return c
	}
	return cmp.Compare(k.RowID, o.RowID)
}

// Row is one raw message row.
type Row struct {
	// LocalID is the local_id column; HasLocalID is false when the table has
	// no such column or the value is NULL. Callers that need an id for every
	// row (query) fall back to RowID.
	LocalID    int64
	HasLocalID bool
	RowID      int64
	// LocalType is the full stored type, -1 when NULL.
	LocalType  int64
	CreateTime int64
	// SenderRowID is the raw sender column (0 when NULL); Sender is its
	// Name2Id username, "" when unknown.
	SenderRowID int64
	Sender      string
	// Body is the message body, decompressed when the compression marker
	// says so; NULL and undecodable bodies are "".
	Body string
	// Packed is the packed metadata blob; nil when the table lacks it.
	Packed []byte
	Key    OrderKey
}

// Bound is an optional integer bound.
type Bound struct {
	At  int64
	Set bool
}

// At returns a set bound.
func At(v int64) Bound { return Bound{At: v, Set: true} }

// Clause is an extra WHERE condition with its bound arguments. SQL may use
// the quoted column variables of this package (BodyCol and so on).
type Clause struct {
	SQL  string
	Args []any
}

// ScanOptions selects rows of one table. Every row read has create_time > 0.
type ScanOptions struct {
	// Since is inclusive, Until exclusive, on create_time.
	Since, Until Bound
	// FilterSenders restricts rows to SenderRowIDs (Name2Id rowids of this
	// shard). An empty list matches nothing.
	FilterSenders bool
	SenderRowIDs  []int64
	// BodyLike are raw substrings matched against the stored body with
	// LIKE (wildcards escaped); all must match, or any with BodyLikeAny.
	// Compressed bodies are matched as stored.
	BodyLike    []string
	BodyLikeAny bool
	// Where are extra conditions, ANDed.
	Where []Clause
	// RowIDStart (inclusive) and RowIDEnd (exclusive) cut the table into
	// disjoint ranges for parallel whole-table scans.
	RowIDStart, RowIDEnd Bound
	// Order is the direction a Limit counts from and rows are sorted in.
	Order Order
	// Limit > 0 pushes `ORDER BY <order col> LIMIT n` down so the index walk
	// can stop early. Otherwise the table is read in rowid order, which reads
	// each page once, and callers sort afterwards (ReadTable does).
	Limit int
}

// ScanTable streams the selected rows of one table to yield, in index order
// with a Limit and rowid order without. label names the shard in errors.
func ScanTable(conn *sqlcipher.Conn, label string, t Table, name2id map[int64]string, o ScanOptions, yield func(Row) error) error {
	if o.FilterSenders && len(o.SenderRowIDs) == 0 {
		return nil
	}
	localID := "NULL"
	if t.Schema.HasLocalID {
		localID = "local_id"
	}
	packed := "NULL"
	if t.Schema.HasPackedMeta {
		packed = PackedCol
	}
	orderCol := t.Schema.OrderColumn()

	where := []string{"create_time > 0"}
	var args []any
	add := func(sql string, a ...any) {
		where = append(where, sql)
		args = append(args, a...)
	}
	if o.Since.Set {
		add("create_time >= ?", o.Since.At)
	}
	if o.Until.Set {
		add("create_time < ?", o.Until.At)
	}
	if o.RowIDStart.Set {
		add("rowid >= ?", o.RowIDStart.At)
	}
	if o.RowIDEnd.Set {
		add("rowid < ?", o.RowIDEnd.At)
	}
	if o.FilterSenders {
		a := make([]any, len(o.SenderRowIDs))
		for i, id := range o.SenderRowIDs {
			a[i] = id
		}
		add(SenderCol+" IN ("+placeholders(len(a))+")", a...)
	}
	if len(o.BodyLike) > 0 {
		parts := make([]string, len(o.BodyLike))
		a := make([]any, len(o.BodyLike))
		for i, s := range o.BodyLike {
			parts[i] = BodyCol + ` LIKE ? ESCAPE '\'`
			a[i] = LikeContains(s)
		}
		joiner := " AND "
		if o.BodyLikeAny {
			joiner = " OR "
		}
		add("("+strings.Join(parts, joiner)+")", a...)
	}
	for _, c := range o.Where {
		add("("+c.SQL+")", c.Args...)
	}
	order := ""
	if o.Limit > 0 {
		order = " ORDER BY " + orderCol + " " + o.Order.sql() + " LIMIT " + strconv.Itoa(o.Limit)
	}
	sql := "SELECT " + localID + ", local_type, create_time, " + BodyCol + ", " + MarkerCol + ", " +
		SenderCol + ", " + packed + ", " + orderCol + ", rowid FROM " + QuoteIdent(t.Name) +
		" WHERE " + strings.Join(where, " AND ") + order

	st, err := conn.Prepare(sql)
	if err != nil {
		return fmt.Errorf("Prepare messages from %s: %w", label, err)
	}
	defer st.Finalize()
	if err := st.Bind(args...); err != nil {
		return fmt.Errorf("Query messages from %s: %w", label, err)
	}
	var vals [rowColumns]sqlcipher.Value
	for {
		ok, err := st.Step()
		if err != nil {
			return fmt.Errorf("Read messages from %s: %w", label, err)
		}
		if !ok {
			return nil
		}
		row, err := readRow(st, &vals, name2id)
		if err != nil {
			return fmt.Errorf("Read messages from %s: %w", label, err)
		}
		if err := yield(row); err != nil {
			return err
		}
	}
}

// rowColumns is the column count of ScanTable's SELECT.
const rowColumns = 9

func readRow(st *sqlcipher.Stmt, vals *[rowColumns]sqlcipher.Value, name2id map[int64]string) (Row, error) {
	st.Row(vals[:])
	optInt := func(i int) (int64, bool, error) {
		switch v := &vals[i]; v.Type() {
		case sqlcipher.TypeNull:
			return 0, false, nil
		case sqlcipher.TypeInteger:
			return v.Int64(), true, nil
		default:
			return 0, false, fmt.Errorf("column %d (%s) is not an integer", i, st.ColumnName(i))
		}
	}
	var r Row
	var err error
	var ok bool
	if r.LocalID, r.HasLocalID, err = optInt(0); err != nil {
		return r, err
	}
	if r.LocalType, ok, err = optInt(1); err != nil {
		return r, err
	} else if !ok {
		r.LocalType = -1
	}
	if r.CreateTime, _, err = optInt(2); err != nil {
		return r, err
	}
	marker, hasMarker, err := optInt(4)
	if err != nil {
		return r, err
	}
	if r.Body, err = readBody(&vals[3], hasMarker && marker == compressedMarker); err != nil {
		return r, err
	}
	if r.SenderRowID, _, err = optInt(5); err != nil {
		return r, err
	}
	r.Sender = name2id[r.SenderRowID]
	if vals[6].Type() != sqlcipher.TypeNull {
		r.Packed = vals[6].Blob()
		if r.Packed == nil {
			r.Packed = []byte{}
		}
	}
	if r.Key.Seq, r.Key.HasSeq, err = optInt(7); err != nil {
		return r, err
	}
	r.Key.RowID = vals[8].Int64()
	r.RowID = r.Key.RowID
	return r, nil
}

// readBody reads the body column. A compressed body must be stored as bytes
// and decodes to "" when decompression fails; an uncompressed body is text,
// bytes that are valid UTF-8 (else ""), or NULL (""). Other types fail.
func readBody(v *sqlcipher.Value, compressed bool) (string, error) {
	t := v.Type()
	if compressed {
		if t != sqlcipher.TypeBlob && t != sqlcipher.TypeText {
			return "", fmt.Errorf("compressed message body has invalid column type")
		}
		s, _ := decode.Decompress(v.Blob())
		return s, nil
	}
	switch t {
	case sqlcipher.TypeNull:
		return "", nil
	case sqlcipher.TypeText:
		return v.Text(), nil
	case sqlcipher.TypeBlob:
		b := v.Text()
		if !utf8.ValidString(b) {
			return "", nil
		}
		return b, nil
	default:
		return "", fmt.Errorf("message body has invalid column type")
	}
}

// ReadTable collects ScanTable's rows sorted in o.Order by OrderKey.
func ReadTable(conn *sqlcipher.Conn, label string, t Table, name2id map[int64]string, o ScanOptions) ([]Row, error) {
	var rows []Row
	err := ScanTable(conn, label, t, name2id, o, func(r Row) error {
		rows = append(rows, r)
		return nil
	})
	if err != nil {
		return nil, err
	}
	SortRows(rows, o.Order)
	return rows, nil
}

// SortRows sorts rows of one table by OrderKey in order. Keys are unique.
func SortRows(rows []Row, order Order) {
	slices.SortFunc(rows, func(a, b Row) int {
		if order == Newest {
			return b.Key.Compare(a.Key)
		}
		return a.Key.Compare(b.Key)
	})
}

// SessionOptions selects one session's rows across every shard.
type SessionOptions struct {
	ScanOptions
	// Senders, when non-nil, restricts rows to these account usernames;
	// shards that know none of them contribute no rows. It replaces
	// ScanOptions.FilterSenders/SenderRowIDs, which are set per shard.
	Senders []string
	// HistoryStart also finds the session's earliest stored message time,
	// whatever the requested range.
	HistoryStart bool
}

// SessionRows is a session's rows from every shard merged by create_time in
// o.Order (stable, so ties keep shard then key order), not yet paged.
type SessionRows struct {
	Rows []Row
	// HistoryStart is the earliest stored message time when HasHistory.
	HistoryStart int64
	HasHistory   bool
}

// ReadSession reads one session's table in every shard. With a Limit, each
// shard returns up to Limit rows from the chosen end, so a caller paging
// with an offset passes offset+limit.
func ReadSession(st *store.Store, username string, jobs int, o SessionOptions) (SessionRows, error) {
	table := TableName(username)
	type shardRows struct {
		rows     []Row
		start    int64
		hasStart bool
	}
	per, err := EachShard(st, jobs, func(label string, conn *sqlcipher.Conn) (shardRows, error) {
		var out shardRows
		schema, ok, err := TableSchema(conn, table)
		if err != nil || !ok {
			return out, err
		}
		t := Table{Name: table, Schema: schema}
		if o.HistoryStart {
			if out.start, out.hasStart, err = HistoryStart(conn, label, t); err != nil {
				return out, err
			}
		}
		so := o.ScanOptions
		if o.Senders != nil {
			ids, err := SenderRowIDs(conn, label, o.Senders)
			if err != nil {
				return out, err
			}
			if len(ids) == 0 {
				return out, nil
			}
			so.FilterSenders, so.SenderRowIDs = true, ids
		}
		name2id, err := LoadName2ID(conn)
		if err != nil {
			return out, err
		}
		out.rows, err = ReadTable(conn, label, t, name2id, so)
		return out, err
	})
	if err != nil {
		return SessionRows{}, err
	}
	var res SessionRows
	for _, s := range per {
		res.Rows = append(res.Rows, s.rows...)
		if s.hasStart && (!res.HasHistory || s.start < res.HistoryStart) {
			res.HistoryStart, res.HasHistory = s.start, true
		}
	}
	slices.SortStableFunc(res.Rows, func(a, b Row) int {
		if o.Order == Newest {
			return cmp.Compare(b.CreateTime, a.CreateTime)
		}
		return cmp.Compare(a.CreateTime, b.CreateTime)
	})
	return res, nil
}

// HistoryStart is the create_time of the table's first message in display
// order, found on the order index.
func HistoryStart(conn *sqlcipher.Conn, label string, t Table) (int64, bool, error) {
	var v int64
	ok, err := conn.QueryRow("SELECT create_time FROM "+QuoteIdent(t.Name)+
		" WHERE create_time > 0 ORDER BY "+t.Schema.OrderColumn()+" LIMIT 1", nil,
		func(st *sqlcipher.Stmt) error {
			var err error
			v, _, err = OptInt(st, 0)
			return err
		})
	if err != nil {
		return 0, false, fmt.Errorf("Earliest message in %s: %w", label, err)
	}
	return v, ok, nil
}

// RowIDBounds returns the table's min and max rowid; ok is false for an
// empty table. Two subqueries, because SQLite answers a lone min() or max()
// of the rowid from the b-tree edge but scans the table for both together.
func RowIDBounds(conn *sqlcipher.Conn, label, table string) (lo, hi int64, ok bool, err error) {
	q := QuoteIdent(table)
	var hasLo, hasHi bool
	_, err = conn.QueryRow("SELECT (SELECT min(rowid) FROM "+q+"), (SELECT max(rowid) FROM "+q+")", nil,
		func(st *sqlcipher.Stmt) error {
			lo, hasLo = st.ColumnInt64(0), !st.ColumnIsNull(0)
			hi, hasHi = st.ColumnInt64(1), !st.ColumnIsNull(1)
			return nil
		})
	if err != nil {
		return 0, 0, false, fmt.Errorf("Message rowid bounds in %s: %w", label, err)
	}
	return lo, hi, hasLo && hasHi, nil
}

// CountRows counts the table's dated messages.
func CountRows(conn *sqlcipher.Conn, label, table string) (int64, error) {
	var n int64
	_, err := conn.QueryRow("SELECT COUNT(*) FROM "+QuoteIdent(table)+" WHERE create_time > 0", nil,
		func(st *sqlcipher.Stmt) error { n = st.ColumnInt64(0); return nil })
	if err != nil {
		return 0, fmt.Errorf("Count messages in %s: %w", label, err)
	}
	return max(n, 0), nil
}

// LatestCreateTime is the table's newest dated message time.
func LatestCreateTime(conn *sqlcipher.Conn, label, table string) (int64, bool, error) {
	var v int64
	var ok bool
	_, err := conn.QueryRow("SELECT MAX(create_time) FROM "+QuoteIdent(table)+" WHERE create_time > 0", nil,
		func(st *sqlcipher.Stmt) error {
			var err error
			v, ok, err = OptInt(st, 0)
			return err
		})
	if err != nil {
		return 0, false, fmt.Errorf("Read latest message in %s: %w", label, err)
	}
	return v, ok, nil
}

// ScanSenders streams (sender rowid, create_time) of every dated row with a
// non-zero sender, in rowid order: one pass that reads each table page once,
// for activity aggregates.
func ScanSenders(conn *sqlcipher.Conn, label, table string, fn func(sender, createTime int64)) error {
	sql := "SELECT " + SenderCol + ", create_time FROM " + QuoteIdent(table) +
		" NOT INDEXED WHERE create_time > 0 AND " + SenderCol + " IS NOT NULL AND " + SenderCol + " != 0"
	err := conn.Query(sql, nil, func(st *sqlcipher.Stmt) error {
		sender, _, err := OptInt(st, 0)
		if err != nil {
			return err
		}
		ct, _, err := OptInt(st, 1)
		if err != nil {
			return err
		}
		fn(sender, ct)
		return nil
	})
	if err != nil {
		return fmt.Errorf("Read member activity in %s: %w", label, err)
	}
	return nil
}
