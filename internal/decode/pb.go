package decode

import "unicode/utf8"

// Minimal protobuf wire reader for packed media metadata. Each reader takes
// the buffer and a cursor; on failure the cursor stays wherever the failed
// read left it, as in the Rust reader, because some callers keep going.

const (
	wireVarint = 0
	wire64Bit  = 1
	wireLen    = 2
	wire32Bit  = 5
)

func readVarint(data []byte, pos *int) (uint64, bool) {
	var result uint64
	var shift uint
	for {
		if *pos >= len(data) {
			return 0, false
		}
		b := data[*pos]
		*pos++
		result |= uint64(b&0x7f) << shift
		shift += 7
		if b&0x80 == 0 {
			return result, true
		}
	}
}

func tagField(tag uint64) (uint32, byte) {
	return uint32(tag >> 3), byte(tag & 0x07)
}

// span checks that n bytes follow *pos and returns the end offset.
func span(data []byte, pos int, n uint64) (int, bool) {
	if n > uint64(len(data)-pos) {
		return 0, false
	}
	return pos + int(n), true
}

func skipField(data []byte, pos *int, wire byte) bool {
	var n uint64
	switch wire {
	case wireVarint:
		_, ok := readVarint(data, pos)
		return ok
	case wireLen:
		l, ok := readVarint(data, pos)
		if !ok {
			return false
		}
		n = l
	case wire64Bit:
		n = 8
	case wire32Bit:
		n = 4
	default:
		return false
	}
	end, ok := span(data, *pos, n)
	if ok {
		*pos = end
	}
	return ok
}

func readString(data []byte, pos *int) (string, bool) {
	l, ok := readVarint(data, pos)
	if !ok {
		return "", false
	}
	end, ok := span(data, *pos, l)
	if !ok || !utf8.Valid(data[*pos:end]) {
		return "", false
	}
	s := string(data[*pos:end])
	*pos = end
	return s, true
}

func readInt32(data []byte, pos *int) (int32, bool) {
	v, ok := readVarint(data, pos)
	return int32(v), ok
}

// readSub returns the next length-delimited field's bytes.
func readSub(data []byte, pos *int) ([]byte, bool) {
	l, ok := readVarint(data, pos)
	if !ok {
		return nil, false
	}
	end, ok := span(data, *pos, l)
	if !ok {
		return nil, false
	}
	sub := data[*pos:end]
	*pos = end
	return sub, true
}

// ImageMeta is packed metadata for image messages.
type ImageMeta struct {
	Width, Height int32
	Filename      string
}

// Identifier is the trimmed filename, "" when blank.
func (m ImageMeta) Identifier() string { return trimSpace(m.Filename) }

// Display is the image tag for this metadata.
func (m ImageMeta) Display() string { return ImageTag(m.Identifier()) }

// VideoMeta is packed metadata for video messages.
type VideoMeta struct {
	Width, Height int32
	Filename      string
}

// Display renders `[视频 WxH name]`, omitting unknown parts.
func (m VideoMeta) Display() string {
	out := "[视频"
	if m.Width > 0 && m.Height > 0 {
		out += " " + itoa(int64(m.Width)) + "x" + itoa(int64(m.Height))
	}
	if m.Filename != "" {
		out += " " + m.Filename
	}
	return out + "]"
}

// AudioMeta is packed metadata for voice messages.
type AudioMeta struct {
	AudioText string
}

// FileMeta is packed metadata for file messages.
type FileMeta struct {
	Filename string
}

// Identifier is the trimmed filename, "" when blank.
func (m FileMeta) Identifier() string { return trimSpace(m.Filename) }

// PackedV2 is the current packed media metadata message.
type PackedV2 struct {
	Field1, Field2 int32
	Image          *ImageMeta
	Video          *VideoMeta
	Audio          *AudioMeta
	File           *FileMeta
}

// PackedV1 is the simpler packed metadata of early 4.0 clients.
type PackedV1 struct {
	Field1, Field2 int32
	Filename       string
}

