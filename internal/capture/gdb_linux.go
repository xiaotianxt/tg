//go:build linux

package capture

import (
	"bufio"
	"context"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/xiaotianxt/tg/internal/keys"
	"github.com/xiaotianxt/tg/internal/logx"
)

const (
	maxDebugOutputBytes = 64 * 1024
	terminationGrace    = 2 * time.Second
)

type gdbRequest struct {
	identity        *processIdentity
	gdbPath         string
	workDir         string
	runtimeAddress  uint64
	codeFingerprint [hookFingerprintLen]byte
	architecture    captureArch
	timeout         int
}

func gdbCapture(ctx context.Context, req gdbRequest) (*keys.Material, error) {
	scriptPath := filepath.Join(req.workDir, "capture_account_material.py")
	candidatePath := filepath.Join(req.workDir, "captured_material.bin")
	logPath := filepath.Join(req.workDir, "capture.log")
	if err := writePrivateNewFile(scriptPath, []byte(gdbCaptureScript)); err != nil {
		return nil, err
	}
	if err := writePrivateNewFile(candidatePath, nil); err != nil {
		return nil, err
	}
	if err := writePrivateNewFile(logPath, nil); err != nil {
		return nil, err
	}

	arch := "x86_64"
	if req.architecture == archAarch64 {
		arch = "aarch64"
	}
	cmd := exec.Command(req.gdbPath,
		"-q", "--nx", "-batch",
		"-ex", "set pagination off",
		"-ex", "set confirm off",
		"-ex", "set print thread-events off",
		"-ex", fmt.Sprintf("attach %d", req.identity.pid),
		"-x", scriptPath,
		"-ex", "continue",
		"-ex", "detach",
	)
	cmd.Dir = req.workDir
	// A cleared environment plus only the TG_GDB_* capture variables keeps the
	// debugger from inheriting anything that could alter its behaviour.
	cmd.Env = []string{
		"TERM=dumb",
		"TG_GDB_CAPTURE_OUTPUT=" + candidatePath,
		"TG_GDB_CAPTURE_LOG=" + logPath,
		fmt.Sprintf("TG_GDB_CAPTURE_ADDRESS=%#x", req.runtimeAddress),
		"TG_GDB_CAPTURE_ARCH=" + arch,
		fmt.Sprintf("TG_GDB_EXPECTED_PID=%d", req.identity.pid),
		fmt.Sprintf("TG_GDB_EXPECTED_UID=%d", req.identity.uid),
		fmt.Sprintf("TG_GDB_EXPECTED_START_TIME=%d", req.identity.startTimeTicks),
		fmt.Sprintf("TG_GDB_EXPECTED_DEVICE=%d", req.identity.executableDev),
		fmt.Sprintf("TG_GDB_EXPECTED_INODE=%d", req.identity.executableInode),
		"TG_GDB_EXPECTED_CODE=" + hex.EncodeToString(req.codeFingerprint[:]),
	}
	// Own process group so the whole debugger tree can be signalled, and
	// Pdeathsig so an abrupt tg exit cannot leave a privileged debugger
	// attached to the client.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true, Pdeathsig: syscall.SIGKILL}
	stdout := &boundedBuffer{max: maxDebugOutputBytes}
	stderr := &boundedBuffer{max: maxDebugOutputBytes}
	cmd.Stdout = stdout
	cmd.Stderr = stderr

	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("Failed to start the trusted system debugger: %w", err)
	}
	waitErr := waitForDebugger(ctx, cmd, req.timeout)

	if material, err := readCapturedMaterial(candidatePath); err == nil {
		return material, nil
	} else {
		logDebuggerOutput(stdout, stderr)
		if waitErr != nil {
			return nil, fmt.Errorf("%s; captured material status: %s.%s",
				waitErr, err, gdbCaptureLogSummary(logPath))
		}
		msg := fmt.Sprintf("Debugger capture exited with %d, but no valid material was available: %s.%s",
			cmd.ProcessState.ExitCode(), err, gdbCaptureLogSummary(logPath))
		if attachWasDenied(stderr.String()) {
			msg += "\nLinux denied debugger attach. Run the command through sudo from the desktop user and check the system ptrace policy."
		}
		return nil, fmt.Errorf("%s", msg)
	}
}

