package main

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"encoding/binary"
	"errors"
	"fmt"
	"hash/fnv"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"runtime"
	"strings"
)

const (
	version   = "0.1.0"
	magic     = "GLMBNDL1"
	footerLen = int64(32)
)

const macOSLauncher = `#!/bin/sh
set -eu

bundle_hash="@@BUNDLE_HASH@@"
erts_version="@@ERTS_VERSION@@"
cache_root="${XDG_CACHE_HOME:-${HOME:?HOME must be set}/Library/Caches}/gleam-bundler"
cache="$cache_root/$bundle_hash"
ready="$cache/.ready"
lock="$cache_root/.$bundle_hash.lock"

if [ ! -f "$ready" ]; then
  /bin/mkdir -p "$cache_root"
  owns_lock=
  lock_wait=0
  missing_pid_wait=0
  while [ ! -f "$ready" ]; do
    if /bin/mkdir "$lock" 2>/dev/null; then
      owns_lock=1
      break
    fi
    lock_wait=$((lock_wait + 1))
    if [ -r "$lock/pid" ]; then
      lock_pid=
      IFS= read -r lock_pid < "$lock/pid" || true
      missing_pid_wait=0
      if [ -z "$lock_pid" ] || ! kill -0 "$lock_pid" 2>/dev/null; then
        /bin/rm -rf "$lock"
        continue
      fi
    else
      missing_pid_wait=$((missing_pid_wait + 1))
      if [ "$missing_pid_wait" -ge 2 ]; then
        /bin/rm -rf "$lock"
        missing_pid_wait=0
        continue
      fi
    fi
    if [ "$lock_wait" -ge 300 ]; then
      /bin/rm -rf "$lock"
      lock_wait=0
      continue
    fi
    /bin/sleep 1
  done

  if [ "$owns_lock" = 1 ]; then
    temp="$cache_root/.$bundle_hash.$$.tmp"
    cleanup() {
      /bin/rm -rf "$temp"
      /bin/rm -rf "$lock"
    }
    trap cleanup EXIT HUP INT TERM
    printf '%s\n' "$$" > "$lock/pid"

    if [ ! -f "$ready" ]; then
      /bin/rm -rf "$temp" "$cache"
      /bin/mkdir "$temp"
      payload_line=$(/usr/bin/awk '/^__GLEAM_BUNDLER_PAYLOAD__$/ { print NR + 1; exit }' "$0")
      /usr/bin/tail -n +"$payload_line" "$0" | /usr/bin/tar -xzf - -C "$temp"
      : > "$temp/.ready"
      /bin/mv "$temp" "$cache"
    fi

    /bin/rm -rf "$lock"
    trap - EXIT HUP INT TERM
  fi
fi

erlang_root="$cache/erlang"
bindir="$erlang_root/erts-$erts_version/bin"
export ROOTDIR="$erlang_root"
export ERL_ROOTDIR="$erlang_root"
export BINDIR="$bindir"
unset ERLEXEC_DIR
exec "$bindir/escript" "$cache/app" "$@"
exit 1
__GLEAM_BUNDLER_PAYLOAD__
`

type footer struct {
	offset uint64
	length uint64
	hash   uint64
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "gleam-bundler:", err)
		os.Exit(1)
	}
}

func run() error {
	executablePath, err := os.Executable()
	if err != nil {
		return err
	}
	executable, err := os.Open(executablePath)
	if err != nil {
		return err
	}
	defer executable.Close()

	bundleFooter, bundled, err := readFooter(executable)
	if err != nil {
		return err
	}
	if bundled {
		return launch(executable, bundleFooter)
	}
	if err := executable.Close(); err != nil {
		return err
	}
	return buildBundle(executablePath)
}

