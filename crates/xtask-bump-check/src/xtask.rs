//! ```text
//! NAME
//!         xtask-bump-check
//!
//! SYNOPSIS
//!         xtask-bump-check --base-rev <REV> --head-rev <REV>
//!
//! DESCRIPTION
//!         Checks if there is any member got changed since a base commit
//!         but forgot to bump its version.
//! ```

#![allow(clippy::print_stdout)] // Fine for build utilities

use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::task;

use cargo::CargoResult;
use cargo::core::Package;
use cargo::core::Registry;
use cargo::core::SourceId;
use cargo::core::Workspace;
use cargo::core::dependency::Dependency;
use cargo::sources::source::QueryKind;
use cargo::util::cache_lock::CacheLockMode;
use cargo::util::command_prelude::*;
use cargo_util::ProcessBuilder;

const UPSTREAM_BRANCH: &str = "master";
const STATUS: &str = "BumpCheck";

pub fn cli() -> clap::Command {
    clap::Command::new("xtask-bump-check")
        .arg(
            opt(
                "verbose",
                "Use verbose output (-vv very verbose/build.rs output)",
            )
            .short('v')
            .action(ArgAction::Count)
            .global(true),
        )
        .arg(
            flag("quiet", "Do not print cargo log messages")
                .short('q')
                .global(true),
        )
        .arg(
            opt("color", "Coloring: auto, always, never")
                .value_name("WHEN")
                .global(true),
        )
        .arg(opt("base-rev", "Git revision to lookup for a baseline"))
        .arg(opt("head-rev", "Git revision with changes"))
        .arg(flag("frozen", "Require Cargo.lock and cache to be up-to-date").global(true))
        .arg(flag("locked", "Require Cargo.lock to be up-to-date").global(true))
        .arg(flag("offline", "Run without accessing the network").global(true))
        .arg(multi_opt("config", "KEY=VALUE", "Override a configuration value").global(true))
        .arg(flag("github", "Group output using GitHub's syntax"))
        .arg(
            Arg::new("unstable-features")
                .help("Unstable (nightly-only) flags to Cargo, see 'cargo -Z help' for details")
                .short('Z')
                .value_name("FLAG")
                .action(ArgAction::Append)
                .global(true),
        )
}

pub fn exec(args: &clap::ArgMatches, gctx: &mut cargo::util::GlobalContext) -> cargo::CliResult {
    global_context_configure(gctx, args)?;

    bump_check(args, gctx)?;

    Ok(())
}

fn global_context_configure(gctx: &mut GlobalContext, args: &ArgMatches) -> CliResult {
    let verbose = args.verbose();
    // quiet is unusual because it is redefined in some subcommands in order
    // to provide custom help text.
    let quiet = args.flag("quiet");
    let color = args.get_one::<String>("color").map(String::as_str);
    let frozen = args.flag("frozen");
    let locked = args.flag("locked");
    let offline = args.flag("offline");
    let mut unstable_flags = vec![];
    if let Some(values) = args.get_many::<String>("unstable-features") {
        unstable_flags.extend(values.cloned());
    }
    let mut config_args = vec![];
    if let Some(values) = args.get_many::<String>("config") {
        config_args.extend(values.cloned());
    }
    gctx.configure(
        verbose,
        quiet,
        color,
        frozen,
        locked,
        offline,
        &None,
        &unstable_flags,
        &config_args,
    )?;
    Ok(())
}

/// Run a git command and return its stdout as a string.
fn git_output(repo_path: &Path, args: &[&str]) -> CargoResult<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Parse a revision spec and return the commit SHA.
fn revparse(repo_path: &Path, spec: &str) -> CargoResult<String> {
    git_output(repo_path, &["rev-parse", spec])
}

