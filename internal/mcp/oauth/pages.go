package oauth

import (
	"slices"
	"strings"
)

const (
	chatGPTStableRedirect = "https://chatgpt.com/connector_platform_oauth_redirect"
	chatGPTRedirectPrefix = "https://chatgpt.com/connector/oauth/"
)

// Gemini issues a per-user, per-server callback under these Google-owned
// hosts and registers all of them at once; the path shape is undocumented,
// so any plain path is accepted.
var geminiRedirectPrefixes = []string{
	"https://oauth-redirect.googleusercontent.com/",
	"https://oauth-redirect-sandbox.googleusercontent.com/",
	"https://oauth-redirect-test.googleusercontent.com/",
}

// RedirectAllowed reports whether a client may register uri: the ChatGPT
// stable callback, a single-segment ChatGPT per-connector callback, a plain
// path under a Gemini callback host, or one of extra exactly.
func RedirectAllowed(uri string, extra []string) bool {
	if uri == chatGPTStableRedirect || slices.Contains(extra, uri) {
		return true
	}
	if seg, ok := strings.CutPrefix(uri, chatGPTRedirectPrefix); ok {
		return seg != "" && allBytes(seg, func(c byte) bool { return isAlnum(c) || c == '-' || c == '_' })
	}
	// No query, fragment, percent-escapes, or dot segments, so the exact-match
	// check at /oauth/authorize compares the same path the browser will visit.
	for _, prefix := range geminiRedirectPrefixes {
		path, ok := strings.CutPrefix(uri, prefix)
		if !ok {
			continue
		}
		if path == "" || slices.ContainsFunc(strings.Split(path, "/"), func(s string) bool { return s == "." || s == ".." }) {
			return false
		}
		return allBytes(path, func(c byte) bool { return isAlnum(c) || strings.IndexByte("-_./", c) >= 0 })
	}
	return false
}

func isAlnum(c byte) bool {
	return 'a' <= c && c <= 'z' || 'A' <= c && c <= 'Z' || '0' <= c && c <= '9'
}

func allBytes(s string, ok func(byte) bool) bool {
	for i := 0; i < len(s); i++ {
		if !ok(s[i]) {
			return false
		}
	}
	return true
}

var htmlEscaper = strings.NewReplacer("&", "&amp;", "<", "&lt;", ">", "&gt;", `"`, "&quot;", "'", "&#39;")

const pageStyle = "body{font:16px/1.5 -apple-system,system-ui,sans-serif;max-width:28rem;margin:4rem auto;padding:0 1rem;color:#1d1d1f;background:#fff}" +
	"h1{font-size:1.3rem}input[type=password]{width:100%;padding:.6rem;font-size:1rem;box-sizing:border-box;border:1px solid #bbb;border-radius:6px}" +
	"button{margin-top:1rem;margin-right:.5rem;padding:.55rem 1.1rem;font-size:1rem;border-radius:6px;border:1px solid #888;background:#f4f4f4}" +
	"button.primary{background:#1d1d1f;color:#fff;border-color:#1d1d1f}.error{color:#b00020}.muted{color:#666;font-size:.9rem}" +
	"@media(prefers-color-scheme:dark){body{background:#111;color:#eee}input[type=password]{background:#222;color:#eee;border-color:#555}button{background:#222;color:#eee}button.primary{background:#eee;color:#111}}"

const pageHead = `<!doctype html><html><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">`

func simplePage(title, message string) string {
	return pageHead + "<title>tg</title><style>" + pageStyle + "</style></head>" +
		"<body><h1>" + htmlEscaper.Replace(title) + "</h1><p>" + htmlEscaper.Replace(message) + "</p></body></html>"
}

// consentPage asks the owner to allow clientName ("" for unnamed) to read,
// showing the host it will return to. errorText "" shows no error line.
func consentPage(csrf, clientName, redirectURI, errorText string) string {
	host := redirectURI
	if _, rest, ok := strings.Cut(redirectURI, "://"); ok {
		rest, _, _ = strings.Cut(rest, "://")
		host, _, _ = strings.Cut(rest, "/")
	}
	if clientName == "" {
		clientName = "An MCP client"
	}
	errorLine := ""
	if errorText != "" {
		errorLine = "<p class=error>" + htmlEscaper.Replace(errorText) + "</p>"
	}
	return pageHead + "<title>Allow access to tg</title><style>" + pageStyle + "</style></head>" +
		"<body><h1>Allow read-only access to your chat history?</h1>" +
		"<p><b>" + htmlEscaper.Replace(clientName) + "</b> will be able to list chats and read and search messages, within your tg remote policy. It will return to <b>" + htmlEscaper.Replace(host) + "</b>.</p>" +
		errorLine + `<form method=post action="/oauth/authorize"><input type=hidden name=request value="` + htmlEscaper.Replace(csrf) + `">` +
		`<label for=p>Owner passphrase</label><input id=p type=password name=passphrase autocomplete=current-password autofocus>` +
		`<div><button class=primary name=decision value=allow>Allow</button><button name=decision value=deny>Deny</button></div></form>` +
		`<p class=muted>Revoke later with <code>tg mcp revoke</code>.</p></body></html>`
}
