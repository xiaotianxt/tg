package tools

import (
	"fmt"
	"slices"
	"strconv"
	"strings"

	"github.com/xiaotianxt/tg/internal/mcp/rpc"
	"github.com/xiaotianxt/tg/internal/msgdb"
)

// jsonLen is the compact encoded size of v.
func jsonLen(v any) int {
	b, err := rpc.Marshal(v)
	if err != nil {
		panic(fmt.Sprintf("tools: encode %T: %v", v, err))
	}
	return len(b)
}

// fit drops items from the end until whole(items) encodes within maxBytes,
// and reports whether anything was dropped. Each round removes about the
// overflow's share of items, so trimming stays linear.
func fit[T any](maxBytes int, items []T, whole func([]T) any) ([]T, bool) {
	dropped := false
	for len(items) > 0 && jsonLen(whole(items)) > maxBytes {
		total := len(items)
		size := jsonLen(items)
		over := max(size-maxBytes, 1)
		perItem := max(size/total, 1)
		remove := min(max((over+perItem-1)/perItem, 1), total)
		items = items[:total-remove]
		dropped = true
	}
	return items, dropped
}

// page is the continuation part of a trimmed list result. A whole result
// carries neither field.
type page struct {
	NextOffset *int `json:"next_offset,omitempty"`
	Truncated  bool `json:"truncated,omitempty"`
}

// continueAt marks a trimmed result to continue right after its kept items.
func continueAt(truncated bool, offset, kept int) page {
	if !truncated {
		return page{}
	}
	next := offset + kept
	return page{NextOffset: &next, Truncated: true}
}

// transcriptMessage is one message rendered for read_messages.
type transcriptMessage struct {
	date string
	// block is `HH:MM sender: text` plus indented continuation lines,
	// newline-terminated.
	block string
	// sender is the message's senders-list line; "" for system messages.
	sender string
}

const senderLegendTitle = "Senders (name = id):\n"

func senderLegendLine(name, id string) string {
	if id == "" {
		return ""
	}
	return "  " + name + " = " + id + "\n"
}

// textLines splits like Rust's str::lines: on \n, dropping one trailing
// empty line and a \r before each \n.
func textLines(s string) []string {
	if s == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(s, "\n"), "\n")
	for i, l := range lines {
		lines[i] = strings.TrimSuffix(l, "\r")
	}
	return lines
}

// transcriptBlock renders one message. Continuation lines are indented so
// they can never be mistaken for a new message.
func transcriptBlock(minute, sender, text string) string {
	lines := textLines(text)
	first := ""
	if len(lines) > 0 {
		first = lines[0]
	}
	var b strings.Builder
	b.WriteString(minute)
	b.WriteByte(' ')
	if sender != "" {
		b.WriteString(sender)
		b.WriteString(": ")
	}
	b.WriteString(first)
	b.WriteByte('\n')
	for _, l := range lines[min(1, len(lines)):] {
		b.WriteString("  ")
		b.WriteString(l)
		b.WriteByte('\n')
	}
	return b.String()
}

func dayHeader(date string) string { return "== " + date + " ==\n" }

func truncationFooter(order msgdb.Order, nextOffset int) string {
	omitted := "Older"
	if order == msgdb.Oldest {
		omitted = "Newer"
	}
	return fmt.Sprintf("[truncated] %s messages were left out to fit the size cap. "+
		"Repeat the same call with offset=%d to read them.\n", omitted, nextOffset)
}

// fitTranscript renders messages (chronological) within maxBytes. When they
// do not all fit it keeps those nearest the paging origin (the end order
// puts first), so offset+kept continues exactly where the kept ones end.
// header holds a {count} placeholder for the number kept.
func fitTranscript(header string, messages []transcriptMessage, order msgdb.Order, maxBytes, offset int) (string, int) {
	// Upper bounds: the count only shrinks, and the footer's offset is at
	// most offset+len(messages).
	headerBytes := len(strings.ReplaceAll(header, "{count}", strconv.Itoa(len(messages))))
	footerBytes := len(truncationFooter(order, offset+len(messages)))
	budget := max(maxBytes-headerBytes, 0)

	fromOrigin := slices.Clone(messages)
	if order == msgdb.Newest {
		slices.Reverse(fromOrigin)
	}
	used, kept := 0, 0
	lastDate := ""
	senders := map[string]bool{}
	for _, m := range fromOrigin {
		cost := len(m.block)
		if kept == 0 || lastDate != m.date {
			cost += len(dayHeader(m.date))
		}
		newSender := m.sender != "" && !senders[m.sender]
		if newSender {
			cost += len(m.sender)
			if len(senders) == 0 {
				cost += len(senderLegendTitle)
			}
		}
		// Reserve room for the footer whenever something would be left out.
		reserve := 0
		if kept+1 < len(messages) {
			reserve = footerBytes
		}
		if used+cost+reserve > budget {
			break
		}
		used += cost
		kept++
		lastDate = m.date
		if newSender {
			senders[m.sender] = true
		}
	}

	shown := messages[:kept]
	if order == msgdb.Newest {
		shown = messages[len(messages)-kept:]
	}
	var b strings.Builder
	b.WriteString(strings.ReplaceAll(header, "{count}", strconv.Itoa(kept)))
	if len(senders) > 0 {
		b.WriteString(senderLegendTitle)
		lines := make([]string, 0, len(senders))
		for l := range senders {
			lines = append(lines, l)
		}
		slices.Sort(lines)
		for _, l := range lines {
			b.WriteString(l)
		}
	}
	for i, m := range shown {
		if i == 0 || shown[i-1].date != m.date {
			b.WriteString(dayHeader(m.date))
		}
		b.WriteString(m.block)
	}
	if kept < len(messages) {
		b.WriteString(truncationFooter(order, offset+kept))
	}
	return b.String(), kept
}
