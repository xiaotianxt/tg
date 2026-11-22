package decode

import "strings"

// transferStatus names a transfer's `<paysubtype>`. Pairing messages by
// transfer id showed 1 and 8 open a transfer, 3 follows when the receiver
// accepts it and 4 when it is returned. Other codes have no confirmed
// meaning and render as `状态<n>`.
func transferStatus(code string) string {
	switch code {
	case "1", "8":
		return "发起"
	case "3":
		return "已收款"
	case "4":
		return "已退还"
	}
	return "状态" + code
}

// decodeTransfer renders `[转账] ¥<amount> <status>[ - <memo>]` from
// `<wcpayinfo>`; false without an amount, so the caller keeps the card
// display.
func decodeTransfer(content string) (string, bool) {
	info, ok := FindXMLElement(content, "wcpayinfo")
	if !ok {
		return "", false
	}
	pay := info.Body(content)
	amount, ok := cdataField(pay, "feedesc")
	if !ok {
		return "", false
	}
	// The client writes the amount with either the full-width or the ASCII
	// yen sign; one form keeps output searchable.
	if rest, ok := strings.CutPrefix(amount, "￥"); ok {
		amount = "¥" + rest
	}
	out := "[转账] " + amount
	if code, ok := cdataField(pay, "paysubtype"); ok {
		out += " " + transferStatus(code)
	}
	if memo, ok := cdataField(pay, "pay_memo"); ok {
		out += " - " + memo
	}
	return out, true
}

// decodeRedPacket renders `[红包] <greeting>`; false without a greeting.
func decodeRedPacket(content string) (string, bool) {
	info, ok := FindXMLElement(content, "wcpayinfo")
	if !ok {
		return "", false
	}
	pay := info.Body(content)
	greeting, ok := cdataField(pay, "receivertitle")
	if !ok {
		greeting, ok = cdataField(pay, "sendertitle")
	}
	if !ok {
		return "", false
	}
	return "[红包] " + greeting, true
}
