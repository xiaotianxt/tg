//go:build linux

package capture

import (
	"fmt"

	"golang.org/x/arch/arm64/arm64asm"

	"github.com/xiaotianxt/tg/internal/dict"
)

// ---- x86_64 -----------------------------------------------------------------

func findX8664Hook(text, rodata *sectionData, relocations map[uint64]bool, unwind *unwindSections) (uint64, error) {
	candidates := map[uint64]bool{}
	for _, anchorOffset := range findBytes(rodata.data, dict.CipherConfigAnchor) {
		anchorAddress := rodata.address + uint64(anchorOffset)
		for _, firstRef := range findX86RIPRefs(text, x8664LeaRSI, anchorAddress) {
			if firstRef < 7 || !bytesEqual(text.data, firstRef-7, x8664LeaRDI[:]) {
				continue
			}
			disp, ok := readI32(text.data, firstRef-4)
			if !ok {
				continue
			}
			globalAddress, ok := addSigned(text.address+uint64(firstRef), int64(disp))
			if !ok || !relocations[globalAddress] {
				continue
			}
			anchorStart, _, err := unwind.functionRange(text.address + uint64(firstRef))
			if err != nil {
				return 0, err
			}
			for _, secondRef := range findX86RIPRefs(text, x8664LeaRSI, globalAddress) {
				address := text.address + uint64(secondRef)
				start, _, err := unwind.functionRange(address)
				if err != nil {
					return 0, err
				}
				if start != anchorStart {
					candidates[start] = true
				}
			}
		}
	}
	return singleHook(candidates, "x86_64")
}

func findX86RIPRefs(text *sectionData, opcode [3]byte, target uint64) []int {
	var out []int
	if len(text.data) < 7 {
		return out
	}
	for offset := 0; offset <= len(text.data)-7; offset++ {
		if !bytesEqual(text.data, offset, opcode[:]) {
			continue
		}
		disp, ok := readI32(text.data, offset+3)
		if !ok {
			continue
		}
		addr, ok := addSigned(text.address+uint64(offset)+7, int64(disp))
		if ok && addr == target {
			out = append(out, offset)
		}
	}
	return out
}

// ---- AArch64 ----------------------------------------------------------------

const (
	aarch64ADRPClassMask uint32 = 0x9f000000
	aarch64ADRPClass     uint32 = 0x90000000
)

func findAarch64Hook(text, rodata *sectionData, relocations map[uint64]bool, unwind *unwindSections) (uint64, error) {
	adrpIndices := aarch64ADRPIndices(text.data)
	candidates := map[uint64]bool{}
	for _, anchorOffset := range findBytes(rodata.data, dict.CipherConfigAnchor) {
		anchorAddress := rodata.address + uint64(anchorOffset)
		for _, anchorRef := range aarch64ADRPAddRefs(text, adrpIndices, anchorAddress) {
			start, end, err := unwind.functionRange(instructionAddress(text, anchorRef))
			if err != nil {
				return 0, err
			}
			block, err := aarch64BasicBlock(text, start, end, anchorRef)
			if err != nil {
				return 0, err
			}
			for _, slot := range aarch64LoadTargetsInBlock(text, adrpIndices, block) {
				if !relocations[slot] {
					continue
				}
				for _, slotRef := range aarch64ADRPLdrRefs(text, adrpIndices, slot) {
					fStart, _, err := unwind.functionRange(instructionAddress(text, slotRef))
					if err != nil {
						return 0, err
					}
					if fStart != start {
						candidates[fStart] = true
					}
				}
			}
		}
	}
	return singleHook(candidates, "AArch64")
}

func aarch64ADRPIndices(text []byte) []int {
	var out []int
	for i := 0; i < len(text)/4; i++ {
		if word, ok := rawInstruction(text, i); ok && word&aarch64ADRPClassMask == aarch64ADRPClass {
			out = append(out, i)
		}
	}
	return out
}

func aarch64ADRPAddRefs(text *sectionData, adrpIndices []int, target uint64) []int {
	var out []int
	for _, index := range adrpIndices {
		register, page, ok := decodeADRP(text, index)
		if !ok {
			continue
		}
		matched := false
		for _, addIndex := range followingBasicBlockIndices(text.data, index) {
			inst, enc, ok := decodeInstr(text.data, addIndex)
			if !ok {
				break
			}
			if dst, base, imm, ok := decodeAddImmediate(enc); ok && dst == register && base == register {
				if v, ok := addSigned(page, imm); ok && v == target {
					matched = true
					break
				}
			}
			if instructionWritesRegister(inst, register) {
				break
			}
		}
		if matched {
			out = append(out, index)
		}
	}
	return out
}

