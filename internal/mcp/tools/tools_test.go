package tools

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/mcp/config"
	"github.com/xiaotianxt/tg/internal/mcp/rpc"
	"github.com/xiaotianxt/tg/internal/msgdb"
)

func testPolicy(remote bool) policy {
	now := time.Unix(1_800_000_000, 0)
	return policy{rules: config.DefaultRemotePolicy(), remote: remote, now: func() time.Time { return now }}
}

func TestRemotePolicyHidesSessionsAndClampsLookback(t *testing.T) {
	p := testPolicy(true)
	p.rules.DenySessions = []string{"secret"}
	if p.visible("secret") || !p.visible("other") {
		t.Error("deny list")
	}
	p.rules.AllowSessions = []string{"only"}
	if p.visible("other") || !p.visible("only") {
		t.Error("allow list")
	}
	floor := p.floor()
	if !floor.Set || floor.At != 1_800_000_000-90*daySeconds {
		t.Fatalf("floor %+v", floor)
	}
	for _, c := range []struct{ in, want msgdb.Bound }{
		{msgdb.Bound{}, floor},
		{msgdb.At(0), floor},
		{msgdb.At(floor.At + 10), msgdb.At(floor.At + 10)},
	} {
		if got := p.clampSince(c.in); got != c.want {
			t.Errorf("clampSince(%+v) = %+v, want %+v", c.in, got, c.want)
		}
	}
	if p.nameMode() != contacts.Anonymous {
		t.Error("remote mode must hide remarks by default")
	}
	p.rules.ShowContactRemarks = true
	if p.nameMode() != contacts.PersonalRemark {
		t.Error("show_contact_remarks")
	}

	local := testPolicy(false)
	local.rules.DenySessions = []string{"secret"}
	if local.clampSince(msgdb.Bound{}).Set || !local.visible("secret") || local.nameMode() != contacts.PersonalRemark {
		t.Error("local mode applies no session or lookback rules")
	}
}

func TestTextIsCappedByCharacters(t *testing.T) {
	p := testPolicy(false)
	long := strings.Repeat("x", 10_000)
	if p.text(long) != long {
		t.Error("no cap by default")
	}
	three := 3
	p.rules.MaxTextChars = &three
	for in, want := range map[string]string{"你好世界": "你好世…[truncated]", "abc": "abc", "ab": "ab"} {
		if got := p.text(in); got != want {
			t.Errorf("text(%q) = %q, want %q", in, got, want)
		}
	}
}

type item struct {
	N   int    `json:"n"`
	Pad string `json:"pad"`
}

type itemsResult struct {
	Items []item `json:"items"`
	page
}

func TestFitTrimsFromTheEndAndContinuesAfterKeptItems(t *testing.T) {
	items := make([]item, 200)
	for i := range items {
		items[i] = item{i, strings.Repeat("x", 40)}
	}
	whole := func(s []item) any { return itemsResult{Items: s} }
	kept, cut := fit(2048, items, whole)
	if !cut || len(kept) == 0 || kept[0].N != 0 {
		t.Fatalf("cut=%v kept=%d", cut, len(kept))
	}
	out := itemsResult{Items: kept, page: continueAt(cut, 7, len(kept))}
	if n := jsonLen(out); n > 2048 {
		t.Errorf("trimmed result is %d bytes", n)
	}
	if *out.NextOffset != 7+len(kept) || !out.Truncated {
		t.Errorf("continuation %+v", out.page)
	}

	small, cut := fit(2048, items[:2], whole)
	if cut || len(small) != 2 || continueAt(cut, 7, 2) != (page{}) {
		t.Error("a whole result carries no continuation")
	}
	b, _ := rpc.Marshal(itemsResult{Items: small[:0]})
	if string(b) != `{"items":[]}` {
		t.Errorf("empty result %s", b)
	}
}

func message(date, minute, sender, text string) transcriptMessage {
	m := transcriptMessage{date: date, block: transcriptBlock(minute, sender, text)}
	if sender != "" {
		m.sender = senderLegendLine(sender, "id_"+sender)
	}
	return m
}

func TestTranscriptPrintsEachDayOnceAndIndentsContinuations(t *testing.T) {
	messages := []transcriptMessage{
		message("2026-11-06", "23:59", "张三", "晚安"),
		message("2026-11-07", "08:00", "李四", "早\r\n第二行\n"),
		message("2026-11-07", "08:01", "", "[系统消息]"),
		message("2026-11-07", "08:02", "张三", ""),
	}
	text, kept := fitTranscript("{count} msgs\n", messages, msgdb.Newest, 1<<30, 0)
	want := "4 msgs\nSenders (name = id):\n  张三 = id_张三\n  李四 = id_李四\n" +
		"== 2026-11-06 ==\n23:59 张三: 晚安\n" +
		"== 2026-11-07 ==\n08:00 李四: 早\n  第二行\n08:01 [系统消息]\n08:02 张三: \n"
	if text != want || kept != 4 {
		t.Errorf("got %d:\n%q\nwant\n%q", kept, text, want)
	}
}

