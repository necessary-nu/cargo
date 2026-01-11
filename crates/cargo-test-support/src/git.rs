//! # Git Testing Support
//!
//! ## Creating a git dependency
//! [`new()`] is an easy way to create a new git repository containing a
//! project that you can then use as a dependency. It will automatically add all
//! the files you specify in the project and commit them to the repository.
//!
//! ### Example:
//!
//! ```no_run
//! # use cargo_test_support::project;
//! # use cargo_test_support::basic_manifest;
//! # use cargo_test_support::git;
//! let git_project = git::new("dep1", |project| {
//!     project
//!         .file("Cargo.toml", &basic_manifest("dep1", "1.0.0"))
//!         .file("src/lib.rs", r#"pub fn f() { println!("hi!"); } "#)
//! });
//!
//! // Use the `url()` method to get the file url to the new repository.
//! let p = project()
//!     .file("Cargo.toml", &format!(r#"
//!         [package]
//!         name = "a"
//!         version = "1.0.0"
//!
//!         [dependencies]
//!         dep1 = {{ git = '{}' }}
//!     "#, git_project.url()))
//!     .file("src/lib.rs", "extern crate dep1;")
//!     .build();
//! ```
//!
//! ## Manually creating repositories
//!
//! [`repo()`] can be used to create a [`RepoBuilder`] which provides a way of
//! adding files to a blank repository and committing them.
//!
//! If you want to then manipulate the repository (such as adding new files or
//! tags), use the helper functions in this file to interact with the repository.

use crate::{Project, ProjectBuilder, SymlinkBuilder, paths::CargoPathExt, project};
use gix::objs::WriteTo;
use gix::prelude::Write as GixWrite;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use url::Url;

/// Manually construct a [`Repository`]
///
/// See also [`new`], [`repo`]
#[must_use]
pub struct RepoBuilder {
    path: PathBuf,
    files: Vec<PathBuf>,
}

/// See [`new`]
pub struct Repository(PathBuf);

/// Create a [`RepoBuilder`] to build a new git repository.
///
/// Call [`RepoBuilder::build()`] to finalize and create the repository.
pub fn repo(p: &Path) -> RepoBuilder {
    RepoBuilder::init(p)
}

impl RepoBuilder {
    pub fn init(p: &Path) -> RepoBuilder {
        t!(fs::create_dir_all(p.parent().unwrap()));
        let _repo = init(p); // initialize and discard, we just need the path
        RepoBuilder {
            path: p.to_path_buf(),
            files: Vec::new(),
        }
    }

    /// Add a file to the repository.
    pub fn file(self, path: &str, contents: &str) -> RepoBuilder {
        let mut me = self.nocommit_file(path, contents);
        me.files.push(PathBuf::from(path));
        me
    }

    /// Create a symlink to a directory
    pub fn nocommit_symlink_dir<T: AsRef<Path>>(self, dst: T, src: T) -> Self {
        SymlinkBuilder::new_dir(self.path.join(dst), self.path.join(src)).mk();
        self
    }

    /// Add a file that will be left in the working directory, but not added
    /// to the repository.
    pub fn nocommit_file(self, path: &str, contents: &str) -> RepoBuilder {
        let dst = self.path.join(path);
        t!(fs::create_dir_all(dst.parent().unwrap()));
        t!(fs::write(&dst, contents));
        self
    }

    /// Create the repository and commit the new files.
    pub fn build(self) -> Repository {
        for file in self.files.iter() {
            git_add(&self.path, file);
        }
        git_commit(&self.path, "Initial commit");
        Repository(self.path)
    }
}

impl Repository {
    pub fn root(&self) -> &Path {
        &self.0
    }

    pub fn url(&self) -> Url {
        self.0.to_url()
    }

    pub fn revparse_head(&self) -> String {
        let repo = t!(gix::open(&self.0));
        t!(repo.head_id()).to_string()
    }
}

/// Initialize a new repository at the given path.
pub fn init(path: &Path) -> gix::Repository {
    set_test_env();
    t!(fs::create_dir_all(path));

    // Initialize repository using gix
    let repo = t!(gix::init(path));

    // Set default config for the repo by writing directly to .git/config
    let config_path = path.join(".git/config");
    let config_content = t!(fs::read_to_string(&config_path));
    let new_config = format!(
        "{}\n[user]\n\temail = foo@bar.com\n\tname = Foo Bar\n",
        config_content.trim_end()
    );
    t!(fs::write(&config_path, new_config));

    repo
}

fn set_test_env() {
    use crate::paths::global_root;

    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Prevent reading system/global git configs during tests
        // SAFETY: Tests run single-threaded during this initialization
        unsafe {
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            let blank_path = global_root().join("blank_git_search_path");
            t!(fs::create_dir_all(&blank_path));
            // Create a global git config with init.defaultBranch=master for consistency
            // and user identity for reflog
            let config_path = blank_path.join("config");
            t!(fs::write(&config_path, "[init]\n\tdefaultBranch = master\n[user]\n\temail = test@test.com\n\tname = Test User\n"));
            std::env::set_var("GIT_CONFIG_GLOBAL", &config_path);
            std::env::set_var("HOME", blank_path.clone());
            std::env::set_var("XDG_CONFIG_HOME", blank_path.clone());
            std::env::set_var("GIT_TERMINAL_PROMPT", "0");
        }
    })
}

/// Create a new [`Project`] in a git [`Repository`]
pub fn new<F>(name: &str, callback: F) -> Project
where
    F: FnOnce(ProjectBuilder) -> ProjectBuilder,
{
    new_repo(name, callback).0
}

/// Create a new [`Project`] with access to the repository
pub fn new_repo<F>(name: &str, callback: F) -> (Project, gix::Repository)
where
    F: FnOnce(ProjectBuilder) -> ProjectBuilder,
{
    let mut git_project = project().at(name);
    git_project = callback(git_project);
    let git_project = git_project.build();

    let repo = init(&git_project.root());
    let repo_path = repo.workdir().unwrap();
    add_all(repo_path);
    git_commit(repo_path, "Initial commit");

    // Create a lightweight tag pointing to HEAD.
    // This is needed because gix's file:// protocol fetch requires at least one tag
    // to be present, otherwise object lookup fails during fetch.
    let head_id = t!(repo.head_id());
    let tag_ref_path = repo_path.join(".git/refs/tags/__dummy_tag__");
    t!(fs::write(&tag_ref_path, format!("{}\n", head_id)));

    (git_project, repo)
}

fn git_add(repo_path: &Path, file: &Path) {
    // Use git CLI to add files - this ensures the index is written in a format
    // that git commit will properly read. Using gix to write the index directly
    // can result in entries not being committed by `git commit`.
    let status = Command::new("git")
        .args(["add", "-f"])
        .arg(file)
        .current_dir(repo_path)
        .status()
        .expect("git add failed");
    assert!(status.success(), "git add -f {} failed", file.display());
}

