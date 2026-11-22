package main

import (
	"fmt"
	"net"
	"slices"
	"strconv"
	"strings"

	"github.com/spf13/cobra"
)

// enumValue is a flag restricted to fixed values, checked at parse time so
// a bad value is a usage error. Aliases map extra spellings to a value.
type enumValue struct {
	value   string
	allowed []string
	aliases map[string]string
}

func newEnum(def string, allowed ...string) *enumValue {
	return &enumValue{value: def, allowed: allowed}
}

func (e *enumValue) String() string { return e.value }
func (e *enumValue) Type() string   { return "string" }

func (e *enumValue) Set(s string) error {
	if canon, ok := e.aliases[s]; ok {
		s = canon
	}
	if !slices.Contains(e.allowed, s) {
		return fmt.Errorf("invalid value '%s' [possible values: %s]", s, strings.Join(e.allowed, ", "))
	}
	e.value = s
	return nil
}

// enumListValue is a repeatable enumValue.
type enumListValue struct {
	values  []string
	allowed []string
}

func (e *enumListValue) String() string { return strings.Join(e.values, ",") }
func (e *enumListValue) Type() string   { return "stringArray" }

func (e *enumListValue) Set(s string) error {
	if !slices.Contains(e.allowed, s) {
		return fmt.Errorf("invalid value '%s' [possible values: %s]", s, strings.Join(e.allowed, ", "))
	}
	e.values = append(e.values, s)
	return nil
}

// addrValue is a socket address (host:port with an IP host) or a bare IP.
type addrValue struct {
	value  string
	withIP bool // false: bare IP; true: IP:port
}

func (a *addrValue) String() string { return a.value }
func (a *addrValue) Type() string   { return "string" }

func (a *addrValue) Set(s string) error {
	if a.withIP {
		host, port, err := net.SplitHostPort(s)
		if err != nil || net.ParseIP(host) == nil {
			return fmt.Errorf("invalid socket address syntax")
		}
		if _, err := strconv.ParseUint(port, 10, 16); err != nil {
			return fmt.Errorf("invalid socket address syntax")
		}
	} else if net.ParseIP(s) == nil {
		return fmt.Errorf("invalid IP address syntax")
	}
	a.value = s
	return nil
}

// conflicts rejects two flags given together; returned from PreRunE it is
// a usage error (exit 2).
func conflicts(cmd *cobra.Command, a, b string) error {
	if cmd.Flags().Changed(a) && cmd.Flags().Changed(b) {
		return fmt.Errorf("the argument '--%s' cannot be used with '--%s'", a, b)
	}
	return nil
}

func valueArgs(name string, allowed ...string) cobra.PositionalArgs {
	return func(cmd *cobra.Command, args []string) error {
		if len(args) != 1 {
			return fmt.Errorf("expected exactly one <%s>", strings.ToUpper(name))
		}
		if !slices.Contains(allowed, args[0]) {
			return fmt.Errorf("invalid value '%s' for '<%s>' [possible values: %s]", args[0], strings.ToUpper(name), strings.Join(allowed, ", "))
		}
		return nil
	}
}
