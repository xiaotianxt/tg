package e2e

import (
	"strings"
	"testing"
)

// cliCase is one tg run whose exit code and stdout are frozen in
// testdata/cli/<name>.golden. stderr is checked only for the listed
// fragments: log wording is not part of the contract, error substance is.
type cliCase struct {
	name      string
	args      []string
	stderrHas []string
}

func runGoldens(t *testing.T, cases []cliCase) {
	t.Helper()
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			t.Parallel()
			r := tg(t, c.args...)
			for _, want := range c.stderrHas {
				if !strings.Contains(r.Stderr, want) {
					t.Errorf("stderr lacks %q:\n%s", want, r.Stderr)
				}
			}
			goldenRun(t, c.name, r)
		})
	}
}

func TestSessions(t *testing.T) {
	runGoldens(t, []cliCase{
		// Most recent first; sessions never active (no timestamps) and rows
		// without a username are not listed. A session without a contact row
		// shows its username.
		{name: "sessions", args: []string{"sessions"}},
		{name: "sessions_top", args: []string{"sessions", "--top", "2"}},
		// A query ranks by match quality and reports matched of total.
		{name: "sessions_query", args: []string{"sessions", "ali"}},
		{name: "sessions_query_remark", args: []string{"sessions", "爱丽丝"}},
		{name: "sessions_no_match", args: []string{"sessions", "nobody-here"}},
		{name: "unread", args: []string{"unread"}},
		{name: "unread_query", args: []string{"unread", "news"}},
		{name: "unread_no_match", args: []string{"unread", "nobody-here"}},
		{name: "unread_top", args: []string{"unread", "--top", "1"}},
	})
}

func TestMembers(t *testing.T) {
	runGoldens(t, []cliCase{
		// Never-spoke members first, then by last recorded message; history
		// spans every shard.
		{name: "members", args: []string{"members", "team@chatroom"}},
		{name: "members_by_name", args: []string{"members", "项目群"}},
		// Anonymous mode shows group cards instead of contact remarks.
		{name: "members_anonymous", args: []string{"members", "team@chatroom", "--anonymous"}},
		{name: "members_inactive", args: []string{"members", "team@chatroom", "--inactive-since", "2024-06-01"}},
		// A cutoff before the local history warns that silence is unproven.
		{name: "members_inactive_before_history", args: []string{"members", "team@chatroom", "--inactive-since", "2024-01-01"}},
		{name: "members_no_history", args: []string{"members", "quiet@chatroom"}},
		// Friends: contact relationship bits only; strangers and the owner
		// are excluded.
		{name: "members_friends", args: []string{"members", "项目群", "--friends"}},
		{name: "members_friends_anonymous", args: []string{"members", "team@chatroom", "--friends", "--anonymous"}},
		{name: "members_not_group", args: []string{"members", "tgid_alice"}, stderrHas: []string{"Not a group session: tgid_alice"}},
		{name: "members_friends_not_group", args: []string{"members", "tgid_bob", "--friends"}, stderrHas: []string{"Not a group session: tgid_bob"}},
		{name: "members_unknown_group", args: []string{"members", "nobody@chatroom"}, stderrHas: []string{"Group not found: nobody@chatroom"}},
	})
}

func TestMembersRelativeInactivity(t *testing.T) {
	t.Parallel()
	// Every fixture message is older than a day.
	r := tg(t, "members", "team@chatroom", "--inactive-since", "1d").mustSucceed(t)
	r.stdoutHas(t, "Filter: no recorded messages since ", "Candidates: 6 of 6 current members (local records only)")
	tg(t, "members", "team@chatroom", "--inactive-since", "soon").
		mustFail(t, 1, "--inactive-since", "Cannot parse time expression 'soon'")
	tg(t, "members", "team@chatroom", "--friends", "--inactive-since", "1d").mustFail(t, 2)
}

