package query

import (
	"math"
	"slices"
	"strings"
	"testing"

	"github.com/xiaotianxt/tg/internal/msgdb"
)

func TestParseFields(t *testing.T) {
	got, err := ParseFields(" Time, ,local_type,text,RAW,create_time,sender_id,body")
	if err != nil {
		t.Fatal(err)
	}
	want := []Field{FieldTime, FieldType, FieldBody, FieldRawBody, FieldTimestamp, FieldSenderID, FieldBody}
	if !slices.Equal(got, want) {
		t.Errorf("got %v, want %v", got, want)
	}
	if _, err := ParseFields("time,bogus"); err == nil || !strings.Contains(err.Error(), "unknown field 'bogus'") {
		t.Errorf("bogus field: %v", err)
	}
	if _, err := ParseFields(" , "); err == nil || err.Error() != "at least one field is required" {
		t.Errorf("empty list: %v", err)
	}
}

func TestValidate(t *testing.T) {
	base := Options{Contains: []string{"a"}, Limit: 100, Limited: true}
	cases := []struct {
		name string
		edit func(*Options)
		want string
	}{
		{"ok", func(*Options) {}, ""},
		{"unlimited", func(o *Options) { o.Limited, o.Limit = false, 0 }, ""},
		{"limit zero", func(o *Options) { o.Limit = 0 }, "--limit must be greater than 0"},
		{"window", func(o *Options) { o.Limit, o.Offset = 9000, 1001 }, "--limit plus --offset must be <= 10000"},
		{"window edge", func(o *Options) { o.Limit, o.Offset = 9000, 1000 }, ""},
		{"window overflow", func(o *Options) { o.Limit, o.Offset = 1, math.MaxInt }, "--limit plus --offset must be <= 10000"},
		{"too many", func(o *Options) { o.Not = make([]string, 17) }, "--not accepts at most 16 values"},
		{"blank", func(o *Options) { o.RawContains = []string{" "} }, "--raw-contains cannot be empty"},
		{"long", func(o *Options) { o.Contains = []string{strings.Repeat("字", 257)} }, "--contains values must be <= 256 characters"},
		{"max length", func(o *Options) { o.Contains = []string{strings.Repeat("字", 256)} }, ""},
		{"no bound", func(o *Options) { o.Contains = nil }, "refusing to return every message"},
		{"not alone", func(o *Options) { o.Contains, o.Not = nil, []string{"x"} }, "refusing to return every message"},
		{"since bound", func(o *Options) { o.Contains, o.Since = nil, msgdb.At(1) }, ""},
	}
	for _, c := range cases {
		o := base
		c.edit(&o)
		err := o.Validate()
		if c.want == "" && err != nil || c.want != "" && (err == nil || !strings.Contains(err.Error(), c.want)) {
			t.Errorf("%s: got %v, want %q", c.name, err, c.want)
		}
	}
}

func TestBodyMatches(t *testing.T) {
	all := Options{Contains: []string{"a", "b"}, Not: []string{"x"}}
	anyMode := all
	anyMode.Match = MatchAny
	for _, c := range []struct {
		o    Options
		body string
		want bool
	}{
		{all, "ab", true}, {all, "a", false}, {all, "abx", false},
		{anyMode, "a", true}, {anyMode, "c", false}, {anyMode, "bx", false},
	} {
		if got := c.o.bodyMatches(c.body); got != c.want {
			t.Errorf("match=%v %q: got %v", c.o.Match, c.body, got)
		}
	}
}

func TestEscapeCell(t *testing.T) {
	for _, c := range []struct {
		in   string
		max  int
		want string
	}{
		{"a\tb\nc\rd", 100, `a\tb\nc\rd`},
		{"x\x1by\u0085", 100, `x\u{001b}y\u{0085}`},
		{"bob message 5", 6, "bob me..."},
		{"exact", 5, "exact"},
		{"中文内容", 2, "中文..."},
		{"", 0, ""},
		{"a", 0, "..."},
	} {
		if got := escapeCell(c.in, c.max); got != c.want {
			t.Errorf("escapeCell(%q, %d) = %q, want %q", c.in, c.max, got, c.want)
		}
	}
}

