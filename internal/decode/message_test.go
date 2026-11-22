package decode

import (
	"bytes"
	"compress/zlib"
	"encoding/binary"
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/klauspost/compress/zstd"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

func names(m map[string]string) func(string) string {
	return func(id string) string {
		if n, ok := m[id]; ok {
			return n
		}
		return id
	}
}

func decodeWith(localType int64, body, session string, packed []byte, bucket tgtime.Bucket, resolve func(string) string) Message {
	return Decode(Raw{LocalType: localType, Body: body, Packed: packed},
		Ctx{SessionName: session, Bucket: bucket, Loc: time.UTC, ResolveName: resolve})
}

func decode(localType int64, body, session string, packed []byte) Message {
	return decodeWith(localType, body, session, packed, tgtime.Minutes(1), nil)
}

// packedImage is packed metadata with field1=1 and an image submessage.
func packedImage(img []byte) []byte {
	return append([]byte{8, 1, 26, byte(len(img))}, img...)
}

var imageMetaTestJPG = []byte{8, 0xb8, 0x08, 16, 0x80, 0x0f, 34, 8, 't', 'e', 's', 't', '.', 'j', 'p', 'g'}

func packedFile(name string) []byte {
	inner := append([]byte{8, 0, 18, byte(len(name))}, name...)
	file := append([]byte{10, byte(len(inner))}, inner...)
	return append([]byte{8, 3, 16, 6, 58, byte(len(file))}, file...)
}

func TestTypeLabels(t *testing.T) {
	for _, tc := range []struct {
		t    Type
		want string
	}{
		{TypeText, "文本"},
		{TypeImage, "图片"},
		{TypeMail, "邮件"},
		{TypeContactCard, "名片"},
		{TypeExternalCard, "外部联系人"},
		{TypeNotification, "通知"},
		{Type(123), "未知(123)"},
		{TypeOf(-1), "未知(-1)"},
	} {
		if got := tc.t.Label(); got != tc.want {
			t.Errorf("%d.Label() = %q; want %q", tc.t, got, tc.want)
		}
	}
	if TypeOf(49|6<<32) != TypeLink {
		t.Error("TypeOf keeps high bits")
	}
	if !TypeSystem.IsSystem() || !TypeRevoke.IsSystem() || TypeText.IsSystem() {
		t.Error("IsSystem")
	}
}

func TestDecodeContent(t *testing.T) {
	for _, tc := range []struct {
		name      string
		localType int64
		body      string
		packed    []byte
		want      string
	}{
		{"text", 1, "Hello", nil, "Hello"},
		{"system", 10000, "提示", nil, "提示"},
		{"revoke system", 10000, `<?xml version="1.0"?><sysmsg type="revokemsg"><revokemsg><content>&quot;张三&quot; 撤回了一条消息</content><revoketime>0</revoketime></revokemsg></sysmsg>`, nil, `[撤回] "张三" 撤回了一条消息`},
		{"system strips sender prefix", 10000, "tgid_abc:\n<sysmsg type=\"revokemsg\"><revokemsg><content>撤回了一条消息</content></revokemsg></sysmsg>", nil, "[撤回] 撤回了一条消息"},
		{"red envelope", 436207665, "", nil, "[红包]"},
		{"notification", 11000, "", nil, "[通知]"},
		{"image packed v2", 3, "", packedImage(imageMetaTestJPG), "[img:test.jpg]"},
		{"image xml", 3, `<msg><img aeskey="abc123" cdnthumbwidth="180" cdnthumbheight="153" rawlength="38186" /></msg>`, nil, "[img:abc123]"},
		{"image packed without name falls back to xml", 3, `<msg><img aeskey="xml-key" /></msg>`, packedImage([]byte{8, 0xb8, 0x08, 16, 0x80, 0x0f}), "[img:xml-key]"},
		{"image packed v1", 3, "", []byte{8, 1, 16, 0, 26, 7, 'p', 'i', 'c', '.', 'p', 'n', 'g'}, "[img:pic.png]"},
		{"image without identifier", 3, `<msg><img cdnthumbwidth="180" cdnthumbheight="153" rawlength="38186" /></msg>`, nil, "[img]"},
		{"voice without id", 34, `<msg><voicemsg endflag="1" voicelength="6600" length="1"/></msg>`, nil, "[语音 7秒]"},
		{"voice spaced attribute", 34, `<msg><voicemsg endflag = "1"  voicelength = "300" /></msg>`, nil, "[语音 1秒]"},
		{"voice legacy body", 34, "tgid_a7:5683:1\n", nil, "[语音 6秒]"},
		{"voice without length", 34, `<msg><voicemsg endflag="1"/></msg>`, nil, "[语音]"},
		{"link", 49, `<msg><appmsg><title>Long Link</title><des>Details</des><type>5</type><url>https://www.example.com/` + strings.Repeat("a", 180) + `</url></appmsg></msg>`, nil,
			"[链接] Long Link - Details\n  https://www.example.com/" + strings.Repeat("a", 180)},
		{"short link keeps url in display", 49, `<msg><appmsg><title>T</title><type>5</type><url>https://x.test/a?b=1&amp;c=2</url></appmsg></msg>`, nil, "[链接] T \n  https://x.test/a?b=1&c=2"},
		{"app subtype 4 video card", 49, `<?xml version="1.0"?>
<msg>
        <appmsg appid="test" sdkver="0">
                <title>她有时候只是激素影响，并不是故意的</title>
                <des>@有趣小剧场's note
38.2k Shares</des>
                <type>4</type>
                <url>https://www.example.com/video/1</url>
        </appmsg>
</msg>`, nil, "[视频] 她有时候只是激素影响，并不是故意的 - @有趣小剧场's note\n38.2k Shares\n  https://www.example.com/video/1"},
		{"subtype 51 short video", 49, `<?xml version="1.0"?>
<msg>
        <appmsg appid="" sdkver="0">
                <title>当前版本不支持展示该内容，请升级至最新版本。</title>
                <type>51</type>
                <url>https://www.example.com/security/readtemplate?t=upgrade</url>
                <finderFeed>
                        <feedType>4</feedType>
                        <title><![CDATA[货币周期]]></title>
                        <nickname><![CDATA[混哥趋势观]]></nickname>
                        <desc><![CDATA[接下来的几年，央妈要靠什么印钱？ #通胀 #AI]]></desc>
                        <mediaList><media><mediaType>4</mediaType></media></mediaList>
                </finderFeed>
        </appmsg>
</msg>`, nil, "[视频] 货币周期 - 接下来的几年，央妈要靠什么印钱？ #通胀 #AI - @混哥趋势观"},
		{"legacy subtype 51 reference", 49, `<msg><appmsg><title>Chat History</title><type>51</type></appmsg></msg>`, nil, "[引用: Chat History]"},
		{"prefixed app video xml in text", 1, `[视频] <?xml version="1.0"?><msg><appmsg><title>思朗诵，开始吟唱</title><type>4</type></appmsg></msg>`, nil, "[视频] 思朗诵，开始吟唱"},
		{"file with size", 49, `<msg><appmsg><title>report.pdf</title><type>6</type><appattach><totallen>1536</totallen><fileext>pdf</fileext></appattach></appmsg></msg>`, nil, "[文件] report.pdf (2KB)"},
		{"app subtype 62 file", 49, `<msg><appmsg><title>report.pdf</title><type>62</type><appattach><totallen>4096</totallen><fileext>pdf</fileext></appattach></appmsg></msg>`, nil, "[文件] report.pdf (4KB)"},
		{"pat is not a file", 49, `<msg><appmsg><title>我拍了拍 "Bob" 放了个炮[爆竹]</title><type>62</type><appattach><totallen>0</totallen><fileext></fileext></appattach><patinfo><fromusername>tgid_me</fromusername><pattedusername>tgid_bob</pattedusername><template>我拍了拍 "${tgid_bob}" 放了个炮[爆竹]</template></patinfo></appmsg></msg>`, nil, `[拍一拍] 我拍了拍 "Bob" 放了个炮[爆竹]`},
		{"file uses packed name", 49, `<msg><appmsg><type>6</type><appattach><totallen>0</totallen></appattach></appmsg></msg>`, packedFile("fallback.docx"), "[文件] fallback.docx"},
		{"plain file path", 62, "/a/b/report.pdf", nil, "[文件] report.pdf"},
		{"long multibyte file name", 62, strings.Repeat("文", 30), nil, "[文件] " + strings.Repeat("文", 25) + "..."},
		{"mail", 35, `<msg><pushmail><subject>Weekly Update</subject><sender>Alice</sender><digest>short</digest></pushmail></msg>`, nil, "[邮件] Weekly Update - Alice"},
		{"contact card", 42, `<msg username="tgid_card" nickname="Alice Card" alias="alice" />`, nil, "[名片] Alice Card"},
		{"external card", 66, `<msg username="tgid_external" nickname="External Alice" />`, nil, "[外部联系人] External Alice"},
		{"location", 48, `<msg><location x="1" y="2" label="Street" poiname="Cafe" /></msg>`, nil, "[位置] Cafe"},
		{"call", 50, "<msg><duration>125</duration></msg>", nil, "[通话] 2分5秒"},
		{"unknown app subtype", 49, `<msg><appmsg><title>Special Card</title><des>Details</des><type>87</type></appmsg></msg>`, nil, "[卡片:87] Special Card - Details"},
		{"group notice", 49, `<msg><appmsg><type>87</type><group_notice_item><announcement><textannouncement><![CDATA[课程资料、课程视频和练习均会开源]]></textannouncement></announcement></group_notice_item></appmsg></msg>`, nil, "[群公告] 课程资料、课程视频和练习均会开源"},
		{"unknown internal xml", 1, `<msg><quote username="tgid_a" displayname="Alice" /></msg>`, nil, "[消息]"},
		{"late internal xml tail", 1, "prefix <msg><emoji /></msg>", nil, "prefix\n[表情]"},
		{"sticker", 47, `<msg><emoji md5="abc123" /></msg>`, nil, "[sticker:abc123]"},
		{"sticker in text tail", 1, `prefix <msg><emoji md5="abc123" /></msg>`, nil, "prefix\n[sticker:abc123]"},
		{"app fallback drops xml summary", 49, `<msg><appmsg><title>&lt;msg&gt;&lt;quote /&gt;&lt;/msg&gt;</title><type>87</type></appmsg></msg>`, nil, "[卡片:87] 未知卡片"},
		{"chat history summary", 49, `<msg><appmsg><title>Chat History</title><des>A: first</des><type>19</type></appmsg></msg>`, nil, "[聊天记录] Chat History - A: first"},
		{"chat history items", 49, `<msg><appmsg><title>Chat History for A and B</title><des>A: first
B: second</des><type>19</type><recorditem><![CDATA[<recordinfo><datalist count="4"><dataitem datatype="1" dataid="a"><sourcename>A</sourcename><sourcetime>2026-04-29 07:39</sourcetime><datadesc>first</datadesc></dataitem><dataitem datatype="1" dataid="b"><sourcename>B</sourcename><sourcetime>2026-04-29 21:15</sourcetime><datadesc>second line
continues</datadesc></dataitem><dataitem datatype="1" dataid="c"><sourcename>B</sourcename><sourcetime>2026-04-29 21:15</sourcetime><datadesc>follow-up</datadesc></dataitem><dataitem datatype="2" dataid="img123"><sourcename>A</sourcename><sourcetime>2026-04-29 21:16</sourcetime></dataitem></datalist></recordinfo>]]></recorditem></appmsg></msg>`, nil,
			"[聊天记录] Chat History for A and B (4条)\n        [2026-04-29 07:39]\n        A: first\n        [2026-04-29 21:15]\n        B: second line\n          continues\n         follow-up\n        [2026-04-29 21:16]\n        A: [img:img123]"},
		{"nested chat history", 49, `<msg><appmsg><title>Outer History</title><type>19</type><recorditem><![CDATA[<recordinfo><datalist count="2"><dataitem datatype="1" dataid="a"><sourcename>A</sourcename><sourcetime>2026-05-01 10:00</sourcetime><datadesc>hello</datadesc></dataitem><dataitem datatype="19" dataid="nested"><sourcename>B</sourcename><sourcetime>2026-05-01 10:05</sourcetime><recorditem><recordinfo><title>Inner Record</title><datalist count="2"><dataitem datatype="1" dataid="x"><sourcename>C</sourcename><sourcetime>2026-05-01 09:00</sourcetime><datadesc>inner msg 1</datadesc></dataitem><dataitem datatype="1" dataid="y"><sourcename>D</sourcename><sourcetime>2026-05-01 09:01</sourcetime><datadesc>inner msg 2</datadesc></dataitem></datalist></recordinfo></recorditem></dataitem></datalist></recordinfo>]]></recorditem></appmsg></msg>`, nil,
			"[聊天记录] Outer History (2条)\n        [2026-05-01 10:00]\n        A: hello\n        [2026-05-01 10:05]\n        B: [聊天记录] Inner Record (2条)\n                  [2026-05-01 09:00]\n                  C: inner msg 1\n                  [2026-05-01 09:01]\n                  D: inner msg 2"},
		{"quote", 49, `<msg><appmsg><title>回复内容</title><type>57</type><refermsg><type>1</type><displayname>Bob</displayname><content>引用内容</content></refermsg></appmsg></msg>`, nil, "> Bob: 引用内容\n 回复内容"},
		{"quote uses embedded name for unknown sender", 49, `<msg><appmsg><title>回复内容</title><type>57</type><refermsg><type>1</type><displayname>Group Bob</displayname><chatusr>tgid_bob</chatusr><content>引用内容</content></refermsg></appmsg></msg>`, nil, "> Group Bob: 引用内容\n 回复内容"},
		{"quoted link with display prefix", 49, `<msg><appmsg><title>回复链接</title><type>57</type><refermsg><type>49</type><displayname>Bob</displayname><content>[链接] &lt;?xml version="1.0"?&gt;&lt;msg&gt;&lt;appmsg&gt;&lt;title&gt;Example&lt;/title&gt;&lt;type&gt;5&lt;/type&gt;&lt;/appmsg&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>`, nil, "> Bob: [链接] Example\n 回复链接"},
		// The client writes `<content>` before `<type>`, and the quoted app
		// message carries its own escaped `<type>`.
		{"quoted reply with content before type", 49, `<msg><appmsg><title>回复内容</title><type>57</type><refermsg><content>&lt;msg&gt;&lt;appmsg appid="" sdkver="0"&gt;&lt;title&gt;被引用的回复&lt;/title&gt;&lt;type&gt;57&lt;/type&gt;&lt;content&gt;&lt;/content&gt;&lt;/appmsg&gt;&lt;/msg&gt;</content><displayname>Bob</displayname><svrid>1</svrid><type>49</type><chatusr>tgid_bob</chatusr></refermsg></appmsg></msg>`, nil, "> Bob: 被引用的回复\n 回复内容"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := decode(tc.localType, tc.body, "Alice", tc.packed).Content; got != tc.want {
				t.Errorf("content =\n%q\nwant\n%q", got, tc.want)
			}
		})
	}
}

