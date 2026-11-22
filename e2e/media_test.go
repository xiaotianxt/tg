package e2e

import (
	"crypto/md5"
	"encoding/hex"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const videoID = "aaaabbbbccccddddeeeeffff00001111"

var (
	jpegBytes = append([]byte{0xFF, 0xD8, 0xFF, 0xE0}, []byte(strings.Repeat("j", 2044))...)
	pngBytes  = append([]byte{0x89, 'P', 'N', 'G', '\r', '\n', 0x1A, '\n'}, []byte(strings.Repeat("p", 92))...)
)

// mediaCache lays out the desktop app's local media cache under e.Home: the
// alice image in original and standard sizes, the group image as a thumbnail
// only, a video cover, and an undecodable file.
func mediaCache(t *testing.T, e *Env) string {
	t.Helper()
	base := filepath.Join(e.Home, "Documents", accountFilesDir, accountDir)
	chatDir := func(username string) string {
		sum := md5.Sum([]byte(username))
		return filepath.Join(base, "msg", "attach", hex.EncodeToString(sum[:]))
	}
	files := map[string][]byte{
		filepath.Join(chatDir(alice), "2024-06", "Img", imageID+"_h.dat"):               jpegBytes,
		filepath.Join(chatDir(alice), "2024-05", "Img", imageID+".dat"):                 jpegBytes[:100],
		filepath.Join(chatDir(alice), "2024-06", "Img", strings.Repeat("1", 32)+".dat"): []byte("not an image"),
		filepath.Join(chatDir(team), "2024-06", "Img", groupImage+"_t.dat"):             pngBytes,
		filepath.Join(base, "msg", "video", "2024-06", videoID+".jpg"):                  jpegBytes[:300],
	}
	for path, data := range files {
		if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, data, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	return base
}

func TestMedia(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	mediaCache(t, e)
	out := filepath.Join(e.Tmp, "out")

	// The original copy wins over the standard one; the whole tag is accepted.
	r := e.run(t, fixtureArgs("media", "爱丽丝", "[img:"+imageID+"]", videoID, "--output", out)...).mustSucceed(t)
	want := imageID + "  original  2KB  " + filepath.Join(out, imageID+".jpg") + "\n" +
		videoID + "  large video cover  300B  " + filepath.Join(out, videoID+".jpg") + "\n"
	if r.Stdout != want {
		t.Errorf("stdout:\n%s\nwant:\n%s", r.Stdout, want)
	}
	assertFile(t, filepath.Join(out, imageID+".jpg"), jpegBytes, 0o600)
	assertMode(t, out, 0o700)

	// Only a thumbnail: saved, with a hint to download the full image.
	r = e.run(t, fixtureArgs("media", "team@chatroom", groupImage)...).mustSucceed(t)
	defaultDir := filepath.Join(e.Tmp, "tg-media")
	r.stdoutHas(t, groupImage+"  thumbnail  100B  "+filepath.Join(defaultDir, groupImage+".png")+"\n",
		"only the thumbnail is cached")
	assertFile(t, filepath.Join(defaultDir, groupImage+".png"), pngBytes, 0o600)
}

func TestMediaUnavailable(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	mediaCache(t, e)
	out := filepath.Join(e.Tmp, "out")
	missing := strings.Repeat("f", 32)
	junk := strings.Repeat("1", 32)

	// Every id is attempted; the run fails if any could not be saved. Cache
	// directories are per chat, so another chat's image is not found.
	r := e.run(t, fixtureArgs("media", "tgid_alice", missing, imageID, junk, groupImage, "--output", out)...)
	r.mustFail(t, 1, "3 of 4 media ids could not be saved")
	r.stdoutHas(t,
		missing+"  unavailable: not in the local cache (never downloaded, or cleared)\n",
		imageID+"  original  2KB  ",
		junk+"  unavailable: unsupported image format",
		groupImage+"  unavailable: not in the local cache")
	if _, err := os.Stat(filepath.Join(out, imageID+".jpg")); err != nil {
		t.Error(err)
	}
}

func TestMediaErrors(t *testing.T) {
	t.Parallel()
	e := newEnv(t)

	// Ids are validated before any database is opened.
	r := e.run(t, "media", "tgid_alice", "../../etc/passwd", "--db-dir", "/nonexistent", "--keys", "/nonexistent")
	r.mustFail(t, 1, "'../../etc/passwd' is not a media id")
	e.run(t, fixtureArgs("media", "tgid_alice", strings.ToUpper(imageID))...).mustFail(t, 1, "is not a media id")
	e.run(t, fixtureArgs("media", "tgid_alice")...).mustFail(t, 2)

	// No cache at all under HOME.
	r = e.run(t, fixtureArgs("media", "tgid_alice", imageID)...)
	r.mustFail(t, 1, "Local media cache not found")
}

// assertFile checks content, and the permission bits unless mode is 0.
func assertFile(t *testing.T, path string, want []byte, mode os.FileMode) {
	t.Helper()
	got, err := os.ReadFile(path)
	if err != nil {
		t.Error(err)
		return
	}
	if string(got) != string(want) {
		t.Errorf("%s: content differs (%d bytes, want %d)", path, len(got), len(want))
	}
	if mode != 0 {
		assertMode(t, path, mode)
	}
}

func assertMode(t *testing.T, path string, mode os.FileMode) {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Error(err)
		return
	}
	if info.Mode().Perm() != mode {
		t.Errorf("%s: mode %o, want %o", path, info.Mode().Perm(), mode)
	}
}
