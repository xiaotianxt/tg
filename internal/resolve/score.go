package resolve

import (
	"strings"
	"unicode"
	"unicode/utf8"
)

// Match scores, best first. Token-or-better matches are "direct": when any
// candidate reaches DirectFloor, weaker ones are dropped.
const (
	ExactScore              = 400
	NormalizedExactScore    = 390
	ContainsScore           = 300
	NormalizedContainsScore = 290
	TokenScore              = 280
	DirectFloor             = TokenScore
	FuzzyScore              = 100
)

// FieldScore is how well one field matched a query.
type FieldScore struct {
	Score int
	// Priority is the field's rank (0 = display name ... 4 = username).
	Priority int
	Distance int
	// FieldLen is the normalized field length in characters.
	FieldLen int
}

// Field is a candidate text with its priority.
type Field struct {
	Priority int
	Text     string
}

// Scorer is a query prepared once for scoring many fields.
type Scorer struct {
	query      string
	normalized string
	queryRunes []rune
	tokens     []string
}

// NewScorer prepares query; ok is false when it normalizes to nothing, which
// matches no field.
func NewScorer(query string) (*Scorer, bool) {
	n := normalize(query)
	if n == "" {
		return nil, false
	}
	return &Scorer{query: query, normalized: n, queryRunes: []rune(n), tokens: tokenize(query)}, true
}

// Best is the best-scoring field: lowest priority, then highest score, then
// smallest distance, then shortest field.
func (s *Scorer) Best(fields []Field) (FieldScore, bool) {
	var best FieldScore
	found := false
	for _, f := range fields {
		score, dist, flen, ok := s.Score(f.Text)
		if !ok {
			continue
		}
		c := FieldScore{Score: score, Priority: f.Priority, Distance: dist, FieldLen: flen}
		if !found || better(c, best) {
			best, found = c, true
		}
	}
	return best, found
}

// better reports whether a beats b; on a full tie the later field wins, as
// Rust's max_by keeps the last maximum.
func better(a, b FieldScore) bool {
	if a.Priority != b.Priority {
		return a.Priority < b.Priority
	}
	if a.Score != b.Score {
		return a.Score > b.Score
	}
	if a.Distance != b.Distance {
		return a.Distance < b.Distance
	}
	return a.FieldLen <= b.FieldLen
}

// Score returns (score, distance, normalized field length in runes).
func (s *Scorer) Score(field string) (score, distance, fieldLen int, ok bool) {
	field = strings.TrimSpace(field)
	if field == "" {
		return 0, 0, 0, false
	}
	nf := normalize(field)
	fieldLen = utf8.RuneCountInString(nf)
	if fieldLen == 0 {
		return 0, 0, 0, false
	}
	switch {
	case field == s.query:
		return ExactScore, 0, fieldLen, true
	case nf == s.normalized:
		return NormalizedExactScore, 0, fieldLen, true
	case strings.Contains(field, s.query):
		return ContainsScore, 0, fieldLen, true
	case strings.Contains(nf, s.normalized):
		return NormalizedContainsScore, 0, fieldLen, true
	}
	if len(s.tokens) > 1 {
		all := true
		for _, t := range s.tokens {
			if !strings.Contains(nf, t) {
				all = false
				break
			}
		}
		if all {
			return TokenScore, 0, fieldLen, true
		}
	}
	queryLen := len(s.queryRunes)
	if queryLen < 2 {
		return 0, 0, 0, false
	}
	maxLen := max(fieldLen, queryLen)
	allowed := 1
	if maxLen > 3 {
		allowed = max(maxLen/3, 1)
	}
	// The length difference is a lower bound on the edit distance.
	if abs(fieldLen-queryLen) > allowed {
		return 0, 0, 0, false
	}
	d, ok := boundedLevenshtein([]rune(nf), s.queryRunes, allowed)
	if !ok {
		return 0, 0, 0, false
	}
	return FuzzyScore + (maxLen-d)*100/maxLen, d, fieldLen, true
}

func abs(v int) int {
	if v < 0 {
		return -v
	}
	return v
}

// normalize lowercases and drops all whitespace.
func normalize(v string) string {
	var b strings.Builder
	b.Grow(len(v))
	for _, r := range v {
		if unicode.IsSpace(r) {
			continue
		}
		writeLower(&b, r)
	}
	return b.String()
}

// writeLower writes the full lowercase mapping of r. Go's unicode.ToLower is
// the simple mapping; the one rune whose full mapping differs is U+0130.
func writeLower(b *strings.Builder, r rune) {
	if r == 'İ' {
		b.WriteString("i̇")
		return
	}
	b.WriteRune(unicode.ToLower(r))
}

// tokenize splits the lowercased, trimmed query on non-alphanumeric runes.
func tokenize(v string) []string {
	var tokens []string
	var cur strings.Builder
	var lower strings.Builder
	for _, r := range strings.TrimSpace(v) {
		writeLower(&lower, r)
	}
	for _, r := range lower.String() {
		if unicode.IsLetter(r) || unicode.IsNumber(r) {
			cur.WriteRune(r)
		} else if cur.Len() > 0 {
			tokens = append(tokens, cur.String())
			cur.Reset()
		}
	}
	if cur.Len() > 0 {
		tokens = append(tokens, cur.String())
	}
	return tokens
}

// boundedLevenshtein is the edit distance over runes, or ok=false once it
// must exceed limit. Row minima never decrease, so a row whose minimum
// exceeds limit proves the final distance does too.
func boundedLevenshtein(left, right []rune, limit int) (int, bool) {
	if len(left) == 0 || len(right) == 0 {
		d := max(len(left), len(right))
		return d, d <= limit
	}
	row := make([]int, len(right)+1)
	for j := range row {
		row[j] = j
	}
	for i, lc := range left {
		diag := row[0]
		row[0] = i + 1
		rowMin := row[0]
		for j, rc := range right {
			sub := diag
			if lc != rc {
				sub++
			}
			v := min(sub, row[j]+1, row[j+1]+1)
			diag = row[j+1]
			row[j+1] = v
			rowMin = min(rowMin, v)
		}
		if rowMin > limit {
			return 0, false
		}
	}
	d := row[len(right)]
	return d, d <= limit
}
