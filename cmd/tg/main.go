// Command tg reads chat history from the client's local databases.
package main

import (
	"errors"
	"fmt"
	"os"
	"slices"
	"strings"

	"github.com/spf13/cobra"

	"github.com/xiaotianxt/tg/internal/env"
)

// retiredCommands are former subcommands. They stay out of the default
// `messages` rewrite so they fail as unknown subcommands instead of being
// read as session names. __complete was the dynamic completion hook; the
// completion scripts are static now.
var retiredCommands = []string{"decrypt", "refresh", "sql", "image", "file", "sticker", "emoji", "voice", "__complete"}

// rewriteArgs inserts `messages` when the first argument is neither a flag
// nor a known or retired subcommand, so `tg <chat>` reads that chat.
func rewriteArgs(args []string, known func(string) bool) []string {
	if len(args) < 2 {
		return args
	}
	first := args[1]
	if strings.HasPrefix(first, "-") || first == "help" || known(first) || slices.Contains(retiredCommands, first) {
		return args
	}
	return slices.Concat(args[:1], []string{"messages"}, args[1:])
}

// runError is a failure after arguments parsed (exit 1); anything else
// cobra returns is a usage error (exit 2), as clap reported them.
type runError struct{ err error }

func (e runError) Error() string { return e.err.Error() }
func (e runError) Unwrap() error { return e.err }

func main() {
	// Help lists commands in declaration order, as clap did.
	cobra.EnableCommandSorting = false
	e := env.FromProcess()
	root := newRoot(e)
	known := func(name string) bool {
		for _, c := range root.Commands() {
			if c.Name() == name {
				return true
			}
		}
		return false
	}
	args := rewriteArgs(os.Args, known)
	var err error
	if len(args) > 1 && slices.Contains(retiredCommands, args[1]) {
		// Checked here because cobra itself answers __complete.
		err = fmt.Errorf("unrecognized subcommand '%s'", args[1])
	} else {
		root.SetArgs(args[1:])
		_, err = root.ExecuteC()
	}
	if err == nil {
		return
	}
	fmt.Fprintf(os.Stderr, "error: %v\n", err)
	var re runError
	if errors.As(err, &re) {
		os.Exit(1)
	}
	fmt.Fprintln(os.Stderr, "\nFor more information, try '--help'.")
	os.Exit(2)
}
