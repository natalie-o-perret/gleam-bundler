# gleam-bundler

Bundle a Gleam CLI and its Erlang runtime into one Linux executable.

```sh
cargo install --git https://github.com/natalie-o-perret/gleam-bundler
gleam-bundler bundle
./build/gleam-bundler/my_app
```

The generated executable can run on a compatible Linux system without Gleam or
Erlang installed. It extracts its payload once under
`$XDG_CACHE_HOME/gleam-bundler`, or `$HOME/.cache/gleam-bundler` when
`XDG_CACHE_HOME` is not set.

## Requirements

Building a bundle requires:

- Linux
- Gleam 1.17 or newer
- Erlang/OTP

The project must have a `pub fn main()` accepted by `gleam export escript`.

Use `--out` to choose another output path, or pass a project directory:

```sh
gleam-bundler bundle --out ./dist/my_app ./path/to/project
```

## How it works

`gleam-bundler` runs `gleam export escript`, then appends that escript and the
local Erlang installation to a copy of itself. The copy detects the embedded
payload, extracts it, and replaces itself with the bundled `escript` process.

## Current limits

- Linux and the current CPU architecture only
- No cross-compilation
- The destination needs system libraries compatible with the build machine
- Native dependencies keep their own system-library requirements
- The full local Erlang installation is included, so bundles are large
- Old cache entries are not removed automatically

Bundles redistribute the local Erlang installation and project dependencies.
Check and include the licences required by those components before distributing
a bundle.

## Licence

MIT
