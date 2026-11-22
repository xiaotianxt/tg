// Package decode turns raw message rows into display text. It is pure: no
// files, clock, or globals beyond a shared zstd decoder. The XML and
// protobuf readers copy the Rust byte-scanning behaviour so output on
// malformed input matches.
package decode

import (
	"strconv"
	"strings"
	"time"
	"unicode"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/strsearch"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// Type is a message type: local_type truncated to 32 bits (sign-wrapped,
// as the Rust `as i32` cast did).
type Type int64

// Known message types.
const (
	TypeText         Type = 1
	TypeImage        Type = 3
	TypeVoice        Type = 34
	TypeMail         Type = 35
	TypeContactCard  Type = 42
	TypeVideo        Type = 43
	TypeSticker      Type = 47
	TypeLocation     Type = 48
	TypeLink         Type = 49 // links, files, mini programs, music
	TypeCall         Type = 50
	TypeFile         Type = 62
	TypeExternalCard Type = 66
	TypeSystem       Type = 10000
	TypeRevoke       Type = 10002
	TypeNotification Type = 11000
	TypeMusic        Type = 419430449
	TypeRedEnvelope  Type = 436207665
	TypeTransfer     Type = 536870918
)

// TypeOf is the message type of a full 64-bit local_type.
func TypeOf(localType int64) Type { return Type(int32(localType)) }

var typeLabels = map[Type]string{
	TypeText:         "文本",
	TypeImage:        "图片",
	TypeVoice:        "语音",
	TypeMail:         "邮件",
	TypeSticker:      "表情",
	TypeVideo:        "视频",
	TypeLink:         "链接/文件/小程序",
	TypeSystem:       "系统提示",
	TypeNotification: "通知",
	TypeRedEnvelope:  "红包",
	TypeTransfer:     "转账",
	TypeLocation:     "位置",
	TypeContactCard:  "名片",
	TypeFile:         "文件",
	TypeCall:         "语音/视频通话",
	TypeExternalCard: "外部联系人",
	TypeMusic:        "音乐",
	TypeRevoke:       "撤回消息",
}

// Label is the Chinese type name, `未知(<n>)` for unknown types.
func (t Type) Label() string {
	if l, ok := typeLabels[t]; ok {
		return l
	}
	return "未知(" + itoa(int64(t)) + ")"
}

// IsSystem reports system notices and recalls, which no account sent.
func (t Type) IsSystem() bool { return t == TypeSystem || t == TypeRevoke }

func (t Type) isMedia() bool {
	return t == TypeImage || t == TypeVoice || t == TypeVideo || t == TypeSticker
}

// Raw is one message row's undecoded fields.
type Raw struct {
	LocalType int64  // full 64-bit local_type (subtype in high 32 bits)
	Body      string // already decompressed message body
	Packed    []byte // packed_info_data
	VoiceID   int64  // > 0 when known (enables [voice:id:Ns] tags)
}

// Ctx is what decoding needs from the caller. Decode panics when Bucket is
// unset, or when Loc is nil while Bucket groups by minute, hour, day, month
// or year (tgtime.Minutes(1) matches the Rust default).
type Ctx struct {
	SessionName string         // fallback display name
	Bucket      tgtime.Bucket  // time grouping inside forwarded chat records
	Loc         *time.Location // time zone of forwarded chat record times
	// ResolveName maps a sender account id to a display name; nil leaves
	// ids unchanged.
	ResolveName func(senderID string) string
}

func (c Ctx) resolve(id string) string {
	if c.ResolveName == nil {
		return id
	}
	return c.ResolveName(id)
}

// Message is a decoded message.
type Message struct {
	Type        Type
	Content     string // human-readable text
	DisplayName string // sender display name; `系统` for system messages
}

// Decode renders a raw message.
func Decode(r Raw, ctx Ctx) Message {
	if ctx.Bucket.Kind == 0 {
		panic("decode: Ctx.Bucket is unset")
	}
	if needsTimestamp(ctx.Bucket) && ctx.Loc == nil {
		panic("decode: Ctx.Loc is required for time buckets")
	}
	t := TypeOf(r.LocalType)

	if t == TypeSystem || t == TypeRevoke {
		_, clean := ParseSenderPrefix(r.Body)
		return Message{Type: t, Content: sanitizeDecoded(decodeSystemContent(clean, t)), DisplayName: "系统"}
	}

	if t.isMedia() {
		sender, clean := ParseSenderPrefix(r.Body)
		return Message{
			Type:        t,
			Content:     sanitizeDecoded(decodeMediaContent(t, clean, r.Packed, r.VoiceID)),
			DisplayName: displayName(sender, ctx),
		}
	}

	if r.Body == "" && t != TypeText {
		return Message{Type: t, Content: sanitizeDecoded("[" + t.Label() + "]"), DisplayName: ctx.SessionName}
	}

	sender, clean := ParseSenderPrefix(r.Body)
	return Message{
		Type:        t,
		Content:     sanitizeDecoded(decodeContentByType(t, clean, r.Packed, ctx)),
		DisplayName: displayName(sender, ctx),
	}
}

func displayName(sender string, ctx Ctx) string {
	if sender == "" {
		return ctx.SessionName
	}
	return ctx.resolve(sender)
}

// ParseSenderPrefix splits a group message's `<sender id>:` prefix from its
// body. Only the first `:` counts, and the prefix must be at least 3 bytes
// and look like an account id. senderID is "" when there is no prefix.
func ParseSenderPrefix(body string) (senderID, rest string) {
	i := strings.IndexByte(body, ':')
	if i <= 0 {
		return "", body
	}
	// The client writes group senders as "<id>:\n<content>". Requiring the
	// newline keeps text such as "https://..." or "note:..." intact; measured
	// on real stores, every genuine prefix has it and equals the row sender.
	if i+1 >= len(body) || body[i+1] != '\n' {
		return "", body
	}
	prefix := body[:i]
	isID := strings.HasPrefix(prefix, dict.AccountIDPrefix) || strings.HasPrefix(prefix, "gh_") || isPlainID(prefix)
	if isID && len(prefix) >= 3 {
		return prefix, strings.TrimLeft(body[i+1:], " \n")
	}
	return "", body
}

func isPlainID(s string) bool {
	for _, c := range []byte(s) {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '_' || c == '-' || c == '@') {
			return false
		}
	}
	return true
}

