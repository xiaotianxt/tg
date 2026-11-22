package main

import (
	"errors"
	"fmt"
	"os"

	"github.com/spf13/cobra"

	"github.com/xiaotianxt/tg/internal/capture"
	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/history"
	"github.com/xiaotianxt/tg/internal/mcp"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/render"
	"github.com/xiaotianxt/tg/internal/resolve"
	"github.com/xiaotianxt/tg/internal/store"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// sourceFlags are the flags every data command takes.
type sourceFlags struct {
	dbDir string
	keys  string
	jobs  uint
}

func addSource(cmd *cobra.Command, e env.Env) *sourceFlags {
	s := &sourceFlags{}
	f := cmd.Flags()
	f.StringVar(&s.dbDir, "db-dir", "", "Path to encrypted source databases (auto-detected if omitted)")
	f.StringVar(&s.keys, "keys", e.KeysPath(), "Path to all_keys.json")
	f.UintVar(&s.jobs, "jobs", 0, "Number of parallel jobs (0 = auto)")
	_ = cmd.MarkFlagDirname("db-dir")
	_ = cmd.MarkFlagFilename("keys")
	return s
}

func (s *sourceFlags) open() (*store.Store, error) { return store.Discover(s.dbDir, s.keys) }

// run wraps a command body so its errors exit 1, not 2.
func run(fn func(cmd *cobra.Command, args []string) error) func(*cobra.Command, []string) error {
	return func(cmd *cobra.Command, args []string) error {
		if err := fn(cmd, args); err != nil {
			return runError{err}
		}
		return nil
	}
}

func nameMode(anonymous bool) contacts.NameMode {
	if anonymous {
		return contacts.Anonymous
	}
	return contacts.PersonalRemark
}

// parseTime parses an optional time flag value.
func parseTime(e env.Env, flag, value string) (msgdb.Bound, error) {
	if value == "" {
		return msgdb.Bound{}, nil
	}
	ts, err := tgtime.Parse(value, e.Now(), e.Loc)
	if err != nil {
		return msgdb.Bound{}, fmt.Errorf("invalid %s: %w", flag, err)
	}
	return msgdb.At(ts), nil
}

// messagesLimit is --limit when given, else 50 unless --since or --all-time
// asks for the whole range.
func messagesLimit(limit uint, limitSet, sinceSet, allTime bool) (int, bool) {
	if limitSet {
		return int(limit), true
	}
	if sinceSet || allTime {
		return 0, false
	}
	return 50, true
}

func newRoot(e env.Env) *cobra.Command {
	root := &cobra.Command{
		Use:           "tg",
		Short:         dict.RenderAppNames("Read Telegram messages from local databases"),
		Version:       version,
		SilenceErrors: true,
		SilenceUsage:  true,
	}
	root.SetVersionTemplate("tg {{.Version}}\n")
	root.SetUsageTemplate(usageTemplate)
	// No subcommand is a usage error, as clap reported it.
	root.RunE = func(*cobra.Command, []string) error { return errors.New("'tg' requires a subcommand") }
	root.Flags().BoolP("version", "V", false, "Print version")
	root.CompletionOptions.DisableDefaultCmd = true
	root.SetFlagErrorFunc(func(_ *cobra.Command, err error) error { return err })
	root.AddCommand(
		keysCmd(e), sessionsCmd(e, history.AllSessions), sessionsCmd(e, history.UnreadSessions),
		membersCmd(e), doctorCmd(e), messagesCmd(e), searchCmd(e), queryCmd(e), mediaCmd(e),
		exportCmd(e), mcpCmd(e), skillCmd(e), completionsCmd(root),
	)
	return root
}

func keysCmd(e env.Env) *cobra.Command {
	var timeout, jobs uint
	var dbDir, output string
	method := newEnum("auto", "auto", "login")
	method.aliases = map[string]string{"lldb-login": "login", "lldb-cold": "login", "gdb-login": "login"}
	cmd := &cobra.Command{
		Use:   "keys",
		Short: "Refresh DB encryption keys from cached or captured account material",
		Args:  cobra.NoArgs,
		RunE: run(func(cmd *cobra.Command, _ []string) error {
			m, err := capture.ParseMethod(method.value)
			if err != nil {
				return err
			}
			if !cmd.Flags().Changed("timeout") {
				timeout = uint(capture.DefaultTimeout(m))
			}
			res, err := capture.Run(cmd.Context(), e, capture.Options{
				Method: m, Timeout: int(timeout), Jobs: int(jobs), DBDir: dbDir, Output: output,
			})
			if err != nil {
				return err
			}
			_, err = fmt.Fprintf(os.Stdout, "Keys saved to: %s\n", res.KeysPath)
			return err
		}),
	}
	f := cmd.Flags()
	f.UintVar(&timeout, "timeout", 30, "Timeout in seconds (180 with --method login)")
	f.Var(method, "method", "Key extraction method [possible values: auto, login]")
	f.UintVar(&jobs, "jobs", 0, "Number of parallel key-derivation jobs (0 = auto)")
	f.StringVar(&dbDir, "db-dir", "", "Path to encrypted source databases (auto-detected if omitted)")
	f.StringVar(&output, "output", e.KeysPath(), "Output path for database keys")
	_ = cmd.RegisterFlagCompletionFunc("method", cobra.FixedCompletions([]string{"auto", "login"}, cobra.ShellCompDirectiveNoFileComp))
	return cmd
}

func sessionsCmd(e env.Env, kind history.SessionListKind) *cobra.Command {
	var top uint
	use, short, queryHelp, topHelp := "sessions", "List all chat sessions/conversations",
		"Optional display name or username query to filter sessions", "Number of top sessions to show"
	if kind == history.UnreadSessions {
		use, short, queryHelp, topHelp = "unread", "List sessions with unread messages",
			"Optional display name or username query to filter unread sessions", "Number of top unread sessions to show"
	}
	cmd := &cobra.Command{
		Use:   use + " [QUERY]",
		Short: short,
		Long:  short + "\n\nQUERY: " + queryHelp,
		Args:  cobra.MaximumNArgs(1),
	}
	src := addSource(cmd, e)
	cmd.Flags().UintVar(&top, "top", 30, topHelp)
	cmd.RunE = run(func(_ *cobra.Command, args []string) error {
		query := ""
		if len(args) == 1 {
			query = args[0]
		}
		st, err := src.open()
		if err != nil {
			return err
		}
		defer st.Close()
		list, err := history.ListSessions(st, kind, int(top), query)
		if err != nil {
			return err
		}
		return render.Sessions(os.Stdout, list, query, e.Loc)
	})
	return cmd
}

func membersCmd(e env.Env) *cobra.Command {
	var inactiveSince string
	var friends, anonymous bool
	cmd := &cobra.Command{
		Use:   "members SESSION",
		Short: "List current group members and their locally recorded speaking activity",
		Long:  "List current group members and their locally recorded speaking activity\n\nSESSION: Group username or display name",
		Args:  cobra.ExactArgs(1),
		PreRunE: func(cmd *cobra.Command, _ []string) error {
			return conflicts(cmd, "friends", "inactive-since")
		},
	}
	src := addSource(cmd, e)
	f := cmd.Flags()
	f.StringVar(&inactiveSince, "inactive-since", "", "Only show members with no recorded messages since this time")
	f.BoolVar(&friends, "friends", false, "Only show locally confirmed friends; does not read message history")
	f.BoolVar(&anonymous, "anonymous", false, "Use group/member public names instead of your contact remarks")
	cmd.RunE = run(func(_ *cobra.Command, args []string) error {
		st, err := src.open()
		if err != nil {
			return err
		}
		defer st.Close()
		ropts := resolve.Options{Jobs: int(src.jobs), Loc: e.Loc}
		if friends {
			r, err := history.ListMemberFriends(st, args[0], nameMode(anonymous), ropts)
			if err != nil {
				return err
			}
			return render.MemberFriends(os.Stdout, r)
		}
		since, err := parseTime(e, "--inactive-since", inactiveSince)
		if err != nil {
			return err
		}
		r, err := history.ListMembers(st, history.MembersOptions{
			Session: args[0], InactiveSince: since, Mode: nameMode(anonymous), Jobs: int(src.jobs), Loc: e.Loc,
		})
		if err != nil {
			return err
		}
		return render.Members(os.Stdout, r, e.Loc)
	})
	return cmd
}

func doctorCmd(e env.Env) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "doctor [SESSION]",
		Short: "Diagnose tg setup and optionally a specific chat",
		Long:  "Diagnose tg setup and optionally a specific chat\n\nSESSION: Optional session username (tgid_xxx) or display name to inspect",
		Args:  cobra.MaximumNArgs(1),
	}
	src := addSource(cmd, e)
	cmd.RunE = run(func(_ *cobra.Command, args []string) error { return runDoctor(e, src, args) })
	return cmd
}