func buildBundle(currentExecutable string) error {
	project, output, stop, err := parseArguments(os.Args[1:])
	if err != nil || stop {
		return err
	}

	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return err
	}
	project, err = filepath.Abs(project)
	if err != nil {
		return err
	}
	config, err := os.ReadFile(filepath.Join(project, "gleam.toml"))
	if err != nil {
		return err
	}
	packageName, err := packageName(string(config))
	if err != nil {
		return err
	}

	escript := filepath.Join(project, packageName)
	windowsWrapper := escript + ".cmd"
	if err := ensureAbsent(escript); err != nil {
		return err
	}
	if runtime.GOOS == "windows" {
		if err := ensureAbsent(windowsWrapper); err != nil {
			return err
		}
	}
	defer os.Remove(escript)
	if runtime.GOOS == "windows" {
		defer os.Remove(windowsWrapper)
	}

	command := exec.Command("gleam", "export", "escript")
	command.Dir = project
	command.Stdin = os.Stdin
	command.Stdout = os.Stdout
	command.Stderr = os.Stderr
	if err := command.Run(); err != nil {
		return fmt.Errorf("gleam export escript failed: %w", err)
	}
	if !regularFile(escript) {
		return fmt.Errorf("gleam did not create the expected escript at %s", escript)
	}

	erlangRoot, ertsVersion, err := erlangInstallation()
	if err != nil {
		return err
	}
	if output == "" {
		name := packageName
		if runtime.GOOS == "windows" {
			name += ".exe"
		}
		output = filepath.Join(project, "build", "gleam-bundler", name)
	} else {
		if !hasOutputFilename(output) {
			return errors.New("the output path has no filename")
		}
		output = trimTrailingSeparators(output)
		if !filepath.IsAbs(output) {
			if runtime.GOOS == "windows" && (filepath.VolumeName(output) != "" || isPathSeparator(output[0])) {
				output, err = filepath.Abs(output)
				if err != nil {
					return err
				}
			} else {
				workingDirectory, err := os.Getwd()
				if err != nil {
					return err
				}
				output = workingDirectory + string(filepath.Separator) + output
			}
		}
	}

	parent, filename := filepath.Split(output)
	if filename == "" {
		return errors.New("the output path has no filename")
	}
	if err := os.MkdirAll(parent, 0o755); err != nil {
		return err
	}
	parent, err = filepath.EvalSymlinks(parent)
	if err != nil {
		return err
	}
	output = filepath.Join(parent, filename)
	aliasesEscript, err := pathsAlias(output, escript)
	if err != nil {
		return err
	}
	aliasesWrapper := false
	if runtime.GOOS == "windows" {
		aliasesWrapper, err = pathsAlias(output, windowsWrapper)
		if err != nil {
			return err
		}
	}
	if aliasesEscript || aliasesWrapper {
		return errors.New("the output path cannot replace the escript")
	}

	temporary, err := os.CreateTemp(parent, ".gleam-bundler-*.tmp")
	if err != nil {
		return err
	}
	temporaryPath := temporary.Name()
	published := false
	defer func() {
		temporary.Close()
		if !published {
			os.Remove(temporaryPath)
		}
	}()

	if err := temporary.Chmod(0o755); err != nil {
		return err
	}
	if err := writeBundle(currentExecutable, temporary, escript, erlangRoot, ertsVersion); err != nil {
		return err
	}
	if err := temporary.Close(); err != nil {
		return err
	}
	if err := publishBundle(temporaryPath, output); err != nil {
		return err
	}
	published = true

	info, err := os.Stat(output)
	if err != nil {
		return err
	}
	fmt.Printf("Bundled %s (%.1f MiB)\n", output, float64(info.Size())/1024/1024)
	return nil
}