// decodeMediaContent prefers packed metadata, then the XML body.
func decodeMediaContent(t Type, raw string, packed []byte, voiceID int64) string {
	switch t {
	case TypeVoice:
		return decodeVoiceContent(raw, packed, voiceID)
	case TypeSticker:
		return ParseStickerInfo(raw).Display()
	case TypeVideo:
		if v, ok := extractVideoDisplay(packed); ok {
			return v
		}
		return ParseVideoInfo(raw).Display()
	case TypeImage:
		if img, ok := extractImageDisplay(packed); ok {
			return img
		}
		return ParseImageInfo(raw).Display()
	}
	return "[" + t.Label() + "]"
}

func decodeVoiceContent(raw string, packed []byte, voiceID int64) string {
	duration, hasDuration := extractVoiceDurationSecs(raw)
	text, hasText := extractAudioText(packed)

	if voiceID > 0 {
		tag := "[voice:" + itoa(voiceID) + "]"
		if hasDuration {
			tag = "[voice:" + itoa(voiceID) + ":" + itoa(duration) + "s]"
		}
		if hasText {
			return tag + " " + text
		}
		return tag
	}
	if hasText {
		return "[语音] " + text
	}
	if hasDuration {
		return "[语音 " + itoa(duration) + "秒]"
	}
	return "[语音]"
}

func extractImageDisplay(data []byte) (string, bool) {
	if len(data) == 0 {
		return "", false
	}
	if v2, ok := ParsePackedV2(data); ok && v2.Image != nil && v2.Image.Identifier() != "" {
		return v2.Image.Display(), true
	}
	if v1, ok := ParsePackedV1(data); ok {
		return ImageTag(v1.Filename), true
	}
	return "", false
}

func extractVideoDisplay(data []byte) (string, bool) {
	if len(data) == 0 {
		return "", false
	}
	if v2, ok := ParsePackedV2(data); ok && v2.Video != nil {
		return v2.Video.Display(), true
	}
	return "", false
}

func extractAudioText(data []byte) (string, bool) {
	if len(data) == 0 {
		return "", false
	}
	if v2, ok := ParsePackedV2(data); ok && v2.Audio != nil && v2.Audio.AudioText != "" {
		return v2.Audio.AudioText, true
	}
	return "", false
}

func decodeContentByType(t Type, content string, packed []byte, ctx Ctx) string {
	switch t {
	case TypeLink:
		return decodeLinkContent(content, packed, ctx)
	case TypeMail:
		return decodeMailContent(content)
	case TypeContactCard:
		return decodeContactCardContent(content, "名片")
	case TypeLocation:
		return decodeLocationContent(content)
	case TypeCall:
		return decodeCallContent(content)
	case TypeFile:
		return decodeFileContent(content, packed)
	case TypeExternalCard:
		return decodeContactCardContent(content, "外部联系人")
	case TypeMusic:
		return decodeMusicContent(content)
	case TypeRedEnvelope, TypeTransfer:
		return "[" + t.Label() + "]"
	}
	return decodeTextContent(content, ctx)
}

func decodeTextContent(content string, ctx Ctx) string {
	if decoded, ok := decodeTextWithInternalXML(content, ctx); ok {
		return decoded
	}
	return content
}

func decodeTextWithInternalXML(content string, ctx Ctx) (string, bool) {
	if decoded, ok := decodeInternalXMLFragment(stripDisplayPrefix(content), ctx); ok {
		return decoded, true
	}
	start := findInternalXMLStart(content)
	if start < 0 {
		return "", false
	}
	prefix := trimEnd(content[:start])
	decoded, ok := decodeInternalXMLFragment(content[start:], ctx)
	if !ok {
		return "", false
	}
	if prefix == "" {
		return decoded, true
	}
	return prefix + "\n" + decoded, true
}

