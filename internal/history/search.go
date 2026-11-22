package history

import (
	"cmp"
	"fmt"
	"slices"
	"strconv"
	"strings"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/par"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
	"github.com/xiaotianxt/tg/internal/store"
)

// FTSDB is the upstream full-text index's path in the store.
const FTSDB = "message/message_fts.db"

// SearchOptions selects a global search.
type SearchOptions struct {
	Query string
	// Limit caps hits when Limited (the CLI always does; MCP may not).
	Limit   int
	Limited bool
	// Since is inclusive.
	Since msgdb.Bound
	Mode  contacts.NameMode
	Jobs  int
}

// SearchHit is one matching message.
type SearchHit struct {
	LocalType      int64
	CreateTime     int64
	Content        string
	Session        string
	SessionDisplay string
}

// SearchResults holds at most Limit hits, oldest first.
type SearchResults struct {
	Hits []SearchHit
	// HasMore is set when more matches exist, all older than the oldest hit.
	HasMore bool
}

// SearchStrategy is how the FTS index is queried.
type SearchStrategy int

const (
	// FTSMatch runs MATCH on the FTS tables with the compatible tokenizer,
	// then keeps hits whose text contains the query.
	FTSMatch SearchStrategy = iota
	// ContentLike runs a literal LIKE over the FTS shadow content tables.
	ContentLike
)

// InitialStrategy is FTSMatch for an all-ideograph query when the tokenizer
// registered, else ContentLike.
func InitialStrategy(query string, tokenizerOK bool) SearchStrategy {
	if _, ok := IdeographQuery(query); ok && tokenizerOK {
		return FTSMatch
	}
	return ContentLike
}

// fallback is the strategy after s failed; ok is false when none is left.
func (s SearchStrategy) fallback() (SearchStrategy, bool) {
	return ContentLike, s == FTSMatch
}

// IdeographQuery returns the trimmed query when it is non-empty and every
// rune is a CJK ideograph, which the compatible tokenizer indexes per rune.
func IdeographQuery(query string) (string, bool) {
	q := strings.TrimSpace(query)
	if q == "" {
		return "", false
	}
	for _, r := range q {
		if !isIdeograph(r) {
			return "", false
		}
	}
	return q, true
}

// ideographPhrase is the MATCH expression for an all-ideograph query: a
// phrase of one token per ideograph, as the upstream tokenizer indexes them.
// Spelling the tokens out keeps the match independent of whether the
// registered tokenizer splits ideographs itself.
func ideographPhrase(q string) string {
	runes := []rune(q)
	parts := make([]string, len(runes))
	for i, r := range runes {
		parts[i] = string(r)
	}
	return `"` + strings.Join(parts, " ") + `"`
}

func isIdeograph(r rune) bool {
	return (r >= 0x3400 && r <= 0x4dbf) || (r >= 0x4e00 && r <= 0x9fff) ||
		(r >= 0xf900 && r <= 0xfaff) || (r >= 0x20000 && r <= 0x2fa1f)
}

type ftsRow struct {
	localType, createTime int64
	content, session      string
}

// Search searches the upstream FTS index across all sessions.
func Search(st *store.Store, o SearchOptions) (SearchResults, error) {
	// Contacts and the index are separate databases; read them together.
	// Contact errors still take precedence.
	var book contacts.Book
	done := make(chan error, 1)
	go func() {
		var err error
		book, err = contacts.LoadAll(st)
		done <- err
	}()
	rows, ftsErr := searchFTS(st, o)
	if err := <-done; err != nil {
		return SearchResults{}, err
	}
	if ftsErr != nil {
		return SearchResults{}, ftsErr
	}

	slices.SortStableFunc(rows, func(a, b ftsRow) int { return cmp.Compare(b.createTime, a.createTime) })
	var res SearchResults
	if o.Limited && len(rows) > o.Limit {
		res.HasMore = true
		rows = rows[:o.Limit]
	}
	slices.SortStableFunc(rows, func(a, b ftsRow) int { return cmp.Compare(a.createTime, b.createTime) })
	for _, r := range rows {
		res.Hits = append(res.Hits, SearchHit{
			LocalType: r.localType, CreateTime: r.createTime, Content: r.content, Session: r.session,
			SessionDisplay: sessionDisplay(book, r.session, o.Mode),
		})
	}
	return res, nil
}

func sessionDisplay(book contacts.Book, session string, mode contacts.NameMode) string {
	if strings.HasPrefix(session, "Msg_") {
		for u, c := range book {
			if msgdb.TableName(u) == session {
				return c.PersonalName() + " (" + u + ")"
			}
		}
		return "(?)"
	}
	return book.Display(session, mode)
}

func openFTS(st *store.Store) (*sqlcipher.Conn, error) {
	conn, err := st.Open(FTSDB)
	if err != nil {
		return nil, fmt.Errorf("Message search index is unavailable: %w; run tg keys or tg doctor, or use tg query to search full history", err)
	}
	return conn, nil
}

func searchFTS(st *store.Store, o SearchOptions) ([]ftsRow, error) {
	conn, err := openFTS(st)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	tables, err := ftsContentTables(conn)
	if err != nil {
		return nil, err
	}
	if len(tables) == 0 {
		return nil, fmt.Errorf("Message search index has no supported FTS tables")
	}
	name2id, err := ftsName2ID(conn)
	if err != nil {
		return nil, err
	}
	strategy := InitialStrategy(o.Query, conn.RegisterCompatibleTokenizer() == nil)
	rows, err := searchWith(st, strategy, tables, name2id, o)
	if err == nil {
		return rows, nil
	}
	next, ok := strategy.fallback()
	if !ok {
		return nil, err
	}
	rows, ferr := searchWith(st, next, tables, name2id, o)
	if ferr != nil {
		return nil, fmt.Errorf("Search message index failed (%v); content fallback failed: %v", err, ferr)
	}
	return rows, nil
}

