package msgdb

import "testing"

func TestLikeContainsEscapesWildcards(t *testing.T) {
	cases := map[string]string{
		"needle": "%needle%",
		"100%_":  `%100\%\_%`,
		`a\b`:    `%a\\b%`,
		"":       "%%",
		"吃饭%":    `%吃饭\%%`,
	}
	for in, want := range cases {
		if got := LikeContains(in); got != want {
			t.Errorf("LikeContains(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestQuoteIdent(t *testing.T) {
	if got := QuoteIdent(`a"b`); got != `"a""b"` {
		t.Errorf("got %s", got)
	}
}

func TestOrderKeyNullFirstThenRowID(t *testing.T) {
	null := OrderKey{RowID: 9}
	a := OrderKey{Seq: 5, HasSeq: true, RowID: 2}
	b := OrderKey{Seq: 5, HasSeq: true, RowID: 3}
	if null.Compare(a) >= 0 || a.Compare(b) >= 0 || b.Compare(a) <= 0 || a.Compare(a) != 0 {
		t.Error("order key comparison broken")
	}
	rows := []Row{{Key: a}, {Key: null}, {Key: b}}
	SortRows(rows, Newest)
	if rows[0].Key != b || rows[2].Key != null {
		t.Errorf("newest order: %+v", rows)
	}
}

// likeMatch must agree with SQL LIKE on uncompressed rows: substring,
// ASCII case-insensitive, wildcards literal, all or any term.
func TestLikeMatchFollowsSQLLike(t *testing.T) {
	cases := []struct {
		body     string
		terms    []string
		matchAny bool
		want     bool
	}{
		{"<msg><WCPayInfo>", []string{"wcpayinfo"}, false, true},
		{"<msg><wcpayinfo>", []string{"WCPAYINFO"}, false, true},
		{"收款 ÄB", []string{"äb"}, false, false}, // non-ASCII letters keep their case
		{"收款 ÄB", []string{"收款"}, false, true},
		{"100%", []string{"0%"}, false, true},
		{"100", []string{"1%0"}, false, false}, // % is literal
		{"alpha beta", []string{"alpha", "gamma"}, false, false},
		{"alpha beta", []string{"alpha", "gamma"}, true, true},
		{"alpha beta", []string{"delta", "gamma"}, true, false},
	}
	for _, c := range cases {
		if got := likeMatch(c.body, c.terms, c.matchAny); got != c.want {
			t.Errorf("likeMatch(%q, %q, any=%v) = %v, want %v", c.body, c.terms, c.matchAny, got, c.want)
		}
	}
}