func decodeInternalXMLFragment(fragment string, ctx Ctx) (string, bool) {
	fragment = trimStart(stripDisplayPrefix(fragment))
	xml := fragment
	if strings.HasPrefix(fragment, "<?xml") {
		start := strsearch.Index(fragment, "<msg")
		if start < 0 {
			start = strsearch.Index(fragment, "<sysmsg")
		}
		if start < 0 {
			return "", false
		}
		xml = fragment[start:]
	}

	if strings.HasPrefix(xml, "&lt;") {
		return "[消息]", true
	}
	if !strings.HasPrefix(xml, "<msg") && !strings.HasPrefix(xml, "<sysmsg") {
		return "", false
	}
	switch {
	case strsearch.Contains(xml, "<appmsg"):
		return decodeLinkContent(xml, nil, ctx), true
	case strsearch.Contains(xml, "<img"):
		return ParseImageInfo(xml).Display(), true
	case strsearch.Contains(xml, "<videomsg") || strsearch.Contains(xml, "<video"):
		return ParseVideoInfo(xml).Display(), true
	case strsearch.Contains(xml, "<emoji"):
		return ParseStickerInfo(xml).Display(), true
	case strings.HasPrefix(xml, "<sysmsg"):
		return decodeSystemContent(xml, TypeSystem), true
	}
	return decodeGenericXMLFragment(xml), true
}

func trimStart(s string) string { return strings.TrimLeftFunc(s, unicode.IsSpace) }
func trimEnd(s string) string   { return strings.TrimRightFunc(s, unicode.IsSpace) }

func stripDisplayPrefix(content string) string {
	trimmed := trimStart(content)
	for _, prefix := range []string{"[链接]", "[卡片]", "[小程序]", "[文件]", "[视频]"} {
		if rest, ok := strings.CutPrefix(trimmed, prefix); ok {
			return trimStart(rest)
		}
	}
	return trimmed
}

// Markers that open an embedded XML fragment.
var xmlStartMarkers = []string{"<?xml", "<msg", "<sysmsg", "&lt;?xml", "&lt;msg", "&lt;sysmsg"}

// Markers that show content still carries raw XML.
var xmlContentMarkers = []string{
	"<?xml", "<msg", "<appmsg", "<sysmsg", "<recordinfo", "<datalist", "<dataitem",
	"<img", "<videomsg", "<voicemsg", "<emoji", "<location", "<pushmail",
	"&lt;?xml", "&lt;msg", "&lt;appmsg", "&lt;sysmsg", "&lt;recordinfo", "&lt;dataitem",
}

// firstMarkerAt is the first offset where any needle matches, or -1. Every
// needle starts with `<` or `&`, so only those bytes are candidates.
func firstMarkerAt(content string, needles []string) int {
	for i := 0; i < len(content); i++ {
		if c := content[i]; c != '<' && c != '&' {
			continue
		}
		for _, n := range needles {
			if strings.HasPrefix(content[i:], n) {
				return i
			}
		}
	}
	return -1
}

func findInternalXMLStart(content string) int { return firstMarkerAt(content, xmlStartMarkers) }

func containsInternalXMLMarker(content string) bool {
	return firstMarkerAt(content, xmlContentMarkers) >= 0
}

// sanitizeDecoded replaces any raw XML left in decoded text with a tag.
func sanitizeDecoded(content string) string {
	if !containsInternalXMLMarker(content) {
		return content
	}
	start := findInternalXMLStart(content)
	for _, needle := range []string{"<img", "<emoji", "<appmsg", "<"} {
		if start >= 0 {
			break
		}
		start = strsearch.Index(content, needle)
	}
	if start < 0 {
		start = len(content)
	}
	prefix := trimEnd(content[:start])
	marker := content[start:]
	var replacement string
	switch {
	case strsearch.Contains(marker, "<emoji"):
		replacement = ParseStickerInfo(marker).Display()
	case strsearch.Contains(marker, "<img") || strsearch.Contains(marker, "&lt;img"):
		replacement = "[img]"
	case strsearch.Contains(marker, "<videomsg") || strsearch.Contains(marker, "<video"):
		replacement = "[视频]"
	default:
		replacement = "[消息]"
	}
	switch prefix {
	case "":
		return replacement
	case ">":
		return "> " + replacement
	}
	return prefix + "\n" + replacement
}

// safeXMLTag is a tag's text unless it is blank or still holds XML,
// truncated to 200 characters.
func safeXMLTag(xml, tag string) (string, bool) {
	v, ok := ExtractXMLTag(xml, tag)
	if !ok {
		return "", false
	}
	return cleanSummaryField(v)
}

func cleanSummaryField(v string) (string, bool) {
	v = trimSpace(v)
	if v == "" || containsInternalXMLMarker(v) {
		return "", false
	}
	return truncateDisplay(v, 200), true
}

