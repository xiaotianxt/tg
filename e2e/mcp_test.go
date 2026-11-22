package e2e

import (
	"encoding/json"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
)

// mcpStdio sends requests to one `tg mcp stdio` process, one JSON line each,
// and returns its stdout lines decoded in order. stdout carries protocol
// messages only.
func mcpStdio(t *testing.T, e *Env, flags []string, requests ...any) []map[string]any {
	t.Helper()
	var stdin strings.Builder
	for _, request := range requests {
		if line, ok := request.(string); ok {
			stdin.WriteString(line + "\n")
			continue
		}
		line, err := json.Marshal(request)
		if err != nil {
			t.Fatal(err)
		}
		stdin.Write(line)
		stdin.WriteString("\n")
	}
	e.Stdin = stdin.String()
	r := e.run(t, fixtureArgs(append([]string{"mcp", "stdio"}, flags...)...)...).mustSucceed(t)
	var responses []map[string]any
	for _, line := range strings.Split(strings.TrimSuffix(r.Stdout, "\n"), "\n") {
		if line == "" {
			continue
		}
		var response map[string]any
		if err := json.Unmarshal([]byte(line), &response); err != nil {
			t.Fatalf("stdout line is not JSON: %q", line)
		}
		responses = append(responses, response)
	}
	return responses
}

func rpcRequest(id any, method string, params any) map[string]any {
	request := map[string]any{"jsonrpc": "2.0", "id": id, "method": method}
	if params != nil {
		request["params"] = params
	}
	return request
}

func toolCall(id any, name string, arguments any) map[string]any {
	params := map[string]any{"name": name}
	if arguments != nil {
		params["arguments"] = arguments
	}
	return rpcRequest(id, "tools/call", params)
}

func prettyJSON(t *testing.T, value any) string {
	t.Helper()
	var out strings.Builder
	enc := json.NewEncoder(&out)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	if err := enc.Encode(value); err != nil {
		t.Fatal(err)
	}
	return out.String()
}

var versionValue = regexp.MustCompile(`"version": "\d+\.\d+\.\d+"`)

func TestMCPProtocol(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	responses := mcpStdio(t, e, nil,
		rpcRequest(1, "initialize", map[string]any{
			"protocolVersion": "2025-06-18", "capabilities": map[string]any{},
			"clientInfo": map[string]any{"name": "e2e", "version": "0"},
		}),
		// Notifications and responses get no reply.
		map[string]any{"jsonrpc": "2.0", "method": "notifications/initialized"},
		map[string]any{"jsonrpc": "2.0", "id": 99, "result": map[string]any{}},
		rpcRequest(2, "initialize", map[string]any{"protocolVersion": "1999-01-01"}),
		rpcRequest("p", "ping", nil),
		rpcRequest(3, "tools/list", nil),
		rpcRequest(4, "resources/list", nil),
		"{not json",
		`[{"jsonrpc":"2.0","id":5,"method":"ping"}]`,
		`{"jsonrpc":"1.0","id":6,"method":"ping"}`,
		rpcRequest(7, "tools/call", map[string]any{}),
	)
	if len(responses) != 9 {
		t.Fatalf("got %d responses, want 9: %v", len(responses), responses)
	}

	init := responses[0]
	result, _ := init["result"].(map[string]any)
	if init["id"] != float64(1) || result["protocolVersion"] != "2025-06-18" {
		t.Errorf("initialize: %v", init)
	}
	if instructions, _ := result["instructions"].(string); strings.Contains(strings.ToLower(instructions), "telegram") ||
		!strings.Contains(instructions, appName()) {
		t.Errorf("instructions must name the app by its real name only")
	}
	golden(t, "mcp/initialize.json", versionValue.ReplaceAllString(coverAppNames(prettyJSON(t, result)), `"version": "<VERSION>"`))

	// An unsupported version gets the newest supported one.
	if v := responses[1]["result"].(map[string]any)["protocolVersion"]; v != "2026-07-28" {
		t.Errorf("unsupported version negotiated to %v", v)
	}
	assertJSONEqual(t, mustJSON(t, responses[2]), map[string]any{"jsonrpc": "2.0", "id": "p", "result": map[string]any{}})

	list := prettyJSON(t, responses[3]["result"])
	if strings.Contains(strings.ToLower(list), "telegram") {
		t.Error("tools/list must not show the cover name")
	}
	golden(t, "mcp/tools.json", coverAppNames(list))

	for i, want := range []struct {
		id   any
		code float64
	}{{float64(4), -32601}, {nil, -32700}, {nil, -32600}, {float64(6), -32600}, {float64(7), -32602}} {
		response := responses[4+i]
		errObj, _ := response["error"].(map[string]any)
		if response["id"] != want.id || errObj["code"] != want.code {
			t.Errorf("response %d: %v, want id %v code %v", 4+i, response, want.id, want.code)
		}
	}
}

