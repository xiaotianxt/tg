package e2e

import (
	"encoding/json"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"testing"
)

// Times of the account owner's own messages in the alice chat. Rust export
// attributes them to the chat partner; Go shows 我 (docs/go-architecture.md,
// "Decided behaviour changes"). Snapshots mask their sender, and
// TestExportOwnerSender pins the Go behaviour.
var ownerRows = []string{
	"2024-06-01 09:00:30", "2024-06-01 09:06:00", "2024-06-01 09:15:00",
	"2024-06-01 09:31:00", "2024-06-02 10:00:20",
}

// maskOwnerSenders replaces the sender of ownerRows in txt and csv exports.
func maskOwnerSenders(text string) string {
	for _, at := range ownerRows {
		txt := regexp.MustCompile(`(?m)^\[` + regexp.QuoteMeta(at) + `\] [^:\n]+: `)
		text = txt.ReplaceAllString(text, "["+at+"] <OWNER_ROW>: ")
		csv := regexp.MustCompile(`(?m)^` + regexp.QuoteMeta(at) + `,[^,\n]*,`)
		text = csv.ReplaceAllString(text, at+",<OWNER_ROW>,")
	}
	return text
}

type exportRow struct {
	Time      string `json:"time"`
	Timestamp int64  `json:"timestamp"`
	Sender    string `json:"sender"`
	Type      int64  `json:"type"`
	TypeName  string `json:"type_name"`
	Content   string `json:"content"`
}

func readExportJSON(t *testing.T, path string) []exportRow {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var rows []exportRow
	dec := json.NewDecoder(strings.NewReader(string(data)))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&rows); err != nil {
		t.Fatalf("chat.json: %v", err)
	}
	return rows
}

// TestExport covers the archive without a local media cache and without a
// working voice decoder: text exports plus native voice kept as .voice.
func TestExport(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	e.Extra = []string{"TG_VOICE_DECODER=/nonexistent/e2e/voice-decoder", "TG_FFMPEG=/nonexistent/e2e/ffmpeg"}
	out := filepath.Join(e.Tmp, "archive")
	r := e.run(t, fixtureArgs("export", "爱丽丝", "--output", out)...).mustSucceed(t)
	goldenRun(t, "export", r, out, "<OUT>")
	for _, want := range []string{"cached image/video/file export skipped", "Voice #1 WAV decode failed; preserving native voice"} {
		if !strings.Contains(r.Stderr, want) {
			t.Errorf("stderr lacks %q:\n%s", want, r.Stderr)
		}
	}

	txt, err := os.ReadFile(filepath.Join(out, "chat.txt"))
	if err != nil {
		t.Fatal(err)
	}
	golden(t, "export/chat.txt", maskOwnerSenders(string(txt)))
	csv, err := os.ReadFile(filepath.Join(out, "chat.csv"))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(string(csv), "\uFEFF时间,发送者,类型,内容\n") {
		t.Errorf("chat.csv must start with a BOM and the header row")
	}
	golden(t, "export/chat.csv", maskOwnerSenders(string(csv)))

	rows := readExportJSON(t, filepath.Join(out, "chat.json"))
	for i := range rows {
		if slices.Contains(ownerRows, rows[i].Time) {
			rows[i].Sender = "<OWNER_ROW>"
		}
	}
	var encoded strings.Builder
	enc := json.NewEncoder(&encoded)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	if err := enc.Encode(rows); err != nil {
		t.Fatal(err)
	}
	golden(t, "export/chat.json", encoded.String())

	// The native voice keeps its header but loses the storage padding byte;
	// other audio is copied as is.
	media := filepath.Join(out, "media")
	assertFile(t, filepath.Join(media, "tgid_alice_Voice_34_0001_4.voice"), nativeVoice("native-voice-frames")[1:], 0)
	assertFile(t, filepath.Join(media, "tgid_alice_Voice_34_0002_99.amr"), []byte("#!AMR\namr-frames"), 0)
	assertDirNames(t, media, "tgid_alice_Voice_34_0001_4.voice", "tgid_alice_Voice_34_0002_99.amr")
}

func TestExportOwnerSender(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	out := filepath.Join(e.Tmp, "archive")
	e.run(t, fixtureArgs("export", "tgid_alice", "--output", out)...).mustSucceed(t)
	for _, row := range readExportJSON(t, filepath.Join(out, "chat.json")) {
		want := "爱丽丝"
		switch {
		case slices.Contains(ownerRows, row.Time):
			want = "我"
		case row.Type == 10000 || row.Type == 10002:
			want = "系统"
		}
		if row.Sender != want {
			t.Errorf("%s: sender %q, want %q", row.Time, row.Sender, want)
		}
	}
}

