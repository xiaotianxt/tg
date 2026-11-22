package decode

import (
	"strings"
	"testing"
)

func TestFindXMLElementBalancesNestedTags(t *testing.T) {
	xml := `x<item a="1"><itemx/><item>in</item><item/>t</item><item>2</item>`
	e, ok := FindXMLElement(xml, "item")
	if !ok {
		t.Fatal("not found")
	}
	if got := xml[e.Start:e.End]; got != `<item a="1"><itemx/><item>in</item><item/>t</item>` {
		t.Errorf("element = %q", got)
	}
	if got := e.Body(xml); got != "<itemx/><item>in</item><item/>t" {
		t.Errorf("body = %q", got)
	}

	e, ok = FindXMLElement("</a><a/><a>x</a>", "a")
	if !ok || e != (Element{Start: 4, BodyStart: 8, BodyEnd: 8, End: 8}) {
		t.Errorf("self-closing = %+v, %v", e, ok)
	}
	if _, ok := FindXMLElement("<a><a></a>", "a"); ok {
		t.Error("unclosed element found")
	}
	if _, ok := FindXMLElement("<ab>x</ab>", "a"); ok {
		t.Error("prefix tag matched")
	}
}

func TestDecodeXMLEntities(t *testing.T) {
	for in, want := range map[string]string{
		"plain 文本":              "plain 文本",
		"a&amp;b":               "a&b",
		"&amp;lt;x&amp;gt;":     "<x>",
		"&amp;amp;":             "&amp;",
		"&lt;&amp;quot;&gt;":    "<\">",
		"&apos;&quot;&lt;&gt;&": "'\"<>&",
		"&gt&lt;&unknown;":      "&gt<&unknown;",
		"":                      "",
	} {
		if got := DecodeXMLEntities(in); got != want {
			t.Errorf("DecodeXMLEntities(%q) = %q; want %q", in, got, want)
		}
	}
}

// chainedReplace is the definition DecodeXMLEntities must match.
func chainedReplace(s string) string {
	for _, r := range [][2]string{{"&amp;", "&"}, {"&quot;", "\""}, {"&apos;", "'"}, {"&lt;", "<"}, {"&gt;", ">"}} {
		s = strings.ReplaceAll(s, r[0], r[1])
	}
	return s
}

func TestDecodeXMLEntitiesMatchesChainedReplace(t *testing.T) {
	pieces := []string{"&", "amp;", "&amp;", "lt;", "gt;", "quot;", "apos;", "&lt", ";", "a", "文", "<"}
	state := uint64(0x2545_f491_4f6c_dd1d)
	for range 20_000 {
		state ^= state << 13
		state ^= state >> 7
		state ^= state << 17
		bits := state
		var b strings.Builder
		for n := bits % 9; n > 0; n-- {
			bits /= 9
			b.WriteString(pieces[bits%uint64(len(pieces))])
			bits /= uint64(len(pieces))
		}
		in := b.String()
		if got, want := DecodeXMLEntities(in), chainedReplace(in); got != want {
			t.Fatalf("DecodeXMLEntities(%q) = %q; want %q", in, got, want)
		}
	}
}

func TestExtractXMLTag(t *testing.T) {
	for _, tc := range []struct {
		xml, tag, want string
		ok             bool
	}{
		{"<title> a &amp; b </title>", "title", "a & b", true},
		{"<subtype>1</subtype><type>2</type>", "type", "2", true},
		{"</type><type>3</type>", "type", "3", true},
		{"<title></title>", "title", "", false},
		{"<title>open", "title", "", false},
		{"<title>", "title", "", false},
		{"<a><a>x</a></a>", "a", "<a>x", true},
	} {
		got, ok := ExtractXMLTag(tc.xml, tc.tag)
		if got != tc.want || ok != tc.ok {
			t.Errorf("ExtractXMLTag(%q, %q) = (%q, %v); want (%q, %v)", tc.xml, tc.tag, got, ok, tc.want, tc.ok)
		}
	}
	if n, ok := ExtractXMLTagInt("<type>-5</type>", "type"); !ok || n != -5 {
		t.Errorf("ExtractXMLTagInt = %d, %v", n, ok)
	}
	if got, ok := ExtractXMLElement("<r> <a><r>in</r></a> </r>", "r"); !ok || got != "<a><r>in</r></a>" {
		t.Errorf("ExtractXMLElement = %q, %v", got, ok)
	}
}

