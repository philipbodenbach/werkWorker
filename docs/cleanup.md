# Temporary files and caches

These host CLI commands work offline, without a model, session database, or an
interactive terminal. They never run as agent tools. `ww` and `werk-worker` expose
the same commands.

## Temporary files

```sh
ww tmp path
ww tmp list
ww tmp list --json
ww tmp purge --all --dry-run
ww tmp purge --all
ww tmp purge <ID>
```

`temp` is an alias for `tmp`. The directory is the OS temporary directory
(`TMPDIR` on Unix). Only top-level names starting with `ww-`, `ww_`,
`werk-worker-`, `werkworker-`, or `werk_doctor_` are candidates. Other programs'
temporary files, including Werk1112 files, are excluded.

By default an entry and every descendant must have been unchanged for at least
24 hours. Use `--older-than-hours 0` on both list and purge to include recent
entries. Symlink roots, entries owned by other users on Unix, special files and
unreadable trees are protected. Child symlinks are never followed. The temporary
directory itself is retained.

## Build files, backups and shared caches

```sh
ww --workspace ~/werkWorker cache list
ww --workspace ~/werkWorker cache purge --all --kind build --dry-run
ww --workspace ~/werkWorker cache purge --all --kind build
ww --workspace ~/werk1112 cache purge --all --kind build
ww cache list --kind backup
ww cache purge <ID> --kind backup
ww cache list --kind shared
ww cache purge --all --kind shared --dry-run
ww cache purge --all --kind shared
```

Every purge requires an exact listed ID or `--all`. IDs are stable for the same
absolute path; paths and wildcards are not accepted as selectors. Match the
scope options between list and purge. `--dry-run` always preserves the files.
Both list and purge accept `--json`.

| Kind | Scope |
| --- | --- |
| `build` | Local `target` directories beneath a Rust `--workspace`, including nested Cargo packages up to four directory levels (such as the SDK example). Symlink directories, hidden directories, `node_modules` and `vendor` are excluded from discovery. Custom target directories outside these locations are not managed. |
| `backup` | Individual saved program versions under `werk-worker/backups` and `werk-worker/binary-backups` in `XDG_STATE_HOME`, or `~/.local/state`; Windows uses `LOCALAPPDATA`. Removing these loses those rollback versions. |
| `shared` | Pip, uv, pipenv and vLLM caches under `XDG_CACHE_HOME` or `~/.cache`; `~/.npm/_cacache`, `~/.nv/ComputeCache`, `~/.triton/cache`; Cargo registry `cache`, `src` and `index` under `CARGO_HOME` or `~/.cargo`. |

Without a kind, list and purge cover builds and backups. Shared caches require
`--kind shared` or `--shared`. They are also used by other projects; packages or
compiled kernels will need downloading or rebuilding. Nonstandard cache
locations beyond the listed environment variables are not discovered.

Model weights, Hugging Face caches and tokens, installed runtimes and commands,
configuration, saved conversations and their artifacts are outside these scopes.
For Werk1112's model-store temporary files use `werk temp list` / `werk temp purge`
(also `werk tmp`); for its persistence caches use `werk cache list` / `purge`.

## Activity and reporting

Run cleanup while builds, installations and affected workloads are idle. Existing
Cargo output locks are checked and held during deletion. On Linux, the command
also takes a best-effort snapshot of this user's process executable paths,
working directories and open file paths; matching entries are skipped. This
snapshot is not a lock against a program starting new work during cleanup.

Reports show IDs, kinds, paths, file counts, allocated sizes and protection
reasons. Active Cargo locks still allow size inspection; unreadable or protected
paths report incomplete sizes (`size_complete: false` in JSON).
On Unix, sizes use allocated filesystem blocks and count hard links
once per entry; other platforms use logical sizes. Hard links to files elsewhere
can make the space actually reclaimed smaller. `--all` skips protected entries;
selecting a protected ID or encountering a deletion failure exits with an error
after printing the report. Purging does not create a session database.
