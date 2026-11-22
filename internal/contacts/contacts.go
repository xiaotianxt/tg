// Package contacts reads contact/contact.db: contact rows, display-name
// modes, group rosters and member cards, and local friend evidence.
package contacts

import (
	"fmt"
	"strings"

	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
	"github.com/xiaotianxt/tg/internal/store"
)

// DB is the contact database's path in the store.
const DB = "contact/contact.db"

// NameMode chooses which names output may show.
type NameMode int

const (
	// PersonalRemark prefers the owner's private remark.
	PersonalRemark NameMode = iota
	// Anonymous shows only public names: nicknames and group cards.
	Anonymous
)

// Contact is one contact or stranger row.
type Contact struct {
	Username, NickName, Remark, Alias string
	// IsStranger is set when the stranger table lists the username.
	IsStranger bool
}

// PersonalName is remark, else nickname, else username (trimmed).
func (c *Contact) PersonalName() string {
	preferred := c.NickName
	if c.Remark != "" {
		preferred = c.Remark
	}
	return firstNonEmpty(preferred, c.Username)
}

// DisplayName is the name shown in mode.
func (c *Contact) DisplayName(mode NameMode) string {
	if mode == Anonymous {
		return firstNonEmpty(c.NickName, c.Username)
	}
	return c.PersonalName()
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v = strings.TrimSpace(v); v != "" {
			return v
		}
	}
	return ""
}

// Book maps usernames to contacts.
type Book map[string]*Contact

// Display is the contact's display name in mode, or username when unknown.
func (b Book) Display(username string, mode NameMode) string {
	if c, ok := b[username]; ok {
		return c.DisplayName(mode)
	}
	return username
}

// Load reads contacts whose username passes keep (nil keeps all), then marks
// or adds stranger rows. Every row is still type-checked, so a malformed row
// fails the read even when it is not kept. Later rows replace earlier ones.
func Load(conn *sqlcipher.Conn, keep func(string) bool) (Book, error) {
	book := Book{}
	if err := loadTable(conn, "contact", keep, func(c *Contact) { book[c.Username] = c }); err != nil {
		return nil, fmt.Errorf("Contact read error: %w", err)
	}
	ok, err := conn.TableExists("stranger")
	if err != nil {
		return nil, fmt.Errorf("Contact schema error: %w", err)
	}
	if ok {
		strangers := Book{}
		if err := loadTable(conn, "stranger", keep, func(c *Contact) {
			c.IsStranger = true
			strangers[c.Username] = c
		}); err != nil {
			return nil, fmt.Errorf("Stranger contact read error: %w", err)
		}
		for u, s := range strangers {
			if c, ok := book[u]; ok {
				c.IsStranger = true
			} else {
				book[u] = s
			}
		}
	}
	return book, nil
}

func loadTable(conn *sqlcipher.Conn, table string, keep func(string) bool, put func(*Contact)) error {
	return conn.Query("SELECT username, nick_name, remark, alias FROM "+table, nil, func(st *sqlcipher.Stmt) error {
		if st.ColumnType(0) != sqlcipher.TypeText {
			return fmt.Errorf("username is not text")
		}
		var fields [4]string
		for i := range fields {
			v, err := msgdb.Text(st, i)
			if err != nil {
				return err
			}
			fields[i] = v
		}
		if keep != nil && !keep(fields[0]) {
			return nil
		}
		put(&Contact{Username: fields[0], NickName: fields[1], Remark: fields[2], Alias: fields[3]})
		return nil
	})
}

// LoadAll reads every contact when the store has a contact database and
// returns an empty book otherwise.
func LoadAll(st *store.Store) (Book, error) {
	return LoadWhere(st, nil)
}

// LoadWhere is LoadAll restricted to usernames passing keep.
func LoadWhere(st *store.Store, keep func(string) bool) (Book, error) {
	conn, err := st.OpenOptional(DB)
	if err != nil || conn == nil {
		return Book{}, err
	}
	defer conn.Close()
	return Load(conn, keep)
}

// Names resolves sender ids to display names.
type Names struct {
	Book Book
	Mode NameMode
	// RoomCards are group cards per member; consulted only in Anonymous mode.
	RoomCards map[string]string
}

// Sender is the display name of senderID: a non-empty group card in
// Anonymous mode, else the contact's display name, else the id itself.
func (n Names) Sender(senderID string) string {
	if n.Mode == Anonymous {
		if card := strings.TrimSpace(n.RoomCards[senderID]); card != "" {
			return card
		}
	}
	return n.Book.Display(senderID, n.Mode)
}

// RoomCardsFor reads room's member cards when mode is Anonymous and conn is
// present; otherwise it returns none, since only anonymous output shows them.
func RoomCardsFor(conn *sqlcipher.Conn, room string, mode NameMode) (map[string]string, error) {
	if conn == nil || mode != Anonymous {
		return map[string]string{}, nil
	}
	return RoomCards(conn, room)
}

