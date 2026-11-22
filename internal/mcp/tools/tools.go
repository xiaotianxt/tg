// Package tools defines tg's read-only MCP tools: names, descriptions, input
// schemas, argument validation, the owner's remote policy, and trimming
// results to the size cap. Arguments are validated here, at the protocol
// boundary; the domain packages receive typed options.
package tools

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/decode"
	"github.com/xiaotianxt/tg/internal/doctor"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/history"
	"github.com/xiaotianxt/tg/internal/mcp/config"
	"github.com/xiaotianxt/tg/internal/mcp/rpc"
	"github.com/xiaotianxt/tg/internal/media"
	"github.com/xiaotianxt/tg/internal/query"
	"github.com/xiaotianxt/tg/internal/render"
	"github.com/xiaotianxt/tg/internal/resolve"
	"github.com/xiaotianxt/tg/internal/store"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// Context is what every tool call reads from.
type Context struct {
	Env env.Env
	// DBDir is the source directory ("" auto-detects); Keys is all_keys.json.
	DBDir, Keys string
	Jobs        int
	// Policy's size caps apply in every mode; its session and lookback rules
	// only when Remote is set.
	Policy config.RemotePolicy
	Remote bool
}

// tools carries one registry's context into the handlers.
type tools struct {
	Context
	policy policy
}

// NewRegistry builds the eight tools for one server.
func NewRegistry(c Context) (*rpc.Registry, error) {
	t := &tools{Context: c, policy: policy{rules: c.Policy, remote: c.Remote, now: c.Env.Now}}
	handlers := map[string]func(args) (rpc.Result, error){
		"list_sessions":   func(a args) (rpc.Result, error) { return t.listSessions(a, history.AllSessions) },
		"list_unread":     func(a args) (rpc.Result, error) { return t.listSessions(a, history.UnreadSessions) },
		"read_messages":   t.readMessages,
		"search_messages": t.searchMessages,
		"query_messages":  t.queryMessages,
		"list_members":    t.listMembers,
		"get_media":       t.getMedia,
		"diagnose":        t.diagnose,
	}
	list := make([]rpc.Tool, 0, len(definitions))
	for _, d := range definitions {
		h := handlers[d.name]
		list = append(list, rpc.Tool{
			Name: d.name, Title: d.title, Description: d.description,
			InputSchema: d.schema(), Annotations: readOnly,
			Handler: func(_ context.Context, raw json.RawMessage) (rpc.Result, error) {
				a, err := parseArgs(raw)
				if err != nil {
					return rpc.Result{}, err
				}
				return h(a)
			},
		})
	}
	return rpc.NewRegistry(list...)
}

// withStore opens the source store for one call.
func (t *tools) withStore(fn func(st *store.Store) (rpc.Result, error)) (rpc.Result, error) {
	st, err := store.Discover(t.DBDir, t.Keys)
	if err != nil {
		return rpc.Result{}, err
	}
	defer st.Close()
	return fn(st)
}

func (t *tools) now() time.Time { return t.Env.Now() }

// noSession is the same text for a hidden session as for a missing one, so
// a remote caller cannot tell that a denied session exists.
func noSession(query string) error {
	return fmt.Errorf("No chat matched '%s'. Use list_sessions to find chat ids.", query)
}

type timeValue struct {
	Local string `json:"local"`
	Unix  int64  `json:"unix"`
}

func (t *tools) timeValue(ts int64) timeValue {
	return timeValue{Local: tgtime.FormatLocal(ts, t.Env.Loc), Unix: ts}
}

func (t *tools) optionalTime(ts int64, ok bool) *timeValue {
	if !ok {
		return nil
	}
	v := t.timeValue(ts)
	return &v
}

type chatRef struct {
	ID   string `json:"id"`
	Name string `json:"name"`
}

func messageKind(localType int64) string {
	switch localType & 0xFFFF_FFFF {
	case 1:
		return "text"
	case 3:
		return "image"
	case 34:
		return "voice"
	case 42:
		return "contact_card"
	case 43:
		return "video"
	case 47:
		return "sticker"
	case 48:
		return "location"
	case 49:
		return "app"
	case 50:
		return "call"
	case 10000, 10002:
		return "system"
	}
	return "other"
}