func firstSafeTag(xml string, tags ...string) (string, bool) {
	for _, tag := range tags {
		if v, ok := safeXMLTag(xml, tag); ok {
			return v, true
		}
	}
	return "", false
}

func decodeGenericXMLFragment(xml string) string {
	title, hasTitle := safeXMLTag(xml, "title")
	desc, hasDesc := firstSafeTag(xml, "des", "desc", "content")
	switch {
	case hasTitle && hasDesc:
		return "[消息] " + title + " - " + desc
	case hasTitle:
		return "[消息] " + title
	case hasDesc:
		return "[消息] " + desc
	}
	return "[消息]"
}

func decodeSystemContent(content string, t Type) string {
	trimmed := trimSpace(content)
	if trimmed == "" {
		return "[" + t.Label() + "]"
	}

	sysmsgType, _ := ExtractXMLAttr(trimmed, "type")
	if sysmsgType == "revokemsg" || t == TypeRevoke {
		if revoked, ok := ExtractXMLTag(trimmed, "content"); ok {
			return "[撤回] " + revoked
		}
		return "[撤回]"
	}

	if strings.HasPrefix(trimmed, "<") {
		if text, ok := safeXMLTag(trimmed, "content"); ok {
			return text
		}
		if generic := decodeGenericXMLFragment(trimmed); generic != "[消息]" {
			return generic
		}
		return "[" + t.Label() + "]"
	}
	return content
}

func decodeLinkContent(content string, packed []byte, ctx Ctx) string {
	if !strings.HasPrefix(trimStart(content), "<") {
		if len(content) > 200 {
			return "[链接] " + truncateDisplay(content, 200)
		}
		return "[链接] " + content
	}

	if IsPatAppMessage(content) {
		return decodePatContent(content)
	}

	subType := appMessageType(content)
	switch subType {
	case 5:
		if push, ok := decodeOfficialPush(content); ok {
			return push
		}
		if info, ok := ParseLinkInfo(content); ok {
			return displayLinkInfo(info)
		}
		return "[链接]"
	case 33:
		if info, ok := ParseMiniProgramInfo(content); ok {
			return info.Display()
		}
		return "[小程序]"
	case 36:
		if info, ok := ParseMiniProgramInfo(content); ok {
			return info.Display()
		}
		return decodeAppCardFallback(subType, "小程序", content)
	case 4:
		return decodeAppCardFallback(subType, "视频", content)
	case 3:
		title, ok := ExtractXMLTag(content, "title")
		if !ok {
			title = "未知歌曲"
		}
		return "[音乐] " + title
	case 6, 62:
		return decodeFileContent(content, packed)
	case 19:
		return decodeChatHistoryContent(content, ctx)
	case 57:
		return decodeQuoteContent(content, ctx)
	case 87:
		if notice, ok := decodeGroupNoticeContent(content); ok {
			return notice
		}
		return decodeAppCardFallback(subType, "卡片", content)
	case 2000:
		if transfer, ok := decodeTransfer(content); ok {
			return transfer
		}
		return decodeAppCardFallback(subType, "转账", content)
	case 2001:
		if packet, ok := decodeRedPacket(content); ok {
			return packet
		}
		return decodeAppCardFallback(subType, "红包", content)
	case 51:
		return decodeShortVideoOrLegacyReference(content)
	}
	return decodeAppCardFallback(subType, "卡片", content)
}

// appMessageType is the first `<type>` as a number, 0 when absent. Some
// clients wrap it in CDATA, as they do every other appmsg field.
func appMessageType(content string) int64 {
	v, ok := ExtractXMLTag(content, "type")
	if !ok {
		return 0
	}
	n, err := strconv.ParseInt(StripCDATA(v), 10, 64)
	if err != nil {
		return 0
	}
	return n
}

func decodePatContent(content string) string {
	if title, ok := firstSafeTag(content, "title", "template"); ok && title != "" {
		return "[拍一拍] " + title
	}
	return "[拍一拍]"
}

func decodeGroupNoticeContent(content string) (string, bool) {
	a, ok := ExtractXMLTag(content, "textannouncement")
	if !ok {
		return "", false
	}
	a = trimSpace(StripCDATA(a))
	if a == "" {
		return "", false
	}
	return "[群公告] " + a, true
}

func displayLinkInfo(info LinkInfo) string {
	display := info.Display()
	url, ok := cleanURLField(info.URL)
	if !ok || strsearch.Contains(display, url) {
		return display
	}
	return display + "\n  " + url
}

func decodeShortVideoOrLegacyReference(content string) string {
	if info, ok := ParseShortVideoFeedInfo(content); ok {
		return displayShortVideoFeedInfo(info)
	}
	title, ok := ExtractXMLTag(content, "title")
	if !ok {
		title = "聊天记录"
	}
	return "[引用: " + title + "]"
}

