package media

import (
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
)

// FindBase returns the account directory that holds the media cache: the
// first account directory with `Message/MessageTemp` (3.x) or `msg` (4.x).
// It reports false when none exists.
func FindBase(e env.Env) (string, bool) {
	for _, root := range dict.AccountFilesCandidateDirs(e.UserHome) {
		entries, err := os.ReadDir(root)
		if err != nil {
			continue
		}
		for _, entry := range entries {
			account := filepath.Join(root, entry.Name())
			if !isDir(account) {
				continue
			}
			if isDir(filepath.Join(account, "Message/MessageTemp")) || isDir(filepath.Join(account, "msg")) {
				return account, true
			}
		}
	}
	return "", false
}

// FindCachedSticker finds `cache/<month>/Emoticon/<md5[:2]>/<md5>` under
// base, matching md5 case-insensitively.
func FindCachedSticker(base, md5 string) (string, bool) {
	md5 = strings.ToLower(strings.TrimSpace(md5))
	if len(md5) < 2 {
		return "", false
	}
	months, err := os.ReadDir(filepath.Join(base, "cache"))
	if err != nil {
		return "", false
	}
	for _, month := range months {
		p := filepath.Join(base, "cache", month.Name(), "Emoticon", md5[:2], md5)
		if isFile(p) {
			return p, true
		}
	}
	return "", false
}

// ExportKind is the outcome of exporting one cached file.
type ExportKind int

const (
	// Written means the file is at CachedExport.Path.
	Written ExportKind = iota
	// NeedsJPEG means CachedExport.HEVC must be converted (HEVCsToJPEG) and
	// written to CachedExport.Path.
	NeedsJPEG
	// SkippedEncrypted means the file is V2-encrypted and no keys were
	// given; nothing was written. Copying the ciphertext would leave an
	// unusable file that looks exported.
	SkippedEncrypted
)

// CachedExport is what ExportCached did with one file.
type CachedExport struct {
	Kind ExportKind
	Path string
	HEVC []byte
}

// ExportCached exports src into dir as `<stem>.<ext>`. V2 files are
// decrypted with keys (nil when they could not be derived) and named by
// their content; TGGF content is returned as NeedsJPEG so callers can batch
// conversions. Other files are copied (see copyPlain for their extension).
func ExportCached(src, dir, stem string, keys *Keys) (CachedExport, error) {
	v2, err := FileIsV2(src)
	if err != nil {
		return CachedExport{}, err
	}
	if !v2 {
		p, err := copyPlain(src, dir, stem)
		return CachedExport{Kind: Written, Path: p}, err
	}
	if keys == nil {
		return CachedExport{Kind: SkippedEncrypted}, nil
	}
	data, err := os.ReadFile(src)
	if err != nil {
		return CachedExport{}, fmt.Errorf("read %s: %w", src, err)
	}
	dec, err := DecryptV2(data, *keys)
	if err != nil {
		return CachedExport{}, fmt.Errorf("decrypt %s: %w", src, err)
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return CachedExport{}, fmt.Errorf("create dir %s: %w", dir, err)
	}
	if dec.Format.Kind == TGGF {
		hevc, err := TGGFPartition(dec.Data)
		if err != nil {
			return CachedExport{}, err
		}
		return CachedExport{Kind: NeedsJPEG, Path: filepath.Join(dir, stem+".jpg"), HEVC: hevc}, nil
	}
	ext := dec.Format.Ext
	if dec.Format.Kind == Unknown {
		ext = "bin"
	}
	dest := filepath.Join(dir, stem+"."+ext)
	if err := os.WriteFile(dest, dec.Data, 0o644); err != nil {
		return CachedExport{}, fmt.Errorf("write %s: %w", dest, err)
	}
	return CachedExport{Kind: Written, Path: dest}, nil
}

// copyPlain copies an unencrypted cached file. A `.dat` container holding a
// recognized image or video is saved under the extension its content shows,
// so the export opens in a viewer; anything else keeps its own extension.
// (Added for the Go export: Rust copied plain `.dat` files as `.dat`.)
func copyPlain(src, dir, stem string) (string, error) {
	if _, ext := splitName(filepath.Base(src)); strings.EqualFold(ext, "dat") {
		head, err := readHead(src, 16)
		if err != nil {
			return "", fmt.Errorf("read %s: %w", src, err)
		}
		if f := Classify(head); f.Kind == Plain {
			dest := filepath.Join(dir, stem+"."+f.Ext)
			if err := os.MkdirAll(dir, 0o755); err != nil {
				return "", fmt.Errorf("cannot create media dir: %w", err)
			}
			if err := copyFile(src, dest); err != nil {
				return "", fmt.Errorf("cannot copy media file: %w", err)
			}
			return dest, nil
		}
	}
	return CopyFile(src, dir, stem)
}

func readHead(path string, n int) ([]byte, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	buf := make([]byte, n)
	m, err := io.ReadFull(f, buf)
	if err != nil && !errors.Is(err, io.ErrUnexpectedEOF) && !errors.Is(err, io.EOF) {
		return nil, err
	}
	return buf[:m], nil
}

// CopyFile copies src to `<dir>/<stem>.<ext>`, keeping src's extension
// ("bin" when it has none) and permission bits.
func CopyFile(src, dir, stem string) (string, error) {
	_, ext := splitName(filepath.Base(src))
	if ext == "" {
		ext = "bin"
	}
	dest := filepath.Join(dir, stem+"."+ext)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", fmt.Errorf("cannot create media dir: %w", err)
	}
	if err := copyFile(src, dest); err != nil {
		return "", fmt.Errorf("cannot copy media file: %w", err)
	}
	return dest, nil
}

func copyFile(src, dest string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	st, err := in.Stat()
	if err != nil {
		return err
	}
	out, err := os.OpenFile(dest, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, st.Mode().Perm())
	if err != nil {
		return err
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	return out.Close()
}

// splitName splits a file name like Rust's Path::file_stem/extension: the
// extension follows the last dot unless that dot starts the name.
func splitName(name string) (stem, ext string) {
	if name == ".." {
		return name, ""
	}
	i := strings.LastIndexByte(name, '.')
	if i <= 0 {
		return name, ""
	}
	return name[:i], name[i+1:]
}

func isDir(p string) bool {
	st, err := os.Stat(p)
	return err == nil && st.IsDir()
}

func isFile(p string) bool {
	st, err := os.Stat(p)
	return err == nil && st.Mode().IsRegular()
}
