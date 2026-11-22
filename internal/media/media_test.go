package media

import (
	"bytes"
	"crypto/aes"
	"encoding/binary"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
)

func TestKeysForVector(t *testing.T) {
	k := KeysFor(1020215821, dict.AccountIDPrefix+"7286922865011")
	if got := string(k.AES[:]); got != "68ec773d54b0245b" {
		t.Fatalf("AES key = %q", got)
	}
	if k.XOR != byte(1020215821&0xff) {
		t.Fatalf("XOR key = %#x", k.XOR)
	}
}

func TestKVCommCode(t *testing.T) {
	cases := []struct {
		name string
		code uint64
		ok   bool
	}{
		{"key_1020215821_4066646301_1_1777357465_1581567597_3600_input.statistic", 1020215821, true},
		{"key_reportnow_1020215821_x", 0, false},
		{"key_123", 0, false}, // no separator after the code
		{"config.ini", 0, false},
		{"monitordata_1020215821_20571", 0, false},
		{"key__1", 0, false},
	}
	for _, c := range cases {
		code, ok := kvcommCode(c.name)
		if code != c.code || ok != c.ok {
			t.Errorf("kvcommCode(%q) = %d, %v; want %d, %v", c.name, code, ok, c.code, c.ok)
		}
	}
}

func testAccount() string { return dict.AccountIDPrefix + "1234567" }

