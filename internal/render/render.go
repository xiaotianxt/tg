// Package render owns all CLI text. Domain packages return typed results;
// the layout here is a compatibility surface for scripts and the bundled
// skill, so changes are user-visible.
package render

import (
	"bufio"
	"fmt"
	"io"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/xiaotianxt/tg/internal/history"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// Term escapes a single-line field (a name or id) for the terminal: control
// and other non-printable runes become \n, \r, \t or \u{hex}, so a crafted
// contact name cannot move the cursor or forge table rows. Message content
// is printed as stored; multi-line text there is the message itself.
func Term(s string) string {
	clean := true
	for _, r := range s {
		if !printable(r) {
			clean = false
			break
		}
	}
	if clean {
		return s
	}
	var b strings.Builder
	for _, r := range s {
		switch {
		case r == '\n':
			b.WriteString(`\n`)
		case r == '\r':
			b.WriteString(`\r`)
		case r == '\t':
			b.WriteString(`\t`)
		case !printable(r):
			b.WriteString(`\u{` + strconv.FormatInt(int64(r), 16) + `}`)
		default:
			b.WriteRune(r)
		}
	}
	return b.String()
}

// printable rejects only what can drive or spoof a terminal: C0/C1 controls,
// DEL, invalid UTF-8, and bidi embeddings, overrides and isolates. Format
// characters such as the zero-width joiner and private-use glyphs appear in
// real names and emoji sequences, so they pass through.
func printable(r rune) bool {
	switch {
	case r == utf8.RuneError:
		return false
	case r < 0x20, r >= 0x7f && r < 0xa0:
		return false
	case r == 0x061c, r == 0x200e, r == 0x200f,
		r >= 0x202a && r <= 0x202e, r >= 0x2066 && r <= 0x2069:
		return false
	}
	return true
}

// TruncatePreview cuts content to maxChars runes, adding "..." when cut.
func TruncatePreview(content string, maxChars int) string {
	n := 0
	for i := range content {
		if n == maxChars {
			return content[:i] + "..."
		}
		n++
	}
	return content
}

// out buffers lines and keeps the first write error.
type out struct {
	w   *bufio.Writer
	err error
}

func newOut(w io.Writer) *out { return &out{w: bufio.NewWriter(w)} }

func (o *out) line(format string, args ...any) {
	if o.err == nil {
		_, o.err = fmt.Fprintf(o.w, format+"\n", args...)
	}
}

func (o *out) blank() { o.line("") }

func (o *out) done() error {
	if o.err != nil {
		return fmt.Errorf("Write output: %w", o.err)
	}
	if err := o.w.Flush(); err != nil {
		return fmt.Errorf("Flush output: %w", err)
	}
	return nil
}

// Sessions renders a session list, then the empty-result note for query.
func Sessions(w io.Writer, list history.SessionList, query string, loc *time.Location) error {
	o := newOut(w)
	if list.Matched > 0 {
		o.line("%-4s %-8s %-20s %-22s Username", "Rank", "Unread", "Last Activity", "Display Name")
		o.line("%s", strings.Repeat("-", 120))
		for i, s := range list.Sessions {
			o.line("%-4d %-8d %-20s %-22s %s", i+1, s.UnreadCount, tgtime.FormatLocalMinutes(s.LastActive, loc),
				Term(s.DisplayName), Term(s.Username))
		}
		o.blank()
		label := "sessions"
		if list.Kind == history.UnreadSessions {
			label = "unread sessions"
		}
		if list.Filtered {
			o.line("Matched: %d of %d %s", list.Matched, list.Total, label)
		} else {
			o.line("Total: %d %s", list.Total, label)
		}
	}
	if len(list.Sessions) == 0 {
		noun := "sessions"
		if list.Kind == history.UnreadSessions {
			noun = "unread sessions"
		}
		switch {
		case strings.TrimSpace(query) != "":
			o.line("No %s matched '%s'.", noun, query)
		case list.Kind == history.UnreadSessions:
			o.line("No unread sessions.")
		default:
			o.line("No sessions found.")
		}
	}
	return o.done()
}

// MessagesView is how a message page was requested.
type MessagesView struct {
	// SessionQuery is the session as the user typed it.
	SessionQuery string
	Search       string
	Sender       string
	Order        msgdb.Order
	Offset       int
	Limited      bool
	Since        msgdb.Bound
	Bucket       tgtime.Bucket
	Loc          *time.Location
}

// Messages renders a message page, the local-history note when Since
// predates the stored history, and the empty-page note.
func Messages(w io.Writer, page history.MessagePage, v MessagesView) error {
	o := newOut(w)
	msgs := page.Messages
	if len(msgs) > 0 {
		o.line("Chat with: %s (%s)", Term(page.DisplayName), Term(page.Username))
		if v.Search != "" {
			o.line("Search: '%s'", v.Search)
		}
		if v.Sender != "" {
			o.line("Sender: '%s'", v.Sender)
		}
		switch {
		case v.Order == msgdb.Newest && v.Offset > 0:
			o.line("Showing %d messages before the latest %d", len(msgs), v.Offset)
		case v.Order == msgdb.Newest && v.Limited:
			o.line("Showing latest %d messages", len(msgs))
		case v.Order == msgdb.Oldest && (v.Limited || v.Offset > 0):
			o.line("Showing messages %d-%d", v.Offset+1, v.Offset+len(msgs))
		default:
			o.line("Showing %d messages", len(msgs))
		}
		o.blank()
		var lastKey int64
		haveKey := false
		lastSender := ""
		haveSender := false
		for _, m := range msgs {
			if v.Bucket.Kind == tgtime.BucketPerMessage {
				o.line("[%s] %s: %s", tgtime.FormatLocal(m.CreateTime, v.Loc), Term(m.Sender), m.Content)
				continue
			}
			if key, ok := tgtime.TimeKey(m.CreateTime, v.Bucket, v.Loc); ok && (!haveKey || key != lastKey) {
				o.line("[%s]", tgtime.FormatBucket(m.CreateTime, v.Bucket, v.Loc))
				lastKey, haveKey = key, true
				haveSender = false
			}
			if haveSender && lastSender == m.Sender {
				o.line(" %s", m.Content)
			} else {
				o.line("%s: %s", Term(m.Sender), m.Content)
				lastSender, haveSender = m.Sender, true
			}
		}
		o.blank()
		o.line("--- End of messages ---")
		mediaHint(o, page.Username, func(yield func(string) bool) {
			for _, m := range msgs {
				if !yield(m.Content) {
					return
				}
			}
		})
	}
	if page.HasHistory && v.Since.Set && v.Since.At < page.HistoryStart {
		o.line("Note: local history of this chat starts at %s; earlier messages are not stored on this device.",
			tgtime.FormatLocal(page.HistoryStart, v.Loc))
	}
	if len(msgs) == 0 {
		o.line("No messages found for '%s'. Use 'sessions' to list available sessions.", v.SessionQuery)
	}
	return o.done()
}

// mediaHint tells the reader how to open images behind [img:<id>]
// placeholders, once per output and only when some are shown.
func mediaHint(o *out, session string, contents func(func(string) bool)) {
	for c := range contents {
		if strings.Contains(c, "[img:") {
			o.line("View images: tg media %s <id>... (ids from [img:<id>])", Term(session))
			return
		}
	}
}

const searchPreviewChars = 100

// Search renders search hits, then the empty-result note.
func Search(w io.Writer, query string, res history.SearchResults, loc *time.Location) error {
	o := newOut(w)
	hits := res.Hits
	if len(hits) == 0 {
		o.line("No messages found for '%s'.", query)
		return o.done()
	}
	if res.HasMore {
		o.line("Search results for '%s': showing latest %d matches (more available)", query, len(hits))
	} else {
		o.line("Search results for '%s': %d matches", query, len(hits))
	}
	o.blank()
	for i, h := range hits {
		o.line("[%d] %s | %s: %s", i+1, tgtime.FormatLocal(h.CreateTime, loc), Term(h.SessionDisplay),
			TruncatePreview(h.Content, searchPreviewChars))
	}
	if res.HasMore {
		o.line("... more older results available")
	}
	mediaHint(o, "<chat>", func(yield func(string) bool) {
		for _, h := range hits {
			if !yield(h.Content) {
				return
			}
		}
	})
	return o.done()
}

// Members renders a members report.
func Members(w io.Writer, r history.MemberReport, loc *time.Location) error {
	o := newOut(w)
	minutes := func(ts int64) string { return tgtime.FormatLocalMinutes(ts, loc) }
	o.line("Group: %s (%s)", Term(r.GroupDisplayName), Term(r.GroupUsername))
	o.line("Current members: %d", r.TotalMembers)
	if r.HasHistory {
		o.line("Local sender history: %s to %s", minutes(r.HistoryStart), minutes(r.HistoryEnd))
	} else {
		o.line("Local sender history: none")
	}
	if r.InactiveSince.Set {
		cutoff := r.InactiveSince.At
		o.line("Filter: no recorded messages since %s", minutes(cutoff))
		o.line("Note: the current roster has no join times; review recent joiners before removal.")
		if !r.HasHistory || r.HistoryStart > cutoff {
			o.line("Warning: local sender history does not reach the cutoff; this cannot prove a full period of silence.")
		}
	}
	o.blank()
	o.line("%-4s %-20s %-10s %-7s %-22s Username", "Rank", "Last Spoke", "Messages", "Role", "Display Name")
	o.line("%s", strings.Repeat("-", 130))
	for i, m := range r.Members {
		last := "never recorded"
		if m.HasSpoken {
			last = minutes(m.LastSpoke)
		}
		role := ""
		if m.IsOwner {
			role = "owner"
		}
		o.line("%-4d %-20s %-10d %-7s %-22s %s", i+1, last, m.MessageCount, role, Term(m.DisplayName), Term(m.Username))
	}
	o.blank()
	if r.InactiveSince.Set {
		o.line("Candidates: %d of %d current members (local records only)", len(r.Members), r.TotalMembers)
	} else {
		o.line("Listed: %d current members", len(r.Members))
	}
	return o.done()
}

// MemberFriends renders a member-friends report.
func MemberFriends(w io.Writer, r history.MemberFriendReport) error {
	o := newOut(w)
	o.line("Group: %s (%s)", Term(r.GroupDisplayName), Term(r.GroupUsername))
	o.line("Cached members: %d", r.CachedMembers)
	o.line("Locally confirmed friends: %d", len(r.Friends))
	o.line("Note: conservative local relationship evidence only; not a live or reciprocal friendship check.")
	o.blank()
	o.line("%-24s Username", "Display Name")
	for _, f := range r.Friends {
		o.line("%-24s %s", Term(f.DisplayName), Term(f.Username))
	}
	return o.done()
}

// CheckStatus is a doctor check outcome.
type CheckStatus int

const (
	// NoStatus marks a fact line without a status.
	NoStatus CheckStatus = iota
	StatusOK
	StatusMissing
	StatusError
)

// DoctorCheck is one doctor line.
type DoctorCheck struct {
	Label  string
	Status CheckStatus
	// Detail is "" for none.
	Detail string
}

// SessionDiagnosisKind is the outcome of doctor's chat probe.
type SessionDiagnosisKind int

const (
	SessionResolved SessionDiagnosisKind = iota
	SessionFailed
	SessionUnavailable
)

// SessionDiagnosis is doctor's chat probe result.
type SessionDiagnosis struct {
	Query string
	Kind  SessionDiagnosisKind
	Probe history.SessionProbe // when SessionResolved
	Error string               // when SessionFailed
}

// DoctorReport is everything doctor prints. Session is nil without a query.
type DoctorReport struct {
	Checks  []DoctorCheck
	Session *SessionDiagnosis
}

// Doctor renders a doctor report.
func Doctor(w io.Writer, r DoctorReport) error {
	o := newOut(w)
	o.line("tg doctor")
	o.blank()
	for _, c := range r.Checks {
		status := map[CheckStatus]string{StatusOK: "OK", StatusMissing: "MISSING", StatusError: "ERROR"}[c.Status]
		switch {
		case status != "" && c.Detail != "":
			o.line("%s: %s (%s)", c.Label, status, c.Detail)
		case status != "":
			o.line("%s: %s", c.Label, status)
		case c.Detail != "":
			o.line("%s: %s", c.Label, c.Detail)
		default:
			o.line("%s", c.Label)
		}
	}
	if s := r.Session; s != nil {
		o.blank()
		o.line("session query: %s", s.Query)
		switch s.Kind {
		case SessionResolved:
			o.line("resolved session: %s (%s)", Term(s.Probe.DisplayName), Term(s.Probe.Username))
			o.line("message table: %s", s.Probe.TableName)
			o.line("message rows: %d across %d database(s)", s.Probe.MessageCount, s.Probe.MatchingDBs)
		case SessionFailed:
			o.line("session probe: ERROR (%s)", s.Error)
		case SessionUnavailable:
			o.line("session probe: unavailable")
		}
	}
	return o.done()
}
