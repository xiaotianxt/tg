package export

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/xiaotianxt/tg/internal/dict"
)

// writeArchive writes chat.txt, chat.csv and chat.json into dir.
func writeArchive(dir, username, displayName string, msgs []message) ([]ArchiveFile, error) {
	files := []ArchiveFile{
		{"txt", filepath.Join(dir, "chat.txt")},
		{"csv", filepath.Join(dir, "chat.csv")},
		{"json", filepath.Join(dir, "chat.json")},
	}
	writers := []func(io.Writer) error{
		func(w io.Writer) error { return writeTXT(w, username, displayName, msgs) },
		func(w io.Writer) error { return writeCSV(w, msgs) },
		func(w io.Writer) error { return writeJSON(w, msgs) },
	}
	for i, f := range files {
		if err := writeFile(f.Path, writers[i]); err != nil {
			return nil, err
		}
	}
	return files, nil
}

func writeFile(path string, write func(io.Writer) error) error {
	f, err := os.Create(path)
	if err != nil {
		return fmt.Errorf("Cannot create %s: %w", path, err)
	}
	b := bufio.NewWriter(f)
	err = write(b)
	if err == nil {
		err = b.Flush()
	}
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		return fmt.Errorf("Cannot write %s: %w", path, err)
	}
	return nil
}

// writeTXT writes a three-line header, a rule, a blank line, then one
// `[time] sender: content` line per message. msgs is not empty.
func writeTXT(w io.Writer, username, displayName string, msgs []message) error {
	var b strings.Builder
	fmt.Fprintf(&b, "%s 聊天记录: %s (%s)\n", dict.AppDisplayName, displayName, username)
	fmt.Fprintf(&b, "总消息数: %d\n", len(msgs))
	fmt.Fprintf(&b, "时间范围: %s ~ %s\n", msgs[0].Time, msgs[len(msgs)-1].Time)
	b.WriteString(strings.Repeat("=", 60) + "\n\n")
	if _, err := io.WriteString(w, b.String()); err != nil {
		return err
	}
	for _, m := range msgs {
		if _, err := fmt.Fprintf(w, "[%s] %s: %s\n", m.Time, m.Sender, m.Content); err != nil {
			return err
		}
	}
	return nil
}

// writeCSV writes a UTF-8 BOM (for spreadsheet apps), the header row, and
// one row per message.
func writeCSV(w io.Writer, msgs []message) error {
	if _, err := io.WriteString(w, "\xef\xbb\xbf时间,发送者,类型,内容\n"); err != nil {
		return err
	}
	for _, m := range msgs {
		line := csvField(m.Time) + "," + csvField(m.Sender) + "," + csvField(m.TypeName) + "," + csvField(m.Content) + "\n"
		if _, err := io.WriteString(w, line); err != nil {
			return err
		}
	}
	return nil
}

// csvField quotes a field holding a comma, quote or line break, doubling
// embedded quotes.
func csvField(s string) string {
	if !strings.ContainsAny(s, ",\"\n\r") {
		return s
	}
	return `"` + strings.ReplaceAll(s, `"`, `""`) + `"`
}

// writeJSON writes a pretty-printed array of messages without a trailing
// newline and without HTML escaping, as the Rust export did.
func writeJSON(w io.Writer, msgs []message) error {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	if err := enc.Encode(msgs); err != nil {
		return fmt.Errorf("JSON serialization error: %w", err)
	}
	_, err := w.Write(bytes.TrimSuffix(buf.Bytes(), []byte("\n")))
	return err
}
