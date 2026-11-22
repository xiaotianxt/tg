// Package tgtime parses time expressions and formats message times. It is
// pure: the caller passes the current instant and the time zone, so nothing
// here reads the clock or time.Local.
package tgtime

import (
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"
	"unicode"
)

const (
	dateTimeLayout = "2006-01-02 15:04:05"
	minuteLayout   = "2006-01-02 15:04"
	hourLayout     = "2006-01-02 15:00"
	dateLayout     = "2006-01-02"
	monthLayout    = "2006-01"
	yearLayout     = "2006"
)

// Parse turns a time expression into a Unix timestamp. It tries, in order:
// RFC 3339 (`T`, `t` or space separator, `Z` or numeric offset); a naive
// local `YYYY-MM-DDTHH:MM:SS` or `YYYY-MM-DD HH:MM:SS`; a local date
// (midnight); `today` / `yesterday` (local midnight); `Nmin`; and
// `N{s,h,d,w,y}` (a year is 365 days). `5m` is an error. A local time that a
// DST change repeats resolves to the earlier instant; one it skips is an
// error.
func Parse(s string, now time.Time, loc *time.Location) (int64, error) {
	if ts, ok := parseRFC3339(s); ok {
		return ts, nil
	}
	for _, sep := range []byte{'T', ' '} {
		if dt, ok := parseNaive(s, sep); ok {
			return localTimestamp(dt, loc)
		}
	}
	if d, ok := parseNaiveDate(s); ok {
		return localTimestamp(d, loc)
	}

	nowTS := now.Unix()
	switch s {
	case "today":
		return localTimestamp(dateOf(now.In(loc), 0), loc)
	case "yesterday":
		return localTimestamp(dateOf(now.In(loc), -1), loc)
	}

	if num, ok := strings.CutSuffix(s, "min"); ok {
		minutes, err := strconv.ParseInt(num, 10, 64)
		if err != nil {
			return 0, fmt.Errorf("Invalid number in '%s'", s)
		}
		return nowTS - minutes*60, nil
	}

	if len(s) >= 2 {
		numPart, unit := s[:len(s)-1], s[len(s)-1:]
		if num, err := strconv.ParseInt(numPart, 10, 64); err == nil {
			switch unit {
			case "s":
				return nowTS - num, nil
			case "h":
				return nowTS - num*3600, nil
			case "d":
				return nowTS - num*86400, nil
			case "w":
				return nowTS - num*604800, nil
			case "y":
				return nowTS - num*365*86400, nil
			default:
				return 0, fmt.Errorf("Unknown time unit '%s' in '%s'", unit, s)
			}
		}
	}

	return 0, fmt.Errorf("Cannot parse time expression '%s'. Use ISO 8601 (e.g. '2024-01-01T00:00:00') "+
		"or relative (e.g. '5min', '1h', '7d', '1y', 'today').", s)
}

// naive is a wall-clock time with no zone attached.
type naive struct {
	year, month, day, hour, min, sec int
}

func dateOf(t time.Time, dayOffset int) naive {
	d := time.Date(t.Year(), t.Month(), t.Day()+dayOffset, 0, 0, 0, 0, time.UTC)
	return naive{year: d.Year(), month: int(d.Month()), day: d.Day()}
}

func (n naive) valid() bool {
	if n.month < 1 || n.month > 12 || n.day < 1 || n.hour > 23 || n.min > 59 || n.sec > 59 {
		return false
	}
	d := time.Date(n.year, time.Month(n.month), n.day, 0, 0, 0, 0, time.UTC)
	return d.Day() == n.day
}

func (n naive) String() string {
	return fmt.Sprintf("%04d-%02d-%02d %02d:%02d:%02d", n.year, n.month, n.day, n.hour, n.min, n.sec)
}

// localTimestamp resolves a wall-clock time in loc. Go's time.Date picks an
// arbitrary side of a DST transition, so both candidate offsets are checked
// explicitly.
func localTimestamp(n naive, loc *time.Location) (int64, error) {
	wall := time.Date(n.year, time.Month(n.month), n.day, n.hour, n.min, n.sec, 0, time.UTC).Unix()
	var found []int64
	seen := map[int]bool{}
	// Offsets in effect around this wall time: a transition shifts the
	// offset by at most a day.
	for _, probe := range []int64{wall - 86400, wall, wall + 86400} {
		_, off := time.Unix(probe, 0).In(loc).Zone()
		if seen[off] {
			continue
		}
		seen[off] = true
		candidate := wall - int64(off)
		if _, got := time.Unix(candidate, 0).In(loc).Zone(); got == off {
			found = append(found, candidate)
		}
	}
	if len(found) == 0 {
		return 0, fmt.Errorf("Local time '%s' does not exist in the system time zone.", n)
	}
	earliest := found[0]
	for _, c := range found[1:] {
		earliest = min(earliest, c)
	}
	return earliest, nil
}