func parseArguments(arguments []string) (project, output string, stop bool, err error) {
	project = "."
	if len(arguments) == 0 || arguments[0] == "-h" || arguments[0] == "--help" {
		printHelp()
		return project, output, true, nil
	}
	if arguments[0] == "-V" || arguments[0] == "--version" {
		fmt.Println("gleam-bundler", version)
		return project, output, true, nil
	}
	if arguments[0] != "bundle" {
		return project, output, false, fmt.Errorf("unknown command: %s", arguments[0])
	}

	projectSet := false
	for index := 1; index < len(arguments); index++ {
		argument := arguments[index]
		switch argument {
		case "-h", "--help":
			printHelp()
			return project, output, true, nil
		case "-V", "--version":
			fmt.Println("gleam-bundler", version)
			return project, output, true, nil
		case "-o", "--out":
			index++
			if index == len(arguments) {
				return project, output, false, errors.New("--out requires a path")
			}
			output = arguments[index]
		default:
			if strings.HasPrefix(argument, "-") {
				return project, output, false, fmt.Errorf("unknown option: %s", argument)
			}
			if projectSet {
				return project, output, false, errors.New("only one project directory can be given")
			}
			project = argument
			projectSet = true
		}
	}
	return project, output, false, nil
}

func writeBundle(currentExecutable string, output *os.File, escript, erlangRoot, ertsVersion string) error {
	if runtime.GOOS == "darwin" {
		if err := writePayload(output, escript, erlangRoot, ertsVersion); err != nil {
			return err
		}
		payloadLength, err := output.Seek(0, io.SeekCurrent)
		if err != nil {
			return err
		}
		payloadHash, err := hashRange(output, 0, payloadLength)
		if err != nil {
			return err
		}
		launcher := strings.ReplaceAll(macOSLauncher, "@@BUNDLE_HASH@@", fmt.Sprintf("%016x-%x", payloadHash, payloadLength))
		launcher = strings.ReplaceAll(launcher, "@@ERTS_VERSION@@", ertsVersion)
		if err := prepend(output, []byte(launcher), payloadLength); err != nil {
			return err
		}
		return output.Sync()
	}

	executable, err := os.Open(currentExecutable)
	if err != nil {
		return err
	}
	if _, err := io.Copy(output, executable); err != nil {
		executable.Close()
		return err
	}
	if err := executable.Close(); err != nil {
		return err
	}
	payloadOffset, err := output.Seek(0, io.SeekCurrent)
	if err != nil {
		return err
	}
	if err := writePayload(output, escript, erlangRoot, ertsVersion); err != nil {
		return err
	}
	end, err := output.Seek(0, io.SeekCurrent)
	if err != nil {
		return err
	}
	payloadLength := end - payloadOffset
	payloadHash, err := hashRange(output, payloadOffset, payloadLength)
	if err != nil {
		return err
	}
	if _, err := output.Write(footer{uint64(payloadOffset), uint64(payloadLength), payloadHash}.encode()); err != nil {
		return err
	}
	return output.Sync()
}

func writePayload(output io.Writer, escript, erlangRoot, ertsVersion string) error {
	gzipWriter := gzip.NewWriter(output)
	tarWriter := tar.NewWriter(gzipWriter)
	if err := appendTree(tarWriter, erlangRoot, "erlang", map[string]bool{}); err != nil {
		tarWriter.Close()
		gzipWriter.Close()
		return err
	}
	if err := appendTree(tarWriter, escript, "app", map[string]bool{}); err != nil {
		tarWriter.Close()
		gzipWriter.Close()
		return err
	}
	if err := tarWriter.WriteHeader(&tar.Header{Name: "erts-version", Mode: 0o644, Size: int64(len(ertsVersion))}); err != nil {
		tarWriter.Close()
		gzipWriter.Close()
		return err
	}
	if _, err := io.WriteString(tarWriter, ertsVersion); err != nil {
		tarWriter.Close()
		gzipWriter.Close()
		return err
	}
	if err := tarWriter.Close(); err != nil {
		gzipWriter.Close()
		return err
	}
	return gzipWriter.Close()
}

