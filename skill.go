// Package tg holds files embedded from the repository root.
package tg

import _ "embed"

// SkillTemplate is the bundled agent skill, written with the public cover
// name; `tg skill install` renders the real app name into it.
//
//go:embed SKILL.md
var SkillTemplate string