func kvcommWith(t *testing.T, codes ...uint64) string {
	dir := t.TempDir()
	os.WriteFile(filepath.Join(dir, "config.ini"), nil, 0o600)
	for i, c := range codes {
		name := fmt.Sprintf("key_%d_%d_1_1700000000_1_3600_input.statistic", c, i)
		if err := os.WriteFile(filepath.Join(dir, name), nil, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	return dir
}

var pngHead = []byte{0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 'I', 'H', 'D', 'R'}

// v2File builds a V2 file: plaintext's AES section encrypted with k
// (PKCS#7-padded), then middle in the clear, then tail XORed with k.XOR.
func v2File(k Keys, aesPart, middle, tail []byte) []byte {
	pad := aes.BlockSize - len(aesPart)%aes.BlockSize
	enc := append(slices.Clone(aesPart), bytes.Repeat([]byte{byte(pad)}, pad)...)
	block, _ := aes.NewCipher(k.AES[:])
	for i := 0; i < len(enc); i += aes.BlockSize {
		block.Encrypt(enc[i:i+aes.BlockSize], enc[i:i+aes.BlockSize])
	}
	data := slices.Clone(v2Magic)
	data = binary.LittleEndian.AppendUint32(data, uint32(len(aesPart)))
	data = binary.LittleEndian.AppendUint32(data, uint32(len(tail)))
	data = append(data, 1)
	data = append(data, enc...)
	data = append(data, middle...)
	for _, b := range tail {
		data = append(data, b^k.XOR)
	}
	return data
}

func writeSample(t *testing.T, dir, name string, data []byte) string {
	p := filepath.Join(dir, name)
	if err := os.WriteFile(p, data, 0o600); err != nil {
		t.Fatal(err)
	}
	return p
}

func TestSelectKeys(t *testing.T) {
	media := t.TempDir()
	plain := writeSample(t, media, "plain.jpg", []byte{0xFF, 0xD8, 0xFF, 0xE0})
	sample := func(name string, code uint64) string {
		return writeSample(t, media, name, v2File(KeysFor(code, testAccount()), pngHead, nil, nil))
	}

	t.Run("single code verified", func(t *testing.T) {
		k, err := SelectKeys(kvcommWith(t, 1111, 1111), testAccount(), []string{sample("a.dat", 1111)})
		if err != nil || k != KeysFor(1111, testAccount()) {
			t.Fatalf("got %v, %v", k, err)
		}
	})
	t.Run("decrypting code wins regardless of order", func(t *testing.T) {
		samples := []string{plain, sample("b.dat", 2222), sample("c.dat", 2222)}
		for _, codes := range [][]uint64{{1111, 2222, 3333}, {3333, 2222, 1111}} {
			k, err := SelectKeys(kvcommWith(t, codes...), testAccount(), samples)
			if err != nil || k != KeysFor(2222, testAccount()) {
				t.Fatalf("codes %v: got %v, %v", codes, k, err)
			}
		}
	})
	t.Run("tie goes to smallest code", func(t *testing.T) {
		samples := []string{sample("d.dat", 5555), sample("e.dat", 4444)}
		k, err := SelectKeys(kvcommWith(t, 5555, 4444), testAccount(), samples)
		if err != nil || k != KeysFor(4444, testAccount()) {
			t.Fatalf("got %v, %v", k, err)
		}
	})
	t.Run("no code decrypts", func(t *testing.T) {
		_, err := SelectKeys(kvcommWith(t, 1111, 3333), testAccount(), []string{sample("f.dat", 2222)})
		if err == nil || !strings.Contains(err.Error(), "none of the 2 media key codes") ||
			strings.Contains(err.Error(), "1111") || strings.Contains(err.Error(), "3333") {
			t.Fatalf("err = %v", err)
		}
	})
	t.Run("no V2 sample uses smallest", func(t *testing.T) {
		k, err := SelectKeys(kvcommWith(t, 5555, 4444, 6666), testAccount(), []string{plain})
		if err != nil || k != KeysFor(4444, testAccount()) {
			t.Fatalf("got %v, %v", k, err)
		}
	})
	t.Run("short V2 file is not a sample", func(t *testing.T) {
		short := writeSample(t, media, "short.dat", v2Magic)
		k, err := SelectKeys(kvcommWith(t, 7777, 8888), testAccount(), []string{short})
		if err != nil || k != KeysFor(7777, testAccount()) {
			t.Fatalf("got %v, %v", k, err)
		}
	})
	t.Run("no key file", func(t *testing.T) {
		_, err := SelectKeys(kvcommWith(t), testAccount(), nil)
		if err == nil || !strings.Contains(err.Error(), "no key_<code>_*.statistic file") {
			t.Fatalf("err = %v", err)
		}
	})
}

func TestDecryptV2(t *testing.T) {
	k := Keys{AES: [16]byte([]byte("0123456789abcdef")), XOR: 0x5c}
	jpg := append([]byte{0xFF, 0xD8, 0xFF, 0xE0}, bytes.Repeat([]byte{7}, 20)...)

	cases := []struct {
		name               string
		aesPart, mid, tail []byte
		wantKind           Kind
		wantExt            string
	}{
		{"head only", jpg, nil, nil, Plain, "jpg"},
		{"block aligned head gets a padding block", pngHead, []byte("middle"), []byte("tail"), Plain, "png"},
		{"head middle tail", jpg, []byte("plain middle"), []byte("xor tail"), Plain, "jpg"},
		{"tggf head", append(slices.Clone(dict.StickerMagic), 1, 2, 3), nil, []byte("t"), TGGF, "tggf"},
		{"unknown head", []byte("nothing known here"), nil, nil, Unknown, ""},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			dec, err := DecryptV2(v2File(k, c.aesPart, c.mid, c.tail), k)
			if err != nil {
				t.Fatal(err)
			}
			want := slices.Concat(c.aesPart, c.mid, c.tail)
			if !bytes.Equal(dec.Data, want) {
				t.Fatalf("data = %q, want %q", dec.Data, want)
			}
			if dec.Format != (Format{c.wantKind, c.wantExt}) {
				t.Fatalf("format = %+v", dec.Format)
			}
		})
	}

	valid := v2File(k, jpg, nil, nil)
	bad := map[string][]byte{
		"too small":         valid[:14],
		"not V2":            append([]byte{0, 0, 0, 0, 0, 0}, valid[6:]...),
		"AES exceeds file":  valid[:len(valid)-1],
		"XOR tail too long": func() []byte { d := slices.Clone(valid); binary.LittleEndian.PutUint32(d[10:], 1<<20); return d }(),
		// One tail byte but no bytes after the AES section.
		"XOR overlaps AES": func() []byte { d := slices.Clone(valid); binary.LittleEndian.PutUint32(d[10:], 1); return d }(),
	}
	for name, data := range bad {
		if _, err := DecryptV2(data, k); err == nil {
			t.Errorf("%s: no error", name)
		}
	}
}