func jsonResult(v any, rows int) (rpc.Result, error) {
	b, err := rpc.Marshal(v)
	if err != nil {
		return rpc.Result{}, err
	}
	return rpc.JSONResult(b, rows), nil
}

// visibleFrom keeps the items whose session the policy shows, then skips
// offset of them: offsets count only rows the caller can see.
func visibleFrom[T any](p policy, items []T, session func(T) string, offset int) []T {
	out := []T{}
	for _, it := range items {
		if p.visible(session(it)) {
			out = append(out, it)
		}
	}
	return out[min(offset, len(out)):]
}

type sessionItem struct {
	ID         string    `json:"id"`
	LastActive timeValue `json:"last_active"`
	Name       string    `json:"name"`
	Unread     int64     `json:"unread"`
}

type sessionsResult struct {
	page
	Sessions []sessionItem `json:"sessions"`
}

func (t *tools) listSessions(a args, kind history.SessionListKind) (rpc.Result, error) {
	if err := a.only("query", "offset"); err != nil {
		return rpc.Result{}, err
	}
	q, err := a.str("query")
	if err != nil {
		return rpc.Result{}, err
	}
	offset, err := a.offset()
	if err != nil {
		return rpc.Result{}, err
	}
	return t.withStore(func(st *store.Store) (rpc.Result, error) {
		list, err := history.ListSessionsAs(st, kind, int(^uint(0)>>1), q, t.policy.nameMode())
		if err != nil {
			return rpc.Result{}, err
		}
		shown := visibleFrom(t.policy, list.Sessions, func(s history.SessionSummary) string { return s.Username }, offset)
		items := make([]sessionItem, len(shown))
		for i, s := range shown {
			items[i] = sessionItem{ID: s.Username, LastActive: t.timeValue(s.LastActive), Name: s.DisplayName, Unread: s.UnreadCount}
		}
		items, cut := fit(t.policy.maxBytes(), items, func(s []sessionItem) any { return sessionsResult{Sessions: s} })
		return jsonResult(sessionsResult{page: continueAt(cut, offset, len(items)), Sessions: items}, len(items))
	})
}

func (t *tools) readMessages(a args) (rpc.Result, error) {
	if err := a.only("session", "order", "offset", "since", "until", "contains", "sender", "all_time"); err != nil {
		return rpc.Result{}, err
	}
	session, err := a.requiredStr("session")
	if err != nil {
		return rpc.Result{}, err
	}
	order, err := a.order()
	if err != nil {
		return rpc.Result{}, err
	}
	offset, err := a.offset()
	if err != nil {
		return rpc.Result{}, err
	}
	since, err := a.time("since", t.now(), t.Env.Loc)
	if err != nil {
		return rpc.Result{}, err
	}
	until, err := a.time("until", t.now(), t.Env.Loc)
	if err != nil {
		return rpc.Result{}, err
	}
	contains, err := a.str("contains")
	if err != nil {
		return rpc.Result{}, err
	}
	sender, err := a.str("sender")
	if err != nil {
		return rpc.Result{}, err
	}
	// Every tool reads the full history by default; all_time is only
	// type-checked for callers written against the old recent window.
	if _, err := a.bool("all_time"); err != nil {
		return rpc.Result{}, err
	}
	return t.withStore(func(st *store.Store) (rpc.Result, error) {
		pg, err := history.ReadMessages(st, history.ReadOptions{
			Session: session, Offset: offset, Search: contains,
			Since: t.policy.clampSince(since), Until: until, Order: order, Sender: sender,
			Bucket: tgtime.PerMessage, Mode: t.policy.nameMode(), Jobs: t.Jobs, Loc: t.Env.Loc,
		})
		if err != nil {
			return rpc.Result{}, err
		}
		if !t.policy.visible(pg.Username) {
			return rpc.Result{}, noSession(session)
		}
		messages := make([]transcriptMessage, len(pg.Messages))
		for i, m := range pg.Messages {
			date, minute := tgtime.FormatLocalDateAndMinute(m.CreateTime, t.Env.Loc)
			messages[i] = transcriptMessage{
				date:   date,
				block:  transcriptBlock(minute, m.Sender, t.policy.text(m.Content)),
				sender: senderLegendLine(m.Sender, m.SenderID),
			}
		}
		var header strings.Builder
		fmt.Fprintf(&header, "Chat: %s (id: %s)\n", pg.DisplayName, pg.Username)
		if pg.HasHistory {
			// The lookback floor also hides when older history begins.
			start := pg.HistoryStart
			if f := t.policy.floor(); f.Set {
				start = max(start, f.At)
			}
			fmt.Fprintf(&header, "Readable history starts %s; nothing earlier is available.\n", tgtime.FormatLocal(start, t.Env.Loc))
		}
		header.WriteString("{count} messages in time order. Times are local; an indented line continues the message above it.\n")
		text, kept := fitTranscript(header.String(), messages, order, t.policy.maxBytes(), offset)
		return rpc.TextResult(text, kept), nil
	})
}

