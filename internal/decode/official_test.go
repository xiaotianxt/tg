package decode

import (
	"strings"
	"testing"
)

// pushXML builds an official-account push shaped like the stored ones: the
// reader always carries a template header and detail, empty unless given.
func pushXML(header, detail, items string) string {
	return `<msg><appmsg appid="" sdkver="0"><title>outer</title><des></des><type>5</type><url>https://example.test/outer</url>` +
		`<mmreader><category type="20" count="1"><name><![CDATA[Acct]]></name><topnew><cover></cover><digest><![CDATA[top digest]]></digest></topnew>` +
		items + `</category><publisher><username><![CDATA[gh_acct]]></username><nickname><![CDATA[Acct Name]]></nickname></publisher>` +
		`<template_header>` + header + `</template_header><template_detail>` + detail + `</template_detail><forbid_forward>0</forbid_forward></mmreader>` +
		`</appmsg><fromusername><![CDATA[gh_acct]]></fromusername></msg>`
}

func itemXML(title, digest, url, shorturl string) string {
	return `<item><itemshowtype>0</itemshowtype><title><![CDATA[` + title + `]]></title><url><![CDATA[` + url + `]]></url>` +
		`<shorturl><![CDATA[` + shorturl + `]]></shorturl><longurl><![CDATA[]]></longurl><digest><![CDATA[` + digest + `]]></digest>` +
		`<finder_feed><nickname><![CDATA[]]></nickname><desc><![CDATA[]]></desc></finder_feed>` +
		`<finder_live><category><![CDATA[]]></category><nickname><![CDATA[]]></nickname></finder_live>` +
		`<sources><source><name><![CDATA[Item Source]]></name></source></sources></item>`
}

func lineXML(key, value string) string {
	return `<line><key><word><![CDATA[` + key + `]]></word><color><![CDATA[#000]]></color></key>` +
		`<value><word><![CDATA[` + value + `]]></word><color><![CDATA[#000]]></color></value></line>`
}

func TestDecodeOfficialPush(t *testing.T) {
	receipt := `<line_content><topline><key><word><![CDATA[付款金额]]></word><hide_dash_line>1</hide_dash_line></key>` +
		`<value><word><![CDATA[¥12.00]]></word><small_text_count>1</small_text_count></value></topline>` +
		`<lines>` + lineXML("收款方", "Shop") +
		`<line><key><word><![CDATA[隐藏键]]></word><hide>1</hide></key><value><word><![CDATA[only value]]></word></value></line>` +
		lineXML("备注", "") + `</lines></line_content><opitems><opitem><word><![CDATA[查看]]></word></opitem></opitems>`

	for _, tc := range []struct {
		name, body, want string
	}{
		{
			"single article shows digest",
			pushXML("", "", itemXML("Title One", "Digest one", "https://example.test/a?x=1&amp;y=2", "")),
			"[文章] Acct Name\n  · Title One - Digest one\n    https://example.test/a?x=1&y=2",
		},
		{
			"three articles omit digests",
			pushXML("", "", itemXML("T1", "D1", "https://example.test/1", "")+itemXML("T2", "D2", "https://example.test/2", "")+itemXML("T3", "", "https://example.test/3", "")),
			"[文章] Acct Name\n  · T1\n    https://example.test/1\n  · T2\n    https://example.test/2\n  · T3\n    https://example.test/3",
		},
		{
			"missing url falls back to shorturl, then none",
			pushXML("", "", itemXML("T1", "", "", "https://example.test/s")+itemXML("T2", "", "", "")),
			"[文章] Acct Name\n  · T1\n    https://example.test/s\n  · T2",
		},
		{
			"untitled items are skipped",
			pushXML("", "", itemXML("", "D0", "https://example.test/0", "")+itemXML("Only", "D1", "https://example.test/1", "")),
			"[文章] Acct Name\n  · Only - D1\n    https://example.test/1",
		},
		{
			"header falls back to item source",
			strings.Replace(pushXML("", "", itemXML("T", "", "", "")), "<nickname><![CDATA[Acct Name]]></nickname>", "", 1),
			"[文章] Item Source\n  · T",
		},
		{
			"receipt notice with topline and lines",
			pushXML(`<title><![CDATA[支付成功]]></title><title_color><![CDATA[#000]]></title_color>`, receipt, itemXML("Item title", "", "https://example.test/r", "")),
			"[通知] 支付成功\n  付款金额：¥12.00\n  收款方：Shop\n  only value\n  备注",
		},
		{
			"flat notice without header title uses item title and trims key colon",
			pushXML(`<title><![CDATA[]]></title>`, `<flat_content><lines>`+lineXML("时间：", "10:00")+lineXML("地点:", "Here")+`</lines></flat_content>`, itemXML("Reminder", "", "", "")),
			"[通知] Reminder\n  时间：10:00\n  地点：Here",
		},
		{
			"template without lines is an article",
			pushXML(`<title><![CDATA[Header only]]></title>`, `<line_content><lines></lines></line_content>`, itemXML("Art", "Dig", "", "")),
			"[文章] Acct Name\n  · Art - Dig",
		},
		{
			"no items and no lines keeps the link display",
			pushXML("", "", ""),
			"[链接] outer \n  https://example.test/outer",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := decode(5<<32|49, tc.body, "Acct", nil).Content; got != tc.want {
				t.Errorf("content =\n%q\nwant\n%q", got, tc.want)
			}
		})
	}
}

