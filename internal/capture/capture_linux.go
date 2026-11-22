//go:build linux

package capture

import (
	"context"
	"fmt"
	"os"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/keys"
)

// loginCapture (Linux) pins the client process identity, locates the
// account-material hook in its executable, maps the hook into the running
// process, then drives a trusted gdb to read the material at the hook during
// a re-login.
func loginCapture(ctx context.Context, _ env.Env, _ string, timeout int) (*keys.Material, error) {
	identity, err := findInvokingUserMainProcess(dict.DesktopAppProcess())
	if err != nil {
		return nil, err
	}
	hook, err := findHook(identity.procExePath())
	if err != nil {
		return nil, err
	}
	if err := identity.verify(); err != nil {
		return nil, err
	}
	runtimeAddr, err := runtimeAddress(identity, hook.fileOffset)
	if err != nil {
		return nil, err
	}
	gdbPath, err := findGDB()
	if err != nil {
		return nil, err
	}
	workDir, err := os.MkdirTemp("/tmp", "tg-gdb-material-")
	if err != nil {
		return nil, fmt.Errorf("Cannot create gdb capture directory: %w", err)
	}
	defer os.RemoveAll(workDir)
	return gdbCapture(ctx, gdbRequest{
		identity:        identity,
		gdbPath:         gdbPath,
		workDir:         workDir,
		runtimeAddress:  runtimeAddr,
		codeFingerprint: hook.codeFingerprint,
		architecture:    hook.architecture,
		timeout:         timeout,
	})
}
