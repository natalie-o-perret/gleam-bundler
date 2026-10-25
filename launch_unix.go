//go:build !windows

package main

import (
	"os/exec"
	"syscall"
)

func launchCommand(command *exec.Cmd) error {
	return syscall.Exec(command.Path, command.Args, command.Env)
}
