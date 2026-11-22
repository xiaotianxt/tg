// Package dict holds upstream names that public source keeps masked. Each
// entry is XOR-encoded so the plain names never appear in the repository;
// callers get decoded strings.
package dict

import (
	"path/filepath"
	"runtime"
	"strings"
)

const mask = 0x5a

func decode(encoded ...byte) string {
	out := make([]byte, len(encoded))
	for i, b := range encoded {
		out[i] = b ^ mask
	}
	return string(out)
}

var (
	// AppDisplayName is the app's name as people write it on every platform.
	AppDisplayName = decode(13, 63, 25, 50, 59, 46)
	// AppLocalizedName is the app's localized name.
	AppLocalizedName = decode(191, 228, 244, 190, 229, 251)
	containerID      = decode(57, 53, 55, 116, 46, 63, 52, 57, 63, 52, 46, 116, 34, 51, 52, 13, 63, 25, 50, 59, 46)
	accountFilesDir  = decode(34, 45, 63, 57, 50, 59, 46, 5, 60, 51, 54, 63, 41)
	// AccountIDPrefix starts every personal account username.
	AccountIDPrefix = decode(45, 34, 51, 62, 5)
	// StickerMagic starts the upstream sticker container format.
	StickerMagic = []byte(decode(45, 34, 61, 60))
	// CipherConfigAnchor locates the cipher configuration in the Linux binary.
	CipherConfigAnchor = []byte(decode(57, 53, 55, 116, 14, 63, 52, 57, 63, 52, 46, 116, 13, 25, 30, 24, 116, 25, 53, 52, 60, 51, 61, 116, 25, 51, 42, 50, 63, 40))
	linuxConfigDir     = decode(116, 34, 45, 63, 57, 50, 59, 46)

	// Message table columns.
	MsgBodyColumn              = decode(55, 63, 41, 41, 59, 61, 63, 5, 57, 53, 52, 46, 63, 52, 46)
	MsgCompressionMarkerColumn = decode(13, 25, 30, 24, 5, 25, 14, 5, 55, 63, 41, 41, 59, 61, 63, 5, 57, 53, 52, 46, 63, 52, 46)
	MsgSenderColumn            = decode(40, 63, 59, 54, 5, 41, 63, 52, 62, 63, 40, 5, 51, 62)
	MsgPackedMetaColumn        = decode(42, 59, 57, 49, 63, 62, 5, 51, 52, 60, 53, 5, 62, 59, 46, 59)
)

// DesktopAppProcess is the client's process name on this platform.
func DesktopAppProcess() string {
	if runtime.GOOS == "linux" {
		return decode(45, 63, 57, 50, 59, 46)
	}
	return AppDisplayName
}

// RenderAppNames replaces the public cover name in text shown only to the
// owner's own tools (the installed skill, MCP metadata) with the real app
// name, so source and public docs keep the cover name.
func RenderAppNames(template string) string {
	r := strings.NewReplacer(
		"Telegram聊天", AppLocalizedName+"聊天",
		"Telegram群", AppLocalizedName+"群",
		"Telegram里", AppLocalizedName+"里",
		"Telegram", AppDisplayName,
		"telegram", strings.ToLower(AppDisplayName),
	)
	return r.Replace(template)
}

func containerDataDir(home string) string {
	return filepath.Join(home, "Library/Containers", containerID, "Data")
}

func documentsAccountFilesDir(home string) string {
	return filepath.Join(containerDataDir(home), "Documents", accountFilesDir)
}

func linuxAccountFilesDir(home string) string {
	return filepath.Join(home, "Documents", accountFilesDir)
}

// AccountFilesCandidateDirs lists where account directories live, the
// platform's native location first.
func AccountFilesCandidateDirs(home string) []string {
	if runtime.GOOS == "linux" {
		return []string{linuxAccountFilesDir(home), documentsAccountFilesDir(home)}
	}
	return []string{documentsAccountFilesDir(home), linuxAccountFilesDir(home)}
}

// AppSupportDir is the older macOS location of versioned account directories.
func AppSupportDir(home string) string {
	return filepath.Join(containerDataDir(home), "Library/Application Support", containerID)
}

// KVCommCandidateDirs lists where the client keeps its kvcomm cache, the
// platform's native location first.
func KVCommCandidateDirs(home string) []string {
	mac := filepath.Join(containerDataDir(home), "Documents/app_data/net/kvcomm")
	linux := filepath.Join(home, linuxConfigDir, "net/kvcomm")
	if runtime.GOOS == "linux" {
		return []string{linux, mac}
	}
	return []string{mac, linux}
}

var (
	// NativeVoiceHeader starts a native voice payload.
	NativeVoiceHeader = []byte(decode(121, 123, 9, 19, 22, 17, 5, 12, 105))
	// NativeVoiceDecoderCommand is the Homebrew native voice decoder's
	// command name.
	NativeVoiceDecoderCommand = "rust-" + decode(41, 51, 54, 49)
)
