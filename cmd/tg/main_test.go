package main

import (
	"slices"
	"testing"

	"github.com/xiaotianxt/tg/internal/env"
)

func knownIn(t *testing.T) func(string) bool {
	root := newRoot(env.Env{Getenv: func(string) string { return "" }, StateDir: t.TempDir()})
	return func(name string) bool {
		for _, c := range root.Commands() {
			if c.Name() == name {
				return true
			}
		}
		return false
	}
}

func TestRewriteArgs(t *testing.T) {
	known := knownIn(t)
	cases := []struct{ in, want []string }{
		{[]string{"tg", "张三", "--limit", "20"}, []string{"tg", "messages", "张三", "--limit", "20"}},
		{[]string{"tg", "--help"}, []string{"tg", "--help"}},
		{[]string{"tg", "help", "messages"}, []string{"tg", "help", "messages"}},
		{[]string{"tg"}, []string{"tg"}},
	}
	for _, c := range cases {
		if got := rewriteArgs(c.in, known); !slices.Equal(got, c.want) {
			t.Errorf("rewriteArgs(%q) = %q", c.in, got)
		}
	}
	for _, name := range []string{"keys", "sessions", "unread", "members", "messages", "search", "query",
		"export", "doctor", "media", "mcp", "skill", "completions"} {
		if !known(name) {
			t.Errorf("%s is not registered", name)
		}
		if got := rewriteArgs([]string{"tg", name}, known); len(got) != 2 {
			t.Errorf("%s was rewritten", name)
		}
	}
	for _, name := range retiredCommands {
		if known(name) {
			t.Errorf("retired %s is registered", name)
		}
		if got := rewriteArgs([]string{"tg", name}, known); len(got) != 2 {
			t.Errorf("retired %s was rewritten", name)
		}
	}
}

func TestMessagesLimit(t *testing.T) {
	cases := []struct {
		limit                    uint
		limitSet, since, allTime bool
		want                     int
		limited                  bool
	}{
		{0, false, false, false, 50, true},
		{0, false, false, true, 0, false},
		{0, false, true, false, 0, false},
		{20, true, false, true, 20, true},
		{0, true, false, false, 0, true},
	}
	for _, c := range cases {
		n, limited := messagesLimit(c.limit, c.limitSet, c.since, c.allTime)
		if n != c.want || limited != c.limited {
			t.Errorf("%+v: got %d,%v", c, n, limited)
		}
	}
}