type searchHit struct {
	Session chatRef   `json:"session"`
	Text    string    `json:"text"`
	Time    timeValue `json:"time"`
	Type    string    `json:"type"`
}

type searchResult struct {
	Hits []searchHit `json:"hits"`
	page
}

func (t *tools) searchMessages(a args) (rpc.Result, error) {
	if err := a.only("query", "since", "all_time", "offset"); err != nil {
		return rpc.Result{}, err
	}
	q, err := a.requiredStr("query")
	if err != nil {
		return rpc.Result{}, err
	}
	offset, err := a.offset()
	if err != nil {
		return rpc.Result{}, err
	}
	since, err := a.time("since", t.now(), t.Env.Loc)
	if err != nil {
		return rpc.Result{}, err
	}
	if _, err := a.bool("all_time"); err != nil {
		return rpc.Result{}, err
	}
	return t.withStore(func(st *store.Store) (rpc.Result, error) {
		res, err := history.Search(st, history.SearchOptions{
			Query: q, Since: t.policy.clampSince(since), Mode: t.policy.nameMode(), Jobs: t.Jobs,
		})
		if err != nil {
			return rpc.Result{}, err
		}
		// Hits arrive oldest first; return newest first.
		hits := slices.Clone(res.Hits)
		slices.Reverse(hits)
		shown := visibleFrom(t.policy, hits, func(h history.SearchHit) string { return h.Session }, offset)
		items := make([]searchHit, len(shown))
		for i, h := range shown {
			items[i] = searchHit{
				Session: chatRef{h.Session, h.SessionDisplay}, Text: t.policy.text(h.Content),
				Time: t.timeValue(h.CreateTime), Type: messageKind(h.LocalType),
			}
		}
		items, cut := fit(t.policy.maxBytes(), items, func(s []searchHit) any { return searchResult{Hits: s} })
		return jsonResult(searchResult{Hits: items, page: continueAt(cut, offset, len(items))}, len(items))
	})
}

type queryMessage struct {
	Sender   string    `json:"sender"`
	SenderID string    `json:"sender_id"`
	Session  chatRef   `json:"session"`
	Text     string    `json:"text"`
	Time     timeValue `json:"time"`
	Type     string    `json:"type"`
}

type queryResult struct {
	Messages []queryMessage `json:"messages"`
	page
}

