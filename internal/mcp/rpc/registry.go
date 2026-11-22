package rpc

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
)

// Tool is one MCP tool. The tools package owns names, descriptions and
// schemas; rpc serves them verbatim.
type Tool struct {
	Name        string
	Title       string
	Description string
	// InputSchema is served as-is in tools/list.
	InputSchema json.RawMessage
	// Annotations is served as-is; nil omits the field.
	Annotations json.RawMessage
	// Handler runs a call. args is a JSON object, or nil when the client sent
	// no arguments or null; rpc rejects any other shape before calling it.
	// A returned error becomes an isError result carrying err.Error(), so
	// the model can read it and correct its arguments.
	Handler func(ctx context.Context, args json.RawMessage) (Result, error)
}

// tools/list wire shape; field order follows the 2.x output (sorted keys).
type toolDefinition struct {
	Annotations json.RawMessage `json:"annotations,omitempty"`
	Description string          `json:"description"`
	InputSchema json.RawMessage `json:"inputSchema"`
	Name        string          `json:"name"`
	Title       string          `json:"title,omitempty"`
}

// Registry is an ordered, immutable set of tools.
type Registry struct {
	tools  []Tool
	byName map[string]int
}

// NewRegistry indexes tools in list order. Duplicate or empty names and
// missing handlers or schemas are programming errors.
func NewRegistry(tools ...Tool) (*Registry, error) {
	r := &Registry{tools: tools, byName: make(map[string]int, len(tools))}
	for i, t := range tools {
		switch {
		case t.Name == "":
			return nil, fmt.Errorf("tool %d has no name", i)
		case t.Handler == nil:
			return nil, fmt.Errorf("tool %s has no handler", t.Name)
		case !json.Valid(t.InputSchema):
			return nil, fmt.Errorf("tool %s has an invalid input schema", t.Name)
		case t.Annotations != nil && !json.Valid(t.Annotations):
			return nil, fmt.Errorf("tool %s has invalid annotations", t.Name)
		}
		if _, dup := r.byName[t.Name]; dup {
			return nil, fmt.Errorf("tool %s is registered twice", t.Name)
		}
		r.byName[t.Name] = i
	}
	return r, nil
}

func (r *Registry) lookup(name string) (Tool, bool) {
	i, ok := r.byName[name]
	if !ok {
		return Tool{}, false
	}
	return r.tools[i], true
}

func (r *Registry) definitions() []toolDefinition {
	defs := make([]toolDefinition, len(r.tools))
	for i, t := range r.tools {
		defs[i] = toolDefinition{Annotations: t.Annotations, Description: t.Description, InputSchema: t.InputSchema, Name: t.Name, Title: t.Title}
	}
	return defs
}

// Result is a successful tool result. Build it with JSONResult, TextResult
// or BlocksResult.
type Result struct {
	content    []Block
	structured json.RawMessage
	// Rows is the item count recorded in the audit log.
	Rows int
}

// JSONResult sends value as structuredContent plus the same JSON as text.
// value must be valid JSON; build it with Marshal to keep text unescaped.
func JSONResult(value json.RawMessage, rows int) Result {
	var buf bytes.Buffer
	if err := json.Compact(&buf, value); err != nil {
		panic(fmt.Sprintf("rpc: JSONResult needs valid JSON: %v", err))
	}
	return Result{content: []Block{Text(buf.String())}, structured: buf.Bytes(), Rows: rows}
}

// TextResult sends text only, for results where JSON field names would
// outweigh the data.
func TextResult(text string, rows int) Result {
	return Result{content: []Block{Text(text)}, Rows: rows}
}

// BlocksResult sends raw content blocks, for results that carry images.
func BlocksResult(rows int, blocks ...Block) Result {
	return Result{content: blocks, Rows: rows}
}

type blockKind uint8

const (
	textBlock blockKind = iota
	imageBlock
)

// Block is one MCP content block.
type Block struct {
	kind     blockKind
	text     string
	data     string
	mimeType string
}

// Text is a text content block.
func Text(s string) Block { return Block{kind: textBlock, text: s} }

// Image is an image content block; data is base64.
func Image(data, mimeType string) Block {
	return Block{kind: imageBlock, data: data, mimeType: mimeType}
}

func (b Block) MarshalJSON() ([]byte, error) {
	switch b.kind {
	case textBlock:
		return Marshal(map[string]string{"type": "text", "text": b.text})
	case imageBlock:
		return Marshal(map[string]string{"type": "image", "data": b.data, "mimeType": b.mimeType})
	}
	return nil, fmt.Errorf("unknown block kind %d", b.kind)
}

// Marshal encodes v as compact JSON without HTML escaping, so message text
// such as "<img>" stays readable to the model.
func Marshal(v any) (json.RawMessage, error) {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buf.Bytes(), []byte("\n")), nil
}

func mustMarshal(v any) []byte {
	b, err := Marshal(v)
	if err != nil {
		panic(fmt.Sprintf("rpc: encode %T: %v", v, err))
	}
	return b
}