func appendTree(writer *tar.Writer, source, archiveName string, activeDirectories map[string]bool) error {
	info, err := os.Stat(source)
	if err != nil {
		return err
	}
	if !info.IsDir() && !info.Mode().IsRegular() {
		return fmt.Errorf("cannot archive unsupported file %s", source)
	}

	if info.IsDir() {
		resolved, err := filepath.EvalSymlinks(source)
		if err != nil {
			return err
		}
		key := resolved
		if runtime.GOOS == "windows" {
			key = strings.ToLower(key)
		}
		if activeDirectories[key] {
			return fmt.Errorf("cannot archive symlink cycle at %s", source)
		}
		activeDirectories[key] = true
		defer delete(activeDirectories, key)
	}

	header, err := tar.FileInfoHeader(info, "")
	if err != nil {
		return err
	}
	header.Name = path.Clean(filepath.ToSlash(archiveName))
	if info.IsDir() {
		header.Name += "/"
	}
	if err := writer.WriteHeader(header); err != nil {
		return err
	}
	if info.Mode().IsRegular() {
		file, err := os.Open(source)
		if err != nil {
			return err
		}
		_, copyErr := io.CopyN(writer, file, info.Size())
		closeErr := file.Close()
		if copyErr != nil {
			return copyErr
		}
		return closeErr
	}

	entries, err := os.ReadDir(source)
	if err != nil {
		return err
	}
	for _, entry := range entries {
		if err := appendTree(writer, filepath.Join(source, entry.Name()), path.Join(archiveName, entry.Name()), activeDirectories); err != nil {
			return err
		}
	}
	return nil
}

func prepend(file *os.File, prefix []byte, originalLength int64) error {
	if err := file.Truncate(originalLength + int64(len(prefix))); err != nil {
		return err
	}
	buffer := make([]byte, 64*1024)
	for remaining := originalLength; remaining > 0; {
		length := min(remaining, int64(len(buffer)))
		source := remaining - length
		if _, err := file.ReadAt(buffer[:length], source); err != nil {
			return err
		}
		if _, err := file.WriteAt(buffer[:length], source+int64(len(prefix))); err != nil {
			return err
		}
		remaining = source
	}
	_, err := file.WriteAt(prefix, 0)
	return err
}

func launch(executable *os.File, bundleFooter footer) error {
	cacheParent, err := cacheParent()
	if err != nil {
		return err
	}
	cache := filepath.Join(cacheParent, fmt.Sprintf("%016x-%x", bundleFooter.hash, bundleFooter.length))
	ready := filepath.Join(cache, ".ready")
	if !regularFile(ready) {
		payloadHash, err := hashRange(executable, int64(bundleFooter.offset), int64(bundleFooter.length))
		if err != nil {
			return err
		}
		if payloadHash != bundleFooter.hash {
			return errors.New("the embedded payload is corrupt")
		}
		if err := os.MkdirAll(cacheParent, 0o755); err != nil {
			return err
		}
		if _, err := os.Stat(cache); err == nil && !regularFile(ready) {
			if err := os.RemoveAll(cache); err != nil {
				return err
			}
		} else if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}

		temporary, err := os.MkdirTemp(cacheParent, fmt.Sprintf(".%016x-*.tmp", bundleFooter.hash))
		if err != nil {
			return err
		}
		defer os.RemoveAll(temporary)
		payload := io.NewSectionReader(executable, int64(bundleFooter.offset), int64(bundleFooter.length))
		if err := unpack(payload, temporary); err != nil {
			return err
		}
		if err := prepareRuntime(temporary); err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(temporary, ".ready"), nil, 0o644); err != nil {
			return err
		}
		if err := os.Rename(temporary, cache); err != nil {
			if !regularFile(ready) {
				return err
			}
			if err := os.RemoveAll(temporary); err != nil {
				return err
			}
		}
	}

	erlangRoot := filepath.Join(cache, "erlang")
	versionBytes, err := os.ReadFile(filepath.Join(cache, "erts-version"))
	if err != nil {
		return err
	}
	bin := filepath.Join(erlangRoot, "erts-"+string(versionBytes), "bin")
	escript := filepath.Join(bin, escriptExecutable())
	app := filepath.Join(cache, "app")
	if !regularFile(escript) || !regularFile(app) {
		return errors.New("the extracted bundle is incomplete")
	}

	command := exec.Command(escript, append([]string{app}, os.Args[1:]...)...)
	command.Stdin = os.Stdin
	command.Stdout = os.Stdout
	command.Stderr = os.Stderr
	command.Env = withoutEnvironment(os.Environ(), "ROOTDIR", "ERL_ROOTDIR", "BINDIR", "ERLEXEC_DIR")
	command.Env = append(command.Env, "ROOTDIR="+erlangRoot, "ERL_ROOTDIR="+erlangRoot, "BINDIR="+bin)
	if err := executable.Close(); err != nil {
		return err
	}
	return launchCommand(command)
}

