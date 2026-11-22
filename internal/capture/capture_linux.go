//go:build linux

package capture

import (
	"context"
	"fmt"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/keys"
)

// loginCapture (Linux) pins the client process identity, locates the
// account-material hook in its executable, converts the hook address to a file
// offset, and registers a kernel uprobe that copies the material at the hook
// during a re-login. Unlike the previous gdb flow it never attaches a debugger
// to the client; the probe reads the buffer in kernel context at the hit.
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
	fileOffset, err := fileOffsetForVA(identity.procExePath(), hook.virtualAddress)
	if err != nil {
		return nil, err
	}
	return uprobeCapture(ctx, uprobeRequest{
		identity:        identity,
		fileOffset:      fileOffset,
		architecture:    hook.architecture,
		codeFingerprint: hook.codeFingerprint,
		timeout:         timeout,
	})
}

// uprobeRequest carries everything a single uprobe capture needs.
type uprobeRequest struct {
	identity        *processIdentity
	fileOffset      uint64
	architecture    captureArch
	codeFingerprint [hookFingerprintLen]byte
	timeout         int
}

func (r uprobeRequest) describe() string {
	return fmt.Sprintf("pid %d offset %#x", r.identity.pid, r.fileOffset)
}
