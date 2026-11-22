// Package rpc handles MCP's JSON-RPC 2.0 messages, independent of transport
// and stateless: every request stands alone and initialize is not required
// before other methods.
package rpc

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"slices"
	"sort"

	"github.com/xiaotianxt/tg/internal/dict"
)

// SupportedVersions lists protocol versions, newest first. initialize echoes
// the client's version when supported and otherwise offers the newest.
var SupportedVersions = []string{"2026-07-28", "2025-11-25", "2025-06-18"}

// JSON-RPC error codes.
const (
	ParseError     = -32700
	InvalidRequest = -32600
	MethodNotFound = -32601
	InvalidParams  = -32602
)

// Rendered with the real app name before it is sent.
const instructions = "Read-only access to the owner's personal Telegram chat history (Telegram聊天记录), " +
	"private and group chats, read from their own device. " +
	"Use it whenever an answer depends on what the owner and other people actually said: past discussions, " +
	"what someone said or why they decided something, a conversation or relationship with a person, " +
	"what a group has been talking about, who is active, or message counts. " +
	"The user does not have to mention Telegram, chat history, or this tool. " +
	"Do not use it for general knowledge, public facts about people, or writing that needs no past conversation. " +
	"To answer: find a named person or group with list_sessions, then read the chat with read_messages; " +
	"use search_messages to find a topic across all chats, query_messages for exhaustive or per-sender filters, " +
	"list_members for a group's members and activity, and get_media to view images shown as [img:<id>]. " +
	"Base answers on the messages you read and cite them. " +
	"Local history can start late or have gaps; say so instead of concluding that nobody discussed something. " +
	"Results contain private message text; quote only what the user asked for."

// Kind classifies one incoming JSON-RPC message.
type Kind uint8

const (
	// Request has a string method and an id; it gets a response.
	Request Kind = iota
	// Notification has a string method and no id; it gets no reply.
	Notification
	// Response has no string method: an answer to something tg never sends,
	// ignored.
	Response
	// Invalid is unparseable or malformed; it gets an error response.
	Invalid
)

// Message is a classified JSON-RPC message.
type Message struct {
	Kind Kind
	// ID is the raw id for Request, and for Invalid when one was readable
	// (else nil, sent as null).
	ID     json.RawMessage
	Method string
	// Params is nil when absent.
	Params json.RawMessage
	// ErrCode and ErrMessage describe an Invalid message.
	ErrCode    int
	ErrMessage string
}

// Classify parses one message. JSON-RPC batches were removed from MCP in
// 2025-06-18 and are Invalid.
func Classify(data []byte) Message {
	var raw json.RawMessage
	if err := json.Unmarshal(data, &raw); err != nil {
		return Message{Kind: Invalid, ErrCode: ParseError, ErrMessage: "Parse error: " + err.Error()}
	}
	raw = bytes.TrimSpace(raw)
	var fields map[string]json.RawMessage
	if raw[0] != '{' || json.Unmarshal(raw, &fields) != nil {
		return Message{Kind: Invalid, ErrCode: InvalidRequest, ErrMessage: "Expected a single JSON-RPC object"}
	}
	id, hasID := fields["id"]
	var version string
	if json.Unmarshal(fields["jsonrpc"], &version) != nil || version != "2.0" {
		return Message{Kind: Invalid, ID: id, ErrCode: InvalidRequest, ErrMessage: `jsonrpc must be "2.0"`}
	}
	var method string
	if m, ok := fields["method"]; !ok || m[0] != '"' || json.Unmarshal(m, &method) != nil {
		return Message{Kind: Response}
	}
	if !hasID {
		return Message{Kind: Notification, Method: method}
	}
	return Message{Kind: Request, ID: id, Method: method, Params: fields["params"]}
}

// CallRecord is what a tool call did, for the audit log. It never holds
// argument values: search terms are private too.
type CallRecord struct {
	Tool string
	// ArgumentKeys are the argument names, sorted.
	ArgumentKeys []string
	Rows         int
	OK           bool
}

// Handled is the outcome of one message.
type Handled struct {
	// Response is nil for notifications and responses, which get no reply.
	Response []byte
	// Call is set for tools/call requests.
	Call *CallRecord
}

// Server answers MCP methods from a tool registry.
type Server struct {
	registry *Registry
	version  string
}

// NewServer serves registry; version is reported in serverInfo.
func NewServer(registry *Registry, version string) *Server {
	return &Server{registry: registry, version: version}
}