func unpack(input io.Reader, destination string) error {
	gzipReader, err := gzip.NewReader(input)
	if err != nil {
		return err
	}
	tarReader := tar.NewReader(gzipReader)
	type directory struct {
		path string
		mode fs.FileMode
	}
	var directories []directory
	for {
		header, err := tarReader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		name := strings.TrimSuffix(header.Name, "/")
		if name == "" || name == "." || !fs.ValidPath(name) {
			return fmt.Errorf("invalid archive path %q", header.Name)
		}
		localName, err := filepath.Localize(name)
		if err != nil {
			return err
		}
		target := filepath.Join(destination, localName)
		mode := fs.FileMode(header.Mode).Perm()
		switch header.Typeflag {
		case tar.TypeDir:
			if err := os.MkdirAll(target, 0o755); err != nil {
				return err
			}
			directories = append(directories, directory{target, mode})
		case tar.TypeReg, tar.TypeRegA:
			if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
				return err
			}
			file, err := os.OpenFile(target, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
			if err != nil {
				return err
			}
			_, copyErr := io.Copy(file, tarReader)
			closeErr := file.Close()
			if copyErr != nil {
				return copyErr
			}
			if closeErr != nil {
				return closeErr
			}
			if err := os.Chmod(target, mode); err != nil {
				return err
			}
		default:
			return fmt.Errorf("unsupported archive entry %q", header.Name)
		}
	}
	if _, err := io.Copy(io.Discard, gzipReader); err != nil {
		return err
	}
	if err := gzipReader.Close(); err != nil {
		return err
	}
	for index := len(directories) - 1; index >= 0; index-- {
		if err := os.Chmod(directories[index].path, directories[index].mode); err != nil {
			return err
		}
	}
	return nil
}

func erlangInstallation() (string, string, error) {
	command := exec.Command(
		"erl",
		"-boot", "no_dot_erlang",
		"-noshell",
		"-noinput",
		"-eval", `io:format("GLEAM_BUNDLER_ROOT=~ts~nGLEAM_BUNDLER_ERTS=~s~n", [code:root_dir(), erlang:system_info(version)]), halt().`,
	)
	command.Env = withoutEnvironment(os.Environ(), "ERL_AFLAGS", "ERL_FLAGS", "ERL_ZFLAGS")
	output, err := command.Output()
	if err != nil {
		var exitError *exec.ExitError
		if errors.As(err, &exitError) {
			return "", "", fmt.Errorf("could not inspect the Erlang installation: %s", strings.TrimSpace(string(exitError.Stderr)))
		}
		return "", "", fmt.Errorf("could not run Erlang: %w", err)
	}

	var root, ertsVersion string
	for _, line := range strings.Split(string(output), "\n") {
		line = strings.TrimSuffix(line, "\r")
		if value, found := strings.CutPrefix(line, "GLEAM_BUNDLER_ROOT="); found {
			root = value
		}
		if value, found := strings.CutPrefix(line, "GLEAM_BUNDLER_ERTS="); found {
			ertsVersion = value
		}
	}
	if root == "" {
		return "", "", errors.New("Erlang did not report its root directory")
	}
	if ertsVersion == "" {
		return "", "", errors.New("Erlang did not report its ERTS version")
	}
	root, err = filepath.EvalSymlinks(root)
	if err != nil {
		return "", "", err
	}
	if !regularFile(filepath.Join(root, "erts-"+ertsVersion, "bin", escriptExecutable())) {
		return "", "", errors.New("could not find escript in the Erlang installation")
	}
	return root, ertsVersion, nil
}

