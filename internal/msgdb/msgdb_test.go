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