/// Main entry of `xtask-bump-check`.
///
/// Assumption: version number are incremental. We never have point release for old versions.
fn bump_check(args: &clap::ArgMatches, gctx: &cargo::util::GlobalContext) -> CargoResult<()> {
    let ws = args.workspace(gctx)?;
    let repo_path = ws.root();
    let base_commit = get_base_commit(gctx, args, repo_path)?;
    let head_commit = get_head_commit(args, repo_path)?;
    let referenced_commit = get_referenced_commit(repo_path, &base_commit)?;
    let github = args.get_flag("github");
    let status = |msg: &str| gctx.shell().status(STATUS, msg);

    let crates_not_check_against_channels = [
        // High false positive rate between beta branch and requisite version bump soon after
        //
        // Low risk because we always bump the "major" version after beta branch; we are
        // only losing out on checks for patch releases.
        //
        // Note: this is already skipped in `changed`
        "cargo",
        // Don't check against beta and stable branches,
        // as the publish of these crates are not tied with Rust release process.
        // See `TO_PUBLISH` in publish.py.
        "home",
    ];

    status(&format!("base commit `{}`", base_commit))?;
    status(&format!("head commit `{}`", head_commit))?;

    let mut needs_bump = Vec::new();
    if github {
        println!("::group::Checking for bumps of changed packages");
    }
    let changed_members = changed(&ws, repo_path, &base_commit, &head_commit)?;
    check_crates_io(&ws, &changed_members, &mut needs_bump)?;
    if let Some(ref referenced_commit) = referenced_commit {
        status(&format!("compare against `{}`", referenced_commit))?;
        for referenced_member in checkout_ws(&ws, repo_path, referenced_commit)?.members() {
            let pkg_name = referenced_member.name().as_str();

            if crates_not_check_against_channels.contains(&pkg_name) {
                continue;
            }

            let Some(changed_member) = changed_members.get(pkg_name) else {
                tracing::trace!("skipping {pkg_name}, may be removed or not published");
                continue;
            };

            if changed_member.version() <= referenced_member.version() {
                needs_bump.push(*changed_member);
            }
        }
    }
    if !needs_bump.is_empty() {
        needs_bump.sort();
        needs_bump.dedup();
        let mut msg = String::new();
        msg.push_str("Detected changes in these crates but no version bump found:\n");
        for pkg in needs_bump {
            writeln!(&mut msg, "  {}@{}", pkg.name(), pkg.version())?;
        }
        msg.push_str("\nPlease bump at least one patch version in each corresponding Cargo.toml.");
        anyhow::bail!(msg)
    }
    if github {
        println!("::endgroup::");
    }

    if let Some(ref referenced_commit) = referenced_commit {
        if github {
            println!("::group::SemVer Checks against {}", referenced_commit);
        }
        let mut cmd = ProcessBuilder::new("cargo");
        cmd.arg("semver-checks")
            .arg("--workspace")
            .arg("--baseline-rev")
            .arg(referenced_commit);
        for krate in crates_not_check_against_channels {
            cmd.args(&["--exclude", krate]);
        }
        gctx.shell().status("Running", &cmd)?;
        cmd.exec()?;
        if github {
            println!("::endgroup::");
        }
    }

    // Even when we test against baseline-rev, we still need to make sure a
    // change doesn't violate SemVer rules against crates.io releases. The
    // possibility of this happening is nearly zero but no harm to check twice.
    if github {
        println!("::group::SemVer Checks against crates.io");
    }

    let mut cmd = ProcessBuilder::new("cargo");
    cmd.arg("semver-checks")
        .arg("check-release")
        .arg("--workspace")
        .args(&["--exclude", "cargo"]);

    gctx.shell().status("Running", &cmd)?;
    cmd.exec()?;

    // Cargo has mutually exclusive features for different HTTP backends, so
    // pass a specific `--features` instead of including this in the
    // `--all-features` performed by the previous command.
    let mut cmd = ProcessBuilder::new("cargo");
    cmd.arg("semver-checks")
        .arg("check-release")
        .args(&["--package", "cargo"])
        .arg("--default-features")
        .args(&["--features", "all-static"]);

    gctx.shell().status("Running", &cmd)?;
    cmd.exec()?;

    if github {
        println!("::endgroup::");
    }

    status("no version bump needed for member crates.")?;

    Ok(())
}

