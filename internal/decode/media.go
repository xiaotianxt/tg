package decode

import (
	"fmt"
	"math"
	"strconv"
	"strings"
	"unicode"

	"github.com/xiaotianxt/tg/internal/strsearch"
)

func trimSpace(s string) string { return strings.TrimSpace(s) }

func itoa(n int64) string { return strconv.FormatInt(n, 10) }

// parseUint parses like Rust's unsigned `str::parse`: an optional leading
// `+`, then decimal digits that fit in bits.
func parseUint(s string, bits int) (uint64, bool) {
	s = strings.TrimPrefix(s, "+")
	if s == "" || s[0] == '+' {
		return 0, false
	}
	v, err := strconv.ParseUint(s, 10, bits)
	return v, err == nil
}

func attrUint(xml, attr string, bits int) (uint64, bool) {
	v, ok := ExtractXMLAttr(xml, attr)
	if !ok {
		return 0, false
	}
	return parseUint(v, bits)
}

// ImageTag renders `[img:<id>]`, or `[img]` when id is blank.
func ImageTag(id string) string {
	if id = trimSpace(id); id != "" {
		return "[img:" + id + "]"
	}
	return "[img]"
}

// StickerTag renders `[sticker:<id>]`, or `[表情]` when id is blank.
func StickerTag(id string) string {
	if id = trimSpace(id); id != "" {
		return "[sticker:" + id + "]"
	}
	return "[表情]"
}

func firstNonBlank(values ...string) string {
	for _, v := range values {
		if v = trimSpace(v); v != "" {
			return v
		}
	}
	return ""
}

// ImageInfo is an image message (type 3) read from its XML.
type ImageInfo struct {
	AESKey      string
	CDNThumbURL string
	CDNMidURL   string
	CDNBigURL   string
	ThumbWidth  uint32
	ThumbHeight uint32
	RawLength   uint64
}

// Identifier is the trimmed AES key, else the thumbnail URL; "" when none.
func (i ImageInfo) Identifier() string { return firstNonBlank(i.AESKey, i.CDNThumbURL) }

// Display is the image tag.
func (i ImageInfo) Display() string { return ImageTag(i.Identifier()) }

// ParseImageInfo reads image attributes from xml.
func ParseImageInfo(xml string) ImageInfo {
	var info ImageInfo
	info.AESKey, _ = ExtractXMLAttr(xml, "aeskey")
	info.CDNThumbURL, _ = ExtractXMLAttr(xml, "cdnthumburl")
	info.CDNMidURL, _ = ExtractXMLAttr(xml, "cdnmidiurl")
	info.CDNBigURL, _ = ExtractXMLAttr(xml, "cdndisplaybackupurl")
	if v, ok := attrUint(xml, "cdnthumbwidth", 32); ok {
		info.ThumbWidth = uint32(v)
	}
	if v, ok := attrUint(xml, "cdnthumbheight", 32); ok {
		info.ThumbHeight = uint32(v)
	}
	if v, ok := attrUint(xml, "rawlength", 64); ok {
		info.RawLength = v
	}
	if info.RawLength == 0 {
		if v, ok := attrUint(xml, "cdnmidimagerawlength", 64); ok {
			info.RawLength = v
		}
	}
	return info
}

// VideoInfo is a video message (type 43) read from its XML.
type VideoInfo struct {
	AESKey         string
	CDNVideoURL    string
	CDNThumbURL    string
	ThumbWidth     uint32
	ThumbHeight    uint32
	PlayLength     uint32
	RawVideoLength uint64
}

// Display renders `[视频 M′SS″ WxH]`, omitting unknown parts.
func (v VideoInfo) Display() string {
	out := "[视频"
	if v.PlayLength > 0 {
		out += fmt.Sprintf(" %d′%02d″", v.PlayLength/60, v.PlayLength%60)
	}
	if v.ThumbWidth > 0 && v.ThumbHeight > 0 {
		out += fmt.Sprintf(" %dx%d", v.ThumbWidth, v.ThumbHeight)
	}
	return out + "]"
}