func displayShortVideoFeedInfo(info ShortVideoFeedInfo) string {
	label := "视频号"
	if info.IsVideo() {
		label = "视频"
	}
	title := truncateDisplay(trimSpace(info.Title), 200)
	desc := truncateDisplay(trimSpace(info.Description), 200)
	nickname := trimSpace(info.Nickname)

	var summary string
	switch {
	case title != "" && desc != "" && title != desc:
		summary = title + " - " + desc
	case title != "":
		summary = title
	case desc != "":
		summary = desc
	case nickname != "":
		summary = "@" + nickname
	default:
		summary = "视频号内容"
	}
	if nickname != "" && !strsearch.Contains(summary, nickname) {
		summary += " - @" + nickname
	}
	return "[" + label + "] " + summary
}

func decodeAppCardFallback(subType int64, label, content string) string {
	title, _ := safeXMLTag(content, "title")
	desc, _ := safeXMLTag(content, "des")
	if label == "卡片" && subType > 0 {
		label = label + ":" + itoa(subType)
	}
	if title == "" {
		if strings.HasPrefix(label, "卡片:") {
			title = "未知卡片"
		} else {
			title = "未知" + label
		}
	}
	display := "[" + label + "] " + title
	if desc != "" {
		display += " - " + desc
	}
	if url, ok := safeURLTag(content); ok {
		display += "\n  " + url
	}
	return display
}

func safeURLTag(xml string) (string, bool) {
	url, ok := ExtractXMLTag(xml, "url")
	if !ok {
		return "", false
	}
	return cleanURLField(url)
}

func cleanURLField(v string) (string, bool) {
	v = trimSpace(v)
	if v == "" || strings.ContainsAny(v, "<>") || containsInternalXMLMarker(v) {
		return "", false
	}
	return v, true
}

// optString is an optional string, compared by value.
type optString struct {
	s  string
	ok bool
}

type chatHistoryItem struct {
	sourceName      string
	sourceTime      optString
	sourceTimestamp int64
	hasTimestamp    bool
	body            string
}

// maxChatHistoryDepth is the deepest forwarded record expanded inside
// another; deeper ones render as a `[记录]` tag. Each level is a strictly
// smaller slice, so recursion always ends; the bound only keeps hostile
// input from exhausting the stack.
const maxChatHistoryDepth = 32

func decodeChatHistoryContent(content string, ctx Ctx) string {
	title, ok := ExtractXMLTag(content, "title")
	if !ok {
		title = "聊天记录"
	}
	var items []chatHistoryItem
	if record, ok := ExtractXMLElement(content, "recorditem"); ok {
		items = parseChatHistoryItems(StripCDATA(record), ctx, 0)
	}
	return renderChatHistory(title, items, func() (string, bool) { return ExtractXMLTag(content, "des") }, ctx)
}

// ownTag reads a tag from around a nested element, so the nested element's
// fields cannot shadow the outer ones.
func ownTag(head, tail, tag string) (string, bool) {
	if v, ok := ExtractXMLTag(head, tag); ok {
		return v, true
	}
	return ExtractXMLTag(tail, tag)
}

// decodeNestedChatHistory expands a `<recordinfo>` nested in a forwarded
// item. Its title and summary sit outside `<datalist>`.
func decodeNestedChatHistory(record string, ctx Ctx, depth int) string {
	head, tail := record, ""
	if list, ok := FindXMLElement(record, "datalist"); ok {
		head, tail = record[:list.Start], record[list.End:]
	}
	title, ok := ownTag(head, tail, "title")
	if !ok {
		title = "聊天记录"
	}
	items := parseChatHistoryItems(record, ctx, depth)
	return renderChatHistory(title, items, func() (string, bool) { return ownTag(head, tail, "desc") }, ctx)
}

func renderChatHistory(title string, items []chatHistoryItem, summary func() (string, bool), ctx Ctx) string {
	if len(items) == 0 {
		if desc, ok := summary(); ok {
			return "[聊天记录] " + title + " - " + desc
		}
		return "[聊天记录] " + title
	}
	var out strings.Builder
	out.WriteString("[聊天记录] " + title + " (" + strconv.Itoa(len(items)) + "条)")
	renderChatHistoryItems(&out, items, ctx)
	return out.String()
}

// parseChatHistoryItems returns the items of the record's top-level
// `<datalist>`; items of nested records stay inside their parent item.
// Records without a `<datalist>` are scanned whole.
func parseChatHistoryItems(record string, ctx Ctx, depth int) []chatHistoryItem {
	rest := record
	if list, ok := FindXMLElement(record, "datalist"); ok {
		rest = list.Body(record)
	}
	var items []chatHistoryItem
	for {
		e, ok := FindXMLElement(rest, "dataitem")
		if !ok {
			return items
		}
		if item, ok := parseChatHistoryItem(rest[e.Start:e.End], ctx, depth); ok {
			items = append(items, item)
		}
		rest = rest[e.End:]
	}
}

