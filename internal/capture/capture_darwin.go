//go:build darwin

package capture

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/keys"
)

// loginCapture (macOS) authenticates sudo, finds the client process and lldb,
// then runs the lldb script under sudo to read the account key material out
// of the client's PBKDF2 call during a re-login.
func loginCapture(ctx context.Context, e env.Env, _ string, timeout int) (*keys.Material, error) {
	if err := validateSudoCredentials(ctx, timeout); err != nil {
		return nil, err
	}
	pid, err := telegramPID()
	if err != nil {
		return nil, fmt.Errorf("Cannot find desktop process: %w", err)
	}
	lldbPath, err := findLLDB()
	if err != nil {
		return nil, err
	}
	workDir, err := os.MkdirTemp(e.TempDir, "tg-lldb-material-")
	if err != nil {
		return nil, fmt.Errorf("Cannot create lldb capture directory: %w", err)
	}
	defer os.RemoveAll(workDir)
	return lldbCapture(ctx, captureRequest{
		pid:      pid,
		lldbPath: lldbPath,
		workDir:  workDir,
		timeout:  timeout,
	})
}

func telegramPID() (int, error) {
	out, err := exec.Command("pgrep", "-x", dict.DesktopAppProcess()).Output()
	if err != nil {
		return 0, fmt.Errorf("Telegram is not running.")
	}
	line := strings.TrimSpace(firstLine(string(out)))
	pid, err := strconv.Atoi(line)
	if err != nil {
		return 0, fmt.Errorf("Invalid PID: %w", err)
	}
	return pid, nil
}

func firstLine(s string) string {
	if i := strings.IndexByte(s, '\n'); i >= 0 {
		return s[:i]
	}
	return s
}

// validateSudoCredentials runs `sudo -v`, reading the password prompt from
// /dev/tty so it works even when stdin is not a terminal.
func validateSudoCredentials(ctx context.Context, timeout int) error {
	cmd := exec.Command("sudo", "-v")
	cmd.Stdout = nil
	if tty, err := os.OpenFile("/dev/tty", os.O_RDONLY, 0); err == nil {
		cmd.Stdin = tty
		defer tty.Close()
	} else {
		cmd.Stdin = os.Stdin
	}
	if tty, err := os.OpenFile("/dev/tty", os.O_WRONLY, 0); err == nil {
		cmd.Stderr = tty
		defer tty.Close()
	} else {
		cmd.Stderr = os.Stderr
	}
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("Failed to request sudo authentication: %w", err)
	}
	status, err := waitWithTimeout(ctx, cmd, timeout)
	if err != nil {
		return fmt.Errorf("sudo authentication %s", err)
	}
	if !status.Success() {
		return fmt.Errorf("sudo authentication failed with exit code %d.", status.ExitCode())
	}
	return nil
}

func findLLDB() (string, error) {
	out, err := exec.Command("xcrun", "--find", "lldb").Output()
	if err == nil {
		if p := strings.TrimSpace(string(out)); p != "" {
			return p, nil
		}
	}
	detail := ""
	if ee, ok := err.(*exec.ExitError); ok {
		if s := strings.TrimSpace(string(ee.Stderr)); s != "" {
			detail = " xcrun said: " + s
		}
	}
	return "", fmt.Errorf(
		"lldb is required for `keys --method login`; install Apple Command Line Tools with `xcode-select --install`.%s", detail)
}

type captureRequest struct {
	pid      int
	lldbPath string
	workDir  string
	timeout  int
}

