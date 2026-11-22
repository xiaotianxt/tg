package main

import (
	"bufio"
	"fmt"
	"os"

	"github.com/xiaotianxt/tg/internal/decode"
	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/media"
	"github.com/xiaotianxt/tg/internal/resolve"
)

// runMedia saves each cached image id of one chat and prints one line per
// id. Every id is attempted; the command fails when any could not be saved.
func runMedia(e env.Env, src *sourceFlags, session string, rawIDs []string, output string) error {
	// Ids reach file paths, so they are checked before anything is opened.
	ids := make([]string, len(rawIDs))
	for i, raw := range rawIDs {
		id, err := media.ParseMediaID(raw)
		if err != nil {
			return err
		}
		ids[i] = id
	}
	st, err := src.open()
	if err != nil {
		return err
	}
	defer st.Close()
	ref, err := resolve.SessionFromStore(st, session, resolve.Options{Activity: true, Jobs: int(src.jobs), Loc: e.Loc})
	if err != nil {
		return err
	}
	cache, err := media.OpenCache(e, ref.Username, "")
	if err != nil {
		return err
	}
	dir := output
	if dir == "" {
		dir = media.DefaultOutputDir(e)
	}

	w := bufio.NewWriter(os.Stdout)
	unavailable := 0
	for _, id := range ids {
		img, err := cache.Load(id, media.Original)
		path := ""
		if err == nil {
			path, err = media.WritePrivate(dir, id, img)
		}
		if err != nil {
			fmt.Fprintf(w, "%s  unavailable: %v\n", id, err)
			unavailable++
			continue
		}
		fmt.Fprintf(w, "%s  %s  %s  %s\n", id, img.Variant, decode.FormatFileSize(uint64(len(img.Data))), path)
		if img.Variant == "thumbnail" {
			fmt.Fprintln(w, dict.RenderAppNames("  only the thumbnail is cached; open the image in Telegram to download it, then retry"))
		}
	}
	if err := w.Flush(); err != nil {
		return fmt.Errorf("Write output: %w", err)
	}
	if unavailable > 0 {
		return fmt.Errorf("%d of %d media ids could not be saved", unavailable, len(ids))
	}
	return nil
}