func cacheParent() (string, error) {
	if runtime.GOOS == "windows" {
		if value := os.Getenv("LOCALAPPDATA"); value != "" {
			return filepath.Join(value, "gleam-bundler"), nil
		}
		if value := os.Getenv("USERPROFILE"); value != "" {
			return filepath.Join(value, "AppData", "Local", "gleam-bundler"), nil
		}
		return "", errors.New("LOCALAPPDATA or USERPROFILE must be set")
	}
	if value := os.Getenv("XDG_CACHE_HOME"); value != "" {
		return filepath.Join(value, "gleam-bundler"), nil
	}
	home := os.Getenv("HOME")
	if home == "" {
		return "", errors.New("HOME or XDG_CACHE_HOME must be set")
	}
	if runtime.GOOS == "darwin" {
		return filepath.Join(home, "Library", "Caches", "gleam-bundler"), nil
	}
	return filepath.Join(home, ".cache", "gleam-bundler"), nil
}

func prepareRuntime(bundle string) error {
	if runtime.GOOS != "windows" {
		return nil
	}
	versionBytes, err := os.ReadFile(filepath.Join(bundle, "erts-version"))
	if err != nil {
		return err
	}
	erlINI := filepath.Join(bundle, "erlang", "erts-"+string(versionBytes), "bin", "erl.ini")
	if err := os.Remove(erlINI); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return nil
}

func packageName(config string) (string, error) {
	for _, line := range strings.Split(config, "\n") {
		line = strings.TrimSpace(line)
		if strings.HasPrefix(line, "[") {
			break
		}
		key, value, found := strings.Cut(line, "=")
		if !found || strings.TrimSpace(key) != "name" {
			continue
		}
		value = strings.TrimSpace(value)
		if len(value) == 0 || value[0] != '"' && value[0] != '\'' {
			return "", errors.New("the package name must be quoted")
		}
		quote := value[0]
		end := strings.IndexByte(value[1:], quote)
		if end < 0 {
			return "", errors.New("the package name is not terminated")
		}
		name := value[1 : end+1]
		if name == "" {
			return "", errors.New("the package name is invalid")
		}
		for index := range len(name) {
			character := name[index]
			if character != '_' && (character < 'a' || character > 'z') && (character < '0' || character > '9') {
				return "", errors.New("the package name is invalid")
			}
		}
		return name, nil
	}
	return "", errors.New("gleam.toml has no top-level package name")
}

func ensureAbsent(name string) error {
	_, err := os.Lstat(name)
	if err == nil {
		return fmt.Errorf("refusing to replace the existing path %s", name)
	}
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	return err
}

func pathsAlias(left, right string) (bool, error) {
	leftInfo, leftErr := os.Stat(left)
	rightInfo, rightErr := os.Stat(right)
	if rightErr != nil {
		return false, rightErr
	}
	if leftErr == nil {
		return os.SameFile(leftInfo, rightInfo), nil
	}
	if !errors.Is(leftErr, os.ErrNotExist) {
		return false, leftErr
	}
	left = filepath.Clean(left)
	right = filepath.Clean(right)
	if runtime.GOOS == "windows" {
		return strings.EqualFold(left, right), nil
	}
	return left == right, nil
}

func hasOutputFilename(name string) bool {
	volumeLength := len(filepath.VolumeName(name))
	end := len(name)
	for end > volumeLength && isPathSeparator(name[end-1]) {
		end--
	}
	if end <= volumeLength {
		return false
	}
	start := end
	for start > volumeLength && !isPathSeparator(name[start-1]) {
		start--
	}
	component := name[start:end]
	return component != "." && component != ".."
}

