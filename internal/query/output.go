package query

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"slices"
	"strconv"
	"strings"
	"time"
	"unicode"

	"github.com/xiaotianxt/tg/internal/tgtime"
)

// Field is one output column.
type Field int

const (
	FieldTime Field = iota
	FieldSession
	FieldSender
	FieldSenderID
	FieldType
	FieldBody
	FieldRawBody
	FieldTimestamp
)

var fieldHeaders = []string{"time", "session", "sender", "sender_id", "type", "body", "raw_body", "timestamp"}

// Header is the field's column header and JSON key.
func (f Field) Header() string { return fieldHeaders[f] }

// fieldAliases are extra spellings accepted for some fields.
var fieldAliases = map[string]Field{
	"local_type":  FieldType,
	"text":        FieldBody,
	"raw":         FieldRawBody,
	"create_time": FieldTimestamp,
}

// ParseFields parses a comma-separated field list, ignoring case,
// surrounding spaces and empty entries. At least one field is required.
func ParseFields(value string) ([]Field, error) {
	var fields []Field
	for _, raw := range strings.Split(value, ",") {
		name := strings.ToLower(strings.TrimSpace(raw))
		if name == "" {
			continue
		}
		f, ok := fieldAliases[name]
		if !ok {
			i := slices.Index(fieldHeaders, name)
			if i < 0 {
				return nil, fmt.Errorf("unknown field '%s'; expected %s", name, strings.Join(fieldHeaders, ", "))
			}
			f = Field(i)
		}
		fields = append(fields, f)
	}
	if len(fields) == 0 {
		return nil, errors.New("at least one field is required")
	}
	return fields, nil
}

// Format is the output layout.
type Format int

const (
	// Table is tab-separated text with a header row.
	Table Format = iota
	// JSONLines is one JSON object per row.
	JSONLines
)

// Output is how rows are printed, separate from the filters so non-CLI
// callers can run a query without choosing a layout.
type Output struct {
	Fields []Field
	Format Format
	// MaxCellChars truncates table cells (runes); JSON is never truncated.
	MaxCellChars int
	Loc          *time.Location
}

// Validate checks the output options.
func (o Output) Validate() error {
	if o.MaxCellChars > MaxCellChars {
		return fmt.Errorf("--max-cell-chars must be <= %d", MaxCellChars)
	}
	return nil
}

// Write prints rows in o.Format. An empty result prints nothing; the caller
// decides where its "no rows" note goes. Table output with the body column
// ends with a hint on opening [img:<id>] images.
func (o Output) Write(w io.Writer, rows []Row) error {
	if o.Format == JSONLines {
		return WriteJSON(w, rows, o.Fields, o.Loc)
	}
	return WriteTable(w, rows, o.Fields, o.MaxCellChars, o.Loc)
}

// WriteTable writes a header row and one tab-separated line per row, with
// cells escaped by escapeCell. Nothing is written for no rows.
func WriteTable(w io.Writer, rows []Row, fields []Field, maxCellChars int, loc *time.Location) error {
	if len(rows) == 0 {
		return nil
	}
	b := bufio.NewWriter(w)
	headers := make([]string, len(fields))
	for i, f := range fields {
		headers[i] = f.Header()
	}
	b.WriteString(strings.Join(headers, "\t") + "\n")
	cells := make([]string, len(fields))
	images := false
	for _, r := range rows {
		for i, f := range fields {
			cells[i] = escapeCell(fieldText(r, f, loc), maxCellChars)
		}
		b.WriteString(strings.Join(cells, "\t") + "\n")
		images = images || strings.Contains(r.Body, "[img:")
	}
	if images && slices.Contains(fields, FieldBody) {
		b.WriteString("View images: tg media <chat> <id>... (ids from [img:<id>])\n")
	}
	if err := b.Flush(); err != nil {
		return fmt.Errorf("Write output: %w", err)
	}
	return nil
}

// WriteJSON writes one JSON object per row with keys in field order (a
// repeated field appears once, at its first position). type and timestamp
// are numbers; type is the full 64-bit local_type.
func WriteJSON(w io.Writer, rows []Row, fields []Field, loc *time.Location) error {
	var keys []Field
	for _, f := range fields {
		if !slices.Contains(keys, f) {
			keys = append(keys, f)
		}
	}
	b := bufio.NewWriter(w)
	for _, r := range rows {
		b.WriteByte('{')
		for i, f := range keys {
			if i > 0 {
				b.WriteByte(',')
			}
			b.Write(jsonString(f.Header()))
			b.WriteByte(':')
			switch f {
			case FieldType:
				b.WriteString(strconv.FormatInt(r.LocalType, 10))
			case FieldTimestamp:
				b.WriteString(strconv.FormatInt(r.CreateTime, 10))
			default:
				b.Write(jsonString(fieldText(r, f, loc)))
			}
		}
		b.WriteString("}\n")
	}
	if err := b.Flush(); err != nil {
		return fmt.Errorf("Write output: %w", err)
	}
	return nil
}

// jsonString encodes s without HTML escaping, as the Rust output did.
func jsonString(s string) []byte {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(s); err != nil {
		panic("encoding a string cannot fail: " + err.Error())
	}
	return bytes.TrimSuffix(buf.Bytes(), []byte("\n"))
}

func fieldText(r Row, f Field, loc *time.Location) string {
	switch f {
	case FieldTime:
		return tgtime.FormatLocal(r.CreateTime, loc)
	case FieldSession:
		return r.Session
	case FieldSender:
		return r.Sender
	case FieldSenderID:
		return r.SenderID
	case FieldType:
		return strconv.FormatInt(r.LocalType, 10)
	case FieldBody:
		return r.Body
	case FieldRawBody:
		return r.RawBody
	case FieldTimestamp:
		return strconv.FormatInt(r.CreateTime, 10)
	}
	panic(fmt.Sprintf("unknown query field %d", f))
}

// escapeCell keeps a table cell on one line and within maxChars runes: tab,
// newline and carriage return become \t, \n, \r, other control characters
// \u{XXXX}, and a cut cell ends with "...". The TSV layout is a script
// contract, so this differs from render.Term's terminal escaping.
func escapeCell(value string, maxChars int) string {
	var b strings.Builder
	n := 0
	for i, r := range value {
		if n == maxChars {
			if i < len(value) {
				b.WriteString("...")
			}
			return b.String()
		}
		n++
		switch {
		case r == '\t':
			b.WriteString(`\t`)
		case r == '\n':
			b.WriteString(`\n`)
		case r == '\r':
			b.WriteString(`\r`)
		case unicode.IsControl(r):
			fmt.Fprintf(&b, `\u{%04x}`, r)
		default:
			b.WriteRune(r)
		}
	}
	return b.String()
}