/// Add all files in the working directory to the git index
pub fn add_all(repo_path: &Path) {
    // Use git CLI to properly respect .gitignore
    let status = Command::new("git")
        .args(["add", "."])
        .current_dir(repo_path)
        .status()
        .expect("git add . failed");
    assert!(status.success(), "git add . failed");
}

fn add_dir_to_index(
    repo: &gix::Repository,
    repo_path: &Path,
    dir: &Path,
    index_file: &mut gix::index::File,
) {
    for entry in t!(fs::read_dir(dir)) {
        let entry = t!(entry);
        let path = entry.path();
        let file_name = path.file_name().unwrap().to_str().unwrap();

        // Skip .git directory
        if file_name == ".git" {
            continue;
        }

        let metadata = t!(entry.metadata());
        if metadata.is_dir() {
            // Check if this is a submodule workdir (has .git file, not directory)
            let git_path = path.join(".git");
            if git_path.exists() && git_path.is_file() {
                // This is a submodule - skip recursing, it's handled by gitlink
                continue;
            }
            // Recurse into subdirectories
            add_dir_to_index(repo, repo_path, &path, index_file);
        } else if metadata.is_file() {
            // Add file to index
            let relative_path = path.strip_prefix(repo_path).unwrap();
            let relative_str = relative_path.to_str().unwrap().replace('\\', "/");

            // Read file and write as blob
            let content = t!(fs::read(&path));
            let blob_id = repo
                .objects
                .write_buf(gix::object::Kind::Blob, &content)
                .expect("failed to write blob");

            // Determine file mode
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 != 0 {
                    gix::index::entry::Mode::FILE_EXECUTABLE
                } else {
                    gix::index::entry::Mode::FILE
                }
            };
            #[cfg(not(unix))]
            let mode = gix::index::entry::Mode::FILE;

            // Use default stat (we don't need accurate timestamps for tests)
            let stat = gix::index::entry::Stat::default();

            // Check if entry exists
            let entry_idx = index_file
                .entries()
                .iter()
                .position(|e| e.path(index_file) == relative_str.as_bytes());

            if let Some(idx) = entry_idx {
                let entry = &mut index_file.entries_mut()[idx];
                entry.id = blob_id;
                entry.mode = mode;
                entry.stat = stat;
            } else {
                index_file.dangerously_push_entry(
                    stat,
                    blob_id,
                    gix::index::entry::Flags::empty(),
                    mode,
                    relative_str.as_str().into(),
                );
            }
        } else if metadata.is_symlink() {
            // Handle symlinks
            let relative_path = path.strip_prefix(repo_path).unwrap();
            let relative_str = relative_path.to_str().unwrap().replace('\\', "/");

            // Read symlink target and write as blob
            let target = t!(fs::read_link(&path));
            let target_bytes = target.to_str().unwrap().as_bytes();
            let blob_id = repo
                .objects
                .write_buf(gix::object::Kind::Blob, target_bytes)
                .expect("failed to write symlink blob");

            // Use default stat (we don't need accurate timestamps for tests)
            let stat = gix::index::entry::Stat::default();

            let entry_idx = index_file
                .entries()
                .iter()
                .position(|e| e.path(index_file) == relative_str.as_bytes());

            if let Some(idx) = entry_idx {
                let entry = &mut index_file.entries_mut()[idx];
                entry.id = blob_id;
                entry.mode = gix::index::entry::Mode::SYMLINK;
                entry.stat = stat;
            } else {
                index_file.dangerously_push_entry(
                    stat,
                    blob_id,
                    gix::index::entry::Flags::empty(),
                    gix::index::entry::Mode::SYMLINK,
                    relative_str.as_str().into(),
                );
            }
        }
    }
}

/// Add all files in the working directory to the git index.
pub fn add(repo: &gix::Repository) {
    add_all(repo.workdir().unwrap());
}

/// Add a git submodule to the repository.
pub fn add_submodule(repo: &gix::Repository, url: &str, path: &Path) {
    let repo_path = repo.workdir().unwrap();
    let path_str = path.to_str().unwrap();

    // Use git submodule add command - this correctly sets up everything
    let status = Command::new("git")
        .args(["-c", "protocol.file.allow=always", "submodule", "add"])
        .arg(url)
        .arg(path_str)
        .current_dir(repo_path)
        .status()
        .expect("git submodule add failed");
    assert!(status.success(), "git submodule add failed");

    // Configure user for submodule commits
    let submodule_path = repo_path.join(path);
    let status = Command::new("git")
        .args(["config", "user.email", "foo@bar.com"])
        .current_dir(&submodule_path)
        .status()
        .expect("git config user.email failed");
    assert!(status.success());
    let status = Command::new("git")
        .args(["config", "user.name", "Foo Bar"])
        .current_dir(&submodule_path)
        .status()
        .expect("git config user.name failed");
    assert!(status.success());
}

/// Add a submodule entry to the index without cloning.
///
/// This is used for testing failure scenarios where the submodule URL
/// is unreachable. It creates a gitlink entry (mode 160000) in the index
/// without actually fetching the submodule content.
pub fn add_submodule_unchecked(
    repo: &gix::Repository,
    url: &str,
    path: &Path,
    commit_id: gix::ObjectId,
) {
    let repo_path = repo.workdir().unwrap();
    let path_str = path.to_str().unwrap();

    // 1. Write/append to .gitmodules
    let gitmodules_content = format!(
        "[submodule \"{}\"]\n\tpath = {}\n\turl = {}\n",
        path_str, path_str, url
    );
    let gitmodules_path = repo_path.join(".gitmodules");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitmodules_path)
        .expect("failed to open .gitmodules");
    file.write_all(gitmodules_content.as_bytes())
        .expect("failed to write .gitmodules");

    // 2. Stage .gitmodules using git CLI
    let status = Command::new("git")
        .args(["add", ".gitmodules"])
        .current_dir(repo_path)
        .status()
        .expect("git add .gitmodules failed");
    assert!(status.success(), "git add .gitmodules failed");

    // 3. Add gitlink entry using git update-index
    // Mode 160000 is the gitlink mode for submodules
    let status = Command::new("git")
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &commit_id.to_string(),
            path_str,
        ])
        .current_dir(repo_path)
        .status()
        .expect("git update-index failed");
    assert!(status.success(), "git update-index failed");
}

fn git_commit(repo_path: &Path, message: &str) {
    // Use git CLI for commit to ensure proper object storage
    // --allow-empty is needed for tests that create multiple commits without changes
    let status = Command::new("git")
        .args(["commit", "--allow-empty", "-m", message])
        .current_dir(repo_path)
        .status()
        .expect("git commit failed");
    assert!(status.success(), "git commit failed");

    // Update the dummy tag to point to the new HEAD.
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .expect("git rev-parse HEAD failed");
    assert!(output.status.success(), "git rev-parse HEAD failed");
    let head_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let repo = t!(gix::open(repo_path));
    let tag_ref_path = repo.git_dir().join("refs/tags/__dummy_tag__");
    if tag_ref_path.exists() {
        t!(fs::write(&tag_ref_path, format!("{}\n", head_id)));
    }
}

