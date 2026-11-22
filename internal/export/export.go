// Package export writes a complete chat archive: chat.txt, chat.csv and
// chat.json, plus the chat's cached media (images, videos, stickers, files)
// and voice messages under media/.
package export

import (
	"fmt"
	"os"
	"path/filepath"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/resolve"
	"github.com/xiaotianxt/tg/internal/store"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// Options selects the chat and where its archive goes.
type Options struct {
	// Session is a session id or display name.
	Session   string
	OutputDir string
	Jobs      int
	Env       env.Env
}

// ArchiveFile is one written text export.
type ArchiveFile struct {
	// Format is txt, csv or json.
	Format string
	Path   string
}

// Report is what an export wrote. Warnings about media that could not be
// saved go to stderr while it runs.
type Report struct {
	Username    string
	DisplayName string
	Messages    int
	// Media holds every media outcome in index order: cached media first,
	// then voice. Only Written outcomes produced a file.
	Media []MediaOutcome
	Files []ArchiveFile
}

// message is one archived message with the raw fields media planning needs.
type message struct {
	Time      string `json:"time"`
	Timestamp int64  `json:"timestamp"`
	Sender    string `json:"sender"`
	Type      int64  `json:"type"`
	TypeName  string `json:"type_name"`
	Content   string `json:"content"`

	rawBody string
	packed  []byte
}

// Run exports one chat. It fails without creating OutputDir when the chat
// has no messages.
func Run(st *store.Store, o Options) (Report, error) {
	conn, err := st.OpenOptional(contacts.DB)
	if err != nil {
		return Report{}, err
	}
	book := contacts.Book{}
	if conn != nil {
		book, err = contacts.Load(conn, nil)
		conn.Close()
		if err != nil {
			return Report{}, err
		}
	}
	ref, err := resolve.Session(st, o.Session, book, resolve.Options{Activity: true, Jobs: o.Jobs, Loc: o.Env.Loc})
	if err != nil {
		return Report{}, err
	}
	r := Report{Username: ref.Username, DisplayName: book.Display(ref.Username, contacts.PersonalRemark)}

	// The cache layout does not depend on the messages, so it is found and
	// indexed while they are read.
	sources := make(chan mediaSources, 1)
	go func() { sources <- findMediaSources(o.Env, ref.Username, o.Jobs) }()

	chat := resolve.Chat{
		Session:     ref,
		DisplayName: r.DisplayName,
		Account:     st.AccountUsername(),
		Names:       contacts.Names{Book: book, Mode: contacts.PersonalRemark},
		Loc:         o.Env.Loc,
	}
	msgs, err := readMessages(st, chat, o.Jobs)
	if err != nil {
		<-sources
		return Report{}, err
	}
	src := <-sources
	if len(msgs) == 0 {
		return Report{}, fmt.Errorf("No messages found for '%s'", ref.Username)
	}
	r.Messages = len(msgs)
	if err := os.MkdirAll(o.OutputDir, 0o755); err != nil {
		return Report{}, fmt.Errorf("Cannot create output dir: %w", err)
	}

	// Planning media only reads the cache, so it overlaps the text exports;
	// a writer error is still reported before any media work.
	type written struct {
		files []ArchiveFile
		err   error
	}
	done := make(chan written, 1)
	go func() {
		files, err := writeArchive(o.OutputDir, ref.Username, r.DisplayName, msgs)
		done <- written{files, err}
	}()
	jobs := planMedia(msgs, src, ref.Username, o.Jobs)
	w := <-done
	if w.err != nil {
		return Report{}, w.err
	}
	r.Files = w.files

	mediaDir := filepath.Join(o.OutputDir, "media")
	r.Media = exportMedia(o.Env, src, jobs, mediaDir, ref.Username, o.Jobs)
	voices, err := exportVoices(o.Env, st, ref.Username, mediaDir, len(jobs)+1)
	if err != nil {
		return Report{}, err
	}
	r.Media = append(r.Media, voices...)
	return r, nil
}

// readMessages reads and decodes every message of the chat, oldest first.
func readMessages(st *store.Store, chat resolve.Chat, jobs int) ([]message, error) {
	rows, err := msgdb.ReadSession(st, chat.Session.Username, jobs, msgdb.SessionOptions{
		ScanOptions: msgdb.ScanOptions{Order: msgdb.Oldest},
	})
	if err != nil {
		return nil, err
	}
	msgs := make([]message, len(rows.Rows))
	for i, row := range rows.Rows {
		// The archive shows voice messages without their [voice:id] tag;
		// voice files are named by local id instead.
		row.HasLocalID = false
		m := chat.Decode(row, tgtime.Minutes(1))
		msgs[i] = message{
			Time:      tgtime.FormatLocal(row.CreateTime, chat.Loc),
			Timestamp: row.CreateTime,
			Sender:    m.Sender,
			Type:      row.LocalType,
			TypeName:  m.Type.Label(),
			Content:   m.Content,
			rawBody:   row.Body,
			packed:    row.Packed,
		}
	}
	return msgs, nil
}
