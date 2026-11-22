package keys

import (
	"os"
	"path/filepath"
	"runtime"
	"testing"
)

func TestMaterialRoundTrip(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "key_material.bin")
	var raw [32]byte
	for i := range raw {
		raw[i] = 0x5a
	}
	if err := StoreMaterial(path, MaterialFromBytes(raw)); err != nil {
		t.Fatal(err)
	}
	loaded, present, err := LoadMaterial(path)
	if err != nil || !present {
		t.Fatalf("LoadMaterial = present %v, err %v", present, err)
	}
	if *loaded.Bytes() != raw {
		t.Fatalf("round-trip mismatch: %x", loaded.Bytes())
	}
	if runtime.GOOS != "windows" {
		st, err := os.Stat(path)
		if err != nil {
			t.Fatal(err)
		}
		if st.Mode().Perm() != 0o600 {
			t.Fatalf("material mode = %o, want 600", st.Mode().Perm())
		}
	}
}

func TestLoadMaterialAbsentIsNotAnError(t *testing.T) {
	_, present, err := LoadMaterial(filepath.Join(t.TempDir(), "missing.bin"))
	if err != nil || present {
		t.Fatalf("absent material = present %v, err %v", present, err)
	}
}

func TestLoadMaterialRejectsUnversionedOrTruncated(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "key_material.bin")
	// 32 bytes with no magic prefix: a bare dump, not a versioned file.
	if err := os.WriteFile(path, make([]byte, 32), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, _, err := LoadMaterial(path); err == nil {
		t.Fatal("unversioned material must be rejected")
	}
}
