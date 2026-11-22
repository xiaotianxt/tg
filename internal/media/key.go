package media

import (
	"crypto/aes"
	"crypto/md5"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"slices"
	"strconv"
	"strings"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/logx"
	"github.com/xiaotianxt/tg/internal/paths"
)

// Keys decrypt V2 .dat media of one account.
type Keys struct {
	// AES is the AES-128 key: the first 16 hex characters of
	// md5(code + account id), used as ASCII bytes, not hex-decoded.
	AES [16]byte
	// XOR is code & 0xff, applied to the tail section.
	XOR byte
}

// maxVerifySamples is how many V2 files are checked per candidate code. A
// wrong key matches a 2-byte image signature about once in 65536 blocks, so
// a few samples separate the codes.
const maxVerifySamples = 4

// FindKeys derives the media keys of the account whose media base is base.
// samples are cached files the caller is about to decrypt; the V2 files
// among them decide which kvcomm code is the live one.
func FindKeys(e env.Env, base string, samples []string) (Keys, error) {
	account := paths.AccountID(base)
	if account == "" {
		return Keys{}, fmt.Errorf("cannot determine clean account id from path: %s", base)
	}
	candidates := dict.KVCommCandidateDirs(e.UserHome)
	for _, dir := range candidates {
		if st, err := os.Stat(dir); err == nil && st.IsDir() {
			return SelectKeys(dir, account, samples)
		}
	}
	return Keys{}, fmt.Errorf("kvcomm directory not found at %s", strings.Join(candidates, ", "))
}

// SelectKeys picks the kvcomm code whose derived key decrypts the samples.
//
// The client can keep `key_<code>_*` files for several codes at once, so
// every distinct code is a candidate, scored by how many sample V2 heads
// decrypt to a known media signature. The highest score wins and ties go to
// the smallest code, so directory order never matters. Without a V2 sample
// the smallest code is used (with a warning when there are several):
// nothing the caller holds needs the key.
func SelectKeys(kvcommDir, accountID string, samples []string) (Keys, error) {
	codes, err := kvcommCodes(kvcommDir)
	if err != nil {
		return Keys{}, err
	}
	heads, err := readV2Heads(samples)
	if err != nil {
		return Keys{}, err
	}
	if len(heads) == 0 {
		if len(codes) > 1 {
			logx.Warnf("%d media key codes found and no V2 media to verify them; using the smallest", len(codes))
		}
		return KeysFor(codes[0], accountID), nil
	}
	best, bestScore := Keys{}, 0
	for _, code := range codes {
		k := KeysFor(code, accountID)
		score := 0
		for _, h := range heads {
			if decryptsHead(h, k) {
				score++
			}
		}
		if score > bestScore {
			best, bestScore = k, score
		}
	}
	if bestScore == 0 {
		return Keys{}, fmt.Errorf("none of the %d media key codes in %s decrypts the cached media (%d V2 files checked)", len(codes), kvcommDir, len(heads))
	}
	return best, nil
}

// KeysFor derives the keys for one kvcomm code.
func KeysFor(code uint64, accountID string) Keys {
	sum := md5.Sum([]byte(strconv.FormatUint(code, 10) + accountID))
	var k Keys
	copy(k.AES[:], hex.EncodeToString(sum[:])[:16])
	k.XOR = byte(code)
	return k
}

// kvcommCodes returns every distinct code from `key_<code>_*.statistic`
// names, ascending.
func kvcommCodes(dir string) ([]uint64, error) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil, fmt.Errorf("cannot read kvcomm dir: %w", err)
	}
	var codes []uint64
	for _, e := range entries {
		if code, ok := kvcommCode(e.Name()); ok {
			codes = append(codes, code)
		}
	}
	slices.Sort(codes)
	codes = slices.Compact(codes)
	if len(codes) == 0 {
		return nil, fmt.Errorf("no key_<code>_*.statistic file found in %s", dir)
	}
	return codes, nil
}

// kvcommCode extracts the code from a name such as
// `key_1020215821_4066646301_1_..._input.statistic`. Like the Rust reader it
// only requires the `key_<digits>_` prefix.
func kvcommCode(name string) (uint64, bool) {
	rest, ok := strings.CutPrefix(name, "key_")
	if !ok {
		return 0, false
	}
	digits, _, ok := strings.Cut(rest, "_")
	if !ok {
		return 0, false
	}
	// Rust's u64 parse accepted one leading '+'; ParseUint does not.
	code, err := strconv.ParseUint(strings.TrimPrefix(digits, "+"), 10, 64)
	return code, err == nil
}

// readV2Heads returns the first AES block of each V2 sample, up to
// maxVerifySamples. Files too short to hold one block are skipped; export
// reports them when it decrypts.
func readV2Heads(samples []string) ([][aes.BlockSize]byte, error) {
	var heads [][aes.BlockSize]byte
	for _, p := range samples {
		buf, err := readPrefix(p, v2HeaderLen+aes.BlockSize)
		if err != nil {
			return nil, err
		}
		if len(buf) == v2HeaderLen+aes.BlockSize && IsV2(buf) {
			heads = append(heads, [aes.BlockSize]byte(buf[v2HeaderLen:]))
			if len(heads) == maxVerifySamples {
				break
			}
		}
	}
	return heads, nil
}

func readPrefix(path string, n int) ([]byte, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}
	defer f.Close()
	buf := make([]byte, n)
	got, err := io.ReadFull(f, buf)
	if err != nil && !errors.Is(err, io.ErrUnexpectedEOF) && !errors.Is(err, io.EOF) {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}
	return buf[:got], nil
}

func decryptsHead(head [aes.BlockSize]byte, k Keys) bool {
	return DetectExt(decryptECB(head[:], k.AES[:])) != "bin"
}

// FileIsV2 reports whether the file at path starts with the V2 magic,
// whatever its extension.
func FileIsV2(path string) (bool, error) {
	buf, err := readPrefix(path, len(v2Magic))
	if err != nil {
		return false, err
	}
	return IsV2(buf), nil
}
