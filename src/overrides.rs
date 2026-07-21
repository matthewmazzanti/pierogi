//! CLI overrides layered onto a loaded spec — the `run` bridge (see
//! `docs/execution-modes.md#run-overrides--rewrites-and-when-they-recompute`).
//!
//! This slice implements the **runtime-only** bucket: `--env`, `--hostname`,
//! an argv override (trailing `-- cmd args`), and additive **host / tmpfs**
//! mounts. None of these touch the closure, so applying them never triggers a
//! re-resolve:
//!
//! * `env` / `hostname` / argv rewrite spec fields in place.
//! * host / tmpfs mounts contribute nothing to `resolve()`'s seeds
//!   (`Mount::store_refs` is `None` for both), so they layer on without a query
//!   and without tripping a sealed spec's drift check.
//!
//! A `--mount store:…` *would* recompute the closure and break a sealed spec —
//! whose policy (silent / warn / require-flag) is still open (see the docs) — so
//! it is rejected here rather than half-implemented.

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::spec::{Mount, Spec};

/// Parsed CLI overrides for the `run` verb.
#[derive(Debug, Default)]
pub struct Overrides {
    /// `--env KEY=VALUE` pairs, applied over `spec.env` (insert / overwrite).
    env: Vec<(String, String)>,
    /// `--hostname NAME`, replacing `spec.hostname`.
    hostname: Option<String>,
    /// `--mount …` additive host / tmpfs mounts, appended to `spec.mounts`.
    mounts: Vec<Mount>,
    /// Trailing `-- cmd args…` overriding `spec.exec` (empty = no override).
    argv: Vec<String>,
}

impl Overrides {
    /// Parse the raw CLI strings, failing loud on any malformed `--env` /
    /// `--mount`. Kept separate from [`apply`](Self::apply) so a bad override
    /// aborts before we load or resolve anything.
    pub fn parse(
        env: Vec<String>,
        hostname: Option<String>,
        mount: Vec<String>,
        argv: Vec<String>,
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
            argv,
        })
    }

    /// Whether any override is present (used to gate a note at launch).
    pub fn is_empty(&self) -> bool {
        self.env.is_empty()
            && self.hostname.is_none()
            && self.mounts.is_empty()
            && self.argv.is_empty()
    }

    /// Overlay the overrides onto `spec`. **Runtime-only**: never touches
    /// `spec.closure`, so a resolved/sealed spec stays sealed and an unresolved
    /// one resolves over the same seeds. Applied *before* `resolve()` so the
    /// argv override's new executable is validated against the closure.
    pub fn apply(self, spec: &mut Spec) {
        for (k, v) in self.env {
            spec.env.insert(k, v);
        }
        if let Some(hostname) = self.hostname {
            spec.hostname = hostname;
        }
        spec.mounts.extend(self.mounts);
        if !self.argv.is_empty() {
            let mut it = self.argv.into_iter();
            // Non-empty checked above, so `next()` yields the executable.
            spec.exec.executable = it.next().expect("argv non-empty");
            spec.exec.args = it.collect();
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
/// `store:…` is deliberately rejected: it would fold a closure and recompute,
/// breaking a sealed spec under a policy that isn't decided yet.
fn parse_mount(s: &str) -> Result<Mount> {
    let (kind, rest) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("--mount expects KIND:…, got `{s}`"))?;
    match kind {
        "tmpfs" => {
            let dest = abs_dest(rest, s)?;
            Ok(Mount::Tmpfs { dest })
        }
        "host" => {
            let parts: Vec<&str> = rest.split(':').collect();
            let (src, dest, rw) = match parts.as_slice() {
                [src, dest] | [src, dest, "ro"] => (*src, *dest, false),
                [src, dest, "rw"] => (*src, *dest, true),
                _ => bail!("--mount host expects host:SRC:DEST[:rw|:ro], got `{s}`"),
            };
            if src.is_empty() {
                bail!("--mount host has an empty source: `{s}`");
            }
            Ok(Mount::Host {
                source: PathBuf::from(src),
                dest: abs_dest(dest, s)?,
                rw,
            })
        }
        "store" | "overlay" => bail!(
            "--mount {kind}:… would fold a closure and recompute over a sealed spec; \
             that seal-break policy is not decided yet (see docs/execution-modes.md). \
             Only host / tmpfs mounts are supported as overrides."
        ),
        other => bail!("--mount has unknown kind `{other}` (expected host or tmpfs): `{s}`"),
    }
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
    use crate::spec::{Exec, StorePath};
    use std::collections::BTreeMap;

    fn spec() -> Spec {
        Spec {
            exec: Exec {
                executable: "bash".into(),
                args: vec!["-l".into()],
                terminal: false,
            },
            root: StorePath::new("/nix/store/aaaa-root").unwrap(),
            mounts: vec![],
            env: BTreeMap::from([("PATH".into(), "/nix/store/aaaa-root/bin".into())]),
            hostname: "pierogi".into(),
            closure: None,
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
        assert_eq!(s.env.get("FOO").unwrap(), "bar");
        assert_eq!(s.env.get("PATH").unwrap(), "/override");
        assert_eq!(s.env.get("EMPTY").unwrap(), "");
        assert!(s.closure.is_none());
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
        assert_eq!(s.hostname, "devbox");
    }

    #[test]
    fn argv_override_rewrites_exec_and_args() {
        let o = Overrides::parse(
            vec![],
            None,
            vec![],
            vec!["rg".into(), "foo".into(), "--color".into()],
        )
        .unwrap();
        let mut s = spec();
        o.apply(&mut s);
        assert_eq!(s.exec.executable, "rg");
        assert_eq!(s.exec.args, vec!["foo", "--color"]);
        // terminal is untouched by an argv override.
        assert!(!s.exec.terminal);
    }

    #[test]
    fn empty_argv_leaves_exec_unchanged() {
        let o = Overrides::default();
        let mut s = spec();
        o.apply(&mut s);
        assert_eq!(s.exec.executable, "bash");
        assert_eq!(s.exec.args, vec!["-l"]);
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
        match &s.mounts[0] {
            Mount::Host { rw, .. } => assert!(!rw),
            _ => panic!("expected host"),
        }
        match &s.mounts[2] {
            Mount::Host { source, dest, rw } => {
                assert_eq!(source, &PathBuf::from("/c"));
                assert_eq!(dest, &PathBuf::from("/c"));
                assert!(rw);
            }
            _ => panic!("expected host"),
        }
    }

    #[test]
    fn tmpfs_mount() {
        let m = parse_mount("tmpfs:/scratch").unwrap();
        match m {
            Mount::Tmpfs { dest } => assert_eq!(dest, PathBuf::from("/scratch")),
            _ => panic!("expected tmpfs"),
        }
    }

    #[test]
    fn store_and_overlay_mounts_are_rejected() {
        assert!(parse_mount("store:/nix/store/xxx-tool").is_err());
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
