//go:build !darwin

package store

const canClone = false

func cloneFile(src, dst string) error { return errCloneUnsupported }
