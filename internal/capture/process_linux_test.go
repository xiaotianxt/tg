//go:build linux

package capture

import (
	"fmt"
	"testing"
)

func TestParseProcStatHandlesSpacesAndParensInCommand(t *testing.T) {
	// state field, then ppid=42, then fields up to start time (field 22) = 987654.
	tail := []string{"S", "42"}
	for i := 5; i <= 21; i++ {
		tail = append(tail, fmt.Sprint(i))
	}
	tail = append(tail, "987654")
	stat := "123 (desktop (main))"
	for _, f := range tail {
		stat += " " + f
	}
	ppid, start, err := parseProcStat(stat)
	if err != nil {
		t.Fatal(err)
	}
	if ppid != 42 || start != 987654 {
		t.Fatalf("parseProcStat = (%d, %d), want (42, 987654)", ppid, start)
	}
}

func TestParseStatusUIDUsesRealUID(t *testing.T) {
	uid, ok := parseStatusUID("Name:\tapp\nUid:\t1000\t1000\t1000\t1000\n")
	if !ok || uid != 1000 {
		t.Fatalf("parseStatusUID = (%d, %v), want (1000, true)", uid, ok)
	}
}