func TestMessages(t *testing.T) {
	runGoldens(t, []cliCase{
		// Every decoded message type of a private chat across two shards:
		// text, image tag, voice with transcript, file, link, location, quote,
		// system notice, sticker, compressed bodies, forwarded chat history,
		// video, recall, and multi-line text.
		{name: "messages_private_all_types", args: []string{"messages", "tgid_alice", "--limit", "30"}},
		// An unknown first argument is a session for `messages`.
		{name: "messages_default_command", args: []string{"爱丽丝", "--limit", "3"}},
		// Default: the latest 50, chronological.
		{name: "messages_default_limit", args: []string{"messages", "tgid_bob"}},
		{name: "messages_all_time", args: []string{"messages", "tgid_bob", "--all-time", "--time-bucket", "1h"}},
		{name: "messages_limit", args: []string{"messages", "tgid_bob", "--limit", "3"}},
		// --offset counts back from the newest unless --head is given.
		{name: "messages_offset", args: []string{"messages", "tgid_bob", "--offset", "50"}},
		{name: "messages_offset_limit", args: []string{"messages", "tgid_bob", "--offset", "10", "--limit", "2"}},
		{name: "messages_tail", args: []string{"messages", "tgid_bob", "--tail", "--limit", "2"}},
		{name: "messages_head", args: []string{"messages", "tgid_bob", "--head", "--limit", "3"}},
		{name: "messages_head_offset", args: []string{"messages", "tgid_bob", "--head", "--limit", "3", "--offset", "2"}},
		{name: "messages_head_all", args: []string{"messages", "tgid_unknown", "--head", "--all-time"}},
		{name: "messages_since_absolute", args: []string{"messages", "tgid_alice", "--since", "2024-06-02"}},
		// A --since before the first stored message adds the history note.
		{name: "messages_since_before_history", args: []string{"messages", "爱丽丝", "--since", "100y", "--limit", "2"}},
		{name: "messages_since_after_history", args: []string{"messages", "tgid_alice", "--since", "1h"}},
		{name: "messages_search", args: []string{"messages", "tgid_alice", "--search", "吃饭"}},
		{name: "messages_search_like_escape", args: []string{"messages", "tgid_alice", "--search", "100%_"}},
		{name: "messages_group", args: []string{"messages", "team@chatroom"}},
		{name: "messages_group_anonymous", args: []string{"messages", "team@chatroom", "--anonymous"}},
		{name: "messages_sender_me", args: []string{"messages", "team@chatroom", "--sender", "me"}},
		{name: "messages_sender_self_name", args: []string{"messages", "tgid_bob", "--sender", "我", "--limit", "2"}},
		{name: "messages_sender_id", args: []string{"messages", "team@chatroom", "--sender", "tgid_bob"}},
		{name: "messages_sender_name", args: []string{"messages", "team@chatroom", "--sender", "bob"}},
		{name: "messages_sender_card", args: []string{"messages", "team@chatroom", "--sender", "小C"}},
		{name: "messages_sender_unknown", args: []string{"messages", "team@chatroom", "--sender", "Mallory"},
			stderrHas: []string{"No sender matched 'Mallory'"}},
		{name: "messages_bucket_full", args: []string{"messages", "team@chatroom", "--time-bucket", "full"}},
		{name: "messages_bucket_none", args: []string{"messages", "team@chatroom", "--time-bucket", "none"}},
		{name: "messages_bucket_hour", args: []string{"messages", "team@chatroom", "--time-bucket", "1h"}},
		{name: "messages_bucket_day", args: []string{"messages", "tgid_alice", "--time-bucket", "1d", "--limit", "4"}},
		{name: "messages_bucket_month", args: []string{"messages", "team@chatroom", "--time-bucket", "1mo"}},
		{name: "messages_bucket_year", args: []string{"messages", "team@chatroom", "--time-bucket", "1y"}},
		{name: "messages_bucket_minute_alias", args: []string{"messages", "tgid_bob", "--time-bucket", "1min", "--limit", "4"}},
		{name: "messages_no_contact", args: []string{"messages", "tgid_unknown"}},
		{name: "messages_empty_session", args: []string{"messages", "gh_news"}},
		{name: "messages_unknown_session", args: []string{"messages", "nobody-at-all"}},
	})
}

