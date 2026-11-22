package history

import (
	"slices"
	"time"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/resolve"
	"github.com/xiaotianxt/tg/internal/store"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// ReadOptions selects one page of a session's messages.
type ReadOptions struct {
	Session string
	// Limit caps the page when Limited; otherwise the whole range is read.
	Limit   int
	Limited bool
	// Offset skips messages counted from the end Order picks.
	Offset int
	// Search is a raw substring of the stored body ("" for none).
	Search string
	// Since is inclusive, Until exclusive.
	Since, Until msgdb.Bound
	Order        msgdb.Order
	// Sender is `me`, an account id, or a shown name ("" for everyone).
	Sender string
	Bucket tgtime.Bucket
	Mode   contacts.NameMode
	Jobs   int
	Loc    *time.Location
}

// MessagePage is one session's messages in chronological order.
type MessagePage struct {
	Username    string
	DisplayName string
	// HistoryStart is the session's earliest stored message time, whatever
	// the requested range; valid when HasHistory.
	HistoryStart int64
	HasHistory   bool
	Messages     []ChatMessage
}

// ChatMessage is one decoded message.
type ChatMessage struct {
	LocalID    int64
	HasLocalID bool
	LocalType  int64
	CreateTime int64
	Sender     string
	// SenderID is the account username; empty for system messages.
	SenderID string
	Content  string
}

// chatNames is the contact-side data of a message page.
type chatNames struct {
	book  contacts.Book
	cards map[string]string
}

func loadChatNames(st *store.Store, session resolve.SessionRef, mode contacts.NameMode) (chatNames, error) {
	conn, err := st.OpenOptional(contacts.DB)
	if err != nil || conn == nil {
		return chatNames{book: contacts.Book{}, cards: map[string]string{}}, err
	}
	defer conn.Close()
	book, err := contacts.Load(conn, nil)
	if err != nil {
		return chatNames{}, err
	}
	cards, err := contacts.RoomCardsFor(conn, session.Username, mode)
	return chatNames{book, cards}, err
}

// ReadMessages resolves the session, reads its rows across shards, cuts the
// page from the chosen end and decodes it, oldest first.
func ReadMessages(st *store.Store, o ReadOptions) (MessagePage, error) {
	so := msgdb.SessionOptions{
		ScanOptions:  msgdb.ScanOptions{Since: o.Since, Until: o.Until, Order: o.Order},
		HistoryStart: true,
	}
	if o.Search != "" {
		so.BodyLike = []string{o.Search}
	}
	if o.Limited {
		// Shards are merged before the page is cut, so each returns the
		// whole window from its end.
		so.Limit = o.Limit + o.Offset
	}
	ropts := resolve.Options{Activity: true, Jobs: o.Jobs, Loc: o.Loc}

	var ref resolve.SessionRef
	var names chatNames
	var rows msgdb.SessionRows
	if id, direct := resolve.DirectID(o.Session); direct && o.Sender == "" {
		// A direct id resolves without contacts, so contacts load beside the
		// shard reads. Contact errors still win, as when they ran first.
		ref = resolve.SessionRef{Username: id}
		done := make(chan error, 1)
		go func() {
			var err error
			names, err = loadChatNames(st, ref, o.Mode)
			done <- err
		}()
		var rowsErr error
		rows, rowsErr = msgdb.ReadSession(st, ref.Username, o.Jobs, so)
		if err := <-done; err != nil {
			return MessagePage{}, err
		}
		if rowsErr != nil {
			return MessagePage{}, rowsErr
		}
	} else {
		conn, err := st.OpenOptional(contacts.DB)
		if err != nil {
			return MessagePage{}, err
		}
		if conn != nil {
			defer conn.Close()
		}
		names.book = contacts.Book{}
		if conn != nil {
			if names.book, err = contacts.Load(conn, nil); err != nil {
				return MessagePage{}, err
			}
		}
		if ref, err = resolve.Session(st, o.Session, names.book, ropts); err != nil {
			return MessagePage{}, err
		}
		if names.cards, err = contacts.RoomCardsFor(conn, ref.Username, o.Mode); err != nil {
			return MessagePage{}, err
		}
		if o.Sender != "" {
			group, err := resolve.LoadGroupNames(conn, ref)
			if err != nil {
				return MessagePage{}, err
			}
			filter, err := resolve.ResolveSender(st, o.Sender, names.book, group, o.Mode)
			if err != nil {
				return MessagePage{}, err
			}
			so.Senders = filter.Usernames
		}
		if rows, err = msgdb.ReadSession(st, ref.Username, o.Jobs, so); err != nil {
			return MessagePage{}, err
		}
	}

	page := MessagePage{
		Username:     ref.Username,
		DisplayName:  names.book.Display(ref.Username, o.Mode),
		HistoryStart: rows.HistoryStart,
		HasHistory:   rows.HasHistory,
	}
	picked := rows.Rows[min(o.Offset, len(rows.Rows)):]
	if o.Limited {
		picked = picked[:min(o.Limit, len(picked))]
	}
	picked = slices.Clone(picked)
	if o.Order == msgdb.Newest {
		slices.Reverse(picked)
	}
	chat := resolve.Chat{
		Session:     ref,
		DisplayName: page.DisplayName,
		Account:     st.AccountUsername(),
		Names:       contacts.Names{Book: names.book, Mode: o.Mode, RoomCards: names.cards},
		Loc:         o.Loc,
	}
	for _, r := range picked {
		m := chat.Decode(r, o.Bucket)
		page.Messages = append(page.Messages, ChatMessage{
			LocalID: r.LocalID, HasLocalID: r.HasLocalID, LocalType: r.LocalType, CreateTime: r.CreateTime,
			Sender: m.Sender, SenderID: m.SenderID, Content: m.Content,
		})
	}
	return page, nil
}