func TestDecodeDisplayName(t *testing.T) {
	bob := names(map[string]string{"tgid_abc": "Bob"})
	for _, tc := range []struct {
		name        string
		localType   int64
		body        string
		wantName    string
		wantContent string
	}{
		{"text keeps session name", 1, "Hello", "Alice", "Hello"},
		{"system", 10000, "提示", "系统", "提示"},
		{"group text sender", 1, "tgid_abc:\nHello", "Bob", "Hello"},
		{"group media sender", 3, "tgid_abc:\n", "Bob", "[img]"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			d := decodeWith(tc.localType, tc.body, "Alice", nil, tgtime.Minutes(1), bob)
			if d.DisplayName != tc.wantName || d.Content != tc.wantContent {
				t.Errorf("got (%q, %q); want (%q, %q)", d.DisplayName, d.Content, tc.wantName, tc.wantContent)
			}
		})
	}
}

func TestDecodeVoiceIncludesExportIDAndDuration(t *testing.T) {
	d := Decode(Raw{LocalType: 34, Body: `<msg><voicemsg endflag="1" voicelength="7000"/></msg>`, VoiceID: 42},
		Ctx{SessionName: "Alice", Bucket: tgtime.Minutes(1), Loc: time.UTC})
	if d.Content != "[voice:42:7s]" {
		t.Errorf("content = %q", d.Content)
	}
}

