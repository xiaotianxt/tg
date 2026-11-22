//go:build darwin

package capture

import (
	"bufio"
	"encoding/json"
	"os"
)

// summarizeCaptureLog folds the JSONL capture log into counts for a
// diagnostic line: how many breakpoint locations were installed, how many
// hits were ignored for not matching the KDF call shape, and whether material
// was captured.
func summarizeCaptureLog(path string) (locations int, ignored int, captured bool) {
	f, err := os.Open(path)
	if err != nil {
		return 0, 0, false
	}
	defer f.Close()
	scanner := bufio.NewScanner(f)
	scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)
	for scanner.Scan() {
		var rec struct {
			Event     string `json:"event"`
			Locations int    `json:"locations"`
		}
		if json.Unmarshal(scanner.Bytes(), &rec) != nil {
			continue
		}
		switch rec.Event {
		case "installed":
			locations = rec.Locations
		case "ignored":
			ignored++
		case "captured":
			captured = true
		}
	}
	return locations, ignored, captured
}

// lldbCaptureScript installs a breakpoint on the system PBKDF2 routine and,
// when the call shape matches the database key derivation (algorithm, 32-byte
// password, 16-byte salt, PRF 5, 256000 rounds, 32-byte output), writes the
// 32-byte password — the account key material — and detaches. Ported verbatim
// from the Rust implementation.
const lldbCaptureScript = `
import json
import os

import lldb

OUTPUT_PATH = os.environ["TG_LLDB_CAPTURE_OUTPUT"]
LOG_PATH = os.environ["TG_LLDB_CAPTURE_LOG"]


def _log(record):
    with open(LOG_PATH, "a", encoding="utf-8") as output:
        output.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
        output.flush()


def _register(frame, name):
    register = frame.FindRegister(name)
    if not register or not register.IsValid():
        return 0
    return register.GetValueAsUnsigned()


def _stack_u64(process, address):
    error = lldb.SBError()
    raw = process.ReadMemory(address, 8, error)
    if not error.Success() or not raw or len(raw) != 8:
        return 0
    return int.from_bytes(raw, byteorder="little", signed=False)


def _stack_u32(process, address):
    error = lldb.SBError()
    raw = process.ReadMemory(address, 4, error)
    if not error.Success() or not raw or len(raw) != 4:
        return 0
    return int.from_bytes(raw, byteorder="little", signed=False)


def _call_shape(frame):
    process = frame.GetThread().GetProcess()
    triple = process.GetTarget().GetTriple().lower()
    if "arm64" in triple or "aarch64" in triple:
        algorithm = _register(frame, "x0")
        password_ptr = _register(frame, "x1")
        password_len = _register(frame, "x2")
        salt_len = _register(frame, "x4")
        prf = _register(frame, "x5")
        rounds = _register(frame, "x6")
        derived_key_len = _stack_u64(process, _register(frame, "sp"))
    elif "x86_64" in triple:
        algorithm = _register(frame, "rdi")
        password_ptr = _register(frame, "rsi")
        password_len = _register(frame, "rdx")
        salt_len = _register(frame, "r8")
        prf = _register(frame, "r9")
        stack_pointer = _register(frame, "rsp")
        rounds = _stack_u32(process, stack_pointer + 8)
        derived_key_len = _stack_u64(process, stack_pointer + 24)
    else:
        return None

    matches = (
        algorithm == 2
        and password_len == 32
        and salt_len == 16
        and prf == 5
        and rounds == 256000
        and derived_key_len == 32
    )
    return process, password_ptr, password_len, matches


def on_pbkdf2(frame, bp_loc, internal_dict):
    shape = _call_shape(frame)
    if shape is None:
        _log({"event": "ignored", "reason": "unsupported_arch"})
        return False
    process, password_ptr, password_len, matches = shape
    if not matches or not password_ptr:
        _log({"event": "ignored", "reason": "shape"})
        return False

    error = lldb.SBError()
    material = process.ReadMemory(password_ptr, int(password_len), error)
    if not error.Success() or not material or len(material) != 32:
        _log({"event": "ignored", "reason": "read"})
        return False

    with open(OUTPUT_PATH, "wb") as output:
        output.write(material)
        output.flush()
    _log({"event": "captured"})
    process.Detach()
    os._exit(0)


def install(debugger, command, result, internal_dict):
    open(LOG_PATH, "w", encoding="utf-8").close()
    target = debugger.GetSelectedTarget()
    breakpoint = target.BreakpointCreateByName("CCKeyDerivationPBKDF")
    breakpoint.SetScriptCallbackFunction("capture_account_material.on_pbkdf2")
    breakpoint.SetAutoContinue(True)
    locations = breakpoint.GetNumLocations()
    _log({"event": "installed", "locations": locations})
    if locations == 0:
        result.SetError("cannot resolve CCKeyDerivationPBKDF")
        return
    result.PutCString("installed PBKDF2 account material capture")


def __lldb_init_module(debugger, internal_dict):
    debugger.HandleCommand("command script add -f capture_account_material.install capture_tg_account_material")
`
