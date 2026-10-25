# gleam-bundler

Bundle a Gleam CLI and its Erlang runtime into one executable for Linux, macOS,
or Windows.

```sh
cargo install --git https://github.com/natalie-o-perret/gleam-bundler
gleam-bundler bundle
./build/gleam-bundler/my_app
```

The generated executable can run on a compatible system without Gleam or Erlang
installed. It extracts its payload once into the user cache directory.

## Requirements

Building a bundle requires:

- Linux, macOS, or Windows
- Gleam 1.17 or newer
- Erlang/OTP

The project must have a `pub fn main()` accepted by `gleam export escript`.

Use `--out` to choose another output path, or pass a project directory:

```sh
gleam-bundler bundle --out ./dist/my_app ./path/to/project
```

## How it works

`gleam-bundler` runs `gleam export escript`, then archives that escript and the
local Erlang installation. Linux and Windows append the archive to a native
launcher. macOS uses a single executable shell archive to avoid invalidating
Mach-O signatures. Each launcher extracts the payload and starts the bundled
`escript` process.

## Current limits

- The current operating system and CPU architecture only
- No cross-compilation
- The destination needs system libraries compatible with the build machine
- Native dependencies keep their own system-library requirements
- The full local Erlang installation is included, so bundles are large
- Old cache entries are not removed automatically
- macOS bundles are not signed or notarised

Bundles redistribute the local Erlang installation and project dependencies.
Check and include the licences required by those components before distributing
a bundle.

## Licence

MIT
