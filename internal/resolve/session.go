// Package resolve turns user input into identities: a session query into a
// SessionRef, a sender value into a SenderFilter, and a raw row's sender
// into the name shown for it.
package resolve

import (
	"cmp"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/logx"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
	"github.com/xiaotianxt/tg/internal/store"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// SessionDB is the session database's path in the store.
const SessionDB = "session/session.db"

// SessionRef is a resolved session username.
type SessionRef struct {
	Username string
}

// IsGroup reports whether the session is a group chat.
func (s SessionRef) IsGroup() bool { return strings.HasSuffix(s.Username, "@chatroom") }

// DirectID returns the trimmed query when it names a session directly and
// resolves without contacts: empty, an account id, a gh_ id, or a group id.
func DirectID(query string) (string, bool) {
	q := strings.TrimSpace(query)
	ok := q == "" || strings.HasPrefix(q, dict.AccountIDPrefix) || strings.HasPrefix(q, "gh_") ||
		strings.Contains(q, "@chatroom")
	return q, ok
}

// Options tunes name resolution.
type Options struct {
	// Activity ranks candidates by latest activity (session.db, else the
	// message shards). Contact-only commands leave it off.
	Activity bool
	Jobs     int
	// Loc formats times in the match log.
	Loc *time.Location
}

// Session resolves query against book. A direct id resolves as is; otherwise
// the best-scoring contact wins, and a query matching no contact is taken as
// a raw username so sessions without a contact row stay readable.
func Session(st *store.Store, query string, book contacts.Book, o Options) (SessionRef, error) {
	if id, ok := DirectID(query); ok {
		return SessionRef{id}, nil
	}
	query = strings.TrimSpace(query)
	if c, ok := book[query]; ok {
		return SessionRef{c.Username}, nil
	}
	scored := scoreContacts(book, query)
	if u, ok := uniqueExactDirect(scored); ok {
		return SessionRef{u}, nil
	}
	cands, err := rankCandidates(st, scored, o)
	if err != nil {
		return SessionRef{}, err
	}
	if len(cands) == 0 {
		return SessionRef{query}, nil
	}
	logMatch(query, cands, o.Loc)
	return SessionRef{cands[0].contact.Username}, nil
}

// SessionFromStore is Session that loads contacts only when the query is
// not a direct id.
func SessionFromStore(st *store.Store, query string, o Options) (SessionRef, error) {
	if id, ok := DirectID(query); ok {
		return SessionRef{id}, nil
	}
	book, err := contacts.LoadAll(st)
	if err != nil {
		return SessionRef{}, err
	}
	return Session(st, query, book, o)
}

type scoredContact struct {
	contact *contacts.Contact
	FieldScore
	latest    int64
	hasLatest bool
}

// MatchFields are the fields a contact is matched on, by priority.
func MatchFields(c *contacts.Contact) []Field {
	return []Field{{0, c.PersonalName()}, {1, c.Remark}, {2, c.NickName}, {3, c.Alias}, {4, c.Username}}
}

func scoreContacts(book contacts.Book, query string) []scoredContact {
	s, ok := NewScorer(query)
	if !ok {
		return nil
	}
	var out []scoredContact
	for _, c := range book {
		if fs, ok := s.Best(MatchFields(c)); ok {
			out = append(out, scoredContact{contact: c, FieldScore: fs})
		}
	}
	return out
}

// uniqueExactDirect is the one contact a query names exactly, provided no
// other contact is a direct (token-or-better) match.
func uniqueExactDirect(scored []scoredContact) (string, bool) {
	var hit *scoredContact
	for i := range scored {
		if scored[i].Score >= DirectFloor {
			if hit != nil {
				return "", false
			}
			hit = &scored[i]
		}
	}
	if hit == nil || hit.Score != ExactScore {
		return "", false
	}
	return hit.contact.Username, true
}

func rankCandidates(st *store.Store, cands []scoredContact, o Options) ([]scoredContact, error) {
	if slices.ContainsFunc(cands, func(c scoredContact) bool { return c.Score >= DirectFloor }) {
		cands = slices.DeleteFunc(cands, func(c scoredContact) bool { return c.Score < DirectFloor })
	}
	if o.Activity && len(cands) > 0 {
		latest, err := latestActivity(st, cands, o.Jobs)
		if err != nil {
			return nil, err
		}
		for i := range cands {
			cands[i].latest, cands[i].hasLatest = latest[cands[i].contact.Username]
		}
	}
	slices.SortFunc(cands, func(a, b scoredContact) int {
		if a.hasLatest != b.hasLatest {
			if a.hasLatest {
				return -1
			}
			return 1
		}
		if c := cmp.Compare(a.Priority, b.Priority); c != 0 {
			return c
		}
		if c := cmp.Compare(b.Score, a.Score); c != 0 {
			return c
		}
		if c := cmp.Compare(b.latest, a.latest); c != 0 {
			return c
		}
		if c := cmp.Compare(a.Distance, b.Distance); c != 0 {
			return c
		}
		if c := cmp.Compare(a.FieldLen, b.FieldLen); c != 0 {
			return c
		}
		if c := strings.Compare(a.contact.PersonalName(), b.contact.PersonalName()); c != 0 {
			return c
		}
		return strings.Compare(a.contact.Username, b.contact.Username)
	})
	return cands, nil
}

// latestActivity is each candidate's latest activity: from session.db when
// present (cheap), else the newest message time across shards.
func latestActivity(st *store.Store, cands []scoredContact, jobs int) (map[string]int64, error) {
	out := map[string]int64{}
	if st.Has(SessionDB) {
		conn, err := st.Open(SessionDB)
		if err != nil {
			return nil, err
		}
		defer conn.Close()
		stmt, err := conn.Prepare(`SELECT MAX(COALESCE(last_timestamp, 0), COALESCE(sort_timestamp, 0))
			FROM SessionTable WHERE username = ?1`)
		if err != nil {
			return nil, fmt.Errorf("Prepare session activity lookup: %w", err)
		}
		defer stmt.Finalize()
		for _, c := range cands {
			stmt.Reset()
			if err := stmt.Bind(c.contact.Username); err != nil {
				return nil, fmt.Errorf("Read session activity: %w", err)
			}
			ok, err := stmt.Step()
			if err != nil {
				return nil, fmt.Errorf("Read session activity: %w", err)
			}
			if ok {
				if v := stmt.ColumnInt64(0); v > 0 {
					out[c.contact.Username] = v
				}
			}
		}
		return out, nil
	}
	byTable := map[string]string{}
	for _, c := range cands {
		byTable[msgdb.TableName(c.contact.Username)] = c.contact.Username
	}
	per, err := msgdb.EachDB(st, st.SessionShards(), jobs, func(label string, conn *sqlcipher.Conn) (map[string]int64, error) {
		tables, err := msgdb.ListTables(conn)
		if err != nil {
			return nil, err
		}
		m := map[string]int64{}
		for _, t := range tables {
			u, ok := byTable[t]
			if !ok {
				continue
			}
			v, ok, err := msgdb.LatestCreateTime(conn, label, t)
			if err != nil {
				return nil, err
			}
			if ok && (m[u] == 0 || v > m[u]) {
				m[u] = v
			}
		}
		return m, nil
	})
	if err != nil {
		return nil, err
	}
	for _, m := range per {
		for u, v := range m {
			if cur, ok := out[u]; !ok || v > cur {
				out[u] = v
			}
		}
	}
	return out, nil
}

func logMatch(query string, cands []scoredContact, loc *time.Location) {
	best := cands[0]
	latest := func(c scoredContact, prefix string) string {
		if !c.hasLatest {
			return ""
		}
		return prefix + tgtime.FormatLocal(c.latest, loc)
	}
	if len(cands) > 1 || best.Score < ExactScore {
		logx.Infof("Matched '%s' to %s (%s)%s", query, best.contact.PersonalName(), best.contact.Username, latest(best, " latest "))
	}
	if len(cands) > 1 {
		logx.Infof("Other matches:")
		for _, c := range cands[1:min(len(cands), 5)] {
			logx.Infof("  %s (nick: %s, remark: %s, alias: %s)%s", c.contact.Username, c.contact.NickName,
				c.contact.Remark, c.contact.Alias, latest(c, " - latest "))
		}
	}
}
