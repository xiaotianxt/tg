//go:build linux

package capture

import (
	"encoding/binary"
	"fmt"
)

// Minimal .eh_frame_hdr + FDE reader, enough to recover the exact bounds of
// the function containing an address: the start comes from the hdr binary
// search table, and the length comes from the covering FDE. Only the DWARF
// pointer encodings real GCC/Clang Linux binaries emit are handled; anything
// else is reported rather than guessed.

const (
	dwEhPeOmit = 0xff

	dwEhPeAbsptr = 0x00
	dwEhPeUleb   = 0x01
	dwEhPeUdata2 = 0x02
	dwEhPeUdata4 = 0x03
	dwEhPeUdata8 = 0x04
	dwEhPeSleb   = 0x09
	dwEhPeSdata2 = 0x0a
	dwEhPeSdata4 = 0x0b
	dwEhPeSdata8 = 0x0c

	dwEhPePcrel   = 0x10
	dwEhPeDatarel = 0x30
)

type unwindSections struct {
	ehFrameAddr    uint64
	ehFrame        []byte
	ehFrameHdrAddr uint64
	ehFrameHdr     []byte
	order          binary.ByteOrder
}

// functionRange returns [start, end) of the function containing address,
// resolved through the FDE that covers it.
func (u *unwindSections) functionRange(address uint64) (uint64, uint64, error) {
	hdr := u.ehFrameHdr
	if len(hdr) < 4 {
		return 0, 0, fmt.Errorf("ELF unwind header is too short")
	}
	if hdr[0] != 1 {
		return 0, 0, fmt.Errorf("Unsupported ELF unwind header version %d", hdr[0])
	}
	ptrEnc, countEnc, tableEnc := hdr[1], hdr[2], hdr[3]
	pos := 4
	if _, err := u.readEncoded(hdr, &pos, ptrEnc, u.ehFrameHdrAddr); err != nil {
		return 0, 0, fmt.Errorf("Cannot parse ELF unwind header: %w", err)
	}
	count, err := u.readEncoded(hdr, &pos, countEnc, u.ehFrameHdrAddr)
	if err != nil {
		return 0, 0, fmt.Errorf("Cannot parse ELF unwind header: %w", err)
	}
	if count == 0 {
		return 0, 0, fmt.Errorf("ELF unwind header has no FDE table")
	}
	stride, err := fixedSize(tableEnc)
	if err != nil {
		return 0, 0, fmt.Errorf("Unsupported ELF unwind table encoding: %w", err)
	}
	tableStart := pos
	entrySize := 2 * stride

	// Binary search for the entry with the largest initial location <= address.
	entryLoc := func(i int) (uint64, error) {
		p := tableStart + i*entrySize
		return u.readEncoded(hdr, &p, tableEnc, u.ehFrameHdrAddr)
	}
	lo, hi := 0, int(count)
	for lo < hi {
		mid := (lo + hi) / 2
		loc, err := entryLoc(mid)
		if err != nil {
			return 0, 0, err
		}
		if loc <= address {
			lo = mid + 1
		} else {
			hi = mid
		}
	}
	if lo == 0 {
		return 0, 0, fmt.Errorf("Cannot resolve function boundary from ELF unwind data")
	}
	entryIdx := lo - 1
	p := tableStart + entryIdx*entrySize + stride
	fdePtr, err := u.readEncoded(hdr, &p, tableEnc, u.ehFrameHdrAddr)
	if err != nil {
		return 0, 0, err
	}
	return u.parseFDE(fdePtr, address)
}