func (t *tools) queryMessages(a args) (rpc.Result, error) {
	if err := a.only("session", "include_official", "contains", "not_contains", "match", "has", "sender", "since", "until", "all_time", "order", "offset"); err != nil {
		return rpc.Result{}, err
	}
	o := query.Options{Mode: t.policy.nameMode(), Jobs: t.Jobs, Loc: t.Env.Loc}
	var err error
	if o.Session, err = a.str("session"); err != nil {
		return rpc.Result{}, err
	}
	if o.Official, err = a.bool("include_official"); err != nil {
		return rpc.Result{}, err
	}
	if o.Contains, err = a.strList("contains"); err != nil {
		return rpc.Result{}, err
	}
	if o.Not, err = a.strList("not_contains"); err != nil {
		return rpc.Result{}, err
	}
	match, err := a.choice("match", "all", "all", "any")
	if err != nil {
		return rpc.Result{}, err
	}
	if match == "any" {
		o.Match = query.MatchAny
	}
	has, err := a.strList("has")
	if err != nil {
		return rpc.Result{}, err
	}
	for _, h := range has {
		m, err := query.ParseMedia(h)
		if err != nil {
			return rpc.Result{}, err
		}
		o.Has = append(o.Has, m)
	}
	if o.Sender, err = a.str("sender"); err != nil {
		return rpc.Result{}, err
	}
	since, err := a.time("since", t.now(), t.Env.Loc)
	if err != nil {
		return rpc.Result{}, err
	}
	o.Since = t.policy.clampSince(since)
	if _, err := a.bool("all_time"); err != nil {
		return rpc.Result{}, err
	}
	if o.Until, err = a.time("until", t.now(), t.Env.Loc); err != nil {
		return rpc.Result{}, err
	}
	if o.Order, err = a.order(); err != nil {
		return rpc.Result{}, err
	}
	offset, err := a.offset()
	if err != nil {
		return rpc.Result{}, err
	}
	// Run validates too; checking here first keeps a bad call from opening
	// any database.
	if err := o.Validate(); err != nil {
		return rpc.Result{}, err
	}
	return t.withStore(func(st *store.Store) (rpc.Result, error) {
		rows, err := query.Run(st, o)
		if err != nil {
			return rpc.Result{}, err
		}
		shown := visibleFrom(t.policy, rows, func(r query.Row) string { return r.SessionID }, offset)
		items := make([]queryMessage, len(shown))
		for i, r := range shown {
			items[i] = queryMessage{
				Sender: r.Sender, SenderID: r.SenderID, Session: chatRef{r.SessionID, r.Session},
				Text: t.policy.text(r.Body), Time: t.timeValue(r.CreateTime), Type: messageKind(r.LocalType),
			}
		}
		items, cut := fit(t.policy.maxBytes(), items, func(s []queryMessage) any { return queryResult{Messages: s} })
		return jsonResult(queryResult{Messages: items, page: continueAt(cut, offset, len(items))}, len(items))
	})
}

type friendItem struct {
	ID   string `json:"id"`
	Name string `json:"name"`
}

type friendsResult struct {
	CachedMembers int          `json:"cached_members"`
	Friends       []friendItem `json:"friends"`
	Group         chatRef      `json:"group"`
	Truncated     bool         `json:"truncated,omitempty"`
}

type memberItem struct {
	ID           string     `json:"id"`
	LastSpoke    *timeValue `json:"last_spoke"`
	MessageCount int64      `json:"message_count"`
	Name         string     `json:"name"`
	Owner        bool       `json:"owner"`
}

type historySpan struct {
	End   *timeValue `json:"end"`
	Start *timeValue `json:"start"`
}

type membersResult struct {
	Group        chatRef      `json:"group"`
	History      historySpan  `json:"history"`
	Members      []memberItem `json:"members"`
	TotalMembers int          `json:"total_members"`
	Truncated    bool         `json:"truncated,omitempty"`
}