// ParseVideoInfo reads video attributes from xml.
func ParseVideoInfo(xml string) VideoInfo {
	var info VideoInfo
	info.AESKey, _ = ExtractXMLAttr(xml, "aeskey")
	info.CDNVideoURL, _ = ExtractXMLAttr(xml, "cdnvideourl")
	info.CDNThumbURL, _ = ExtractXMLAttr(xml, "cdnthumburl")
	if v, ok := attrUint(xml, "cdnthumbwidth", 32); ok {
		info.ThumbWidth = uint32(v)
	}
	if v, ok := attrUint(xml, "cdnthumbheight", 32); ok {
		info.ThumbHeight = uint32(v)
	}
	if v, ok := attrUint(xml, "playlength", 32); ok {
		info.PlayLength = uint32(v)
	}
	if v, ok := attrUint(xml, "rawvideolength", 64); ok {
		info.RawVideoLength = v
	}
	return info
}

// StickerInfo is a sticker message (type 47) read from its XML.
type StickerInfo struct {
	MD5         string
	AESKey      string
	ProductID   string
	URL         string
	CDNURL      string
	EncryptURL  string
	ExternURL   string
	ExternMD5   string
	ThumbURL    string
	PackName    string
	PackURL     string
	Len         uint64
	Width       uint32
	Height      uint32
	HasEmojiBuf bool
}

// Identifier is the first non-blank of md5, extern md5, CDN, extern, plain,
// thumbnail and encrypted URLs; "" when none.
func (s StickerInfo) Identifier() string {
	return firstNonBlank(s.MD5, s.ExternMD5, s.CDNURL, s.ExternURL, s.URL, s.ThumbURL, s.EncryptURL)
}

// Display is the sticker tag, or `[表情 <pack>]` (with ` (含图)` when the
// XML embeds image bytes) when there is no identifier.
func (s StickerInfo) Display() string {
	if id := s.Identifier(); id != "" {
		return StickerTag(id)
	}
	name := ""
	if s.PackName != "" {
		name = " " + s.PackName
	} else if s.ProductID != "" {
		short := s.ProductID[strings.LastIndexByte(s.ProductID, '.')+1:]
		if len(short) <= 30 {
			name = " " + short
		}
	}
	if s.HasEmojiBuf {
		return "[表情" + name + " (含图)]"
	}
	return "[表情" + name + "]"
}

// ParseStickerInfo reads sticker attributes from xml.
func ParseStickerInfo(xml string) StickerInfo {
	var info StickerInfo
	info.MD5, _ = ExtractXMLAttr(xml, "md5")
	info.AESKey, _ = ExtractXMLAttr(xml, "aeskey")
	info.ProductID, _ = ExtractXMLAttr(xml, "productid")
	info.URL, _ = ExtractXMLAttr(xml, "url")
	info.CDNURL, _ = ExtractXMLAttr(xml, "cdnurl")
	info.EncryptURL, _ = ExtractXMLAttr(xml, "encrypturl")
	info.ExternURL, _ = ExtractXMLAttr(xml, "externurl")
	info.ExternMD5, _ = ExtractXMLAttr(xml, "externmd5")
	info.ThumbURL, _ = ExtractXMLAttr(xml, "thumburl")
	info.PackName, _ = ExtractXMLTag(xml, "packname")
	info.PackURL, _ = ExtractXMLAttr(xml, "packurl")
	if v, ok := attrUint(xml, "len", 64); ok {
		info.Len = v
	}
	if v, ok := attrUint(xml, "width", 32); ok {
		info.Width = uint32(v)
	}
	if v, ok := attrUint(xml, "height", 32); ok {
		info.Height = uint32(v)
	}
	info.HasEmojiBuf = strsearch.Contains(xml, "<emojibuf>")
	return info
}

// LinkInfo is a link app message (type 49, subtype 5).
type LinkInfo struct {
	Title, Description, URL string
}

// Display renders `[链接] title - desc`, plus the URL on its own line when
// it is shorter than 120 bytes.
func (l LinkInfo) Display() string {
	parts := []string{"[链接]"}
	if l.Title != "" {
		parts = append(parts, l.Title)
	}
	if l.Description != "" {
		parts = append(parts, "- "+l.Description)
	}
	if l.URL != "" && len(l.URL) < 120 {
		parts = append(parts, "\n  "+l.URL)
	}
	return strings.Join(parts, " ")
}

// ParseLinkInfo reads a subtype-5 app message; false for other subtypes.
func ParseLinkInfo(xml string) (LinkInfo, bool) {
	if !strsearch.Contains(xml, "<type>5</type>") {
		return LinkInfo{}, false
	}
	var l LinkInfo
	l.Title, _ = ExtractXMLTag(xml, "title")
	l.Description, _ = ExtractXMLTag(xml, "des")
	l.URL, _ = ExtractXMLTag(xml, "url")
	return l, true
}