/// Returns the commit of upstream `master` branch if `base-rev` is missing.
fn get_base_commit(
    gctx: &GlobalContext,
    args: &clap::ArgMatches,
    repo_path: &Path,
) -> CargoResult<String> {
    let base_commit = match args.get_one::<String>("base-rev") {
        Some(sha) => revparse(repo_path, sha)?,
        None => {
            // Find remote branches ending with /master
            let branches_output = git_output(repo_path, &["branch", "-r"])?;
            let upstream_branches: Vec<&str> = branches_output
                .lines()
                .map(|s| s.trim())
                .filter(|name| name.ends_with(&format!("/{UPSTREAM_BRANCH}")))
                .collect();

            if upstream_branches.is_empty() {
                anyhow::bail!(
                    "could not find `base-sha` for `{UPSTREAM_BRANCH}`, pass it in directly"
                );
            }

            let upstream_ref = upstream_branches[0];
            if upstream_branches.len() > 1 {
                let _ = gctx.shell().warn(format!(
                    "multiple `{UPSTREAM_BRANCH}` found, picking {upstream_ref}"
                ));
            }
            revparse(repo_path, upstream_ref)?
        }
    };
    Ok(base_commit)
}

/// Returns `HEAD` of the Git repository if `head-rev` is missing.
fn get_head_commit(args: &clap::ArgMatches, repo_path: &Path) -> CargoResult<String> {
    let head_commit = match args.get_one::<String>("head-rev") {
        Some(sha) => revparse(repo_path, sha)?,
        None => revparse(repo_path, "HEAD")?,
    };
    Ok(head_commit)
}

/// Gets the referenced commit to compare if version bump needed.
///
/// * When merging into nightly, check the version with beta branch
/// * When merging into beta, check the version with stable branch
/// * When merging into stable, check against crates.io registry directly
fn get_referenced_commit(repo_path: &Path, base: &str) -> CargoResult<Option<String>> {
    let [beta, stable] = beta_and_stable_branch(repo_path)?;
    let beta_commit = revparse(repo_path, &beta)?;
    let stable_commit = revparse(repo_path, &stable)?;

    let referenced_commit = if base == stable_commit {
        None
    } else if base == beta_commit {
        tracing::trace!("stable branch from `{}`", stable);
        Some(stable_commit)
    } else {
        tracing::trace!("beta branch from `{}`", beta);
        Some(beta_commit)
    };

    Ok(referenced_commit)
}

/// Get the current beta and stable branch in cargo repository.
///
/// Assumptions:
///
/// * The repository contains the full history of `<remote>/rust-1.*.0` branches.
/// * The version part of `<remote>/rust-1.*.0` always ends with a zero.
/// * The maximum version is for beta channel, and the second one is for stable.
fn beta_and_stable_branch(repo_path: &Path) -> CargoResult<[String; 2]> {
    let branches_output = git_output(repo_path, &["branch", "-r"])?;
    let mut release_branches: Vec<(semver::Version, String)> = Vec::new();

    for line in branches_output.lines() {
        let name = line.trim();
        let Some((_, version_str)) = name.split_once("/rust-") else {
            tracing::trace!("branch `{name}` is not in the format of `<remote>/rust-<semver>`");
            continue;
        };
        let Ok(version) = version_str.parse::<semver::Version>() else {
            tracing::trace!("branch `{name}` is not a valid semver: `{version_str}`");
            continue;
        };
        release_branches.push((version, name.to_string()));
    }
    release_branches.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    release_branches.dedup_by(|a, b| a.0 == b.0);

    let beta = release_branches.pop().unwrap();
    let stable = release_branches.pop().unwrap();

    assert_eq!(beta.0.major, 1);
    assert_eq!(beta.0.patch, 0);
    assert_eq!(stable.0.major, 1);
    assert_eq!(stable.0.patch, 0);
    assert_ne!(beta.0.minor, stable.0.minor);

    Ok([beta.1, stable.1])
}

