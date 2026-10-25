#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("gleam-bundler supports Linux, macOS, and Windows only");

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use std::{
    env,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{self, Command},
};
use tar::{Archive, Builder, Header};

#[cfg(unix)]
use std::os::unix::{fs::PermissionsExt, process::CommandExt};

const MAGIC: &[u8; 8] = b"GLMBNDL1";
const FOOTER_LEN: u64 = 32;
const HASH_OFFSET: u64 = 0xcbf29ce484222325;
const HASH_PRIME: u64 = 0x100000001b3;

#[cfg(target_os = "macos")]
const MACOS_LAUNCHER: &str = r#"#!/bin/sh
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
"#;

#[cfg(windows)]
const ESCRIPT_EXECUTABLE: &str = "escript.exe";
#[cfg(not(windows))]
const ESCRIPT_EXECUTABLE: &str = "escript";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Footer {
    offset: u64,
    length: u64,
    hash: u64,
}

struct GeneratedFiles(Vec<PathBuf>);

impl Drop for GeneratedFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("gleam-bundler: {error}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let current_exe = env::current_exe()?;
    let mut executable = File::open(&current_exe)?;

    if let Some(footer) = read_footer(&mut executable)? {
        return launch(executable, footer);
    }

    build_bundle(&current_exe)
}