func messagesCmd(e env.Env) *cobra.Command {
	var limit, offset uint
	var search, sender, since string
	var allTime, tail, head, anonymous bool
	bucket := newEnum("1m", tgtime.BucketValues...)
	cmd := &cobra.Command{
		Use:   "messages SESSION",
		Short: "Read messages from a specific session",
		Long:  "Read messages from a specific session\n\nSESSION: Session username (tgid_xxx) or display name to search",
		Args:  cobra.ExactArgs(1),
		PreRunE: func(cmd *cobra.Command, _ []string) error {
			if err := conflicts(cmd, "since", "all-time"); err != nil {
				return err
			}
			return conflicts(cmd, "head", "tail")
		},
	}
	src := addSource(cmd, e)
	f := cmd.Flags()
	f.UintVar(&limit, "limit", 0, "Number of messages to show (defaults to 50 unless --since or --all-time is used)")
	f.UintVar(&offset, "offset", 0, "Skip this many messages, counted back from the newest (from the oldest with --head)")
	f.StringVar(&search, "search", "", "Search within messages")
	f.StringVar(&sender, "sender", "", "Only messages from this sender: me, an account id, or a name as shown")
	f.StringVar(&since, "since", "", "Show messages after this time (ISO 8601 or relative: 5min, 1h, today)")
	f.BoolVar(&allTime, "all-time", false, "Show the full history instead of the default latest 50 messages")
	f.BoolVar(&tail, "tail", false, "Read from the newest end (the default; accepted for older scripts)")
	f.BoolVar(&head, "head", false, "Show earliest messages instead of the default latest messages")
	f.Var(bucket, "time-bucket", "Timestamp grouping for output: 1m/1min, 1h, 1d, 1mo, 1y, full, or none")
	f.BoolVar(&anonymous, "anonymous", false, "Use group/member public names instead of your contact remarks")
	_ = cmd.RegisterFlagCompletionFunc("time-bucket", cobra.FixedCompletions(tgtime.BucketValues, cobra.ShellCompDirectiveNoFileComp))
	cmd.RunE = run(func(cmd *cobra.Command, args []string) error {
		st, err := src.open()
		if err != nil {
			return err
		}
		defer st.Close()
		sinceTS, err := parseTime(e, "--since", since)
		if err != nil {
			return err
		}
		b, err := tgtime.ParseBucket(bucket.value)
		if err != nil {
			return fmt.Errorf("invalid --time-bucket: %w", err)
		}
		n, limited := messagesLimit(limit, cmd.Flags().Changed("limit"), sinceTS.Set, allTime)
		order := msgdb.Newest
		if head {
			order = msgdb.Oldest
		}
		page, err := history.ReadMessages(st, history.ReadOptions{
			Session: args[0], Limit: n, Limited: limited, Offset: int(offset), Search: search,
			Since: sinceTS, Order: order, Sender: sender, Bucket: b, Mode: nameMode(anonymous),
			Jobs: int(src.jobs), Loc: e.Loc,
		})
		if err != nil {
			return err
		}
		return render.Messages(os.Stdout, page, render.MessagesView{
			SessionQuery: args[0], Search: search, Sender: sender, Order: order, Offset: int(offset),
			Limited: limited, Since: sinceTS, Bucket: b, Loc: e.Loc,
		})
	})
	return cmd
}