func parseChatHistoryItem(itemXML string, ctx Ctx, depth int) (chatHistoryItem, bool) {
	// A forwarded record nested in this item carries its own senders, times
	// and titles; read this item's fields from around it.
	nested, hasNested := FindXMLElement(itemXML, "recordxml")
	if !hasNested {
		nested, hasNested = FindXMLElement(itemXML, "recorditem")
	}
	head, tail := itemXML, ""
	if hasNested {
		head, tail = itemXML[:nested.Start], itemXML[nested.End:]
	}

	var item chatHistoryItem
	if name, ok := ownTag(head, tail, "sourcename"); ok {
		item.sourceName = name
	} else if name, ok := ownTag(head, tail, "sourcedisplayname"); ok {
		item.sourceName = name
	} else {
		item.sourceName = "未知"
	}
	item.sourceTime.s, item.sourceTime.ok = ownTag(head, tail, "sourcetime")
	if item.sourceTime.ok && needsTimestamp(ctx.Bucket) {
		item.sourceTimestamp, item.hasTimestamp = tgtime.ParseLocalMinutes(item.sourceTime.s, ctx.Loc)
	}

	switch {
	case hasNested && depth < maxChatHistoryDepth:
		record := StripCDATA(nested.Body(itemXML))
		item.body = decodeNestedChatHistory(record, ctx, depth+1)
	case hasNested:
		title, ok := ownTag(head, tail, "datatitle")
		if !ok {
			title, _ = ownTag(head, tail, "title")
		}
		item.body = fallbackTagWithTitle("记录", title)
	default:
		var datatype int64
		hasDatatype := false
		if s, ok := ExtractXMLAttr(itemXML, "datatype"); ok {
			if n, err := strconv.ParseInt(s, 10, 64); err == nil {
				datatype, hasDatatype = n, true
			}
		}
		item.body = decodeChatHistoryItemBody(itemXML, datatype, hasDatatype, ctx, depth)
	}
	if item.body == "" {
		return chatHistoryItem{}, false
	}
	return item, true
}

// needsTimestamp reports whether chat record times are rebucketed, the
// only case that reads them as instants.
func needsTimestamp(b tgtime.Bucket) bool {
	return b.Kind != tgtime.BucketPerMessage && b.Kind != tgtime.BucketNone
}

func decodeChatHistoryItemBody(itemXML string, datatype int64, hasDatatype bool, ctx Ctx, depth int) string {
	if desc, ok := ExtractXMLTag(itemXML, "datadesc"); ok {
		return decodeEmbeddedHistoryText(desc, ctx, depth)
	}
	title, ok := ExtractXMLTag(itemXML, "datatitle")
	if !ok {
		title, _ = ExtractXMLTag(itemXML, "title")
	}

	if hasDatatype {
		switch datatype {
		case 2:
			return chatHistoryImageTag(itemXML)
		case 3:
			return fallbackTagWithTitle("语音", title)
		case 4, 15:
			return fallbackTagWithTitle("视频", title)
		case 5:
			return fallbackTagWithTitle("链接", title)
		case 6:
			return fallbackTagWithTitle("位置", title)
		case 8:
			return fallbackTagWithTitle("文件", title)
		}
	}
	switch {
	case title != "":
		return decodeEmbeddedHistoryText(title, ctx, depth)
	case hasDatatype:
		return "[记录:" + itoa(datatype) + "]"
	}
	return "[记录]"
}

func decodeEmbeddedHistoryText(value string, ctx Ctx, depth int) string {
	// Some items carry a nested record as entity-escaped text.
	if depth < maxChatHistoryDepth && strsearch.Contains(value, "<recordinfo") {
		return decodeNestedChatHistory(value, ctx, depth+1)
	}
	if !containsInternalXMLMarker(value) {
		return value
	}
	plain := Ctx{SessionName: ctx.SessionName, Bucket: tgtime.Minutes(1), Loc: ctx.Loc}
	if decoded, ok := decodeTextWithInternalXML(value, plain); ok {
		return decoded
	}
	return "[消息]"
}

func chatHistoryImageTag(itemXML string) string {
	if id, ok := ExtractXMLTag(itemXML, "fullmd5"); ok {
		return ImageTag(id)
	}
	if id, ok := ExtractXMLTag(itemXML, "thumbfullmd5"); ok {
		return ImageTag(id)
	}
	if id, ok := ExtractXMLAttr(itemXML, "dataid"); ok {
		return ImageTag(id)
	}
	return ImageTag("")
}

func fallbackTagWithTitle(label, title string) string {
	if title == "" {
		return "[" + label + "]"
	}
	return "[" + label + "] " + title
}

func renderChatHistoryItems(out *strings.Builder, items []chatHistoryItem, ctx Ctx) {
	if ctx.Bucket.Kind == tgtime.BucketPerMessage {
		for _, item := range items {
			appendChatHistoryItem(out, item, true, item.sourceTime)
		}
		return
	}

	var lastLabel, lastSender optString
	for _, item := range items {
		label := chatHistoryTimeLabel(item, ctx)
		if label != lastLabel {
			if label.ok {
				appendChatHistoryLine(out, "["+label.s+"]")
			}
			lastLabel = label
			lastSender = optString{}
		}
		showSender := lastSender != optString{item.sourceName, true}
		appendChatHistoryItem(out, item, showSender, optString{})
		lastSender = optString{item.sourceName, true}
	}
}

