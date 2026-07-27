# CLAUDE.md — src/ (the engine)

The pierogi crate: a **pure realizer**. It obtains a resolved spec, projects it
verbatim into an OCI runtime spec, and drives `crun` to namespace + `exec`. It
holds **no policy** — see the root `CLAUDE.md` invariants. All defaults live in
`nix/`, never here.

Every file opens with a header that already explains its role and history; this
is just the map and the rules that span modules.

## The pipeline

```
main.rs        parse CLI → two verbs: `run` / `resolve`
  │
run.rs         orchestrate: obtain spec → assert built → realize → drive backend
  ├─ overrides.rs   `run` only: overlay CLI --env/--hostname/--mount/-- args
  ├─ resolve.rs     `resolve` only: traverse the supplied --graph, stamp closure.resolved
  ├─ spec.rs        the typed pierogi Spec (v0 schema; mirrors OCI + closure/dbus carve-outs)
  ├─ oci.rs         translate resolved Spec → in-memory oci-spec::Spec (the pure projection)
  ├─ dbus.rs        supervise xdg-dbus-proxy sidecars (mechanism; policy travels in the spec)
  ├─ crun.rs        write config.json, exec the crun binary, return exit code
  └─ tty.rs         host-side console-socket relay for `tty = true` (SCM_RIGHTS pty master)
```

## Rules specific to the engine

- **oci.rs is a projection, `absent = absent`.** `oci-spec`'s builders inject
  their own opinionated defaults (nnp, 3 caps, a nofile rlimit, `readonly:true`,
  `hostname:"youki"`); we **explicitly set every pass-through field** — to `None`
  when the spec is silent — to neutralize them. Never introduce an engine-side
  default; if something needs a default, it belongs in `nix/profiles.nix`.
- **`run` never re-resolves.** Overrides (`overrides.rs`) layer onto a loaded
  spec without a query. Hence the constraints: **no `--mount store:`** (that
  would change the sealed closure — a different closure is a different image),
  and the **executable is sealed** (an args override replaces `process.args`
  only; the entrypoint is the image's). Host/tmpfs mounts must target a
  *baked* mountpoint — a runtime mount can bind onto one but can't create it
  under the read-only rootfs.
- **`resolve` is the only place `closure.resolved` is stamped**, and it does so
  by traversing the supplied `exportReferencesGraph` in-process from
  `closure.seeds` — not by trusting a flat path list — so `resolved` is provably
  the closure of the declared seeds. At run time a missing `resolved` is a hard
  error, never a trigger to compute one.
- **Devices via tmpfs, not `mounts[]`** (EINVAL on a char-device bind). Mount a
  `/dev` tmpfs and let crun populate default nodes; a *specific* host node (e.g.
  a GPU render node) binds onto that tmpfs at runtime with `nodev` omitted.
- **`deny_unknown_fields` at every level** in `spec.rs`. Adding an OCI knob means
  adding the typed field — a silent drop is not an option. Verbatim-OCI leaves
  reuse `oci_spec` types directly rather than re-modeling them.
- **crun is exec'd with an absolute `-f config.json`** (it chdirs into the bundle
  before resolving a relative path). See `crun.rs`.

## Conventions

- Errors: `anyhow` with `.context(...)`; fail loud, no swallowing.
- Keep raw `unsafe`/`libc` confined (today: `tty.rs` ioctls + the
  async-signal-safe SIGWINCH write); prefer `nix`'s checked wrappers otherwise.
- New non-obvious code gets a header comment explaining *why*, cross-referencing
  the owning `docs/` file (`spec.md`, `control-surface.md`, `execution-modes.md`,
  `architecture.md`). Match the density of the existing headers.
