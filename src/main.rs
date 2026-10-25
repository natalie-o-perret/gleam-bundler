#[cfg(not(target_os = "linux"))]
compile_error!("gleam-bundler currently supports Linux only");

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use std::{
    env,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{self, Command},
};
use tar::{Archive, Builder, Header};

const MAGIC: &[u8; 8] = b"GLMBNDL1";
const FOOTER_LEN: u64 = 32;
const HASH_OFFSET: u64 = 0xcbf29ce484222325;
const HASH_PRIME: u64 = 0x100000001b3;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Footer {
    offset: u64,
    length: u64,
    hash: u64,
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
    match fs::symlink_metadata(&escript) {
        Ok(_) => {
            return Err(invalid_input(format!(
                "refusing to replace the existing path {}",
                escript.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

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
            .join(&package_name)
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
    if output == escript {
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

    if let Err(error) = fs::rename(&temporary, &output) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    fs::remove_file(&escript)?;

    let size = fs::metadata(&output)?.len() as f64 / 1024.0 / 1024.0;
    println!("Bundled {} ({size:.1} MiB)", output.display());
    Ok(())
}

fn write_bundle(
    current_exe: &Path,
    mut output: File,
    escript: &Path,
    erlang_root: &Path,
    erts_version: &str,
) -> Result<()> {
    io::copy(&mut File::open(current_exe)?, &mut output)?;
    output.set_permissions(fs::Permissions::from_mode(0o755))?;
    let payload_offset = output.seek(SeekFrom::End(0))?;

    {
        let gzip = GzEncoder::new(&mut output, Compression::default());
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
    }

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
    let cache_home = env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .ok_or_else(|| invalid_input("HOME or XDG_CACHE_HOME must be set"))?;
    let cache_parent = cache_home.join("gleam-bundler");
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
    let escript = bin.join("escript");
    let app = cache.join("app");
    if !escript.is_file() || !app.is_file() {
        return Err(invalid_input("the extracted bundle is incomplete"));
    }

    let error = Command::new(&escript)
        .arg(&app)
        .args(env::args_os().skip(1))
        .env("ROOTDIR", &erlang_root)
        .env("ERL_ROOTDIR", &erlang_root)
        .env("BINDIR", &bin)
        .exec();
    Err(error.into())
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
    if !root.join(format!("erts-{version}/bin/escript")).is_file() {
        return Err(invalid_input(
            "could not find escript in the Erlang installation",
        ));
    }
    Ok((root, version))
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

Bundle a Gleam CLI and the local Erlang runtime into one Linux executable.

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
