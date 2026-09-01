# Portable Zuno environment bundles

`zuno export` and `zuno import` move one Zuno user environment between Linux,
macOS, and Windows without embedding the source machine's absolute paths. The
archive is a local ZIP container with the `.zuno-bundle` suffix, a versioned
`bundle.json` manifest, logical roots, per-file SHA-256 digests, sizes, and file
modes.

This is an environment backup, not a session export.

## What is included

By default, `zuno export` walks both Zuno-owned user roots resolved for the
current process:

- the global Zuno configuration root, including `zuno.json`, `AGENTS.md`,
  Agents, Skills, Markdown commands, extensions, profiles, themes, and other
  user-created files below that root;
- `$HOME/.zuno`, including Zuno-native user assets stored there.

Built-in Skills from `zuno-orchestration` are compiled into the executable and
do not need a physical copy in the bundle. External shared Skill roots such as
`~/.agents/skills` and directories selected explicitly through `skills.paths` are not Zuno-owned and are not
exported.

The default bundle deliberately excludes:

- session databases, session messages, transcripts, and WAL/SHM files;
- provider and MCP credential stores;
- logs, caches, snapshots, prompt history, tool output, and temporary files;
- `.git`, `.omo`, and `__pycache__` directories.

A symbolic link whose final target is a regular file inside the same exported
root is materialized as a regular bundle file at the link's logical path. Links
to directories, excluded content, external roots, broken targets, or special
filesystem entries are rejected or excluded instead of followed. One file may
be at most 256 MiB, the unpacked payload may be at most 1 GiB, and the manifest
may describe at most 50,000 files.

## Export

```sh
# Creates zuno-export-YYYYMMDDTHHMMSSZ.zuno-bundle in the current directory.
zuno export

# Choose the destination explicitly.
zuno export /path/to/workstation.zuno-bundle

# Replace an existing bundle file.
zuno export /path/to/workstation.zuno-bundle --force
```

The output file cannot be placed inside either exported root. Export writes a
temporary archive beside the destination, syncs it, and installs it without
clobbering an existing file unless `--force` is present.

Credentials are opt-in:

```sh
zuno export /path/to/private.zuno-bundle --include-credentials
```

This adds the resolved provider and MCP credential stores. The bundle is not
encrypted; Zuno prints a warning, and the operator is responsible for protected
transport, storage, and deletion.

## Import

Copy the bundle to the destination machine, then validate it before changing
files:

```sh
zuno import /path/to/workstation.zuno-bundle --dry-run
```

Import accepts only a local file. It validates the format and schema version,
manifest/archive agreement, size limits, hashes, path safety, and target
conflicts. A non-empty destination root is never merged implicitly:

```sh
# Validate a replacement while leaving the destination unchanged.
zuno import /path/to/workstation.zuno-bundle --replace --dry-run

# Transactionally replace the roots carried by the bundle.
zuno import /path/to/workstation.zuno-bundle --replace
```

Without `--replace`, an existing non-empty target fails before staging. With
`--replace`, Zuno stages every root beside its destination, moves existing roots
to temporary backups, and rolls committed roots back in reverse order if a
later replacement fails. A successful import removes the backups.

## Cross-platform path rules

The manifest stores only a logical root plus forward-slash relative paths. At
import time, Zuno resolves `config`, `home-zuno`, and optional credential roots
using the destination operating system and environment. It never restores a
Linux absolute path onto Windows or vice versa.

For portability and extraction safety, export and import reject:

- absolute paths, `.`/`..` traversal, backslashes, drive-colon paths, and NULs;
- path segments ending in a dot or space;
- Windows reserved device names such as `CON`, `NUL`, `COM1`, and `LPT9`;
- distinct names that collide on a case-insensitive filesystem;
- archive entries missing from the manifest or unexpected entries not declared
  by it.

Unix permission bits are restored on Unix. On other platforms, the portable
read-only bit is applied where the filesystem API permits it.

## Recommended migration flow

1. Run `zuno export` on the source machine without credentials.
2. Transfer the `.zuno-bundle` through a trusted channel.
3. Run `zuno import ... --dry-run` on the destination.
4. If the destination already has Zuno files, inspect or back them up, then run
   `zuno import ... --replace --dry-run`.
5. Run the matching real import and verify with `zuno debug paths`,
   `zuno debug config`, `zuno debug skill`, and `zuno plugin list`.
6. Log providers and MCP servers in on the destination machine. Use
   `--include-credentials` only when copying the unencrypted stores is an
   explicit security decision.
