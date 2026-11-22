package main

import (
	"fmt"
	"io"
	"slices"
	"strings"

	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
)

// Completion scripts are static, generated from the command tree: they list
// subcommands, flags and fixed values without calling tg back, so they work
// unchanged after upgrades and need no hidden completion subcommand.

type compFlag struct {
	long, short, usage string
	takesValue         bool
	values             []string
	dirs, files        bool
}

type compNode struct {
	path   string // "tg", "tg mcp", "tg mcp stdio"
	subs   []*cobra.Command
	flags  []compFlag
	values []string // fixed positional values
}

func compNodes(root *cobra.Command) []compNode {
	var nodes []compNode
	var walk func(c *cobra.Command, path string)
	walk = func(c *cobra.Command, path string) {
		n := compNode{path: path, values: c.ValidArgs}
		for _, sub := range c.Commands() {
			if sub.IsAvailableCommand() {
				n.subs = append(n.subs, sub)
			}
		}
		c.InitDefaultHelpFlag()
		c.LocalFlags().VisitAll(func(f *pflag.Flag) {
			if f.Hidden {
				return
			}
			cf := compFlag{long: f.Name, short: f.Shorthand, usage: f.Usage, takesValue: f.Value.Type() != "bool"}
			switch v := f.Value.(type) {
			case *enumValue:
				cf.values = v.allowed
			case *enumListValue:
				cf.values = v.allowed
			}
			_, cf.dirs = f.Annotations[cobra.BashCompSubdirsInDir]
			_, cf.files = f.Annotations[cobra.BashCompFilenameExt]
			n.flags = append(n.flags, cf)
		})
		nodes = append(nodes, n)
		for _, sub := range n.subs {
			walk(sub, path+" "+sub.Name())
		}
	}
	walk(root, root.Name())
	return nodes
}

// subPaths lists every "parent child" step, for the scripts' word walk.
func subPaths(nodes []compNode) []string {
	var out []string
	for _, n := range nodes {
		for _, s := range n.subs {
			out = append(out, n.path+" "+s.Name())
		}
	}
	return out
}

func (n compNode) words() []string {
	var w []string
	for _, s := range n.subs {
		w = append(w, s.Name())
	}
	w = append(w, n.values...)
	for _, f := range n.flags {
		w = append(w, "--"+f.long)
		if f.short != "" {
			w = append(w, "-"+f.short)
		}
	}
	return w
}

func flagNames(f compFlag) []string {
	names := []string{"--" + f.long}
	if f.short != "" {
		names = append(names, "-"+f.short)
	}
	return names
}

func writeBash(w io.Writer, root *cobra.Command) error {
	nodes := compNodes(root)
	var b strings.Builder
	b.WriteString("# bash completion for tg\n_tg() {\n")
	b.WriteString("    local cur prev cmdpath i\n")
	b.WriteString("    cur=\"${COMP_WORDS[COMP_CWORD]}\"\n    prev=\"${COMP_WORDS[COMP_CWORD-1]}\"\n    cmdpath=tg\n")
	b.WriteString("    for ((i = 1; i < COMP_CWORD; i++)); do\n        case \"$cmdpath ${COMP_WORDS[i]}\" in\n")
	fmt.Fprintf(&b, "            %s) cmdpath=\"$cmdpath ${COMP_WORDS[i]}\" ;;\n", shQuoteList(subPaths(nodes), "|"))
	b.WriteString("        esac\n    done\n    case \"$cmdpath\" in\n")
	for _, n := range nodes {
		fmt.Fprintf(&b, "        %s)\n            case \"$prev\" in\n", shQuote(n.path))
		for _, f := range n.flags {
			if !f.takesValue {
				continue
			}
			fmt.Fprintf(&b, "                %s) ", strings.Join(flagNames(f), "|"))
			switch {
			case len(f.values) > 0:
				fmt.Fprintf(&b, "COMPREPLY=($(compgen -W %s -- \"$cur\")); return ;;\n", shQuote(strings.Join(f.values, " ")))
			case f.dirs:
				b.WriteString("COMPREPLY=($(compgen -d -- \"$cur\")); return ;;\n")
			case f.files:
				b.WriteString("COMPREPLY=($(compgen -f -- \"$cur\")); return ;;\n")
			default:
				b.WriteString("return ;;\n")
			}
		}
		b.WriteString("            esac\n")
		fmt.Fprintf(&b, "            COMPREPLY=($(compgen -W %s -- \"$cur\")) ;;\n", shQuote(strings.Join(n.words(), " ")))
	}
	b.WriteString("    esac\n}\ncomplete -F _tg tg\n")
	_, err := io.WriteString(w, b.String())
	return err
}