func (t *tools) listMembers(a args) (rpc.Result, error) {
	if err := a.only("session", "inactive_since", "friends_only"); err != nil {
		return rpc.Result{}, err
	}
	session, err := a.requiredStr("session")
	if err != nil {
		return rpc.Result{}, err
	}
	friendsOnly, err := a.bool("friends_only")
	if err != nil {
		return rpc.Result{}, err
	}
	inactiveSince, err := a.time("inactive_since", t.now(), t.Env.Loc)
	if err != nil {
		return rpc.Result{}, err
	}
	if friendsOnly && inactiveSince.Set {
		return rpc.Result{}, errors.New("friends_only cannot be combined with inactive_since")
	}
	return t.withStore(func(st *store.Store) (rpc.Result, error) {
		if friendsOnly {
			r, err := history.ListMemberFriends(st, session, t.policy.nameMode(), resolve.Options{Jobs: t.Jobs, Loc: t.Env.Loc})
			if err != nil {
				return rpc.Result{}, err
			}
			if !t.policy.visible(r.GroupUsername) {
				return rpc.Result{}, noSession(session)
			}
			items := make([]friendItem, len(r.Friends))
			for i, f := range r.Friends {
				items[i] = friendItem{f.Username, f.DisplayName}
			}
			whole := func(s []friendItem) friendsResult {
				return friendsResult{CachedMembers: r.CachedMembers, Friends: s, Group: chatRef{r.GroupUsername, r.GroupDisplayName}}
			}
			items, cut := fit(t.policy.maxBytes(), items, func(s []friendItem) any { return whole(s) })
			out := whole(items)
			out.Truncated = cut
			return jsonResult(out, len(items))
		}
		r, err := history.ListMembers(st, history.MembersOptions{
			Session: session, InactiveSince: inactiveSince, Since: t.policy.floor(),
			Mode: t.policy.nameMode(), Jobs: t.Jobs, Loc: t.Env.Loc,
		})
		if err != nil {
			return rpc.Result{}, err
		}
		if !t.policy.visible(r.GroupUsername) {
			return rpc.Result{}, noSession(session)
		}
		items := make([]memberItem, len(r.Members))
		for i, m := range r.Members {
			items[i] = memberItem{
				ID: m.Username, LastSpoke: t.optionalTime(m.LastSpoke, m.HasSpoken),
				MessageCount: m.MessageCount, Name: m.DisplayName, Owner: m.IsOwner,
			}
		}
		whole := func(s []memberItem) membersResult {
			return membersResult{
				Group: chatRef{r.GroupUsername, r.GroupDisplayName},
				History: historySpan{
					End:   t.optionalTime(r.HistoryEnd, r.HasHistory),
					Start: t.optionalTime(r.HistoryStart, r.HasHistory),
				},
				Members: s, TotalMembers: r.TotalMembers,
			}
		}
		items, cut := fit(t.policy.maxBytes(), items, func(s []memberItem) any { return whole(s) })
		out := whole(items)
		out.Truncated = cut
		return jsonResult(out, len(items))
	})
}

func (t *tools) getMedia(a args) (rpc.Result, error) {
	if err := a.only("session", "ids", "size"); err != nil {
		return rpc.Result{}, err
	}
	if t.Remote && !t.Policy.AllowMedia {
		return rpc.Result{}, errors.New("Images are disabled for remote clients. The owner can enable them " +
			"with remote.allow_media in the tg MCP config.")
	}
	session, err := a.requiredStr("session")
	if err != nil {
		return rpc.Result{}, err
	}
	sizeName, err := a.choice("size", "preview", "preview", "original")
	if err != nil {
		return rpc.Result{}, err
	}
	size := media.Preview
	if sizeName == "original" {
		size = media.Original
	}
	raw, err := a.strList("ids")
	if err != nil {
		return rpc.Result{}, err
	}
	if len(raw) == 0 || len(raw) > maxMediaPerCall {
		return rpc.Result{}, fmt.Errorf("ids must list 1 to %d media ids", maxMediaPerCall)
	}
	ids := make([]string, len(raw))
	for i, r := range raw {
		if ids[i], err = media.ParseMediaID(r); err != nil {
			return rpc.Result{}, err
		}
	}
	return t.withStore(func(st *store.Store) (rpc.Result, error) {
		ref, err := resolve.SessionFromStore(st, session, resolve.Options{Activity: true, Jobs: t.Jobs, Loc: t.Env.Loc})
		if err != nil {
			return rpc.Result{}, err
		}
		if !t.policy.visible(ref.Username) {
			return rpc.Result{}, noSession(session)
		}
		// The cache is filed by month, so the lookback applies per whole month.
		minMonth := ""
		if f := t.policy.floor(); f.Set {
			minMonth = time.Unix(f.At, 0).In(t.Env.Loc).Format("2006-01")
		}
		cache, err := media.OpenCache(t.Env, ref.Username, minMonth)
		if err != nil {
			return rpc.Result{}, err
		}
		budget := t.policy.maxBytes()
		var blocks []rpc.Block
		var notes []string
		rows := 0
		for _, id := range ids {
			img, err := cache.Load(id, size)
			if err != nil {
				note := fmt.Sprintf("%s: %v", id, err)
				blocks, notes = append(blocks, rpc.Text(note)), append(notes, note)
				continue
			}
			data := base64.StdEncoding.EncodeToString(img.Data)
			if len(data) > budget {
				note := id + ": left out to fit the result size cap; request it in its own call or with size=preview"
				blocks, notes = append(blocks, rpc.Text(note)), append(notes, note)
				continue
			}
			budget -= len(data)
			blocks = append(blocks,
				rpc.Text(fmt.Sprintf("%s (%s, %s)", id, img.Variant, decode.FormatFileSize(uint64(len(img.Data))))),
				rpc.Image(data, img.MIME))
			rows++
		}
		// With no image at all the notes are all failures; isError lets
		// clients tell that from a result.
		if rows == 0 {
			return rpc.Result{}, errors.New(strings.Join(notes, "\n"))
		}
		return rpc.BlocksResult(rows, blocks...), nil
	})
}