/// Commit changes to the git repository.
/// Returns the new HEAD commit id.
pub fn commit(repo: &gix::Repository) -> gix::ObjectId {
    let repo_path = repo.workdir().unwrap();
    git_commit(repo_path, "test");
    // Reopen repo to get fresh refs
    let fresh_repo = t!(gix::open(repo_path));
    // Return the new HEAD commit id (dummy tag is updated in git_commit)
    t!(fresh_repo.head_id()).detach()
}

/// Create a new tag in the git repository
pub fn tag(repo: &gix::Repository, name: &str) {
    let repo_path = repo.workdir().unwrap();
    // Reopen repo to get fresh refs
    let fresh_repo = t!(gix::open(repo_path));
    let head_id = t!(fresh_repo.head_id());

    // Create the annotated tag object
    let time = gix::date::Time::now_local_or_utc();

    // Write tag object in raw format
    let tag_content = format!(
        "object {}\ntype commit\ntag {}\ntagger Foo Bar <foo@bar.com> {} +0000\n\nmake a new tag\n",
        head_id, name, time.seconds
    );

    let tag_oid = fresh_repo
        .objects
        .write_buf(gix::object::Kind::Tag, tag_content.as_bytes())
        .expect("failed to write tag");

    // Create the tag reference
    let tag_ref_path = repo_path.join(".git/refs/tags").join(name);
    t!(fs::create_dir_all(tag_ref_path.parent().unwrap()));
    t!(fs::write(&tag_ref_path, format!("{}\n", tag_oid)));
}

/// Create a new branch at the given commit
pub fn branch(repo: &gix::Repository, name: &str, commit: &gix::ObjectId) {
    let repo_path = repo.workdir().unwrap();

    // Create the branch reference
    let branch_ref_path = repo_path.join(".git/refs/heads").join(name);
    t!(fs::create_dir_all(branch_ref_path.parent().unwrap()));
    t!(fs::write(&branch_ref_path, format!("{}\n", commit)));
}

/// Set HEAD to a symbolic reference
pub fn set_head(repo: &gix::Repository, refname: &str) {
    let repo_path = repo.workdir().unwrap();
    let head_path = repo_path.join(".git/HEAD");
    t!(fs::write(&head_path, format!("ref: {}\n", refname)));
}

/// Get the .git directory for a repo, handling gitlink files for submodules
fn get_git_dir(repo_path: &Path) -> PathBuf {
    let git_path = repo_path.join(".git");
    if git_path.is_file() {
        // .git is a gitlink file for submodules, format: "gitdir: <path>"
        let content = t!(fs::read_to_string(&git_path));
        let target = content.strip_prefix("gitdir: ").unwrap().trim();
        if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            repo_path.join(target)
        }
    } else {
        git_path
    }
}

/// Reset the repository to a given commit (hard reset)
pub fn reset_hard(repo: &gix::Repository, commit_spec: &str) {
    let repo_path = repo.workdir().unwrap().to_path_buf();
    let git_dir = get_git_dir(&repo_path);

    // Resolve the commit spec
    let commit_id = t!(repo.rev_parse_single(commit_spec)).detach();

    // Update HEAD to point to the commit
    // First check if HEAD is a symbolic ref to a branch
    let head_path = git_dir.join("HEAD");
    let head_content = t!(fs::read_to_string(&head_path));
    if head_content.starts_with("ref: ") {
        // HEAD is a symbolic ref, update the branch it points to
        let refname = head_content.trim().strip_prefix("ref: ").unwrap();
        let ref_path = git_dir.join(refname);
        t!(fs::create_dir_all(ref_path.parent().unwrap()));
        t!(fs::write(&ref_path, format!("{}\n", commit_id)));
    } else {
        // HEAD is detached, update it directly
        t!(fs::write(&head_path, format!("{}\n", commit_id)));
    }

    // Re-open the repository to pick up any config changes (like core.symlinks)
    // that may have been made after the original repo object was created.
    let repo = t!(gix::open(&repo_path));

    // Get the tree from the commit
    let commit = t!(repo.find_commit(commit_id));
    let tree = t!(commit.tree());

    // Update index from tree
    let mut index = t!(repo.index_from_tree(&tree.id));

    // Checkout the tree to the working directory
    let checkout_opts =
        t!(repo.checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping));

    t!(gix::worktree::state::checkout(
        &mut index,
        &repo_path,
        t!(repo.objects.clone().into_arc()),
        &gix::progress::Discard,
        &gix::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false),
        checkout_opts,
    ));

    t!(index.write(gix::index::write::Options::default()));
}

/// Get the head commit id
pub fn head_id(repo: &gix::Repository) -> gix::ObjectId {
    t!(repo.head_id()).detach()
}

/// Create a reference pointing to HEAD
pub fn create_ref(repo: &gix::Repository, refname: &str) {
    let repo_path = repo.workdir().unwrap();
    let git_dir = get_git_dir(repo_path);
    let head_id = t!(repo.head_id());

    // Create the reference file
    let ref_path = git_dir.join(refname);
    t!(fs::create_dir_all(ref_path.parent().unwrap()));
    t!(fs::write(&ref_path, format!("{}\n", head_id)));
}

/// Amend the most recent commit with a new message
pub fn amend_message(repo: &gix::Repository, message: &str) {
    let repo_path = repo.workdir().unwrap();
    // Use git CLI for --amend to avoid stale ref issues with gix
    let status = Command::new("git")
        .args(["commit", "--amend", "-m", message])
        .current_dir(repo_path)
        .status()
        .expect("git commit --amend failed");
    assert!(status.success(), "git commit --amend failed");

    // Update the dummy tag to point to the new HEAD commit.
    // If we don't do this, the tag keeps the old commit alive through gc.
    let tag_ref_path = repo_path.join(".git/refs/tags/__dummy_tag__");
    if tag_ref_path.exists() {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_path)
            .output()
            .expect("git rev-parse HEAD failed");
        assert!(output.status.success(), "git rev-parse HEAD failed");
        let new_head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        t!(fs::write(&tag_ref_path, format!("{}\n", new_head)));
    }

    // Expire reflogs and prune dangling objects immediately so amended commits
    // are truly gone. This is needed for tests that check behavior when a
    // submodule points to a commit that no longer exists.
    let status = Command::new("git")
        .args(["reflog", "expire", "--expire=now", "--all"])
        .current_dir(repo_path)
        .status()
        .expect("git reflog expire failed");
    assert!(status.success(), "git reflog expire failed");

    let status = Command::new("git")
        .args(["gc", "--prune=now"])
        .current_dir(repo_path)
        .status()
        .expect("git gc failed");
    assert!(status.success(), "git gc failed");
}