func TestDecodeXMLDeclaredAppLinkFromTextPath(t *testing.T) {
	xml := `<?xml version="1.0"?>
<msg>
        <appmsg appid="test" sdkver="0">
                <title>我说这家纽约zui好吃台湾菜应该没人反对吧</title>
                <des>@曼迪酱's note
95 Shares</des>
                <type>5</type>
                <url>https://www.example.com/item/1?x=1&amp;y=2</url>
        </appmsg>
</msg>`
	c := decode(1, xml, "Alice", nil).Content
	if !strings.HasPrefix(c, "[链接]") || !strings.Contains(c, "我说这家纽约zui好吃台湾菜应该没人反对吧") ||
		!strings.Contains(c, "@曼迪酱's note") || !strings.Contains(c, "https://www.example.com/item/1?x=1&y=2") ||
		strings.Contains(c, "<appmsg") {
		t.Errorf("content = %q", c)
	}
}

func TestLongNonXMLLinkTruncatesOnCharBoundary(t *testing.T) {
	c := decode(49, strings.Repeat("好", 220), "Alice", nil).Content
	if c != "[链接] "+strings.Repeat("好", 200)+"..." {
		t.Errorf("content = %q", c)
	}
}

func TestDecodeChatHistoryUsesRequestedBucket(t *testing.T) {
	xml := `<msg><appmsg><title>Chat History</title><type>19</type><recorditem><![CDATA[<recordinfo><datalist count="2"><dataitem datatype="1"><sourcename>A</sourcename><sourcetime>2026-04-29 21:15</sourcetime><datadesc>first</datadesc></dataitem><dataitem datatype="1"><sourcename>B</sourcename><sourcetime>2026-04-29 21:59</sourcetime><datadesc>second</datadesc></dataitem></datalist></recordinfo>]]></recorditem></appmsg></msg>`
	got := decodeWith(49, xml, "Alice", nil, tgtime.Hours(1), nil).Content
	want := "[聊天记录] Chat History (2条)\n        [2026-04-29 21:00]\n        A: first\n        B: second"
	if got != want {
		t.Errorf("content =\n%q\nwant\n%q", got, want)
	}
}

