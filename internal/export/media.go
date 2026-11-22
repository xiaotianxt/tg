package export

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"github.com/xiaotianxt/tg/internal/decode"
	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/logx"
	"github.com/xiaotianxt/tg/internal/media"
	"github.com/xiaotianxt/tg/internal/par"
	"github.com/xiaotianxt/tg/internal/store"
	"github.com/xiaotianxt/tg/internal/voice"
)

// MediaKind is the state of one media item in the export pipeline.
type MediaKind int

const (
	// MediaWritten: the file is at Path.
	MediaWritten MediaKind = iota
	// MediaNeedsJPEG: an HEVC picture waits for the JPEG stage, which
	// writes it to Path. No outcome leaves exportMedia in this state.
	MediaNeedsJPEG
	// MediaSkippedEncrypted: a V2-encrypted file that could not be
	// decrypted because media keys were unavailable; nothing was written.
	MediaSkippedEncrypted
	// MediaFailed: Err says why nothing was written.
	MediaFailed
)

// MediaOutcome is what happened to one numbered media item.
type MediaOutcome struct {
	Index int
	Kind  MediaKind
	Path  string
	Err   error

	sticker bool
	hevc    []byte
}

func written(index int, path string) MediaOutcome {
	return MediaOutcome{Index: index, Kind: MediaWritten, Path: path}
}

func failed(index int, err error) MediaOutcome {
	return MediaOutcome{Index: index, Kind: MediaFailed, Err: err}
}

// jpegDone moves a MediaNeedsJPEG outcome to Written or Failed.
func (m *MediaOutcome) jpegDone(jpeg []byte, err error) {
	if m.Kind != MediaNeedsJPEG {
		panic("jpegDone on an outcome that is not waiting for JPEG")
	}
	m.hevc = nil
	if err == nil {
		if werr := os.WriteFile(m.Path, jpeg, 0o644); werr != nil {
			err = fmt.Errorf("write %s: %w", m.Path, werr)
		}
	}
	if err != nil {
		m.Kind, m.Path, m.Err = MediaFailed, "", err
		return
	}
	m.Kind = MediaWritten
}

// mediaSources is the local media cache of one chat, when present.
type mediaSources struct {
	base  string
	ok    bool
	index *media.Index
}

func findMediaSources(e env.Env, username string, jobs int) mediaSources {
	base, ok := media.FindBase(e)
	if !ok {
		return mediaSources{}
	}
	return mediaSources{base: base, ok: true, index: media.LoadIndex(base, username, []media.Category{media.Image, media.Video}, jobs)}
}

// jobKind is what a media job exports.
type jobKind int

const (
	jobCached jobKind = iota
	jobSticker
)

// mediaJob is one planned media export, numbered from 1.
type mediaJob struct {
	index int
	kind  jobKind
	src   string
	// category and stem (output name without extension) are for cached
	// jobs; sticker for sticker jobs.
	category media.Category
	stem     string
	sticker  decode.StickerInfo
}

// planMedia finds the cached file for every media message, numbering jobs
// from 1 in export order: images, videos, new stickers, then files.
//
// Images and videos are written as `<id>.<ext>`, the id their placeholder
// shows, so the transcript leads to the file; a repeated id is exported
// once.
func planMedia(msgs []message, src mediaSources, username string, jobs int) []mediaJob {
	if !src.ok {
		return nil
	}
	var out []mediaJob
	add := func(j mediaJob) {
		j.index = len(out) + 1
		out = append(out, j)
	}

	seen := map[string]bool{}
	for _, cat := range []struct {
		category  media.Category
		localType int64
	}{{media.Image, 3}, {media.Video, 43}} {
		for _, m := range msgs {
			if m.Type != cat.localType {
				continue
			}
			id := mediaIdentifier(m)
			if id == "" || !fileSafeID(id) || seen[id] {
				continue
			}
			seen[id] = true
			if p, ok := src.index.Find(cat.category, id); ok {
				add(mediaJob{kind: jobCached, src: p, category: cat.category, stem: id})
			}
		}
	}

	seenStickers := map[string]bool{}
	for _, m := range msgs {
		if m.Type != 47 {
			continue
		}
		info := decode.ParseStickerInfo(m.rawBody)
		if info.MD5 == "" {
			continue
		}
		if id := info.Identifier(); id != "" {
			if seenStickers[id] {
				continue
			}
			seenStickers[id] = true
		}
		if p, ok := media.FindCachedSticker(src.base, info.MD5); ok {
			add(mediaJob{kind: jobSticker, src: p, sticker: info})
		}
	}

	var files *media.Index
	for _, m := range msgs {
		if decode.MediaType(m.Type, m.rawBody) != "file" {
			continue
		}
		id := fileIdentifier(m)
		if id == "" {
			continue
		}
		if files == nil {
			// File directories are indexed only when a file message needs them.
			files = media.LoadIndex(src.base, username, []media.Category{media.File}, jobs)
		}
		if p, ok := files.Find(media.File, id); ok {
			stem := fmt.Sprintf("%s_File_%d_%04d", decode.SanitizeFilename(username), fileExportType(m.Type), len(out)+1)
			add(mediaJob{kind: jobCached, src: p, category: media.File, stem: stem})
		}
	}
	return out
}