/// Sync submodule URLs from .gitmodules
pub fn submodule_sync(repo: &gix::Repository) {
    let repo_path = repo.workdir().unwrap();
    let gitmodules_path = repo_path.join(".gitmodules");

    if !gitmodules_path.exists() {
        return;
    }

    // Parse .gitmodules to extract submodule URLs
    let gitmodules_content = t!(fs::read_to_string(&gitmodules_path));
    let mut submodules: Vec<(String, String)> = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_url: Option<String> = None;

    for line in gitmodules_content.lines() {
        let line = line.trim();
        if line.starts_with("[submodule \"") && line.ends_with("\"]") {
            // Save previous submodule if complete
            if let (Some(name), Some(url)) = (current_name.take(), current_url.take()) {
                submodules.push((name, url));
            }
            // Extract submodule name
            let name = line
                .strip_prefix("[submodule \"")
                .unwrap()
                .strip_suffix("\"]")
                .unwrap()
                .to_string();
            current_name = Some(name);
        } else if line.starts_with("url = ") {
            current_url = Some(line.strip_prefix("url = ").unwrap().to_string());
        }
    }
    // Save last submodule if complete
    if let (Some(name), Some(url)) = (current_name, current_url) {
        submodules.push((name, url));
    }

    // Update .git/config with the URLs
    let config_path = repo_path.join(".git/config");
    let mut config_content = t!(fs::read_to_string(&config_path));

    for (name, url) in submodules {
        // Check if submodule section exists in config
        let section_header = format!("[submodule \"{}\"]", name);
        if config_content.contains(&section_header) {
            // Update existing URL (simple replacement, works for basic cases)
            // For more complex cases, a proper ini parser would be better
            let mut new_content = String::new();
            let mut in_section = false;
            let mut url_updated = false;
            for line in config_content.lines() {
                if line.trim().starts_with("[submodule") {
                    in_section = line.contains(&name);
                }
                if in_section && line.trim().starts_with("url = ") && !url_updated {
                    new_content.push_str(&format!("\turl = {}\n", url));
                    url_updated = true;
                } else {
                    new_content.push_str(line);
                    new_content.push('\n');
                }
            }
            config_content = new_content;
        } else {
            // Add new submodule section
            config_content.push_str(&format!("\n{}\n\turl = {}\n", section_header, url));
        }
    }

    t!(fs::write(&config_path, config_content));
}

/// Update submodules
pub fn submodule_update(repo: &gix::Repository, should_init: bool) {
    let repo_path = repo.workdir().unwrap();
    let gitmodules_path = repo_path.join(".gitmodules");

    if !gitmodules_path.exists() {
        return;
    }

    // Parse .gitmodules to get submodule info
    let gitmodules_content = t!(fs::read_to_string(&gitmodules_path));
    let mut submodules: Vec<(String, String, String)> = Vec::new(); // (name, path, url)
    let mut current_name: Option<String> = None;
    let mut current_path: Option<String> = None;
    let mut current_url: Option<String> = None;

    for line in gitmodules_content.lines() {
        let line = line.trim();
        if line.starts_with("[submodule \"") && line.ends_with("\"]") {
            if let (Some(name), Some(path), Some(url)) =
                (current_name.take(), current_path.take(), current_url.take())
            {
                submodules.push((name, path, url));
            }
            let name = line
                .strip_prefix("[submodule \"")
                .unwrap()
                .strip_suffix("\"]")
                .unwrap()
                .to_string();
            current_name = Some(name);
        } else if line.starts_with("path = ") {
            current_path = Some(line.strip_prefix("path = ").unwrap().to_string());
        } else if line.starts_with("url = ") {
            current_url = Some(line.strip_prefix("url = ").unwrap().to_string());
        }
    }
    if let (Some(name), Some(path), Some(url)) = (current_name, current_path, current_url) {
        submodules.push((name, path, url));
    }

    // Read index to get gitlink commit IDs
    let index_path = repo_path.join(".git/index");
    let index = t!(gix::index::File::at(
        &index_path,
        gix::hash::Kind::Sha1,
        false,
        gix::index::decode::Options::default(),
    ));

    for (_name, path, url) in submodules {
        let submodule_path = repo_path.join(&path);
        let modules_path = repo_path.join(".git/modules").join(&path);

        // Find the commit ID from the index
        let target_commit = index
            .entries()
            .iter()
            .find(|e| e.path(&index) == path.as_bytes())
            .map(|e| e.id);

        let target_commit = match target_commit {
            Some(id) => id,
            None => continue, // No gitlink in index
        };

        // Check if module is already initialized
        let module_exists = modules_path.join("config").exists();

        if !module_exists && !should_init {
            continue;
        }

        if !module_exists && should_init {
            // Initialize (clone) the submodule
            t!(fs::create_dir_all(&modules_path));

            let mut prep = t!(gix::prepare_clone(url.as_str(), &modules_path));
            prep = prep.with_remote_name("origin").expect("valid remote name");
            let (mut checkout, _) = t!(prep.fetch_then_checkout(
                gix::progress::Discard,
                &std::sync::atomic::AtomicBool::new(false),
            ));
            let (sub_repo, _) = t!(checkout.main_worktree(
                gix::progress::Discard,
                &std::sync::atomic::AtomicBool::new(false)
            ));

            // Set up worktree linkage
            let sub_config_path = modules_path.join("config");
            let mut sub_config = t!(fs::read_to_string(&sub_config_path));
            if !sub_config.contains("[core]") {
                sub_config.push_str("[core]\n");
            }
            sub_config.push_str(&format!("\tworktree = {}\n", submodule_path.display()));
            sub_config.push_str("[user]\n\temail = foo@bar.com\n\tname = Foo Bar\n");
            t!(fs::write(&sub_config_path, sub_config));

            // Create .git file in workdir
            t!(fs::create_dir_all(&submodule_path));
            t!(fs::write(
                submodule_path.join(".git"),
                format!("gitdir: {}\n", modules_path.display()),
            ));

            // Reset to target commit
            reset_submodule_to_commit(&sub_repo, &submodule_path, target_commit);
        } else if module_exists {
            // Module exists, just reset to target commit
            let sub_repo = t!(gix::open(&modules_path));

            // Ensure workdir exists
            t!(fs::create_dir_all(&submodule_path));
            if !submodule_path.join(".git").exists() {
                t!(fs::write(
                    submodule_path.join(".git"),
                    format!("gitdir: {}\n", modules_path.display()),
                ));
            }

            // Try to find the commit, if not found try fetching
            if sub_repo.find_commit(target_commit).is_err() {
                // Fetch from origin
                if let Ok(remote) = sub_repo.find_remote("origin") {
                    if let Ok(conn) = remote.connect(gix::remote::Direction::Fetch) {
                        if let Ok(prep) =
                            conn.prepare_fetch(gix::progress::Discard, Default::default())
                        {
                            let _ = prep.receive(
                                gix::progress::Discard,
                                &std::sync::atomic::AtomicBool::new(false),
                            );
                        }
                    }
                }
            }

            reset_submodule_to_commit(&sub_repo, &submodule_path, target_commit);
        }
    }
}