func aarch64LoadTargetsInBlock(text *sectionData, adrpIndices []int, block [2]int) []uint64 {
	seen := map[uint64]bool{}
	var out []uint64
	for _, index := range adrpIndices {
		if index < block[0] || index >= block[1] {
			continue
		}
		for _, t := range aarch64LoadTargetsFrom(text, index) {
			if !seen[t] {
				seen[t] = true
				out = append(out, t)
			}
		}
	}
	return out
}

func aarch64ADRPLdrRefs(text *sectionData, adrpIndices []int, target uint64) []int {
	var out []int
	for _, index := range adrpIndices {
		for _, t := range aarch64LoadTargetsFrom(text, index) {
			if t == target {
				out = append(out, index)
				break
			}
		}
	}
	return out
}

func aarch64LoadTargetsFrom(text *sectionData, adrpIndex int) []uint64 {
	register, page, ok := decodeADRP(text, adrpIndex)
	if !ok {
		return nil
	}
	for _, index := range followingBasicBlockIndices(text.data, adrpIndex) {
		inst, enc, ok := decodeInstr(text.data, index)
		if !ok {
			return nil
		}
		if _, base, disp, ok := decodeLdrU64(enc); ok && base == register {
			if v, ok := addSigned(page, disp); ok {
				return []uint64{v}
			}
			return nil
		}
		if instructionWritesRegister(inst, register) {
			return nil
		}
	}
	return nil
}

func decodeADRP(text *sectionData, index int) (register int, page uint64, ok bool) {
	enc, ok := rawInstruction(text.data, index)
	if !ok || enc&aarch64ADRPClassMask != aarch64ADRPClass {
		return 0, 0, false
	}
	immlo := (enc >> 29) & 0x3
	immhi := (enc >> 5) & 0x7ffff
	imm := int64(signExtend((immhi<<2)|immlo, 21)) << 12
	pcPage := instructionAddress(text, index) &^ 0xfff
	v, ok := addSigned(pcPage, imm)
	if !ok {
		return 0, 0, false
	}
	return int(enc & 0x1f), v, true
}

// decodeAddImmediate decodes a 64-bit ADD (immediate): sf=1, op=0, S=0.
func decodeAddImmediate(enc uint32) (dst, base int, imm int64, ok bool) {
	if enc&0xff800000 != 0x91000000 {
		return 0, 0, 0, false
	}
	imm12 := int64((enc >> 10) & 0xfff)
	if (enc>>22)&1 == 1 {
		imm12 <<= 12
	}
	return int(enc & 0x1f), int((enc >> 5) & 0x1f), imm12, true
}

// decodeLdrU64 decodes LDR (immediate, unsigned offset, 64-bit).
func decodeLdrU64(enc uint32) (dst, base int, disp int64, ok bool) {
	if enc&0xffc00000 != 0xf9400000 {
		return 0, 0, 0, false
	}
	disp = int64((enc>>10)&0xfff) * 8
	return int(enc & 0x1f), int((enc >> 5) & 0x1f), disp, true
}

func instructionWritesRegister(inst arm64asm.Inst, register int) bool {
	if isStoreOp(inst.Op) || isControlFlowOp(inst.Op) {
		return false
	}
	if num, ok := regRawNum(inst.Args[0]); ok {
		return num == register
	}
	return false
}

func isStoreOp(op arm64asm.Op) bool {
	switch op {
	case arm64asm.STLR, arm64asm.STLRB, arm64asm.STLRH,
		arm64asm.STLXP, arm64asm.STLXR, arm64asm.STLXRB, arm64asm.STLXRH,
		arm64asm.STP, arm64asm.STR, arm64asm.STTR, arm64asm.STTRB, arm64asm.STTRH,
		arm64asm.STRB, arm64asm.STRH, arm64asm.STUR, arm64asm.STURB, arm64asm.STURH,
		arm64asm.STXP, arm64asm.STXR, arm64asm.STXRB, arm64asm.STXRH:
		return true
	}
	return false
}

func isControlFlowOp(op arm64asm.Op) bool {
	switch op {
	case arm64asm.B, arm64asm.BR, arm64asm.BL, arm64asm.BLR,
		arm64asm.RET, arm64asm.ERET, arm64asm.DRPS,
		arm64asm.CBZ, arm64asm.CBNZ, arm64asm.TBZ, arm64asm.TBNZ:
		return true
	}
	return false
}

func regRawNum(arg arm64asm.Arg) (int, bool) {
	switch v := arg.(type) {
	case arm64asm.Reg:
		return regToNum(v)
	case arm64asm.RegSP:
		return regToNum(arm64asm.Reg(v))
	}
	return 0, false
}