func lldbCapture(ctx context.Context, req captureRequest) (*keys.Material, error) {
	scriptPath := filepath.Join(req.workDir, "capture_account_material.py")
	candidatePath := filepath.Join(req.workDir, "captured_material.bin")
	logPath := filepath.Join(req.workDir, "capture.log")
	if err := writePrivateNewFile(scriptPath, []byte(lldbCaptureScript)); err != nil {
		return nil, err
	}
	if err := writePrivateNewFile(candidatePath, nil); err != nil {
		return nil, err
	}

	cmd := exec.Command("sudo", "-n", "env", "TERM=dumb",
		"TG_LLDB_CAPTURE_OUTPUT="+candidatePath,
		"TG_LLDB_CAPTURE_LOG="+logPath,
		req.lldbPath,
		"-b",
		"-o", "command script import "+scriptPath,
		"-o", fmt.Sprintf("process attach --pid %d", req.pid),
		"-o", "capture_tg_account_material",
		"-o", "process continue",
	)
	cmd.Dir = req.workDir
	var stdout, stderr strings.Builder
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("Failed to run lldb account material capture: %w", err)
	}
	status, waitErr := waitWithTimeout(ctx, cmd, req.timeout+10)

	if material, err := readCapturedMaterial(candidatePath); err == nil {
		return material, nil
	}

	if waitErr != nil {
		return nil, fmt.Errorf("lldb account material capture %s.%s", waitErr, captureLogSummary(logPath))
	}
	message := fmt.Sprintf("lldb account material capture exited with %d.%s%s%s",
		status.ExitCode(),
		labeledOutput("lldb output", stdout.String()),
		labeledOutput("lldb error output", stderr.String()),
		captureLogSummary(logPath))
	return nil, fmt.Errorf("%s", withAttachRecoveryHint(message, stderr.String()))
}

func labeledOutput(label, output string) string {
	if strings.TrimSpace(output) == "" {
		return ""
	}
	return "\n" + label + ":\n" + strings.TrimRight(output, "\n")
}

func withAttachRecoveryHint(message, errorOutput string) string {
	lower := strings.ToLower(errorOutput)
	if !strings.Contains(lower, "not allowed to attach") &&
		!strings.Contains(lower, "attach failed") &&
		!strings.Contains(lower, "task_for_pid") {
		return message
	}
	return message + "\n\nmacOS denied the lldb/debugserver attach. Enable Developer Tools access for " +
		"this terminal, restart the terminal, and retry. You can initialize the system setting " +
		"with:\n\n  sudo DevToolsSecurity -enable\n\nThen rerun:\n\n  " +
		"sudo tg keys --method login --timeout 180"
}

// waitWithTimeout waits for cmd, killing it and returning an error when the
// timeout elapses or ctx is canceled first.
func waitWithTimeout(ctx context.Context, cmd *exec.Cmd, timeoutSecs int) (processStatus, error) {
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	var timer <-chan time.Time
	if timeoutSecs > 0 {
		t := time.NewTimer(time.Duration(timeoutSecs) * time.Second)
		defer t.Stop()
		timer = t.C
	}
	select {
	case err := <-done:
		return statusFromWait(cmd, err), nil
	case <-timer:
		killProcess(cmd)
		<-done
		return processStatus{}, fmt.Errorf("timed out after %d seconds.", timeoutSecs)
	case <-ctx.Done():
		killProcess(cmd)
		<-done
		return processStatus{}, fmt.Errorf("was canceled.")
	}
}

func killProcess(cmd *exec.Cmd) {
	if cmd.Process != nil {
		_ = cmd.Process.Kill()
	}
}

type processStatus struct {
	code int
	ok   bool
}

func (p processStatus) Success() bool { return p.ok }
func (p processStatus) ExitCode() int { return p.code }

func statusFromWait(cmd *exec.Cmd, err error) processStatus {
	if err == nil {
		return processStatus{code: 0, ok: true}
	}
	if cmd.ProcessState != nil {
		return processStatus{code: cmd.ProcessState.ExitCode(), ok: cmd.ProcessState.Success()}
	}
	return processStatus{code: -1, ok: false}
}

func captureLogSummary(path string) string {
	installed, ignored, captured := summarizeCaptureLog(path)
	return fmt.Sprintf("\nlldb summary: locations=%d, ignored_hits=%d, captured=%t", installed, ignored, captured)
}