func searchCmd(e env.Env) *cobra.Command {
	var limit uint
	var since string
	var allTime, anonymous bool
	cmd := &cobra.Command{
		Use:   "search QUERY",
		Short: "Search across all sessions",
		Long:  "Search across all sessions\n\nQUERY: Search query",
		Args:  cobra.ExactArgs(1),
	}
	src := addSource(cmd, e)
	f := cmd.Flags()
	f.UintVar(&limit, "limit", 20, "Number of results")
	f.StringVar(&since, "since", "", "Show matches after this time (ISO 8601 or relative: 5min, 1h, today)")
	f.BoolVar(&allTime, "all-time", false, "Search the full history (the default; accepted for older scripts)")
	f.BoolVar(&anonymous, "anonymous", false, "Use public names instead of your contact remarks in output")
	cmd.RunE = run(func(_ *cobra.Command, args []string) error {
		if limit == 0 {
			return errors.New("--limit must be greater than 0")
		}
		st, err := src.open()
		if err != nil {
			return err
		}
		defer st.Close()
		sinceTS, err := parseTime(e, "--since", since)
		if err != nil {
			return err
		}
		res, err := history.Search(st, history.SearchOptions{
			Query: args[0], Limit: int(limit), Limited: true, Since: sinceTS, Mode: nameMode(anonymous), Jobs: int(src.jobs),
		})
		if err != nil {
			return err
		}
		return render.Search(os.Stdout, args[0], res, e.Loc)
	})
	return cmd
}

