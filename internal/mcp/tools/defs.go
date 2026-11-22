package tools

import (
	"encoding/json"
	"fmt"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/mcp/rpc"
)

// maxMediaPerCall caps get_media; each image costs the model about as much
// as a page of text.
const maxMediaPerCall = 10

const senderHint = "Only messages from one sender: `me` for the account owner, an account id " +
	"(the ids that list_members and the message tools show), or a name exactly as those tools show it. " +
	"A name several accounts share is rejected with their ids."

const timeHint = "ISO 8601 date/time or relative (30min, 2h, 7d, today, yesterday)"

const offsetHint = "Only to continue a truncated result: pass its next_offset."

// readOnly marks every tool: clients that respect readOnlyHint run them
// without a confirmation.
var readOnly = json.RawMessage(`{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}`)

type definition struct {
	name, title, description string
	properties               map[string]any
	required                 []string
}

func str(description string) map[string]any {
	if description == "" {
		return map[string]any{"type": "string"}
	}
	return map[string]any{"type": "string", "description": description}
}

func offsetProp(description string) map[string]any {
	return map[string]any{"type": "integer", "minimum": 0, "default": 0, "description": description}
}

func enum(def string, values ...string) map[string]any {
	return map[string]any{"type": "string", "enum": values, "default": def}
}

func strArray(maxItems int) map[string]any {
	return map[string]any{"type": "array", "items": map[string]any{"type": "string"}, "maxItems": maxItems}
}

