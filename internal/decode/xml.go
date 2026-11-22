package decode

import (
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/xiaotianxt/tg/internal/strsearch"
)

// The XML helpers scan bytes the way the Rust implementation did; they are
// not an XML parser. Malformed input yields the same partial results, which
// is why encoding/xml is not used.

// ExtractXMLAttr returns the entity-decoded value of the first `attr="…"`
// (or single-quoted) attribute whose name is preceded by `<` or whitespace.
// Spaces are allowed around `=`. Empty values and an unclosed quote give
// ("", false).
func ExtractXMLAttr(xml, attr string) (string, bool) {
	searchFrom := 0
	for {
		rel := strsearch.Index(xml[searchFrom:], attr)
		if rel < 0 {
			return "", false
		}
		start := searchFrom + rel
		prev, size := utf8.DecodeLastRuneInString(xml[:start])
		if size == 0 || (prev != '<' && !unicode.IsSpace(prev)) {
			searchFrom = start + 1
			continue
		}

		pos := start + len(attr)
		for pos < len(xml) && isASCIISpace(xml[pos]) {
			pos++
		}
		if pos >= len(xml) || xml[pos] != '=' {
			searchFrom = start + len(attr)
			continue
		}
		pos++
		for pos < len(xml) && isASCIISpace(xml[pos]) {
			pos++
		}
		if pos >= len(xml) || (xml[pos] != '"' && xml[pos] != '\'') {
			searchFrom = start + len(attr)
			continue
		}
		quote := xml[pos]
		rest := xml[pos+1:]
		end := strings.IndexByte(rest, quote)
		if end < 0 {
			return "", false
		}
		value := DecodeXMLEntities(rest[:end])
		return value, value != ""
	}
}

// isASCIISpace matches Rust's u8::is_ascii_whitespace (no vertical tab).
func isASCIISpace(c byte) bool {
	return c == ' ' || c == '\t' || c == '\n' || c == '\f' || c == '\r'
}

// ExtractXMLTag returns the trimmed, entity-decoded text between the first
// `<tag>` and the next `</tag>`. Nesting is not balanced; use
// ExtractXMLElement for that. Empty text gives ("", false).
func ExtractXMLTag(xml, tag string) (string, bool) {
	start := findWrapped(xml, "<", tag)
	if start < 0 {
		return "", false
	}
	valueStart := start + len("<>") + len(tag)
	if valueStart >= len(xml) {
		return "", false
	}
	rest := xml[valueStart:]
	valueEnd := findWrapped(rest, "</", tag)
	if valueEnd < 0 {
		return "", false
	}
	value := DecodeXMLEntities(strings.TrimSpace(rest[:valueEnd]))
	return value, value != ""
}

// ExtractXMLTagInt parses ExtractXMLTag's text as a signed decimal.
func ExtractXMLTagInt(xml, tag string) (int64, bool) {
	text, ok := ExtractXMLTag(xml, tag)
	if !ok {
		return 0, false
	}
	n, err := strconv.ParseInt(text, 10, 64)
	return n, err == nil
}

// findWrapped is the offset of the first `{prefix}{tag}>` in haystack, or -1.
func findWrapped(haystack, prefix, tag string) int {
	from := 0
	for from <= len(haystack) {
		rel := strsearch.Index(haystack[from:], tag)
		if rel < 0 {
			return -1
		}
		at := from + rel
		end := at + len(tag)
		if strings.HasSuffix(haystack[:at], prefix) && end < len(haystack) && haystack[end] == '>' {
			return at - len(prefix)
		}
		from = at + 1
	}
	return -1
}

// Element holds byte offsets of one element in the string it was found in:
// Start is its `<`, the body lies in [BodyStart, BodyEnd), and End is just
// past the close tag.
type Element struct {
	Start, BodyStart, BodyEnd, End int
}

// Body is the element's body within s, the string it was found in.
func (e Element) Body(s string) string { return s[e.BodyStart:e.BodyEnd] }