// searchWith scans each FTS table on its own connection; rows concatenate in
// table order so the caller's stable sort matches one sequential scan.
func searchWith(st *store.Store, s SearchStrategy, tables []string, name2id map[int64]string, o SearchOptions) ([]ftsRow, error) {
	type result struct {
		rows []ftsRow
		err  error
	}
	// The FTS tables total far less than the message shards; one worker per
	// table measured fastest.
	results := par.Map(tables, par.Jobs(o.Jobs, len(tables)), func(table string) result {
		conn, err := openFTS(st)
		if err != nil {
			return result{err: err}
		}
		defer conn.Close()
		if s == FTSMatch {
			if err := conn.RegisterCompatibleTokenizer(); err != nil {
				return result{err: err}
			}
		}
		rows, err := searchTable(conn, s, table, name2id, o)
		return result{rows, err}
	})
	var rows []ftsRow
	for _, r := range results {
		if r.err != nil {
			return nil, r.err
		}
		rows = append(rows, r.rows...)
	}
	if s == FTSMatch {
		rows = slices.DeleteFunc(rows, func(r ftsRow) bool { return !strings.Contains(r.content, o.Query) })
	}
	return rows, nil
}

func searchTable(conn *sqlcipher.Conn, s SearchStrategy, contentTable string, name2id map[int64]string, o SearchOptions) ([]ftsRow, error) {
	var table, columns, where, timeCol string
	var arg any
	if s == FTSMatch {
		q, _ := IdeographQuery(o.Query)
		q = ideographPhrase(q)
		base, ok := strings.CutSuffix(contentTable, "_content")
		if !ok {
			return nil, fmt.Errorf("Invalid FTS content table name: %s", contentTable)
		}
		table = msgdb.QuoteIdent(base)
		columns, where, timeCol, arg = "local_type, create_time, acontent, session_id", table+" MATCH ?", "create_time", q
	} else {
		table = msgdb.QuoteIdent(contentTable)
		columns, where, timeCol, arg = "c3, c6, c0, c4", `c0 LIKE ? ESCAPE '\'`, "c6", msgdb.LikeContains(o.Query)
	}
	args := []any{arg}
	if o.Since.Set {
		where += " AND " + timeCol + " >= ?"
		args = append(args, o.Since.At)
	}
	limit := ""
	if o.Limited {
		// One extra row tells whether more hits exist.
		limit = " LIMIT " + strconv.Itoa(o.Limit+1)
	}
	sql := "SELECT " + columns + " FROM " + table + " WHERE " + where + " ORDER BY " + timeCol + " DESC" + limit
	stmt, err := conn.Prepare(sql)
	if err != nil {
		return nil, fmt.Errorf("Prepare search of %s: %w", contentTable, err)
	}
	defer stmt.Finalize()
	if err := stmt.Bind(args...); err != nil {
		return nil, fmt.Errorf("Query search index table %s: %w", contentTable, err)
	}
	var rows []ftsRow
	for {
		ok, err := stmt.Step()
		if err != nil {
			return nil, fmt.Errorf("Read search index table %s: %w", contentTable, err)
		}
		if !ok {
			return rows, nil
		}
		r, err := readFTSRow(stmt, name2id)
		if err != nil {
			return nil, fmt.Errorf("Read search index table %s: %w", contentTable, err)
		}
		rows = append(rows, r)
	}
}

func readFTSRow(st *sqlcipher.Stmt, name2id map[int64]string) (ftsRow, error) {
	var r ftsRow
	var ok bool
	var err error
	if r.localType, ok, err = msgdb.OptInt(st, 0); err != nil {
		return r, err
	} else if !ok {
		r.localType = -1
	}
	if r.createTime, _, err = msgdb.OptInt(st, 1); err != nil {
		return r, err
	}
	if r.content, err = msgdb.Text(st, 2); err != nil {
		return r, err
	}
	sid, _, err := msgdb.OptInt(st, 3)
	if err != nil {
		return r, err
	}
	if name, ok := name2id[sid]; ok {
		r.session = name
	} else {
		r.session = strconv.FormatInt(sid, 10)
	}
	return r, nil
}

func ftsContentTables(conn *sqlcipher.Conn) ([]string, error) {
	var out []string
	err := conn.Query(`SELECT name FROM sqlite_master
		WHERE type='table' AND name LIKE 'message_fts_v4_%_content' ORDER BY name`, nil, func(st *sqlcipher.Stmt) error {
		name := st.ColumnText(0)
		mid, ok := strings.CutPrefix(name, "message_fts_v4_")
		if mid, ok2 := strings.CutSuffix(mid, "_content"); ok && ok2 && allDigits(mid) {
			out = append(out, name)
		}
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("Inspect message search index tables: %w", err)
	}
	return out, nil
}

func allDigits(s string) bool {
	for _, c := range s {
		if c < '0' || c > '9' {
			return false
		}
	}
	return true
}

func ftsName2ID(conn *sqlcipher.Conn) (map[int64]string, error) {
	out := map[int64]string{}
	err := conn.Query("SELECT rowid, username FROM name2id", nil, func(st *sqlcipher.Stmt) error {
		if st.ColumnType(1) != sqlcipher.TypeText {
			return fmt.Errorf("session name is not text")
		}
		out[st.ColumnInt64(0)] = st.ColumnText(1)
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("Read message search session map: %w", err)
	}
	return out, nil
}