// TestExportVoiceDecoder pins the external decoder protocol and the WAV
// output it enables.
func TestExportVoiceDecoder(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	bin := filepath.Join(e.Tmp, "bin")
	if err := os.Mkdir(bin, 0o700); err != nil {
		t.Fatal(err)
	}
	decoder := filepath.Join(bin, "voice-decoder")
	script := `#!/bin/sh
printf '%s\n' "$@" > "$0.args"
while [ $# -gt 0 ]; do
  case "$1" in
    -i) in="$2"; shift ;;
    -o) out="$2"; shift ;;
  esac
  shift
done
cp "$in" "$0.input"
printf 'RIFF\044\000\000\000WAVEfmt \020\000\000\000\001\000\001\000\300\135\000\000\200\273\000\000\002\000\020\000data\000\000\000\000' > "$out"
`
	if err := os.WriteFile(decoder, []byte(script), 0o700); err != nil {
		t.Fatal(err)
	}
	e.Extra = []string{"TG_VOICE_DECODER=" + decoder}
	out := filepath.Join(e.Tmp, "archive")
	r := e.run(t, fixtureArgs("export", "tgid_alice", "--output", out)...).mustSucceed(t)
	r.stdoutHas(t, "tgid_alice_Voice_34_0001_4.wav", "tgid_alice_Voice_34_0002_99.amr")

	media := filepath.Join(out, "media")
	wav, err := os.ReadFile(filepath.Join(media, "tgid_alice_Voice_34_0001_4.wav"))
	if err != nil {
		t.Fatal(err)
	}
	if len(wav) != 44 || string(wav[:4]) != "RIFF" || string(wav[8:12]) != "WAVE" {
		t.Errorf("WAV output is not the decoder's file: %q", wav)
	}
	assertDirNames(t, media, "tgid_alice_Voice_34_0001_4.wav", "tgid_alice_Voice_34_0002_99.amr")

	args, err := os.ReadFile(decoder + ".args")
	if err != nil {
		t.Fatal(err)
	}
	argv := strings.Split(strings.TrimSuffix(string(args), "\n"), "\n")
	if len(argv) != 10 || argv[0] != "decode" || argv[1] != "-i" || argv[3] != "-o" ||
		argv[4] != filepath.Join(media, "tgid_alice_Voice_34_0001_4.wav") ||
		strings.Join(argv[5:], " ") != "--sample-rate 24000 --tolerant skip --wav" {
		t.Errorf("decoder argv %q, want decode -i <in> -o <wav> --sample-rate 24000 --tolerant skip --wav", argv)
	}
	input, err := os.ReadFile(decoder + ".input")
	if err != nil {
		t.Fatal(err)
	}
	if string(input) != string(nativeVoice("native-voice-frames")[1:]) {
		t.Errorf("decoder input %q is not the unpadded native voice", input)
	}

	// A decoder that exits 0 without writing a WAV falls back to .voice.
	if err := os.WriteFile(decoder, []byte("#!/bin/sh\nexit 0\n"), 0o700); err != nil {
		t.Fatal(err)
	}
	out2 := filepath.Join(e.Tmp, "archive2")
	r = e.run(t, fixtureArgs("export", "tgid_alice", "--output", out2)...).mustSucceed(t)
	r.stdoutHas(t, "tgid_alice_Voice_34_0001_4.voice")
	if !strings.Contains(r.Stderr, "preserving native voice") {
		t.Errorf("stderr lacks the fallback warning:\n%s", r.Stderr)
	}
}

// TestExportCachedVideoCover checks that cached media is exported under its
// [img:<id>]-style id when a local media cache exists.
func TestExportCachedVideoCover(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	mediaCache(t, e)
	e.Extra = []string{"TG_VOICE_DECODER=/nonexistent/e2e/voice-decoder", "TG_FFMPEG=/nonexistent/e2e/ffmpeg"}
	out := filepath.Join(e.Tmp, "archive")
	r := e.run(t, fixtureArgs("export", "tgid_alice", "--output", out)...).mustSucceed(t)
	r.stdoutHas(t, "Media #2: "+videoID+".jpg\n", "Exported 4 media files\n")
	assertFile(t, filepath.Join(out, "media", videoID+".jpg"), jpegBytes[:300], 0)
}

func TestExportErrors(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	out := filepath.Join(e.Tmp, "archive")
	e.run(t, fixtureArgs("export", "gh_news", "--output", out)...).mustFail(t, 1, "No messages found for 'gh_news'")
	if _, err := os.Stat(out); err == nil {
		t.Error("a failed export created its output directory")
	}
	e.run(t, fixtureArgs("export", "nobody-at-all", "--output", out)...).mustFail(t, 1, "nobody-at-all")
	e.run(t, fixtureArgs("export")...).mustFail(t, 2)
}

func assertDirNames(t *testing.T, dir string, want ...string) {
	t.Helper()
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Error(err)
		return
	}
	var got []string
	for _, entry := range entries {
		got = append(got, entry.Name())
	}
	slices.Sort(got)
	slices.Sort(want)
	if !slices.Equal(got, want) {
		t.Errorf("%s holds %q, want %q", dir, got, want)
	}
}
