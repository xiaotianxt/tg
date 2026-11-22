package keys

import (
	"bytes"
	"fmt"
	"os"
)

// materialMagic prefixes a key_material.bin file and carries its version.
var materialMagic = []byte("TGKM\x00\x00\x00\x01")

const materialLen = 32

// Material is the cached account key material: the 32-byte input to the
// database KDF, captured once from the client and stored privately.
type Material struct {
	bytes [materialLen]byte
}

// MaterialFromBytes wraps raw 32-byte material.
func MaterialFromBytes(b [materialLen]byte) *Material {
	m := &Material{}
	copy(m.bytes[:], b[:])
	return m
}

// Bytes returns the material for KDF input. The caller must not retain it
// past the Material's lifetime.
func (m *Material) Bytes() *[32]byte { return &m.bytes }

// Wipe zeroes the material in memory.
func (m *Material) Wipe() {
	if m != nil {
		clear(m.bytes[:])
	}
}

// LoadMaterial reads cached key material from path. It returns (nil, false,
// nil) when the file is absent, and an error when it is present but malformed
// (wrong magic or length), so a corrupt cache is reported rather than
// silently ignored.
func LoadMaterial(path string) (*Material, bool, error) {
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, fmt.Errorf("Cannot read key material %s: %w", path, err)
	}
	defer clear(data)
	if len(data) != len(materialMagic)+materialLen || !bytes.HasPrefix(data, materialMagic) {
		return nil, false, fmt.Errorf("Invalid key material file %s; remove it and capture again", path)
	}
	var m Material
	copy(m.bytes[:], data[len(materialMagic):])
	return &m, true, nil
}

// StoreMaterial writes material to path atomically with mode 0600.
func StoreMaterial(path string, m *Material) error {
	content := make([]byte, 0, len(materialMagic)+materialLen)
	content = append(content, materialMagic...)
	content = append(content, m.bytes[:]...)
	defer clear(content)
	return writeAtomicPrivate(path, content)
}