// definitions are the eight tools in tools/list order. Descriptions name the
// app by its real name, so they go through dict.RenderAppNames.
var definitions = []definition{
	{
		name: "list_sessions", title: "List chats",
		description: dict.RenderAppNames("Use this first when the user names a person, group, or chat, or asks about a conversation or relationship with someone, in their Telegram history: it turns the name into the chat `id` that the other tools take as `session`. Lists every direct and group chat, most recently active first. Returns each chat's stable `id`, which other tools accept as `session`. Only if the list exceeds the result size cap is it trimmed, with `truncated: true` and `next_offset`."),
		properties: map[string]any{
			"query":  str("Optional name filter (fuzzy match on names and ids)"),
			"offset": offsetProp(offsetHint),
		},
	},
	{
		name: "list_unread", title: "List unread chats",
		description: "Use this when the user asks what is new, unread, or waiting for a reply. Lists every chat that has unread messages, with unread counts. Only if the list exceeds the result size cap is it trimmed, with `truncated: true` and `next_offset`.",
		properties: map[string]any{
			"query":  str("Optional name filter"),
			"offset": offsetProp(offsetHint),
		},
	},
	{
		name: "read_messages", title: "Read a chat",
		description: "Use this to see what was actually said in one chat: summarize what a group has been discussing, recall a conversation with someone, or read the context around a search hit. Reads every message of one chat in an optional [since, until) time range. Returns a plain-text transcript in chronological order: a `== YYYY-MM-DD ==` line per local day, then one `HH:MM sender: text` line per message, with indented continuation lines. A senders list after the header maps each shown name to its stable account id; the account owner always appears as 我. The header also says when the chat's readable history starts: nothing earlier is available, so an empty early range means missing history, not silence. Non-text messages appear as bracketed placeholders; images show as [img:<id>], which get_media can display. There is no page size: one call returns the whole range. Only if the range exceeds the result size cap is the transcript trimmed and ends with a `[truncated]` line giving the offset to repeat the same call with. order=newest (default) keeps the latest messages when trimming, order=oldest the earliest.",
		properties: map[string]any{
			"session":  str("Chat id from list_sessions (preferred) or a display name"),
			"order":    enum("newest", "newest", "oldest"),
			"offset":   offsetProp(offsetHint + " Counted from the end of the range for order=newest and from the start for order=oldest."),
			"since":    str("Inclusive start; " + timeHint),
			"until":    str("Exclusive end; " + timeHint),
			"contains": str("Only messages whose text contains this substring"),
			"sender":   str(senderHint),
		},
		required: []string{"session"},
	},
	{
		name: "search_messages", title: "Search messages",
		description: dict.RenderAppNames("Use this when answering needs what people previously said or discussed about a topic, across private and group Telegram chats, including indirect questions that never mention Telegram or chat history (why someone decided something, who talked about a subject, what was said about a plan). Fast keyword search on the client's own index, over the full history unless `since` is given; it can miss some card and notice text, so use query_messages when completeness matters. Returns every match, newest first. Only if the matches exceed the result size cap is the result trimmed, with `truncated: true` and `next_offset`; then repeat the same call with offset=next_offset."),
		properties: map[string]any{
			"query":  map[string]any{"type": "string", "minLength": 1},
			"since":  str(timeHint),
			"offset": offsetProp(offsetHint),
		},
		required: []string{"query"},
	},
	{
		name: "query_messages", title: "Filter messages",
		description: "Use this for exhaustive or precise lookups: everything one person said (`sender`), messages with a media type, keyword combinations with exclusions, or counting messages in a period. Structured message filter across all chats or one chat: keywords to include/exclude, media type, sender, and a time range. Each message carries `sender` (the account owner is 我) and its stable `sender_id`. Slower than search_messages but exhaustive. Covers the full history unless `since` is given; with no session, give at least one filter or `since`. Returns every match. Only if the matches exceed the result size cap is the result trimmed, with `truncated: true` and `next_offset`; then repeat the same call with offset=next_offset. To read every message of one chat in a period, use read_messages instead.",
		properties: map[string]any{
			"session":      str("Optional chat id or display name"),
			"contains":     strArray(16),
			"not_contains": strArray(16),
			"match":        enum("all", "all", "any"),
			"has": map[string]any{"type": "array", "items": map[string]any{
				"type": "string", "enum": []string{"voice", "image", "sticker", "file", "video"},
			}},
			"sender": str(senderHint),
			"since":  str(timeHint),
			"until":  str(timeHint),
			"order":  enum("newest", "newest", "oldest"),
			"offset": offsetProp(offsetHint),
		},
	},
	{
		name: "list_members", title: "List group members",
		description: "Use this for questions about who is in a group or how active its members are: who speaks most, who has gone quiet, or which members are your contacts. Lists a group's current members with each one's id, message count, and last message time, or only members who are your local contacts (friends_only).",
		properties: map[string]any{
			"session":        str("Group id or display name"),
			"inactive_since": str("Only members with no recorded messages since this time; " + timeHint),
			"friends_only":   map[string]any{"type": "boolean", "default": false},
		},
		required: []string{"session"},
	},
	{
		name: "get_media", title: "View cached images",
		description: fmt.Sprintf("Use this when an image in a transcript matters to the answer. Views images from one chat. Pass the 32-character ids that read_messages and the other message tools show as [img:<id>] (images) or as the name in [视频 WxH <id>] (video cover). Returns each image the local client has cached, up to %d per call; an id that was never downloaded reports so instead. size=preview (default) returns the client's standard copy, size=original the full-resolution copy when one was downloaded. Each image costs about as much context as a page of text, so fetch only the ones the task needs.", maxMediaPerCall),
		properties: map[string]any{
			"session": str("Chat id or display name the images were sent in"),
			"ids":     map[string]any{"type": "array", "items": map[string]any{"type": "string"}, "minItems": 1, "maxItems": maxMediaPerCall},
			"size":    enum("preview", "preview", "original"),
		},
		required: []string{"session", "ids"},
	},
	{
		name: "diagnose", title: "Diagnose tg",
		description: "Check whether tg can read the local history, optionally for one chat. Use when another tool fails.",
		properties: map[string]any{
			"session": str(""),
		},
	},
}

func (d definition) schema() json.RawMessage {
	s := map[string]any{"type": "object", "properties": d.properties, "additionalProperties": false}
	if d.required != nil {
		s["required"] = d.required
	}
	b, err := rpc.Marshal(s)
	if err != nil {
		panic(fmt.Sprintf("tools: encode %s schema: %v", d.name, err))
	}
	return b
}