func TestDecodeNestedRecordXMLReadsFieldsAroundIt(t *testing.T) {
	// The nested record comes before this item's own sender and time, and
	// its own items carry the same field names.
	xml := `<msg><appmsg><title>Outer</title><type>19</type><recorditem><![CDATA[<recordinfo><datalist count="1"><dataitem datatype="17" dataid="n"><datatitle>Inner</datatitle><recordxml><recordinfo><title>Inner</title><desc>C: hi</desc><datalist count="1"><dataitem datatype="1"><sourcename>C</sourcename><sourcetime>2026-05-01 09:00</sourcetime><datadesc>hi</datadesc></dataitem></datalist></recordinfo></recordxml><sourcename>B</sourcename><sourcetime>2026-05-01 10:05</sourcetime></dataitem></datalist></recordinfo>]]></recorditem></appmsg></msg>`
	got := decodeWith(49, xml, "Alice", nil, tgtime.PerMessage, nil).Content
	want := "[聊天记录] Outer (1条)\n        [2026-05-01 10:05] B: [聊天记录] Inner (1条)\n                  [2026-05-01 09:00] C: hi"
	if got != want {
		t.Errorf("content =\n%q\nwant\n%q", got, want)
	}
}

func TestDecodeNestedChatHistoryStopsAtDepthLimit(t *testing.T) {
	record := `<dataitem datatype="1"><sourcename>E</sourcename><datadesc>deepest</datadesc></dataitem>`
	for level := maxChatHistoryDepth + 1; level >= 1; level-- {
		record = fmt.Sprintf(`<dataitem datatype="17"><datatitle>L%d</datatitle><recordxml><recordinfo><title>L%d</title><datalist>%s</datalist></recordinfo></recordxml><sourcename>S</sourcename></dataitem>`, level, level, record)
	}
	xml := `<msg><appmsg><title>L0</title><type>19</type><recorditem><![CDATA[<recordinfo><datalist>` + record + `</datalist></recordinfo>]]></recorditem></appmsg></msg>`
	c := decode(49, xml, "Alice", nil).Content
	if !strings.Contains(c, fmt.Sprintf("[聊天记录] L%d (1条)", maxChatHistoryDepth)) ||
		!strings.Contains(c, fmt.Sprintf("S: [记录] L%d", maxChatHistoryDepth+1)) ||
		strings.Contains(c, "deepest") {
		t.Errorf("content = %s", c)
	}
}

