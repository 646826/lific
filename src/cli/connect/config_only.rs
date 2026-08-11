//! Config-only client provisioning for external orchestrators.
//!
//! This path intentionally does not open a Lific database and never creates,
//! rotates, or prints an API key. The caller owns credential provisioning and
//! supplies the already-scoped harness key through `--key-env` (or `--key`).
//! The existing client matrix and merge-preserving writers remain the single
//! source of truth for paths and formats.

use std::io::IsTerminal;

use super::clients::{OauthSupport, PathBase, ServerConfig};
use super::{ClientOutcome, ConnectArgs, ConnectResult, KeyOrigin, clients, writer};
use crate::config::Config;

const DRY_RUN_KEY: &str = "lific_sk-live-DRYRUN000000000000000000000000";

pub fn run(args: &ConnectArgs, _cfg: &Config, base: &PathBase) -> Result<ConnectResult, String> {
    if args.stdio {
        return Err("--config-only cannot be combined with --stdio".into());
    }
    if args.user.is_some() {
        return Err(
            "--config-only does not mint a bot; omit --user and provision the scoped key before running it"
                .into(),
        );
    }
    if !args.skip_agents {
        return Err(
            "--config-only requires --skip-agents; update AGENTS.md explicitly with `lific agents-md`"
                .into(),
        );
    }
    let url = args
        .url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "--config-only requires an explicit --url".to_string())?;
    let mcp_url = if url.trim_end_matches('/').ends_with("/mcp") {
        url.trim_end_matches('/').to_string()
    } else {
        format!("{}/mcp", url.trim_end_matches('/'))
    };
    if args.oauth && args.key.is_some() {
        return Err("--oauth and --key/--key-env are mutually exclusive".into());
    }
    if !args.oauth && args.key.as_deref().is_none_or(str::is_empty) {
        return Err("--config-only requires --key, --key-env, or --oauth".into());
    }
    if args.clients.is_empty() {
        return Err("--config-only requires at least one explicit --client".into());
    }
    if !args.yes && !std::io::stdin().is_terminal() {
        return Err("--config-only requires --yes in non-interactive use".into());
    }

    let selected = super::resolve_clients_inner(
        &args.clients,
        std::io::stdin().is_terminal(),
        base,
        args.scope,
        |_| Err("interactive selection is disabled in --config-only mode".into()),
    )?;

    let key = if args.dry_run {
        DRY_RUN_KEY
    } else {
        args.key.as_deref().unwrap_or("")
    };
    let mut outcomes = Vec::new();

    for id in selected {
        let Some(spec) = clients::find_client(&id) else {
            continue;
        };
        if args.oauth
            && let OauthSupport::Unsupported { reason } = spec.oauth
        {
            outcomes.push(ClientOutcome {
                id,
                display: spec.display.to_string(),
                format: spec.format.as_str().to_string(),
                error: Some(format!(
                    "{} does not support --oauth; skipped",
                    spec.display
                )),
                notes: vec![reason.to_string()],
                ..Default::default()
            });
            continue;
        }
        let Some(path) = spec.path_for(base, args.scope) else {
            outcomes.push(ClientOutcome {
                id,
                display: spec.display.to_string(),
                format: spec.format.as_str().to_string(),
                error: Some(format!(
                    "{} has no {}-scope config; skipped",
                    spec.display,
                    args.scope.as_str()
                )),
                ..Default::default()
            });
            continue;
        };

        let server = if args.oauth {
            ServerConfig::oauth_remote(&mcp_url)
        } else {
            ServerConfig::remote(&mcp_url, key)
        };
        let entry = spec.compile(&server);
        let auth_hint = if args.oauth {
            match spec.oauth {
                OauthSupport::Capable { hint } => Some(hint.to_string()),
                OauthSupport::Unsupported { .. } => None,
            }
        } else {
            None
        };

        if args.dry_run {
            match writer::render(&path, spec.format, &entry) {
                Ok(rendered) => outcomes.push(ClientOutcome {
                    id,
                    display: spec.display.to_string(),
                    format: spec.format.as_str().to_string(),
                    path: Some(path),
                    action: Some(rendered.action.as_str().to_string()),
                    notes: entry.notes.clone(),
                    dry_run_contents: Some(rendered.contents),
                    auth_hint,
                    // Never echo a caller-owned credential.
                    key: None,
                    ..Default::default()
                }),
                Err(error) => outcomes.push(ClientOutcome {
                    id,
                    display: spec.display.to_string(),
                    format: spec.format.as_str().to_string(),
                    path: Some(path),
                    notes: entry.notes.clone(),
                    error: Some(error.message),
                    manual_snippet: error.manual_snippet,
                    ..Default::default()
                }),
            }
        } else {
            match writer::write(&path, spec.format, &entry) {
                Ok(action) => outcomes.push(ClientOutcome {
                    id,
                    display: spec.display.to_string(),
                    format: spec.format.as_str().to_string(),
                    path: Some(path),
                    action: Some(action.as_str().to_string()),
                    notes: entry.notes.clone(),
                    auth_hint,
                    key: None,
                    ..Default::default()
                }),
                Err(error) => outcomes.push(ClientOutcome {
                    id,
                    display: spec.display.to_string(),
                    format: spec.format.as_str().to_string(),
                    path: Some(path),
                    notes: entry.notes.clone(),
                    error: Some(error.message),
                    manual_snippet: error.manual_snippet,
                    ..Default::default()
                }),
            }
        }
    }

    Ok(ConnectResult {
        outcomes,
        key_origin: (!args.oauth).then_some(KeyOrigin::Provided),
        agents_md: None,
        dry_run: args.dry_run,
        stdio: false,
        oauth: args.oauth,
        url: mcp_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::connect::clients::{Os, Scope};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let serial = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("lific-config-only-{}-{serial}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn base() -> (TestDir, PathBase) {
        let temp = TestDir::new();
        let base = PathBase {
            home: temp.path().join("home"),
            project: temp.path().join("repo"),
            os: Os::Linux,
            appdata: None,
        };
        std::fs::create_dir_all(&base.home).unwrap();
        std::fs::create_dir_all(&base.project).unwrap();
        (temp, base)
    }

    fn args(key: Option<&str>, dry_run: bool) -> ConnectArgs {
        ConnectArgs {
            clients: vec!["codex".into()],
            scope: Scope::Global,
            stdio: false,
            oauth: false,
            url: Some("https://lific.example/mcp".into()),
            key: key.map(str::to_string),
            user: None,
            yes: true,
            dry_run,
            skip_agents: true,
        }
    }

    #[test]
    fn writes_without_a_database_and_never_returns_the_key() {
        let (_temp, base) = base();
        let cfg = Config::default();
        let result = run(&args(Some("lific_sk-test-secret"), false), &cfg, &base).unwrap();
        assert_eq!(result.outcomes.len(), 1);
        assert!(result.outcomes[0].key.is_none());
        let text = std::fs::read_to_string(base.home.join(".codex/config.toml")).unwrap();
        assert!(text.contains("bearer_token_env_var"));
        assert!(!text.contains("lific_sk-test-secret"));
    }

    #[test]
    fn dry_run_does_not_write_and_uses_only_the_placeholder() {
        let (_temp, base) = base();
        let cfg = Config::default();
        let result = run(&args(Some("lific_sk-test-secret"), true), &cfg, &base).unwrap();
        assert!(!base.home.join(".codex/config.toml").exists());
        let rendered = result.outcomes[0].dry_run_contents.as_deref().unwrap();
        assert!(!rendered.contains("lific_sk-test-secret"));
    }

    #[test]
    fn rejects_implicit_instance_and_agents_md_mutation() {
        let (_temp, base) = base();
        let cfg = Config::default();
        let mut input = args(Some("key"), false);
        input.url = None;
        assert!(
            run(&input, &cfg, &base)
                .unwrap_err()
                .contains("explicit --url")
        );
        input.url = Some("https://lific.example/mcp".into());
        input.skip_agents = false;
        assert!(
            run(&input, &cfg, &base)
                .unwrap_err()
                .contains("--skip-agents")
        );
    }
}
