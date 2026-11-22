//go:build linux

package capture

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"golang.org/x/sys/unix"

	"github.com/xiaotianxt/tg/internal/keys"
	"github.com/xiaotianxt/tg/internal/logx"
)

// tracefsCandidates are the usual tracefs mount points; the first with a
// writable uprobe_events control file is used.
var tracefsCandidates = []string{"/sys/kernel/tracing", "/sys/kernel/debug/tracing"}

const (
	// uprobeGroup is the tracefs event subsystem the capture probe lives in.
	uprobeGroup = "tgcap"
	// pollIntervalMs bounds how long a single poll blocks, so cancellation and
	// the overall timeout are observed promptly while reading trace_pipe.
	pollIntervalMs = 200
	// accountMaterialLen is the required argument length of a key-derivation
	// call; any other length is a different, ignored call shape.
	accountMaterialLen = capturedMaterialLen
)

// uprobeCapture registers a kernel uprobe at the account-material hook and
// reads the 32-byte material from the first matching hit, then removes the
// probe. The flow is an explicit state machine; every registered kernel
// resource is released on success, timeout, cancellation and error.
func uprobeCapture(ctx context.Context, req uprobeRequest) (*keys.Material, error) {
	if err := verifyOnDiskCode(req); err != nil {
		return nil, err
	}
	tracefs, err := findTracefs()
	if err != nil {
		return nil, err
	}
	ptrReg, lenReg, ok := uprobeRegisters(req.architecture)
	if !ok {
		return nil, fmt.Errorf("Unsupported Linux desktop client architecture for uprobe capture")
	}
	exePath := req.identity.executablePath
	if strings.ContainsAny(exePath, " \t\n:") {
		return nil, fmt.Errorf(
			"Desktop client path %q contains whitespace or ':'; uprobe registration needs a plain path", exePath)
	}

	p := &uprobe{
		tracefs: tracefs,
		group:   uprobeGroup,
		event:   fmt.Sprintf("m_%d_%d", req.identity.pid, time.Now().UnixNano()&0xffffff),
		traceFD: -1,
		state:   stateInit,
	}
	defer p.close()

	if err := p.createInstance(); err != nil {
		return nil, err
	}
	definition := fmt.Sprintf("p:%s/%s %s:0x%x %s",
		p.group, p.event, exePath, req.fileOffset, buildFetchArgs(ptrReg, lenReg))
	if err := p.register(definition); err != nil {
		return nil, err
	}
	p.scopeToProcessTree(req.identity)
	p.restrictToMaterialShape()
	if err := p.openTracePipe(); err != nil {
		return nil, err
	}
	if err := p.enable(); err != nil {
		return nil, err
	}
	logx.Debugf("Installed account-material uprobe (%s) at %s.", p.event, req.describe())

	material, err := p.read(ctx, req.timeout)
	if err != nil {
		return nil, err
	}
	p.state = stateCaptured
	return material, nil
}

// uprobe owns the kernel resources a single capture holds, so close can undo
// exactly what was created regardless of where the flow stopped.
type uprobe struct {
	tracefs      string
	group        string
	event        string
	traceFD      int
	state        captureState
	instanceMade bool
	registered   bool
	enabled      bool
	ignoredHits  int
}

func (u *uprobe) instanceDir() string { return filepath.Join(u.tracefs, "instances", u.event) }
func (u *uprobe) eventDir() string {
	return filepath.Join(u.instanceDir(), "events", u.group, u.event)
}

func (u *uprobe) createInstance() error {
	// Making the directory creates a private trace buffer; capture events are
	// read only from it, never from the shared global trace_pipe.
	if err := os.Mkdir(u.instanceDir(), 0o700); err != nil {
		return fmt.Errorf("Cannot create tracefs capture instance: %w", err)
	}
	u.instanceMade = true
	u.state = stateInstance
	return nil
}

func (u *uprobe) register(definition string) error {
	if err := appendTracefs(filepath.Join(u.tracefs, "uprobe_events"), definition); err != nil {
		if errors.Is(err, os.ErrPermission) || errors.Is(err, syscall.EPERM) {
			return fmt.Errorf(
				"Kernel denied uprobe registration: %w\nRun through sudo from the desktop user and ensure the kernel has CONFIG_UPROBE_EVENTS.", err)
		}
		return fmt.Errorf("Cannot register account-material uprobe: %w", err)
	}
	u.registered = true
	u.state = stateRegistered
	return nil
}