// parseRFC3339 follows chrono's `DateTime::parse_from_rfc3339`: a fixed
// 19-byte date-time with a `T`, `t` or space separator, optional fraction,
// and `Z`/`z` or a `±HH:MM` offset (U+2212 accepted as minus). Second 60 is
// a leap second and maps to the timestamp of second 59.
func parseRFC3339(s string) (int64, bool) {
	if len(s) < 19 {
		return 0, false
	}
	digits := func(i, n int) (int, bool) {
		v := 0
		for _, c := range []byte(s[i : i+n]) {
			if c < '0' || c > '9' {
				return 0, false
			}
			v = v*10 + int(c-'0')
		}
		return v, true
	}
	year, ok1 := digits(0, 4)
	month, ok2 := digits(5, 2)
	day, ok3 := digits(8, 2)
	hour, ok4 := digits(11, 2)
	minute, ok5 := digits(14, 2)
	sec, ok6 := digits(17, 2)
	if !ok1 || !ok2 || !ok3 || !ok4 || !ok5 || !ok6 ||
		s[4] != '-' || s[7] != '-' || s[13] != ':' || s[16] != ':' ||
		(s[10] != 'T' && s[10] != 't' && s[10] != ' ') {
		return 0, false
	}
	if sec == 60 {
		sec = 59
	}
	n := naive{year, month, day, hour, minute, sec}
	if !n.valid() {
		return 0, false
	}
	rest := s[19:]
	if strings.HasPrefix(rest, ".") {
		i := 1
		for i < len(rest) && rest[i] >= '0' && rest[i] <= '9' {
			i++
		}
		if i == 1 {
			return 0, false
		}
		rest = rest[i:]
	}
	var offset int
	switch {
	case strings.HasPrefix(rest, "Z") || strings.HasPrefix(rest, "z"):
		rest = rest[1:]
	default:
		negative := false
		switch {
		case strings.HasPrefix(rest, "+"):
			rest = rest[1:]
		case strings.HasPrefix(rest, "-"):
			negative, rest = true, rest[1:]
		case strings.HasPrefix(rest, "−"):
			negative, rest = true, rest[len("−"):]
		default:
			return 0, false
		}
		if len(rest) != 5 || rest[2] != ':' {
			return 0, false
		}
		if !isDigits(rest[:2]) || !isDigits(rest[3:]) {
			return 0, false
		}
		h, _ := strconv.Atoi(rest[:2])
		m, _ := strconv.Atoi(rest[3:])
		if m > 59 {
			return 0, false
		}
		offset = h*3600 + m*60
		if offset >= 86400 {
			return 0, false
		}
		if negative {
			offset = -offset
		}
		rest = ""
	}
	if rest != "" {
		return 0, false
	}
	wall := time.Date(n.year, time.Month(n.month), n.day, n.hour, n.min, n.sec, 0, time.UTC).Unix()
	return wall - int64(offset), true
}

func isDigits(s string) bool {
	for _, c := range []byte(s) {
		if c < '0' || c > '9' {
			return false
		}
	}
	return s != ""
}

// scanner mirrors chrono's strftime parser for `%Y-%m-%d` style formats:
// each number may follow whitespace, `%Y` takes 1–4 digits or a signed
// number of any width, the other fields take 1–2 digits, and no input may
// remain.
type scanner struct{ s string }

func (sc *scanner) number(maxDigits int, signed bool) (int, bool) {
	sc.s = strings.TrimLeftFunc(sc.s, unicode.IsSpace)
	negative := false
	if signed && sc.s != "" && (sc.s[0] == '-' || sc.s[0] == '+') {
		negative = sc.s[0] == '-'
		sc.s = sc.s[1:]
		maxDigits = 18
	}
	i := 0
	for i < len(sc.s) && i < maxDigits && sc.s[i] >= '0' && sc.s[i] <= '9' {
		i++
	}
	if i == 0 {
		return 0, false
	}
	v, err := strconv.Atoi(sc.s[:i])
	if err != nil {
		return 0, false
	}
	sc.s = sc.s[i:]
	if negative {
		v = -v
	}
	return v, true
}

func (sc *scanner) literal(c byte) bool {
	if sc.s == "" || sc.s[0] != c {
		return false
	}
	sc.s = sc.s[1:]
	return true
}

