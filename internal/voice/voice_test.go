package voice

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"testing"

	"github.com/xiaotianxt/tg/internal/dict"
)

func nativeFixture() []byte { return append(slices.Clone(dict.NativeVoiceHeader), " payload"...) }

func TestSniff(t *testing.T) {
	native := nativeFixture()
	cases := []struct {
		name   string
		data   []byte
		bytes  []byte
		format Format
	}{
		{"native", native, native, NativeEncoded},
		{"native with pad byte", append([]byte{2}, native...), native, NativeEncoded},
		{"two pad bytes is raw", append([]byte{2, 2}, native...), append([]byte{2, 2}, native...), Raw},
		{"amr", []byte("#!AMR\nxx"), []byte("#!AMR\nxx"), Amr},
		{"raw", []byte{1}, []byte{1}, Raw},
		{"header only", dict.NativeVoiceHeader, dict.NativeVoiceHeader, NativeEncoded},
	}
	for _, c := range cases {
		p, ok := Sniff(c.data)
		if !ok || p.Format != c.format || !bytes.Equal(p.Bytes, c.bytes) {
			t.Errorf("%s: got %+v, %v", c.name, p, ok)
		}
	}
	if _, ok := Sniff(nil); ok {
		t.Error("empty blob sniffed")
	}
	if NativeEncoded.Ext() != "voice" || Amr.Ext() != "amr" || Raw.Ext() != "aud" {
		t.Error("extensions changed")
	}
}

// fakeDecoder writes an executable shell script standing in for the decoder.
func fakeDecoder(t *testing.T, body string) *Decoder {
	dir := t.TempDir()
	p := filepath.Join(dir, dict.NativeVoiceDecoderCommand)
	if err := os.WriteFile(p, []byte("#!/bin/sh\n"+body), 0o755); err != nil {
		t.Fatal(err)
	}
	return &Decoder{Path: p, TempDir: dir}
}

// Scripts parse -o like the real decoder and check the protocol.
const (
	parseArgs = `[ "$1" = decode ] || exit 2
out=''; wav=0; rate=''; tolerant=''; in=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    -i) shift; in="$1" ;;
    -o) shift; out="$1" ;;
    --sample-rate) shift; rate="$1" ;;
    --tolerant) shift; tolerant="$1" ;;
    --wav) wav=1 ;;
  esac
  shift
done
[ "$wav" = 1 ] && [ "$rate" = 24000 ] && [ "$tolerant" = skip ] || exit 3
case "$in" in *.voice) ;; *) exit 4 ;; esac
`
	goodWAV = `printf 'RIFF....WAVE................................' > "$out"
`
)

func TestExport(t *testing.T) {
	native, _ := Sniff(nativeFixture())
	cases := []struct {
		name    string
		decoder string // "" for no decoder
		outcome Outcome
		ext     string
	}{
		{"decoded", parseArgs + goodWAV, WAV, "wav"},
		{"decoder fails after partial output", parseArgs + "printf partial > \"$out\"\nexit 1\n", Native, "voice"},
		{"decoder writes nothing", "exit 0\n", Native, "voice"},
		{"decoder writes an invalid header", parseArgs + "printf 'not a wav file' > \"$out\"\n", Native, "voice"},
		{"short header", parseArgs + "printf 'RIFF....WAVE' > \"$out\"\n", Native, "voice"},
		{"no decoder", "", Native, "voice"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			out := t.TempDir()
			pathFor := func(ext string) string { return filepath.Join(out, "v_Voice_34_0001_7."+ext) }
			var d *Decoder
			if c.decoder != "" {
				d = fakeDecoder(t, c.decoder)
				// A stale WAV from an earlier export must not survive a
				// failed decode.
				os.WriteFile(pathFor("wav"), []byte("RIFF....WAVE................................"), 0o644)
			}
			res, err := Export(native, d, pathFor)
			if err != nil {
				t.Fatal(err)
			}
			if res.Outcome != c.outcome || res.Path != pathFor(c.ext) || res.Format != NativeEncoded {
				t.Fatalf("got %+v", res)
			}
			if (res.Outcome == Native) != (res.Reason != nil) {
				t.Fatalf("reason = %v", res.Reason)
			}
			if c.decoder == "" && !errors.Is(res.Reason, ErrNoDecoder) {
				t.Fatalf("reason = %v", res.Reason)
			}
			entries, _ := os.ReadDir(out)
			if len(entries) != 1 {
				t.Fatalf("output files = %v", entries)
			}
			if res.Outcome == Native {
				if got, _ := os.ReadFile(res.Path); !bytes.Equal(got, native.Bytes) {
					t.Fatalf("native file = %q", got)
				}
			}
		})
	}
}

func TestExportPassthrough(t *testing.T) {
	out := t.TempDir()
	p, _ := Sniff([]byte("#!AMR\nxx"))
	res, err := Export(p, fakeDecoder(t, "exit 9\n"), func(ext string) string { return filepath.Join(out, "a."+ext) })
	if err != nil || res.Outcome != Passthrough || res.Path != filepath.Join(out, "a.amr") || res.Reason != nil {
		t.Fatalf("got %+v, %v", res, err)
	}
}