// parseFDE reads the FDE at the given virtual address and returns the function
// bounds, failing if the FDE does not actually cover address.
func (u *unwindSections) parseFDE(fdeAddr, address uint64) (uint64, uint64, error) {
	if fdeAddr < u.ehFrameAddr {
		return 0, 0, fmt.Errorf("FDE pointer precedes .eh_frame")
	}
	off := int(fdeAddr - u.ehFrameAddr)
	eh := u.ehFrame
	length, next, err := u.readInitialLength(eh, off)
	if err != nil {
		return 0, 0, fmt.Errorf("Cannot read FDE length: %w", err)
	}
	_ = length
	ciePtrOff := next
	if ciePtrOff+4 > len(eh) {
		return 0, 0, fmt.Errorf("Truncated FDE CIE pointer")
	}
	ciePtr := u.order.Uint32(eh[ciePtrOff : ciePtrOff+4])
	if ciePtr == 0 {
		return 0, 0, fmt.Errorf("Expected an FDE but found a CIE")
	}
	cieOff := ciePtrOff - int(ciePtr)
	fdeEnc, err := u.cieFDEEncoding(cieOff)
	if err != nil {
		return 0, 0, err
	}
	pos := ciePtrOff + 4
	locFieldVaddr := u.ehFrameAddr + uint64(pos)
	start, err := u.readEncodedAt(eh, &pos, fdeEnc, locFieldVaddr, u.ehFrameAddr)
	if err != nil {
		return 0, 0, fmt.Errorf("Cannot read FDE initial location: %w", err)
	}
	// The address range is a length: read with the encoding's format, no base.
	rng, err := u.readValue(eh, &pos, fdeEnc)
	if err != nil {
		return 0, 0, fmt.Errorf("Cannot read FDE address range: %w", err)
	}
	end := start + rng
	if address < start || address >= end {
		return 0, 0, fmt.Errorf("No FDE covers address %#x", address)
	}
	return start, end, nil
}

// cieFDEEncoding parses a CIE far enough to return the pointer encoding its
// FDEs use (the 'R' augmentation byte), defaulting to absptr when absent.
func (u *unwindSections) cieFDEEncoding(off int) (byte, error) {
	eh := u.ehFrame
	_, next, err := u.readInitialLength(eh, off)
	if err != nil {
		return 0, fmt.Errorf("Cannot read CIE length: %w", err)
	}
	pos := next
	if pos+4 > len(eh) {
		return 0, fmt.Errorf("Truncated CIE id")
	}
	if u.order.Uint32(eh[pos:pos+4]) != 0 {
		return 0, fmt.Errorf("CIE id is not zero")
	}
	pos += 4
	if pos >= len(eh) {
		return 0, fmt.Errorf("Truncated CIE version")
	}
	version := eh[pos]
	pos++
	aug, err := readCString(eh, &pos)
	if err != nil {
		return 0, err
	}
	if version >= 4 {
		pos += 2 // address_size, segment_size
	}
	if _, err := readULEB(eh, &pos); err != nil { // code alignment factor
		return 0, err
	}
	if _, err := readSLEB(eh, &pos); err != nil { // data alignment factor
		return 0, err
	}
	if version == 1 { // return address register
		pos++
	} else if _, err := readULEB(eh, &pos); err != nil {
		return 0, err
	}
	if len(aug) == 0 || aug[0] != 'z' {
		return dwEhPeAbsptr, nil
	}
	if _, err := readULEB(eh, &pos); err != nil { // augmentation data length
		return 0, err
	}
	for _, c := range aug[1:] {
		switch c {
		case 'R':
			if pos >= len(eh) {
				return 0, fmt.Errorf("Truncated CIE augmentation data")
			}
			return eh[pos], nil
		case 'L':
			pos++ // LSDA encoding
		case 'P':
			if pos >= len(eh) {
				return 0, fmt.Errorf("Truncated CIE personality encoding")
			}
			penc := eh[pos]
			pos++
			sz, err := fixedSize(penc)
			if err != nil {
				return 0, err
			}
			pos += sz
		default:
			return 0, fmt.Errorf("Unsupported CIE augmentation %q", c)
		}
	}
	return dwEhPeAbsptr, nil
}

// readInitialLength reads a DWARF initial length, returning the content length
// and the offset just past the length field. 64-bit DWARF is reported, not
// supported, since Linux .eh_frame uses 32-bit lengths.
func (u *unwindSections) readInitialLength(data []byte, off int) (uint64, int, error) {
	if off < 0 || off+4 > len(data) {
		return 0, 0, fmt.Errorf("truncated length field")
	}
	l := u.order.Uint32(data[off : off+4])
	if l == 0xffffffff {
		return 0, 0, fmt.Errorf("64-bit DWARF .eh_frame is not supported")
	}
	return uint64(l), off + 4, nil
}

// readEncoded reads a DWARF-encoded pointer whose relative base is the value's
// own location for pcrel and sectionAddr for datarel.
func (u *unwindSections) readEncoded(data []byte, pos *int, enc byte, sectionAddr uint64) (uint64, error) {
	locVaddr := sectionAddr + uint64(*pos)
	return u.readEncodedAt(data, pos, enc, locVaddr, sectionAddr)
}