func TestOversizedTranscriptKeepsThePagingEnd(t *testing.T) {
	messages := make([]transcriptMessage, 100)
	for i := range messages {
		messages[i] = message("2026-11-07", "12:00", "a", fmt.Sprintf("%03d", i))
	}
	const max = 600
	newest, kept := fitTranscript("{count}\n", messages, msgdb.Newest, max, 5)
	if len(newest) > max || kept == 0 || kept >= 100 {
		t.Fatalf("newest: %d bytes, %d kept", len(newest), kept)
	}
	if !strings.Contains(newest, "12:00 a: 099\n") || strings.Contains(newest, ": 000\n") ||
		!strings.HasPrefix(newest, fmt.Sprintf("%d\n", kept)) ||
		!strings.HasSuffix(newest, fmt.Sprintf("Repeat the same call with offset=%d to read them.\n", 5+kept)) {
		t.Errorf("newest transcript:\n%s", newest)
	}
	oldest, _ := fitTranscript("{count}\n", messages, msgdb.Oldest, max, 0)
	if len(oldest) > max || !strings.Contains(oldest, "12:00 a: 000\n") || !strings.Contains(oldest, "Newer messages") {
		t.Errorf("oldest transcript:\n%s", oldest)
	}
}

// callErr runs one tool call and returns its isError text, or nil.
func callErr(t *testing.T, c Context, name string, arguments string) error {
	t.Helper()
	r, err := NewRegistry(c)
	if err != nil {
		t.Fatal(err)
	}
	server := rpc.NewServer(r, "0")
	req := fmt.Sprintf(`{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":%q,"arguments":%s}}`, name, arguments)
	var resp struct {
		Result struct {
			Content []struct{ Text string } `json:"content"`
			IsError bool                    `json:"isError"`
		} `json:"result"`
	}
	if err := json.Unmarshal(server.Handle(context.Background(), []byte(req)).Response, &resp); err != nil {
		t.Fatal(err)
	}
	if resp.Result.IsError {
		return fmt.Errorf("%s", resp.Result.Content[0].Text)
	}
	return nil
}

func testContext(remote bool) Context {
	return Context{
		Env:   env.Env{Now: time.Now, Loc: time.UTC, Getenv: func(string) string { return "" }},
		DBDir: "/nonexistent/db_storage", Keys: "/nonexistent/keys.json", Jobs: 1,
		Policy: config.DefaultRemotePolicy(), Remote: remote,
	}
}

// Argument errors are reported before any database is opened; the context
// points at nothing, so reaching the store would fail differently.
func TestArgumentsAreValidatedBeforeTheStore(t *testing.T) {
	c := testContext(false)
	ids32 := func(n int) string {
		ids := make([]string, n)
		for i := range ids {
			ids[i] = `"` + strings.Repeat("0", 32) + `"`
		}
		return "[" + strings.Join(ids, ",") + "]"
	}
	for _, tc := range []struct{ tool, args, want string }{
		{"list_sessions", `{"limt":3,"zzz":1}`, "Unknown argument: limt"},
		{"list_sessions", `{"limit":10}`, "Unknown argument: limit"},
		{"read_messages", `{"session":"a","limit":10}`, "Unknown argument: limit"},
		{"search_messages", `{"query":"a","limit":10}`, "Unknown argument: limit"},
		{"query_messages", `{"limit":10}`, "Unknown argument: limit"},
		{"list_sessions", `{"offset":-1}`, "offset must be a non-negative integer"},
		{"list_sessions", `{"offset":1.5}`, "offset must be a non-negative integer"},
		{"list_sessions", `{"query":3}`, "query must be a string"},
		{"read_messages", `{"session":"  "}`, "session is required"},
		{"read_messages", `{"session":"a","all_time":"yes"}`, "all_time must be a boolean"},
		{"query_messages", `{"contains":[null]}`, "contains must be a list of strings"},
		{"query_messages", `{"contains":"x"}`, "contains must be a list of strings"},
		{"query_messages", `{"match":"some"}`, "match must be one of: all, any"},
		{"query_messages", `{}`, "refusing to return every message"},
		{"list_members", `{"session":"g","friends_only":true,"inactive_since":"1d"}`, "friends_only cannot be combined"},
		{"get_media", `{"session":"room@chatroom","ids":[]}`, "1 to 10"},
		{"get_media", `{"session":"room@chatroom","ids":` + ids32(11) + `}`, "1 to 10"},
		{"get_media", `{"session":"room@chatroom","ids":["../../etc/passwd"]}`, "not a media id"},
	} {
		err := callErr(t, c, tc.tool, tc.args)
		if err == nil || !strings.Contains(err.Error(), tc.want) {
			t.Errorf("%s %s: %v, want %q", tc.tool, tc.args, err, tc.want)
		}
	}
	// A copied tag is accepted; the store lookup is what fails here.
	err := callErr(t, c, "get_media", `{"session":"room@chatroom","ids":["[img:`+strings.Repeat("a", 32)+`]"]}`)
	if err == nil || strings.Contains(err.Error(), "not a media id") {
		t.Errorf("tag id: %v", err)
	}
}

func TestRemoteMediaIsOffUnlessTheOwnerAllowsIt(t *testing.T) {
	c := testContext(true)
	args := `{"session":"room@chatroom","ids":["` + strings.Repeat("a", 32) + `"]}`
	if err := callErr(t, c, "get_media", args); err == nil || !strings.Contains(err.Error(), "remote.allow_media") {
		t.Errorf("remote media: %v", err)
	}
	c.Policy.AllowMedia = true
	if err := callErr(t, c, "get_media", args); err == nil || strings.Contains(err.Error(), "remote.allow_media") {
		t.Errorf("allowed media: %v", err)
	}
}
