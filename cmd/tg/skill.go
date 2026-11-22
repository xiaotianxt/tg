package main

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/xiaotianxt/tg"
	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
)

// installSkill writes the rendered SKILL.md into dir (default
// $CODEX_HOME/skills/tg or ~/.codex/skills/tg). A symlinked directory is
// refused so the write cannot land somewhere the user did not choose.
func installSkill(e env.Env, dir string) (string, error) {
	if dir == "" {
		home := e.Getenv("CODEX_HOME")
		if home == "" {
			if e.Home == "" {
				return "", errors.New("HOME is not set; pass --dir to choose a skill directory")
			}
			home = filepath.Join(e.Home, ".codex")
		}
		dir = filepath.Join(home, "skills", "tg")
	}
	if st, err := os.Lstat(dir); err == nil && st.Mode()&os.ModeSymlink != 0 {
		return "", fmt.Errorf("%s is a symlink; remove it explicitly or choose --dir", dir)
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", fmt.Errorf("Cannot create %s: %w", dir, err)
	}
	path := filepath.Join(dir, "SKILL.md")
	if err := os.WriteFile(path, []byte(dict.RenderAppNames(tg.SkillTemplate)), 0o644); err != nil {
		return "", fmt.Errorf("Cannot write %s: %w", path, err)
	}
	return path, nil
}