type checkItem struct {
	Check  string  `json:"check"`
	Detail *string `json:"detail"`
	Status *string `json:"status"`
}

type diagnoseResult struct {
	Checks  []checkItem `json:"checks"`
	Session any         `json:"session,omitempty"`
}

type sessionResolved struct {
	Databases   int    `json:"databases"`
	ID          string `json:"id"`
	MessageRows int64  `json:"message_rows"`
	Name        string `json:"name"`
	Outcome     string `json:"outcome"`
}

type sessionFailed struct {
	Error   string `json:"error"`
	Outcome string `json:"outcome"`
}

type sessionOutcome struct {
	Outcome string `json:"outcome"`
}

func (t *tools) diagnose(a args) (rpc.Result, error) {
	if err := a.only("session"); err != nil {
		return rpc.Result{}, err
	}
	session, err := a.str("session")
	if err != nil {
		return rpc.Result{}, err
	}
	report := doctor.Run(t.Env, doctor.Options{Session: session, DBDir: t.DBDir, KeysPath: t.Keys, Jobs: t.Jobs})
	statusNames := map[render.CheckStatus]string{render.StatusOK: "ok", render.StatusMissing: "missing", render.StatusError: "error"}
	out := diagnoseResult{Checks: make([]checkItem, len(report.Checks))}
	for i, c := range report.Checks {
		item := checkItem{Check: c.Label}
		if name, ok := statusNames[c.Status]; ok {
			item.Status = &name
		}
		// Status details carry local paths and raw errors; facts are counts.
		if c.Detail != "" && !(t.Remote && item.Status != nil) {
			detail := c.Detail
			item.Detail = &detail
		}
		out.Checks[i] = item
	}
	if s := report.Session; s != nil {
		switch {
		case s.Kind == render.SessionResolved && t.policy.visible(s.Probe.Username):
			name := s.Probe.DisplayName
			if t.policy.nameMode() == contacts.Anonymous {
				name = s.Probe.PublicName
			}
			out.Session = sessionResolved{
				Databases: s.Probe.MatchingDBs, ID: s.Probe.Username, MessageRows: s.Probe.MessageCount,
				Name: name, Outcome: "resolved",
			}
		case s.Kind == render.SessionFailed && !t.Remote:
			out.Session = sessionFailed{Error: s.Error, Outcome: "failed"}
		case s.Kind == render.SessionUnavailable:
			out.Session = sessionOutcome{"unavailable"}
		default:
			out.Session = sessionOutcome{"not_found"}
		}
	}
	return jsonResult(out, len(report.Checks))
}
