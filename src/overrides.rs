//! CLI overrides layered onto a loaded spec — the `run` bridge (see
//! `docs/execution-modes.md#run-overrides--rewrites-and-when-they-recompute`).
//!
//! Supported overrides: `--env`, `--hostname`, an argv override (trailing
//! `-- cmd args`), and additive **host / tmpfs / store** mounts.
//!
//! * `env` / `hostname` / argv rewrite spec fields in place.
//! * host / tmpfs mounts contribute nothing to `resolve()`'s seeds
//!   (`Mount::store_refs` is `None` for both), so they layer on without a query
//!   and without tripping a sealed spec's drift check.
//! * a **store** mount adds a closure seed. On an *unsealed* spec `resolve()`'s
//!   live query picks it up for free; on a *sealed* spec it breaks the seal and
//!   triggers a delta-recompute — the seal-break policy `resolve()` implements
//!   (warn + recompute). Parsing here is closure-agnostic; that split lives in
//!   `resolve()`.
//!
//! `--mount overlay:…` is still rejected — its realization is deferred within M1
//! (`crate::oci::build_spec` bails on `Mount::Overlay`).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::spec::{Mount, Spec, StorePath};

/// Parsed CLI overrides for the `run` verb.
#[derive(Debug, Default)]
pub struct Overrides {
    /// `--env KEY=VALUE` pairs, applied over `spec.env` (insert / overwrite).
    env: Vec<(String, String)>,
    /// `--hostname NAME`, replacing `spec.hostname`.
    hostname: Option<String>,
    /// `--mount …` additive host / tmpfs / store mounts, appended to
    /// `spec.mounts`.
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

    /// Overlay the overrides onto `spec`. This rewrites spec *fields* only and
    /// never touches `spec.closure` directly — but it is applied *before*
    /// `resolve()`, which is where the consequences land: the argv override's
    /// new executable is validated against the closure, and a store mount adds a
    /// seed that `resolve()` folds in live (unsealed) or delta-recomputes,
    /// breaking the seal (sealed).
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
/// * `store:SRC` — identity store bind (folds `SRC`'s closure).
/// * `store:SRC:DEST` — relocated store bind at `DEST`.
///
/// `overlay:…` is rejected (realization deferred within M1).
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
        "store" => {
            let parts: Vec<&str> = rest.split(':').collect();
            let (src, dest) = match parts.as_slice() {
                [src] => (*src, None),
                [src, dest] => (*src, Some(abs_dest(dest, s)?)),
                _ => bail!("--mount store expects store:SRC[:DEST], got `{s}`"),
            };
            let source =
                StorePath::new(src).with_context(|| format!("--mount store source in `{s}`"))?;
            Ok(Mount::Store { source, dest })
        }
        "overlay" => bail!(
            "--mount overlay:… realization is deferred within M1 \
             (host / tmpfs / store only)"
        ),
        other => {
            bail!("--mount has unknown kind `{other}` (expected host, tmpfs, or store): `{s}`")
        }
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
    fn store_mount_parses_identity_and_relocated() {
        match parse_mount("store:/nix/store/xxxx-tool").unwrap() {
            Mount::Store { source, dest } => {
                assert_eq!(source.to_string(), "/nix/store/xxxx-tool");
                assert!(dest.is_none());
            }
            _ => panic!("expected store"),
        }
        match parse_mount("store:/nix/store/xxxx-proj:/home/dev/project").unwrap() {
            Mount::Store { source, dest } => {
                assert_eq!(source.to_string(), "/nix/store/xxxx-proj");
                assert_eq!(dest, Some(PathBuf::from("/home/dev/project")));
            }
            _ => panic!("expected store"),
        }
    }

    #[test]
    fn store_mount_rejects_bad_source_or_dest() {
        assert!(parse_mount("store:/etc/passwd").is_err()); // not a store path
        assert!(parse_mount("store:/nix/store/xxx-tool/bin/x").is_err()); // not top-level
        assert!(parse_mount("store:/nix/store/xxx-tool:relative").is_err()); // dest not absolute
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
