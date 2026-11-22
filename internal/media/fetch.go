package media

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"

	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/paths"
)

// Size selects which cached copy of an image Cache.Load prefers.
type Size int

const (
	// Preview is the client's standard-size copy, falling back to the
	// thumbnail.
	Preview Size = iota
	// Original is the full-resolution copy when it was downloaded.
	Original
)

// CachedImage is one image (or video cover) read from the cache.
type CachedImage struct {
	MIME string
	// Ext matches MIME: jpg, png, gif or webp.
	Ext  string
	Data []byte
	// Variant names the copy used, e.g. "standard" or "thumbnail".
	Variant string
}

// IsMediaID reports whether id is a cache id: 32 lowercase hex characters.
// Anything else is rejected before it can reach a path.
func IsMediaID(id string) bool {
	if len(id) != 32 {
		return false
	}
	for i := 0; i < len(id); i++ {
		if c := id[i]; !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}

// ParseMediaID accepts a bare id or the whole `[img:<id>]` tag, which people
// and models often copy as is.
func ParseMediaID(raw string) (string, error) {
	id := strings.TrimSpace(raw)
	for strings.HasPrefix(id, "[img:") {
		id = id[len("[img:"):]
	}
	id = strings.TrimRight(id, "]")
	if !IsMediaID(id) {
		return "", fmt.Errorf("'%s' is not a media id; pass the 32-character id from [img:<id>]", raw)
	}
	return id, nil
}

// DefaultOutputDir is where `tg media` writes decrypted copies unless told
// otherwise. The system temp directory is per user on macOS and cleaned up
// by the OS.
func DefaultOutputDir(e env.Env) string { return filepath.Join(e.TempDir, "tg-media") }

// WritePrivate writes img to `<dir>/<id>.<ext>` with mode 0600 in a 0700
// directory, replacing an earlier copy of the same id.
func WritePrivate(dir, id string, img CachedImage) (string, error) {
	if err := paths.EnsurePrivateDir(dir); err != nil {
		return "", fmt.Errorf("cannot create %s: %w", dir, err)
	}
	p := filepath.Join(dir, id+"."+img.Ext)
	f, err := os.OpenFile(p, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o600)
	if err == nil {
		_, err = f.Write(img.Data)
		if cerr := f.Close(); err == nil {
			err = cerr
		}
	}
	if err != nil {
		return "", fmt.Errorf("cannot write %s: %w", p, err)
	}
	return p, nil
}

// ErrNoMediaBase means the local media cache directory does not exist.
var ErrNoMediaBase = errors.New("Local media cache not found; images are only available on this computer")

// ErrNotCached means no cached copy of the id exists in any month the cache
// covers.
var ErrNotCached = errors.New("not in the local cache (never downloaded, or cleared)")

// Cache loads cached images of one chat by id, for `tg media` and the MCP
// get_media tool.
type Cache struct {
	env  env.Env
	base string
	// imageDirs are `msg/attach/<md5(chat)>/<YYYY-MM>/Img`, newest first.
	imageDirs []string
	// videoDirs are `msg/video/<YYYY-MM>`, newest first.
	videoDirs []string

	keysOnce sync.Once
	keys     Keys
	keysErr  error
}

// OpenCache opens the media cache of username. With minMonth (`YYYY-MM`,
// "" for none), month directories before it are invisible.
func OpenCache(e env.Env, username, minMonth string) (*Cache, error) {
	base, ok := FindBase(e)
	if !ok {
		return nil, ErrNoMediaBase
	}
	return newCache(e, base, username, minMonth), nil
}

func newCache(e env.Env, base, username, minMonth string) *Cache {
	chat := filepath.Join(base, "msg/attach", md5Hex(username))
	c := &Cache{env: e, base: base}
	for _, m := range monthDirs(chat, minMonth) {
		c.imageDirs = append(c.imageDirs, filepath.Join(m, "Img"))
	}
	c.videoDirs = monthDirs(filepath.Join(base, "msg/video"), minMonth)
	return c
}

// Location is a cached file Find chose.
type Location struct {
	Path    string
	Variant string
}

// Find returns the first cached copy of id in variant order (see
// candidates), searching months newest first. It returns ErrNotCached when
// none exists. id must satisfy IsMediaID.
func (c *Cache) Find(id string, size Size) (Location, error) {
	if !IsMediaID(id) {
		panic("media id must be validated by the caller")
	}
	for _, cand := range candidates(id, size) {
		dirs := c.imageDirs
		if cand.video {
			dirs = c.videoDirs
		}
		for _, d := range dirs {
			if p := filepath.Join(d, cand.name); isFile(p) {
				return Location{Path: p, Variant: cand.variant}, nil
			}
		}
	}
	return Location{}, ErrNotCached
}

// Load returns the image cached under id as JPEG, PNG, GIF or WebP,
// decrypting V2 files and converting TGGF pictures. Errors: ErrNotCached
// when absent, otherwise a description of why the copy cannot be read.
func (c *Cache) Load(id string, size Size) (CachedImage, error) {
	loc, err := c.Find(id, size)
	if err != nil {
		return CachedImage{}, err
	}
	data, err := os.ReadFile(loc.Path)
	if err != nil {
		return CachedImage{}, fmt.Errorf("cannot read cached file: %w", err)
	}
	f := Classify(data)
	if f.Kind == V2 {
		keys, err := c.mediaKeys(loc.Path)
		if err != nil {
			return CachedImage{}, fmt.Errorf("cannot derive media keys: %w", err)
		}
		dec, err := DecryptV2(data, keys)
		if err != nil {
			return CachedImage{}, err
		}
		data, f = dec.Data, dec.Format
	}
	if f.Kind == TGGF {
		ffmpeg, err := FFmpegPath(c.env)
		if err != nil {
			return CachedImage{}, err
		}
		if data, err = TGGFToJPEG(ffmpeg, data); err != nil {
			return CachedImage{}, err
		}
		f = Format{Kind: Plain, Ext: "jpg"}
	}
	mime, ok := imageMIME(f.Ext)
	if f.Kind != Plain || !ok {
		ext := f.Ext
		if f.Kind == Unknown {
			ext = "bin"
		}
		return CachedImage{}, fmt.Errorf("unsupported image format: %s", ext)
	}
	return CachedImage{MIME: mime, Ext: f.Ext, Data: data, Variant: loc.Variant}, nil
}

// mediaKeys derives keys once per cache, verified against the first V2 file
// loaded.
func (c *Cache) mediaKeys(sample string) (Keys, error) {
	c.keysOnce.Do(func() {
		c.keys, c.keysErr = FindKeys(c.env, c.base, []string{sample})
	})
	return c.keys, c.keysErr
}

type candidate struct {
	name    string
	variant string
	video   bool
}

// candidates lists the file names to try, in order: image copies
// (`<id>.dat`, `<id>_t.dat`, `<id>_h.dat`) then video covers
// (`<id>_thumb.jpg`, `<id>.jpg`), each ordered by size preference.
func candidates(id string, size Size) []candidate {
	type v struct{ suffix, variant string }
	images := []v{{"", "standard"}, {"_t", "thumbnail"}, {"_h", "original"}}
	videos := []v{{"_thumb.jpg", "video cover"}, {".jpg", "large video cover"}}
	if size == Original {
		images = []v{{"_h", "original"}, {"", "standard"}, {"_t", "thumbnail"}}
		videos = []v{{".jpg", "large video cover"}, {"_thumb.jpg", "video cover"}}
	}
	var out []candidate
	for _, i := range images {
		out = append(out, candidate{id + i.suffix + ".dat", i.variant, false})
	}
	for _, i := range videos {
		out = append(out, candidate{id + i.suffix, i.variant, true})
	}
	return out
}

// monthDirs lists the `YYYY-MM` subdirectories of dir from minMonth on,
// newest first.
func monthDirs(dir, minMonth string) []string {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil
	}
	names := make([]string, len(entries))
	for i, e := range entries {
		names[i] = e.Name()
	}
	months := selectMonths(names, minMonth)
	for i, m := range months {
		months[i] = filepath.Join(dir, m)
	}
	return months
}

// selectMonths keeps month names (`YYYY-MM`) not before minMonth ("" keeps
// all) and sorts them newest first.
func selectMonths(names []string, minMonth string) []string {
	var out []string
	for _, n := range names {
		if isMonthName(n) && n >= minMonth {
			out = append(out, n)
		}
	}
	slices.Sort(out)
	slices.Reverse(out)
	return out
}

func isMonthName(s string) bool {
	if len(s) != 7 || s[4] != '-' {
		return false
	}
	for i := 0; i < len(s); i++ {
		if i != 4 && (s[i] < '0' || s[i] > '9') {
			return false
		}
	}
	return true
}
