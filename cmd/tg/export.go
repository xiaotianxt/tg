package main

import (
	"bufio"
	"fmt"
	"os"
	"path/filepath"

	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/export"
)

func runExport(e env.Env, src *sourceFlags, session, output string) error {
	st, err := src.open()
	if err != nil {
		return err
	}
	defer st.Close()
	r, err := export.Run(st, export.Options{Session: session, OutputDir: output, Jobs: int(src.jobs), Env: e})
	if err != nil {
		return err
	}
	w := bufio.NewWriter(os.Stdout)
	saved := 0
	for _, m := range r.Media {
		if m.Kind == export.MediaWritten {
			fmt.Fprintf(w, "  Media #%d: %s\n", m.Index, filepath.Base(m.Path))
			saved++
		}
	}
	if saved > 0 {
		fmt.Fprintf(w, "Exported %d media files\n", saved)
	}
	fmt.Fprintf(w, "Exported %d messages for %s (%s)\n", r.Messages, r.DisplayName, r.Username)
	fmt.Fprintln(w, "Exported to:")
	for _, f := range r.Files {
		fmt.Fprintf(w, "  [%s] %s\n", f.Format, f.Path)
	}
	if err := w.Flush(); err != nil {
		return fmt.Errorf("Write output: %w", err)
	}
	return nil
}
