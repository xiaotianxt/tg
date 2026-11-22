package tools

import (
	"bytes"
	"encoding/json"
	"fmt"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// args are a tool's arguments, validated at the protocol boundary. Missing
// and null values read as absent; blank strings too.
type args map[string]json.RawMessage

// parseArgs decodes a call's arguments. rpc already rejected anything but an
// object or nil.
func parseArgs(raw json.RawMessage) (args, error) {
	a := args{}
	if raw == nil {
		return a, nil
	}
	if err := json.Unmarshal(raw, &a); err != nil {
		return nil, fmt.Errorf("arguments must be an object")
	}
	for k, v := range a {
		if bytes.Equal(v, []byte("null")) {
			delete(a, k)
		}
	}
	return a, nil
}

// only rejects the first unknown argument in sorted order.
func (a args) only(allowed ...string) error {
	keys := make([]string, 0, len(a))
	for k := range a {
		keys = append(keys, k)
	}
	slices.Sort(keys)
	for _, k := range keys {
		if !slices.Contains(allowed, k) {
			return fmt.Errorf("Unknown argument: %s", k)
		}
	}
	return nil
}

// str returns a string argument, "" when absent or blank. The value is
// returned untrimmed.
func (a args) str(name string) (string, error) {
	v, ok := a[name]
	if !ok {
		return "", nil
	}
	var s string
	if json.Unmarshal(v, &s) != nil {
		return "", fmt.Errorf("%s must be a string", name)
	}
	if strings.TrimSpace(s) == "" {
		return "", nil
	}
	return s, nil
}

func (a args) requiredStr(name string) (string, error) {
	s, err := a.str(name)
	if err == nil && s == "" {
		err = fmt.Errorf("%s is required", name)
	}
	return s, err
}

func (a args) strList(name string) ([]string, error) {
	v, ok := a[name]
	if !ok {
		return nil, nil
	}
	var items []json.RawMessage
	if json.Unmarshal(v, &items) != nil {
		return nil, fmt.Errorf("%s must be a list of strings", name)
	}
	out := make([]string, len(items))
	for i, item := range items {
		// Unmarshal accepts null into a string; a list item must be a string.
		if item[0] != '"' || json.Unmarshal(item, &out[i]) != nil {
			return nil, fmt.Errorf("%s must be a list of strings", name)
		}
	}
	return out, nil
}

// offset is the continuation offset of a truncated result; 0 when absent.
func (a args) offset() (int, error) {
	v, ok := a["offset"]
	if !ok {
		return 0, nil
	}
	n, err := strconv.ParseUint(string(v), 10, 63)
	if err != nil {
		return 0, fmt.Errorf("offset must be a non-negative integer")
	}
	return int(n), nil
}

func (a args) bool(name string) (bool, error) {
	v, ok := a[name]
	if !ok {
		return false, nil
	}
	switch string(v) {
	case "true":
		return true, nil
	case "false":
		return false, nil
	}
	return false, fmt.Errorf("%s must be a boolean", name)
}

// choice returns the argument when it is one of allowed, else def when
// absent.
func (a args) choice(name string, def string, allowed ...string) (string, error) {
	s, err := a.str(name)
	switch {
	case err != nil:
		return "", err
	case s == "":
		return def, nil
	case slices.Contains(allowed, s):
		return s, nil
	}
	return "", fmt.Errorf("%s must be one of: %s", name, strings.Join(allowed, ", "))
}

func (a args) order() (msgdb.Order, error) {
	s, err := a.choice("order", "newest", "newest", "oldest")
	if s == "oldest" {
		return msgdb.Oldest, err
	}
	return msgdb.Newest, err
}

func (a args) time(name string, now time.Time, loc *time.Location) (msgdb.Bound, error) {
	s, err := a.str(name)
	if err != nil || s == "" {
		return msgdb.Bound{}, err
	}
	ts, err := tgtime.Parse(s, now, loc)
	if err != nil {
		return msgdb.Bound{}, fmt.Errorf("Invalid %s: %w", name, err)
	}
	return msgdb.At(ts), nil
}
