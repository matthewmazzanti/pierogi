//! CLI overrides layered onto a loaded spec — the `run` bridge (see
//! `docs/execution-modes.md#run-overrides--rewrites-and-when-they-recompute`).
//!
//! Supported overrides: `--env`, `--hostname`, an args override (trailing
//! `-- args`), and additive **host / tmpfs** mounts.
//!
//! * `env` / `hostname` / args rewrite spec fields in place. The args override
//!   replaces `process.args` only — the **executable stays the spec's**, sealed
//!   by the image (the entrypoint is fixed; only its arguments vary).
//! * host / tmpfs mounts are never closure seeds (only an `Overlay` lower is),
//!   so they layer on without a query and without tripping a sealed spec's drift
//!   check. Their dest mountpoints must already exist in the sealed `root` — a
//!   runtime mount can bind onto a baked mountpoint but cannot create one under
//!   the read-only rootfs.
//!
//! **No `--mount store:`.** Adding a store path at runtime would change the
//! closure, which is frozen at build time — its resolved set drives the runtime
//! `/nix/store` binds, and overrides never re-resolve. A different closure means
//! a different image. See `docs/execution-modes.md`. `--mount overlay:…` is
//! likewise rejected (its realization is deferred within M1).

use std::path::PathBuf;

use anyhow::{bail, Result};
use oci_spec::runtime::{Mount as OciMount, MountBuilder};

use crate::spec::Spec;

/// Parsed CLI overrides for the `run` verb.
#[derive(Debug, Default)]
pub struct Overrides {
    /// `--env KEY=VALUE` pairs, applied over `spec.process.env` (insert /
    /// overwrite).
    env: Vec<(String, String)>,
    /// `--hostname NAME`, replacing `spec.hostname`.
    hostname: Option<String>,
    /// `--mount …` additive host / tmpfs mounts (verbatim OCI mounts), appended
    /// to `spec.mounts`.
    mounts: Vec<OciMount>,
    /// Trailing `-- args…` overriding `spec.process.args` only — the executable
    /// stays the spec's (sealed by the image). Empty = no override.
    args: Vec<String>,
}

impl Overrides {
    /// Parse the raw CLI strings, failing loud on any malformed `--env` /
    /// `--mount`. Kept separate from [`apply`](Self::apply) so a bad override
    /// aborts before we load or resolve anything.
    pub fn parse(
        env: Vec<String>,
        hostname: Option<String>,
        mount: Vec<String>,
        args: Vec<String>,
    ) -> Result<Self> {
        let env = env.iter().map(|s| parse_env(s)).collect::<Result<_>>()?;
        let mounts = mount
            .iter()
            .map(|s| parse_mount(s))
            .collect::<Result<_>>()?;
        Ok(Overrides {
            env,
            hostname,
            mounts,
            args,
        })
    }

    /// Whether any override is present (used to gate a note at launch).
    pub fn is_empty(&self) -> bool {
        self.env.is_empty()
            && self.hostname.is_none()
            && self.mounts.is_empty()
            && self.args.is_empty()
    }

    /// Overlay the overrides onto `spec`. This rewrites spec *fields* only and
    /// never touches `spec.closure` — the closure is sealed at build time, and
    /// host / tmpfs mounts contribute no seed, so a `run` override can't change
    /// the store view. The args override replaces `process.args` only; the
    /// executable is the spec's, fixed by the image (a sealed entrypoint).
    pub fn apply(self, spec: &mut Spec) {
        for (k, v) in self.env {
            spec.process.env.insert(k, v);
        }
        if let Some(hostname) = self.hostname {
            spec.hostname = Some(hostname);
        }
        spec.mounts.extend(self.mounts);
        if !self.args.is_empty() {
            spec.process.args = self.args;
        }
    }
}

/// Parse `KEY=VALUE`. The value may contain `=` (only the first splits); the
/// key must be non-empty. An empty value (`KEY=`) is allowed.
fn parse_env(s: &str) -> Result<(String, String)> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => bail!("--env expects KEY=VALUE, got `{s}`"),
    }
}