func TestDecodePayment(t *testing.T) {
	transfer := func(sub, fee, memo string) string {
		return `<msg><appmsg appid="" sdkver=""><title>转账</title><des>fallback des</des><type>2000</type>` +
			`<wcpayinfo><paysubtype>` + sub + `</paysubtype><feedesc><![CDATA[` + fee + `]]></feedesc><transcationid><![CDATA[t1]]></transcationid>` +
			`<transferid><![CDATA[x1]]></transferid><pay_memo><![CDATA[` + memo + `]]></pay_memo><begintransfertime><![CDATA[1]]></begintransfertime></wcpayinfo></appmsg></msg>`
	}
	packet := func(receiver, sender string) string {
		return `<msg><appmsg appid="" sdkver=""><title>红包标题</title><des>fallback des</des><type>2001</type>` +
			`<wcpayinfo><templateid><![CDATA[1]]></templateid><url><![CDATA[https://example.test/p]]></url><iconurl><![CDATA[]]></iconurl>` +
			`<receivertitle><![CDATA[` + receiver + `]]></receivertitle><sendertitle><![CDATA[` + sender + `]]></sendertitle><scenetext><![CDATA[红包]]></scenetext></wcpayinfo></appmsg></msg>`
	}

	for _, tc := range []struct {
		name, body, want string
	}{
		{"transfer sent (legacy code)", transfer("1", "￥12.00", ""), "[转账] ¥12.00 发起"},
		{"transfer sent with memo", transfer("8", "¥3.50", "lunch"), "[转账] ¥3.50 发起 - lunch"},
		{"transfer accepted", transfer("3", "￥12.00", ""), "[转账] ¥12.00 已收款"},
		{"transfer returned", transfer("4", "￥12.00", ""), "[转账] ¥12.00 已退还"},
		{"transfer unknown status", transfer("10", "￥1,000.00", ""), "[转账] ¥1,000.00 状态10"},
		{"transfer missing amount keeps card", transfer("8", "", "memo"), "[转账] 转账 - fallback des"},
		{"red packet greeting", packet("Best wishes", "Best wishes"), "[红包] Best wishes"},
		{"red packet sender title fallback", packet("", "From sender"), "[红包] From sender"},
		{"red packet with CDATA type", strings.Replace(packet("Wish", ""), "<type>2001</type>", "<type><![CDATA[2001]]></type>", 1), "[红包] Wish"},
		{"red packet without greeting keeps card", packet("", ""), "[红包] 红包标题 - fallback des"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			lt := int64(2000)<<32 | 49
			if strings.Contains(tc.body, "2001") {
				lt = int64(2001)<<32 | 49
			}
			if got := decode(lt, tc.body, "Alice", nil).Content; got != tc.want {
				t.Errorf("content =\n%q\nwant\n%q", got, tc.want)
			}
		})
	}
}