func TestMessagesArgumentErrors(t *testing.T) {
	t.Parallel()
	tg(t, "messages", "tgid_alice", "--since", "banana").mustFail(t, 1, "--since", "Cannot parse time expression 'banana'")
	tg(t, "messages", "tgid_alice", "--since", "1h", "--all-time").mustFail(t, 2, "--all-time")
	tg(t, "messages", "tgid_alice", "--head", "--tail").mustFail(t, 2)
	tg(t, "messages", "tgid_alice", "--time-bucket", "2d").mustFail(t, 2, "2d")
	tg(t, "messages", "tgid_alice", "--limit", "-1").mustFail(t, 2)
	tg(t, "messages").mustFail(t, 2)
}

func TestSearch(t *testing.T) {
	runGoldens(t, []cliCase{
		// An all-ideograph query runs on the full-text index; hits are listed
		// oldest first.
		{name: "search_ideographs", args: []string{"search", "吃饭"}},
		{name: "search_ideographs_since", args: []string{"search", "吃饭", "--since", "2024-06-01T09:00:10"}},
		// Other queries match substrings of the stored text.
		{name: "search_substring", args: []string{"search", "message"}},
		{name: "search_mixed", args: []string{"search", "开会 meeting"}},
		{name: "search_limit_more", args: []string{"search", "message", "--limit", "2"}},
		{name: "search_anonymous", args: []string{"search", "早上好", "--anonymous"}},
		{name: "search_all_time_accepted", args: []string{"search", "welcome", "--all-time"}},
		{name: "search_no_match", args: []string{"search", "nothing-at-all"}},
		{name: "search_since_after_all", args: []string{"search", "message", "--since", "1h"}},
		{name: "search_limit_zero", args: []string{"search", "message", "--limit", "0"},
			stderrHas: []string{"--limit must be greater than 0"}},
	})
}

func TestQuery(t *testing.T) {
	runGoldens(t, []cliCase{
		// Default: newest first, tab-separated time/session/sender/body, with
		// tabs and newlines in cells escaped.
		{name: "query_session", args: []string{"query", "--session", "tgid_alice", "--limit", "4"}},
		{name: "query_contains", args: []string{"query", "--contains", "吃饭"}},
		{name: "query_contains_all", args: []string{"query", "--contains", "bob", "--contains", "55"}},
		{name: "query_contains_any", args: []string{"query", "--contains", "开会", "--contains", "welcome", "--match-mode", "any"}},
		{name: "query_not", args: []string{"query", "--session", "team@chatroom", "--contains", "e", "--not", "welcome", "--not", "dave"}},
		// --raw-contains matches the stored body, here the file's XML.
		{name: "query_raw_contains", args: []string{"query", "--raw-contains", "<totallen>1536", "--fields", "session,type,body"}},
		{name: "query_has_any", args: []string{"query", "--session", "tgid_alice", "--has", "voice", "--has", "file", "--has", "video", "--has", "sticker", "--has", "image", "--match-mode", "any", "--fields", "type,body"}},
		{name: "query_has_image", args: []string{"query", "--has", "image", "--fields", "session,sender,body"}},
		{name: "query_sender", args: []string{"query", "--session", "team@chatroom", "--sender", "bob"}},
		{name: "query_sender_me", args: []string{"query", "--sender", "me", "--contains", "e", "--fields", "session,body"}},
		// The range avoids landing on a message so it holds whether --until is
		// inclusive (Rust) or exclusive (Go).
		{name: "query_since_until", args: []string{"query", "--session", "tgid_bob", "--since", "2024-06-03T08:10:00", "--until", "2024-06-03T08:11:15"}},
		{name: "query_order_offset", args: []string{"query", "--session", "tgid_bob", "--order", "oldest", "--limit", "3", "--offset", "1"}},
		{name: "query_fields_raw", args: []string{"query", "--session", "tgid_alice", "--contains", "compressed", "--fields", "timestamp,type,raw_body,body"}},
		{name: "query_max_cell_chars", args: []string{"query", "--contains", "bob message 5", "--max-cell-chars", "6", "--fields", "body"}},
		{name: "query_anonymous", args: []string{"query", "--session", "team@chatroom", "--contains", "welcome", "--anonymous"}},
		// Account-feed shards are not chat history.
		{name: "query_excludes_feed_shard", args: []string{"query", "--contains", "biz shard secret"}},
		{name: "query_no_rows", args: []string{"query", "--contains", "zzz-nothing"}},
		{name: "query_refuses_everything", args: []string{"query"},
			stderrHas: []string{"refusing to return every message"}},
		{name: "query_limit_zero", args: []string{"query", "--contains", "a", "--limit", "0"},
			stderrHas: []string{"--limit must be greater than 0"}},
		{name: "query_bad_field", args: []string{"query", "--contains", "a", "--fields", "time,bogus"},
			stderrHas: []string{"unknown field 'bogus'"}},
		{name: "query_cell_chars_too_big", args: []string{"query", "--contains", "a", "--max-cell-chars", "10001"},
			stderrHas: []string{"--max-cell-chars must be <= 10000"}},
		{name: "query_window_too_big", args: []string{"query", "--contains", "a", "--limit", "9000", "--offset", "1001"},
			stderrHas: []string{"--limit plus --offset must be <= 10000"}},
	})
}