func trimTrailingSeparators(name string) string {
	volumeLength := len(filepath.VolumeName(name))
	end := len(name)
	for end > volumeLength && isPathSeparator(name[end-1]) {
		end--
	}
	return name[:end]
}

func isPathSeparator(character byte) bool {
	return character == byte(filepath.Separator) || runtime.GOOS == "windows" && character == '/'
}

func publishBundle(temporary, output string) error {
	if runtime.GOOS == "windows" {
		if info, err := os.Lstat(output); err == nil && info.IsDir() {
			return fmt.Errorf("refusing to replace the directory %s", output)
		} else if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		if err := os.Remove(output); err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
	}
	return os.Rename(temporary, output)
}

func regularFile(name string) bool {
	info, err := os.Stat(name)
	return err == nil && info.Mode().IsRegular()
}

func escriptExecutable() string {
	if runtime.GOOS == "windows" {
		return "escript.exe"
	}
	return "escript"
}

func withoutEnvironment(environment []string, names ...string) []string {
	filtered := make([]string, 0, len(environment))
	for _, entry := range environment {
		key, _, found := strings.Cut(entry, "=")
		remove := false
		if found {
			for _, name := range names {
				if key == name || runtime.GOOS == "windows" && strings.EqualFold(key, name) {
					remove = true
					break
				}
			}
		}
		if !remove {
			filtered = append(filtered, entry)
		}
	}
	return filtered
}

func readFooter(reader io.ReadSeeker) (footer, bool, error) {
	length, err := reader.Seek(0, io.SeekEnd)
	if err != nil {
		return footer{}, false, err
	}
	if length < footerLen {
		return footer{}, false, nil
	}
	if _, err := reader.Seek(-footerLen, io.SeekEnd); err != nil {
		return footer{}, false, err
	}
	encoded := make([]byte, footerLen)
	if _, err := io.ReadFull(reader, encoded); err != nil {
		return footer{}, false, err
	}
	decoded, found := decodeFooter(encoded)
	if !found {
		return footer{}, false, nil
	}
	payloadEnd := uint64(length - footerLen)
	if decoded.offset > payloadEnd || decoded.length != payloadEnd-decoded.offset {
		return footer{}, false, errors.New("the bundle footer is invalid")
	}
	return decoded, true, nil
}

func hashRange(reader io.ReaderAt, offset, length int64) (uint64, error) {
	hash := fnv.New64a()
	written, err := io.Copy(hash, io.NewSectionReader(reader, offset, length))
	if err != nil {
		return 0, err
	}
	if written != length {
		return 0, io.ErrUnexpectedEOF
	}
	return hash.Sum64(), nil
}

func (value footer) encode() []byte {
	encoded := make([]byte, footerLen)
	binary.LittleEndian.PutUint64(encoded[0:8], value.offset)
	binary.LittleEndian.PutUint64(encoded[8:16], value.length)
	binary.LittleEndian.PutUint64(encoded[16:24], value.hash)
	copy(encoded[24:32], magic)
	return encoded
}

func decodeFooter(encoded []byte) (footer, bool) {
	if len(encoded) != int(footerLen) || !bytes.Equal(encoded[24:32], []byte(magic)) {
		return footer{}, false
	}
	return footer{
		offset: binary.LittleEndian.Uint64(encoded[0:8]),
		length: binary.LittleEndian.Uint64(encoded[8:16]),
		hash:   binary.LittleEndian.Uint64(encoded[16:24]),
	}, true
}

func printHelp() {
	fmt.Printf(`gleam-bundler %s

Bundle a Gleam CLI and the local Erlang runtime into one executable.

Usage: gleam-bundler bundle [OPTIONS] [PROJECT]

Commands:
  bundle           Build a standalone executable

Arguments:
  [PROJECT]      Gleam project directory [default: .]

Options:
  -o, --out PATH Write the executable to PATH
  -h, --help     Print help
  -V, --version  Print version
`, version)
}