func TestClassifyAndDetectExt(t *testing.T) {
	cases := []struct {
		data []byte
		want Format
	}{
		{[]byte{0xFF, 0xD8, 0xFF, 0xE0, 0}, Format{Plain, "jpg"}},
		{[]byte{0x89, 0x50, 0x4E, 0x47, 0x0D}, Format{Plain, "png"}},
		{[]byte("GIF89a"), Format{Plain, "gif"}},
		{[]byte("RIFF\x00\x00\x00\x00WEBP"), Format{Plain, "webp"}},
		{[]byte("RIFF\x00\x00\x00\x00WAVE"), Format{Unknown, ""}},
		{[]byte("\x00\x00\x00\x18ftypmp42"), Format{Plain, "mp4"}},
		{[]byte("\x00\x00\x00\x18ftypavif"), Format{Plain, "avif"}},
		{[]byte("\x00\x00\x00\x18ftypmif1"), Format{Plain, "heic"}},
		{[]byte("\x00\x00\x00\x18ftyp"), Format{Unknown, ""}}, // too short for a brand
		{[]byte{0x42, 0x4D, 0}, Format{Plain, "bmp"}},
		{append(slices.Clone(dict.StickerMagic), 0x13, 0, 0, 0), Format{TGGF, "tggf"}},
		{slices.Concat(v2Magic, []byte{1, 2}), Format{V2, ""}},
		{[]byte{0xFF}, Format{Unknown, ""}},
		{nil, Format{Unknown, ""}},
	}
	for _, c := range cases {
		if got := Classify(c.data); got != c.want {
			t.Errorf("Classify(%q) = %+v, want %+v", c.data, got, c.want)
		}
	}
}

