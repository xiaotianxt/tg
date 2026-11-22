//go:build linux

package capture

import (
	"fmt"
	"os"
	"sort"
	"strconv"
	"strings"
	"syscall"
)

// processIdentity pins the desktop client process so a debugger attach cannot
// be redirected to a different process substituted between discovery and
// attach: the start time, executable device and inode must all still match.
type processIdentity struct {
	pid             int
	uid             uint32
	parentPID       int
	startTimeTicks  uint64
	executableDev   uint64
	executableInode uint64
	executablePath  string
}

func (p *processIdentity) procExePath() string {
	return fmt.Sprintf("/proc/%d/exe", p.pid)
}

// verify re-reads the process identity and fails if anything load-bearing
// changed since discovery.
func (p *processIdentity) verify() error {
	current, err := captureIdentity(p.pid, p.uid)
	if err != nil {
		return err
	}
	if current.startTimeTicks != p.startTimeTicks ||
		current.executableDev != p.executableDev ||
		current.executableInode != p.executableInode {
		return fmt.Errorf("Desktop client process changed during capture setup; retry.")
	}
	return nil
}

// findInvokingUserMainProcess returns the single top-level client process
// owned by the invoking (non-root) user, refusing ambiguity.
func findInvokingUserMainProcess(processName string) (*processIdentity, error) {
	uid, err := invokingUserUID()
	if err != nil {
		return nil, err
	}
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return nil, fmt.Errorf("Cannot enumerate Linux processes: %w", err)
	}
	var candidates []*processIdentity
	for _, entry := range entries {
		pid, err := strconv.Atoi(entry.Name())
		if err != nil {
			continue
		}
		comm, err := os.ReadFile(fmt.Sprintf("/proc/%d/comm", pid))
		if err != nil {
			continue
		}
		if strings.TrimRight(string(comm), "\n") != processName {
			continue
		}
		if id, err := captureIdentity(pid, uid); err == nil {
			candidates = append(candidates, id)
		}
	}
	if len(candidates) == 0 {
		return nil, fmt.Errorf("Desktop client is not running for the invoking user.")
	}

	pids := map[int]bool{}
	for _, c := range candidates {
		pids[c.pid] = true
	}
	var roots []*processIdentity
	for _, c := range candidates {
		if !pids[c.parentPID] {
			roots = append(roots, c)
		}
	}
	sort.Slice(roots, func(i, j int) bool { return roots[i].startTimeTicks < roots[j].startTimeTicks })
	switch len(roots) {
	case 1:
		return roots[0], nil
	default:
		return nil, fmt.Errorf(
			"Found %d independent desktop client processes for the invoking user; close duplicates and retry.", len(roots))
	}
}

// invokingUserUID requires tg to run as root via sudo from the desktop user,
// returning that user's uid from SUDO_UID.
func invokingUserUID() (uint32, error) {
	if os.Geteuid() != 0 {
		return 0, fmt.Errorf("Linux login capture must be started with `sudo tg keys --method login`.")
	}
	raw := os.Getenv("SUDO_UID")
	if raw == "" {
		return 0, fmt.Errorf("Cannot identify the invoking desktop user from SUDO_UID.")
	}
	uid, err := strconv.ParseUint(raw, 10, 32)
	if err != nil {
		return 0, fmt.Errorf("Invalid SUDO_UID: %w", err)
	}
	if uid == 0 {
		return 0, fmt.Errorf("Run login capture through sudo from the desktop user, not a root shell.")
	}
	return uint32(uid), nil
}

func captureIdentity(pid int, expectedUID uint32) (*processIdentity, error) {
	statusRaw, err := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
	if err != nil {
		return nil, fmt.Errorf("Cannot read desktop process status: %w", err)
	}
	status := string(statusRaw)
	uid, ok := parseStatusUID(status)
	if !ok {
		return nil, fmt.Errorf("Desktop process status has no uid")
	}
	if uid != expectedUID {
		return nil, fmt.Errorf("Desktop process belongs to another user")
	}
	for _, line := range strings.Split(status, "\n") {
		if strings.HasPrefix(line, "State:") && (strings.Contains(line, " Z ") || strings.Contains(line, "(zombie)")) {
			return nil, fmt.Errorf("Desktop process is a zombie")
		}
	}

	statRaw, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if err != nil {
		return nil, fmt.Errorf("Cannot read desktop process identity: %w", err)
	}
	parentPID, startTime, err := parseProcStat(string(statRaw))
	if err != nil {
		return nil, err
	}

	exe := fmt.Sprintf("/proc/%d/exe", pid)
	var st syscall.Stat_t
	if err := syscall.Stat(exe, &st); err != nil {
		return nil, fmt.Errorf("Cannot inspect desktop process executable: %w", err)
	}
	target, err := os.Readlink(exe)
	if err != nil {
		return nil, fmt.Errorf("Cannot resolve desktop process executable: %w", err)
	}

	return &processIdentity{
		pid:             pid,
		uid:             uid,
		parentPID:       parentPID,
		startTimeTicks:  startTime,
		executableDev:   uint64(st.Dev),
		executableInode: uint64(st.Ino),
		executablePath:  target,
	}, nil
}

func parseStatusUID(status string) (uint32, bool) {
	for _, line := range strings.Split(status, "\n") {
		if !strings.HasPrefix(line, "Uid:") {
			continue
		}
		fields := strings.Fields(line)
		if len(fields) < 2 {
			return 0, false
		}
		v, err := strconv.ParseUint(fields[1], 10, 32)
		if err != nil {
			return 0, false
		}
		return uint32(v), true
	}
	return 0, false
}

// parseProcStat returns the parent pid (field 4) and start time in clock
// ticks (field 22). The command name in field 2 is wrapped in parentheses and
// may itself contain spaces and parentheses, so the fixed fields are read from
// after the final ')'.
func parseProcStat(stat string) (int, uint64, error) {
	close := strings.LastIndexByte(stat, ')')
	if close < 0 {
		return 0, 0, fmt.Errorf("Malformed /proc stat command field")
	}
	rest := stat[close+1:]
	fields := strings.Fields(rest)
	// fields[0] is state; parent pid is the next field, start time is 20
	// fields after state.
	if len(fields) < 2 {
		return 0, 0, fmt.Errorf("Missing /proc stat parent pid")
	}
	parentPID, err := strconv.Atoi(fields[1])
	if err != nil {
		return 0, 0, fmt.Errorf("Invalid /proc stat parent pid: %w", err)
	}
	if len(fields) < 20 {
		return 0, 0, fmt.Errorf("Missing /proc stat start time")
	}
	startTime, err := strconv.ParseUint(fields[19], 10, 64)
	if err != nil {
		return 0, 0, fmt.Errorf("Invalid /proc stat start time: %w", err)
	}
	return parentPID, startTime, nil
}
