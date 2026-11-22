package main

import (
	"fmt"
	"os"

	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/query"
)

// queryFlags are the parsed `tg query` flags.
type queryFlags struct {
	session, sender, since, until, fields string
	contains, not, rawContains, has       []string
	limit, offset, maxCellChars           uint
	order, match, format                  string
	anonymous, official                   bool
}

func runQuery(e env.Env, src *sourceFlags, f queryFlags) error {
	since, err := parseTime(e, "--since", f.since)
	if err != nil {
		return err
	}
	until, err := parseTime(e, "--until", f.until)
	if err != nil {
		return err
	}
	fields, err := query.ParseFields(f.fields)
	if err != nil {
		return fmt.Errorf("invalid --fields: %w", err)
	}
	out := query.Output{Fields: fields, MaxCellChars: int(f.maxCellChars), Loc: e.Loc}
	if f.format == "json" {
		out.Format = query.JSONLines
	}
	if err := out.Validate(); err != nil {
		return err
	}
	o := query.Options{
		Session: f.session, Official: f.official, Contains: f.contains, Not: f.not, RawContains: f.rawContains,
		Sender: f.sender, Since: since, Until: until,
		Limit: int(f.limit), Limited: true, Offset: int(f.offset),
		Mode: nameMode(f.anonymous), Jobs: int(src.jobs), Loc: e.Loc,
	}
	for _, h := range f.has {
		m, err := query.ParseMedia(h)
		if err != nil {
			return err
		}
		o.Has = append(o.Has, m)
	}
	if f.order == "oldest" {
		o.Order = msgdb.Oldest
	}
	if f.match == "any" {
		o.Match = query.MatchAny
	}
	if err := o.Validate(); err != nil {
		return err
	}

	st, err := src.open()
	if err != nil {
		return err
	}
	defer st.Close()
	rows, err := query.Run(st, o)
	if err != nil {
		return err
	}
	if err := out.Write(os.Stdout, rows); err != nil {
		return err
	}
	if len(rows) == 0 {
		// JSON lines stay machine-readable: the note goes to stderr.
		w := os.Stdout
		if out.Format == query.JSONLines {
			w = os.Stderr
		}
		if _, err := fmt.Fprintln(w, "No rows returned."); err != nil {
			return fmt.Errorf("Write output: %w", err)
		}
	}
	return nil
}
