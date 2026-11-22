package media

import (
	"crypto/md5"
	"encoding/hex"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync/atomic"

	"github.com/xiaotianxt/tg/internal/par"
)

// Category is a kind of cached media the export index covers.
type Category string

const (
	Image Category = "Image"
	Video Category = "Video"
	File  Category = "File"
)

// Index is export's fuzzy lookup of cached media files for one chat.
type Index struct {
	categories map[Category]*categoryIndex
}

type indexedFile struct {
	path      string
	lowerName string
	lowerStem string
	isThumb   bool
	// size+1 once read, 0 before. Only files that compete for one key need
	// it, and statting every cached file dominated the directory walk.
	size atomic.Int64
}

func (f *indexedFile) len() int64 {
	if v := f.size.Load(); v > 0 {
		return v - 1
	}
	var n int64
	if st, err := os.Lstat(f.path); err == nil {
		n = st.Size()
	}
	f.size.Store(n + 1)
	return n
}

// better prefers a non-thumbnail, then the larger file.
func (f *indexedFile) better(than *indexedFile) bool {
	if f.isThumb != than.isThumb {
		return !f.isThumb
	}
	return f.len() > than.len()
}

type categoryIndex struct {
	files       []*indexedFile
	exactNames  map[string]int
	exactStems  map[string]int
	sortedNames []int // indices into files, by lowerName
}

type dirSpec struct {
	path     string
	category Category
}

// LoadIndex scans the media directories of sessionID for categories: 3.x
// `Message/MessageTemp/<session>/<category>`, 4.x `msg/attach/*/*/Img` for
// images, `msg/file` and `msg/attach/<md5(session)>/**/F` for files, and
// `msg/video/*` for videos. Missing directories are skipped.
func LoadIndex(base, sessionID string, categories []Category, jobs int) *Index {
	jobs = par.Jobs(jobs, 8)
	dirs := discoverMediaDirs(base, sessionID, categories, jobs)
	scanned := par.Map(dirs, jobs, func(d dirSpec) []*indexedFile {
		var files []*indexedFile
		scanFiles(d.path, &files)
		return files
	})
	byCategory := map[Category][]*indexedFile{}
	for i, files := range scanned {
		byCategory[dirs[i].category] = append(byCategory[dirs[i].category], files...)
	}
	idx := &Index{categories: map[Category]*categoryIndex{}}
	for c, files := range byCategory {
		idx.categories[c] = newCategoryIndex(files)
	}
	return idx
}

// Find returns the best cached file for identifier in category: an exact
// name or stem match, then the identifier's own stem, then a name prefix,
// then a name substring, all case-insensitive. Among candidates it prefers
// non-thumbnails, then larger files. It reports false when nothing matches
// or the winner is no longer a regular file. Safe for concurrent use.
func (idx *Index) Find(category Category, identifier string) (string, bool) {
	c := idx.categories[category]
	if c == nil {
		return "", false
	}
	return c.find(identifier)
}

func newCategoryIndex(files []*indexedFile) *categoryIndex {
	c := &categoryIndex{
		files:       files,
		exactNames:  map[string]int{},
		exactStems:  map[string]int{},
		sortedNames: make([]int, len(files)),
	}
	for i, f := range files {
		c.insertBest(c.exactNames, f.lowerName, i)
		c.insertBest(c.exactStems, f.lowerStem, i)
		c.sortedNames[i] = i
	}
	slices.SortStableFunc(c.sortedNames, func(a, b int) int {
		return strings.Compare(files[a].lowerName, files[b].lowerName)
	})
	return c
}

func (c *categoryIndex) insertBest(m map[string]int, key string, i int) {
	if cur, ok := m[key]; !ok || c.files[i].better(c.files[cur]) {
		m[key] = i
	}
}

func (c *categoryIndex) find(identifier string) (string, bool) {
	lower := strings.ToLower(strings.TrimSpace(identifier))
	if lower == "" {
		return "", false
	}
	if p, ok := c.findExact(lower); ok {
		return p, true
	}
	if stem := pathStem(lower); stem != "" && stem != lower {
		if p, ok := c.findExact(stem); ok {
			return p, true
		}
	}
	if p, ok := c.findPrefix(lower); ok {
		return p, true
	}
	return c.findContains(lower)
}

func (c *categoryIndex) findExact(needle string) (string, bool) {
	var cands []int
	if i, ok := c.exactNames[needle]; ok {
		cands = append(cands, i)
	}
	if i, ok := c.exactStems[needle]; ok {
		cands = append(cands, i)
	}
	return c.best(cands)
}

func (c *categoryIndex) findPrefix(needle string) (string, bool) {
	start, _ := slices.BinarySearchFunc(c.sortedNames, needle, func(i int, n string) int {
		return strings.Compare(c.files[i].lowerName, n)
	})
	var cands []int
	for _, i := range c.sortedNames[start:] {
		if !strings.HasPrefix(c.files[i].lowerName, needle) {
			break
		}
		cands = append(cands, i)
	}
	return c.best(cands)
}