fn reset_submodule_to_commit(
    sub_repo: &gix::Repository,
    submodule_path: &Path,
    target_commit: gix::ObjectId,
) {
    // Update HEAD
    let head_path = sub_repo.path().join("HEAD");
    t!(fs::write(&head_path, format!("{}\n", target_commit)));

    // Checkout files
    if let Ok(commit) = sub_repo.find_commit(target_commit) {
        if let Ok(tree) = commit.tree() {
            if let Ok(mut index) = sub_repo.index_from_tree(&tree.id) {
                if let Ok(checkout_opts) = sub_repo
                    .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
                {
                    if let Ok(objects) = sub_repo.objects.clone().into_arc() {
                        let _ = gix::worktree::state::checkout(
                            &mut index,
                            submodule_path,
                            objects,
                            &gix::progress::Discard,
                            &gix::progress::Discard,
                            &std::sync::atomic::AtomicBool::new(false),
                            checkout_opts,
                        );
                        let _ = index.write(gix::index::write::Options::default());
                    }
                }
            }
        }
    }
}

/// Reinitialize a submodule after its URL has changed in .gitmodules.
///
/// This completely removes the old submodule entry and re-adds it from
/// the new URL specified in .gitmodules. This is necessary because
/// git submodule update can't handle URL changes where the old commit
/// doesn't exist in the new remote.
///
/// Note: The caller must have already modified .gitmodules with the new URL.
pub fn submodule_reinit(repo: &gix::Repository, path: &Path, new_url: &str) {
    let repo_path = repo.workdir().unwrap();
    let submodule_path = repo_path.join(path);
    let path_str = path.to_str().unwrap();
    let modules_path = repo_path.join(".git/modules").join(path);

    // 1. Remove submodule entry from index
    let index_path = repo_path.join(".git/index");
    if index_path.exists() {
        let index = t!(gix::index::File::at(
            &index_path,
            gix::hash::Kind::Sha1,
            false,
            gix::index::decode::Options::default(),
        ));

        // Filter out the submodule entry and write back
        let entries: Vec<_> = index
            .entries()
            .iter()
            .filter(|e| e.path(&index) != path_str.as_bytes())
            .map(|e| (e.stat, e.id, e.flags, e.mode, e.path(&index).to_owned()))
            .collect();

        let mut new_index = gix::index::File::from_state(
            gix::index::State::new(gix::hash::Kind::Sha1),
            index_path.clone(),
        );

        for (stat, id, flags, mode, path_bytes) in entries {
            use gix::bstr::ByteSlice;
            new_index.dangerously_push_entry(
                stat,
                id,
                flags,
                mode,
                path_bytes.to_str().unwrap().into(),
            );
        }
        new_index.sort_entries();
        t!(new_index.write(gix::index::write::Options::default()));
    }

    // 2. Remove submodule working directory
    if submodule_path.exists() {
        t!(fs::remove_dir_all(&submodule_path));
    }

    // 3. Remove old submodule config from .git/config
    let config_path = repo_path.join(".git/config");
    if config_path.exists() {
        let config_content = t!(fs::read_to_string(&config_path));
        let section_header = format!("[submodule \"{}\"]", path_str);
        if config_content.contains(&section_header) {
            let mut new_content = String::new();
            let mut skip_section = false;
            for line in config_content.lines() {
                if line.starts_with("[submodule") {
                    skip_section = line.contains(path_str);
                }
                if !skip_section {
                    new_content.push_str(line);
                    new_content.push('\n');
                }
            }
            t!(fs::write(&config_path, new_content));
        }
    }

    // 4. Remove .git/modules/<path> if it exists
    if modules_path.exists() {
        t!(fs::remove_dir_all(&modules_path));
    }

    // 5. Clone from new URL into a temp directory, then reorganize for submodule structure
    let temp_clone_path = repo_path.join(".git/temp_submodule_reinit");
    if temp_clone_path.exists() {
        t!(fs::remove_dir_all(&temp_clone_path));
    }

    let mut prep = t!(gix::prepare_clone(new_url, &temp_clone_path));
    prep = prep.with_remote_name("origin").expect("valid remote name");
    let (mut checkout, _) = t!(prep.fetch_then_checkout(
        gix::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false),
    ));
    let (sub_repo, _) = t!(checkout.main_worktree(
        gix::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false)
    ));

    // Get the HEAD commit of the cloned submodule
    let sub_head_id = t!(sub_repo.head_id()).detach();

    // 6. Move .git to modules_path (submodule structure)
    let cloned_git_dir = temp_clone_path.join(".git");
    t!(fs::create_dir_all(modules_path.parent().unwrap()));
    t!(fs::rename(&cloned_git_dir, &modules_path));

    // 7. Move working files to submodule workdir
    t!(fs::create_dir_all(&submodule_path));
    for entry in t!(fs::read_dir(&temp_clone_path)) {
        let entry = t!(entry);
        let file_name = entry.file_name();
        if file_name != ".git" {
            let dest = submodule_path.join(&file_name);
            t!(fs::rename(entry.path(), dest));
        }
    }
    t!(fs::remove_dir(&temp_clone_path));

    // 8. Set up worktree linkage in config
    let sub_config_path = modules_path.join("config");
    let mut sub_config = t!(fs::read_to_string(&sub_config_path));
    if !sub_config.contains("[core]") {
        sub_config.push_str("[core]\n");
    }
    sub_config.push_str(&format!("\tworktree = {}\n", submodule_path.display()));
    sub_config.push_str("[user]\n\temail = foo@bar.com\n\tname = Foo Bar\n");
    t!(fs::write(&sub_config_path, sub_config));

    // 7. Create .git file in workdir pointing to modules path
    t!(fs::create_dir_all(&submodule_path));
    t!(fs::write(
        submodule_path.join(".git"),
        format!("gitdir: {}\n", modules_path.display()),
    ));

    // 8. Add gitlink to parent index
    let index_path = repo_path.join(".git/index");
    let mut index_file = if index_path.exists() {
        t!(gix::index::File::at(
            &index_path,
            gix::hash::Kind::Sha1,
            false,
            gix::index::decode::Options::default(),
        ))
    } else {
        gix::index::File::from_state(
            gix::index::State::new(gix::hash::Kind::Sha1),
            index_path.clone(),
        )
    };

    index_file.dangerously_push_entry(
        gix::index::entry::Stat::default(),
        sub_head_id,
        gix::index::entry::Flags::empty(),
        gix::index::entry::Mode::COMMIT, // gitlink mode (160000)
        path_str.into(),
    );
    index_file.sort_entries();
    t!(index_file.write(gix::index::write::Options::default()));

    // 9. Update .git/config with new submodule URL
    let config_path = repo_path.join(".git/config");
    let mut config_content = t!(fs::read_to_string(&config_path));
    config_content.push_str(&format!(
        "\n[submodule \"{}\"]\n\turl = {}\n",
        path_str, new_url
    ));
    t!(fs::write(&config_path, config_content));

    // 10. Update .gitmodules with new URL
    let gitmodules_path = repo_path.join(".gitmodules");
    let gitmodules_content = t!(fs::read_to_string(&gitmodules_path));
    // Replace the URL for this submodule
    let mut new_gitmodules = String::new();
    let mut in_submodule = false;
    let mut found_url = false;
    for line in gitmodules_content.lines() {
        if line.trim().starts_with("[submodule") {
            in_submodule = line.contains(&format!("\"{}\"", path_str));
            found_url = false;
        }
        if in_submodule && line.trim().starts_with("url = ") {
            new_gitmodules.push_str(&format!("\turl = {}\n", new_url));
            found_url = true;
        } else {
            new_gitmodules.push_str(line);
            new_gitmodules.push('\n');
        }
    }
    t!(fs::write(&gitmodules_path, new_gitmodules));
}