func TestDecodeQuote(t *testing.T) {
	for _, tc := range []struct {
		name, xml, want string
		names           map[string]string
	}{
		{"prefers resolved sender over embedded name",
			`<msg><appmsg><title>回复内容</title><type>57</type><refermsg><type>1</type><displayname>Group Bob</displayname><chatusr>tgid_bob</chatusr><content>引用内容</content></refermsg></appmsg></msg>`,
			"> Remark Bob: 引用内容\n 回复内容", map[string]string{"tgid_bob": "Remark Bob"}},
		{"quoted group image",
			"<msg><appmsg><title>回复图片</title><type>57</type><refermsg><type>3</type><chatusr>tgid_abc</chatusr><content>tgid_abc:\n&lt;msg&gt;&lt;img cdnthumbwidth=\"180\" cdnthumbheight=\"153\" length=\"38186\" /&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>",
			"> Bob: [img]\n 回复图片", map[string]string{"tgid_abc": "Bob"}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got := decodeWith(49, tc.xml, "Alice", nil, tgtime.Minutes(1), names(tc.names)).Content
			if got != tc.want {
				t.Errorf("content = %q; want %q", got, tc.want)
			}
		})
	}

	quotedFile := `<msg><appmsg><title>收到</title><type>57</type><refermsg><content>&lt;msg&gt;&lt;appmsg&gt;&lt;title&gt;plan.pdf&lt;/title&gt;&lt;type&gt;6&lt;/type&gt;&lt;appattach&gt;&lt;totallen&gt;2048&lt;/totallen&gt;&lt;fileext&gt;pdf&lt;/fileext&gt;&lt;/appattach&gt;&lt;/appmsg&gt;&lt;/msg&gt;</content><displayname>Bob</displayname><type>49</type></refermsg></appmsg></msg>`
	if c := decode(49, quotedFile, "Alice", nil).Content; !strings.HasPrefix(c, "> Bob: [文件] plan.pdf") || strings.Contains(c, "未知") {
		t.Errorf("quoted file content = %q", c)
	}

	nested := "<msg><appmsg><title>外层回复文本</title><type>57</type><refermsg><type>1</type><displayname>Bob</displayname><content>tgid_bob:\n被引用的占位文本\n&lt;msg&gt;&lt;appmsg&gt;&lt;title&gt;嵌套引用标题&lt;/title&gt;&lt;type&gt;57&lt;/type&gt;&lt;refermsg&gt;&lt;type&gt;3&lt;/type&gt;&lt;chatusr&gt;tgid_alice&lt;/chatusr&gt;&lt;content&gt;tgid_alice:\n&amp;lt;msg&amp;gt;&amp;lt;img aeskey=\"img-key\" /&amp;gt;&amp;lt;/msg&amp;gt;&lt;/content&gt;&lt;/refermsg&gt;&lt;/appmsg&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>"
	c := decodeWith(49, nested, "Alice", nil, tgtime.Minutes(1), names(map[string]string{"tgid_alice": "Alice", "tgid_bob": "Bob"})).Content
	for _, want := range []string{"被引用的占位文本", "嵌套引用标题", "外层回复文本"} {
		if !strings.Contains(c, want) {
			t.Errorf("nested quote lacks %q: %q", want, c)
		}
	}
	if strings.Contains(c, "<msg") || strings.Contains(c, "<appmsg") {
		t.Errorf("nested quote keeps raw XML: %q", c)
	}
}