// FindXMLElement finds the first `<tag>` or `<tag attr…>` element, closed
// by the `</tag>` that balances it, so a nested element of the same name
// stays inside its parent's body. A self-closing `<tag/>` has an empty
// body. False when the element is missing or never closed. `>` inside
// attribute values is not supported.
func FindXMLElement(xml, tag string) (Element, bool) {
	openStart, bodyStart := -1, 0
	depth := 0
	from := 0
	for {
		rel := strsearch.Index(xml[from:], tag)
		if rel < 0 {
			return Element{}, false
		}
		at := from + rel
		nameEnd := at + len(tag)
		from = nameEnd
		var next byte
		hasNext := nameEnd < len(xml)
		if hasNext {
			next = xml[nameEnd]
		}
		if strings.HasSuffix(xml[:at], "</") {
			if !hasNext || next != '>' {
				continue
			}
			if openStart < 0 {
				continue
			}
			depth--
			if depth == 0 {
				return Element{Start: openStart, BodyStart: bodyStart, BodyEnd: at - 2, End: nameEnd + 1}, true
			}
			from = nameEnd + 1
		} else if strings.HasSuffix(xml[:at], "<") && hasNext &&
			(next == '>' || next == '/' || next == ' ' || next == '\t' || next == '\n' || next == '\r') {
			gt := strings.IndexByte(xml[nameEnd:], '>')
			if gt < 0 {
				return Element{}, false
			}
			openEnd := nameEnd + gt
			from = openEnd + 1
			if xml[openEnd-1] == '/' {
				if depth == 0 {
					return Element{Start: at - 1, BodyStart: from, BodyEnd: from, End: from}, true
				}
				continue
			}
			if depth == 0 {
				openStart, bodyStart = at-1, from
			}
			depth++
		}
	}
}

// ExtractXMLElement is the trimmed, entity-decoded body of the first
// balanced `tag` element (see FindXMLElement); empty gives ("", false).
func ExtractXMLElement(xml, tag string) (string, bool) {
	e, ok := FindXMLElement(xml, tag)
	if !ok {
		return "", false
	}
	value := DecodeXMLEntities(strings.TrimSpace(e.Body(xml)))
	return value, value != ""
}

// StripCDATA trims value and removes a surrounding `<![CDATA[ … ]]>`.
func StripCDATA(value string) string {
	trimmed := strings.TrimSpace(value)
	if inner, ok := strings.CutPrefix(trimmed, "<![CDATA["); ok {
		if inner, ok := strings.CutSuffix(inner, "]]>"); ok {
			return inner
		}
	}
	return trimmed
}

// DecodeXMLEntities decodes the five predefined entities with the result of
// replacing `&amp;` first and then `&quot;`, `&apos;`, `&lt;`, `&gt;` on
// that output, so `&amp;lt;` becomes `<`. Other entities stay as written.
func DecodeXMLEntities(s string) string {
	if strings.IndexByte(s, '&') < 0 {
		return s
	}
	later := [...]struct {
		entity string
		text   byte
	}{{"quot;", '"'}, {"apos;", '\''}, {"lt;", '<'}, {"gt;", '>'}}
	var out strings.Builder
	out.Grow(len(s))
	copied := 0
	for at := 0; at < len(s); at++ {
		if s[at] != '&' || at < copied {
			continue
		}
		body := s[at+1:]
		consumed := 1
		if rest, ok := strings.CutPrefix(body, "amp;"); ok {
			body = rest
			consumed += len("amp;")
		}
		text, n := byte(0), 0
		for _, l := range later {
			if strings.HasPrefix(body, l.entity) {
				text, n = l.text, consumed+len(l.entity)
				break
			}
		}
		if n == 0 {
			if consumed == 1 {
				continue
			}
			text, n = '&', consumed
		}
		out.WriteString(s[copied:at])
		out.WriteByte(text)
		copied = at + n
	}
	out.WriteString(s[copied:])
	return out.String()
}