/// Returns true if gitoxide is globally activated.
///
/// That way, tests that normally use `git2` can transparently use `gitoxide`.
pub fn cargo_uses_gitoxide() -> bool {
    // With gix as the only backend, this is always true
    true
}

/// Amend the most recent commit without changing its message.
/// This is equivalent to `git commit --amend --no-edit`.
pub fn amend(repo: &gix::Repository) {
    let repo_path = repo.workdir().unwrap();

    // Use git CLI for amend to ensure proper object storage
    let status = Command::new("git")
        .args(["commit", "--amend", "--no-edit"])
        .current_dir(repo_path)
        .status()
        .expect("git commit --amend failed");
    assert!(status.success(), "git commit --amend failed");

    // Get the new HEAD commit ID
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .expect("git rev-parse HEAD failed");
    assert!(output.status.success(), "git rev-parse HEAD failed");
    let new_commit_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Update the dummy tag
    let tag_ref_path = repo_path.join(".git/refs/tags/__dummy_tag__");
    if tag_ref_path.exists() {
        t!(std::fs::write(
            &tag_ref_path,
            format!("{}\n", new_commit_id)
        ));
    }

    // Repack objects to ensure they're in a format gix can fetch
    let status = Command::new("git")
        .args(["repack", "-a", "-d"])
        .current_dir(repo_path)
        .status()
        .expect("git repack failed");
    assert!(status.success(), "git repack failed");

    // Update server info for dumb HTTP/file protocol
    let status = Command::new("git")
        .args(["update-server-info"])
        .current_dir(repo_path)
        .status()
        .expect("git update-server-info failed");
    assert!(status.success(), "git update-server-info failed");
}

fn write_index_as_tree(repo: &gix::Repository, index: &gix::index::File) -> gix::ObjectId {
    use gix::bstr::ByteSlice;
    use std::collections::BTreeMap;

    // Build a tree from index entries
    // Group entries by their immediate parent directory
    // Key: directory path (empty string for root)
    // Value: list of (name, oid, kind) for direct children
    let mut dir_contents: BTreeMap<
        String,
        Vec<(String, gix::ObjectId, gix::object::tree::EntryKind)>,
    > = BTreeMap::new();

    for entry in index.entries() {
        let path = entry.path(index).to_str().expect("valid utf8 path");
        let (dir, filename) = if let Some(slash_pos) = path.rfind('/') {
            (
                path[..slash_pos].to_string(),
                path[slash_pos + 1..].to_string(),
            )
        } else {
            ("".to_string(), path.to_string())
        };

        let mode = match entry.mode {
            gix::index::entry::Mode::FILE => gix::object::tree::EntryKind::Blob,
            gix::index::entry::Mode::FILE_EXECUTABLE => {
                gix::object::tree::EntryKind::BlobExecutable
            }
            gix::index::entry::Mode::SYMLINK => gix::object::tree::EntryKind::Link,
            gix::index::entry::Mode::DIR => gix::object::tree::EntryKind::Tree,
            gix::index::entry::Mode::COMMIT => gix::object::tree::EntryKind::Commit,
            _ => gix::object::tree::EntryKind::Blob,
        };

        dir_contents
            .entry(dir)
            .or_default()
            .push((filename, entry.id, mode));
    }

    // Collect all unique directory paths that need tree objects
    let mut all_dirs: Vec<String> = dir_contents.keys().cloned().collect();
    // Also need to create trees for intermediate directories
    for dir in dir_contents.keys() {
        let mut current = dir.clone();
        while let Some(pos) = current.rfind('/') {
            current = current[..pos].to_string();
            if !all_dirs.contains(&current) {
                all_dirs.push(current.clone());
            }
        }
    }
    // Always ensure root is in the list
    if !all_dirs.contains(&"".to_string()) {
        all_dirs.push("".to_string());
    }
    // Sort by depth (deepest first) so we build from leaves to root
    all_dirs.sort_by(|a, b| {
        b.matches('/')
            .count()
            .cmp(&a.matches('/').count())
            .then(b.cmp(a))
    });

    // Build trees from deepest to shallowest
    let mut tree_ids: BTreeMap<String, gix::ObjectId> = BTreeMap::new();

    for dir in &all_dirs {
        let mut tree_entries: Vec<gix::objs::tree::Entry> = Vec::new();

        // Add blob entries from this directory
        if let Some(entries) = dir_contents.get(dir) {
            for (name, id, kind) in entries {
                tree_entries.push(gix::objs::tree::Entry {
                    mode: (*kind).into(),
                    filename: name.clone().into(),
                    oid: *id,
                });
            }
        }

        // Add subtree entries (child directories)
        let prefix = if dir.is_empty() {
            "".to_string()
        } else {
            format!("{}/", dir)
        };
        for (child_dir, tree_id) in &tree_ids {
            // Check if child_dir is a direct child of dir
            if child_dir.starts_with(&prefix) || (dir.is_empty() && !child_dir.contains('/')) {
                let child_name = if dir.is_empty() {
                    // Root level: child could be "src" or "foo/bar"
                    if let Some(slash) = child_dir.find('/') {
                        continue; // Not a direct child
                    }
                    child_dir.clone()
                } else {
                    let suffix = &child_dir[prefix.len()..];
                    if suffix.contains('/') {
                        continue; // Not a direct child
                    }
                    suffix.to_string()
                };
                tree_entries.push(gix::objs::tree::Entry {
                    mode: gix::object::tree::EntryKind::Tree.into(),
                    filename: child_name.into(),
                    oid: *tree_id,
                });
            }
        }

        // Use git's special tree sorting (directories treated as if they have trailing /)
        tree_entries.sort();

        let tree = gix::objs::Tree {
            entries: tree_entries,
        };
        let tree_id = repo
            .write_object(&tree)
            .expect("failed to write tree object")
            .detach();
        tree_ids.insert(dir.clone(), tree_id);
    }

    // Return the root tree id
    tree_ids.get("").copied().unwrap_or_else(|| {
        // Empty tree
        let tree = gix::objs::Tree { entries: vec![] };
        repo.write_object(&tree)
            .expect("failed to write empty tree")
            .detach()
    })
}