func TestParseSenderPrefix(t *testing.T) {
	for _, tc := range []struct{ in, id, rest string }{
		{"tgid_a:\nHi", "tgid_a", "Hi"},
		{"room@chatroom:\nHi", "room@chatroom", "Hi"},
		{"gh_x y:\nz", "gh_x y", "z"},
		{"a-b@c:\n\nd", "a-b@c", "d"},
		{"plain text", "", "plain text"},
		{"", "", ""},
		{":abc", "", ":abc"},
		{"ab:\nx", "", "ab:\nx"},
		{"a b:\nc", "", "a b:\nc"},
		{"中文名:\nx", "", "中文名:\nx"},
		{"abc:", "", "abc:"},
		{"https://example.com/a", "", "https://example.com/a"},
		{"note:remember this", "", "note:remember this"},
		{"tgid_a: Hi", "", "tgid_a: Hi"},
		{"x y:abc:\nz", "", "x y:abc:\nz"},
	} {
		id, rest := ParseSenderPrefix(tc.in)
		if id != tc.id || rest != tc.rest {
			t.Errorf("ParseSenderPrefix(%q) = (%q, %q); want (%q, %q)", tc.in, id, rest, tc.id, tc.rest)
		}
	}
}

func zstdFrame(t *testing.T, text []byte) []byte {
	t.Helper()
	enc, err := zstd.NewWriter(nil)
	if err != nil {
		t.Fatal(err)
	}
	defer enc.Close()
	return enc.EncodeAll(text, nil)
}