func TestQueryJSON(t *testing.T) {
	t.Parallel()
	r := tg(t, "query", "--session", "tgid_bob", "--limit", "2", "--format", "json",
		"--fields", "time,session,sender,type,body,raw_body,timestamp").mustSucceed(t)
	lines := strings.Split(strings.TrimSuffix(r.Stdout, "\n"), "\n")
	if len(lines) != 2 {
		t.Fatalf("want one JSON object per line, got:\n%s", r.Stdout)
	}
	want := []map[string]any{
		{"time": "2024-06-03 08:27:00", "session": "Bob", "sender": "Bob", "type": float64(1),
			"body": "bob message 55", "raw_body": "bob message 55", "timestamp": float64(ts("2024-06-03 08:27:00"))},
		{"time": "2024-06-03 08:26:30", "session": "Bob", "sender": "我", "type": float64(1),
			"body": "bob message 54", "raw_body": "bob message 54", "timestamp": float64(ts("2024-06-03 08:26:30"))},
	}
	for i, line := range lines {
		assertJSONEqual(t, line, want[i])
	}
	// Only the requested fields are present.
	r = tg(t, "query", "--contains", "welcome", "--format", "json", "--fields", "body").mustSucceed(t)
	assertJSONEqual(t, strings.TrimSpace(r.Stdout), map[string]any{"body": "大家好 welcome"})
}

func TestQueryUntilExclusive(t *testing.T) {
	t.Parallel()
	r := tg(t, "query", "--session", "tgid_bob", "--since", "2024-06-03T08:10:00",
		"--until", "2024-06-03T08:11:00", "--fields", "body").mustSucceed(t)
	if r.Stdout != "body\nbob message 22\nbob message 21\n" {
		t.Errorf("--until must exclude the boundary message:\n%s", r.Stdout)
	}
}

func TestQueryJSONNoRows(t *testing.T) {
	t.Parallel()
	r := tg(t, "query", "--contains", "zzz-nothing", "--format", "json").mustSucceed(t)
	if r.Stdout != "" {
		t.Errorf("stdout must be empty, got %q", r.Stdout)
	}
	if !strings.Contains(r.Stderr, "No rows returned.") {
		t.Errorf("stderr lacks the no-rows note:\n%s", r.Stderr)
	}
}

func TestQueryArgumentErrors(t *testing.T) {
	t.Parallel()
	tg(t, "query", "--contains", "a", "--has", "gif").mustFail(t, 2, "gif")
	tg(t, "query", "--contains", "a", "--order", "random").mustFail(t, 2)
	tg(t, "query", "--contains", "a", "--format", "xml").mustFail(t, 2)
	tg(t, "query", "--contains", "a", "--match-mode", "some").mustFail(t, 2)
	tg(t, "query", "--contains", "a", "--since", "yesterday-ish").mustFail(t, 1, "yesterday-ish")
}