// waitForDebugger waits for the debugger, and on timeout or cancellation
// signals its whole process group (SIGTERM then SIGKILL after a grace period)
// and reaps it. It returns a non-nil error exactly when it had to stop the
// debugger.
func waitForDebugger(ctx context.Context, cmd *exec.Cmd, timeoutSecs int) error {
	done := make(chan struct{})
	go func() { cmd.Wait(); close(done) }()
	pgid := cmd.Process.Pid
	var timer <-chan time.Time
	if timeoutSecs > 0 {
		t := time.NewTimer(time.Duration(timeoutSecs) * time.Second)
		defer t.Stop()
		timer = t.C
	}
	select {
	case <-done:
		return nil
	case <-timer:
		terminateGroup(pgid, done)
		return fmt.Errorf("Debugger capture timed out after %d seconds", timeoutSecs)
	case <-ctx.Done():
		terminateGroup(pgid, done)
		return fmt.Errorf("Debugger capture was canceled")
	}
}

func terminateGroup(pgid int, done <-chan struct{}) {
	signalGroup(pgid, syscall.SIGTERM)
	select {
	case <-done:
		return
	case <-time.After(terminationGrace):
	}
	signalGroup(pgid, syscall.SIGKILL)
	<-done
}

func signalGroup(pgid int, sig syscall.Signal) {
	// A negative pid targets the process group whose id equals the leader pid.
	_ = syscall.Kill(-pgid, sig)
}

func findGDB() (string, error) {
	for _, candidate := range []string{"/usr/bin/gdb", "/bin/gdb", "/usr/local/bin/gdb"} {
		canonical, err := filepath.EvalSymlinks(candidate)
		if err != nil {
			continue
		}
		if trustedRootExecutable(canonical) {
			return canonical, nil
		}
	}
	return "", fmt.Errorf("gdb is required for `keys --method login`; install it with `sudo apt install gdb`.")
}

// trustedRootExecutable reports whether path is a root-owned executable with
// no group/other write permission, and every parent directory is likewise
// root-owned and not group/other writable, so an unprivileged user cannot
// have substituted the debugger tg runs as root.
func trustedRootExecutable(path string) bool {
	var st syscall.Stat_t
	if syscall.Stat(path, &st) != nil {
		return false
	}
	mode := st.Mode
	if mode&syscall.S_IFMT != syscall.S_IFREG || st.Uid != 0 || mode&0o111 == 0 || mode&0o022 != 0 {
		return false
	}
	dir := filepath.Dir(path)
	for {
		var ds syscall.Stat_t
		if syscall.Stat(dir, &ds) != nil {
			return false
		}
		if ds.Mode&syscall.S_IFMT != syscall.S_IFDIR || ds.Uid != 0 || ds.Mode&0o022 != 0 {
			return false
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return true
		}
		dir = parent
	}
}

func attachWasDenied(stderr string) bool {
	lower := strings.ToLower(stderr)
	return strings.Contains(lower, "operation not permitted") ||
		strings.Contains(lower, "ptrace") ||
		strings.Contains(lower, "could not attach")
}

func logDebuggerOutput(stdout, stderr *boundedBuffer) {
	if s := strings.TrimSpace(stdout.String()); s != "" {
		logx.Debugf("Debugger stdout (bounded):\n%s", s)
	}
	if s := strings.TrimSpace(stderr.String()); s != "" {
		logx.Debugf("Debugger stderr (bounded):\n%s", s)
	}
}

func gdbCaptureLogSummary(path string) string {
	installed, ignored, captured := false, 0, false
	f, err := os.Open(path)
	if err == nil {
		defer f.Close()
		scanner := bufio.NewScanner(f)
		scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)
		for scanner.Scan() {
			var rec struct {
				Event string `json:"event"`
			}
			if json.Unmarshal(scanner.Bytes(), &rec) != nil {
				continue
			}
			switch rec.Event {
			case "installed":
				installed = true
			case "ignored":
				ignored++
			case "captured":
				captured = true
			}
		}
	}
	return fmt.Sprintf("\ndebugger summary: installed=%t, ignored_hits=%d, captured=%t", installed, ignored, captured)
}

// boundedBuffer captures at most max bytes of a stream, dropping the rest.
type boundedBuffer struct {
	mu  sync.Mutex
	max int
	buf []byte
}

