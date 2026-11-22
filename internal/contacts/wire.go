package contacts

import "unicode/utf8"

// A minimal protobuf wire reader for the chat_room ext buffer. It mirrors
// the Rust reader: varints of any length up to 64 bits, and a failed read
// leaves the caller to stop or skip.

func varint(data []byte, pos *int) (uint64, bool) {
	var v uint64
	for shift := uint(0); shift < 64; shift += 7 {
		if *pos >= len(data) {
			return 0, false
		}
		b := data[*pos]
		*pos++
		v |= uint64(b&0x7f) << shift
		if b&0x80 == 0 {
			return v, true
		}
	}
	return 0, false
}

func lenBytes(data []byte, pos *int) ([]byte, bool) {
	n, ok := varint(data, pos)
	if !ok || n > uint64(len(data)-*pos) {
		return nil, false
	}
	b := data[*pos : *pos+int(n)]
	*pos += int(n)
	return b, true
}

func skip(data []byte, pos *int, wire byte) bool {
	switch wire {
	case 0:
		_, ok := varint(data, pos)
		return ok
	case 2:
		_, ok := lenBytes(data, pos)
		return ok
	case 1, 5:
		n := 8
		if wire == 5 {
			n = 4
		}
		if len(data)-*pos < n {
			return false
		}
		*pos += n
		return true
	default:
		return false
	}
}

func utf8Valid(b []byte) bool { return utf8.Valid(b) }
