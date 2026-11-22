package export

import "testing"

func TestCSVField(t *testing.T) {
	for in, want := range map[string]string{
		"plain":       "plain",
		"a,b":         `"a,b"`,
		`say "hi"`:    `"say ""hi"""`,
		"line\nbreak": "\"line\nbreak\"",
		"cr\rhere":    "\"cr\rhere\"",
		"":            "",
		"中文，全角逗号":     "中文，全角逗号",
	} {
		if got := csvField(in); got != want {
			t.Errorf("csvField(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestFileSafeID(t *testing.T) {
	for id, want := range map[string]bool{
		"0123456789abcdef0123456789abcdef": true,
		"a-b_c.d":                          true,
		".hidden":                          false,
		"../x":                             false,
		"https://cdn/x":                    false,
		"中文":                               false,
		string(make([]byte, 129)):          false,
	} {
		if got := fileSafeID(id); got != want {
			t.Errorf("fileSafeID(%q) = %v, want %v", id, got, want)
		}
	}
}

func TestFileIdentifierFallbacks(t *testing.T) {
	for _, c := range []struct{ body, want string }{
		{"<msg><appmsg><title>report.pdf</title><type>6</type></appmsg></msg>", "report.pdf"},
		{"<msg><appmsg><title> </title><type>6</type></appmsg></msg>", ""},
		{"  /tmp/dir/notes.txt ", "notes.txt"},
		{"name-only", "name-only"},
		{"/trailing/", ""},
	} {
		if got := fileIdentifier(message{rawBody: c.body}); got != c.want {
			t.Errorf("fileIdentifier(%q) = %q, want %q", c.body, got, c.want)
		}
	}
}
