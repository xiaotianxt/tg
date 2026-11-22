package render

import "testing"

func TestTerm(t *testing.T) {
	cases := map[string]string{
		"Alice":                  "Alice",
		"爱丽丝":                    "爱丽丝",
		"O'Brien \\ x":           "O'Brien \\ x",
		"a\nb\tc\rd":             `a\nb\tc\rd`,
		"bell\x07":               `bell\u{7}`,
		"esc\x1b[31m":            `esc\u{1b}[31m`,
		"family\u200d\U0001F467": "family\u200d\U0001F467",
		"pua\ue600":              "pua\ue600",
		"rtl\u202egnp.exe":       `rtl\u{202e}gnp.exe`,
		"c1\u009b31m":            `c1\u{9b}31m`,
	}
	for in, want := range cases {
		if got := Term(in); got != want {
			t.Errorf("Term(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestTruncatePreview(t *testing.T) {
	if got := TruncatePreview("提取一下", 2); got != "提取..." {
		t.Errorf("got %q", got)
	}
	if got := TruncatePreview("提取", 2); got != "提取" {
		t.Errorf("got %q", got)
	}
	if got := TruncatePreview("", 0); got != "" {
		t.Errorf("got %q", got)
	}
}
