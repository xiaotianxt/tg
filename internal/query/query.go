// Package query runs structured, parameterized message queries across every
// message shard: argument validation, the shard/rowid scan plan, decoded
// and raw filters, global ordering and paging, and the table and JSON-lines
// output of `tg query`. Callers never pass SQL.
//
// The MCP query tool calls Run with Options and gets structured rows back;
// the CLI additionally validates an Output and writes rows with Output.Write.
package query

import (
	"errors"
	"fmt"
	"slices"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/strsearch"
)

// Limits on query arguments.
const (
	MaxKeywords     = 16
	MaxKeywordChars = 256
	// MaxWindow caps limit+offset, the rows each table may contribute.
	MaxWindow = 10_000
	// MaxCellChars caps --max-cell-chars.
	MaxCellChars = 10_000
)

// MatchMode is how repeated --contains (and --raw-contains) values combine.
type MatchMode int

const (
	MatchAll MatchMode = iota
	MatchAny
)

// Media is a structured media type --has selects.
type Media int

const (
	Voice Media = iota
	Image
	Sticker
	File
	Video
)

var mediaNames = []string{"voice", "image", "sticker", "file", "video"}

// String is the media name decode.MediaType reports.
func (m Media) String() string { return mediaNames[m] }

// ParseMedia parses a --has value.
func ParseMedia(s string) (Media, error) {
	for i, n := range mediaNames {
		if s == n {
			return Media(i), nil
		}
	}
	return 0, fmt.Errorf("unknown media type '%s'; expected %s", s, strings.Join(mediaNames, ", "))
}

// Options selects messages. Session, Sender and the term lists are user
// input; Validate checks them before any database is opened.
type Options struct {
	// Session is a session id or display name; "" searches every chat.
	Session string
	// Contains and Not match the decoded body (substring); RawContains the
	// stored body (SQL LIKE). Match combines Contains and RawContains.
	Contains, Not, RawContains []string
	// Has keeps messages carrying any of these media types.
	Has []Media
	// Sender is `me`, an account id, or a shown name; "" for everyone.
	Sender string
	// Since is inclusive, Until exclusive, on create_time.
	Since, Until msgdb.Bound
	// Limit caps the rows returned when Limited. MCP passes Limited=false
	// to get every match; the CLI always pages.
	Limit   int
	Limited bool
	// Offset skips rows after global ordering.
	Offset int
	Order  msgdb.Order
	Match  MatchMode
	Mode   contacts.NameMode
	Jobs   int
	// Loc formats times inside forwarded chat records.
	Loc *time.Location
}

// Row is one matched message.
type Row struct {
	// Session is the chat's display name; SessionID its username.
	Session   string
	SessionID string
	// Sender is the display name (我 for the account owner); SenderID the
	// account username, empty for system messages.
	Sender   string
	SenderID string
	// LocalType is the full stored 64-bit type.
	LocalType  int64
	CreateTime int64
	// Body is the decoded text; RawBody the stored (decompressed) body.
	Body    string
	RawBody string
}

// Validate checks the options without touching any database.
func (o Options) Validate() error {
	if o.Limited && o.Limit <= 0 {
		return errors.New("--limit must be greater than 0")
	}
	if o.Offset < 0 {
		return errors.New("--offset must not be negative")
	}
	for _, t := range []struct {
		flag  string
		terms []string
	}{{"--contains", o.Contains}, {"--not", o.Not}, {"--raw-contains", o.RawContains}} {
		if err := validateTerms(t.flag, t.terms); err != nil {
			return err
		}
	}
	if _, err := o.window(); err != nil {
		return err
	}
	if len(o.Contains) == 0 && len(o.RawContains) == 0 && len(o.Has) == 0 &&
		o.Sender == "" && o.Session == "" && !o.Since.Set {
		return errors.New("refusing to return every message of every chat; " +
			"pass --session, --contains, --raw-contains, --has, --sender, or --since")
	}
	return nil
}

func validateTerms(flag string, terms []string) error {
	if len(terms) > MaxKeywords {
		return fmt.Errorf("%s accepts at most %d values", flag, MaxKeywords)
	}
	for _, t := range terms {
		if strings.TrimSpace(t) == "" {
			return fmt.Errorf("%s cannot be empty", flag)
		}
		if utf8.RuneCountInString(t) > MaxKeywordChars {
			return fmt.Errorf("%s values must be <= %d characters", flag, MaxKeywordChars)
		}
	}
	return nil
}

// window is limit+offset, the most rows any one table can contribute to the
// result; 0 for an unlimited query.
func (o Options) window() (int, error) {
	if !o.Limited {
		return 0, nil
	}
	if o.Limit > MaxWindow || o.Offset > MaxWindow-o.Limit {
		return 0, fmt.Errorf("--limit plus --offset must be <= %d", MaxWindow)
	}
	return o.Limit + o.Offset, nil
}

// filtersBody reports whether rows must be decoded while scanning.
func (o Options) filtersBody() bool { return len(o.Contains) > 0 || len(o.Not) > 0 }

// needsPostFilter reports whether some rows the SQL selects are dropped
// afterwards, so a pushed-down LIMIT could cut matches.
func (o Options) needsPostFilter() bool {
	if o.filtersBody() {
		return true
	}
	for _, m := range o.Has {
		if m == File || m == Video {
			return true
		}
	}
	return false
}

// bodyMatches applies --contains and --not to a decoded body.
func (o Options) bodyMatches(body string) bool {
	if len(o.Contains) > 0 {
		in := func(t string) bool { return strsearch.Contains(body, t) }
		out := func(t string) bool { return !strsearch.Contains(body, t) }
		if o.Match == MatchAll && slices.ContainsFunc(o.Contains, out) ||
			o.Match == MatchAny && !slices.ContainsFunc(o.Contains, in) {
			return false
		}
	}
	for _, t := range o.Not {
		if strsearch.Contains(body, t) {
			return false
		}
	}
	return true
}
