//! Tests for git garbage collection.

use std::env;
use std::ffi::OsStr;
use std::path::PathBuf;

use crate::prelude::*;
use cargo_test_support::git;
use cargo_test_support::git::cargo_uses_gitoxide;
use cargo_test_support::paths;
use cargo_test_support::project;
use cargo_test_support::registry::Package;
use url::Url;

pub fn find_index() -> PathBuf {
    let dir = paths::home().join(".cargo/registry/index");
    dir.read_dir().unwrap().next().unwrap().unwrap().path()
}

fn run_test(path_env: Option<&OsStr>) {
    const N: usize = 50;

    let foo = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.0.1"
                authors = []

                [dependencies]
                bar = "*"
            "#,
        )
        .file("src/lib.rs", "")
        .build();
    Package::new("bar", "0.1.0").publish();

    foo.cargo("check").run();

    let index = find_index();
    let path = paths::home().join("tmp");
    let url = Url::from_file_path(&path).unwrap().to_string();
    let repo = git::init(&path);

    for _ in 0..N {
        git::commit(&repo);
        // Fetch from the temp repo into the index
        let index_repo = gix::open(&index).unwrap();
        git::fetch(
            &index_repo,
            &url,
            "refs/heads/master:refs/remotes/foo/master",
        );
    }
    drop(repo);
    Package::new("bar", "0.1.1").publish();

    let before = find_index()
        .join(".git/objects/pack")
        .read_dir()
        .unwrap()
        .count();
    assert!(before > N);

    let mut cmd = foo.cargo("update");
    cmd.env("__CARGO_PACKFILE_LIMIT", "10");
    if let Some(path) = path_env {
        cmd.env("PATH", path);
    }
    cmd.env("CARGO_LOG", "trace");
    cmd.run();
    let after = find_index()
        .join(".git/objects/pack")
        .read_dir()
        .unwrap()
        .count();
    assert!(
        after < before,
        "packfiles before: {}\n\
         packfiles after:  {}",
        before,
        after
    );
}

#[cargo_test(requires = "git")]
fn use_git_gc() {
    if cargo_uses_gitoxide() {
        // gitoxide uses reinitialize instead of git gc, which has different behavior.
        // The test would need to be updated to handle the reinitialize flow.
        return;
    }
    run_test(None);
}

#[cargo_test]
fn avoid_using_git() {
    if cargo_uses_gitoxide() {
        // file protocol without git binary is currently not possible - needs built-in upload-pack.
        // See https://github.com/Byron/gitoxide/issues/734 (support for the file protocol) progress updates.
        return;
    }
    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = env::split_paths(&path).collect::<Vec<_>>();
    let idx = paths
        .iter()
        .position(|p| p.join("git").exists() || p.join("git.exe").exists());
    match idx {
        Some(i) => {
            paths.remove(i);
        }
        None => return,
    }
    run_test(Some(&env::join_paths(&paths).unwrap()));
}
