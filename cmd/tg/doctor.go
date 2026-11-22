package main

import (
	"os"

	"github.com/xiaotianxt/tg/internal/doctor"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/render"
)

func runDoctor(e env.Env, src *sourceFlags, args []string) error {
	o := doctor.Options{DBDir: src.dbDir, KeysPath: src.keys, Jobs: int(src.jobs)}
	if len(args) == 1 {
		o.Session = args[0]
	}
	return render.Doctor(os.Stdout, doctor.Run(e, o))
}