func skippableFrame(payload []byte) []byte {
	out := []byte{0x50, 0x2A, 0x4D, 0x18}
	out = binary.LittleEndian.AppendUint32(out, uint32(len(payload)))
	return append(out, payload...)
}

func zlibStream(t *testing.T, text []byte) []byte {
	t.Helper()
	var buf bytes.Buffer
	w := zlib.NewWriter(&buf)
	if _, err := w.Write(text); err != nil {
		t.Fatal(err)
	}
	if err := w.Close(); err != nil {
		t.Fatal(err)
	}
	return buf.Bytes()
}

func cat(parts ...[]byte) []byte { return bytes.Join(parts, nil) }

func TestDecompress(t *testing.T) {
	big := strings.Repeat("长消息", 100_000)
	nihao := zstdFrame(t, []byte("你好你好你好你好"))
	lone := skippableFrame([]byte("hi"))
	for _, tc := range []struct {
		name string
		in   []byte
		want string
		ok   bool
	}{
		{"zstd", zstdFrame(t, []byte("你好")), "你好", true},
		{"zstd large", zstdFrame(t, []byte(big)), big, true},
		{"two zstd frames", cat(zstdFrame(t, []byte("ab")), zstdFrame(t, []byte("cd"))), "abcd", true},
		{"skippable frames", cat(skippableFrame([]byte("xyz")), zstdFrame(t, []byte("ab")), skippableFrame(nil)), "ab", true},
		// Output that is empty, not UTF-8, truncated, or followed by trailing
		// bytes is not zstd text; the raw bytes then decide.
		{"empty zstd frame", zstdFrame(t, nil), "", false},
		{"lone skippable frame is raw utf8", lone, string(lone), true},
		{"zstd non-utf8", zstdFrame(t, []byte{0xff, 0xfe}), "", false},
		{"truncated zstd", nihao[:len(nihao)-1], "", false},
		{"zstd trailing bytes", cat(zstdFrame(t, []byte("ab")), []byte("tail")), "", false},
		{"zstd one trailing byte", cat(zstdFrame(t, []byte("x")), []byte{0x28}), "", false},
		{"empty frame then text", cat(zstdFrame(t, nil), zstdFrame(t, []byte("ok"))), "ok", true},
		{"zlib", zlibStream(t, []byte("旧版")), "旧版", true},
		{"empty zlib", zlibStream(t, nil), "", false},
		{"plain ascii", []byte("plain text"), "plain text", true},
		{"plain utf8", []byte("纯文本"), "纯文本", true},
		{"empty", nil, "", false},
		{"binary", []byte{0xff, 0x00}, "", false},
		{"zstd magic only", []byte{0x28, 0xB5, 0x2F, 0xFD}, "", false},
		{"zstd magic and byte", []byte{0x28, 0xB5, 0x2F, 0xFD, 'a'}, "", false},
	} {
		got, ok := Decompress(tc.in)
		if got != tc.want || ok != tc.ok {
			t.Errorf("%s: Decompress = (%.40q, %v); want (%.40q, %v)", tc.name, got, ok, tc.want, tc.ok)
		}
	}
}