func (sc *scanner) date() (naive, bool) {
	var n naive
	var ok bool
	if n.year, ok = sc.number(4, true); !ok || !sc.literal('-') {
		return n, false
	}
	if n.month, ok = sc.number(2, false); !ok || !sc.literal('-') {
		return n, false
	}
	if n.day, ok = sc.number(2, false); !ok {
		return n, false
	}
	// chrono's NaiveDate covers roughly ±262143 years.
	return n, n.year >= -262143 && n.year <= 262142
}

func parseNaiveDate(s string) (naive, bool) {
	sc := scanner{s}
	n, ok := sc.date()
	return n, ok && sc.s == "" && n.valid()
}

// parseNaive parses `%Y-%m-%d{sep}%H:%M:%S`. Second 60 (a leap second)
// maps to second 59.
func parseNaive(s string, sep byte) (naive, bool) {
	sc := scanner{s}
	n, ok := sc.date()
	if !ok {
		return n, false
	}
	if sep == ' ' {
		// A space in a chrono format skips any whitespace.
		sc.s = strings.TrimLeftFunc(sc.s, unicode.IsSpace)
	} else if !sc.literal(sep) {
		return n, false
	}
	if n.hour, ok = sc.number(2, false); !ok || !sc.literal(':') {
		return n, false
	}
	if n.min, ok = sc.number(2, false); !ok || !sc.literal(':') {
		return n, false
	}
	if n.sec, ok = sc.number(2, false); !ok || sc.s != "" {
		return n, false
	}
	if n.sec == 60 {
		n.sec = 59
	}
	return n, n.valid()
}

// BucketKind names how message times are grouped.
type BucketKind int

const (
	// BucketPerMessage prints a full timestamp on every message.
	BucketPerMessage BucketKind = iota + 1
	// BucketNone prints no times.
	BucketNone
	// BucketMinute groups by N minutes (Bucket.N).
	BucketMinute
	// BucketHour groups by N hours (Bucket.N).
	BucketHour
	// BucketDay groups by local calendar day.
	BucketDay
	// BucketMonth groups by local calendar month.
	BucketMonth
	// BucketYear groups by local calendar year.
	BucketYear
)

// Bucket is a message time grouping. The zero value is invalid; build one
// with ParseBucket, Minutes, Hours or the predefined values.
type Bucket struct {
	Kind BucketKind
	N    int64 // count for BucketMinute and BucketHour; positive
}

// Predefined buckets.
var (
	PerMessage = Bucket{Kind: BucketPerMessage}
	NoTime     = Bucket{Kind: BucketNone}
	Day        = Bucket{Kind: BucketDay}
	Month      = Bucket{Kind: BucketMonth}
	Year       = Bucket{Kind: BucketYear}
)

// Minutes is a bucket of n minutes (n > 0).
func Minutes(n int64) Bucket { return Bucket{Kind: BucketMinute, N: n} }

// Hours is a bucket of n hours (n > 0).
func Hours(n int64) Bucket { return Bucket{Kind: BucketHour, N: n} }

// BucketValues lists the bucket spellings offered for shell completion.
var BucketValues = []string{"1m", "1min", "1h", "1d", "1mo", "1y", "full", "none"}

// ParseBucket parses a --time-bucket value: `full`/`always`, `none`/`off`,
// `N` + minute/hour units, or `1` + day/month/year units.
func ParseBucket(s string) (Bucket, error) {
	value := asciiLower(strings.TrimSpace(s))
	switch value {
	case "full", "always":
		return PerMessage, nil
	case "none", "off":
		return NoTime, nil
	}

	split := strings.IndexFunc(value, func(r rune) bool { return r < '0' || r > '9' })
	if split < 0 {
		return Bucket{}, fmt.Errorf("Missing time unit in '%s'", s)
	}
	numStr, unit := value[:split], value[split:]
	num, err := strconv.ParseInt(numStr, 10, 64)
	if err != nil {
		return Bucket{}, fmt.Errorf("Invalid number in '%s'", s)
	}
	if num <= 0 {
		return Bucket{}, fmt.Errorf("Time bucket must be positive in '%s'", s)
	}

	switch unit {
	case "m", "min", "mins", "minute", "minutes":
		return Minutes(num), nil
	case "h", "hour", "hours":
		return Hours(num), nil
	}
	var calendar Bucket
	switch unit {
	case "d", "day", "days":
		calendar = Day
	case "mo", "mon", "month", "months":
		calendar = Month
	case "y", "year", "years":
		calendar = Year
	default:
		return Bucket{}, fmt.Errorf("Unknown time bucket '%s'. Use 1m, 1min, 1h, 1d, 1mo, 1y, full, or none.", s)
	}
	if num != 1 {
		return Bucket{}, fmt.Errorf("Only 1%s is supported for calendar buckets.", unit)
	}
	return calendar, nil
}

