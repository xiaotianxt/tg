package keys

import (
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"

	"github.com/xiaotianxt/tg/internal/paths"
)

// writeAtomicPrivate writes content to a fresh 0600 temp file in path's
// directory, fsyncs it, renames it over path, then re-asserts 0600 and hands
// the file back to the invoking user under sudo. The temp file is removed on
// any failure before the rename.
func writeAtomicPrivate(path string, content []byte) error {
	dir := filepath.Dir(path)
	if dir != "" && dir != "." {
		if err := paths.EnsurePrivateDir(dir); err != nil {
			return fmt.Errorf("Cannot create private directory %s: %w", dir, err)
		}
	}
	tmp, err := os.CreateTemp(dir, ".tg-*.tmp")
	if err != nil {
		return fmt.Errorf("Cannot create temporary keys file in %s: %w", dir, err)
	}
	tmpName := tmp.Name()
	// os.CreateTemp already makes the file 0600; assert it in case umask or
	// the platform widened it.
	if err := os.Chmod(tmpName, 0o600); err != nil {
		tmp.Close()
		os.Remove(tmpName)
		return fmt.Errorf("Cannot restrict temporary keys file %s: %w", tmpName, err)
	}
	if _, err := tmp.Write(content); err != nil {
		tmp.Close()
		os.Remove(tmpName)
		return fmt.Errorf("Cannot write keys file %s: %w", tmpName, err)
	}
	if err := tmp.Sync(); err != nil {
		tmp.Close()
		os.Remove(tmpName)
		return fmt.Errorf("Cannot flush keys file %s: %w", tmpName, err)
	}
	if err := tmp.Close(); err != nil {
		os.Remove(tmpName)
		return fmt.Errorf("Cannot close keys file %s: %w", tmpName, err)
	}
	if err := os.Rename(tmpName, path); err != nil {
		os.Remove(tmpName)
		return fmt.Errorf("Cannot install keys file at %s: %w", path, err)
	}
	if err := os.Chmod(path, 0o600); err != nil {
		return fmt.Errorf("Cannot restrict keys file permissions for %s: %w", path, err)
	}
	if err := paths.RestoreInvokingUserOwnership(path); err != nil {
		return fmt.Errorf("Cannot restore keys file ownership for %s: %w", path, err)
	}
	return nil
}

// decodeHexKey decodes a 64-char hex string into a 32-byte key.
func decodeHexKey(s string) (*[32]byte, error) {
	if hex.DecodedLen(len(s)) != 32 {
		return nil, fmt.Errorf("key is not 32 bytes")
	}
	var key [32]byte
	if _, err := hex.Decode(key[:], []byte(s)); err != nil {
		return nil, err
	}
	return &key, nil
}
