package history

import "testing"

func TestInitialStrategy(t *testing.T) {
	cases := []struct {
		query string
		tok   bool
		want  SearchStrategy
	}{
		{"吃饭", true, FTSMatch},
		{" 吃饭 ", true, FTSMatch},
		{"吃饭", false, ContentLike},
		{"开会 meeting", true, ContentLike},
		{"message", true, ContentLike},
		{"", true, ContentLike},
		{"𠀀", true, FTSMatch}, // extension B
		{"ア", true, ContentLike},
	}
	for _, c := range cases {
		if got := InitialStrategy(c.query, c.tok); got != c.want {
			t.Errorf("InitialStrategy(%q, %v) = %v", c.query, c.tok, got)
		}
	}
	if next, ok := FTSMatch.fallback(); !ok || next != ContentLike {
		t.Error("FTSMatch falls back to ContentLike")
	}
	if _, ok := ContentLike.fallback(); ok {
		t.Error("ContentLike has no fallback")
	}
}

func TestIdeographPhrase(t *testing.T) {
	if got := ideographPhrase("吃饭"); got != `"吃 饭"` {
		t.Errorf("got %s", got)
	}
}