// MiniProgramInfo is a mini program app message (type 49, subtype 33).
type MiniProgramInfo struct {
	Title, AppName, PagePath string
}

// Display renders `[小程序] title - app`, plus a path line when it is
// shorter than 80 bytes.
func (m MiniProgramInfo) Display() string {
	title := m.Title
	if title == "" {
		title = "小程序"
	}
	out := "[小程序] " + title
	if m.AppName != "" {
		out += " - " + m.AppName
	}
	if m.PagePath != "" && len(m.PagePath) < 80 {
		out += "\n  path: " + m.PagePath
	}
	return out
}

// ParseMiniProgramInfo reads a subtype-33 app message; false otherwise.
func ParseMiniProgramInfo(xml string) (MiniProgramInfo, bool) {
	if !strsearch.Contains(xml, "<type>33</type>") {
		return MiniProgramInfo{}, false
	}
	var m MiniProgramInfo
	m.Title, _ = ExtractXMLTag(xml, "title")
	m.AppName, _ = ExtractXMLTag(xml, "appname")
	m.PagePath, _ = ExtractXMLTag(xml, "pagepath")
	return m, true
}

// FileInfo is a file attachment app message (type 49, subtype 6 or 62).
type FileInfo struct {
	Title    string
	TotalLen uint64
	FileExt  string
}

// Display renders `[文件] name (size)`. The name falls back to fallback,
// then to `未知文件[.ext]`.
func (f FileInfo) Display(fallback string) string {
	name := firstNonBlank(f.Title, fallback)
	if name == "" {
		ext := strings.TrimLeft(trimSpace(f.FileExt), ".")
		if ext == "" {
			name = "未知文件"
		} else {
			name = "未知文件." + ext
		}
	}
	if size := FormatFileSize(f.TotalLen); size != "" {
		return "[文件] " + name + " (" + size + ")"
	}
	return "[文件] " + name
}

// ParseFileInfo reads a subtype 6/62 app message; false for pats and other
// subtypes.
func ParseFileInfo(xml string) (FileInfo, bool) {
	if IsPatAppMessage(xml) {
		return FileInfo{}, false
	}
	if !strsearch.Contains(xml, "<type>6</type>") && !strsearch.Contains(xml, "<type>62</type>") {
		return FileInfo{}, false
	}
	var f FileInfo
	f.Title, _ = ExtractXMLTag(xml, "title")
	if v, ok := ExtractXMLTag(xml, "totallen"); ok {
		f.TotalLen, _ = parseUint(v, 64)
	}
	f.FileExt, _ = ExtractXMLTag(xml, "fileext")
	return f, true
}

// ShortVideoFeedInfo is a short video feed app message (type 49, subtype 51).
type ShortVideoFeedInfo struct {
	Title, Nickname, Description string
	feedType, mediaType          int64
	hasFeedType, hasMediaType    bool
}

// IsVideo reports whether the feed item is a video.
func (s ShortVideoFeedInfo) IsVideo() bool {
	return (s.hasMediaType && s.mediaType == 4) || (s.hasFeedType && s.feedType == 4)
}

// ParseShortVideoFeedInfo reads a subtype-51 message with a `finderFeed`;
// false otherwise.
func ParseShortVideoFeedInfo(xml string) (ShortVideoFeedInfo, bool) {
	if !strsearch.Contains(xml, "<type>51</type>") {
		return ShortVideoFeedInfo{}, false
	}
	feed, ok := ExtractXMLTag(xml, "finderFeed")
	if !ok {
		return ShortVideoFeedInfo{}, false
	}
	media, _ := ExtractXMLTag(feed, "media")
	var s ShortVideoFeedInfo
	s.Title = shortVideoTitle(xml, feed, media)
	s.Nickname, _ = extractCleanXMLTag(feed, "nickname")
	s.Description, _ = extractCleanXMLTag(feed, "desc")
	s.feedType, s.hasFeedType = ExtractXMLTagInt(feed, "feedType")
	s.mediaType, s.hasMediaType = ExtractXMLTagInt(media, "mediaType")
	return s, true
}