// Handle answers one raw message.
func (s *Server) Handle(ctx context.Context, data []byte) Handled {
	msg := Classify(data)
	switch msg.Kind {
	case Invalid:
		return Handled{Response: errorResponse(msg.ID, msg.ErrCode, msg.ErrMessage)}
	case Notification, Response:
		return Handled{}
	}
	switch msg.Method {
	case "initialize":
		return Handled{Response: success(msg.ID, s.initialize(msg.Params))}
	case "ping":
		return Handled{Response: success(msg.ID, map[string]any{})}
	case "tools/list":
		return Handled{Response: success(msg.ID, map[string]any{"tools": s.registry.definitions()})}
	case "tools/call":
		return s.callTool(ctx, msg.ID, msg.Params)
	}
	return Handled{Response: errorResponse(msg.ID, MethodNotFound, "Method not found: "+msg.Method)}
}

// NegotiateVersion returns requested when supported, else the newest.
func NegotiateVersion(requested string) string {
	if slices.Contains(SupportedVersions, requested) {
		return requested
	}
	return SupportedVersions[0]
}

func (s *Server) initialize(params json.RawMessage) map[string]any {
	var p struct {
		ProtocolVersion json.RawMessage `json:"protocolVersion"`
	}
	var requested string
	if json.Unmarshal(params, &p) == nil {
		_ = json.Unmarshal(p.ProtocolVersion, &requested) // non-strings negotiate the newest
	}
	return map[string]any{
		"protocolVersion": NegotiateVersion(requested),
		"capabilities":    map[string]any{"tools": map[string]any{"listChanged": false}},
		"serverInfo":      map[string]any{"name": "tg", "title": "tg chat history", "version": s.version},
		"instructions":    dict.RenderAppNames(instructions),
	}
}

func (s *Server) callTool(ctx context.Context, id, params json.RawMessage) Handled {
	var p struct {
		Name      *string         `json:"name"`
		Arguments json.RawMessage `json:"arguments"`
	}
	if json.Unmarshal(params, &p) != nil || p.Name == nil {
		return Handled{Response: errorResponse(id, InvalidParams, "tools/call needs a name")}
	}
	args := p.Arguments
	if bytes.Equal(args, []byte("null")) {
		args = nil
	}
	var keys []string
	var argErr error
	if args != nil {
		var object map[string]json.RawMessage
		if args[0] != '{' || json.Unmarshal(args, &object) != nil {
			argErr = errors.New("arguments must be an object")
		}
		for k := range object {
			keys = append(keys, k)
		}
		sort.Strings(keys)
	}
	record := &CallRecord{Tool: *p.Name, ArgumentKeys: keys}
	result, err := s.dispatch(ctx, *p.Name, args, argErr)
	var body map[string]any
	if err != nil {
		// Tool failures are results, not protocol errors, so the model can
		// read them and correct its arguments.
		body = map[string]any{"content": []Block{Text(err.Error())}, "isError": true}
	} else {
		body = map[string]any{"content": result.content, "isError": false}
		if result.structured != nil {
			body["structuredContent"] = result.structured
		}
		record.Rows, record.OK = result.Rows, true
	}
	return Handled{Response: success(id, body), Call: record}
}

func (s *Server) dispatch(ctx context.Context, name string, args json.RawMessage, argErr error) (Result, error) {
	if argErr != nil {
		return Result{}, argErr
	}
	tool, ok := s.registry.lookup(name)
	if !ok {
		return Result{}, fmt.Errorf("Unknown tool: %s", name)
	}
	return tool.Handler(ctx, args)
}

func success(id json.RawMessage, result any) []byte {
	return mustMarshal(map[string]any{"jsonrpc": "2.0", "id": idOrNull(id), "result": result})
}

func errorResponse(id json.RawMessage, code int, message string) []byte {
	return mustMarshal(map[string]any{
		"jsonrpc": "2.0",
		"id":      idOrNull(id),
		"error":   map[string]any{"code": code, "message": message},
	})
}

func idOrNull(id json.RawMessage) json.RawMessage {
	if id == nil {
		return json.RawMessage("null")
	}
	return id
}

// ServeStdio reads newline-delimited messages from in and writes each
// response as one line to out, until in ends. Blank lines are skipped.
// out carries protocol messages only; logs go to stderr.
func (s *Server) ServeStdio(ctx context.Context, in io.Reader, out io.Writer) error {
	r := bufio.NewReader(in)
	w := bufio.NewWriter(out)
	for {
		line, err := r.ReadBytes('\n')
		if len(bytes.TrimSpace(line)) > 0 {
			if h := s.Handle(ctx, line); h.Response != nil {
				if _, werr := w.Write(append(h.Response, '\n')); werr != nil {
					return fmt.Errorf("Write response: %w", werr)
				}
				if werr := w.Flush(); werr != nil {
					return fmt.Errorf("Write response: %w", werr)
				}
			}
		}
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return fmt.Errorf("Read stdin: %w", err)
		}
	}
}