/// Checkout a branch by name.
/// This is equivalent to `git checkout <branch>`.
pub fn checkout_branch(repo: &gix::Repository, branch_name: &str) {
    let repo_path = repo.workdir().unwrap();

    // Update HEAD to point to the branch
    let refname = format!("refs/heads/{}", branch_name);

    // Find the branch reference
    let mut reference = t!(repo.find_reference(&refname));
    let commit_id = t!(reference.peel_to_commit()).id;

    // Update HEAD to be a symbolic reference to the branch
    let head_path = repo_path.join(".git/HEAD");
    t!(fs::write(&head_path, format!("ref: {}\n", refname)));

    // Checkout the tree
    let commit = t!(repo.find_commit(commit_id));
    let tree = t!(commit.tree());

    let mut index = t!(repo.index_from_tree(&tree.id));
    let checkout_opts =
        t!(repo.checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping));

    t!(gix::worktree::state::checkout(
        &mut index,
        repo_path,
        t!(repo.objects.clone().into_arc()),
        &gix::progress::Discard,
        &gix::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false),
        checkout_opts,
    ));

    t!(index.write(gix::index::write::Options::default()));
}

/// Force update a tag to point to HEAD.
/// This is equivalent to `git tag -f -a <name> -m <message>`.
pub fn force_tag(repo: &gix::Repository, name: &str, message: &str) {
    let repo_path = repo.workdir().unwrap();
    // Reopen repo to get fresh refs (previous operations may have modified refs directly)
    let fresh_repo = t!(gix::open(repo_path));
    let head_id = t!(fresh_repo.head_id());

    // Delete existing tag if it exists
    let tag_ref_path = repo_path.join(".git/refs/tags").join(name);
    if tag_ref_path.exists() {
        t!(fs::remove_file(&tag_ref_path));
    }

    // Also remove from packed-refs if present
    let packed_refs_path = repo_path.join(".git/packed-refs");
    if packed_refs_path.exists() {
        let content = t!(fs::read_to_string(&packed_refs_path));
        let filtered: Vec<&str> = content
            .lines()
            .filter(|line| !line.contains(&format!("refs/tags/{}", name)))
            .collect();
        t!(fs::write(&packed_refs_path, filtered.join("\n") + "\n"));
    }

    // Create the annotated tag object
    let time = gix::date::Time::now_local_or_utc();

    // Write tag object in raw format
    let tag_content = format!(
        "object {}\ntype commit\ntag {}\ntagger Foo Bar <foo@bar.com> {} +0000\n\n{}\n",
        head_id, name, time.seconds, message
    );

    let tag_oid = fresh_repo
        .objects
        .write_buf(gix::object::Kind::Tag, tag_content.as_bytes())
        .expect("failed to write tag");

    // Create the tag reference
    t!(fs::create_dir_all(tag_ref_path.parent().unwrap()));
    t!(fs::write(&tag_ref_path, format!("{}\n", tag_oid)));
}

/// Add a worktree to the repository.
/// This is equivalent to `git worktree add <path>`.
pub fn add_worktree(repo: &gix::Repository, worktree_path: &Path, name: &str) {
    let repo_path = repo.workdir().unwrap_or_else(|| repo.path());
    let git_dir = if repo.is_bare() {
        repo.path().to_path_buf()
    } else {
        repo_path.join(".git")
    };

    // Create worktree directory
    t!(fs::create_dir_all(worktree_path));

    // Create worktrees directory in git dir
    let worktrees_dir = git_dir.join("worktrees").join(name);
    t!(fs::create_dir_all(&worktrees_dir));

    // Write gitdir file in worktrees directory
    t!(fs::write(
        worktrees_dir.join("gitdir"),
        format!("{}\n", worktree_path.join(".git").display()),
    ));

    // Write HEAD file in worktrees directory (point to same as main repo HEAD)
    let head_content = t!(fs::read_to_string(git_dir.join("HEAD")));
    t!(fs::write(worktrees_dir.join("HEAD"), &head_content));

    // Write commondir to link back to the main repo (needed for refs and objects)
    t!(fs::write(
        worktrees_dir.join("commondir"),
        format!("{}\n", git_dir.display())
    ));

    // Write .git file in worktree (points to worktrees dir)
    t!(fs::write(
        worktree_path.join(".git"),
        format!("gitdir: {}\n", worktrees_dir.display()),
    ));

    // Checkout files to worktree
    let worktree_repo = t!(gix::open(worktree_path));
    let head_commit = t!(worktree_repo.head_commit());
    let tree = t!(head_commit.tree());

    let mut index = t!(worktree_repo.index_from_tree(&tree.id));
    let checkout_opts =
        t!(worktree_repo
            .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping));

    t!(gix::worktree::state::checkout(
        &mut index,
        worktree_path,
        t!(worktree_repo.objects.clone().into_arc()),
        &gix::progress::Discard,
        &gix::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false),
        checkout_opts,
    ));

    t!(index.write(gix::index::write::Options::default()));
}

/// Add a single file to the index. Forces addition even if in .gitignore.
/// This is equivalent to `git add -f <file>`.
pub fn add_file(repo: &gix::Repository, file: &Path) {
    git_add(repo.workdir().unwrap(), file);
}

/// Remove a file from the index without removing from the working directory.
/// This is equivalent to `git rm --cached <file>`.
pub fn rm_cached(repo: &gix::Repository, file: &Path) {
    let repo_path = repo.workdir().unwrap();
    let file_str = file.to_str().unwrap().replace('\\', "/");

    let index_path = repo_path.join(".git/index");
    let index = t!(gix::index::File::at(
        &index_path,
        gix::hash::Kind::Sha1,
        false,
        gix::index::decode::Options::default(),
    ));

    // Filter out the file entry and write back
    let entries: Vec<_> = index
        .entries()
        .iter()
        .filter(|e| e.path(&index) != file_str.as_bytes())
        .map(|e| (e.stat, e.id, e.flags, e.mode, e.path(&index).to_owned()))
        .collect();

    let mut new_index = gix::index::File::from_state(
        gix::index::State::new(gix::hash::Kind::Sha1),
        index_path.clone(),
    );

    for (stat, id, flags, mode, path_bytes) in entries {
        use gix::bstr::ByteSlice;
        new_index.dangerously_push_entry(
            stat,
            id,
            flags,
            mode,
            path_bytes.to_str().unwrap().into(),
        );
    }
    new_index.sort_entries();
    t!(new_index.write(gix::index::write::Options::default()));
}