func TestFindWrapped(t *testing.T) {
	inputs := []string{"", "<a>", "<<a>>", "</a</a>", "<aa><a>", "a><a>", "<a<a>",
		"<title>文本</title><title>", "<subtype>1</subtype><type>2</type>", "</type><type>"}
	for _, h := range inputs {
		for _, tag := range []string{"a", "aa", "type", "title", ""} {
			for _, prefix := range []string{"<", "</"} {
				if got, want := findWrapped(h, prefix, tag), strings.Index(h, prefix+tag+">"); got != want {
					t.Errorf("findWrapped(%q, %q, %q) = %d; want %d", h, prefix, tag, got, want)
				}
			}
		}
	}
}

func TestExtractXMLAttr(t *testing.T) {
	xml := `<emoji productid="" cdnurl="http://x.test/a?m=1&amp;n=2" externurl=""></emoji>`
	if _, ok := ExtractXMLAttr(xml, "productid"); ok {
		t.Error("empty value found")
	}
	if _, ok := ExtractXMLAttr(xml, "url"); ok {
		t.Error("attribute suffix matched")
	}
	if got, _ := ExtractXMLAttr(xml, "cdnurl"); got != "http://x.test/a?m=1&n=2" {
		t.Errorf("cdnurl = %q", got)
	}

	spaced := `<emoji cdnurl = "http://x.test/a?m=1&amp;n=2" aeskey= 'abc123'></emoji>`
	if got, _ := ExtractXMLAttr(spaced, "cdnurl"); got != "http://x.test/a?m=1&n=2" {
		t.Errorf("spaced cdnurl = %q", got)
	}
	if got, _ := ExtractXMLAttr(spaced, "aeskey"); got != "abc123" {
		t.Errorf("single-quoted aeskey = %q", got)
	}
	if _, ok := ExtractXMLAttr(`<a md5="open`, "md5"); ok {
		t.Error("unclosed quote found")
	}
}

func TestStripCDATA(t *testing.T) {
	for in, want := range map[string]string{
		" <![CDATA[ x ]]> ": " x ",
		"<![CDATA[x":        "<![CDATA[x",
		"  plain ":          "plain",
	} {
		if got := StripCDATA(in); got != want {
			t.Errorf("StripCDATA(%q) = %q; want %q", in, got, want)
		}
	}
}

func TestParseStickerInfo(t *testing.T) {
	info := ParseStickerInfo(`<emoji md5="abc123" len="4963" cdnurl="http://x.test/a.gif" encrypturl="http://x.test/e" aeskey="00112233445566778899aabbccddeeff" width="48" height="47"></emoji>`)
	want := StickerInfo{MD5: "abc123", AESKey: "00112233445566778899aabbccddeeff", CDNURL: "http://x.test/a.gif",
		EncryptURL: "http://x.test/e", Len: 4963, Width: 48, Height: 47}
	if info != want {
		t.Errorf("info = %+v", info)
	}
	if info.Identifier() != "abc123" || info.Display() != "[sticker:abc123]" {
		t.Errorf("identifier %q display %q", info.Identifier(), info.Display())
	}

	for xml, want := range map[string]string{
		`<emoji productid="com.test.pack"><packname>fun</packname><emojibuf>abc</emojibuf></emoji>`: "[表情 fun (含图)]",
		`<emoji><emojibuf>abc</emojibuf></emoji>`:                                                   "[表情 (含图)]",
		`<emoji productid="com.test.pack"></emoji>`:                                                 "[表情 pack]",
	} {
		info := ParseStickerInfo(xml)
		if info.Identifier() != "" || info.Display() != want {
			t.Errorf("%s: identifier %q display %q; want %q", xml, info.Identifier(), info.Display(), want)
		}
	}
}