func TestPKCS7Unpad(t *testing.T) {
	cases := []struct{ in, want string }{
		{"hello\x03\x03\x03", "hello"},
		{"hello\x01", "hello"},
		{"no padding  ", "no padding  "}, // 0x20 > 16
		{"bad\x02\x03\x03", "bad\x02\x03\x03"},
		{"zero\x00", "zero\x00"},
		{"\x05\x05", "\x05\x05"}, // longer than the data
		{"", ""},
		{strings.Repeat("\x10", 16), ""},
	}
	for _, c := range cases {
		if got := string(pkcs7Unpad([]byte(c.in))); got != c.want {
			t.Errorf("pkcs7Unpad(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestTGGFPartition(t *testing.T) {
	hevc := []byte("\x00\x00\x00\x01\x40\x01\x0c\x01")
	short := []byte("\x00\x00\x01\x42")
	tggf := func(parts ...[]byte) []byte { return slices.Concat(append([][]byte{dict.StickerMagic}, parts...)...) }
	be := func(n int) []byte { return binary.BigEndian.AppendUint32(nil, uint32(n)) }

	cases := []struct {
		name string
		data []byte
		want []byte
	}{
		{"length-prefixed NAL", tggf([]byte("\x08abc"), be(len(hevc)), hevc, []byte("tail")), hevc},
		{"longest prefixed NAL wins", tggf([]byte("\x08abc"), be(len(short)), short, be(len(hevc)), hevc), hevc},
		{"oversized prefix falls back to first start code", tggf([]byte("\x08abc"), be(999), hevc), hevc},
		{"no start code", tggf([]byte("\x08abcdefgh")), nil},
		{"not tggf", slices.Concat([]byte("JUNK\x08abc"), be(len(hevc)), hevc), nil},
	}
	for _, c := range cases {
		got, err := TGGFPartition(c.data)
		if c.want == nil {
			if err == nil {
				t.Errorf("%s: got %q, want error", c.name, got)
			}
			continue
		}
		if err != nil || !bytes.Equal(got, c.want) {
			t.Errorf("%s: got %q, %v; want %q", c.name, got, err, c.want)
		}
	}
}

func TestDecryptStickerCBC(t *testing.T) {
	keyHex := "00112233445566778899aabbccddeeff"
	key := []byte{0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff}
	plain := []byte("GIF89a sticker body")
	pad := aes.BlockSize - len(plain)%aes.BlockSize
	buf := append(slices.Clone(plain), bytes.Repeat([]byte{byte(pad)}, pad)...)
	block, _ := aes.NewCipher(key)
	prev := key
	for i := 0; i < len(buf); i += aes.BlockSize {
		for j := range aes.BlockSize {
			buf[i+j] ^= prev[j]
		}
		block.Encrypt(buf[i:i+aes.BlockSize], buf[i:i+aes.BlockSize])
		prev = buf[i : i+aes.BlockSize]
	}
	if got, ok := DecryptStickerCBC(buf, keyHex); !ok || !bytes.Equal(got, plain) {
		t.Fatalf("got %q, %v", got, ok)
	}
	if _, ok := DecryptStickerCBC(buf, "zz"); ok {
		t.Error("accepted a bad key")
	}
	if _, ok := DecryptStickerCBC(buf[:len(buf)-1], keyHex); ok {
		t.Error("accepted a partial block")
	}
}

func TestParseMediaID(t *testing.T) {
	const id = "00437c16256fa6f2316db8cffcc7dce0"
	for _, raw := range []string{id, " [img:" + id + "] ", "[img:" + id + "]"} {
		if got, err := ParseMediaID(raw); err != nil || got != id {
			t.Errorf("ParseMediaID(%q) = %q, %v", raw, got, err)
		}
	}
	for _, raw := range []string{strings.ToUpper(id), id[:31], "[img:../../etc/passwd]", "", "[img:]"} {
		if _, err := ParseMediaID(raw); err == nil {
			t.Errorf("ParseMediaID(%q) accepted", raw)
		}
	}
}

func TestCandidateOrder(t *testing.T) {
	const id = "00437c16256fa6f2316db8cffcc7dce0"
	describe := func(cs []candidate) []string {
		var out []string
		for _, c := range cs {
			out = append(out, strings.TrimPrefix(c.name, id)+"="+c.variant)
		}
		return out
	}
	preview := []string{".dat=standard", "_t.dat=thumbnail", "_h.dat=original", "_thumb.jpg=video cover", ".jpg=large video cover"}
	original := []string{"_h.dat=original", ".dat=standard", "_t.dat=thumbnail", ".jpg=large video cover", "_thumb.jpg=video cover"}
	if got := describe(candidates(id, Preview)); !slices.Equal(got, preview) {
		t.Errorf("preview = %v", got)
	}
	if got := describe(candidates(id, Original)); !slices.Equal(got, original) {
		t.Errorf("original = %v", got)
	}
}

func TestSelectMonths(t *testing.T) {
	names := []string{"2026-01", "2026-11", "2025-12", "2026-1", "2026_02", "abcd-ef", "2026-02x", "2026-02"}
	if got := selectMonths(names, ""); !slices.Equal(got, []string{"2026-11", "2026-02", "2026-01", "2025-12"}) {
		t.Errorf("no floor = %v", got)
	}
	if got := selectMonths(names, "2026-02"); !slices.Equal(got, []string{"2026-11", "2026-02"}) {
		t.Errorf("floor = %v", got)
	}
}

func TestIndexRanking(t *testing.T) {
	base := t.TempDir()
	img := filepath.Join(base, "msg/attach/account/2026-04/Img")
	os.MkdirAll(img, 0o755)
	files := map[string]string{
		"abc_thumb.dat":       "larger thumb",
		"abc.dat":             "full-size",
		"prefix_thumb.dat":    "larger thumb",
		"prefix_full.dat":     "full",
		"x%_needle_thumb.dat": "larger thumb",
		"y%_needle.dat":       "full",
		"big_a.dat":           "aa",
		"big_b.dat":           "bbbb",
	}
	for name, body := range files {
		writeSample(t, img, name, []byte(body))
	}
	idx := LoadIndex(base, "session", []Category{Image}, 2)
	cases := map[string]string{
		"abc":      "abc.dat",         // exact stem beats the larger thumbnail
		"ABC.DAT":  "abc.dat",         // case-insensitive exact name
		"abc.jpg":  "abc.dat",         // identifier's own stem
		"prefix":   "prefix_full.dat", // prefix, non-thumbnail first
		"%_needle": "y%_needle.dat",   // substring
		"big_":     "big_b.dat",       // prefix, larger first
	}
	for id, want := range cases {
		got, ok := idx.Find(Image, id)
		if !ok || got != filepath.Join(img, want) {
			t.Errorf("Find(%q) = %q, %v; want %s", id, got, ok, want)
		}
	}
	if _, ok := idx.Find(Image, "missing"); ok {
		t.Error("found a missing id")
	}
	if _, ok := idx.Find(File, "abc"); ok {
		t.Error("found an unindexed category")
	}
}

func TestPathStem(t *testing.T) {
	cases := map[string]string{"report.pdf": "report", "a/b.pdf": "b", "a/b/": "b", ".hidden": ".hidden", "x.tar.gz": "x.tar", "..": "", "": ""}
	for in, want := range cases {
		if got := pathStem(in); got != want {
			t.Errorf("pathStem(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestExportCachedSkipsUndecryptableV2(t *testing.T) {
	dir := t.TempDir()
	src := writeSample(t, dir, "x.dat", v2File(KeysFor(1, testAccount()), pngHead, nil, nil))
	out := filepath.Join(dir, "out")
	got, err := ExportCached(src, out, "x", nil)
	if err != nil || got.Kind != SkippedEncrypted {
		t.Fatalf("got %+v, %v", got, err)
	}
	if _, err := os.Stat(out); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("output dir created: %v", err)
	}
	k := KeysFor(1, testAccount())
	got, err = ExportCached(src, out, "x", &k)
	if err != nil || got.Kind != Written || got.Path != filepath.Join(out, "x.png") {
		t.Fatalf("got %+v, %v", got, err)
	}
}

// encodeTestHEVC needs ffmpeg with libx265; it reports false otherwise.
func encodeTestHEVC(ffmpeg, size, pixFmt string) ([]byte, bool) {
	out, err := exec.Command(ffmpeg, "-hide_banner", "-loglevel", "error", "-nostdin", "-f", "lavfi",
		"-i", "testsrc2=size="+size, "-frames:v", "1", "-pix_fmt", pixFmt, "-c:v", "libx265",
		"-x265-params", "log-level=none", "-f", "hevc", "pipe:1").Output()
	return out, err == nil && len(out) > 0
}

func TestHEVCBatchMatchesSingle(t *testing.T) {
	ffmpeg, err := FFmpegPath(env.FromProcess())
	if err != nil {
		t.Skip(err)
	}
	var hevcs [][]byte
	for _, c := range [][2]string{{"64x48", "yuv420p"}, {"66x34", "yuvj420p"}, {"40x40", "yuv444p"}} {
		h, ok := encodeTestHEVC(ffmpeg, c[0], c[1])
		if !ok {
			t.Skip("ffmpeg with libx265 is unavailable")
		}
		hevcs = append(hevcs, h)
	}
	batch, err := HEVCBatchToJPEG(ffmpeg, hevcs)
	if err != nil {
		t.Fatal(err)
	}
	for i, h := range hevcs {
		single, err := HEVCToJPEG(ffmpeg, h)
		if err != nil || !bytes.Equal(single, batch[i]) {
			t.Fatalf("image %d differs: %v", i, err)
		}
	}
}