fn build_bundle(current_exe: &Path) -> Result<()> {
    let mut project = None;
    let mut output = None;
    let mut args = env::args_os().skip(1);

    match args.next().as_deref().and_then(|arg| arg.to_str()) {
        Some("bundle") => {}
        Some("-h" | "--help") | None => {
            print_help();
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("gleam-bundler {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(command) => return Err(invalid_input(format!("unknown command: {command}"))),
    }

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-h" | "--help") => {
                print_help();
                return Ok(());
            }
            Some("-V" | "--version") => {
                println!("gleam-bundler {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some("-o" | "--out") => {
                output = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| invalid_input("--out requires a path"))?,
                ));
            }
            Some(value) if value.starts_with('-') => {
                return Err(invalid_input(format!("unknown option: {value}")));
            }
            _ if project.is_none() => project = Some(PathBuf::from(arg)),
            _ => return Err(invalid_input("only one project directory can be given")),
        }
    }

    let project = fs::canonicalize(project.unwrap_or_else(|| PathBuf::from(".")))?;
    let config = fs::read_to_string(project.join("gleam.toml"))?;
    let package_name = package_name(&config)?;
    let escript = project.join(&package_name);
    let windows_wrapper = escript.with_extension("cmd");
    ensure_absent(&escript)?;
    if cfg!(windows) {
        ensure_absent(&windows_wrapper)?;
    }
    let _generated_files = GeneratedFiles(if cfg!(windows) {
        vec![escript.clone(), windows_wrapper.clone()]
    } else {
        vec![escript.clone()]
    });

    let status = Command::new("gleam")
        .args(["export", "escript"])
        .current_dir(&project)
        .status()
        .map_err(|error| io::Error::other(format!("could not run gleam: {error}")))?;
    if !status.success() {
        return Err(invalid_input("gleam export escript failed"));
    }

    if !escript.is_file() {
        return Err(invalid_input(format!(
            "gleam did not create the expected escript at {}",
            escript.display()
        )));
    }

    let (erlang_root, erts_version) = erlang_installation()?;
    let output = output.unwrap_or_else(|| {
        project
            .join("build")
            .join("gleam-bundler")
            .join(if cfg!(windows) {
                format!("{package_name}.exe")
            } else {
                package_name.clone()
            })
    });
    let output = if output.is_absolute() {
        output
    } else {
        env::current_dir()?.join(output)
    };
    let parent = output
        .parent()
        .ok_or_else(|| invalid_input("the output path has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let parent = fs::canonicalize(parent)?;
    let filename = output
        .file_name()
        .ok_or_else(|| invalid_input("the output path has no filename"))?
        .to_owned();
    let output = parent.join(&filename);
    if paths_alias(&output, &escript)? || cfg!(windows) && paths_alias(&output, &windows_wrapper)? {
        return Err(invalid_input("the output path cannot replace the escript"));
    }

    let (temporary, temporary_file) = create_temporary(&parent, &filename)?;

    let result = write_bundle(
        current_exe,
        temporary_file,
        &escript,
        &erlang_root,
        &erts_version,
    );
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    if let Err(error) = publish_bundle(&temporary, &output) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    let size = fs::metadata(&output)?.len() as f64 / 1024.0 / 1024.0;
    println!("Bundled {} ({size:.1} MiB)", output.display());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn write_bundle(
    current_exe: &Path,
    mut output: File,
    escript: &Path,
    erlang_root: &Path,
    erts_version: &str,
) -> Result<()> {
    io::copy(&mut File::open(current_exe)?, &mut output)?;
    make_executable(&output)?;
    let payload_offset = output.seek(SeekFrom::End(0))?;
    write_payload(&mut output, escript, erlang_root, erts_version)?;

    let payload_length = output.stream_position()? - payload_offset;
    let hash = hash_range(&mut output, payload_offset, payload_length)?;
    output.seek(SeekFrom::End(0))?;
    output.write_all(
        &Footer {
            offset: payload_offset,
            length: payload_length,
            hash,
        }
        .encode(),
    )?;
    output.sync_all()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn write_bundle(
    _current_exe: &Path,
    mut output: File,
    escript: &Path,
    erlang_root: &Path,
    erts_version: &str,
) -> Result<()> {
    make_executable(&output)?;
    write_payload(&mut output, escript, erlang_root, erts_version)?;
    let payload_length = output.stream_position()?;
    let hash = hash_range(&mut output, 0, payload_length)?;
    let launcher = MACOS_LAUNCHER
        .replace(
            "@@BUNDLE_HASH@@",
            &format!("{hash:016x}-{payload_length:x}"),
        )
        .replace("@@ERTS_VERSION@@", erts_version);
    prepend(&mut output, launcher.as_bytes(), payload_length)?;
    output.sync_all()?;
    Ok(())
}

fn write_payload<W: Write>(
    output: W,
    escript: &Path,
    erlang_root: &Path,
    erts_version: &str,
) -> Result<()> {
    let gzip = GzEncoder::new(output, Compression::default());
    let mut archive = Builder::new(gzip);
    archive.follow_symlinks(true);
    archive.append_dir_all("erlang", erlang_root)?;
    archive.append_path_with_name(escript, "app")?;

    let mut header = Header::new_gnu();
    header.set_size(erts_version.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, "erts-version", erts_version.as_bytes())?;
    archive.into_inner()?.finish()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn prepend(file: &mut File, prefix: &[u8], original_length: u64) -> io::Result<()> {
    let offset = prefix.len() as u64;
    file.set_len(original_length + offset)?;
    let mut remaining = original_length;
    let mut buffer = vec![0; 64 * 1024];

    while remaining > 0 {
        let length = remaining.min(buffer.len() as u64);
        let source = remaining - length;
        file.seek(SeekFrom::Start(source))?;
        file.read_exact(&mut buffer[..length as usize])?;
        file.seek(SeekFrom::Start(source + offset))?;
        file.write_all(&buffer[..length as usize])?;
        remaining = source;
    }

    file.seek(SeekFrom::Start(0))?;
    file.write_all(prefix)
}

fn create_temporary(parent: &Path, filename: &std::ffi::OsStr) -> Result<(PathBuf, File)> {
    let filename = filename.to_string_lossy();
    for attempt in 0..100 {
        let path = parent.join(format!(".{filename}.{}.{attempt}.tmp", process::id()));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(invalid_input("could not create a temporary output file"))
}

fn launch(mut executable: File, footer: Footer) -> Result<()> {
    let cache_parent = cache_parent()?;
    let cache = cache_parent.join(format!("{:016x}-{:x}", footer.hash, footer.length));
    let ready = cache.join(".ready");

    if !ready.is_file() {
        if hash_range(&mut executable, footer.offset, footer.length)? != footer.hash {
            return Err(invalid_input("the embedded payload is corrupt"));
        }
        fs::create_dir_all(&cache_parent)?;
        if cache.exists() && !ready.is_file() {
            fs::remove_dir_all(&cache)?;
        }

        let temporary = cache_parent.join(format!(".{:016x}-{}.tmp", footer.hash, process::id()));
        if temporary.exists() {
            fs::remove_dir_all(&temporary)?;
        }
        fs::create_dir(&temporary)?;

        let extraction = (|| -> Result<()> {
            executable.seek(SeekFrom::Start(footer.offset))?;
            let payload = (&mut executable).take(footer.length);
            let gzip = GzDecoder::new(payload);
            Archive::new(gzip).unpack(&temporary)?;
            prepare_runtime(&temporary)?;
            File::create(temporary.join(".ready"))?;
            Ok(())
        })();
        if let Err(error) = extraction {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }

        if let Err(error) = fs::rename(&temporary, &cache) {
            if ready.is_file() {
                fs::remove_dir_all(&temporary)?;
            } else {
                let _ = fs::remove_dir_all(&temporary);
                return Err(error.into());
            }
        }
    }

    // ponytail: cache entries are not pruned; add cleanup when disk usage matters.
    let erlang_root = cache.join("erlang");
    let erts_version = fs::read_to_string(cache.join("erts-version"))?;
    let bin = erlang_root.join(format!("erts-{erts_version}/bin"));
    let escript = bin.join(ESCRIPT_EXECUTABLE);
    let app = cache.join("app");
    if !escript.is_file() || !app.is_file() {
        return Err(invalid_input("the extracted bundle is incomplete"));
    }

    let mut command = Command::new(&escript);
    command
        .arg(&app)
        .args(env::args_os().skip(1))
        .env("ROOTDIR", &erlang_root)
        .env("ERL_ROOTDIR", &erlang_root)
        .env("BINDIR", &bin)
        .env_remove("ERLEXEC_DIR");
    launch_command(command)
}

fn erlang_installation() -> Result<(PathBuf, String)> {
    let output = Command::new("erl")
        .args([
            "-boot",
            "no_dot_erlang",
            "-noshell",
            "-noinput",
            "-eval",
            "io:format(\"GLEAM_BUNDLER_ROOT=~ts~nGLEAM_BUNDLER_ERTS=~s~n\", [code:root_dir(), erlang:system_info(version)]), halt().",
        ])
        .env_remove("ERL_AFLAGS")
        .env_remove("ERL_FLAGS")
        .env_remove("ERL_ZFLAGS")
        .output()
        .map_err(|error| io::Error::other(format!("could not run Erlang: {error}")))?;
    if !output.status.success() {
        return Err(invalid_input(format!(
            "could not inspect the Erlang installation: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let output = String::from_utf8(output.stdout)?;
    let root = output
        .lines()
        .find_map(|line| line.strip_prefix("GLEAM_BUNDLER_ROOT="))
        .ok_or_else(|| invalid_input("Erlang did not report its root directory"))?;
    let version = output
        .lines()
        .find_map(|line| line.strip_prefix("GLEAM_BUNDLER_ERTS="))
        .ok_or_else(|| invalid_input("Erlang did not report its ERTS version"))?;
    let root = PathBuf::from(root);
    let version = version.to_string();
    let root = fs::canonicalize(root)?;
    if !root
        .join(format!("erts-{version}/bin/{ESCRIPT_EXECUTABLE}"))
        .is_file()
    {
        return Err(invalid_input(
            "could not find escript in the Erlang installation",
        ));
    }
    Ok((root, version))
}

fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(invalid_input(format!(
            "refusing to replace the existing path {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn paths_alias(left: &Path, right: &Path) -> Result<bool> {
    if left == right {
        return Ok(true);
    }
    let left = match fs::canonicalize(left) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(left == fs::canonicalize(right)?)
}

fn publish_bundle(temporary: &Path, output: &Path) -> Result<()> {
    #[cfg(windows)]
    match fs::remove_file(output) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    fs::rename(temporary, output)?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(file: &File) -> io::Result<()> {
    file.set_permissions(fs::Permissions::from_mode(0o755))
}

#[cfg(windows)]
fn make_executable(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn cache_parent() -> Result<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .map(|path| path.join("gleam-bundler"))
        .ok_or_else(|| invalid_input("HOME or XDG_CACHE_HOME must be set"))
}

#[cfg(target_os = "macos")]
fn cache_parent() -> Result<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join("Library/Caches"))
        })
        .map(|path| path.join("gleam-bundler"))
        .ok_or_else(|| invalid_input("HOME or XDG_CACHE_HOME must be set"))
}

#[cfg(windows)]
fn cache_parent() -> Result<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("USERPROFILE")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join("AppData/Local"))
        })
        .map(|path| path.join("gleam-bundler"))
        .ok_or_else(|| invalid_input("LOCALAPPDATA or USERPROFILE must be set"))
}

#[cfg(not(windows))]
fn prepare_runtime(_bundle: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn prepare_runtime(bundle: &Path) -> Result<()> {
    let version = fs::read_to_string(bundle.join("erts-version"))?;
    let erl_ini = bundle
        .join("erlang")
        .join(format!("erts-{version}/bin/erl.ini"));
    match fs::remove_file(erl_ini) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn launch_command(mut command: Command) -> Result<()> {
    Err(command.exec().into())
}

#[cfg(windows)]
fn launch_command(mut command: Command) -> Result<()> {
    let status = command.status()?;
    process::exit(status.code().unwrap_or(1));
}

fn package_name(config: &str) -> Result<String> {
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "name" {
            continue;
        }

        let value = value.trim();
        let quote = value
            .chars()
            .next()
            .filter(|quote| *quote == '"' || *quote == '\'')
            .ok_or_else(|| invalid_input("the package name must be quoted"))?;
        let rest = &value[quote.len_utf8()..];
        let end = rest
            .find(quote)
            .ok_or_else(|| invalid_input("the package name is not terminated"))?;
        let name = &rest[..end];
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(invalid_input("the package name is invalid"));
        }
        return Ok(name.to_string());
    }

    Err(invalid_input("gleam.toml has no top-level package name"))
}

fn read_footer<R: Read + Seek>(reader: &mut R) -> Result<Option<Footer>> {
    let length = reader.seek(SeekFrom::End(0))?;
    if length < FOOTER_LEN {
        return Ok(None);
    }

    reader.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
    let mut bytes = [0; FOOTER_LEN as usize];
    reader.read_exact(&mut bytes)?;
    let Some(footer) = Footer::decode(bytes) else {
        return Ok(None);
    };
    if footer
        .offset
        .checked_add(footer.length)
        .and_then(|end| end.checked_add(FOOTER_LEN))
        != Some(length)
    {
        return Err(invalid_input("the bundle footer is invalid"));
    }
    Ok(Some(footer))
}

fn hash_range<R: Read + Seek>(reader: &mut R, offset: u64, length: u64) -> io::Result<u64> {
    reader.seek(SeekFrom::Start(offset))?;
    let mut remaining = length;
    let mut hash = HASH_OFFSET;
    let mut buffer = [0; 64 * 1024];

    while remaining > 0 {
        let wanted = remaining.min(buffer.len() as u64) as usize;
        let read = reader.read(&mut buffer[..wanted])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload ended early",
            ));
        }
        for byte in &buffer[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(HASH_PRIME);
        }
        remaining -= read as u64;
    }
    Ok(hash)
}

impl Footer {
    #[cfg(any(not(target_os = "macos"), test))]
    fn encode(self) -> [u8; FOOTER_LEN as usize] {
        let mut bytes = [0; FOOTER_LEN as usize];
        bytes[0..8].copy_from_slice(&self.offset.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.length.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.hash.to_le_bytes());
        bytes[24..32].copy_from_slice(MAGIC);
        bytes
    }

    fn decode(bytes: [u8; FOOTER_LEN as usize]) -> Option<Self> {
        if &bytes[24..32] != MAGIC {
            return None;
        }
        Some(Self {
            offset: u64::from_le_bytes(bytes[0..8].try_into().ok()?),
            length: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            hash: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
        })
    }
}

fn invalid_input(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

fn print_help() {
    println!(
        "gleam-bundler {}

Bundle a Gleam CLI and the local Erlang runtime into one executable.

Usage: gleam-bundler bundle [OPTIONS] [PROJECT]

Commands:
  bundle           Build a standalone executable

Arguments:
  [PROJECT]      Gleam project directory [default: .]

Options:
  -o, --out PATH Write the executable to PATH
  -h, --help     Print help
  -V, --version  Print version",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_the_top_level_package_name() {
        assert_eq!(
            package_name("name = \"hello_world\" # comment\n[dependencies]\n").unwrap(),
            "hello_world"
        );
        assert_eq!(package_name("name='tiny2'\n").unwrap(), "tiny2");
        assert!(package_name("name = \"unterminated\n").is_err());
        assert!(package_name("[dependencies]\nname = \"wrong_table\"\n").is_err());
    }

    #[test]
    fn recognises_a_valid_footer() {
        let payload = b"payload";
        let footer = Footer {
            offset: 3,
            length: payload.len() as u64,
            hash: 42,
        };
        let mut bytes = b"exe".to_vec();
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&footer.encode());

        assert_eq!(read_footer(&mut Cursor::new(bytes)).unwrap(), Some(footer));
    }

    #[test]
    fn hashes_only_the_requested_range() {
        let mut reader = Cursor::new(b"xhelloy".to_vec());
        assert_eq!(hash_range(&mut reader, 1, 5).unwrap(), 0xa430d84680aabd0b);
    }
}
