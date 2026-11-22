package rpc

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/xiaotianxt/tg/internal/dict"
)

func TestClassify(t *testing.T) {
	for _, tc := range []struct {
		in   string
		kind Kind
		code int
		id   string
	}{
		{`{"jsonrpc":"2.0","id":1,"method":"ping"}`, Request, 0, "1"},
		{` {"jsonrpc":"2.0","id":null,"method":"ping"}`, Request, 0, "null"},
		{`{"jsonrpc":"2.0","method":"notifications/initialized"}`, Notification, 0, ""},
		{`{"jsonrpc":"2.0","id":5,"result":{}}`, Response, 0, ""},
		{`{"jsonrpc":"2.0","id":5,"method":null}`, Response, 0, ""},
		{`{"jsonrpc":"2.0","id":5,"method":7}`, Response, 0, ""},
		{`{not json`, Invalid, ParseError, ""},
		{``, Invalid, ParseError, ""},
		{`[{"jsonrpc":"2.0","id":1,"method":"ping"}]`, Invalid, InvalidRequest, ""},
		{`"ping"`, Invalid, InvalidRequest, ""},
		{`{"jsonrpc":"1.0","id":"a","method":"ping"}`, Invalid, InvalidRequest, `"a"`},
		{`{"id":2,"method":"ping"}`, Invalid, InvalidRequest, "2"},
	} {
		m := Classify([]byte(tc.in))
		if m.Kind != tc.kind || m.ErrCode != tc.code || string(m.ID) != tc.id {
			t.Errorf("%s: got kind=%d code=%d id=%s", tc.in, m.Kind, m.ErrCode, m.ID)
		}
	}
}

func stubServer(t *testing.T) *Server {
	t.Helper()
	r, err := NewRegistry(
		Tool{Name: "echo", Title: "Echo", Description: "Echo <args>", InputSchema: json.RawMessage(`{"type":"object", "properties":{}}`),
			Annotations: json.RawMessage(`{"readOnlyHint":true}`),
			Handler: func(_ context.Context, args json.RawMessage) (Result, error) {
				if args == nil {
					return TextResult("no args", 0), nil
				}
				return JSONResult(args, 2), nil
			}},
		Tool{Name: "fail", InputSchema: json.RawMessage(`{}`),
			Handler: func(context.Context, json.RawMessage) (Result, error) {
				return Result{}, errors.New("Unknown argument: bogus")
			}},
		Tool{Name: "image", InputSchema: json.RawMessage(`{}`),
			Handler: func(context.Context, json.RawMessage) (Result, error) {
				return BlocksResult(1, Text("id (preview)"), Image("aGk=", "image/png")), nil
			}},
	)
	if err != nil {
		t.Fatal(err)
	}
	return NewServer(r, "3.0.0")
}

func respond(t *testing.T, s *Server, msg string) (map[string]any, *CallRecord) {
	t.Helper()
	h := s.Handle(context.Background(), []byte(msg))
	if h.Response == nil {
		return nil, h.Call
	}
	var out map[string]any
	if err := json.Unmarshal(h.Response, &out); err != nil {
		t.Fatalf("response %s: %v", h.Response, err)
	}
	return out, h.Call
}

func TestInitializeNegotiatesProtocolVersion(t *testing.T) {
	s := stubServer(t)
	for requested, want := range map[string]string{
		`"2025-06-18"`: "2025-06-18",
		`"2026-07-28"`: "2026-07-28",
		`"2025-03-26"`: "2026-07-28",
		`"1999-01-01"`: "2026-07-28",
		`5`:            "2026-07-28",
	} {
		out, _ := respond(t, s, `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":`+requested+`}}`)
		result := out["result"].(map[string]any)
		if result["protocolVersion"] != want {
			t.Errorf("%s: got %v", requested, result["protocolVersion"])
		}
		if result["serverInfo"].(map[string]any)["name"] != "tg" {
			t.Errorf("serverInfo = %v", result["serverInfo"])
		}
	}
	// Stateless: tools/list works without initialize; params may be absent.
	out, _ := respond(t, s, `{"jsonrpc":"2.0","id":2,"method":"initialize"}`)
	if out["result"].(map[string]any)["protocolVersion"] != SupportedVersions[0] {
		t.Errorf("no params: %v", out)
	}
}

func TestInstructionsNameTheRealAppAndNeverTheCoverName(t *testing.T) {
	out, _ := respond(t, stubServer(t), `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}`)
	text := out["result"].(map[string]any)["instructions"].(string)
	if !strings.Contains(text, dict.AppDisplayName) || !strings.Contains(text, dict.AppLocalizedName) {
		t.Error("instructions do not name the app")
	}
	if strings.Contains(strings.ToLower(text), "telegram") {
		t.Error("instructions leak the cover name")
	}
}

func TestNotificationsAndResponsesGetNoReply(t *testing.T) {
	s := stubServer(t)
	for _, msg := range []string{`{"jsonrpc":"2.0","method":"notifications/initialized"}`, `{"jsonrpc":"2.0","id":5,"result":{}}`} {
		if out, _ := respond(t, s, msg); out != nil {
			t.Errorf("%s got a reply: %v", msg, out)
		}
	}
}

