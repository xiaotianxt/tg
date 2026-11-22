//go:build linux

package capture

import "testing"

func TestFileOffsetFromSegments(t *testing.T) {
	segments := []loadSegment{
		{vaddr: 0x0, offset: 0x0, filesz: 0x1000},       // headers
		{vaddr: 0x2000, offset: 0x1000, filesz: 0x4000}, // .text, offset shifted by page alignment
	}
	off, err := fileOffsetFromSegments(segments, 0x2abc)
	if err != nil || off != 0x1abc {
		t.Fatalf("fileOffsetFromSegments(0x2abc) = (%#x, %v), want (0x1abc, nil)", off, err)
	}
	// An address inside a segment's file-backed range at its very start.
	if off, err := fileOffsetFromSegments(segments, 0x2000); err != nil || off != 0x1000 {
		t.Fatalf("segment start = (%#x, %v), want (0x1000, nil)", off, err)
	}
	// An address past every file-backed range (e.g. .bss tail) has no offset.
	if _, err := fileOffsetFromSegments(segments, 0x9000); err == nil {
		t.Fatal("address outside all segments must error")
	}
}

func TestUprobeRegistersPerArch(t *testing.T) {
	ptr, length, ok := uprobeRegisters(archX8664)
	if !ok || ptr != "si" || length != "dx" {
		t.Fatalf("x86_64 regs = (%q, %q, %v), want (si, dx, true)", ptr, length, ok)
	}
	ptr, length, ok = uprobeRegisters(archAarch64)
	if !ok || ptr != "x1" || length != "x2" {
		t.Fatalf("aarch64 regs = (%q, %q, %v), want (x1, x2, true)", ptr, length, ok)
	}
}

func TestBuildFetchArgs(t *testing.T) {
	got := buildFetchArgs("si", "dx")
	want := "w0=+0(%si):x64 w1=+8(%si):x64 w2=+16(%si):x64 w3=+24(%si):x64 len=%dx:u64"
	if got != want {
		t.Fatalf("buildFetchArgs =\n  %q\nwant\n  %q", got, want)
	}
}

func TestParseTraceLineCapturesMaterial(t *testing.T) {
	line := "    Telegram-12345  [001] d...1  1234.567890: m_12345_7: (0x7f0012340000) " +
		"w0=0x0807060504030201 w1=0x100f0e0d0c0b0a09 w2=0x1817161514131211 w3=0x201f1e1d1c1b1a19 len=0x20"
	hit, ok := parseTraceLine(line)
	if !ok {
		t.Fatal("expected the capture line to parse")
	}
	if hit.length != 0x20 {
		t.Fatalf("length = %#x, want 0x20", hit.length)
	}
	material := hit.material()
	defer material.Wipe()
	got := material.Bytes()
	for i := 0; i < capturedMaterialLen; i++ {
		if got[i] != byte(i+1) {
			t.Fatalf("material[%d] = %#x, want %#x", i, got[i], i+1)
		}
	}
}

func TestParseTraceLineWrongShapeKeepsLength(t *testing.T) {
	line := "app-9 [000] .... 9.9: m_9_1: (0x1) w0=0x1 w1=0x2 w2=0x3 w3=0x4 len=0x10"
	hit, ok := parseTraceLine(line)
	if !ok {
		t.Fatal("a fully-formed line with a non-matching length must still parse")
	}
	if hit.length != 0x10 {
		t.Fatalf("length = %#x, want 0x10", hit.length)
	}
}

func TestParseTraceLineRejectsIncompleteAndComments(t *testing.T) {
	cases := []string{
		"",
		"# tracer: nop",
		"app-9 [000] .... 9.9: m: (0x1) w0=0x1 w1=0x2 w3=0x4 len=0x20", // missing w2
		"app-9 [000] .... 9.9: m: (0x1) w0=0x1 w1=0x2 w2=0x3 w3=0x4",   // missing len
	}
	for _, c := range cases {
		if _, ok := parseTraceLine(c); ok {
			t.Fatalf("line %q must not parse as a hit", c)
		}
	}
}

func TestParseTraceLineAcceptsDecimalLength(t *testing.T) {
	line := "app-9 [000] .... 9.9: m: (0x1) w0=0x1 w1=0x2 w2=0x3 w3=0x4 len=32"
	hit, ok := parseTraceLine(line)
	if !ok || hit.length != 32 {
		t.Fatalf("decimal length parse = (%v, %d), want (true, 32)", ok, hit.length)
	}
}

func TestCaptureStateString(t *testing.T) {
	for state, want := range map[captureState]string{
		stateInit:       "init",
		stateInstance:   "instance",
		stateRegistered: "registered",
		stateEnabled:    "enabled",
		stateCaptured:   "captured",
	} {
		if got := state.String(); got != want {
			t.Fatalf("state %d string = %q, want %q", state, got, want)
		}
	}
}