// scopeToProcessTree confines the instance to the client's current processes
// and their future children, so only the desktop client's hits are recorded.
// It is best-effort: on a kernel without this control the binary-inode scope
// and the length filter still select the right call.
func (u *uprobe) scopeToProcessTree(identity *processIdentity) {
	pids := clientProcessPIDs(identity)
	if len(pids) == 0 {
		return
	}
	var b strings.Builder
	for _, pid := range pids {
		fmt.Fprintf(&b, "%d ", pid)
	}
	if err := writeTracefs(filepath.Join(u.instanceDir(), "set_event_pid"), strings.TrimSpace(b.String())); err != nil {
		logx.Debugf("Could not restrict capture to the client process set: %v", err)
		return
	}
	if err := writeTracefs(filepath.Join(u.instanceDir(), "options", "event-fork"), "1"); err != nil {
		logx.Debugf("Could not enable event-fork for child processes: %v", err)
	}
}

// restrictToMaterialShape asks the kernel to record only hits whose argument
// length is the account-material length, so wrong-shaped calls never reach
// trace_pipe. The read loop re-checks the length regardless.
func (u *uprobe) restrictToMaterialShape() {
	filter := fmt.Sprintf("len == %d", accountMaterialLen)
	if err := writeTracefs(filepath.Join(u.eventDir(), "filter"), filter); err != nil {
		logx.Debugf("Could not set capture length filter: %v", err)
	}
}

func (u *uprobe) openTracePipe() error {
	fd, err := unix.Open(filepath.Join(u.instanceDir(), "trace_pipe"), unix.O_RDONLY|unix.O_NONBLOCK, 0)
	if err != nil {
		return fmt.Errorf("Cannot open capture trace pipe: %w", err)
	}
	u.traceFD = fd
	return nil
}

func (u *uprobe) enable() error {
	if err := writeTracefs(filepath.Join(u.eventDir(), "enable"), "1"); err != nil {
		return fmt.Errorf("Cannot enable account-material uprobe: %w", err)
	}
	u.enabled = true
	u.state = stateEnabled
	return nil
}

// read polls the capture trace pipe until a hit yields valid material, the
// timeout elapses, or the context is canceled.
func (u *uprobe) read(ctx context.Context, timeoutSecs int) (*keys.Material, error) {
	var deadline time.Time
	if timeoutSecs > 0 {
		deadline = time.Now().Add(time.Duration(timeoutSecs) * time.Second)
	}
	buf := make([]byte, 16*1024)
	var pending []byte
	for {
		select {
		case <-ctx.Done():
			return nil, fmt.Errorf("Uprobe capture was canceled")
		default:
		}
		pollMs := pollIntervalMs
		if !deadline.IsZero() {
			remaining := time.Until(deadline)
			if remaining <= 0 {
				return nil, fmt.Errorf("Uprobe capture timed out after %d seconds (%d non-matching hit(s)).", timeoutSecs, u.ignoredHits)
			}
			if ms := int(remaining.Milliseconds()); ms < pollMs {
				pollMs = ms + 1
			}
		}
		fds := []unix.PollFd{{Fd: int32(u.traceFD), Events: unix.POLLIN}}
		if _, err := unix.Poll(fds, pollMs); err != nil {
			if errors.Is(err, syscall.EINTR) {
				continue
			}
			return nil, fmt.Errorf("Cannot poll capture trace pipe: %w", err)
		}
		if fds[0].Revents&unix.POLLIN == 0 {
			continue
		}
		n, err := unix.Read(u.traceFD, buf)
		if err != nil {
			if errors.Is(err, syscall.EAGAIN) || errors.Is(err, syscall.EINTR) {
				continue
			}
			return nil, fmt.Errorf("Cannot read capture trace pipe: %w", err)
		}
		if n == 0 {
			continue
		}
		pending = append(pending, buf[:n]...)
		if material, done := u.consumeLines(&pending); done {
			return material, nil
		}
	}
}