func TestErrors(t *testing.T) {
	s := stubServer(t)
	for msg, code := range map[string]float64{
		`{bad`: ParseError,
		`[{"jsonrpc":"2.0","id":1,"method":"ping"}]`:                         InvalidRequest,
		`{"jsonrpc":"2.0","id":3,"method":"resources/list"}`:                 MethodNotFound,
		`{"jsonrpc":"2.0","id":4,"method":"tools/call"}`:                     InvalidParams,
		`{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":3}}`: InvalidParams,
	} {
		out, _ := respond(t, s, msg)
		if got := out["error"].(map[string]any)["code"]; got != code {
			t.Errorf("%s: code %v, want %v", msg, got, code)
		}
	}
	out, _ := respond(t, s, `{"jsonrpc":"2.0","id":3,"method":"resources/list"}`)
	if out["error"].(map[string]any)["message"] != "Method not found: resources/list" || out["id"] != float64(3) {
		t.Errorf("method not found: %v", out)
	}
}

func TestToolsListServesDefinitionsVerbatim(t *testing.T) {
	h := stubServer(t).Handle(context.Background(), []byte(`{"jsonrpc":"2.0","id":"a","method":"tools/list"}`))
	want := `{"id":"a","jsonrpc":"2.0","result":{"tools":[` +
		`{"annotations":{"readOnlyHint":true},"description":"Echo <args>","inputSchema":{"type":"object","properties":{}},"name":"echo","title":"Echo"},` +
		`{"description":"","inputSchema":{},"name":"fail"},{"description":"","inputSchema":{},"name":"image"}]}}`
	if string(h.Response) != want {
		t.Errorf("got  %s\nwant %s", h.Response, want)
	}
}

func TestToolCallResults(t *testing.T) {
	s := stubServer(t)
	h := s.Handle(context.Background(), []byte(`{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"z":"<b>","a":1}}}`))
	want := `{"id":1,"jsonrpc":"2.0","result":{"content":[{"text":"{\"z\":\"<b>\",\"a\":1}","type":"text"}],"isError":false,"structuredContent":{"z":"<b>","a":1}}}`
	if string(h.Response) != want {
		t.Errorf("got  %s\nwant %s", h.Response, want)
	}
	if c := h.Call; c == nil || c.Tool != "echo" || strings.Join(c.ArgumentKeys, ",") != "a,z" || c.Rows != 2 || !c.OK {
		t.Errorf("call = %+v", h.Call)
	}

	for _, args := range []string{``, `,"arguments":null`} {
		out, _ := respond(t, s, `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo"`+args+`}}`)
		if text := out["result"].(map[string]any)["content"].([]any)[0].(map[string]any)["text"]; text != "no args" {
			t.Errorf("args %q: %v", args, text)
		}
	}

	h = s.Handle(context.Background(), []byte(`{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"image"}}`))
	if !bytes.Contains(h.Response, []byte(`{"data":"aGk=","mimeType":"image/png","type":"image"}`)) || bytes.Contains(h.Response, []byte("structuredContent")) {
		t.Errorf("image: %s", h.Response)
	}
}

func TestToolFailuresAreErrorResults(t *testing.T) {
	s := stubServer(t)
	for msg, text := range map[string]string{
		`{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fail","arguments":{"bogus":1}}}`: "Unknown argument: bogus",
		`{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}`:                         "Unknown tool: nope",
		`{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope","arguments":[1]}}`:         "arguments must be an object",
	} {
		out, call := respond(t, s, msg)
		result := out["result"].(map[string]any)
		if result["isError"] != true || result["content"].([]any)[0].(map[string]any)["text"] != text {
			t.Errorf("%s: %v", msg, result)
		}
		if call == nil || call.OK || call.Rows != 0 {
			t.Errorf("%s: call = %+v", msg, call)
		}
	}
}

func TestRegistryRejectsDuplicates(t *testing.T) {
	h := func(context.Context, json.RawMessage) (Result, error) { return Result{}, nil }
	tool := Tool{Name: "a", InputSchema: json.RawMessage(`{}`), Handler: h}
	if _, err := NewRegistry(tool, tool); err == nil {
		t.Error("duplicate accepted")
	}
	if _, err := NewRegistry(Tool{Name: "b", Handler: h}); err == nil {
		t.Error("missing schema accepted")
	}
}

func TestServeStdio(t *testing.T) {
	in := strings.NewReader("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\r\n\n   \n" +
		`{"jsonrpc":"2.0","method":"notifications/initialized"}` + "\n" +
		`{"jsonrpc":"2.0","id":2,"method":"ping"}`)
	var out bytes.Buffer
	if err := stubServer(t).ServeStdio(context.Background(), in, &out); err != nil {
		t.Fatal(err)
	}
	want := `{"id":1,"jsonrpc":"2.0","result":{}}` + "\n" + `{"id":2,"jsonrpc":"2.0","result":{}}` + "\n"
	if out.String() != want {
		t.Errorf("got %q", out.String())
	}
}