func TestWriteJSONKeyOrder(t *testing.T) {
	var b strings.Builder
	rows := []Row{{Body: "<a&b>", LocalType: 1 << 33, CreateTime: 7}}
	if err := WriteJSON(&b, rows, []Field{FieldType, FieldBody, FieldTimestamp, FieldBody}, nil); err != nil {
		t.Fatal(err)
	}
	if want := `{"type":8589934592,"body":"<a&b>","timestamp":7}` + "\n"; b.String() != want {
		t.Errorf("got %q, want %q", b.String(), want)
	}
}

func span(lo, hi int64) RowIDSpan { return RowIDSpan{Lo: lo, Hi: hi, OK: true} }

func TestPlanTasksPacksSmallTables(t *testing.T) {
	spans := [][]RowIDSpan{{span(1, 10), {}, span(5, 100)}, nil, {span(1, 3000), span(1, 2000)}}
	for _, mode := range []ReadMode{ReadWindow, ReadWhole} {
		tasks := PlanTasks(spans, 4, mode)
		want := []Task{
			{Shard: 0, Parts: []Part{{Table: 0}, {Table: 1}, {Table: 2}}, Weight: 106},
			{Shard: 2, Parts: []Part{{Table: 0}}, Weight: 3000},
			{Shard: 2, Parts: []Part{{Table: 1}}, Weight: 2000},
		}
		if !tasksEqual(tasks, want) {
			t.Errorf("mode %v: got %+v", mode, tasks)
		}
	}
}

func TestPlanTasksSplitsLargeTables(t *testing.T) {
	// 100000 rows over 2 readers: chunk 12500, 8 parts of 12500 rows.
	spans := [][]RowIDSpan{{span(1, 100000)}}
	if tasks := PlanTasks(spans, 2, ReadWindow); len(tasks) != 1 {
		t.Fatalf("a pushed-down limit reads tables whole, got %+v", tasks)
	}
	tasks := PlanTasks(spans, 2, ReadWhole)
	if len(tasks) != 8 {
		t.Fatalf("got %d tasks: %+v", len(tasks), tasks)
	}
	assertCover(t, tasks, 1, 100000)
	if first, last := tasks[0].Parts[0], tasks[7].Parts[0]; first.Start.Set || last.End.Set {
		t.Errorf("outer ranges must be open: %+v %+v", first, last)
	}
}

func TestPlanTasksExtremeRowIDs(t *testing.T) {
	spans := [][]RowIDSpan{{span(math.MinInt64, math.MaxInt64), span(-5, 5)}}
	tasks := PlanTasks(spans, 3, ReadWhole)
	if len(tasks) != 13 {
		t.Fatalf("got %d tasks: %+v", len(tasks), tasks)
	}
	assertCover(t, tasks[:12], math.MinInt64, math.MaxInt64)
	if p := tasks[12].Parts; len(p) != 1 || p[0].Table != 1 || p[0].Start.Set || p[0].End.Set {
		t.Errorf("small table after a split one: %+v", tasks[12])
	}
}

// assertCover checks that the tasks' single-part ranges of table 0 are
// contiguous and non-decreasing between lo and hi.
func assertCover(t *testing.T, tasks []Task, lo, hi int64) {
	t.Helper()
	prev := msgdb.Bound{}
	for i, task := range tasks {
		p := task.Parts[0]
		if p.Start != prev {
			t.Errorf("task %d starts at %+v, previous ended at %+v", i, p.Start, prev)
		}
		if p.Start.Set && (p.Start.At < lo || p.Start.At > hi) || p.End.Set && p.Start.Set && p.End.At < p.Start.At {
			t.Errorf("task %d range %+v out of order", i, p)
		}
		prev = p.End
	}
}

func tasksEqual(a, b []Task) bool {
	return slices.EqualFunc(a, b, func(x, y Task) bool {
		return x.Shard == y.Shard && x.Weight == y.Weight && slices.Equal(x.Parts, y.Parts)
	})
}
