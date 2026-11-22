//go:build linux

package capture

import (
	"encoding/binary"
	"fmt"
	"strconv"
	"strings"

	"github.com/xiaotianxt/tg/internal/keys"
)

// captureState names the steps of the uprobe capture so the flow reads as an
// explicit state machine rather than a tangle of booleans, mirroring the macOS
// debugger flow.
type captureState int

const (
	// stateInit is the starting state before any kernel resource is held.
	stateInit captureState = iota
	// stateInstance means the dedicated trace instance directory exists.
	stateInstance
	// stateRegistered means the uprobe definition is in uprobe_events.
	stateRegistered
	// stateEnabled means the event is enabled and trace_pipe is being read.
	stateEnabled
	// stateCaptured means valid 32-byte material was read from a probe hit.
	stateCaptured
)

func (s captureState) String() string {
	switch s {
	case stateInit:
		return "init"
	case stateInstance:
		return "instance"
	case stateRegistered:
		return "registered"
	case stateEnabled:
		return "enabled"
	case stateCaptured:
		return "captured"
	default:
		return "unknown"
	}
}

// uprobeMaterialWords is the number of 64-bit words that span the 32-byte
// account material. Reading four fixed x64 fetch args and reassembling them is
// simpler to parse out of trace_pipe than a single array fetch arg whose text
// formatting varies between kernels.
const uprobeMaterialWords = capturedMaterialLen / 8

// uprobeRegisters returns the tracefs fetch-arg register names for the account
// material pointer (arg1) and its length (arg2) on the given architecture.
// x86_64 System V passes arg1 in rsi and arg2 in rdx; AArch64 AAPCS passes
// them in x1 and x2. The names are spelled as tracefs expects after a '%'.
func uprobeRegisters(arch captureArch) (ptrReg, lenReg string, ok bool) {
	switch arch {
	case archX8664:
		return "si", "dx", true
	case archAarch64:
		return "x1", "x2", true
	default:
		return "", "", false
	}
}

// buildFetchArgs builds the fetch-arg suffix of a uprobe definition: four
// 64-bit reads of the material buffer plus the length register. The pointer is
// dereferenced in kernel context at the probe hit, so the bytes are copied
// while they are still live.
func buildFetchArgs(ptrReg, lenReg string) string {
	var b strings.Builder
	for i := 0; i < uprobeMaterialWords; i++ {
		fmt.Fprintf(&b, "w%d=+%d(%%%s):x64 ", i, i*8, ptrReg)
	}
	fmt.Fprintf(&b, "len=%%%s:u64", lenReg)
	return b.String()
}

// loadSegment is one PT_LOAD program header, reduced to the fields needed to
// map a virtual address back to a file offset.
type loadSegment struct {
	vaddr  uint64
	offset uint64
	filesz uint64
}

// fileOffsetFromSegments converts a virtual address to a file offset using the
// loadable segments, the way the kernel maps an inode for a uprobe. Only the
// file-backed part of a segment (filesz) can carry a probe; an address in a
// segment's bss tail has no file offset.
func fileOffsetFromSegments(segments []loadSegment, va uint64) (uint64, error) {
	for _, s := range segments {
		if va < s.vaddr || va >= s.vaddr+s.filesz {
			continue
		}
		return s.offset + (va - s.vaddr), nil
	}
	return 0, fmt.Errorf("Virtual address %#x is not in any file-backed load segment", va)
}

// capturedHit is the parsed payload of one uprobe trace line.
type capturedHit struct {
	words  [uprobeMaterialWords]uint64
	length uint64
}

// material reassembles the 32-byte account material from the per-word reads.
// Both supported architectures are little-endian, so each 64-bit word is
// written back in little-endian order.
func (h capturedHit) material() *keys.Material {
	var b [capturedMaterialLen]byte
	for i, w := range h.words {
		binary.LittleEndian.PutUint64(b[i*8:], w)
	}
	return keys.MaterialFromBytes(b)
}

// parseTraceLine parses a single trace_pipe line emitted by the capture
// uprobe. It returns the hit and true only when every material word and the
// length field are present; other lines (headers, comments, unrelated events)
// return ok=false. It does not enforce the length here so the caller can
// distinguish a wrong-shaped hit from a parse failure.
func parseTraceLine(line string) (capturedHit, bool) {
	line = strings.TrimSpace(line)
	if line == "" || strings.HasPrefix(line, "#") {
		return capturedHit{}, false
	}
	var (
		hit     capturedHit
		seen    [uprobeMaterialWords]bool
		haveLen bool
	)
	for _, field := range strings.Fields(line) {
		key, value, ok := strings.Cut(field, "=")
		if !ok {
			continue
		}
		switch {
		case key == "len":
			v, err := parseHitUint(value)
			if err != nil {
				return capturedHit{}, false
			}
			hit.length = v
			haveLen = true
		case strings.HasPrefix(key, "w"):
			idx, err := strconv.Atoi(key[1:])
			if err != nil || idx < 0 || idx >= uprobeMaterialWords {
				continue
			}
			v, err := parseHitUint(value)
			if err != nil {
				return capturedHit{}, false
			}
			hit.words[idx] = v
			seen[idx] = true
		}
	}
	if !haveLen {
		return capturedHit{}, false
	}
	for _, s := range seen {
		if !s {
			return capturedHit{}, false
		}
	}
	return hit, true
}

// parseHitUint reads a fetch-arg value, which tracefs prints in hex with a 0x
// prefix but which older kernels may print in decimal.
func parseHitUint(s string) (uint64, error) {
	return strconv.ParseUint(strings.TrimPrefix(s, "0x"), hexOrDecBase(s), 64)
}

func hexOrDecBase(s string) int {
	if strings.HasPrefix(s, "0x") {
		return 16
	}
	return 10
}