func queryCmd(e env.Env) *cobra.Command {
	var session, sender, since, until, fields string
	var contains, notContains, rawContains []string
	var allTime, anonymous bool
	var limit, offset, maxCell uint
	has := &enumListValue{allowed: []string{"voice", "image", "sticker", "file", "video"}}
	order := newEnum("newest", "newest", "oldest")
	match := newEnum("all", "all", "any")
	format := newEnum("table", "table", "json")
	cmd := &cobra.Command{
		Use:   "query",
		Short: "Search message tables with fixed, parameterized query templates",
		Args:  cobra.NoArgs,
	}
	src := addSource(cmd, e)
	f := cmd.Flags()
	f.StringVar(&session, "session", "", "Optional session username (tgid_xxx) or display name to search within")
	f.StringArrayVar(&contains, "contains", nil, "Keyword that must appear in message text; repeat for multiple keywords")
	f.StringArrayVar(&notContains, "not", nil, "Keyword that must not appear in message text; repeat for multiple keywords")
	f.StringArrayVar(&rawContains, "raw-contains", nil, "Keyword that must appear in the raw database message body; repeat for multiple keywords")
	f.Var(has, "has", "Require a structured media type: voice, image, sticker, file, or video")
	f.StringVar(&sender, "sender", "", "Only messages from this sender: me, an account id, or a name as shown")
	f.StringVar(&since, "since", "", "Show messages after this time (ISO 8601 or relative: 5min, 1h, today)")
	f.BoolVar(&allTime, "all-time", false, "Search the full history (the default; accepted for older scripts)")
	f.StringVar(&until, "until", "", "Show messages before this time (ISO 8601 or relative: 5min, 1h, today)")
	f.UintVar(&limit, "limit", 100, "Maximum rows to print")
	f.UintVar(&offset, "offset", 0, "Offset for pagination after global sorting")
	f.Var(order, "order", "Sort order: newest or oldest")
	f.Var(match, "match-mode", "Keyword matching mode when --contains is repeated: all or any")
	f.StringVar(&fields, "fields", "time,session,sender,body", "Output fields: time,session,sender,type,body,raw_body,timestamp")
	f.Var(format, "format", "Output format: table or json")
	f.UintVar(&maxCell, "max-cell-chars", 500, "Maximum displayed characters per text cell")
	f.BoolVar(&anonymous, "anonymous", false, "Use public names instead of your contact remarks in output")
	_ = cmd.RegisterFlagCompletionFunc("has", cobra.FixedCompletions(has.allowed, cobra.ShellCompDirectiveNoFileComp))
	_ = cmd.RegisterFlagCompletionFunc("order", cobra.FixedCompletions(order.allowed, cobra.ShellCompDirectiveNoFileComp))
	_ = cmd.RegisterFlagCompletionFunc("match-mode", cobra.FixedCompletions(match.allowed, cobra.ShellCompDirectiveNoFileComp))
	_ = cmd.RegisterFlagCompletionFunc("format", cobra.FixedCompletions(format.allowed, cobra.ShellCompDirectiveNoFileComp))
	cmd.RunE = run(func(*cobra.Command, []string) error {
		return runQuery(e, src, queryFlags{
			session: session, sender: sender, since: since, until: until, fields: fields,
			contains: contains, not: notContains, rawContains: rawContains, has: has.values,
			limit: limit, offset: offset, order: order.value, match: match.value, format: format.value,
			maxCellChars: maxCell, anonymous: anonymous,
		})
	})
	return cmd
}

