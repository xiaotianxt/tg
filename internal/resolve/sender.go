package resolve

import (
	"errors"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/decode"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
	"github.com/xiaotianxt/tg/internal/store"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// SelfName names the signed-in account in every message listing, so its
// messages read the same in private chats, groups and structured queries.
const SelfName = "我"

// IsSelf reports whether senderID is the signed-in account.
func IsSelf(senderID, account string) bool {
	return senderID != "" && senderID == account
}

// SenderFilter is the account usernames a sender value selects.
type SenderFilter struct {
	Usernames []string
}

// GroupNames are the names a group gives its members, used to match a
// sender in that group.
type GroupNames struct {
	// Cards is the group nickname per member username.
	Cards map[string]string
	// Members are current members, preferred when a name matches several
	// accounts.
	Members map[string]bool
}

// LoadGroupNames reads session's cards and members; empty for a private
// chat, a missing contact database (conn nil), or an undescribed group.
func LoadGroupNames(conn *sqlcipher.Conn, session SessionRef) (GroupNames, error) {
	if conn == nil || !session.IsGroup() {
		return GroupNames{}, nil
	}
	described, err := contacts.RoomDescribed(conn, session.Username)
	if err != nil || !described {
		return GroupNames{}, err
	}
	cards, err := contacts.RoomCards(conn, session.Username)
	if err != nil {
		return GroupNames{}, err
	}
	roster, err := contacts.LoadRoster(conn, session.Username)
	if err != nil {
		return GroupNames{}, err
	}
	members := make(map[string]bool, len(roster.Usernames))
	for _, u := range roster.Usernames {
		members[u] = true
	}
	return GroupNames{Cards: cards, Members: members}, nil
}

// ResolveSender resolves `me`, an account id, or a name shown for the
// account. Names match exactly, ignoring case, against the names output can
// show in mode, so a remote caller cannot probe contact remarks. A name
// several accounts share is an error that lists them. A value matching no
// name is an account id when some message shard knows it.
func ResolveSender(st *store.Store, value string, book contacts.Book, group GroupNames, mode contacts.NameMode) (SenderFilter, error) {
	value = strings.TrimSpace(value)
	if value == "" {
		return SenderFilter{}, errors.New("sender cannot be empty")
	}
	if strings.EqualFold(value, "me") || value == SelfName {
		account := st.AccountUsername()
		if account == "" {
			return SenderFilter{}, errors.New("Cannot tell which account is yours from the source directory name; " +
				"pass your account id as sender instead")
		}
		return SenderFilter{[]string{account}}, nil
	}
	if _, ok := book[value]; ok {
		return SenderFilter{[]string{value}}, nil
	}
	if _, ok := group.Cards[value]; ok {
		return SenderFilter{[]string{value}}, nil
	}

	matches := nameMatches(value, book, group, mode)
	var inGroup []nameMatch
	for _, m := range matches {
		if group.Members[m.username] {
			inGroup = append(inGroup, m)
		}
	}
	if len(inGroup) > 0 {
		matches = inGroup
	}
	switch len(matches) {
	case 0:
	case 1:
		return SenderFilter{[]string{matches[0].username}}, nil
	default:
		listed := make([]string, len(matches))
		for i, m := range matches {
			listed[i] = m.name + " (" + m.username + ")"
		}
		return SenderFilter{}, fmt.Errorf("Sender '%s' matches several accounts: %s; pass one account id", value, strings.Join(listed, ", "))
	}

	for _, label := range st.MessageShards() {
		ids, err := shardKnows(st, label, value)
		if err != nil {
			return SenderFilter{}, err
		}
		if ids {
			return SenderFilter{[]string{value}}, nil
		}
	}
	return SenderFilter{}, fmt.Errorf("No sender matched '%s'. Pass me, an account id, or a name exactly as listed", value)
}

func shardKnows(st *store.Store, label, username string) (bool, error) {
	conn, err := st.Open(label)
	if err != nil {
		return false, err
	}
	defer conn.Close()
	ids, err := msgdb.SenderRowIDs(conn, label, []string{username})
	return len(ids) > 0, err
}

type nameMatch struct{ username, name string }

// nameMatches lists accounts whose visible names equal value, sorted by
// username, with the name that matched (group cards first).
func nameMatches(value string, book contacts.Book, group GroupNames, mode contacts.NameMode) []nameMatch {
	wanted := strings.ToLower(value)
	same := func(name string) bool {
		return name != "" && strings.ToLower(strings.TrimSpace(name)) == wanted
	}
	found := map[string]string{}
	for u, card := range group.Cards {
		if same(card) {
			found[u] = card
		}
	}
	for _, c := range book {
		names := []string{c.NickName, c.Alias}
		if mode == contacts.PersonalRemark {
			names = append(names, c.Remark)
		}
		for _, n := range names {
			if same(n) {
				if _, ok := found[c.Username]; !ok {
					found[c.Username] = n
				}
				break
			}
		}
	}
	out := make([]nameMatch, 0, len(found))
	for u, n := range found {
		out = append(out, nameMatch{u, n})
	}
	slices.SortFunc(out, func(a, b nameMatch) int { return strings.Compare(a.username, b.username) })
	return out
}

// Chat holds what sender inference needs about one session. It is the one
// rule for messages, query, export and MCP: in a private chat a row without
// a sender, or sent by the partner, is the partner's; any other row is the
// owner's, shown as 我. Group rows carry a sender prefix that decode
// resolves; the owner's own group rows have none and fall back to 我.
type Chat struct {
	Session SessionRef
	// DisplayName is the session's display name.
	DisplayName string
	// Account is the signed-in account, "" when unknown.
	Account string
	Names   contacts.Names
	// Loc formats times inside forwarded chat records.
	Loc *time.Location
}

// Message is one decoded message with its inferred sender.
type Message struct {
	Type    decode.Type
	Content string
	// Sender is the display name; SenderID the account username, empty for
	// system messages.
	Sender   string
	SenderID string
}

// Decode decodes row and attributes it. The voice id is row.LocalID when
// HasLocalID; callers wanting another id (query falls back to the rowid) set
// LocalID/HasLocalID before calling.
func (c Chat) Decode(row msgdb.Row, bucket tgtime.Bucket) Message {
	fallback := c.DisplayName
	if row.Sender != "" && row.Sender != c.Session.Username {
		fallback = SelfName
	}
	var voiceID int64
	if row.LocalType == int64(decode.TypeVoice) && row.HasLocalID && row.LocalID > 0 {
		voiceID = row.LocalID
	}
	m := decode.Decode(decode.Raw{LocalType: row.LocalType, Body: row.Body, Packed: row.Packed, VoiceID: voiceID},
		decode.Ctx{SessionName: fallback, Bucket: bucket, Loc: c.Loc, ResolveName: c.Names.Sender})
	out := Message{Type: m.Type, Content: m.Content, Sender: m.DisplayName}
	switch {
	case m.Type.IsSystem():
	case row.Sender == "" && !c.Session.IsGroup():
		out.SenderID = c.Session.Username
	default:
		out.SenderID = row.Sender
	}
	if IsSelf(out.SenderID, c.Account) {
		out.Sender = SelfName
	}
	return out
}
