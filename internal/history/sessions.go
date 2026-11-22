// Package history answers the read commands: session lists, a session's
// messages, full-text search, group members and member friends, and the
// chat probe doctor uses. It returns typed results and never formats text.
package history

import (
	"cmp"
	"fmt"
	"slices"
	"strings"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/resolve"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
	"github.com/xiaotianxt/tg/internal/store"
)

// SessionListKind selects which sessions a list holds.
type SessionListKind int

const (
	AllSessions SessionListKind = iota
	UnreadSessions
)

func (k SessionListKind) where() string {
	if k == UnreadSessions {
		return `WHERE COALESCE(unread_count, 0) > 0
		   AND (COALESCE(last_timestamp, 0) > 0 OR COALESCE(sort_timestamp, 0) > 0)`
	}
	return `WHERE COALESCE(last_timestamp, 0) > 0 OR COALESCE(sort_timestamp, 0) > 0`
}

// SessionSummary is one listed session.
type SessionSummary struct {
	Username    string
	DisplayName string
	UnreadCount int64
	// LastActive is the later of the last and sort timestamps (Unix s).
	LastActive int64
}

// SessionList is one page of sessions. Sessions holds at most the requested
// top N; Matched and Total count before that cut.
type SessionList struct {
	Kind     SessionListKind
	Filtered bool
	Matched  int
	Total    int
	Sessions []SessionSummary
}

type sessionEntry struct {
	SessionSummary
	match resolve.FieldScore
}

// ListSessions lists sessions from session.db and contact.db only; it never
// opens message shards. A non-blank query filters and ranks by match.
func ListSessions(st *store.Store, kind SessionListKind, top int, query string) (SessionList, error) {
	return ListSessionsAs(st, kind, top, query, contacts.PersonalRemark)
}

// ListSessionsAs is ListSessions showing names in mode. Matching and
// ranking always use every name field, so a query finds the same sessions
// in every mode; only the shown names change. Added for MCP remote mode,
// which hides contact remarks.
func ListSessionsAs(st *store.Store, kind SessionListKind, top int, query string, mode contacts.NameMode) (SessionList, error) {
	conn, err := st.Open(resolve.SessionDB)
	if err != nil {
		return SessionList{}, fmt.Errorf("Session database is unavailable: %w", err)
	}
	defer conn.Close()

	var entries []sessionEntry
	err = conn.Query(`SELECT username, COALESCE(unread_count, 0), COALESCE(last_timestamp, 0),
		COALESCE(sort_timestamp, 0) FROM SessionTable `+kind.where(), nil, func(s *sqlcipher.Stmt) error {
		if s.ColumnType(0) != sqlcipher.TypeText {
			return fmt.Errorf("session username is not text")
		}
		e := sessionEntry{}
		e.Username = s.ColumnText(0)
		e.UnreadCount = max(s.ColumnInt64(1), 0)
		e.LastActive = max(s.ColumnInt64(2), s.ColumnInt64(3))
		if strings.TrimSpace(e.Username) != "" && e.LastActive > 0 {
			entries = append(entries, e)
		}
		return nil
	})
	if err != nil {
		return SessionList{}, fmt.Errorf("Read sessions: %w", err)
	}

	// Names and matching look up only these sessions' own contact rows.
	wanted := make(map[string]bool, len(entries))
	for _, e := range entries {
		wanted[e.Username] = true
	}
	book, err := contacts.LoadWhere(st, func(u string) bool { return wanted[u] })
	if err != nil {
		return SessionList{}, err
	}
	for i := range entries {
		entries[i].DisplayName = book.Display(entries[i].Username, contacts.PersonalRemark)
	}

	list := SessionList{Kind: kind, Total: len(entries)}
	query = strings.TrimSpace(query)
	if query != "" {
		list.Filtered = true
		scorer, ok := resolve.NewScorer(query)
		kept := entries[:0]
		for _, e := range entries {
			if !ok {
				break
			}
			fields := []resolve.Field{{Priority: 0, Text: e.DisplayName}, {Priority: 4, Text: e.Username}}
			if c, found := book[e.Username]; found {
				fields = resolve.MatchFields(c)
			}
			if m, found := scorer.Best(fields); found {
				e.match = m
				kept = append(kept, e)
			}
		}
		entries = kept
		slices.SortFunc(entries, func(a, b sessionEntry) int {
			if c := cmp.Compare(a.match.Priority, b.match.Priority); c != 0 {
				return c
			}
			if c := cmp.Compare(b.match.Score, a.match.Score); c != 0 {
				return c
			}
			if c := cmp.Compare(b.LastActive, a.LastActive); c != 0 {
				return c
			}
			if c := cmp.Compare(a.match.Distance, b.match.Distance); c != 0 {
				return c
			}
			if c := cmp.Compare(a.match.FieldLen, b.match.FieldLen); c != 0 {
				return c
			}
			if c := strings.Compare(a.DisplayName, b.DisplayName); c != 0 {
				return c
			}
			return strings.Compare(a.Username, b.Username)
		})
	} else {
		slices.SortStableFunc(entries, func(a, b sessionEntry) int { return cmp.Compare(b.LastActive, a.LastActive) })
	}
	list.Matched = len(entries)
	for _, e := range entries[:min(top, len(entries))] {
		if mode != contacts.PersonalRemark {
			e.DisplayName = book.Display(e.Username, mode)
		}
		list.Sessions = append(list.Sessions, e.SessionSummary)
	}
	return list, nil
}

// SessionProbe is doctor's view of one chat.
type SessionProbe struct {
	Username    string
	DisplayName string
	// PublicName is the chat's name without the owner's contact remark,
	// for callers that must not reveal remarks (remote MCP clients).
	PublicName string
	TableName  string
	// MatchingDBs counts shards holding the table; MessageCount its dated rows.
	MatchingDBs  int
	MessageCount int64
}

// ProbeSession resolves query and counts the session's stored messages.
func ProbeSession(st *store.Store, query string, o resolve.Options) (SessionProbe, error) {
	ref, err := resolve.SessionFromStore(st, query, o)
	if err != nil {
		return SessionProbe{}, err
	}
	book, err := contacts.LoadWhere(st, func(u string) bool { return u == ref.Username })
	if err != nil {
		return SessionProbe{}, err
	}
	table := msgdb.TableName(ref.Username)
	type count struct {
		n     int64
		found bool
	}
	per, err := msgdb.EachDB(st, st.SessionShards(), o.Jobs, func(label string, conn *sqlcipher.Conn) (count, error) {
		ok, err := conn.TableExists(table)
		if err != nil || !ok {
			return count{}, err
		}
		n, err := msgdb.CountRows(conn, label, table)
		return count{n, true}, err
	})
	if err != nil {
		return SessionProbe{}, err
	}
	p := SessionProbe{
		Username:    ref.Username,
		DisplayName: book.Display(ref.Username, contacts.PersonalRemark),
		PublicName:  book.Display(ref.Username, contacts.Anonymous),
		TableName:   table,
	}
	for _, c := range per {
		if c.found {
			p.MatchingDBs++
			p.MessageCount += c.n
		}
	}
	return p, nil
}