func mustJSON(t *testing.T, value any) string {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return string(data)
}

type toolCase struct {
	name      string
	tool      string
	arguments any
}

// runToolGoldens calls each tool in one stdio session and freezes every
// result under testdata/mcp/<prefix><name>.json.
func runToolGoldens(t *testing.T, e *Env, flags []string, prefix string, cases []toolCase) {
	t.Helper()
	var requests []any
	for i, c := range cases {
		requests = append(requests, toolCall(i+1, c.tool, c.arguments))
	}
	responses := mcpStdio(t, e, flags, requests...)
	if len(responses) != len(cases) {
		t.Fatalf("got %d responses for %d calls", len(responses), len(cases))
	}
	for i, c := range cases {
		response := responses[i]
		if response["id"] != float64(i+1) {
			t.Fatalf("response %d has id %v", i, response["id"])
		}
		result, ok := response["result"].(map[string]any)
		if !ok {
			t.Errorf("%s: no result: %v", c.name, response)
			continue
		}
		// JSON results carry the same value as text and as structuredContent;
		// check they agree and freeze it once.
		if structured, ok := result["structuredContent"].(map[string]any); ok {
			content := result["content"].([]any)[0].(map[string]any)
			assertJSONEqual(t, content["text"].(string), structured)
			content["text"] = "<structuredContent>"
			// Diagnose reports what runs on the test machine; drop that check.
			if checks, ok := structured["checks"].([]any); ok {
				kept := []any{}
				for _, check := range checks {
					if name, _ := check.(map[string]any)["check"].(string); !strings.HasSuffix(name, " process") {
						kept = append(kept, check)
					}
				}
				structured["checks"] = kept
			}
		}
		golden(t, "mcp/"+prefix+c.name+".json", normalize(prettyJSON(t, result)))
	}
}

