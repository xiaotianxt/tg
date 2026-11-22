package history

import (
	"cmp"
	"fmt"
	"maps"
	"slices"
	"strings"
	"time"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/resolve"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
	"github.com/xiaotianxt/tg/internal/store"
)

// MembersOptions selects a members report.
type MembersOptions struct {
	Session string
	// InactiveSince keeps only members with no recorded message at or after
	// it, when set.
	InactiveSince msgdb.Bound
	// Since, when set, ignores messages before it, as if history started
	// there: counts, last-spoke times and the history span cover only the
	// window. Added for the MCP remote lookback.
	Since msgdb.Bound
	Mode  contacts.NameMode
	Jobs  int
	Loc   *time.Location
}

// MemberActivity is one current member and their recorded speech.
type MemberActivity struct {
	Username    string
	DisplayName string
	IsOwner     bool
	// LastSpoke is valid when HasSpoken.
	LastSpoke    int64
	HasSpoken    bool
	MessageCount int64
}

// MemberReport lists a group's current members.
type MemberReport struct {
	GroupUsername    string
	GroupDisplayName string
	TotalMembers     int
	// HistoryStart/End span the group's recorded sender history when
	// HasHistory.
	HistoryStart, HistoryEnd int64
	HasHistory               bool
	InactiveSince            msgdb.Bound
	// Members are sorted by last recorded speech, never-spoke first.
	Members []MemberActivity
}

type speech struct {
	first, last, count int64
}

type memberGroup struct {
	book   contacts.Book
	roster contacts.Roster
	cards  map[string]string
}

func openContacts(st *store.Store) (*sqlcipher.Conn, error) {
	conn, err := st.Open(contacts.DB)
	if err != nil {
		return nil, fmt.Errorf("Contact database is unavailable: %w", err)
	}
	return conn, nil
}

// ListMembers reports a group's current roster with each member's locally
// recorded speaking activity.
func ListMembers(st *store.Store, o MembersOptions) (MemberReport, error) {
	var ref resolve.SessionRef
	var group memberGroup
	var activity map[string]speech
	if id, direct := resolve.DirectID(o.Session); direct && strings.HasSuffix(id, "@chatroom") {
		// A direct group id resolves without contacts, so the contact read
		// runs beside the shard scans. Its errors still win.
		ref = resolve.SessionRef{Username: id}
		done := make(chan error, 1)
		go func() {
			var err error
			group, err = loadMemberGroup(st, ref.Username, o.Mode)
			done <- err
		}()
		var speechErr error
		activity, speechErr = memberSpeech(st, ref.Username, o.Since, o.Jobs)
		if err := <-done; err != nil {
			return MemberReport{}, err
		}
		if speechErr != nil {
			return MemberReport{}, speechErr
		}
	} else {
		conn, err := openContacts(st)
		if err != nil {
			return MemberReport{}, err
		}
		defer conn.Close()
		book, err := contacts.Load(conn, nil)
		if err != nil {
			return MemberReport{}, err
		}
		if ref, err = resolve.Session(st, o.Session, book, resolve.Options{Activity: true, Jobs: o.Jobs, Loc: o.Loc}); err != nil {
			return MemberReport{}, err
		}
		if !ref.IsGroup() {
			return MemberReport{}, fmt.Errorf("Not a group session: %s", ref.Username)
		}
		roster, err := contacts.LoadRoster(conn, ref.Username)
		if err != nil {
			return MemberReport{}, err
		}
		if group, err = finishMemberGroup(conn, ref.Username, o.Mode, book, roster); err != nil {
			return MemberReport{}, err
		}
		if activity, err = memberSpeech(st, ref.Username, o.Since, o.Jobs); err != nil {
			return MemberReport{}, err
		}
	}

	r := MemberReport{
		GroupUsername:    ref.Username,
		GroupDisplayName: group.book.Display(ref.Username, o.Mode),
		TotalMembers:     len(group.roster.Usernames),
		InactiveSince:    o.InactiveSince,
	}
	for _, s := range activity {
		if !r.HasHistory {
			r.HistoryStart, r.HistoryEnd, r.HasHistory = s.first, s.last, true
		}
		r.HistoryStart, r.HistoryEnd = min(r.HistoryStart, s.first), max(r.HistoryEnd, s.last)
	}
	names := contacts.Names{Book: group.book, Mode: o.Mode, RoomCards: group.cards}
	for _, u := range group.roster.Usernames {
		s, spoke := activity[u]
		m := MemberActivity{
			Username: u, DisplayName: names.Sender(u), IsOwner: group.roster.Owner == u,
			LastSpoke: s.last, HasSpoken: spoke, MessageCount: s.count,
		}
		if o.InactiveSince.Set && spoke && s.last >= o.InactiveSince.At {
			continue
		}
		r.Members = append(r.Members, m)
	}
	slices.SortStableFunc(r.Members, func(a, b MemberActivity) int {
		if a.HasSpoken != b.HasSpoken {
			if a.HasSpoken {
				return 1
			}
			return -1
		}
		if c := cmp.Compare(a.LastSpoke, b.LastSpoke); c != 0 {
			return c
		}
		if c := strings.Compare(a.DisplayName, b.DisplayName); c != 0 {
			return c
		}
		return strings.Compare(a.Username, b.Username)
	})
	return r, nil
}

// loadMemberGroup reads the roster and only the contact rows a report
// shows: the group and its members. A contact error is reported before a
// roster error, matching the unfiltered order.
func loadMemberGroup(st *store.Store, room string, mode contacts.NameMode) (memberGroup, error) {
	conn, err := openContacts(st)
	if err != nil {
		return memberGroup{}, err
	}
	defer conn.Close()
	roster, rosterErr := contacts.LoadRoster(conn, room)
	members := map[string]bool{}
	for _, u := range roster.Usernames {
		members[u] = true
	}
	book, err := contacts.Load(conn, func(u string) bool { return u == room || members[u] })
	if err != nil {
		return memberGroup{}, err
	}
	if rosterErr != nil {
		return memberGroup{}, rosterErr
	}
	return finishMemberGroup(conn, room, mode, book, roster)
}