func regToNum(r arm64asm.Reg) (int, bool) {
	switch {
	case r == arm64asm.WZR || r == arm64asm.XZR:
		return 31, true
	case r >= arm64asm.W0 && r <= arm64asm.W30:
		return int(r - arm64asm.W0), true
	case r >= arm64asm.X0 && r <= arm64asm.X30:
		return int(r - arm64asm.X0), true
	}
	return 0, false
}

// decodeInstr decodes the AArch64 instruction at a 4-byte index, returning the
// decoded form, the raw word, and ok=false when the word does not decode.
func decodeInstr(text []byte, index int) (arm64asm.Inst, uint32, bool) {
	enc, ok := rawInstruction(text, index)
	if !ok {
		return arm64asm.Inst{}, 0, false
	}
	inst, err := arm64asm.Decode(text[index*4 : index*4+4])
	if err != nil {
		return arm64asm.Inst{}, enc, false
	}
	return inst, enc, true
}

func followingBasicBlockIndices(text []byte, index int) []int {
	var out []int
	for i := index + 1; i < len(text)/4; i++ {
		if isControlFlow(text, i) {
			break
		}
		out = append(out, i)
	}
	return out
}

func aarch64BasicBlock(text *sectionData, funcStart, funcEnd uint64, containing int) ([2]int, error) {
	startIdx, err := addressToInstructionIndex(text, funcStart)
	if err != nil {
		return [2]int{}, err
	}
	endIdx, err := addressToInstructionIndex(text, funcEnd)
	if err != nil {
		return [2]int{}, err
	}
	if containing < startIdx || containing >= endIdx {
		return [2]int{}, fmt.Errorf("Anchor reference falls outside its unwind function")
	}
	start := startIdx
	for i := containing - 1; i >= startIdx; i-- {
		if isControlFlow(text.data, i) {
			start = i + 1
			break
		}
	}
	end := endIdx
	for i := containing; i < endIdx; i++ {
		if isControlFlow(text.data, i) {
			end = i
			break
		}
	}
	return [2]int{start, end}, nil
}

func addressToInstructionIndex(text *sectionData, address uint64) (int, error) {
	if address < text.address {
		return 0, fmt.Errorf("Unwind function precedes the text section")
	}
	relative := address - text.address
	if relative%4 != 0 || relative > uint64(len(text.data)) {
		return 0, fmt.Errorf("Unwind function boundary is not an AArch64 text address")
	}
	return int(relative / 4), nil
}

func isControlFlow(text []byte, index int) bool {
	inst, _, ok := decodeInstr(text, index)
	if !ok {
		return true
	}
	return isControlFlowOp(inst.Op)
}

func instructionAddress(text *sectionData, index int) uint64 {
	return text.address + uint64(index*4)
}

func rawInstruction(text []byte, index int) (uint32, bool) {
	offset := index * 4
	if offset < 0 || offset+4 > len(text) {
		return 0, false
	}
	return uint32(text[offset]) | uint32(text[offset+1])<<8 |
		uint32(text[offset+2])<<16 | uint32(text[offset+3])<<24, true
}

// ---- shared -----------------------------------------------------------------

func singleHook(candidates map[uint64]bool, architecture string) (uint64, error) {
	switch len(candidates) {
	case 1:
		for addr := range candidates {
			return addr, nil
		}
	case 0:
		return 0, fmt.Errorf(
			"Cannot locate the %s account-material hook from the cipher configuration anchor", architecture)
	}
	return 0, fmt.Errorf(
		"Found %d possible %s account-material hooks; refusing an ambiguous capture", len(candidates), architecture)
}

func findBytes(haystack, needle []byte) []int {
	var out []int
	if len(needle) == 0 || len(haystack) < len(needle) {
		return out
	}
	for offset := 0; offset+len(needle) <= len(haystack); offset++ {
		if bytesEqual(haystack, offset, needle) {
			out = append(out, offset)
		}
	}
	return out
}

func bytesEqual(data []byte, offset int, needle []byte) bool {
	if offset < 0 || offset+len(needle) > len(data) {
		return false
	}
	for i, b := range needle {
		if data[offset+i] != b {
			return false
		}
	}
	return true
}

func addSigned(base uint64, displacement int64) (uint64, bool) {
	if displacement >= 0 {
		v := base + uint64(displacement)
		if v < base {
			return 0, false
		}
		return v, true
	}
	d := uint64(-displacement)
	if d > base {
		return 0, false
	}
	return base - d, true
}

func readI32(data []byte, offset int) (int32, bool) {
	if offset < 0 || offset+4 > len(data) {
		return 0, false
	}
	return int32(uint32(data[offset]) | uint32(data[offset+1])<<8 |
		uint32(data[offset+2])<<16 | uint32(data[offset+3])<<24), true
}

func signExtend(value uint32, bits uint) int32 {
	shift := 32 - bits
	return int32(value<<shift) >> shift
}
