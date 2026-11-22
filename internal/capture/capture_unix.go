//go:build darwin || linux

package capture

import (
	"fmt"
	"os"

	"github.com/xiaotianxt/tg/internal/keys"
)

const capturedMaterialLen = 32

// writePrivateNewFile creates path exclusively with mode 0600, writes
// content, and fsyncs it. It fails if the path already exists, so a stale or
// attacker-planted file is never reused.
func writePrivateNewFile(path string, content []byte) error {
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return fmt.Errorf("Cannot create private capture file %s: %w", path, err)
	}
	if _, err := f.Write(content); err != nil {
		f.Close()
		return fmt.Errorf("Cannot write private capture file %s: %w", path, err)
	}
	if err := f.Sync(); err != nil {
		f.Close()
		return fmt.Errorf("Cannot flush private capture file %s: %w", path, err)
	}
	if err := f.Close(); err != nil {
		return fmt.Errorf("Cannot close private capture file %s: %w", path, err)
	}
	return nil
}

// readCapturedMaterial reads exactly the expected number of captured bytes,
// treating any other length as a failed capture.
func readCapturedMaterial(path string) (*keys.Material, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("cannot read captured key material: %w", err)
	}
	defer clear(data)
	if len(data) != capturedMaterialLen {
		return nil, fmt.Errorf("expected %d captured bytes, found %d", capturedMaterialLen, len(data))
	}
	var b [capturedMaterialLen]byte
	copy(b[:], data)
	return keys.MaterialFromBytes(b), nil
}
