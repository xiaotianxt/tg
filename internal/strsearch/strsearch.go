// Package strsearch is strings.Index for message text and XML.
//
// strings.Index anchors its scan on the needle's first byte. Message XML
// needles mostly start with '<' and CJK needles with a UTF-8 lead byte,
// both present at a large share of haystack positions, so decoding a full
// history spent more time verifying false anchors than reading pages. These
// functions anchor on the needle's rarest byte instead; results are
// identical to package strings.
package strsearch

import "strings"

// Index is the offset of the first sub in s, or -1, like strings.Index.
func Index(s, sub string) int {
	n := len(sub)
	if n < 2 || n >= len(s) {
		return strings.Index(s, sub)
	}
	k := anchor(sub)
	// Anchors are tried left to right, so the first full match is the
	// leftmost. A match's anchor lies in s[k : len(s)-n+k+1].
	end := len(s) - n + k + 1
	for i := k; i < end; i++ {
		j := strings.IndexByte(s[i:end], sub[k])
		if j < 0 {
			return -1
		}
		i += j
		if s[i-k:i-k+n] == sub {
			return i - k
		}
	}
	return -1
}

// Contains reports whether sub is in s, like strings.Contains.
func Contains(s, sub string) bool { return Index(s, sub) >= 0 }

// anchor is the position of sub's least frequent byte, the last on ties.
func anchor(sub string) int {
	best, bestRank := 0, 256
	for i := 0; i < len(sub); i++ {
		if r := rank(sub[i]); r <= bestRank {
			best, bestRank = i, r
		}
	}
	return best
}

// asciiOrder lists ASCII bytes from most to least frequent in message XML
// and text; bytes not listed rank below all of them.
const asciiOrder = "<>/\" =\n\tetaoinsrhldcumfpgwybvkxjqz0123456789_:.-,;&!?'"

// rank estimates how often c occurs in message bodies (higher is more
// often). UTF-8 continuation bytes spread over 64 values and rank low; lead
// bytes of CJK text fall in a few values and rank high.
func rank(c byte) int {
	switch {
	case c >= 0x80 && c < 0xc0:
		return 1
	case c >= 0xc0:
		return 200
	}
	if i := strings.IndexByte(asciiOrder, c); i >= 0 {
		return 190 - i
	}
	return 2
}