// consumeLines parses every complete line in pending (keeping any partial
// trailing line) and returns material for the first hit of the right length.
func (u *uprobe) consumeLines(pending *[]byte) (*keys.Material, bool) {
	data := *pending
	for {
		idx := bytes.IndexByte(data, '\n')
		if idx < 0 {
			break
		}
		line := string(data[:idx])
		data = data[idx+1:]
		hit, ok := parseTraceLine(line)
		if !ok {
			continue
		}
		if hit.length != accountMaterialLen {
			u.ignoredHits++
			continue
		}
		*pending = data
		return hit.material(), true
	}
	*pending = data
	return nil, false
}

// close releases every kernel resource this capture created, in reverse order.
// It is safe to call once at any state and never leaves a uprobe registered.
func (u *uprobe) close() {
	if u.traceFD >= 0 {
		unix.Close(u.traceFD)
		u.traceFD = -1
	}
	if u.enabled {
		if err := writeTracefs(filepath.Join(u.eventDir(), "enable"), "0"); err != nil {
			logx.Debugf("Could not disable capture uprobe: %v", err)
		}
		u.enabled = false
	}
	if u.registered {
		if err := appendTracefs(filepath.Join(u.tracefs, "uprobe_events"), fmt.Sprintf("-:%s/%s", u.group, u.event)); err != nil {
			logx.Debugf("Could not remove capture uprobe definition: %v", err)
		}
		u.registered = false
	}
	if u.instanceMade {
		if err := os.Remove(u.instanceDir()); err != nil {
			logx.Debugf("Could not remove tracefs capture instance: %v", err)
		}
		u.instanceMade = false
	}
}

// verifyOnDiskCode confirms the bytes at the hook offset still match the
// fingerprint findHook decoded, so a binary swapped between locating the hook
// and registering the probe is rejected. The process inode is pinned
// separately by processIdentity.verify.
func verifyOnDiskCode(req uprobeRequest) error {
	f, err := os.Open(req.identity.procExePath())
	if err != nil {
		return fmt.Errorf("Cannot open desktop client executable: %w", err)
	}
	defer f.Close()
	got := make([]byte, hookFingerprintLen)
	if _, err := f.ReadAt(got, int64(req.fileOffset)); err != nil {
		return fmt.Errorf("Cannot read desktop client code at the hook: %w", err)
	}
	for i := range got {
		if got[i] != req.codeFingerprint[i] {
			return fmt.Errorf("Desktop client code at the hook changed; retry.")
		}
	}
	return nil
}

// findTracefs returns the first tracefs mount whose uprobe_events control file
// can be opened for writing.
func findTracefs() (string, error) {
	for _, root := range tracefsCandidates {
		if fd, err := unix.Open(filepath.Join(root, "uprobe_events"), unix.O_WRONLY|unix.O_APPEND, 0); err == nil {
			unix.Close(fd)
			return root, nil
		}
	}
	return "", fmt.Errorf(
		"Cannot access kernel tracefs (uprobe_events) under %s.\n"+
			"Mount it with `sudo mount -t tracefs tracefs /sys/kernel/tracing` and ensure the kernel has CONFIG_UPROBE_EVENTS; run tg through sudo.",
		strings.Join(tracefsCandidates, " or "))
}

// clientProcessPIDs lists the pids whose executable is the same inode as the
// pinned client, owned by the same user, so capture can be scoped to the whole
// client process set rather than a single pid.
func clientProcessPIDs(identity *processIdentity) []int {
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return nil
	}
	var pids []int
	for _, entry := range entries {
		pid, err := strconv.Atoi(entry.Name())
		if err != nil {
			continue
		}
		var st syscall.Stat_t
		if syscall.Stat(fmt.Sprintf("/proc/%d/exe", pid), &st) != nil {
			continue
		}
		if uint64(st.Ino) != identity.executableInode || uint64(st.Dev) != identity.executableDev {
			continue
		}
		pids = append(pids, pid)
	}
	return pids
}

// writeTracefs writes content to a tracefs control file, truncating it.
func writeTracefs(path, content string) error {
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_TRUNC, 0)
	if err != nil {
		return err
	}
	_, werr := f.WriteString(content)
	cerr := f.Close()
	if werr != nil {
		return werr
	}
	return cerr
}

// appendTracefs appends a line to a tracefs control file, used for the
// append-only uprobe_events definition list.
func appendTracefs(path, content string) error {
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_APPEND, 0)
	if err != nil {
		return err
	}
	_, werr := f.WriteString(content + "\n")
	cerr := f.Close()
	if werr != nil {
		return werr
	}
	return cerr
}