func TestParseImageAndVideoInfo(t *testing.T) {
	info := ParseImageInfo(`<msg><img aeskey="abc123" cdnthumbwidth="180" cdnthumbheight="153" rawlength="38186" /></msg>`)
	if info.Identifier() != "abc123" || info.Display() != "[img:abc123]" || info.ThumbWidth != 180 || info.RawLength != 38186 {
		t.Errorf("image = %+v", info)
	}
	info = ParseImageInfo(`<msg><img cdnthumbwidth="+180" cdnthumbheight="-153" /></msg>`)
	if info.Identifier() != "" || info.Display() != "[img]" || info.ThumbWidth != 180 || info.ThumbHeight != 0 {
		t.Errorf("image without identifier = %+v", info)
	}
	video := ParseVideoInfo(`<msg><videomsg playlength="75" cdnthumbwidth="320" cdnthumbheight="240" /></msg>`)
	if got := video.Display(); got != "[视频 1′15″ 320x240]" {
		t.Errorf("video display = %q", got)
	}
}

func TestParseFileInfo(t *testing.T) {
	info, ok := ParseFileInfo(`<msg><appmsg><title>report.pdf</title><type>6</type><appattach><totallen>1536000</totallen><fileext>pdf</fileext><md5>abc</md5></appattach></appmsg></msg>`)
	if !ok || info != (FileInfo{Title: "report.pdf", TotalLen: 1536000, FileExt: "pdf"}) {
		t.Fatalf("info = %+v, %v", info, ok)
	}
	if got := info.Display(""); got != "[文件] report.pdf (1.5MB)" {
		t.Errorf("display = %q", got)
	}
	if got := (FileInfo{FileExt: ".zip"}).Display(" "); got != "[文件] 未知文件.zip" {
		t.Errorf("unnamed display = %q", got)
	}
}

func TestMediaType(t *testing.T) {
	short := `<msg><appmsg><title>当前版本不支持展示该内容，请升级至最新版本。</title><type>51</type><finderFeed><feedType>4</feedType><title><![CDATA[Short Title]]></title><nickname><![CDATA[Alice]]></nickname><desc><![CDATA[short clip]]></desc><mediaList><media><mediaType>4</mediaType></media></mediaList></finderFeed></appmsg></msg>`
	legacy51 := `<msg><appmsg><title>Chat History</title><type>51</type></appmsg></msg>`
	pat := `<msg><appmsg><title>我拍了拍 "Bob"</title><type>62</type><appattach><totallen>0</totallen></appattach><patinfo><template>我拍了拍 "${tgid_bob}"</template></patinfo></appmsg></msg>`
	for _, tc := range []struct {
		localType int64
		body      string
		want      string
	}{
		{34, "", "voice"},
		{3, "", "image"},
		{47, "", "sticker"},
		{43, "", "video"},
		{62, "", "file"},
		{6<<32 | 49, "", "file"},
		{49, "<msg><appmsg><title>report.pdf</title><type>6</type></appmsg></msg>", "file"},
		{49, "<msg><appmsg><type>5</type></appmsg></msg>", ""},
		{51<<32 | 49, short, "video"},
		{51<<32 | 49, legacy51, ""},
		{62<<32 | 49, pat, ""},
		{1, "", ""},
	} {
		if got := MediaType(tc.localType, tc.body); got != tc.want {
			t.Errorf("MediaType(%#x, %.30q) = %q; want %q", tc.localType, tc.body, got, tc.want)
		}
	}
	if _, ok := ParseFileInfo(pat); ok {
		t.Error("pat parsed as file")
	}
	if _, ok := ParseShortVideoFeedInfo(legacy51); ok {
		t.Error("legacy subtype 51 parsed as feed")
	}
}