func (b *boundedBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if remaining := b.max - len(b.buf); remaining > 0 {
		if len(p) > remaining {
			b.buf = append(b.buf, p[:remaining]...)
		} else {
			b.buf = append(b.buf, p...)
		}
	}
	return len(p), nil
}

func (b *boundedBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return string(b.buf)
}

const gdbCaptureScript = `
import gdb
import json
import os

OUTPUT_PATH = os.environ["TG_GDB_CAPTURE_OUTPUT"]
LOG_PATH = os.environ["TG_GDB_CAPTURE_LOG"]
ADDRESS = int(os.environ["TG_GDB_CAPTURE_ADDRESS"], 0)
ARCHITECTURE = os.environ["TG_GDB_CAPTURE_ARCH"]
EXPECTED_PID = int(os.environ["TG_GDB_EXPECTED_PID"])
EXPECTED_UID = int(os.environ["TG_GDB_EXPECTED_UID"])
EXPECTED_START_TIME = int(os.environ["TG_GDB_EXPECTED_START_TIME"])
EXPECTED_DEVICE = int(os.environ["TG_GDB_EXPECTED_DEVICE"])
EXPECTED_INODE = int(os.environ["TG_GDB_EXPECTED_INODE"])
EXPECTED_CODE = bytes.fromhex(os.environ["TG_GDB_EXPECTED_CODE"])
ACCOUNT_MATERIAL_LENGTH = 32
IGNORED_LOG_LIMIT = 16
ignored_hits = 0


def _log(record):
    with open(LOG_PATH, "a", encoding="utf-8") as output:
        output.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
        output.flush()


def _ignored(reason):
    global ignored_hits
    ignored_hits += 1
    if ignored_hits <= IGNORED_LOG_LIMIT:
        _log({"event": "ignored", "reason": reason})


def _status_uid(pid):
    with open("/proc/{}/status".format(pid), "r", encoding="utf-8") as status:
        for line in status:
            if line.startswith("Uid:"):
                return int(line.split()[1])
    return -1


def _start_time(pid):
    with open("/proc/{}/stat".format(pid), "r", encoding="utf-8") as stat:
        value = stat.read()
    fields = value[value.rfind(")") + 1:].split()
    return int(fields[19])


def _verify_identity(inferior):
    if inferior.pid != EXPECTED_PID:
        raise gdb.GdbError("desktop process pid changed")
    metadata = os.stat("/proc/{}/exe".format(EXPECTED_PID))
    matches = (
        _status_uid(EXPECTED_PID) == EXPECTED_UID
        and _start_time(EXPECTED_PID) == EXPECTED_START_TIME
        and metadata.st_dev == EXPECTED_DEVICE
        and metadata.st_ino == EXPECTED_INODE
    )
    if not matches:
        raise gdb.GdbError("desktop process identity changed")
    code = inferior.read_memory(ADDRESS, len(EXPECTED_CODE)).tobytes()
    if code != EXPECTED_CODE:
        raise gdb.GdbError("desktop process code fingerprint changed")


def _register(name):
    return int(gdb.parse_and_eval("$" + name))


class AccountMaterialBreakpoint(gdb.Breakpoint):
    def stop(self):
        inferior = gdb.selected_inferior()
        try:
            if ARCHITECTURE == "aarch64":
                key_pointer = _register("x1")
                key_length = _register("x2")
            elif ARCHITECTURE == "x86_64":
                key_pointer = _register("rsi")
                key_length = _register("rdx")
            else:
                _ignored("architecture")
                return False

            if not key_pointer or key_length != ACCOUNT_MATERIAL_LENGTH:
                _ignored("shape")
                return False
            material = inferior.read_memory(
                key_pointer, ACCOUNT_MATERIAL_LENGTH
            ).tobytes()
        except (gdb.error, gdb.MemoryError):
            _ignored("read")
            return False

        with open(OUTPUT_PATH, "r+b") as output:
            output.seek(0)
            output.write(material)
            output.truncate()
            output.flush()
            os.fsync(output.fileno())
        _log({"event": "captured"})
        return True


inferior = gdb.selected_inferior()
_verify_identity(inferior)
AccountMaterialBreakpoint("*{:#x}".format(ADDRESS), internal=True)
_log({"event": "installed"})
`
