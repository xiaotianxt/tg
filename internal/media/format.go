// Package media finds and decodes the client's local media cache: base
// directory discovery, the export media index, the per-id image cache, media
// keys from kvcomm, V2 .dat decryption, sticker decryption and HEVC to JPEG
// conversion through ffmpeg.
package media

import (
	"bytes"

	"github.com/xiaotianxt/tg/internal/dict"
)

// Kind is what a cached file's leading bytes say it is.
type Kind int

const (
	// Unknown matches no known signature.
	Unknown Kind = iota
	// Plain is an ordinary image or video file; Format.Ext names it.
	Plain
	// V2 is an encrypted V2 .dat file that needs media keys.
	V2
	// TGGF is the upstream sticker container wrapping an HEVC picture; it
	// needs ffmpeg to become a JPEG.
	TGGF
)

// Format is the classification of a file header.
type Format struct {
	Kind Kind
	// Ext is the file extension for Plain, "tggf" for TGGF, "" otherwise.
	Ext string
}

// v2Magic starts every V2 .dat file.
var v2Magic = []byte{0x07, 0x08, 0x56, 0x32, 0x08, 0x07}

// v2HeaderLen is magic(6) + aesLen(4) + xorLen(4) + flag(1).
const v2HeaderLen = 15

// IsV2 reports whether header starts with the V2 magic.
func IsV2(header []byte) bool { return bytes.HasPrefix(header, v2Magic) }

// Classify sniffs a file header. V2 is checked first; everything else goes
// through DetectExt.
func Classify(header []byte) Format {
	if IsV2(header) {
		return Format{Kind: V2}
	}
	switch ext := DetectExt(header); ext {
	case "bin":
		return Format{Kind: Unknown}
	case "tggf":
		return Format{Kind: TGGF, Ext: ext}
	default:
		return Format{Kind: Plain, Ext: ext}
	}
}

// DetectExt names the file extension of decrypted content from its magic
// bytes: png, jpg, webp, avif, heic, mp4, gif, bmp, tggf, or bin when
// nothing matches.
func DetectExt(data []byte) string {
	switch {
	case bytes.HasPrefix(data, []byte{0x89, 0x50, 0x4E, 0x47}):
		return "png"
	case bytes.HasPrefix(data, []byte{0xFF, 0xD8}):
		return "jpg"
	case len(data) >= 12 && string(data[:4]) == "RIFF" && string(data[8:12]) == "WEBP":
		return "webp"
	case len(data) >= 12 && string(data[4:8]) == "ftyp":
		switch string(data[8:12]) {
		case "avif", "avis":
			return "avif"
		case "heic", "heix", "hevc", "hevx", "mif1", "msf1":
			return "heic"
		default:
			return "mp4"
		}
	case bytes.HasPrefix(data, []byte("GIF8")):
		return "gif"
	case bytes.HasPrefix(data, []byte{0x42, 0x4D}):
		return "bmp"
	case bytes.HasPrefix(data, dict.StickerMagic):
		return "tggf"
	default:
		return "bin"
	}
}

// imageMIME maps the image formats viewers accept to their MIME type.
func imageMIME(ext string) (string, bool) {
	switch ext {
	case "jpg":
		return "image/jpeg", true
	case "png":
		return "image/png", true
	case "gif":
		return "image/gif", true
	case "webp":
		return "image/webp", true
	}
	return "", false
}