func asciiLower(s string) string {
	b := []byte(s)
	for i, c := range b {
		if c >= 'A' && c <= 'Z' {
			b[i] = c + ('a' - 'A')
		}
	}
	return string(b)
}

func (b Bucket) mustValid() {
	switch b.Kind {
	case BucketMinute, BucketHour:
		if b.N <= 0 {
			panic(fmt.Sprintf("tgtime: bucket %d with non-positive count %d", b.Kind, b.N))
		}
	case BucketPerMessage, BucketNone, BucketDay, BucketMonth, BucketYear:
	default:
		panic(errors.New("tgtime: zero or unknown Bucket"))
	}
}

func format(ts int64, layout string, loc *time.Location) string {
	return time.Unix(ts, 0).In(loc).Format(layout)
}

// FormatLocal formats ts as `YYYY-MM-DD HH:MM:SS` in loc.
func FormatLocal(ts int64, loc *time.Location) string { return format(ts, dateTimeLayout, loc) }

// FormatLocalMinutes formats ts as `YYYY-MM-DD HH:MM` in loc.
func FormatLocalMinutes(ts int64, loc *time.Location) string { return format(ts, minuteLayout, loc) }

// FormatLocalDateAndMinute returns (`YYYY-MM-DD`, `HH:MM`) for layouts that
// print the date once per day.
func FormatLocalDateAndMinute(ts int64, loc *time.Location) (string, string) {
	return format(ts, dateLayout, loc), format(ts, "15:04", loc)
}

// ParseLocalMinutes parses `YYYY-MM-DD HH:MM` or `YYYY-MM-DD HH:MM:SS` as a
// local time in loc; false when it does not parse or does not exist.
func ParseLocalMinutes(s string, loc *time.Location) (int64, bool) {
	n, ok := parseNaiveMinutes(s)
	if !ok {
		n, ok = parseNaive(s, ' ')
	}
	if !ok {
		return 0, false
	}
	ts, err := localTimestamp(n, loc)
	return ts, err == nil
}

func parseNaiveMinutes(s string) (naive, bool) {
	sc := scanner{s}
	n, ok := sc.date()
	if !ok {
		return n, false
	}
	sc.s = strings.TrimLeftFunc(sc.s, unicode.IsSpace)
	if n.hour, ok = sc.number(2, false); !ok || !sc.literal(':') {
		return n, false
	}
	if n.min, ok = sc.number(2, false); !ok || sc.s != "" {
		return n, false
	}
	return n, n.valid()
}

// TimeKey is the group key of ts under b: messages with equal keys share a
// time header. False for BucketNone.
func TimeKey(ts int64, b Bucket, loc *time.Location) (int64, bool) {
	b.mustValid()
	switch b.Kind {
	case BucketPerMessage:
		return ts, true
	case BucketMinute:
		return floorDiv(ts, b.N*60), true
	case BucketHour:
		return floorDiv(ts, b.N*3600), true
	case BucketDay:
		t := time.Unix(ts, 0).In(loc)
		return int64(t.Year())*400 + int64(t.YearDay()), true
	case BucketMonth:
		t := time.Unix(ts, 0).In(loc)
		return int64(t.Year())*12 + int64(t.Month()) - 1, true
	case BucketYear:
		return int64(time.Unix(ts, 0).In(loc).Year()), true
	}
	return 0, false
}

// FormatBucket is the header text of ts's group under b; "" for BucketNone.
func FormatBucket(ts int64, b Bucket, loc *time.Location) string {
	b.mustValid()
	switch b.Kind {
	case BucketPerMessage:
		return FormatLocal(ts, loc)
	case BucketMinute:
		return format(floorDiv(ts, b.N*60)*b.N*60, minuteLayout, loc)
	case BucketHour:
		return format(floorDiv(ts, b.N*3600)*b.N*3600, hourLayout, loc)
	case BucketDay:
		return format(ts, dateLayout, loc)
	case BucketMonth:
		return format(ts, monthLayout, loc)
	case BucketYear:
		return format(ts, yearLayout, loc)
	}
	return ""
}

func floorDiv(a, b int64) int64 {
	q := a / b
	if a%b != 0 && (a < 0) != (b < 0) {
		q--
	}
	return q
}