/// Parse a `--mount` string into a [`Mount`]. Supported forms:
///
/// * `host:SRC:DEST` / `host:SRC:DEST:ro` — read-only host bind.
/// * `host:SRC:DEST:rw` — writable host bind.
/// * `tmpfs:DEST` — writable scratch.
///
/// Both bind onto a mountpoint the sealed `root` must already carry. `store:…`
/// and `overlay:…` are rejected: a store path added at runtime would change the
/// closure, which is frozen at build time (its resolved set drives the runtime
/// `/nix/store` binds), and overlay realization is deferred within M1.
fn parse_mount(s: &str) -> Result<OciMount> {
    let (kind, rest) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("--mount expects KIND:…, got `{s}`"))?;
    match kind {
        "tmpfs" => {
            let dest = abs_dest(rest, s)?;
            // The CLI is a producer: it bakes hardened tmpfs options in.
            Ok(MountBuilder::default()
                .destination(dest)
                .typ("tmpfs".to_string())
                .options(opts(["nosuid", "nodev", "mode=1777"]))
                .build()?)
        }
        "host" => {
            let parts: Vec<&str> = rest.split(':').collect();
            let (src, dest, access) = match parts.as_slice() {
                [src, dest] | [src, dest, "ro"] => (*src, *dest, "ro"),
                [src, dest, "rw"] => (*src, *dest, "rw"),
                _ => bail!("--mount host expects host:SRC:DEST[:rw|:ro], got `{s}`"),
            };
            if src.is_empty() {
                bail!("--mount host has an empty source: `{s}`");
            }
            // Expand the ro/rw sugar into explicit, hardened bind options.
            Ok(MountBuilder::default()
                .destination(abs_dest(dest, s)?)
                .source(PathBuf::from(src))
                .typ("bind".to_string())
                .options(opts(["bind", access, "nosuid", "nodev"]))
                .build()?)
        }
        "store" => bail!(
            "--mount store:… is not supported: the closure is sealed at build \
             time (a different closure means a different image), got `{s}`"
        ),
        "overlay" => bail!(
            "--mount overlay:… realization is deferred within M1 \
             (host / tmpfs only)"
        ),
        other => {
            bail!("--mount has unknown kind `{other}` (expected host or tmpfs): `{s}`")
        }
    }
}

/// Collect a fixed set of option strings into a `Vec<String>`.
fn opts<const N: usize>(o: [&str; N]) -> Vec<String> {
    o.iter().map(|s| s.to_string()).collect()
}