// ParsePackedV2 decodes packed metadata; false when malformed or empty.
func ParsePackedV2(data []byte) (PackedV2, bool) {
	var r PackedV2
	pos := 0
	for pos < len(data) {
		tag, ok := readVarint(data, &pos)
		if !ok {
			return PackedV2{}, false
		}
		field, w := tagField(tag)
		switch {
		case field == 1 && w == wireVarint:
			if r.Field1, ok = readInt32(data, &pos); !ok {
				return PackedV2{}, false
			}
		case field == 2 && w == wireVarint:
			if r.Field2, ok = readInt32(data, &pos); !ok {
				return PackedV2{}, false
			}
		case field == 3 && w == wireLen:
			r.Image = nil
			if sub, ok := readSub(data, &pos); ok {
				if m, ok := parseImageMeta(sub); ok {
					r.Image = &m
				}
			}
		case field == 4 && w == wireLen:
			r.Video = nil
			if sub, ok := readSub(data, &pos); ok {
				if m, ok := parseVideoMeta(sub); ok {
					r.Video = &m
				}
			}
		case field == 5 && w == wireLen:
			r.Audio = nil
			if sub, ok := readSub(data, &pos); ok {
				if m, ok := parseAudioMeta(sub); ok {
					r.Audio = &m
				}
			}
		case field == 7 && w == wireLen:
			r.File = nil
			if sub, ok := readSub(data, &pos); ok {
				if m, ok := parseFileMeta(sub); ok {
					r.File = &m
				}
			}
		case field == 9 && w == wireLen:
			l, ok := readVarint(data, &pos)
			if !ok {
				return PackedV2{}, false
			}
			// Unchecked skip, as in Rust: running past the end ends the loop.
			if end, ok := span(data, pos, l); ok {
				pos = end
			} else {
				pos = len(data)
			}
		default:
			if !skipField(data, &pos, w) {
				return PackedV2{}, false
			}
		}
	}
	if r.Field1 == 0 && r.Field2 == 0 && r.Image == nil && r.Video == nil && r.Audio == nil && r.File == nil {
		return PackedV2{}, false
	}
	return r, true
}

// ParsePackedV1 decodes the older packed metadata; false when malformed or
// without a filename.
func ParsePackedV1(data []byte) (PackedV1, bool) {
	var r PackedV1
	pos := 0
	for pos < len(data) {
		tag, ok := readVarint(data, &pos)
		if !ok {
			return PackedV1{}, false
		}
		field, w := tagField(tag)
		switch {
		case field == 1 && w == wireVarint:
			r.Field1, ok = readInt32(data, &pos)
		case field == 2 && w == wireVarint:
			r.Field2, ok = readInt32(data, &pos)
		case field == 3 && w == wireLen:
			r.Filename, ok = readString(data, &pos)
		default:
			ok = skipField(data, &pos, w)
		}
		if !ok {
			return PackedV1{}, false
		}
	}
	return r, r.Filename != ""
}

func parseImageMeta(data []byte) (ImageMeta, bool) {
	var r ImageMeta
	pos := 0
	for pos < len(data) {
		tag, ok := readVarint(data, &pos)
		if !ok {
			return ImageMeta{}, false
		}
		field, w := tagField(tag)
		switch {
		case field == 1 && w == wireVarint:
			r.Height, ok = readInt32(data, &pos)
		case field == 2 && w == wireVarint:
			r.Width, ok = readInt32(data, &pos)
		case field == 4 && w == wireLen:
			r.Filename, ok = readString(data, &pos)
		default:
			ok = skipField(data, &pos, w)
		}
		if !ok {
			return ImageMeta{}, false
		}
	}
	return r, true
}

func parseVideoMeta(data []byte) (VideoMeta, bool) {
	var r VideoMeta
	pos := 0
	for pos < len(data) {
		tag, ok := readVarint(data, &pos)
		if !ok {
			return VideoMeta{}, false
		}
		field, w := tagField(tag)
		switch {
		case field == 4 && w == wireVarint:
			r.Height, ok = readInt32(data, &pos)
		case field == 5 && w == wireVarint:
			r.Width, ok = readInt32(data, &pos)
		case field == 8 && w == wireLen:
			r.Filename, ok = readString(data, &pos)
		default:
			ok = skipField(data, &pos, w)
		}
		if !ok {
			return VideoMeta{}, false
		}
	}
	return r, true
}

func parseAudioMeta(data []byte) (AudioMeta, bool) {
	var r AudioMeta
	pos := 0
	for pos < len(data) {
		tag, ok := readVarint(data, &pos)
		if !ok {
			return AudioMeta{}, false
		}
		field, w := tagField(tag)
		switch {
		case field == 1 && w == wireVarint:
			_, ok = readVarint(data, &pos)
		case field == 2 && w == wireLen:
			r.AudioText, ok = readString(data, &pos)
		default:
			ok = skipField(data, &pos, w)
		}
		if !ok {
			return AudioMeta{}, false
		}
	}
	return r, true
}

// parseFileMeta ignores errors inside the nested name message and keeps
// scanning, as the Rust reader did; only top-level errors fail.
func parseFileMeta(data []byte) (FileMeta, bool) {
	var r FileMeta
	pos := 0
	for pos < len(data) {
		tag, ok := readVarint(data, &pos)
		if !ok {
			return FileMeta{}, false
		}
		field, w := tagField(tag)
		if field == 1 && w == wireLen {
			sub, ok := readSub(data, &pos)
			if !ok {
				continue
			}
			p := 0
			for p < len(sub) {
				t, _ := readVarint(sub, &p)
				f, w2 := tagField(t)
				if f == 2 && w2 == wireLen {
					r.Filename, _ = readString(sub, &p)
				} else {
					skipField(sub, &p, w2)
				}
			}
			continue
		}
		if !skipField(data, &pos, w) {
			return FileMeta{}, false
		}
	}
	return r, true
}
