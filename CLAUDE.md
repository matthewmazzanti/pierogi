# CLAUDE.md — pierogi

A fast, Nix-native sandbox runtime — between `bubblewrap` and `docker`. It runs a
process in a rootless namespace sandbox whose root filesystem **is a sealed Nix
store path**, built ahead of time. A launch is just namespaces + binds + `exec`
— no Nix query at runtime.

## Mental model in one paragraph

A **producer** (the Nix layer, `nix/`) composes a *spec* — a complete, auditable
JSON description of one sandbox, mirroring the OCI runtime spec plus a Nix
`closure` carve-out. The **engine** (the Rust binary, `src/`) is a *pure
realizer*: it reads the resolved spec, projects it verbatim into an OCI runtime
spec, and drives the `crun` binary to namespace + `exec`. All policy lives in the
producer; the engine adds nothing.

## The invariants (do not break these)

These recur across every module/nix header because they are the design. A change
that violates one is almost certainly wrong — stop and reconsider.

1. **The engine holds no policy — it's a pure realizer.** `absent = absent`: if
   the spec omits a field, the engine emits nothing for it. Every default and
   floor lives in the producer (`nix/profiles.nix`), never baked into `src/`.
2. **Pre-bake only — no runtime Nix query.** `run` *asserts* `closure.resolved`;
   only `resolve` (build-time, from a supplied `--graph`) *stamps* it. There is
   no live `nix-store` lookup anywhere in the run path.
3. **One backend: crun, driven as a subprocess.** No FFI, no libcrun linking.
   (youki was dropped — can't RO-remount a store rootfs. See `src/crun.rs`.)
4. **The resolved spec is the complete, auditable record.** Every loosening — a
   host bind, an opened namespace, a dropped default — is visible in the spec or
   does not happen. Nothing is applied ambiently.
5. **Fail loud.** No `|| true`, no swallowed errors, no redundant guards. Let a
   required step fail the build/run.
6. **Mechanical gotchas** (each cost real debugging once): device nodes go via a
   `/dev` tmpfs, never `mounts[]` (bind of a char device → EINVAL); GPU/GL
   sandboxes need Mesa *seeded explicitly* + `/sys` mounted or they silently fall
   back to software; `deny_unknown_fields` is on at every spec level, so an
   unmodeled field is a hard error, not a silent drop.

## Build & run

```sh
nix develop                    # dev shell (rust toolchain; pins CRUN_BIN,
                               # XDG_DBUS_PROXY_BIN — the crate won't build without them)
                               # or: direnv allow  (.envrc is `use flake`)

cargo build                    # build the engine (inside the dev shell)
cargo clippy                   # lint
cargo test                     # NOTE: tests hit nix-store / namespaces; the Nix
                               # package build sets doCheck = false

nix run .#bash-sandbox         # build + run a pre-baked sealed-rootfs sandbox
nix run .#bash-sandbox -- -c 'echo hi'
nix build .#bash-sandbox       # inspect the built spec.json without running
```

Runnable sandboxes and single-axis probes are flake packages (`.#<name>`); see
`flake.nix` for the full list (`wayland`/`foot`/`ghostty` sandboxes; `egl`,
`vulkan`, `dbus`, `pipewire`, `clipboard`, … probes).

## Repo map

```
src/     the engine (pure realizer). See src/CLAUDE.md.
nix/     the producer (all policy). See nix/CLAUDE.md.
docs/    design source-of-truth (the WHY). Read before non-trivial changes.
flake.nix / build.rs / .envrc   dev shell + pinned binary paths (CRUN_BIN, XDG_DBUS_PROXY_BIN)
todo.md  open follow-ups
```

## Where the depth already lives — read it, don't restate it

This repo is heavily documented. **Prefer reading the existing prose over
re-deriving it.** Every source and nix file opens with a dense header explaining
its role and history; `docs/` carries the design rationale.

| Before you touch… | Read |
|---|---|
| the spec schema / a new field | `docs/spec.md`, `src/spec.rs` header |
| what moves the sandbox boundary | `docs/control-surface.md` |
| the verbs / override & resolve pipeline | `docs/execution-modes.md`, `src/run.rs` header |
| the engine/producer boundary, big picture | `docs/architecture.md`, `docs/vision.md` |
| launch cost / why pre-bake | `docs/startup-performance.md` |
| what's planned vs done | `docs/roadmap.md`, `todo.md` |

When you add non-obvious code, match the house style: a module/file header that
explains *why* the shape is what it is and cross-references the relevant doc.