func (u *unwindSections) readEncodedAt(data []byte, pos *int, enc byte, locVaddr, dataBase uint64) (uint64, error) {
	if enc == dwEhPeOmit {
		return 0, fmt.Errorf("value is omitted")
	}
	value, err := u.readValue(data, pos, enc)
	if err != nil {
		return 0, err
	}
	switch enc & 0x70 {
	case 0x00: // absolute
		return value, nil
	case dwEhPePcrel:
		return locVaddr + value, nil
	case dwEhPeDatarel:
		return dataBase + value, nil
	default:
		return 0, fmt.Errorf("unsupported pointer application %#x", enc&0x70)
	}
}

// readValue reads just the value bits of a DWARF pointer encoding, advancing
// pos, with signed formats sign-extended into the returned uint64.
func (u *unwindSections) readValue(data []byte, pos *int, enc byte) (uint64, error) {
	read := func(n int) ([]byte, error) {
		if *pos+n > len(data) {
			return nil, fmt.Errorf("truncated encoded value")
		}
		b := data[*pos : *pos+n]
		*pos += n
		return b, nil
	}
	switch enc & 0x0f {
	case dwEhPeAbsptr, dwEhPeUdata8:
		b, err := read(8)
		if err != nil {
			return 0, err
		}
		return u.order.Uint64(b), nil
	case dwEhPeUdata2:
		b, err := read(2)
		if err != nil {
			return 0, err
		}
		return uint64(u.order.Uint16(b)), nil
	case dwEhPeUdata4:
		b, err := read(4)
		if err != nil {
			return 0, err
		}
		return uint64(u.order.Uint32(b)), nil
	case dwEhPeSdata2:
		b, err := read(2)
		if err != nil {
			return 0, err
		}
		return uint64(int64(int16(u.order.Uint16(b)))), nil
	case dwEhPeSdata4:
		b, err := read(4)
		if err != nil {
			return 0, err
		}
		return uint64(int64(int32(u.order.Uint32(b)))), nil
	case dwEhPeSdata8:
		b, err := read(8)
		if err != nil {
			return 0, err
		}
		return u.order.Uint64(b), nil
	case dwEhPeUleb:
		return readULEB(data, pos)
	case dwEhPeSleb:
		v, err := readSLEB(data, pos)
		return uint64(v), err
	default:
		return 0, fmt.Errorf("unsupported pointer format %#x", enc&0x0f)
	}
}

// fixedSize is the byte width of a fixed-size DWARF pointer format (the table
// and length-less encodings); variable-length formats are rejected.
func fixedSize(enc byte) (int, error) {
	switch enc & 0x0f {
	case dwEhPeAbsptr, dwEhPeUdata8, dwEhPeSdata8:
		return 8, nil
	case dwEhPeUdata4, dwEhPeSdata4:
		return 4, nil
	case dwEhPeUdata2, dwEhPeSdata2:
		return 2, nil
	default:
		return 0, fmt.Errorf("encoding %#x is not fixed-size", enc)
	}
}

func readCString(data []byte, pos *int) (string, error) {
	start := *pos
	for *pos < len(data) {
		if data[*pos] == 0 {
			s := string(data[start:*pos])
			*pos++
			return s, nil
		}
		*pos++
	}
	return "", fmt.Errorf("unterminated string")
}

func readULEB(data []byte, pos *int) (uint64, error) {
	var result uint64
	var shift uint
	for {
		if *pos >= len(data) {
			return 0, fmt.Errorf("truncated ULEB128")
		}
		b := data[*pos]
		*pos++
		result |= uint64(b&0x7f) << shift
		if b&0x80 == 0 {
			return result, nil
		}
		shift += 7
		if shift >= 64 {
			return 0, fmt.Errorf("ULEB128 overflow")
		}
	}
}

func readSLEB(data []byte, pos *int) (int64, error) {
	var result int64
	var shift uint
	for {
		if *pos >= len(data) {
			return 0, fmt.Errorf("truncated SLEB128")
		}
		b := data[*pos]
		*pos++
		result |= int64(b&0x7f) << shift
		shift += 7
		if b&0x80 == 0 {
			if shift < 64 && b&0x40 != 0 {
				result |= -1 << shift
			}
			return result, nil
		}
		if shift >= 64 {
			return 0, fmt.Errorf("SLEB128 overflow")
		}
	}
}