/// Validate a mount destination is a non-empty absolute path.
fn abs_dest(dest: &str, whole: &str) -> Result<PathBuf> {
    if dest.is_empty() {
        bail!("--mount has an empty destination: `{whole}`");
    }
    if !dest.starts_with('/') {
        bail!("--mount destination must be absolute, got `{dest}` in `{whole}`");
    }
    Ok(PathBuf::from(dest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Closure, Linux, Process, Root, StorePath};
    use std::collections::BTreeMap;

    fn spec() -> Spec {
        Spec {
            root: Root {
                path: StorePath::new("/nix/store/aaaa-root").unwrap(),
                readonly: None,
            },
            process: Process {
                executable: "bash".into(),
                args: vec!["-l".into()],
                terminal: false,
                user: None,
                cwd: None,
                env: BTreeMap::from([("PATH".into(), "/nix/store/aaaa-root/bin".into())]),
                capabilities: None,
                rlimits: vec![],
                no_new_privileges: None,
                oom_score_adj: None,
            },
            hostname: None,
            domainname: None,
            annotations: BTreeMap::new(),
            linux: Linux::default(),
            mounts: vec![],
            closure: Closure::default(),
            dbus: Vec::new(),
        }
    }

    #[test]
    fn env_inserts_and_overwrites_without_touching_closure() {
        let o = Overrides::parse(
            vec!["FOO=bar".into(), "PATH=/override".into(), "EMPTY=".into()],
            None,
            vec![],
            vec![],
        )
        .unwrap();
        let mut s = spec();
        o.apply(&mut s);
        assert_eq!(s.process.env.get("FOO").unwrap(), "bar");
        assert_eq!(s.process.env.get("PATH").unwrap(), "/override");
        assert_eq!(s.process.env.get("EMPTY").unwrap(), "");
        assert!(s.closure.resolved.is_none());
    }

    #[test]
    fn value_may_contain_equals() {
        let (k, v) = parse_env("KEY=a=b=c").unwrap();
        assert_eq!(k, "KEY");
        assert_eq!(v, "a=b=c");
    }

    #[test]
    fn empty_key_is_rejected() {
        assert!(parse_env("=value").is_err());
        assert!(parse_env("noequals").is_err());
    }

    #[test]
    fn hostname_override() {
        let o = Overrides::parse(vec![], Some("devbox".into()), vec![], vec![]).unwrap();
        let mut s = spec();
        o.apply(&mut s);
        assert_eq!(s.hostname.as_deref(), Some("devbox"));
    }

    #[test]
    fn args_override_rewrites_args_only() {
        let o = Overrides::parse(
            vec![],
            None,
            vec![],
            vec!["-c".into(), "echo hi".into()],
        )
        .unwrap();
        let mut s = spec();
        o.apply(&mut s);
        // Executable is the spec's — sealed by the image, never overridden.
        assert_eq!(s.process.executable, "bash");
        assert_eq!(s.process.args, vec!["-c", "echo hi"]);
        // tty is untouched by an args override.
        assert!(!s.process.terminal);
    }

    #[test]
    fn empty_args_leaves_process_unchanged() {
        let o = Overrides::default();
        let mut s = spec();
        o.apply(&mut s);
        assert_eq!(s.process.executable, "bash");
        assert_eq!(s.process.args, vec!["-l"]);
    }

    #[test]
    fn host_mount_ro_rw_and_default() {
        let o = Overrides::parse(
            vec![],
            None,
            vec![
                "host:/a:/a".into(),
                "host:/b:/b:ro".into(),
                "host:/c:/c:rw".into(),
            ],
            vec![],
        )
        .unwrap();
        let mut s = spec();
        o.apply(&mut s);
        assert_eq!(s.mounts.len(), 3);
        // ro default → bind,ro,nosuid,nodev.
        let m0 = &s.mounts[0];
        let o0 = m0.options().as_ref().unwrap();
        assert!(o0.contains(&"ro".to_string()));
        assert!(o0.contains(&"nosuid".to_string()));
        // explicit rw → a bind mount, bind,rw,nosuid,nodev.
        let m2 = &s.mounts[2];
        assert_eq!(m2.source().as_ref().unwrap(), &PathBuf::from("/c"));
        assert_eq!(m2.destination(), &PathBuf::from("/c"));
        assert_eq!(m2.typ().as_deref(), Some("bind"));
        assert_eq!(
            m2.options().as_ref().unwrap(),
            &["bind", "rw", "nosuid", "nodev"]
        );
    }

    #[test]
    fn tmpfs_mount() {
        let m = parse_mount("tmpfs:/scratch").unwrap();
        assert_eq!(m.destination(), &PathBuf::from("/scratch"));
        assert_eq!(m.typ().as_deref(), Some("tmpfs"));
        assert!(m.options().as_ref().unwrap().contains(&"nosuid".to_string()));
    }

    #[test]
    fn store_mount_is_rejected() {
        // The closure is sealed at build time — no runtime store additions,
        // regardless of whether the source is a valid store path.
        assert!(parse_mount("store:/nix/store/xxxx-tool").is_err());
        assert!(parse_mount("store:/nix/store/xxxx-proj:/home/dev/project").is_err());
        assert!(parse_mount("store:/etc/passwd").is_err());
    }

    #[test]
    fn overlay_mount_is_rejected() {
        assert!(parse_mount("overlay:/nix/store/xxx-proj:/work").is_err());
    }

    #[test]
    fn malformed_mounts_are_rejected() {
        assert!(parse_mount("host:/only-source").is_err()); // missing dest
        assert!(parse_mount("host:/a:relative").is_err()); // dest not absolute
        assert!(parse_mount("host::/dest").is_err()); // empty source
        assert!(parse_mount("tmpfs:").is_err()); // empty dest
        assert!(parse_mount("tmpfs:relative").is_err()); // dest not absolute
        assert!(parse_mount("bogus:/x").is_err()); // unknown kind
        assert!(parse_mount("nocolon").is_err()); // no `:`
    }
}