// mediaIdentifier is the cache id of an image or video message: the packed
// metadata filename, else the XML aeskey, else the cdnthumburl.
func mediaIdentifier(m message) string {
	if len(m.packed) > 0 {
		if v2, ok := decode.ParsePackedV2(m.packed); ok {
			if v2.Image != nil && v2.Image.Filename != "" {
				return v2.Image.Filename
			}
			if v2.Video != nil && v2.Video.Filename != "" {
				return v2.Video.Filename
			}
		}
		if v1, ok := decode.ParsePackedV1(m.packed); ok && v1.Filename != "" {
			return v1.Filename
		}
	}
	if v, ok := decode.ExtractXMLAttr(m.rawBody, "aeskey"); ok {
		return v
	}
	v, _ := decode.ExtractXMLAttr(m.rawBody, "cdnthumburl")
	return v
}

// fileSafeID reports whether a media id can be a file name as is. Cache ids
// are short and file-safe; the CDN-URL fallback id is not, and those
// messages show a placeholder no file can match anyway.
func fileSafeID(id string) bool {
	if len(id) > 128 || strings.HasPrefix(id, ".") {
		return false
	}
	for i := 0; i < len(id); i++ {
		c := id[i]
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c == '-' || c == '_' || c == '.') {
			return false
		}
	}
	return true
}

// fileIdentifier names a file message's attachment: the packed metadata
// filename, else the XML title, else the basename of a plain path body.
func fileIdentifier(m message) string {
	if len(m.packed) > 0 {
		if v2, ok := decode.ParsePackedV2(m.packed); ok && v2.File != nil {
			if id := v2.File.Identifier(); id != "" {
				return id
			}
		}
	}
	if info, ok := decode.ParseFileInfo(m.rawBody); ok && strings.TrimSpace(info.Title) != "" {
		return info.Title
	}
	body := strings.TrimSpace(m.rawBody)
	if body == "" || strings.HasPrefix(body, "<") {
		return ""
	}
	return strings.TrimSpace(body[strings.LastIndexByte(body, '/')+1:])
}

func fileExportType(localType int64) int {
	if sub, ok := decode.AppMessageSubtype(localType); localType == 62 || ok && sub == 62 {
		return 62
	}
	return 49
}

// exportMedia runs the planned jobs into dir, converts pending HEVC
// pictures in one batched JPEG stage, and warns about what was not saved.
func exportMedia(e env.Env, src mediaSources, jobs []mediaJob, dir, username string, jobCount int) []MediaOutcome {
	if !src.ok {
		logx.Warnf("%s data directory not found, cached image/video/file export skipped", dict.AppDisplayName)
		return nil
	}
	if len(jobs) == 0 {
		return nil
	}
	var keys *media.Keys
	var samples []string
	cached := false
	for _, j := range jobs {
		if j.kind == jobCached {
			cached = true
			if j.category != media.File {
				samples = append(samples, j.src)
			}
		}
	}
	if cached {
		// Keys are verified against this export's cached images and videos.
		if k, err := media.FindKeys(e, src.base, samples); err != nil {
			logx.Warnf("Cannot derive media decryption keys: %v", err)
		} else {
			keys = &k
		}
	}
	ffmpeg := sync.OnceValues(func() (string, error) { return media.FFmpegPath(e) })

	workers := par.Jobs(jobCount, 8)
	outcomes := par.Map(jobs, workers, func(j mediaJob) MediaOutcome {
		if j.kind == jobSticker {
			o := exportSticker(j, dir, username, ffmpeg)
			o.sticker = true
			return o
		}
		c, err := media.ExportCached(j.src, dir, j.stem, keys)
		switch {
		case err != nil:
			return failed(j.index, err)
		case c.Kind == media.NeedsJPEG:
			return MediaOutcome{Index: j.index, Kind: MediaNeedsJPEG, Path: c.Path, hevc: c.HEVC}
		case c.Kind == media.SkippedEncrypted:
			return MediaOutcome{Index: j.index, Kind: MediaSkippedEncrypted}
		default:
			return written(j.index, c.Path)
		}
	})

	convertJPEGs(outcomes, ffmpeg, workers)

	var stickerFailures, encrypted int
	var firstStickerErr error
	for _, o := range outcomes {
		switch o.Kind {
		case MediaFailed:
			if o.sticker {
				stickerFailures++
				if firstStickerErr == nil {
					firstStickerErr = o.Err
				}
			} else {
				logx.Warnf("  Media #%d export failed: %v", o.Index, o.Err)
			}
		case MediaSkippedEncrypted:
			encrypted++
		}
	}
	if stickerFailures > 0 {
		logx.Warnf("Skipped %d cached stickers: %v", stickerFailures, firstStickerErr)
	}
	if encrypted > 0 {
		logx.Warnf("Skipped %d encrypted media files: media decryption keys are unavailable", encrypted)
	}
	return outcomes
}