func (c *categoryIndex) findContains(needle string) (string, bool) {
	var cands []int
	for i, f := range c.files {
		if strings.Contains(f.lowerName, needle) {
			cands = append(cands, i)
		}
	}
	return c.best(cands)
}

// best picks the first candidate no later one beats. Only the winner is
// checked to still be a file, as in the Rust index.
func (c *categoryIndex) best(cands []int) (string, bool) {
	if len(cands) == 0 {
		return "", false
	}
	win := cands[0]
	for _, i := range cands[1:] {
		if c.files[i].better(c.files[win]) {
			win = i
		}
	}
	p := c.files[win].path
	return p, isFile(p)
}

// pathStem is Rust's Path::file_stem of an identifier: the last path
// component without its extension, "" when there is no component.
func pathStem(p string) string {
	p = strings.TrimRight(p, "/")
	if i := strings.LastIndexByte(p, '/'); i >= 0 {
		p = p[i+1:]
	}
	if p == "" || p == "." || p == ".." {
		return ""
	}
	stem, _ := splitName(p)
	return stem
}

// discoverMediaDirs lists candidate directories. A candidate may be missing
// or not a directory; scanning it then finds nothing.
func discoverMediaDirs(base, sessionID string, categories []Category, jobs int) []dirSpec {
	var dirs []dirSpec
	for _, c := range categories {
		if d := filepath.Join(base, "Message/MessageTemp", sessionID, string(c)); isDir(d) {
			dirs = append(dirs, dirSpec{d, c})
		}
	}
	attach := filepath.Join(base, "msg/attach")
	if slices.Contains(categories, Image) && isDir(attach) {
		dirs = append(dirs, discoverAttachImageDirs(attach, jobs)...)
	}
	if slices.Contains(categories, File) {
		if d := filepath.Join(base, "msg/file"); isDir(d) {
			dirs = append(dirs, dirSpec{d, File})
		}
		if session := filepath.Join(attach, md5Hex(sessionID)); isDir(session) {
			walkAttachFileDirs(session, &dirs)
		}
	}
	if slices.Contains(categories, Video) {
		videos := filepath.Join(base, "msg/video")
		entries, _ := os.ReadDir(videos)
		for _, e := range entries {
			if d := filepath.Join(videos, e.Name()); isDir(d) {
				dirs = append(dirs, dirSpec{d, Video})
			}
		}
	}
	slices.SortFunc(dirs, func(a, b dirSpec) int {
		if c := strings.Compare(a.path, b.path); c != 0 {
			return c
		}
		return strings.Compare(string(a.category), string(b.category))
	})
	return slices.Compact(dirs)
}

// discoverAttachImageDirs lists `<chat>/<month>/Img` for every chat without
// statting each Img: the scan's own ReadDir rejects missing ones, in
// parallel. Listing months needs one directory open per chat, so that runs
// in parallel too. ReadDir follows symlinks and fails on non-directories.
func discoverAttachImageDirs(attach string, jobs int) []dirSpec {
	chats, err := os.ReadDir(attach)
	if err != nil {
		return nil
	}
	chatPaths := make([]string, len(chats))
	for i, c := range chats {
		chatPaths[i] = filepath.Join(attach, c.Name())
	}
	perChat := par.Map(chatPaths, jobs, func(chat string) []dirSpec {
		months, _ := os.ReadDir(chat)
		out := make([]dirSpec, len(months))
		for i, m := range months {
			out[i] = dirSpec{filepath.Join(chat, m.Name(), "Img"), Image}
		}
		return out
	})
	var dirs []dirSpec
	for _, d := range perChat {
		dirs = append(dirs, d...)
	}
	return dirs
}

// walkAttachFileDirs collects every directory named F below dir, following
// symlinked directories.
func walkAttachFileDirs(dir string, dirs *[]dirSpec) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return
	}
	for _, e := range entries {
		p := filepath.Join(dir, e.Name())
		isDirectory := e.IsDir()
		if e.Type()&os.ModeSymlink != 0 {
			isDirectory = isDir(p)
		}
		if !isDirectory {
			continue
		}
		if e.Name() == "F" {
			*dirs = append(*dirs, dirSpec{p, File})
		} else {
			walkAttachFileDirs(p, dirs)
		}
	}
}

// scanFiles indexes regular files below dir. Entries are not followed
// through symlinks.
func scanFiles(dir string, files *[]*indexedFile) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return
	}
	for _, e := range entries {
		p := filepath.Join(dir, e.Name())
		if e.IsDir() {
			scanFiles(p, files)
			continue
		}
		if !e.Type().IsRegular() {
			continue
		}
		lower := strings.ToLower(e.Name())
		stem, _ := splitName(lower)
		*files = append(*files, &indexedFile{
			path:      p,
			lowerName: lower,
			lowerStem: stem,
			isThumb:   isThumbName(lower),
		})
	}
}

func isThumbName(lower string) bool {
	return strings.Contains(lower, ".pic_thm") || strings.Contains(lower, "thumb")
}

func md5Hex(s string) string {
	sum := md5.Sum([]byte(s))
	return hex.EncodeToString(sum[:])
}
