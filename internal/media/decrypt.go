package media

import (
	"crypto/aes"
	"crypto/cipher"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
)

// Decrypted is the plaintext of a V2 .dat file.
type Decrypted struct {
	Data []byte
	// Format is classified from the AES section alone. TGGF means Data still
	// needs TGGFPartition and an HEVC conversion before export.
	Format Format
}

// DecryptV2 decrypts a V2 .dat file held in memory.
//
// Layout after the 15-byte header (magic, aesLen u32le, xorLen u32le, flag):
// an AES-128-ECB section of aesLen rounded up to a whole block (a full extra
// padding block when aesLen is block-aligned), a plaintext middle, and an
// xorLen-byte tail XORed with the key's XOR byte.
func DecryptV2(data []byte, k Keys) (Decrypted, error) {
	if len(data) < v2HeaderLen {
		return Decrypted{}, fmt.Errorf("file too small for V2 header: %d bytes", len(data))
	}
	if !IsV2(data) {
		return Decrypted{}, errors.New("not a V2 media file")
	}
	aesLen := int(binary.LittleEndian.Uint32(data[6:10]))
	xorLen := int(binary.LittleEndian.Uint32(data[10:14]))
	aesCipherLen := (aesLen/aes.BlockSize + 1) * aes.BlockSize
	if aesLen%aes.BlockSize != 0 {
		aesCipherLen = (aesLen + aes.BlockSize - 1) / aes.BlockSize * aes.BlockSize
	}
	bodyStart := v2HeaderLen + aesCipherLen
	if bodyStart > len(data) {
		return Decrypted{}, fmt.Errorf("AES cipher len %d exceeds file (%d bytes)", aesCipherLen, len(data))
	}
	// The Rust reader underflowed (panicked) when the tail was longer than
	// the file and duplicated ciphertext bytes when it overlapped the AES
	// section; both mean a corrupt header.
	if xorLen > len(data)-bodyStart {
		return Decrypted{}, fmt.Errorf("XOR tail len %d overlaps the AES section (%d bytes after it)", xorLen, len(data)-bodyStart)
	}

	head := decryptECB(data[v2HeaderLen:bodyStart], k.AES[:])
	head = pkcs7Unpad(head)
	plainLen := len(head)
	xorStart := len(data) - xorLen

	out := make([]byte, 0, plainLen+len(data)-bodyStart)
	out = append(out, head...)
	out = append(out, data[bodyStart:xorStart]...)
	for _, b := range data[xorStart:] {
		out = append(out, b^k.XOR)
	}
	return Decrypted{Data: out, Format: Classify(out[:plainLen])}, nil
}

// decryptECB decrypts whole blocks of data; a trailing partial block cannot
// occur because callers pass block-aligned lengths.
func decryptECB(data, key []byte) []byte {
	block, err := aes.NewCipher(key)
	if err != nil {
		panic(err) // key length is fixed at 16
	}
	out := make([]byte, len(data))
	for i := 0; i+aes.BlockSize <= len(data); i += aes.BlockSize {
		block.Decrypt(out[i:i+aes.BlockSize], data[i:i+aes.BlockSize])
	}
	return out
}

// pkcs7Unpad strips valid PKCS#7 padding of 1..16 bytes and returns data
// unchanged when the padding is missing or malformed.
func pkcs7Unpad(data []byte) []byte {
	if len(data) == 0 {
		return data
	}
	pad := int(data[len(data)-1])
	if pad == 0 || pad > 16 || pad > len(data) {
		return data
	}
	for _, b := range data[len(data)-pad:] {
		if int(b) != pad {
			return data
		}
	}
	return data[:len(data)-pad]
}

// TGGFPartition extracts the HEVC stream inside a TGGF container: the
// longest NAL run whose start code is preceded by a big-endian u32 length
// that fits the data, else everything from the first start code.
func TGGFPartition(data []byte) ([]byte, error) {
	if p := findTGGFPartition(data); p != nil {
		return p, nil
	}
	return nil, errors.New("tggf HEVC partition not found")
}

func findTGGFPartition(data []byte) []byte {
	if DetectExt(data) != "tggf" || len(data) < 8 {
		return nil
	}
	start := min(max(int(data[4]), 4), len(data))
	bestOff, bestLen := -1, 0
	for i := start; i+3 < len(data); i++ {
		if !isStartCode(data[i:]) || i < 4 {
			continue
		}
		n := int(binary.BigEndian.Uint32(data[i-4 : i]))
		if n > 0 && i+n <= len(data) && n > bestLen {
			bestOff, bestLen = i, n
		}
	}
	if bestOff >= 0 {
		return data[bestOff : bestOff+bestLen]
	}
	for i := start; i+3 < len(data); i++ {
		if isStartCode(data[i:]) {
			return data[i:]
		}
	}
	return nil
}

func isStartCode(b []byte) bool {
	return len(b) >= 3 && b[0] == 0 && b[1] == 0 && (b[2] == 1 || len(b) >= 4 && b[2] == 0 && b[3] == 1)
}

// DecryptStickerCBC decrypts a cached sticker with AES-128-CBC, where the
// key and IV are both the 16 bytes hex-decoded from keyHex. It reports false
// when the key, length or padding is invalid.
func DecryptStickerCBC(data []byte, keyHex string) ([]byte, bool) {
	key, err := hex.DecodeString(keyHex)
	if err != nil || len(key) != 16 || len(data) == 0 || len(data)%aes.BlockSize != 0 {
		return nil, false
	}
	block, err := aes.NewCipher(key)
	if err != nil {
		return nil, false
	}
	out := make([]byte, len(data))
	cipher.NewCBCDecrypter(block, key).CryptBlocks(out, data)
	// Strict PKCS#7, as the Rust decryptor required.
	pad := int(out[len(out)-1])
	if pad == 0 || pad > aes.BlockSize {
		return nil, false
	}
	for _, b := range out[len(out)-pad:] {
		if int(b) != pad {
			return nil, false
		}
	}
	return out[:len(out)-pad], true
}