func extractCleanXMLTag(xml, tag string) (string, bool) {
	v, ok := ExtractXMLTag(xml, tag)
	if !ok {
		return "", false
	}
	v = trimSpace(StripCDATA(v))
	if v == "" || strings.EqualFold(v, "null") {
		return "", false
	}
	return v, true
}

func shortVideoTitle(xml, feed, media string) string {
	if t, ok := extractCleanXMLTag(feed, "title"); ok {
		return t
	}
	if t, ok := extractCleanXMLTag(media, "title"); ok {
		return t
	}
	if t, ok := extractCleanXMLTag(xml, "title"); ok && !isUpgradeTitle(t) {
		return t
	}
	return ""
}

func isUpgradeTitle(v string) bool {
	return strsearch.Contains(v, "不支持展示该内容") || strsearch.Contains(v, "升级至最新版本")
}

// IsShortVideoAppMessage reports a subtype-51 feed message that is a video.
func IsShortVideoAppMessage(xml string) bool {
	info, ok := ParseShortVideoFeedInfo(xml)
	return ok && info.IsVideo()
}

// IsPatAppMessage reports a pat ("拍一拍") app message.
func IsPatAppMessage(xml string) bool { return strsearch.Contains(xml, "<patinfo") }

// LocalTypeLow32 is the message type stored in local_type's low 32 bits.
func LocalTypeLow32(localType int64) int64 { return int64(uint64(localType) & 0xffff_ffff) }

// AppMessageSubtype is the app message subtype carried in local_type's high
// 32 bits; false unless the low bits are 49 and the subtype is non-zero.
func AppMessageSubtype(localType int64) (int64, bool) {
	encoded := uint64(localType)
	if encoded&0xffff_ffff != 49 {
		return 0, false
	}
	sub := encoded >> 32
	return int64(sub), sub != 0
}

// MediaType classifies a message's media for export and query: "image",
// "voice", "video", "sticker" or "file"; "" when it has none.
func MediaType(localType int64, rawBody string) string {
	if IsPatAppMessage(rawBody) {
		return ""
	}
	switch LocalTypeLow32(localType) {
	case 3:
		return "image"
	case 34:
		return "voice"
	case 43:
		return "video"
	case 47:
		return "sticker"
	case 62:
		return "file"
	case 49:
		if IsShortVideoAppMessage(rawBody) {
			return "video"
		}
		if isFileAppMessage(localType, rawBody) {
			return "file"
		}
	}
	return ""
}

func isFileAppMessage(localType int64, rawBody string) bool {
	if IsPatAppMessage(rawBody) {
		return false
	}
	if sub, ok := AppMessageSubtype(localType); ok && (sub == 6 || sub == 62) {
		return true
	}
	t, ok := ExtractXMLTagInt(rawBody, "type")
	return ok && (t == 6 || t == 62)
}

// FormatFileSize renders a byte count as B, whole KB, or MB/GB with one
// decimal; "" for zero.
func FormatFileSize(bytes uint64) string {
	const kb, mb, gb = 1024.0, 1024.0 * 1024.0, 1024.0 * 1024.0 * 1024.0
	switch {
	case bytes == 0:
		return ""
	case bytes >= 1<<30:
		return strconv.FormatFloat(float64(bytes)/gb, 'f', 1, 64) + "GB"
	case bytes >= 1<<20:
		return strconv.FormatFloat(float64(bytes)/mb, 'f', 1, 64) + "MB"
	case bytes >= 1<<10:
		return strconv.FormatUint(uint64(math.Round(float64(bytes)/kb)), 10) + "KB"
	default:
		return strconv.FormatUint(bytes, 10) + "B"
	}
}

// SanitizeFilename replaces every rune that is not alphanumeric (Unicode
// Alphabetic or Numeric), `-` or `_` with `_`.
func SanitizeFilename(s string) string {
	var b strings.Builder
	b.Grow(len(s))
	for _, r := range s {
		if isAlphanumeric(r) || r == '-' || r == '_' {
			b.WriteRune(r)
		} else {
			b.WriteByte('_')
		}
	}
	return b.String()
}

// isAlphanumeric matches Rust's char::is_alphanumeric: the Alphabetic
// property (letters, letter numbers, Other_Alphabetic) or a Numeric category.
func isAlphanumeric(r rune) bool {
	return unicode.IsLetter(r) || unicode.IsNumber(r) || unicode.Is(unicode.Other_Alphabetic, r)
}