func TestMCPTools(t *testing.T) {
	t.Parallel()
	e := doctorEnv(t)
	mediaCache(t, e)
	missing := strings.Repeat("f", 32)
	runToolGoldens(t, e, nil, "", []toolCase{
		{"list_sessions", "list_sessions", map[string]any{}},
		{"list_sessions_query", "list_sessions", map[string]any{"query": "ali"}},
		{"list_sessions_offset", "list_sessions", map[string]any{"offset": 5}},
		{"list_unread", "list_unread", nil},
		{"list_unread_query", "list_unread", map[string]any{"query": "news"}},
		{"read_messages", "read_messages", map[string]any{"session": "tgid_alice"}},
		{"read_messages_by_name", "read_messages", map[string]any{"session": "爱丽丝", "since": "2024-06-02"}},
		{"read_messages_group", "read_messages", map[string]any{"session": "team@chatroom"}},
		{"read_messages_sender", "read_messages", map[string]any{"session": "team@chatroom", "sender": "tgid_bob"}},
		{"read_messages_sender_me", "read_messages", map[string]any{"session": "tgid_bob", "sender": "me", "since": "2024-06-03T08:25:00"}},
		// until is exclusive.
		{"read_messages_range", "read_messages", map[string]any{"session": "tgid_bob", "since": "2024-06-03T08:20:00", "until": "2024-06-03T08:22:00"}},
		{"read_messages_oldest_contains", "read_messages", map[string]any{"session": "tgid_bob", "order": "oldest", "contains": "message 1"}},
		{"read_messages_empty_range", "read_messages", map[string]any{"session": "tgid_alice", "until": "2024-01-01"}},
		{"search_messages", "search_messages", map[string]any{"query": "吃饭"}},
		{"search_messages_substring", "search_messages", map[string]any{"query": "message", "since": "2024-06-03T08:00:20"}},
		{"search_messages_none", "search_messages", map[string]any{"query": "nothing-at-all"}},
		{"query_messages_session", "query_messages", map[string]any{"session": "team@chatroom", "contains": []string{"e"}}},
		{"query_messages_has", "query_messages", map[string]any{"has": []string{"image", "video"}, "match": "any"}},
		{"query_messages_any", "query_messages", map[string]any{"contains": []string{"开会", "welcome"}, "match": "any", "order": "oldest"}},
		{"query_messages_not", "query_messages", map[string]any{"session": "tgid_alice", "contains": []string{"吃饭"}, "not_contains": []string{"明天"}}},
		{"query_messages_sender", "query_messages", map[string]any{"session": "tgid_bob", "sender": "me", "since": "2024-06-03T08:24:00", "until": "2024-06-03T08:26:15"}},
		{"list_members", "list_members", map[string]any{"session": "team@chatroom"}},
		{"list_members_friends", "list_members", map[string]any{"session": "项目群", "friends_only": true}},
		{"list_members_inactive", "list_members", map[string]any{"session": "team@chatroom", "inactive_since": "2024-06-01"}},
		{"get_media", "get_media", map[string]any{"session": "tgid_alice", "ids": []string{imageID, missing}}},
		{"get_media_original", "get_media", map[string]any{"session": "tgid_alice", "ids": []string{"[img:" + imageID + "]"}, "size": "original"}},
		{"get_media_thumbnail", "get_media", map[string]any{"session": "team@chatroom", "ids": []string{groupImage}}},
		{"diagnose", "diagnose", nil},
		{"diagnose_session", "diagnose", map[string]any{"session": "tgid_alice"}},
		// Failures are tool results the model can read, not protocol errors.
		{"error_unknown_argument", "list_sessions", map[string]any{"bogus": 1}},
		{"error_unknown_tool", "drop_tables", map[string]any{}},
		{"error_arguments_not_object", "list_sessions", "x"},
		{"error_missing_session", "read_messages", map[string]any{}},
		{"error_bad_time", "read_messages", map[string]any{"session": "tgid_alice", "since": "banana"}},
		{"error_bad_choice", "read_messages", map[string]any{"session": "tgid_alice", "order": "random"}},
		{"error_not_group", "list_members", map[string]any{"session": "tgid_alice"}},
		{"error_media_id", "get_media", map[string]any{"session": "tgid_alice", "ids": []string{"../x"}}},
		{"error_query_unbounded", "query_messages", map[string]any{}},
		{"error_empty_search", "search_messages", map[string]any{"query": ""}},
	})
}