func chatHistoryTimeLabel(item chatHistoryItem, ctx Ctx) optString {
	if !needsTimestamp(ctx.Bucket) {
		return optString{}
	}
	if item.hasTimestamp {
		return optString{tgtime.FormatBucket(item.sourceTimestamp, ctx.Bucket, ctx.Loc), true}
	}
	return item.sourceTime
}

func appendChatHistoryItem(out *strings.Builder, item chatHistoryItem, showSender bool, inlineTime optString) {
	lines := rustLines(item.body)
	if len(lines) == 0 {
		return
	}
	first := lines[0]
	var firstLine string
	switch {
	case inlineTime.ok && showSender:
		firstLine = "[" + inlineTime.s + "] " + item.sourceName + ": " + first
	case inlineTime.ok:
		firstLine = "[" + inlineTime.s + "] " + first
	case showSender:
		firstLine = item.sourceName + ": " + first
	default:
		firstLine = " " + first
	}
	appendChatHistoryLine(out, firstLine)
	for _, line := range lines[1:] {
		if showSender {
			appendChatHistoryLine(out, "  "+line)
		} else {
			appendChatHistoryLine(out, " "+line)
		}
	}
}

// rustLines splits like Rust's str::lines: on `\n`, dropping one trailing
// `\r` per line and no empty final line after a trailing `\n`.
func rustLines(s string) []string {
	if s == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(s, "\n"), "\n")
	for i, l := range lines {
		lines[i] = strings.TrimSuffix(l, "\r")
	}
	return lines
}

func appendChatHistoryLine(out *strings.Builder, line string) {
	out.WriteString("\n        ")
	out.WriteString(line)
}

func decodeQuoteContent(content string, ctx Ctx) string {
	reply, _ := safeXMLTag(content, "title")
	// The quoted body stays entity-escaped until its own `<content>` is read:
	// decoding the whole `<refermsg>` first would expose the quoted message's
	// own `<type>` and `<content>` tags to the field lookups below.
	quoted := ""
	if e, ok := FindXMLElement(content, "refermsg"); ok {
		quoted = decodeRefermsgContent(e.Body(content), ctx)
	}
	switch {
	case quoted != "" && reply != "":
		return "> " + quoted + "\n " + reply
	case quoted != "":
		return "> " + quoted
	case reply != "":
		return reply
	}
	return "[引用]"
}

func decodeRefermsgContent(refermsg string, ctx Ctx) string {
	// Read the other fields with `<content>` cut out, so a quoted body that
	// arrives unescaped (CDATA) cannot shadow them.
	refContent, fields := "", refermsg
	if e, ok := FindXMLElement(refermsg, "content"); ok {
		refContent = DecodeXMLEntities(StripCDATA(e.Body(refermsg)))
		fields = refermsg[:e.Start] + refermsg[e.End:]
	}
	refType := TypeText
	if n, ok := ExtractXMLTagInt(fields, "type"); ok {
		refType = Type(int32(n))
	}
	contentSender, clean := ParseSenderPrefix(refContent)

	var body string
	switch {
	case clean == "":
		body = "[" + refType.Label() + "]"
	case refType == TypeText:
		body = decodeTextContent(clean, ctx)
	case refType.isMedia():
		body = decodeMediaContent(refType, clean, nil, 0)
	case refType == TypeLink:
		normalized := stripDisplayPrefix(clean)
		if strings.HasPrefix(trimStart(normalized), "<") {
			body = decodeLinkContent(normalized, nil, ctx)
		} else {
			body = decodeTextContent(clean, ctx)
		}
	case strings.HasPrefix(trimStart(clean), "<"):
		body = "[" + refType.Label() + "]"
	default:
		body = decodeTextContent(clean, ctx)
	}

	if sender, ok := decodeRefermsgSender(fields, contentSender, ctx); ok {
		return sender + ": " + body
	}
	return body
}

func decodeRefermsgSender(refermsg, contentSender string, ctx Ctx) (string, bool) {
	embedded, hasEmbedded := ExtractXMLTag(refermsg, "displayname")
	embedded = trimSpace(embedded)
	hasEmbedded = hasEmbedded && embedded != ""

	senderID, hasID := contentSender, contentSender != ""
	if !hasID {
		senderID, hasID = ExtractXMLTag(refermsg, "chatusr")
	}
	if !hasID {
		if id, ok := ExtractXMLTag(refermsg, "fromusr"); ok && !strsearch.Contains(id, "@chatroom") {
			senderID, hasID = id, true
		}
	}

	if hasID {
		sender := ctx.resolve(senderID)
		blank := trimSpace(sender) == ""
		if !blank && sender != senderID {
			return sender, true
		}
		if hasEmbedded {
			return embedded, true
		}
		return sender, !blank
	}
	return embedded, hasEmbedded
}