/// Check if the repository has no uncommitted changes.
/// This is equivalent to checking `git status --porcelain` is empty.
pub fn is_clean(repo: &gix::Repository) -> bool {
    use gix::status::index_worktree::Item;

    let status = t!(repo.status(gix::progress::Discard));
    let iter = t!(status.into_index_worktree_iter(None));

    // If there are any changes, it's not clean
    for item in iter {
        match t!(item) {
            Item::Modification { .. } | Item::DirectoryContents { .. } | Item::Rewrite { .. } => {
                return false;
            }
        }
    }
    true
}

/// Fetch from a remote URL into the repository.
/// This is equivalent to `git fetch <url> <refspec>`.
pub fn fetch(repo: &gix::Repository, url: &str, refspec: &str) {
    // Parse the refspec parts
    let (src, dst) = refspec.split_once(':').unwrap_or((refspec, refspec));

    // Create a remote with the refspec configured
    let remote = t!(repo
        .remote_at(url)
        .unwrap()
        .with_refspecs([refspec], gix::remote::Direction::Fetch));
    let connection = t!(remote.connect(gix::remote::Direction::Fetch));

    // Perform the fetch
    let prepare = t!(connection.prepare_fetch(gix::progress::Discard, Default::default()));
    let _outcome = t!(prepare.receive(
        gix::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false)
    ));

    // After fetch, look up the ref in the remote repo directly
    // For file:// URLs (which is what tests use), we can open the remote repo
    let remote_repo = if url.starts_with("file://") {
        let path = url.strip_prefix("file://").unwrap();
        Some(t!(gix::open(path)))
    } else if !url.contains("://") {
        // Assume it's a path
        Some(t!(gix::open(url)))
    } else {
        None
    };

    // Update the destination ref from the fetched refs
    let repo_path = repo.workdir().unwrap_or_else(|| repo.path());
    let git_dir = if repo.is_bare() {
        repo_path.to_path_buf()
    } else {
        repo_path.join(".git")
    };

    // Get the target commit from remote repo
    if let Some(remote_repo) = remote_repo {
        let target_id = if src == "HEAD" {
            t!(remote_repo.head_id()).detach()
        } else if let Ok(id) = remote_repo.rev_parse_single(src) {
            id.detach()
        } else {
            return;
        };

        // Write the destination ref
        let ref_path = git_dir.join(dst);
        t!(fs::create_dir_all(ref_path.parent().unwrap()));
        t!(fs::write(&ref_path, format!("{}\n", target_id)));
    }
}

/// Count the number of commits reachable from HEAD.
/// This is equivalent to `git rev-list --count HEAD`.
/// This respects shallow repo boundaries.
pub fn rev_list_count(repo: &gix::Repository) -> usize {
    let head_id = match repo.head_id() {
        Ok(id) => id,
        Err(_) => return 0,
    };

    // Read shallow boundaries if present
    let shallow_commits = read_shallow_commits(repo);

    let mut count = 0;
    let mut to_visit = vec![head_id.detach()];
    let mut visited = std::collections::HashSet::new();

    while let Some(commit_id) = to_visit.pop() {
        if visited.contains(&commit_id) {
            continue;
        }
        visited.insert(commit_id);
        count += 1;

        // Don't traverse past shallow boundary commits
        if shallow_commits.contains(&commit_id) {
            continue;
        }

        if let Ok(commit) = repo.find_commit(commit_id) {
            for parent_id in commit.parent_ids() {
                to_visit.push(parent_id.detach());
            }
        }
    }

    count
}

/// Read the shallow file to get boundary commit IDs.
fn read_shallow_commits(repo: &gix::Repository) -> std::collections::HashSet<gix::ObjectId> {
    let repo_path = repo.workdir().unwrap_or_else(|| repo.path());
    let git_dir = if repo.is_bare() {
        repo_path.to_path_buf()
    } else {
        repo_path.join(".git")
    };

    let shallow_path = git_dir.join("shallow");
    let mut shallow_commits = std::collections::HashSet::new();

    if let Ok(content) = fs::read_to_string(&shallow_path) {
        for line in content.lines() {
            let line = line.trim();
            if !line.is_empty() {
                if let Ok(id) = gix::ObjectId::from_hex(line.as_bytes()) {
                    shallow_commits.insert(id);
                }
            }
        }
    }

    shallow_commits
}

/// Get the symbolic ref that HEAD points to.
/// This is equivalent to `git symbolic-ref HEAD`.
pub fn symbolic_ref_head(repo: &gix::Repository) -> Option<String> {
    let repo_path = repo.workdir().unwrap_or_else(|| repo.path());
    let git_dir = if repo.is_bare() {
        repo_path.to_path_buf()
    } else {
        repo_path.join(".git")
    };

    let head_path = git_dir.join("HEAD");
    let content = t!(fs::read_to_string(&head_path));

    if content.starts_with("ref: ") {
        Some(content.trim().strip_prefix("ref: ").unwrap().to_string())
    } else {
        None
    }
}

/// Set a git config value in the repository.
/// This is equivalent to `git config <key> <value>`.
pub fn set_config(repo: &gix::Repository, key: &str, value: &str) {
    let repo_path = repo.workdir().unwrap_or_else(|| repo.path());
    let git_dir = if repo.is_bare() {
        repo_path.to_path_buf()
    } else {
        repo_path.join(".git")
    };

    let config_path = git_dir.join("config");
    let mut config_content = t!(fs::read_to_string(&config_path));

    // Parse key into section and name (e.g., "core.symlinks" -> ["core"], "symlinks")
    let parts: Vec<&str> = key.split('.').collect();
    let (section, name) = if parts.len() == 2 {
        (parts[0], parts[1])
    } else if parts.len() == 3 {
        // subsection case like user.signingkey
        (parts[0], parts[2])
    } else {
        panic!("Invalid config key format: {}", key);
    };

    let section_header = format!("[{}]", section);

    // Check if section exists
    if config_content.contains(&section_header) {
        // Find and update or add within section
        let mut new_content = String::new();
        let mut in_section = false;
        let mut value_set = false;

        for line in config_content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                // New section starting
                if in_section && !value_set {
                    // Add the value before leaving the section
                    new_content.push_str(&format!("\t{} = {}\n", name, value));
                    value_set = true;
                }
                in_section = trimmed
                    .to_lowercase()
                    .starts_with(&section_header.to_lowercase());
            }

            if in_section
                && trimmed
                    .to_lowercase()
                    .starts_with(&format!("{} =", name).to_lowercase())
                && !value_set
            {
                // Replace this line
                new_content.push_str(&format!("\t{} = {}\n", name, value));
                value_set = true;
            } else {
                new_content.push_str(line);
                new_content.push('\n');
            }
        }

        // If still in section at end and value not set
        if in_section && !value_set {
            new_content.push_str(&format!("\t{} = {}\n", name, value));
        }

        config_content = new_content;
    } else {
        // Add new section
        config_content.push_str(&format!("\n{}\n\t{} = {}\n", section_header, name, value));
    }

    t!(fs::write(&config_path, config_content));
}
