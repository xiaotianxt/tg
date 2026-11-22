package decode

import (
	"bytes"
	"compress/zlib"
	"io"
	"unicode/utf8"

	"github.com/klauspost/compress/zstd"
)

// zstdDecoder is shared; DecodeAll is safe for concurrent use.
var zstdDecoder = func() *zstd.Decoder {
	d, err := zstd.NewReader(nil, zstd.WithDecoderConcurrency(0))
	if err != nil {
		panic("decode: create zstd decoder: " + err.Error())
	}
	return d
}()

// Decompress decodes a message body: zstd (any number of frames, skippable
// frames allowed, no trailing bytes), then zlib, then the raw bytes. Each
// step must yield non-empty UTF-8; false when none does.
func Decompress(raw []byte) (string, bool) {
	if out, err := zstdDecoder.DecodeAll(raw, nil); err == nil && len(out) > 0 && utf8.Valid(out) {
		return string(out), true
	}
	if s, ok := inflateZlib(raw); ok {
		return s, true
	}
	if len(raw) > 0 && utf8.Valid(raw) {
		return string(raw), true
	}
	return "", false
}

func inflateZlib(data []byte) (string, bool) {
	r, err := zlib.NewReader(bytes.NewReader(data))
	if err != nil {
		return "", false
	}
	out, err := io.ReadAll(r)
	if err != nil || len(out) == 0 || !utf8.Valid(out) {
		return "", false
	}
	return string(out), true
}