func writeZsh(w io.Writer, root *cobra.Command) error {
	nodes := compNodes(root)
	var b strings.Builder
	b.WriteString("#compdef tg\n\n_tg() {\n    local cmdpath=tg prev i\n    local -a opts\n")
	b.WriteString("    for ((i = 2; i < CURRENT; i++)); do\n        case \"$cmdpath ${words[i]}\" in\n")
	fmt.Fprintf(&b, "            (%s) cmdpath=\"$cmdpath ${words[i]}\" ;;\n", shQuoteList(subPaths(nodes), "|"))
	b.WriteString("        esac\n    done\n    prev=${words[CURRENT-1]}\n    case \"$cmdpath\" in\n")
	for _, n := range nodes {
		fmt.Fprintf(&b, "        (%s)\n            case \"$prev\" in\n", shQuote(n.path))
		for _, f := range n.flags {
			if !f.takesValue {
				continue
			}
			fmt.Fprintf(&b, "                (%s) ", strings.Join(flagNames(f), "|"))
			switch {
			case len(f.values) > 0:
				fmt.Fprintf(&b, "compadd -- %s; return ;;\n", strings.Join(shQuoteAll(f.values), " "))
			case f.dirs:
				b.WriteString("_files -/; return ;;\n")
			case f.files:
				b.WriteString("_files; return ;;\n")
			default:
				b.WriteString("return ;;\n")
			}
		}
		b.WriteString("            esac\n")
		fmt.Fprintf(&b, "            opts=(%s) ;;\n", strings.Join(shQuoteAll(n.words()), " "))
	}
	b.WriteString("    esac\n    compadd -- $opts\n}\n\n")
	b.WriteString("if [ \"$funcstack[1]\" = \"_tg\" ]; then\n    _tg \"$@\"\nelse\n    compdef _tg tg\nfi\n")
	_, err := io.WriteString(w, b.String())
	return err
}

func writeFish(w io.Writer, root *cobra.Command) error {
	nodes := compNodes(root)
	var b strings.Builder
	b.WriteString("# fish completion for tg\nfunction __tg_path\n    set -l cmdpath tg\n")
	b.WriteString("    for w in (commandline -opc)[2..-1]\n        switch \"$cmdpath $w\"\n")
	fmt.Fprintf(&b, "            case %s\n                set cmdpath \"$cmdpath $w\"\n", strings.Join(shQuoteAll(subPaths(nodes)), " "))
	b.WriteString("        end\n    end\n    test \"$cmdpath\" = \"$argv[1]\"\nend\n\ncomplete -c tg -f\n")
	for _, n := range nodes {
		cond := "-n " + shQuote("__tg_path "+shQuote(n.path))
		for _, s := range n.subs {
			fmt.Fprintf(&b, "complete -c tg %s -a %s -d %s\n", cond, shQuote(s.Name()), shQuote(s.Short))
		}
		for _, v := range n.values {
			fmt.Fprintf(&b, "complete -c tg %s -a %s\n", cond, shQuote(v))
		}
		for _, f := range n.flags {
			fmt.Fprintf(&b, "complete -c tg %s -l %s", cond, f.long)
			if f.short != "" {
				fmt.Fprintf(&b, " -s %s", f.short)
			}
			switch {
			case len(f.values) > 0:
				fmt.Fprintf(&b, " -x -a %s", shQuote(strings.Join(f.values, " ")))
			case f.dirs:
				b.WriteString(" -x -a '(__fish_complete_directories)'")
			case f.files:
				b.WriteString(" -r -F")
			case f.takesValue:
				b.WriteString(" -x")
			}
			fmt.Fprintf(&b, " -d %s\n", shQuote(f.usage))
		}
	}
	_, err := io.WriteString(w, b.String())
	return err
}

// shQuote single-quotes s for bash, zsh and fish ('\” closes, escapes and
// reopens, which all three read the same way).
func shQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}

func shQuoteAll(items []string) []string {
	out := slices.Clone(items)
	for i, s := range out {
		out[i] = shQuote(s)
	}
	return out
}

func shQuoteList(items []string, sep string) string { return strings.Join(shQuoteAll(items), sep) }
