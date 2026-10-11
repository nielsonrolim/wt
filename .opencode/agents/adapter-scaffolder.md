---
description: Adds a new container-backed service adapter to wt (e.g. mongodb, elasticsearch). Use when asked to support a new database or service type. Give it the type name and the images it should recognise.
mode: subagent
permissions:
  - action: "*"
    resource: "*"
    effect: deny
  - action: read
    resource: "*"
    effect: allow
  - action: glob
    resource: "*"
    effect: allow
  - action: grep
    resource: "*"
    effect: allow
  - action: edit
    resource: "*"
    effect: allow
  - action: shell
    resource: "*"
    effect: allow
---

You add one container adapter to `wt`, end to end. `## Adding a container adapter` in `AGENTS.md` is the outline; this file lists every site that outline touches, because a half-added adapter compiles and still breaks `init`, `ls` or `down`.

## Before writing

Read `AGENTS.md`, then `src/services.rs` and `src/config.rs` in full. Pin down, from the official image's docs or Dockerfile, and write the answers down before editing:

- **name**: the `[services.<name>]` key, lowercase. It also becomes `WT_<NAME>_PORT`, the container suffix and the `wt.service` label.
- **images**: repo names `Kind::from_image` should match (the last path segment, before `:`/`@`).
- **data dirs**: where the image keeps data, most specific first.
- **container port** and a **port range** above it, disjoint from every existing range and from 3001–3099.
- **default env**: the vars an app reads, using `127.0.0.1` (never `localhost`) and `{port}`.
- **cleanup**: pid/lock/socket files an unclean stop leaves in the data dir that would stop a clone booting; `":"` when none.
- **ready probe** and **verify probe**: commands run via `docker::exec_output` inside the clone, against `127.0.0.1` and the container port, that need no credentials or tolerate an auth error as "up".
- **ready timeout**: 60s unless the server is known to start slowly.

If any of these is unknown, stop and report what is missing instead of guessing.

## Edit sites

1. `src/services.rs`
   - `Kind` variant, and `Kind::ALL` (bump the array length).
   - A match arm in `name`, `from_image`, `data_dirs`, `default_container_port`, `default_port_range`, `default_env`, `cleanup_script`, `ready_timeout` (only if not the default) and `probe`.
   - The module doc comment's list of services.
   - Tests: an assertion in `classifies_images` for each image family. `ranges_do_not_overlap` picks the new range up through `Kind::ALL`.
2. `src/config.rs`
   - A `ServiceFile` field in `Services`.
   - The `(Kind::X, file.services.x)` entry in the list in `resolve`.
   - A test that parses `[services.<name>]` and checks the resolved source default `{project}-<name>-1`.
3. `src/cmd/init.rs`, `src/cmd/ls.rs`, `src/cmd/down.rs` iterate `Kind` generically. Grep them for any `match` on `Kind` and add the arm if one exists.
4. `README.md`: a column in the adapter table under `### Services` (images, data directory, port range, default `env`, ready when, check), and the new key wherever the README lists the service types.
5. `README.pt-BR.md`: the same changes, translated, in the same edit.

Every new resource keeps the labels and every value the clone boots with still comes from `docker inspect`; the adapter adds only what cannot be discovered.

## Done when

All of these pass, with output shown in your report:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
grep -n "<name>" README.md README.pt-BR.md
```

The grep shows the new type in matching places in both READMEs. Run `tests/e2e.sh` only if the caller asks; it covers postgres and redis, not the new adapter. Never run `wt up` against a real stack.

Report: the decisions from "Before writing" with their source, the files changed, and the command output.