func finishMemberGroup(conn *sqlcipher.Conn, room string, mode contacts.NameMode, book contacts.Book, roster contacts.Roster) (memberGroup, error) {
	if len(roster.Usernames) == 0 {
		return memberGroup{}, fmt.Errorf("No current members found for group: %s", room)
	}
	cards, err := contacts.RoomCardsFor(conn, room, mode)
	if err != nil {
		return memberGroup{}, err
	}
	return memberGroup{book, roster, cards}, nil
}

// memberSpeech aggregates each sender's first/last message time and count
// across shards. A sender id missing from Name2Id fails the report rather
// than producing a partial inactivity list.
func memberSpeech(st *store.Store, room string, since msgdb.Bound, jobs int) (map[string]speech, error) {
	table := msgdb.TableName(room)
	per, err := msgdb.EachShard(st, jobs, func(label string, conn *sqlcipher.Conn) (map[string]speech, error) {
		ok, err := conn.TableExists(table)
		if err != nil || !ok {
			return nil, err
		}
		name2id, err := msgdb.LoadName2ID(conn)
		if err != nil {
			return nil, err
		}
		bySender := map[int64]speech{}
		err = msgdb.ScanSenders(conn, label, table, func(sender, ct int64) {
			if since.Set && ct < since.At {
				return
			}
			s, seen := bySender[sender]
			if !seen {
				s = speech{first: ct, last: ct}
			}
			s.first, s.last, s.count = min(s.first, ct), max(s.last, ct), s.count+1
			bySender[sender] = s
		})
		if err != nil {
			return nil, err
		}
		out := map[string]speech{}
		// Lowest id first, so an unresolved identity is reported stably.
		for _, id := range slices.Sorted(maps.Keys(bySender)) {
			u, ok := name2id[id]
			if !ok {
				return nil, fmt.Errorf("Cannot resolve sender identity %d in %s; refusing a partial inactivity report", id, label)
			}
			out[u] = bySender[id]
		}
		return out, nil
	})
	if err != nil {
		return nil, err
	}
	total := map[string]speech{}
	for _, m := range per {
		for u, s := range m {
			if t, ok := total[u]; ok {
				s = speech{min(t.first, s.first), max(t.last, s.last), t.count + s.count}
			}
			total[u] = s
		}
	}
	return total, nil
}

// MemberFriend is a group member the account locally lists as a friend.
type MemberFriend struct {
	Username    string
	DisplayName string
}

// MemberFriendReport intersects a group's cached roster with local friend
// evidence.
type MemberFriendReport struct {
	GroupUsername    string
	GroupDisplayName string
	CachedMembers    int
	// Friends are sorted by display name, then username.
	Friends []MemberFriend
}

// ListMemberFriends reads only the contact database: a newly joined group
// needs no message history.
func ListMemberFriends(st *store.Store, query string, mode contacts.NameMode, o resolve.Options) (MemberFriendReport, error) {
	conn, err := st.Open(contacts.DB)
	if err != nil {
		return MemberFriendReport{}, err
	}
	defer conn.Close()
	// Name matching needs every contact; a direct id needs only the rows shown.
	var ref resolve.SessionRef
	var book contacts.Book
	if id, direct := resolve.DirectID(query); direct {
		ref = resolve.SessionRef{Username: id}
	} else {
		if book, err = contacts.Load(conn, nil); err != nil {
			return MemberFriendReport{}, err
		}
		o.Activity = false
		if ref, err = resolve.Session(st, query, book, o); err != nil {
			return MemberFriendReport{}, err
		}
	}
	if !ref.IsGroup() {
		return MemberFriendReport{}, fmt.Errorf("Not a group session: %s", ref.Username)
	}
	roster, err := contacts.LoadRoster(conn, ref.Username)
	if err != nil {
		return MemberFriendReport{}, err
	}
	if len(roster.Usernames) == 0 {
		return MemberFriendReport{}, fmt.Errorf("No cached members found for group: %s", ref.Username)
	}
	friends, err := contacts.FriendUsernames(conn)
	if err != nil {
		return MemberFriendReport{}, err
	}
	if book == nil {
		members := map[string]bool{}
		for _, u := range roster.Usernames {
			members[u] = true
		}
		if book, err = contacts.Load(conn, func(u string) bool { return u == ref.Username || (members[u] && friends[u]) }); err != nil {
			return MemberFriendReport{}, err
		}
	}
	cards, err := contacts.RoomCardsFor(conn, ref.Username, mode)
	if err != nil {
		return MemberFriendReport{}, err
	}
	names := contacts.Names{Book: book, Mode: mode, RoomCards: cards}
	seen := map[string]bool{}
	r := MemberFriendReport{
		GroupUsername:    ref.Username,
		GroupDisplayName: book.Display(ref.Username, mode),
		CachedMembers:    len(roster.Usernames),
	}
	for _, u := range roster.Usernames {
		c, ok := book[u]
		if !friends[u] || !ok || c.IsStranger || seen[u] {
			continue
		}
		seen[u] = true
		r.Friends = append(r.Friends, MemberFriend{Username: u, DisplayName: names.Sender(u)})
	}
	slices.SortFunc(r.Friends, func(a, b MemberFriend) int {
		if c := strings.Compare(a.DisplayName, b.DisplayName); c != 0 {
			return c
		}
		return strings.Compare(a.Username, b.Username)
	})
	return r, nil
}
