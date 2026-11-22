package strsearch

import (
	"strings"
	"testing"
)

func TestIndexMatchesStrings(t *testing.T) {
	hay := []string{
		"", "a", "哈", "哈哈", "呵哈哈嗯", "嗯嗯哈", "ab哈哈", "哈哈ab", "aXb",
		"\x88\x88哈", "e5\xe5\x93哈", "<msg><appmsg><title>x</title><type>5</type></appmsg></msg>",
		"<<patinfo<patinfo>", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
	}
	subs := []string{
		"", "a", "ab", "哈", "哈哈", "嗯嗯", "哈ab", "\x93", "\xe5\x93", "\x88哈", "哈哈哈哈哈哈",
		"<patinfo", "</title>", "type", "<type>", "msg>", "zz", "zzzzzzzzzz",
	}
	for _, h := range hay {
		for _, n := range subs {
			if got, want := Index(h, n), strings.Index(h, n); got != want {
				t.Errorf("Index(%q, %q) = %d, want %d", h, n, got, want)
			}
		}
	}
}
