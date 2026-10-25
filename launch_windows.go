//go:build windows

package main

import (
	"errors"
	"os"
	"os/exec"
)

func launchCommand(command *exec.Cmd) error {
	err := command.Run()
	if err == nil {
		os.Exit(0)
	}
	var exitError *exec.ExitError
	if errors.As(err, &exitError) {
		code := exitError.ExitCode()
		if code < 0 {
			code = 1
		}
		os.Exit(code)
	}
	return err
}