/// Lists all changed workspace members between two commits.
fn changed<'r, 'ws>(
    ws: &'ws Workspace<'_>,
    repo_path: &Path,
    base_commit: &str,
    head_commit: &str,
) -> CargoResult<HashMap<&'ws str, &'ws Package>> {
    let root_pkg_name = ws.current()?.name(); // `cargo` crate.
    let ws_members = ws
        .members()
        .filter(|pkg| pkg.name() != root_pkg_name) // Only take care of sub crates here.
        .filter(|pkg| pkg.publish() != &Some(vec![])) // filter out `publish = false`
        .map(|pkg| {
            // Having relative package root path so that we can compare with
            // paths of changed files to determine which package has changed.
            let relative_pkg_root = pkg.root().strip_prefix(ws.root()).unwrap();
            (relative_pkg_root, pkg)
        })
        .collect::<Vec<_>>();

    let changed_files = symmetric_diff(repo_path, base_commit, head_commit)?;
    let mut changed_members = HashMap::new();

    for file_path in changed_files {
        let file_path = Path::new(&file_path);
        for (pkg_root, pkg) in ws_members.iter() {
            if file_path.starts_with(pkg_root) {
                changed_members.insert(pkg.name().as_str(), *pkg);
                break;
            }
        }
    }

    tracing::trace!("changed_members: {:?}", changed_members.keys());
    Ok(changed_members)
}

/// Using a "symmetric difference" between base and head.
/// Returns a list of changed file paths.
fn symmetric_diff(repo_path: &Path, base: &str, head: &str) -> CargoResult<Vec<String>> {
    // Find the merge base
    let ancestor = git_output(repo_path, &["merge-base", base, head])?;
    tracing::info!(merge_base = %ancestor, base = %base, head = %head, "git diff base...head");

    // Get the list of changed files between ancestor and head
    let diff_output = git_output(
        repo_path,
        &["diff-tree", "-r", "--name-only", &ancestor, head],
    )?;

    let files: Vec<String> = diff_output
        .lines()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    Ok(files)
}

/// Compares version against published crates on crates.io.
///
/// Assumption: We always release a version larger than all existing versions.
fn check_crates_io<'a>(
    ws: &Workspace<'a>,
    changed_members: &HashMap<&'a str, &'a Package>,
    needs_bump: &mut Vec<&'a Package>,
) -> CargoResult<()> {
    let gctx = ws.gctx();
    let source_id = SourceId::crates_io(gctx)?;
    let mut registry = ws.package_registry()?;
    let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
    registry.lock_patches();
    gctx.shell().status(
        STATUS,
        format_args!("compare against `{}`", source_id.display_registry_name()),
    )?;
    for (name, member) in changed_members {
        let current = member.version();
        let version_req = format!(">={current}");
        let query = Dependency::parse(*name, Some(&version_req), source_id)?;
        let possibilities = loop {
            // Exact to avoid returning all for path/git
            match registry.query_vec(&query, QueryKind::Exact) {
                task::Poll::Ready(res) => {
                    break res?;
                }
                task::Poll::Pending => registry.block_until_ready()?,
            }
        };
        if possibilities.is_empty() {
            tracing::trace!("dep `{name}` has no version greater than or equal to `{current}`");
        } else {
            tracing::trace!(
                "`{name}@{current}` needs a bump because its should have a version newer than crates.io: {:?}`",
                possibilities
                    .iter()
                    .map(|s| s.as_summary())
                    .map(|s| format!("{}@{}", s.name(), s.version()))
                    .collect::<Vec<_>>(),
            );
            needs_bump.push(member);
        }
    }

    Ok(())
}

/// Checkouts a temporary workspace to do further version comparisons.
fn checkout_ws<'gctx>(
    ws: &Workspace<'gctx>,
    repo_path: &Path,
    referenced_commit: &str,
) -> CargoResult<Workspace<'gctx>> {
    let repo_path_str = repo_path.to_str().unwrap();
    // Put it under `target/cargo-<short-id>`
    let short_id = &referenced_commit[..7];
    let checkout_path = ws.target_dir().join(format!("cargo-{short_id}"));
    let checkout_path = checkout_path.as_path_unlocked();
    let _ = fs::remove_dir_all(checkout_path);

    // Clone locally
    let status = Command::new("git")
        .args(["clone", "--local", repo_path_str])
        .arg(checkout_path)
        .status()?;
    if !status.success() {
        anyhow::bail!("git clone --local failed");
    }

    // Reset to the referenced commit
    let status = Command::new("git")
        .args(["reset", "--hard", referenced_commit])
        .current_dir(checkout_path)
        .status()?;
    if !status.success() {
        anyhow::bail!("git reset --hard failed");
    }

    Workspace::new(&checkout_path.join("Cargo.toml"), ws.gctx())
}

#[test]
fn verify_cli() {
    cli().debug_assert();
}
