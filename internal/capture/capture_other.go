//go:build !darwin && !linux

package capture

import (
	"context"
	"fmt"

	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/keys"
)

func loginCapture(_ context.Context, _ env.Env, _ string, _ int) (*keys.Material, error) {
	return nil, fmt.Errorf("Login key extraction is only supported on macOS and Linux.")
}
