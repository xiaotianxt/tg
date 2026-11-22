package tgtime

import (
	"testing"
	"time"
)

func newYork(t *testing.T) *time.Location {
	t.Helper()
	loc, err := time.LoadLocation("America/New_York")
	if err != nil {
		t.Fatal(err)
	}
	return loc
}

func TestParse(t *testing.T) {
	ny := newYork(t)
	now := time.Date(2026, 4, 28, 15, 30, 0, 0, ny)
	nowTS := now.Unix()
	local := func(y int, m time.Month, d, h, mi, s int) int64 {
		return time.Date(y, m, d, h, mi, s, 0, ny).Unix()
	}
	for _, tc := range []struct {
		in   string
		want int64
	}{
		{"2026-04-28", local(2026, 4, 28, 0, 0, 0)},
		{"2026-4-8", local(2026, 4, 8, 0, 0, 0)},
		{"2024-01-01T00:00:00", local(2024, 1, 1, 0, 0, 0)},
		{"2024-01-01 12:34:56", local(2024, 1, 1, 12, 34, 56)},
		{"2024-01-01T00:00:00Z", 1704067200},
		{"2024-01-01 00:00:00z", 1704067200},
		{"2024-01-01T08:00:00.123+08:00", 1704067200},
		{"2024-01-01t00:00:00-01:30", 1704067200 + 5400},
		{"today", local(2026, 4, 28, 0, 0, 0)},
		{"yesterday", local(2026, 4, 27, 0, 0, 0)},
		{"5min", nowTS - 300},
		{"30s", nowTS - 30},
		{"2h", nowTS - 7200},
		{"7d", nowTS - 7*86400},
		{"1w", nowTS - 604800},
		{"1y", nowTS - 365*86400},
		// A repeated local hour resolves to the earlier instant (EDT).
		{"2026-11-01 01:30:00", time.Date(2026, 11, 1, 5, 30, 0, 0, time.UTC).Unix()},
	} {
		got, err := Parse(tc.in, now, ny)
		if err != nil || got != tc.want {
			t.Errorf("Parse(%q) = %d, %v; want %d", tc.in, got, err, tc.want)
		}
	}

	for _, tc := range []struct{ in, err string }{
		{"5m", "Unknown time unit 'm' in '5m'"},
		{"xmin", "Invalid number in 'xmin'"},
		{"2026-03-08 02:30:00", "Local time '2026-03-08 02:30:00' does not exist in the system time zone."},
		{"2024-02-30", "Cannot parse time expression '2024-02-30'. Use ISO 8601 (e.g. '2024-01-01T00:00:00') or relative (e.g. '5min', '1h', '7d', '1y', 'today')."},
		{"5好", "Cannot parse time expression '5好'. Use ISO 8601 (e.g. '2024-01-01T00:00:00') or relative (e.g. '5min', '1h', '7d', '1y', 'today')."},
	} {
		_, err := Parse(tc.in, now, ny)
		if err == nil || err.Error() != tc.err {
			t.Errorf("Parse(%q) error = %v; want %q", tc.in, err, tc.err)
		}
	}
}

func TestFormatLocal(t *testing.T) {
	ny := newYork(t)
	const ts = 1_775_000_000 // 2026-03-31 23:33:20 UTC
	if got := FormatLocal(ts, ny); got != "2026-03-31 19:33:20" {
		t.Errorf("FormatLocal = %q", got)
	}
	if got := FormatLocalMinutes(ts, ny); got != "2026-03-31 19:33" {
		t.Errorf("FormatLocalMinutes = %q", got)
	}
	if d, m := FormatLocalDateAndMinute(ts, ny); d != "2026-03-31" || m != "19:33" {
		t.Errorf("FormatLocalDateAndMinute = %q %q", d, m)
	}
	if got, ok := ParseLocalMinutes("2026-03-31 19:33", ny); !ok || got != ts-20 {
		t.Errorf("ParseLocalMinutes = %d %v", got, ok)
	}
	if got, ok := ParseLocalMinutes("2026-03-31 19:33:20", ny); !ok || got != ts {
		t.Errorf("ParseLocalMinutes seconds = %d %v", got, ok)
	}
}

func TestParseBucket(t *testing.T) {
	for _, tc := range []struct {
		in   string
		want Bucket
	}{
		{"1m", Minutes(1)},
		{"1min", Minutes(1)},
		{"2h", Hours(2)},
		{"1d", Day},
		{"1mo", Month},
		{"1y", Year},
		{"full", PerMessage},
		{" FULL ", PerMessage},
		{"always", PerMessage},
		{"none", NoTime},
		{"off", NoTime},
	} {
		got, err := ParseBucket(tc.in)
		if err != nil || got != tc.want {
			t.Errorf("ParseBucket(%q) = %v, %v; want %v", tc.in, got, err, tc.want)
		}
	}
	for _, tc := range []struct{ in, err string }{
		{"2d", "Only 1d is supported for calendar buckets."},
		{"0m", "Time bucket must be positive in '0m'"},
		{"15", "Missing time unit in '15'"},
		{"m", "Invalid number in 'm'"},
		{"1x", "Unknown time bucket '1x'. Use 1m, 1min, 1h, 1d, 1mo, 1y, full, or none."},
	} {
		_, err := ParseBucket(tc.in)
		if err == nil || err.Error() != tc.err {
			t.Errorf("ParseBucket(%q) error = %v; want %q", tc.in, err, tc.err)
		}
	}
}

func TestTimeKeyCoalescesMinutes(t *testing.T) {
	ny := newYork(t)
	const ts = 1_775_000_000 - 20 // on a minute boundary
	key := func(ts int64) int64 {
		k, ok := TimeKey(ts, Minutes(1), ny)
		if !ok {
			t.Fatal("no key")
		}
		return k
	}
	if key(ts) != key(ts+30) || key(ts) == key(ts+60) {
		t.Error("minute bucket keys do not coalesce within a minute")
	}
	if _, ok := TimeKey(ts, NoTime, ny); ok {
		t.Error("BucketNone has a key")
	}
}

func TestFormatBucket(t *testing.T) {
	ny := newYork(t)
	const ts = 1_775_000_000
	for _, tc := range []struct {
		b    Bucket
		want string
	}{
		{PerMessage, "2026-03-31 19:33:20"},
		{NoTime, ""},
		{Minutes(1), "2026-03-31 19:33"},
		{Minutes(15), "2026-03-31 19:30"},
		{Hours(1), "2026-03-31 19:00"},
		{Day, "2026-03-31"},
		{Month, "2026-03"},
		{Year, "2026"},
	} {
		if got := FormatBucket(ts, tc.b, ny); got != tc.want {
			t.Errorf("FormatBucket(%v) = %q; want %q", tc.b, got, tc.want)
		}
	}
}