func decodeLocationContent(content string) string {
	if !strings.HasPrefix(trimStart(content), "<") {
		return "[位置] " + content
	}
	pick := func(name string) (string, bool) {
		if v, ok := ExtractXMLAttr(content, name); ok {
			return v, true
		}
		return ExtractXMLTag(content, name)
	}
	if poi, ok := pick("poiname"); ok {
		return "[位置] " + poi
	}
	if label, ok := pick("label"); ok {
		return "[位置] " + label
	}
	return "[位置] 未知位置"
}

func decodeCallContent(content string) string {
	secs, ok := ExtractXMLTagInt(content, "duration")
	if !ok {
		for _, w := range strings.Fields(content) {
			if n, err := strconv.ParseInt(w, 10, 64); err == nil {
				secs, ok = n, true
				break
			}
		}
	}
	if !ok {
		return "[通话]"
	}
	if mins := secs / 60; mins > 0 {
		return "[通话] " + itoa(mins) + "分" + itoa(secs%60) + "秒"
	}
	return "[通话] " + itoa(secs%60) + "秒"
}

func decodeMailContent(content string) string {
	if !strings.HasPrefix(trimStart(content), "<") {
		if content == "" {
			return "[邮件]"
		}
		return "[邮件] " + truncateDisplay(content, 120)
	}
	subject, ok := ExtractXMLTag(content, "subject")
	if !ok {
		subject, _ = ExtractXMLTag(content, "title")
	}
	sender, ok := ExtractXMLTag(content, "sender")
	if !ok {
		sender, _ = ExtractXMLTag(content, "name")
	}
	summary := subject
	if summary == "" {
		summary, _ = ExtractXMLTag(content, "digest")
	}
	switch {
	case summary != "" && sender != "":
		return "[邮件] " + summary + " - " + sender
	case summary != "":
		return "[邮件] " + summary
	case sender != "":
		return "[邮件] " + sender
	}
	return "[邮件]"
}

func decodeContactCardContent(content, label string) string {
	if !strings.HasPrefix(trimStart(content), "<") {
		if content == "" {
			return "[" + label + "]"
		}
		return "[" + label + "] " + truncateDisplay(content, 120)
	}
	for _, attr := range []string{"nickname", "alias", "username"} {
		if name, ok := ExtractXMLAttr(content, attr); ok {
			return "[" + label + "] " + name
		}
	}
	return "[" + label + "]"
}

func decodeFileContent(content string, packed []byte) string {
	packedName := fileNameFromPacked(packed)
	if strings.HasPrefix(trimStart(content), "<") {
		if info, ok := ParseFileInfo(content); ok {
			return info.Display(packedName)
		}
		if packedName != "" {
			return "[文件] " + packedName
		}
		return "[文件]"
	}

	name := packedName
	switch {
	case name != "":
	case strsearch.Contains(content, "/"):
		name = content[strings.LastIndexByte(content, '/')+1:]
	case content != "":
		name = content
	default:
		return "[文件]"
	}
	if len(name) > 80 {
		return "[文件] " + truncateBytes(name, 77) + "..."
	}
	return "[文件] " + name
}

// truncateBytes cuts s to at most n bytes without splitting a UTF-8
// sequence. (Rust sliced at byte n and panicked mid-character.)
func truncateBytes(s string, n int) string {
	if len(s) <= n {
		return s
	}
	for n > 0 && n < len(s) && s[n]&0xC0 == 0x80 {
		n--
	}
	return s[:n]
}

func fileNameFromPacked(data []byte) string {
	if len(data) == 0 {
		return ""
	}
	v2, ok := ParsePackedV2(data)
	if !ok || v2.File == nil {
		return ""
	}
	return v2.File.Identifier()
}

func decodeMusicContent(content string) string {
	if content == "" {
		return "[音乐]"
	}
	return "[音乐] " + content
}

// extractVoiceDurationSecs reads a voice message's length, stored in
// milliseconds as `voicelength="<ms>"` on <voicemsg> or, in legacy bodies,
// as `<sender>:<ms>:<flag>`. It rounds to the nearest second, at least 1.
// Rust took the first digit run of the body, which is the endflag attribute
// (0 or 1) on current bodies and digits of the sender id on legacy ones.
func extractVoiceDurationSecs(content string) (int64, bool) {
	var field string
	if v, ok := ExtractXMLAttr(content, "voicelength"); ok {
		field = v
	} else if parts := strings.Split(strings.TrimSpace(content), ":"); len(parts) == 3 && parts[0] != "" {
		field = parts[1]
	}
	ms, err := strconv.ParseInt(field, 10, 64)
	if err != nil || ms <= 0 {
		return 0, false
	}
	return max((ms+500)/1000, 1), true
}

// truncateDisplay keeps the first maxChars runes, adding `...` when more
// follow. It slices by byte offsets so invalid UTF-8 passes through intact.
func truncateDisplay(value string, maxChars int) string {
	count := 0
	for i := range value {
		if count == maxChars {
			return value[:i] + "..."
		}
		count++
	}
	return value
}