func mediaCmd(e env.Env) *cobra.Command {
	var output string
	cmd := &cobra.Command{
		Use:   "media SESSION ID...",
		Short: "Save cached images from one chat as viewable files and print their paths",
		Long: "Save cached images from one chat as viewable files and print their paths\n\n" +
			"SESSION: Session username (tgid_xxx) or display name to search\nID: Media ids from [img:<id>]; the whole tag is accepted too",
		Args: cobra.MinimumNArgs(2),
	}
	src := addSource(cmd, e)
	cmd.Flags().StringVar(&output, "output", "", "Directory for the decrypted files [default: $TMPDIR/tg-media]")
	_ = cmd.MarkFlagDirname("output")
	cmd.RunE = run(func(_ *cobra.Command, args []string) error { return runMedia(e, src, args[0], args[1:], output) })
	return cmd
}

func exportCmd(e env.Env) *cobra.Command {
	var output string
	cmd := &cobra.Command{
		Use:   "export SESSION",
		Short: "Export a complete chat archive",
		Long:  "Export a complete chat archive\n\nSESSION: Session username (tgid_xxx) or display name to search",
		Args:  cobra.ExactArgs(1),
	}
	src := addSource(cmd, e)
	cmd.Flags().StringVar(&output, "output", "exported", "Output directory")
	_ = cmd.MarkFlagDirname("output")
	cmd.RunE = run(func(_ *cobra.Command, args []string) error { return runExport(e, src, args[0], output) })
	return cmd
}

func mcpCmd(e env.Env) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "mcp",
		Short: "Serve read-only tools to MCP clients (stdio, or HTTP with OAuth)",
		Args:  cobra.NoArgs,
	}
	var remotePolicy bool
	stdio := &cobra.Command{Use: "stdio", Short: "Serve MCP over stdin/stdout for a local client", Args: cobra.NoArgs}
	stdioSrc := addSource(stdio, e)
	stdio.RunE = run(func(*cobra.Command, []string) error { return runMCPStdio(e, stdioSrc, remotePolicy) })
	stdio.Flags().BoolVar(&remotePolicy, "remote-policy", false, "Apply the remote policy from ~/.tg/mcp/config.json")

	var publicURL string
	var redirectURIs []string
	var passphraseStdin bool
	listen := &addrValue{withIP: true}
	trustedProxy := &addrValue{}
	initCmd := &cobra.Command{Use: "init", Short: "Create or update the remote-mode config and owner passphrase", Args: cobra.NoArgs,
		RunE: run(func(*cobra.Command, []string) error {
			return runMCPInit(e, publicURL, listen, trustedProxy, redirectURIs, passphraseStdin)
		})}
	fi := initCmd.Flags()
	fi.StringVar(&publicURL, "public-url", "", "Public HTTPS origin clients connect to, e.g. https://mcp.example.com")
	fi.Var(listen, "listen", "Loopback or tailnet address to listen on, e.g. 100.64.0.5:8765")
	fi.Var(trustedProxy, "trusted-proxy", "Reverse proxy address whose X-Forwarded-For is trusted")
	fi.StringArrayVar(&redirectURIs, "redirect-uri", nil, "Extra OAuth redirect URI to accept (repeatable), e.g. for MCP Inspector")
	fi.BoolVar(&passphraseStdin, "passphrase-stdin", false, "Read a new owner passphrase from the first line of stdin")

	serve := &cobra.Command{Use: "serve", Short: "Serve MCP over HTTP with OAuth; requires `tg mcp init`", Args: cobra.NoArgs}
	serveSrc := addSource(serve, e)
	serve.RunE = run(func(*cobra.Command, []string) error { return runMCPServe(e, serveSrc) })

	status := &cobra.Command{Use: "status", Short: "Show the remote config and connected clients", Args: cobra.NoArgs,
		RunE: run(func(*cobra.Command, []string) error { return mcp.Status(e, os.Stdout) })}

	var client string
	var all bool
	revoke := &cobra.Command{
		Use: "revoke", Short: "Revoke OAuth clients and their tokens", Args: cobra.NoArgs,
		RunE: run(func(*cobra.Command, []string) error {
			return mcp.Revoke(e, mcp.RevokeOptions{Client: client, All: all}, os.Stdout)
		}),
		PreRunE: func(cmd *cobra.Command, _ []string) error {
			if err := conflicts(cmd, "client", "all"); err != nil {
				return err
			}
			if !cmd.Flags().Changed("client") && !cmd.Flags().Changed("all") {
				return errors.New("the following required arguments were not provided: --client <CLIENT>")
			}
			return nil
		},
	}
	revoke.Flags().StringVar(&client, "client", "", "Client id to revoke")
	revoke.Flags().BoolVar(&all, "all", false, "Revoke every client")

	var auditSince string
	audit := &cobra.Command{Use: "audit", Short: "Print recent tool calls from the audit log (no message content is logged)", Args: cobra.NoArgs,
		RunE: run(func(*cobra.Command, []string) error { return runMCPAudit(e, auditSince) })}
	audit.Flags().StringVar(&auditSince, "since", "1d", "Only entries after this time (ISO 8601 or relative: 1h, 1d, today)")

	cmd.AddCommand(stdio, initCmd, serve, status, revoke, audit)
	return cmd
}