// writeMCPConfig writes the remote-mode config tg reads from ~/.tg/mcp.
func writeMCPConfig(t *testing.T, e *Env, policy map[string]any) {
	t.Helper()
	dir := filepath.Join(e.Home, ".tg", "mcp")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	config := map[string]any{
		"public_url": "https://mcp.example.test",
		"listen":     "127.0.0.1:18765",
		"remote":     policy,
	}
	if err := os.WriteFile(filepath.Join(dir, "config.json"), []byte(mustJSON(t, config)), 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestMCPRemotePolicy(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	mediaCache(t, e)
	writeMCPConfig(t, e, map[string]any{
		"deny_sessions":     []string{"tgid_bob"},
		"max_lookback_days": 100000,
		"max_text_chars":    8,
	})
	runToolGoldens(t, e, []string{"--remote-policy"}, "remote_", []toolCase{
		{"read_messages_denied", "read_messages", map[string]any{"session": "tgid_bob"}},
		{"read_messages_truncated", "read_messages", map[string]any{"session": "tgid_alice", "since": "2024-06-02"}},
		// Contact remarks are hidden by default.
		{"search_messages", "search_messages", map[string]any{"query": "message"}},
		{"query_messages", "query_messages", map[string]any{"contains": []string{"latest"}}},
		{"get_media_disabled", "get_media", map[string]any{"session": "tgid_alice", "ids": []string{imageID}}},
	})

	// Denied chats disappear from listings.
	responses := mcpStdio(t, e, []string{"--remote-policy"}, toolCall(1, "list_sessions", nil))
	var ids []string
	for _, session := range responses[0]["result"].(map[string]any)["structuredContent"].(map[string]any)["sessions"].([]any) {
		ids = append(ids, session.(map[string]any)["id"].(string))
	}
	if strings.Join(ids, " ") != "team@chatroom tgid_alice gh_news quiet@chatroom tgid_alicia tgid_unknown" {
		t.Errorf("remote list_sessions ids: %v", ids)
	}

	// An allow list shows only its chats.
	e2 := newEnv(t)
	writeMCPConfig(t, e2, map[string]any{"allow_sessions": []string{"team@chatroom"}, "max_lookback_days": 100000, "show_contact_remarks": true})
	responses = mcpStdio(t, e2, []string{"--remote-policy"}, toolCall(1, "list_sessions", nil), toolCall(2, "read_messages", map[string]any{"session": "tgid_alice"}))
	sessions := responses[0]["result"].(map[string]any)["structuredContent"].(map[string]any)["sessions"].([]any)
	if len(sessions) != 1 || sessions[0].(map[string]any)["id"] != "team@chatroom" {
		t.Errorf("allow list: %v", sessions)
	}
	if responses[1]["result"].(map[string]any)["isError"] != true {
		t.Errorf("a chat outside the allow list was readable: %v", responses[1])
	}

	// The default lookback (90 days) hides the whole fixture history.
	e3 := newEnv(t)
	writeMCPConfig(t, e3, map[string]any{})
	responses = mcpStdio(t, e3, []string{"--remote-policy"}, toolCall(1, "search_messages", map[string]any{"query": "message"}))
	golden(t, "mcp/remote_default_lookback_search.json", prettyJSON(t, responses[0]["result"]))

	// Without a config, --remote-policy refuses to start.
	e4 := newEnv(t)
	e4.run(t, fixtureArgs("mcp", "stdio", "--remote-policy")...).mustFail(t, 1, "config.json not found")
}

func TestMCPListMembersLookback(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	writeMCPConfig(t, e, map[string]any{"max_lookback_days": 1})
	responses := mcpStdio(t, e, []string{"--remote-policy"}, toolCall(1, "list_members", map[string]any{"session": "team@chatroom"}))
	text := mustJSON(t, responses[0]["result"])
	for _, old := range []string{"2024-05-01", "2024-06-04"} {
		if strings.Contains(text, old) {
			t.Errorf("list_members shows activity from %s, outside a 1-day lookback:\n%s", old, text)
		}
	}
}

func TestMCPListSessionsHidesRemarks(t *testing.T) {
	t.Parallel()
	e := newEnv(t)
	writeMCPConfig(t, e, map[string]any{"max_lookback_days": 100000})
	responses := mcpStdio(t, e, []string{"--remote-policy"}, toolCall(1, "list_sessions", map[string]any{"query": "爱丽丝"}))
	sessions := responses[0]["result"].(map[string]any)["structuredContent"].(map[string]any)["sessions"].([]any)
	if len(sessions) == 0 {
		t.Fatal("a remark still finds its chat, but nothing matched")
	}
	alice := sessions[0].(map[string]any)
	if alice["id"] != "tgid_alice" || alice["name"] != "Alice Zhang" {
		t.Errorf("remote list_sessions shows %v, want tgid_alice under its public name", alice)
	}
}