// RoomCards reads the member cards (group nicknames) of room from its
// chat_room ext buffer; empty when the room or buffer is missing.
func RoomCards(conn *sqlcipher.Conn, room string) (map[string]string, error) {
	var buf []byte
	_, err := conn.QueryRow("SELECT ext_buffer FROM chat_room WHERE username = ?1", []any{room},
		func(st *sqlcipher.Stmt) error { buf = st.ColumnBlob(0); return nil })
	if err != nil {
		return nil, fmt.Errorf("Chat room member query error: %w", err)
	}
	return parseRoomCards(buf), nil
}

// RoomDescribed reports whether chat_room has a row for room. A group can
// have messages without one.
func RoomDescribed(conn *sqlcipher.Conn, room string) (bool, error) {
	ok, err := conn.QueryRow("SELECT 1 FROM chat_room WHERE username = ?1", []any{room},
		func(*sqlcipher.Stmt) error { return nil })
	if err != nil {
		return false, fmt.Errorf("Chat room query error: %w", err)
	}
	return ok, nil
}

// Roster is a group's current members.
type Roster struct {
	// Owner is "" when unknown.
	Owner string
	// Usernames are sorted by username.
	Usernames []string
}

// LoadRoster reads room's members from chatroom_member joined to contact.
// A member without a contact identity makes the roster incomplete, which is
// an error rather than a silently shorter list.
func LoadRoster(conn *sqlcipher.Conn, room string) (Roster, error) {
	var roomID int64
	var owner string
	found, err := conn.QueryRow("SELECT id, owner FROM chat_room WHERE username = ?1", []any{room},
		func(st *sqlcipher.Stmt) error {
			roomID = st.ColumnInt64(0)
			var err error
			owner, err = msgdb.Text(st, 1)
			return err
		})
	if err != nil {
		return Roster{}, fmt.Errorf("Chat room query error: %w", err)
	}
	if !found {
		return Roster{}, fmt.Errorf("Group not found: %s", room)
	}
	var r Roster
	unidentified := 0
	err = conn.Query(`SELECT member.member_id, contact.username
		FROM chatroom_member AS member
		LEFT JOIN contact ON contact.id = member.member_id
		WHERE member.room_id = ?1
		ORDER BY contact.username`, []any{roomID}, func(st *sqlcipher.Stmt) error {
		u, err := msgdb.Text(st, 1)
		if err != nil {
			return err
		}
		if strings.TrimSpace(u) == "" {
			unidentified++
		} else {
			r.Usernames = append(r.Usernames, u)
		}
		return nil
	})
	if err != nil {
		return Roster{}, fmt.Errorf("Read group roster: %w", err)
	}
	if unidentified > 0 {
		return Roster{}, fmt.Errorf("Group roster is incomplete: %d current member(s) have no contact identity", unidentified)
	}
	if strings.TrimSpace(owner) != "" {
		r.Owner = owner
	}
	return r, nil
}

// FriendUsernames is conservative local friend evidence, not a server-side
// relationship check: both relationship bits (a single bit can include the
// account itself), not deleted, not a service account.
func FriendUsernames(conn *sqlcipher.Conn) (map[string]bool, error) {
	out := map[string]bool{}
	err := conn.Query(`SELECT username FROM contact
		WHERE local_type = 1 AND (flag & 3) = 3
		  AND delete_flag = 0 AND verify_flag = 0
		  AND username IS NOT NULL AND username != ''`, nil, func(st *sqlcipher.Stmt) error {
		u, err := msgdb.Text(st, 0)
		out[u] = true
		return err
	})
	if err != nil {
		return nil, fmt.Errorf("Friend relationship metadata unavailable: %w", err)
	}
	return out, nil
}

// parseRoomCards reads repeated field 1 member records {1: username,
// 2: card}. Parsing stops at the first malformed outer field; a malformed or
// incomplete record is skipped.
func parseRoomCards(data []byte) map[string]string {
	names := map[string]string{}
	pos := 0
	for pos < len(data) {
		tag, ok := varint(data, &pos)
		if !ok {
			break
		}
		field, wire := tag>>3, byte(tag&7)
		if field != 1 || wire != 2 {
			if !skip(data, &pos, wire) {
				break
			}
			continue
		}
		member, ok := lenBytes(data, &pos)
		if !ok {
			break
		}
		if u, card, ok := parseRoomCard(member); ok {
			names[u] = card
		}
	}
	return names
}

func parseRoomCard(data []byte) (username, card string, ok bool) {
	pos := 0
	for pos < len(data) {
		tag, ok := varint(data, &pos)
		if !ok {
			return "", "", false
		}
		field, wire := tag>>3, byte(tag&7)
		switch {
		case wire == 2 && (field == 1 || field == 2):
			b, ok := lenBytes(data, &pos)
			if !ok || !utf8Valid(b) {
				return "", "", false
			}
			if field == 1 {
				username = string(b)
			} else {
				card = string(b)
			}
		default:
			if !skip(data, &pos, wire) {
				return "", "", false
			}
		}
	}
	username, card = strings.TrimSpace(username), strings.TrimSpace(card)
	if username == "" || card == "" {
		return "", "", false
	}
	return username, card, true
}