func TestParseShortVideoFeedInfo(t *testing.T) {
	info, ok := ParseShortVideoFeedInfo(`<msg><appmsg><title>当前版本不支持展示该内容，请升级至最新版本。</title><type>51</type><finderFeed><feedType>4</feedType><title><![CDATA[Short Title]]></title><nickname><![CDATA[Alice]]></nickname><desc><![CDATA[short clip]]></desc><mediaList><media><mediaType>4</mediaType></media></mediaList></finderFeed></appmsg></msg>`)
	if !ok || info.Title != "Short Title" || info.Nickname != "Alice" || info.Description != "short clip" || !info.IsVideo() {
		t.Errorf("info = %+v, %v", info, ok)
	}
	// The upgrade placeholder is not a title.
	info, ok = ParseShortVideoFeedInfo(`<msg><appmsg><title>当前版本不支持展示该内容，请升级至最新版本。</title><type>51</type><finderFeed><feedType>4</feedType><nickname>Alice</nickname><desc>short clip</desc><mediaList><media><mediaType>4</mediaType></media></mediaList></finderFeed></appmsg></msg>`)
	if !ok || info.Title != "" || info.Description != "short clip" {
		t.Errorf("placeholder info = %+v, %v", info, ok)
	}
}

func TestLocalTypeHelpers(t *testing.T) {
	if got := LocalTypeLow32(-1); got != 0xffff_ffff {
		t.Errorf("LocalTypeLow32(-1) = %#x", got)
	}
	if sub, ok := AppMessageSubtype(57<<32 | 49); !ok || sub != 57 {
		t.Errorf("AppMessageSubtype = %d, %v", sub, ok)
	}
	if _, ok := AppMessageSubtype(49); ok {
		t.Error("zero subtype reported")
	}
	if _, ok := AppMessageSubtype(57<<32 | 3); ok {
		t.Error("subtype of non-app message reported")
	}
}

func TestFormatFileSize(t *testing.T) {
	for in, want := range map[uint64]string{
		0:          "",
		512:        "512B",
		1023:       "1023B",
		1024:       "1KB",
		1536:       "2KB",
		1536000:    "1.5MB",
		1 << 20:    "1.0MB",
		3 << 30:    "3.0GB",
		1288490189: "1.2GB",
	} {
		if got := FormatFileSize(in); got != want {
			t.Errorf("FormatFileSize(%d) = %q; want %q", in, got, want)
		}
	}
}

func TestSanitizeFilename(t *testing.T) {
	for in, want := range map[string]string{
		"群聊 A/B:c-d_e.txt": "群聊_A_B_c-d_e_txt",
		"Ⅻ²":               "Ⅻ²",
		"":                 "",
	} {
		if got := SanitizeFilename(in); got != want {
			t.Errorf("SanitizeFilename(%q) = %q; want %q", in, got, want)
		}
	}
}

func TestParsePacked(t *testing.T) {
	v2, ok := ParsePackedV2(packedImage(imageMetaTestJPG))
	if !ok || v2.Field1 != 1 || v2.Image == nil {
		t.Fatalf("v2 = %+v, %v", v2, ok)
	}
	if *v2.Image != (ImageMeta{Height: 1080, Width: 1920, Filename: "test.jpg"}) || v2.Image.Display() != "[img:test.jpg]" {
		t.Errorf("image = %+v", *v2.Image)
	}

	v1, ok := ParsePackedV1([]byte{8, 1, 16, 0, 26, 7, 'p', 'i', 'c', '.', 'p', 'n', 'g'})
	if !ok || v1.Filename != "pic.png" {
		t.Errorf("v1 = %+v, %v", v1, ok)
	}

	v2, ok = ParsePackedV2(packedFile("report.pdf"))
	if !ok || v2.File == nil || v2.File.Identifier() != "report.pdf" {
		t.Errorf("file = %+v, %v", v2, ok)
	}

	video := []byte{8, 1, 34, 6, 32, 0xf0, 0x01, 40, 0x80, 0x02}
	v2, ok = ParsePackedV2(video)
	if !ok || v2.Video == nil || v2.Video.Display() != "[视频 256x240]" {
		t.Errorf("video = %+v, %v", v2, ok)
	}

	for _, bad := range [][]byte{{}, {8}, {8, 0}, {0x1b}} {
		if _, ok := ParsePackedV2(bad); ok {
			t.Errorf("ParsePackedV2(%v) accepted", bad)
		}
	}
}