// convertJPEGs is the JPEG stage: every MediaNeedsJPEG outcome is converted
// (batched per ffmpeg process) and becomes Written or Failed.
func convertJPEGs(outcomes []MediaOutcome, ffmpeg func() (string, error), jobs int) {
	var pending []int
	var hevcs [][]byte
	for i, o := range outcomes {
		if o.Kind == MediaNeedsJPEG {
			pending = append(pending, i)
			hevcs = append(hevcs, o.hevc)
		}
	}
	if len(pending) == 0 {
		return
	}
	path, err := ffmpeg()
	if err != nil {
		for _, i := range pending {
			outcomes[i].jpegDone(nil, err)
		}
		return
	}
	for k, res := range media.HEVCsToJPEG(path, hevcs, jobs) {
		outcomes[pending[k]].jpegDone(res.JPEG, res.Err)
	}
}

// exportSticker writes a cached sticker as `<user>_Sticker_47_NNNN.<ext>`:
// as stored when its format is recognized, else decrypted with the
// sticker's AES key; TGGF pictures are converted to JPEG.
func exportSticker(j mediaJob, dir, username string, ffmpeg func() (string, error)) MediaOutcome {
	data, err := os.ReadFile(j.src)
	if err != nil {
		return failed(j.index, fmt.Errorf("read cached sticker %s: %w", j.src, err))
	}
	write := func(data []byte, ext string) MediaOutcome {
		if ext == "tggf" {
			path, err := ffmpeg()
			if err != nil {
				return failed(j.index, err)
			}
			if data, err = media.TGGFToJPEG(path, data); err != nil {
				return failed(j.index, err)
			}
			ext = "jpg"
		}
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return failed(j.index, fmt.Errorf("cannot create media dir: %w", err))
		}
		dest := filepath.Join(dir, fmt.Sprintf("%s_Sticker_47_%04d.%s", decode.SanitizeFilename(username), j.index, ext))
		if err := os.WriteFile(dest, data, 0o644); err != nil {
			return failed(j.index, fmt.Errorf("write sticker %s: %w", dest, err))
		}
		return written(j.index, dest)
	}
	if ext := media.DetectExt(data); ext != "bin" {
		return write(data, ext)
	}
	if j.sticker.AESKey != "" {
		if plain, ok := media.DecryptStickerCBC(data, j.sticker.AESKey); ok {
			if ext := media.DetectExt(plain); ext != "bin" {
				return write(plain, ext)
			}
		}
	}
	return failed(j.index, errors.New("cached sticker could not be decoded"))
}

// exportVoices saves the chat's voice messages, numbered from start: native
// voice decoded to WAV when the decoder works, else kept as .voice; other
// payloads as is. A voice message that cannot be written fails the export.
func exportVoices(e env.Env, st *store.Store, username, dir string, start int) ([]MediaOutcome, error) {
	const mediaDB = "message/media_0.db"
	if !st.Has(mediaDB) {
		logx.Warnf("Voice media database not found; cached voice export skipped")
		return nil, nil
	}
	var dec *voice.Decoder
	if d, err := voice.ResolveDecoder(e); err != nil {
		logx.Warnf("Voice WAV decoder unavailable; preserving native voice files: %v", err)
	} else {
		dec = &d
	}
	conn, err := st.Open(mediaDB)
	if err != nil {
		return nil, err
	}
	rows, err := voice.LoadRows(conn, username)
	conn.Close()
	if err != nil {
		return nil, err
	}
	out := make([]MediaOutcome, 0, len(rows))
	for i, row := range rows {
		index := start + i
		p, ok := voice.Sniff(row.Data)
		if !ok {
			return nil, fmt.Errorf("Voice #%d export failed: Voice data is empty", index)
		}
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return nil, fmt.Errorf("Voice #%d export failed: cannot create voice dir: %w", index, err)
		}
		res, err := voice.Export(p, dec, func(ext string) string {
			return filepath.Join(dir, fmt.Sprintf("%s_Voice_34_%04d_%d.%s", decode.SanitizeFilename(username), index, row.LocalID, ext))
		})
		if err != nil {
			return nil, fmt.Errorf("Voice #%d export failed: %w", index, err)
		}
		if res.Outcome == voice.Native && !errors.Is(res.Reason, voice.ErrNoDecoder) {
			logx.Warnf("Voice #%d WAV decode failed; preserving native voice: %v", index, res.Reason)
		}
		out = append(out, written(index, res.Path))
	}
	return out, nil
}