func skillCmd(e env.Env) *cobra.Command {
	cmd := &cobra.Command{Use: "skill", Short: "Install or manage the local agent skill", Args: cobra.NoArgs}
	var dir string
	install := &cobra.Command{
		Use:   "install",
		Short: "Install the local SKILL.md generated from tg's bundled template",
		Args:  cobra.NoArgs,
		RunE: run(func(*cobra.Command, []string) error {
			path, err := installSkill(e, dir)
			if err != nil {
				return err
			}
			_, err = fmt.Fprintf(os.Stdout, "Skill installed to: %s\n", path)
			return err
		}),
	}
	install.Flags().StringVar(&dir, "dir", "", "Skill directory to write; defaults to $CODEX_HOME/skills/tg or ~/.codex/skills/tg")
	_ = install.MarkFlagDirname("dir")
	cmd.AddCommand(install)
	return cmd
}

var completionShells = []string{"fish", "zsh", "bash"}

func completionsCmd(root *cobra.Command) *cobra.Command {
	return &cobra.Command{
		Use:       "completions SHELL",
		Short:     "Generate shell completion scripts",
		Long:      "Generate shell completion scripts\n\nSHELL: Shell to generate completions for [possible values: fish, zsh, bash]",
		Args:      valueArgs("shell", completionShells...),
		ValidArgs: completionShells,
		RunE: run(func(_ *cobra.Command, args []string) error {
			switch args[0] {
			case "fish":
				return writeFish(os.Stdout, root)
			case "zsh":
				return writeZsh(os.Stdout, root)
			default:
				return writeBash(os.Stdout, root)
			}
		}),
	}
}

// usageTemplate follows clap's layout ("Usage: tg ...", Commands, Options)
// so the help text reads as before.
const usageTemplate = `Usage: {{if .HasAvailableSubCommands}}{{.CommandPath}} <COMMAND>{{else}}{{.UseLine}}{{end}}{{if .HasAvailableSubCommands}}

Commands:{{range .Commands}}{{if .IsAvailableCommand}}
  {{rpad .Name .NamePadding}} {{.Short}}{{end}}{{end}}{{end}}{{if .HasAvailableLocalFlags}}

Options:
{{.LocalFlags.FlagUsages | trimTrailingWhitespaces}}{{end}}{{if .HasAvailableInheritedFlags}}

Global Options:
{{.InheritedFlags.FlagUsages | trimTrailingWhitespaces}}{{end}}
`
