package resolve

import (
	"testing"
	"unicode/utf8"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/dict"
)

func TestDirectID(t *testing.T) {
	cases := map[string]bool{
		"":                         true,
		"  ":                       true,
		dict.AccountIDPrefix + "x": true,
		"gh_news":                  true,
		"room@chatroom":            true,
		" room@chatroom ":          true,
		"Alice":                    false,
		"xgh_news":                 false,
	}
	for in, want := range cases {
		if _, got := DirectID(in); got != want {
			t.Errorf("DirectID(%q) = %v, want %v", in, got, want)
		}
	}
	if id, _ := DirectID(" gh_a "); id != "gh_a" {
		t.Errorf("DirectID trims: %q", id)
	}
}

// fullLevenshtein is the unbounded reference distance.
func fullLevenshtein(a, b []rune) int {
	prev := make([]int, len(b)+1)
	for j := range prev {
		prev[j] = j
	}
	for i, ca := range a {
		cur := make([]int, len(b)+1)
		cur[0] = i + 1
		for j, cb := range b {
			sub := prev[j]
			if ca != cb {
				sub++
			}
			cur[j+1] = min(sub, cur[j]+1, prev[j+1]+1)
		}
		prev = cur
	}
	return prev[len(b)]
}

func TestBoundedLevenshteinMatchesFull(t *testing.T) {
	words := []string{"", "a", "ab", "ba", "abc", "kitten", "sitting", "李雨田", "田雨", "flaw", "lawn"}
	for _, l := range words {
		for _, r := range words {
			full := fullLevenshtein([]rune(l), []rune(r))
			for limit := 0; limit < 8; limit++ {
				got, ok := boundedLevenshtein([]rune(l), []rune(r), limit)
				if ok != (full <= limit) || (ok && got != full) {
					t.Errorf("%q %q limit %d: got %d,%v full %d", l, r, limit, got, ok, full)
				}
			}
		}
	}
}

func score(t *testing.T, query, field string) (int, bool) {
	t.Helper()
	s, ok := NewScorer(query)
	if !ok {
		t.Fatalf("NewScorer(%q) empty", query)
	}
	sc, _, _, ok := s.Score(field)
	return sc, ok
}

func TestScorerTiers(t *testing.T) {
	cases := []struct {
		query, field string
		want         int
		ok           bool
	}{
		{"Alice", "Alice", ExactScore, true},
		{"alice", "A lice", NormalizedExactScore, true},
		{"li", "Alice", ContainsScore, true},
		{"ALI", "alice zhang", NormalizedContainsScore, true},
		{"Linux 2026", "Linux 俱乐部 #2026🌅", TokenScore, true},
		{"Alic Zhang", "Alice Zhang", TokenScore, true},
		{"Alce", "Alice", FuzzyScore + 80, true},
		{"zz", "Alice", 0, false},
		{"a", "b", 0, false}, // fuzzy needs a two-rune query
		{"abc", "   ", 0, false},
	}
	for _, c := range cases {
		got, ok := score(t, c.query, c.field)
		if ok != c.ok || got != c.want {
			t.Errorf("Score(%q, %q) = %d,%v want %d,%v", c.query, c.field, got, ok, c.want, c.ok)
		}
	}
	if _, ok := NewScorer(" \t　"); ok {
		t.Error("whitespace-only query must not match")
	}
}

func TestScorerFieldLenCountsRunes(t *testing.T) {
	s, _ := NewScorer("田雨坤")
	_, _, n, ok := s.Score("田雨 坤")
	if !ok || n != utf8.RuneCountInString("田雨坤") {
		t.Errorf("field len %d ok %v", n, ok)
	}
}

func TestBestPrefersLowerPriority(t *testing.T) {
	s, _ := NewScorer("Linux")
	best, ok := s.Best([]Field{{4, "Linux"}, {1, "Linuxer"}})
	if !ok || best.Priority != 1 || best.Score != ContainsScore {
		t.Errorf("best = %+v", best)
	}
}

func TestUniqueExactDirect(t *testing.T) {
	mk := func(u string, s int) scoredContact {
		c := scoredContact{FieldScore: FieldScore{Score: s}}
		c.contact = &contacts.Contact{Username: u}
		return c
	}
	if u, ok := uniqueExactDirect([]scoredContact{mk("a", ExactScore), mk("b", FuzzyScore+50)}); !ok || u != "a" {
		t.Errorf("got %q %v", u, ok)
	}
	if _, ok := uniqueExactDirect([]scoredContact{mk("a", ExactScore), mk("b", ContainsScore)}); ok {
		t.Error("another direct match must block")
	}
	if _, ok := uniqueExactDirect([]scoredContact{mk("a", ContainsScore)}); ok {
		t.Error("a lone contains match is not exact")
	}
}
