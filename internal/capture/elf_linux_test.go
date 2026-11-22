//go:build linux

package capture

import (
	"os"
	"strconv"
	"strings"
	"testing"
)

func TestDecoderResolvesRealAarch64AddressMaterialization(t *testing.T) {
	text := &sectionData{
		address: 0x064b_0a4c,
		data: []byte{
			0xc8, 0xe2, 0x00, 0x90, // adrp x8, slot page
			0x40, 0x50, 0xfd, 0xd0, // adrp x0, anchor page
			0x00, 0xf4, 0x12, 0x91, // add x0, x0, #0x4bd
			0x08, 0xf5, 0x47, 0xf9, // ldr x8, [x8, #0xfe8]
		},
	}

	anchorReg, _, ok := decodeADRP(text, 1)
	if !ok || anchorReg != 0 {
		t.Fatalf("decodeADRP(1) = (%d, %v), want register 0", anchorReg, ok)
	}

	enc, _ := rawInstruction(text.data, 2)
	dst, base, imm, ok := decodeAddImmediate(enc)
	if !ok || dst != 0 || base != 0 || imm != 0x4bd {
		t.Fatalf("decodeAddImmediate = (%d,%d,%#x,%v), want (0,0,0x4bd,true)", dst, base, imm, ok)
	}

	enc, _ = rawInstruction(text.data, 3)
	if _, _, _, ok := decodeLdrU64(enc); !ok {
		t.Fatal("decodeLdrU64 should decode the fixture ldr")
	}
}

func TestSingleHookRejectsAmbiguity(t *testing.T) {
	_, err := singleHook(map[uint64]bool{0x1000: true, 0x2000: true}, "AArch64")
	if err == nil || !strings.Contains(err.Error(), "ambiguous") {
		t.Fatalf("singleHook ambiguity error = %v", err)
	}
}

func TestSingleHookEmptyAndUnique(t *testing.T) {
	if _, err := singleHook(map[uint64]bool{}, "x86_64"); err == nil {
		t.Fatal("empty candidate set must error")
	}
	addr, err := singleHook(map[uint64]bool{0x4000: true}, "x86_64")
	if err != nil || addr != 0x4000 {
		t.Fatalf("singleHook unique = (%#x, %v)", addr, err)
	}
}

// TestResolvesExternalClientELF is an opt-in integration check against a real
// client binary. It runs only when TG_TEST_CLIENT_ELF names one.
func TestResolvesExternalClientELF(t *testing.T) {
	path := os.Getenv("TG_TEST_CLIENT_ELF")
	if path == "" {
		t.Skip("set TG_TEST_CLIENT_ELF to run the hook-location integration test")
	}
	hook, err := findHook(path)
	if err != nil {
		t.Fatal(err)
	}
	if expected := os.Getenv("TG_TEST_EXPECTED_HOOK"); expected != "" {
		want, err := strconv.ParseUint(strings.TrimPrefix(expected, "0x"), 16, 64)
		if err != nil {
			t.Fatal(err)
		}
		if hook.virtualAddress != want {
			t.Fatalf("hook virtual address = %#x, want %#x", hook.virtualAddress, want)
		}
	}
}
