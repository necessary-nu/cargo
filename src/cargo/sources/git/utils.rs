//! Utilities for handling git repositories, mainly around
//! authentication/cloning.

use crate::core::{GitReference, SourceId, Verbosity};
use crate::sources::git::fetch::RemoteKind;
use crate::sources::git::source::GitSource;
use crate::sources::source::Source as _;
use crate::util::errors::{CargoResult, GitCliError};
use crate::util::network::http::HttpTimeout;
use crate::util::{GlobalContext, HumanBytes, IntoUrl, MetricsCounter, Progress, network};
use anyhow::{Context as _, anyhow};
use cargo_util::{ProcessBuilder, paths};
use curl::easy::List;
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use serde::Serialize;
use serde::ser;
use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tracing::{debug, info};
use url::Url;

/// A file indicates that if present, `git reset` has been done and a repo
/// checkout is ready to go. See [`GitCheckout::reset`] for why we need this.
const CHECKOUT_READY_LOCK: &str = ".cargo-ok";

fn serialize_str<T, S>(t: &T, s: S) -> Result<S::Ok, S::Error>
where
    T: fmt::Display,
    S: ser::Serializer,
{
    s.collect_str(t)
}

/// A short abbreviated OID.
///
/// Exists for avoiding extra allocations in [`GitDatabase::to_short_id`].
pub struct GitShortID(String);

impl GitShortID {
    /// Views the short ID as a `str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A remote repository. It gets cloned into a local [`GitDatabase`].
#[derive(PartialEq, Clone, Debug, Serialize)]
pub struct GitRemote {
    /// URL to a remote repository.
    #[serde(serialize_with = "serialize_str")]
    url: Url,
}

/// A local clone of a remote repository's database. Multiple [`GitCheckout`]s
/// can be cloned from a single [`GitDatabase`].
pub struct GitDatabase {
    /// The remote repository where this database is fetched from.
    remote: GitRemote,
    /// Path to the root of the underlying Git repository on the local filesystem.
    path: PathBuf,
    /// Underlying Git repository instance for this database.
    repo: gix::Repository,
}

/// A local checkout of a particular revision from a [`GitDatabase`].
pub struct GitCheckout<'a> {
    /// The git database where this checkout is cloned from.
    database: &'a GitDatabase,
    /// Path to the root of the underlying Git repository on the local filesystem.
    path: PathBuf,
    /// The git revision this checkout is for.
    revision: gix::ObjectId,
    /// Underlying Git repository instance for this checkout.
    repo: gix::Repository,
}

impl GitRemote {
    /// Creates an instance for a remote repository URL.
    pub fn new(url: &Url) -> GitRemote {
        GitRemote { url: url.clone() }
    }

    /// Gets the remote repository URL.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Fetches and checkouts to a reference or a revision from this remote
    /// into a local path.
    ///
    /// This ensures that it gets the up-to-date commit when a named reference
    /// is given (tag, branch, refs/*). Thus, network connection is involved.
    ///
    /// If we have a previous instance of [`GitDatabase`] then fetch into that
    /// if we can. If that can successfully load our revision then we've
    /// populated the database with the latest version of `reference`, so
    /// return that database and the rev we resolve to.
    pub fn checkout(
        &self,
        into: &Path,
        db: Option<GitDatabase>,
        reference: &GitReference,
        gctx: &GlobalContext,
    ) -> CargoResult<(GitDatabase, ObjectId)> {
        if let Some(mut db) = db {
            fetch(
                &mut db.repo,
                self.url.as_str(),
                reference,
                gctx,
                RemoteKind::GitDependency,
            )
            .with_context(|| format!("failed to fetch into: {}", into.display()))?;

            if let Some(rev) = resolve_ref(reference, &db.repo).ok() {
                return Ok((db, rev));
            }
        }

        // Otherwise start from scratch to handle corrupt git repositories.
        // After our fetch (which is interpreted as a clone now) we do the same
        // resolution to figure out what we cloned.
        if into.exists() {
            paths::remove_dir_all(into)?;
        }
        paths::create_dir_all(into)?;
        let mut repo = init(into, true)?;
        fetch(
            &mut repo,
            self.url.as_str(),
            reference,
            gctx,
            RemoteKind::GitDependency,
        )
        .with_context(|| format!("failed to clone into: {}", into.display()))?;
        let rev = resolve_ref(reference, &repo)?;

        Ok((
            GitDatabase {
                remote: self.clone(),
                path: into.to_path_buf(),
                repo,
            },
            rev,
        ))
    }

    /// Creates a [`GitDatabase`] of this remote at `db_path`.
    pub fn db_at(&self, db_path: &Path) -> CargoResult<GitDatabase> {
        let repo = gix::open(db_path)?;
        Ok(GitDatabase {
            remote: self.clone(),
            path: db_path.to_path_buf(),
            repo,
        })
    }
}

impl GitDatabase {
    /// Checkouts to a revision at `dest`ination from this database.
    #[tracing::instrument(skip(self, gctx))]
    pub fn copy_to(
        &self,
        rev: ObjectId,
        dest: &Path,
        gctx: &GlobalContext,
        quiet: bool,
    ) -> CargoResult<GitCheckout<'_>> {
        // If the existing checkout exists, and it is fresh, use it.
        // A non-fresh checkout can happen if the checkout operation was
        // interrupted. In that case, the checkout gets deleted and a new
        // clone is created.
        let checkout = match gix::open(dest)
            .ok()
            .map(|repo| GitCheckout::new(self, rev, repo))
            .filter(|co| co.is_fresh())
        {
            Some(co) => co,
            None => {
                let (checkout, guard) = GitCheckout::clone_into(dest, self, rev, gctx)?;
                checkout.update_submodules(gctx, quiet)?;
                guard.mark_ok()?;
                checkout
            }
        };

        Ok(checkout)
    }

    /// Get a short OID for a `revision`, usually 7 chars.
    pub fn to_short_id(&self, revision: ObjectId) -> CargoResult<GitShortID> {
        // Use gix's to_hex_with_len for short hash - 7 chars is the standard minimum
        let short_id = revision.to_hex_with_len(7).to_string();
        Ok(GitShortID(short_id))
    }

    /// Checks if the database contains the object of this `oid`.
    pub fn contains(&self, oid: ObjectId) -> bool {
        // Reopen the repo to clear any stale pack file index cache
        // and ensure we see freshly fetched objects
        gix::open(self.repo.path())
            .map(|repo| repo.has_object(oid))
            .unwrap_or(false)
    }

    /// [`resolve_ref`]s this reference with this database.
    pub fn resolve(&self, r: &GitReference) -> CargoResult<ObjectId> {
        resolve_ref(r, &self.repo)
    }
}

/// Resolves [`GitReference`] to an object ID with objects the `repo` currently has.
pub fn resolve_ref(gitref: &GitReference, repo: &gix::Repository) -> CargoResult<ObjectId> {
    // Reopen the repo to ensure we see the latest refs from disk.
    // This is necessary after fetch operations that update refs, as the
    // repo object may have cached the old ref values.
    let repo = gix::open(repo.path())?;
    let id = match gitref {
        // Note that we resolve the named tag here in sync with where it's
        // fetched into via `fetch` below.
        GitReference::Tag(s) => (|| -> CargoResult<ObjectId> {
            let refname = format!("refs/remotes/origin/tags/{}", s);
            let mut reference = repo.find_reference(&refname)?;
            let obj = reference.peel_to_commit()?;
            Ok(obj.id().detach())
        })()
        .with_context(|| format!("failed to find tag `{}`", s))?,

        // Resolve the remote name since that's all we're configuring in
        // `fetch` below.
        GitReference::Branch(s) => {
            let refname = format!("refs/remotes/origin/{}", s);
            let reference = repo
                .find_reference(&refname)
                .with_context(|| format!("failed to find branch `{}`", s))?;
            reference.id().detach()
        }

        // We'll be using the HEAD commit
        GitReference::DefaultBranch => {
            let mut reference = repo.find_reference("refs/remotes/origin/HEAD")?;
            let obj = reference.peel_to_commit()?;
            obj.id().detach()
        }

        GitReference::Rev(s) => {
            use gix::bstr::ByteSlice;
            let obj = repo.rev_parse_single(s.as_bytes().as_bstr())?;
            // Peel to commit - the object might be a tag
            obj.object()?.peel_to_kind(gix::object::Kind::Commit)?.id
        }
    };
    Ok(id)
}

impl<'a> GitCheckout<'a> {
    /// Creates an instance of [`GitCheckout`]. This doesn't imply the checkout
    /// is done. Use [`GitCheckout::is_fresh`] to check.
    ///
    /// * The `database` is where this checkout is from.
    /// * The `repo` will be the checked out Git repository.
    fn new(
        database: &'a GitDatabase,
        revision: ObjectId,
        repo: gix::Repository,
    ) -> GitCheckout<'a> {
        let path = repo.workdir().unwrap_or_else(|| repo.path());
        GitCheckout {
            path: path.to_path_buf(),
            database,
            revision,
            repo,
        }
    }

    /// Gets the remote repository URL.
    fn remote_url(&self) -> &Url {
        &self.database.remote.url()
    }

    /// Clone a repo for a `revision` into a local path from a `database`.
    /// This is a filesystem-to-filesystem clone.
    fn clone_into(
        into: &Path,
        database: &'a GitDatabase,
        revision: ObjectId,
        gctx: &GlobalContext,
    ) -> CargoResult<(GitCheckout<'a>, CheckoutGuard)> {
        let dirname = into.parent().unwrap();
        paths::create_dir_all(&dirname)?;
        if into.exists() {
            paths::remove_dir_all(into)?;
        }

        let _progress = Progress::new("Cloning", gctx);

        // Reopen the source database to ensure fresh object visibility
        let source_repo = gix::open(&database.path)
            .with_context(|| format!("failed to open database repo at {:?}", database.path))?;

        // Verify the source repo can find the revision we need
        if source_repo.find_object(revision).is_err() {
            anyhow::bail!(
                "database repository at {:?} does not contain revision {}, \
                 please try `cargo update` to refresh it",
                database.path,
                revision
            );
        }

        // Create a new repo and manually copy objects from the database.
        // We use gix::init to create the repo structure, then set up alternates
        // to share objects with the database.
        let checkout_repo = gix::init(into)?;

        // Set up alternates to point to the database's object store
        // This allows the checkout to access all objects from the database
        let alternates_path = checkout_repo.path().join("objects/info/alternates");
        paths::create_dir_all(alternates_path.parent().unwrap())?;
        let db_objects = source_repo.path().join("objects");
        std::fs::write(&alternates_path, format!("{}\n", db_objects.display()))?;

        // Copy refs from the database to the checkout
        // This is needed for submodule operations and other ref-based operations
        let db_refs_dir = source_repo.path().join("refs");
        let checkout_refs_dir = checkout_repo.path().join("refs");
        if db_refs_dir.exists() {
            copy_dir_contents(&db_refs_dir, &checkout_refs_dir)?;
        }

        // Copy packed-refs if it exists
        let db_packed_refs = source_repo.path().join("packed-refs");
        if db_packed_refs.exists() {
            let checkout_packed_refs = checkout_repo.path().join("packed-refs");
            std::fs::copy(&db_packed_refs, &checkout_packed_refs)?;
        }

        // Copy shallow file if the source repo is shallow
        if source_repo.is_shallow() {
            let src_shallow = source_repo.path().join("shallow");
            let dst_shallow = checkout_repo.path().join("shallow");
            if src_shallow.exists() {
                std::fs::copy(&src_shallow, &dst_shallow)?;
            }
        }

        // Reopen the checkout repo to see the copied refs.
        // Use isolated mode to prevent reading global git config (like core.autocrlf)
        // which could modify file contents during checkout.
        let checkout_repo = gix::open_opts(into, gix::open::Options::isolated())?;

        let checkout = GitCheckout::new(database, revision, checkout_repo);
        let guard = checkout.reset(gctx)?;

        Ok((checkout, guard))
    }

    /// Checks if the `HEAD` of this checkout points to the expected revision.
    fn is_fresh(&self) -> bool {
        match self.repo.head_id() {
            Ok(head_id) if head_id.detach() == self.revision => {
                // See comments in reset() for why we check this
                self.path.join(CHECKOUT_READY_LOCK).exists()
            }
            _ => false,
        }
    }

    /// Similar to [`reset()`]. This roughly performs `git reset --hard` to the
    /// revision of this checkout, with additional interrupt protection by a
    /// dummy file [`CHECKOUT_READY_LOCK`].
    ///
    /// If we're interrupted while performing a `git reset` (e.g., we die
    /// because of a signal) Cargo needs to be sure to try to check out this
    /// repo again on the next go-round.
    ///
    /// To enable this we have a dummy file in our checkout, [`.cargo-ok`],
    /// which if present means that the repo has been successfully reset and is
    /// ready to go. Hence if we start to do a reset, we make sure this file
    /// *doesn't* exist. The caller of [`reset`] has an option to perform additional operations
    /// (e.g. submodule update) before marking the check-out as ready.
    ///
    /// [`.cargo-ok`]: CHECKOUT_READY_LOCK
    fn reset(&self, gctx: &GlobalContext) -> CargoResult<CheckoutGuard> {
        let guard = CheckoutGuard::guard(&self.path);
        info!("reset {} to {}", self.repo.path().display(), self.revision);

        reset(&self.repo, self.revision, gctx)?;

        Ok(guard)
    }

    /// Like `git submodule update --recursive` but for this git checkout.
    ///
    /// This function respects `submodule.<name>.update = none`[^1] git config.
    /// Submodules set to `none` won't be fetched.
    ///
    /// [^1]: <https://git-scm.com/docs/git-submodule#Documentation/git-submodule.txt-none>
    fn update_submodules(&self, gctx: &GlobalContext, quiet: bool) -> CargoResult<()> {
        // Reopen the repo to pick up any .gitmodules file that was checked out
        // by the reset operation. The original repo object may have been opened
        // before the working tree was populated.
        let repo = gix::open(self.repo.path())?;
        return update_submodules(&repo, gctx, quiet, self.remote_url().as_str());

        /// Recursive helper for [`GitCheckout::update_submodules`].
        fn update_submodules(
            repo: &gix::Repository,
            gctx: &GlobalContext,
            quiet: bool,
            parent_remote_url: &str,
        ) -> CargoResult<()> {
            let workdir = repo.workdir().ok_or_else(|| anyhow!("bare repository"))?;
            debug!("update submodules for: {:?}", workdir);

            let submodules = match repo.submodules() {
                Ok(Some(s)) => s,
                Ok(None) => return Ok(()),
                Err(e) => {
                    debug!("Error getting submodules in {:?}: {}", workdir, e);
                    return Ok(());
                }
            };

            for submodule in submodules {
                let submodule_name = submodule.name().to_str_lossy().into_owned();
                update_submodule(repo, submodule, gctx, quiet, parent_remote_url)
                    .with_context(|| format!("failed to update submodule `{}`", submodule_name))?;
            }
            Ok(())
        }

        /// Update a single Git submodule, and recurse into its submodules.
        fn update_submodule(
            parent: &gix::Repository,
            submodule: gix::Submodule<'_>,
            gctx: &GlobalContext,
            quiet: bool,
            parent_remote_url: &str,
        ) -> CargoResult<()> {
            let name = submodule.name().to_str_lossy().into_owned();
            let path = submodule.path()?.to_path()?.to_owned();
            let child_url_str = submodule.url()?.to_bstring().to_string();

            // Skip the submodule if the config says not to update it.
            // gix uses gix::submodule::config::Update enum
            if matches!(
                submodule.update()?,
                Some(gix::submodule::config::Update::None)
            ) {
                gctx.shell().status(
                    "Skipping",
                    format!(
                        "git submodule `{}` due to update strategy in .gitmodules",
                        child_url_str
                    ),
                )?;
                return Ok(());
            }

            let child_remote_url = absolute_submodule_url(parent_remote_url, &child_url_str)?;

            // A submodule which is listed in .gitmodules but not actually
            // checked out will not have a head id, so we should ignore it.
            let Some(head_id) = submodule.head_id()? else {
                return Ok(());
            };

            let parent_workdir = parent.workdir().ok_or_else(|| anyhow!("bare repository"))?;
            let submodule_path = parent_workdir.join(&path);

            // If the submodule hasn't been checked out yet, we need to
            // clone it. If it has been checked out and the head is the same
            // as the submodule's head, then we can skip an update and keep
            // recursing.
            let head_and_repo = gix::open(&submodule_path).ok().and_then(|repo| {
                let target = repo.head_id().ok()?.detach();
                Some((target, repo))
            });
            let _repo = match head_and_repo {
                Some((current_head, repo)) => {
                    if head_id == current_head {
                        return update_submodules(&repo, gctx, quiet, &child_remote_url);
                    }
                    repo
                }
                None => {
                    let _ = paths::remove_dir_all(&submodule_path);
                    init(&submodule_path, false)?
                }
            };
            // Fetch submodule database and checkout to target revision
            let reference = GitReference::Rev(head_id.to_string());

            // GitSource created from SourceId without git precise will result to
            // locked_rev being Deferred and fetch_db always try to fetch if online
            let source_id = SourceId::for_git(&child_remote_url.into_url()?, reference)?
                .with_git_precise(Some(head_id.to_string()));

            let mut source = GitSource::new(source_id, gctx)?;
            source.set_quiet(quiet);

            let (db, actual_rev) = source.fetch_db(true).with_context(|| {
                format!("failed to fetch submodule `{name}` from {child_remote_url}",)
            })?;
            // Use the submodule path (worktree), not repo.path() which would be .git
            db.copy_to(actual_rev, &submodule_path, gctx, quiet)?;
            Ok(())
        }
    }
}

/// See [`GitCheckout::reset`] for rationale on this type.
#[must_use]
struct CheckoutGuard {
    ok_file: PathBuf,
}

impl CheckoutGuard {
    fn guard(path: &Path) -> Self {
        let ok_file = path.join(CHECKOUT_READY_LOCK);
        let _ = paths::remove_file(&ok_file);
        Self { ok_file }
    }

    fn mark_ok(self) -> CargoResult<()> {
        let _ = paths::create(self.ok_file)?;
        Ok(())
    }
}

/// Constructs an absolute URL for a child submodule URL with its parent base URL.
///
/// Git only assumes a submodule URL is a relative path if it starts with `./`
/// or `../` [^1]. To fetch the correct repo, we need to construct an absolute
/// submodule URL.
///
/// At this moment it comes with some limitations:
///
/// * GitHub doesn't accept non-normalized URLs with relative paths.
///   (`ssh://git@github.com/rust-lang/cargo.git/relative/..` is invalid)
/// * `url` crate cannot parse SCP-like URLs.
///   (`git@github.com:rust-lang/cargo.git` is not a valid WHATWG URL)
///
/// To overcome these, this patch always tries [`Url::parse`] first to normalize
/// the path. If it couldn't, append the relative path as the last resort and
/// pray the remote git service supports non-normalized URLs.
///
/// See also rust-lang/cargo#12404 and rust-lang/cargo#12295.
///
/// [^1]: <https://git-scm.com/docs/git-submodule>
fn absolute_submodule_url<'s>(base_url: &str, submodule_url: &'s str) -> CargoResult<Cow<'s, str>> {
    let absolute_url = if ["./", "../"].iter().any(|p| submodule_url.starts_with(p)) {
        match Url::parse(base_url) {
            Ok(mut base_url) => {
                let path = base_url.path();
                if !path.ends_with('/') {
                    base_url.set_path(&format!("{path}/"));
                }
                let absolute_url = base_url.join(submodule_url).with_context(|| {
                    format!(
                        "failed to parse relative child submodule url `{submodule_url}` \
                        using parent base url `{base_url}`"
                    )
                })?;
                Cow::from(absolute_url.to_string())
            }
            Err(_) => {
                let mut absolute_url = base_url.to_string();
                if !absolute_url.ends_with('/') {
                    absolute_url.push('/');
                }
                absolute_url.push_str(submodule_url);
                Cow::from(absolute_url)
            }
        }
    } else {
        Cow::from(submodule_url)
    };

    Ok(absolute_url)
}

/// `git reset --hard` to the given commit for the `repo`.
///
/// The `commit_id` is a commit-ish to which the head should be moved.
fn reset(repo: &gix::Repository, commit_id: ObjectId, _gctx: &GlobalContext) -> CargoResult<()> {
    use gix::worktree::stack::state::attributes::Source;
    use std::sync::atomic::AtomicBool;

    debug!("doing reset to {}", commit_id);

    let workdir = repo.workdir().ok_or_else(|| anyhow!("bare repository"))?;

    // Reopen the repo to ensure we see all objects.
    // Use isolated mode to prevent reading global git config (like core.autocrlf)
    // which could modify file contents during checkout.
    let repo = gix::open_opts(repo.path(), gix::open::Options::isolated())?;

    // Get the commit and its tree
    let commit = repo
        .find_commit(commit_id)
        .with_context(|| format!("failed to find commit {}", commit_id))?;
    let tree_id = commit.tree_id()?.detach();

    // Build index from the tree using default protect options
    // These defaults are safe: protect_ntfs=true, protect_hfs=cfg!(macos), protect_windows=cfg!(windows)
    let protect_opts = gix::validate::path::component::Options::default();
    let index = gix::index::State::from_tree(&tree_id, &repo.objects, protect_opts)
        .with_context(|| format!("failed to build index from tree {}", tree_id))?;
    let mut index = gix::index::File::from_state(index, repo.index_path());

    // Get checkout options with overwrite_existing=true (like --hard)
    let mut opts = repo.checkout_options(Source::IdMapping)?;
    opts.overwrite_existing = true;
    opts.destination_is_initially_empty = false;

    // Checkout the tree to the working directory
    gix::worktree::state::checkout(
        &mut index,
        workdir,
        repo.objects.clone().into_arc()?,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &AtomicBool::new(false),
        opts,
    )?;

    // Write the updated index
    index.write(Default::default())?;

    debug!("reset done");
    Ok(())
}

/// Attempts to fetch the given git `reference` for a Git repository.
///
/// This is the main entry for git clone/fetch. It does the followings:
///
/// * Turns [`GitReference`] into refspecs accordingly.
/// * Dispatches `git fetch` using gitoxide or git CLI.
///
/// The `remote_url` argument is the git remote URL where we want to fetch from.
///
/// The `remote_kind` argument is used for shallow clone settings.
pub fn fetch(
    repo: &mut gix::Repository,
    remote_url: &str,
    reference: &GitReference,
    gctx: &GlobalContext,
    remote_kind: RemoteKind,
) -> CargoResult<()> {
    if let Some(offline_flag) = gctx.offline_flag() {
        anyhow::bail!(
            "attempting to update a git repository, but {offline_flag} \
             was specified"
        )
    }

    let shallow = remote_kind.to_shallow_setting(repo.is_shallow(), gctx);

    // Flag to keep track if the rev is a full commit hash
    let mut fast_path_rev: bool = false;

    let oid_to_fetch = match github_fast_path(repo, remote_url, reference, gctx) {
        Ok(FastPathRev::UpToDate) => return Ok(()),
        Ok(FastPathRev::NeedsFetch(rev)) => Some(rev),
        Ok(FastPathRev::Indeterminate) => None,
        Err(e) => {
            debug!("failed to check github {:?}", e);
            None
        }
    };

    maybe_gc_repo(repo, gctx)?;

    clean_repo_temp_files(repo);

    // Translate the reference desired here into an actual list of refspecs
    // which need to get fetched. Additionally record if we're fetching tags.
    let mut refspecs = Vec::new();
    let mut tags = false;
    // The `+` symbol on the refspec means to allow a forced (fast-forward)
    // update which is needed if there is ever a force push that requires a
    // fast-forward.
    match reference {
        // For branches and tags we can fetch simply one reference and copy it
        // locally, no need to fetch other branches/tags.
        GitReference::Branch(b) => {
            refspecs.push(format!("+refs/heads/{0}:refs/remotes/origin/{0}", b));
            // Also fetch HEAD and all branches to ensure we get the commit that
            // force-updated branches might point to (the commit may not be reachable
            // from any other ref if the branch was force-updated after an amend)
            refspecs.push(String::from("+HEAD:refs/remotes/origin/HEAD"));
            refspecs.push(String::from("+refs/heads/*:refs/remotes/origin/*"));
        }

        GitReference::Tag(t) => {
            refspecs.push(format!("+refs/tags/{0}:refs/remotes/origin/tags/{0}", t));
            // Also fetch HEAD and its branch to ensure we get the commit that
            // force-updated tags might point to (the commit may not be reachable
            // from any other ref if the tag was force-updated after an amend)
            refspecs.push(String::from("+HEAD:refs/remotes/origin/HEAD"));
            refspecs.push(String::from("+refs/heads/*:refs/remotes/origin/*"));
        }

        GitReference::DefaultBranch => {
            refspecs.push(String::from("+HEAD:refs/remotes/origin/HEAD"));
            // Also fetch the branch that HEAD points to. Without this, on subsequent
            // fetches, HEAD becomes a symbolic ref but the branch it points to may be
            // stale or missing, causing resolve_ref to return the old commit.
            refspecs.push(String::from("+refs/heads/*:refs/remotes/origin/*"));
        }

        GitReference::Rev(rev) => {
            if rev.starts_with("refs/") {
                refspecs.push(format!("+{0}:{0}", rev));
            } else if let Some(oid_to_fetch) = oid_to_fetch {
                fast_path_rev = true;
                refspecs.push(format!("+{0}:refs/commit/{0}", oid_to_fetch));
            } else if !matches!(shallow, gix::remote::fetch::Shallow::NoChange)
                && ObjectId::from_hex(rev.as_bytes()).is_ok()
            {
                // There is a specific commit to fetch and we will do so in shallow-mode only
                // to not disturb the previous logic.
                // Note that with typical settings for shallowing, we will just fetch a single `rev`
                // as single commit.
                // The reason we write to `refs/remotes/origin/HEAD` is that it's of special significance
                // when during `GitReference::resolve()`, but otherwise it shouldn't matter.
                refspecs.push(format!("+{0}:refs/remotes/origin/HEAD", rev));
            } else {
                // We don't know what the rev will point to. To handle this
                // situation we fetch all branches and tags, and then we pray
                // it's somewhere in there.
                refspecs.push(String::from("+refs/heads/*:refs/remotes/origin/*"));
                refspecs.push(String::from("+HEAD:refs/remotes/origin/HEAD"));
                tags = true;
            }
        }
    }

    debug!("doing a fetch for {remote_url}");
    // Always use gitoxide for network operations.
    // Fall back to git CLI if explicitly requested.
    let result = if let Some(true) = gctx.net_config()?.git_fetch_with_cli {
        fetch_with_cli(repo, remote_url, &refspecs, tags, shallow, gctx)
    } else {
        fetch_with_gitoxide(repo, remote_url, refspecs, tags, shallow, gctx)
    };

    if fast_path_rev {
        if let Some(oid) = oid_to_fetch {
            return result.with_context(|| format!("revision {} not found", oid));
        }
    }
    result
}

/// `gitoxide` uses shallow locks to assure consistency when fetching to and to avoid races, and to write
/// files atomically.
/// Cargo has its own lock files and doesn't need that mechanism for race protection, so a stray lock means
/// a signal interrupted a previous shallow fetch and doesn't mean a race is happening.
fn has_shallow_lock_file(err: &crate::sources::git::fetch::Error) -> bool {
    matches!(
        err,
        gix::env::collate::fetch::Error::Fetch(gix::remote::fetch::Error::Fetch(
            gix::protocol::fetch::Error::LockShallowFile(_)
        ))
    )
}

/// Attempts to use `git` CLI installed on the system to fetch a repository,
/// when the config value [`net.git-fetch-with-cli`][1] is set.
///
/// This is an escape hatch for users that would prefer to use `git`-the-CLI
/// for fetching repositories. This can help with authentication issues as
/// git CLI has more authentication options.
///
/// [1]: https://doc.rust-lang.org/nightly/cargo/reference/config.html#netgit-fetch-with-cli
fn fetch_with_cli(
    repo: &mut gix::Repository,
    url: &str,
    refspecs: &[String],
    tags: bool,
    shallow: gix::remote::fetch::Shallow,
    gctx: &GlobalContext,
) -> CargoResult<()> {
    debug!(target: "git-fetch", backend = "git-cli");

    let mut cmd = ProcessBuilder::new("git");
    cmd.arg("fetch");
    if tags {
        cmd.arg("--tags");
    } else {
        cmd.arg("--no-tags");
    }
    if let gix::remote::fetch::Shallow::DepthAtRemote(depth) = shallow {
        let depth = 0i32.saturating_add_unsigned(depth.get());
        cmd.arg(format!("--depth={depth}"));
    }
    match gctx.shell().verbosity() {
        Verbosity::Normal => {}
        Verbosity::Verbose => {
            cmd.arg("--verbose");
        }
        Verbosity::Quiet => {
            cmd.arg("--quiet");
        }
    }
    cmd.arg("--force") // handle force pushes
        .arg("--update-head-ok") // see discussion in #2078
        .arg(url)
        .args(refspecs)
        // If cargo is run by git (for example, the `exec` command in `git
        // rebase`), the GIT_DIR is set by git and will point to the wrong
        // location. This makes sure GIT_DIR is always the repository path.
        .env("GIT_DIR", repo.path())
        // The reset of these may not be necessary, but I'm including them
        // just to be extra paranoid and avoid any issues.
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .cwd(repo.path());
    gctx.shell()
        .verbose(|s| s.status("Running", &cmd.to_string()))?;
    network::retry::with_retry(gctx, || {
        cmd.exec()
            .map_err(|error| GitCliError::new(error, true).into())
    })?;

    // After CLI fetch, sync refs/heads/main with refs/remotes/origin/main so gix's
    // clone will fetch all the objects we need. Without this, HEAD points to refs/heads/main
    // which may be stale (or non-existent), causing clone_into to fail.
    let repo_path = repo.path();
    let refs_heads_main = repo_path.join("refs/heads/main");
    let origin_main = repo_path.join("refs/remotes/origin/main");
    let origin_head = repo_path.join("refs/remotes/origin/HEAD");

    let commit_id = if origin_main.exists() {
        std::fs::read_to_string(&origin_main).ok()
    } else if origin_head.exists() {
        std::fs::read_to_string(&origin_head)
            .ok()
            .and_then(|content| {
                if content.trim().starts_with("ref:") {
                    // It's a symbolic ref, follow it
                    let target = content.trim().strip_prefix("ref:").unwrap().trim();
                    let target_path = repo_path.join(target);
                    std::fs::read_to_string(&target_path).ok()
                } else {
                    Some(content)
                }
            })
    } else {
        None
    };

    if let Some(commit) = commit_id {
        let commit = commit.trim();
        if !commit.is_empty() && !commit.starts_with("ref:") {
            std::fs::create_dir_all(repo_path.join("refs/heads"))?;
            std::fs::write(&refs_heads_main, format!("{}\n", commit))?;
        }
    }

    Ok(())
}

fn fetch_with_gitoxide(
    repo: &mut gix::Repository,
    remote_url: &str,
    refspecs: Vec<String>,
    tags: bool,
    shallow: gix::remote::fetch::Shallow,
    gctx: &GlobalContext,
) -> CargoResult<()> {
    debug!(target: "git-fetch", backend = "gitoxide");

    let repo_path = repo.path().to_path_buf();
    let config_overrides = cargo_config_to_gitoxide_overrides(gctx)?;
    let repo_reinitialized = AtomicBool::default();
    let res = with_retry_and_progress(
        &repo_path,
        gctx,
        remote_url,
        &|repo_path,
          should_interrupt,
          mut progress,
          url_for_authentication: &mut dyn FnMut(&gix::bstr::BStr)| {
            // The `fetch` operation here may fail spuriously due to a corrupt
            // repository. It could also fail, however, for a whole slew of other
            // reasons (aka network related reasons). We want Cargo to automatically
            // recover from corrupt repositories, but we don't want Cargo to stomp
            // over other legitimate errors.
            //
            // Consequently we save off the error of the `fetch` operation and if it
            // looks like a "corrupt repo" error then we blow away the repo and try
            // again. If it looks like any other kind of error, or if we've already
            // blown away the repository, then we want to return the error as-is.
            loop {
                let res = open_repo(
                    repo_path,
                    config_overrides.clone(),
                    OpenMode::ForFetch,
                )
                .map_err(crate::sources::git::fetch::Error::from)
                .and_then(|repo| {
                    debug!("initiating fetch of {refspecs:?} from {remote_url}");
                    let url_for_authentication = &mut *url_for_authentication;
                    let remote = repo
                        .remote_at(remote_url)?
                        .with_fetch_tags(if tags {
                            gix::remote::fetch::Tags::All
                        } else {
                            gix::remote::fetch::Tags::Included
                        })
                        .with_refspecs(
                            refspecs.iter().map(|s| s.as_str()),
                            gix::remote::Direction::Fetch,
                        )
                        .map_err(crate::sources::git::fetch::Error::Other)?;
                    let url = remote
                        .url(gix::remote::Direction::Fetch)
                        .expect("set at init")
                        .to_owned();
                    let connection = remote.connect(gix::remote::Direction::Fetch)?;
                    let mut authenticate = connection.configured_credentials(url)?;
                    let connection = connection.with_credentials(
                        move |action: gix::protocol::credentials::helper::Action| {
                            if let Some(url) = action
                                .context()
                                .and_then(|gctx| gctx.url.as_ref().filter(|url| *url != remote_url))
                            {
                                url_for_authentication(url.as_ref());
                            }
                            authenticate(action)
                        },
                    );
                    let outcome = connection
                        .prepare_fetch(&mut progress, gix::remote::ref_map::Options::default())?
                        .with_shallow(shallow.clone())
                        .receive(&mut progress, should_interrupt)?;
                    Ok(outcome)
                });
                let err = match res {
                    Ok(_) => break,
                    Err(e) => e,
                };
                debug!("fetch failed: {}", err);

                if !repo_reinitialized.load(Ordering::Relaxed)
                        // We check for errors that could occur if the configuration, refs or odb files are corrupted.
                        // We don't check for errors related to writing as `gitoxide` is expected to create missing leading
                        // folder before writing files into it, or else not even open a directory as git repository (which is
                        // also handled here).
                        && err.is_corrupted()
                    || has_shallow_lock_file(&err)
                {
                    repo_reinitialized.store(true, Ordering::Relaxed);
                    debug!(
                        "looks like this is a corrupt repository, reinitializing \
                     and trying again"
                    );
                    match reinitialize_at_path(repo_path) {
                        Ok(()) => {
                            debug!("reinitialize succeeded, retrying fetch");
                            continue;
                        }
                        Err(e) => {
                            debug!("reinitialize failed: {}", e);
                        }
                    }
                }

                return Err(err.into());
            }
            Ok(())
        },
    );
    // After fetch (whether gitoxide or reinitialized), we need to reopen the repo
    // so it sees the newly fetched objects. The fetch happens on a separate repo
    // handle opened inside the closure, so the original `repo` has a stale snapshot.
    if res.is_ok() {
        // Sync refs/heads/main with refs/remotes/origin/main so gix's clone will
        // fetch all the objects we need. Without this, HEAD points to refs/heads/main
        // which may be stale, causing clone_into to only fetch old objects.
        let refs_heads_main = repo_path.join("refs/heads/main");
        let origin_main = repo_path.join("refs/remotes/origin/main");
        let origin_head = repo_path.join("refs/remotes/origin/HEAD");

        let commit_id = if origin_main.exists() {
            std::fs::read_to_string(&origin_main).ok()
        } else if origin_head.exists() {
            std::fs::read_to_string(&origin_head)
                .ok()
                .and_then(|content| {
                    if content.trim().starts_with("ref:") {
                        // It's a symbolic ref, follow it
                        let target = content.trim().strip_prefix("ref:").unwrap().trim();
                        let target_path = repo_path.join(target);
                        std::fs::read_to_string(&target_path).ok()
                    } else {
                        Some(content)
                    }
                })
        } else {
            None
        };

        if let Some(commit) = commit_id {
            let commit = commit.trim();
            if !commit.is_empty() && !commit.starts_with("ref:") {
                let _ = std::fs::create_dir_all(repo_path.join("refs/heads"));
                let _ = std::fs::write(&refs_heads_main, format!("{}\n", commit));
            }
        }

        *repo = gix::open(&repo_path)?;
    }
    res
}

/// Attempts to `git gc` a repository.
///
/// Cargo has a bunch of long-lived git repositories in its global cache and
/// some, like the index, are updated very frequently. Right now each update
/// creates a new "pack file" inside the git database, and over time this can
/// cause bad performance.
///
/// One pathological use case today is where hundreds of file descriptors are
/// opened, getting us dangerously close to blowing out the OS limits. This is
/// detailed in [#4403].
///
/// To try to combat this problem we attempt a `git gc` here. Note, though, that
/// we may not even have `git` installed on the system! As a result we
/// opportunistically try a `git gc` when the pack directory looks too big, and
/// failing that we just blow away the repository and start over.
///
/// In theory this shouldn't be too expensive compared to the network request
/// we're about to issue.
///
/// [#4403]: https://github.com/rust-lang/cargo/issues/4403
fn maybe_gc_repo(repo: &mut gix::Repository, gctx: &GlobalContext) -> CargoResult<()> {
    // Here we arbitrarily declare that if you have more than 100 files in your
    // `pack` folder that we need to do a gc.
    let entries = match repo.path().join("objects/pack").read_dir() {
        Ok(e) => e.count(),
        Err(_) => {
            debug!("skipping gc as pack dir appears gone");
            return Ok(());
        }
    };
    let max = gctx
        .get_env("__CARGO_PACKFILE_LIMIT")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100);
    if entries < max {
        debug!("skipping gc as there's only {} pack files", entries);
        return Ok(());
    }

    // gix doesn't have a built-in gc operation, so we reinitialize the repo
    // when there are too many pack files. This is the same fallback behavior
    // that was used when git gc failed.
    reinitialize(repo)
}

/// Removes temporary files left from previous activity.
///
/// If git is interrupted while indexing pack files, it will leave behind
/// some temporary files that it doesn't clean up. These can be quite large in
/// size, so this tries to clean things up.
///
/// This intentionally ignores errors. This is only an opportunistic cleaning,
/// and we don't really care if there are issues (there's unlikely anything
/// that can be done).
///
/// The git CLI has similar behavior (its temp files look like
/// `objects/pack/tmp_pack_9kUSA8`). Those files are normally deleted via `git
/// prune` which is run by `git gc`.
fn clean_repo_temp_files(repo: &gix::Repository) {
    let path = repo.path().join("objects/pack/pack_git2_*");
    let Some(pattern) = path.to_str() else {
        tracing::warn!("cannot convert {path:?} to a string");
        return;
    };
    let Ok(paths) = glob::glob(pattern) else {
        return;
    };
    for path in paths {
        if let Ok(path) = path {
            match paths::remove_file(&path) {
                Ok(_) => tracing::debug!("removed stale temp git file {path:?}"),
                Err(e) => {
                    tracing::warn!("failed to remove {path:?} while cleaning temp files: {e}")
                }
            }
        }
    }
}

/// Recursively copy contents of one directory to another.
fn copy_dir_contents(src: &Path, dst: &Path) -> CargoResult<()> {
    paths::create_dir_all(dst)?;
    for entry in src.read_dir()? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_contents(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Reinitializes a given Git repository. This is useful when a Git repository
/// seems corrupted and we want to start over.
fn reinitialize(repo: &mut gix::Repository) -> CargoResult<()> {
    // Here we want to drop the current repository object pointed to by `repo`,
    // so we initialize temporary repository in a sub-folder, blow away the
    // existing git folder, and then recreate the git repo. Finally we blow away
    // the `tmp` folder we allocated.
    let path = repo.path().to_path_buf();
    debug!("reinitializing git repo at {:?}", path);
    let tmp = path.join("tmp");
    let bare = !repo.path().ends_with(".git");
    *repo = init(&tmp, false)?;
    for entry in path.read_dir()? {
        let entry = entry?;
        if entry.file_name().to_str() == Some("tmp") {
            continue;
        }
        let path = entry.path();
        drop(paths::remove_file(&path).or_else(|_| paths::remove_dir_all(&path)));
    }
    *repo = init(&path, bare)?;
    paths::remove_dir_all(&tmp)?;
    Ok(())
}

/// Initializes a Git repository at `path`.
fn init(path: &Path, bare: bool) -> CargoResult<gix::Repository> {
    let repo = if bare {
        gix::init_bare(path)?
    } else {
        gix::init(path)?
    };
    Ok(repo)
}

/// The result of GitHub fast path check. See [`github_fast_path`] for more.
enum FastPathRev {
    /// The local rev (determined by `reference.resolve(repo)`) is already up to
    /// date with what this rev resolves to on GitHub's server.
    UpToDate,
    /// The following SHA must be fetched in order for the local rev to become
    /// up to date.
    NeedsFetch(ObjectId),
    /// Don't know whether local rev is up to date. We'll fetch _all_ branches
    /// and tags from the server and see what happens.
    Indeterminate,
}

/// Attempts GitHub's special fast path for testing if we've already got an
/// up-to-date copy of the repository.
///
/// Updating the index is done pretty regularly so we want it to be as fast as
/// possible. For registries hosted on GitHub (like the crates.io index) there's
/// a fast path available to use[^1] to tell us that there's no updates to be
/// made.
///
/// Note that this function should never cause an actual failure because it's
/// just a fast path. As a result, a caller should ignore `Err` returned from
/// this function and move forward on the normal path.
///
/// [^1]: <https://developer.github.com/v3/repos/commits/#get-the-sha-1-of-a-commit-reference>
fn github_fast_path(
    repo: &mut gix::Repository,
    url: &str,
    reference: &GitReference,
    gctx: &GlobalContext,
) -> CargoResult<FastPathRev> {
    let url = Url::parse(url)?;
    if !is_github(&url) {
        return Ok(FastPathRev::Indeterminate);
    }

    let local_object = resolve_ref(reference, repo).ok();

    let github_branch_name = match reference {
        GitReference::Branch(branch) => branch,
        GitReference::Tag(tag) => tag,
        GitReference::DefaultBranch => "HEAD",
        GitReference::Rev(rev) => {
            if rev.starts_with("refs/") {
                rev
            } else if looks_like_commit_hash(rev) {
                // `rev_parse_single` (used by `resolve`) is the only way to turn
                // short hash -> long hash, but it also parses other things,
                // like branch and tag names, which might coincidentally be
                // valid hex.
                //
                // We only return early if `rev` is a prefix of the object found
                // by `rev_parse_single`. Don't bother talking to GitHub in that
                // case, since commit hashes are permanent. If a commit with the
                // requested hash is already present in the local clone, its
                // contents must be the same as what is on the server for that
                // hash.
                //
                // If `rev` is not found locally by `rev_parse_single`, we'll
                // need GitHub to resolve it and get a hash. If `rev` is found
                // but is not a short hash of the found object, it's probably a
                // branch and we also need to get a hash from GitHub, in case
                // the branch has moved.
                if let Some(local_object) = local_object {
                    if is_short_hash_of(rev, local_object) {
                        debug!("github fast path already has {local_object}");
                        return Ok(FastPathRev::UpToDate);
                    }
                }
                // If `rev` is a full commit hash, the only thing it can resolve
                // to is itself. Don't bother talking to GitHub in that case
                // either. (This ensures that we always attempt to fetch the
                // commit directly even if we can't reach the GitHub API.)
                if let Some(oid) = rev_to_oid(rev) {
                    debug!("github fast path is already a full commit hash {rev}");
                    return Ok(FastPathRev::NeedsFetch(oid));
                }
                rev
            } else {
                debug!("can't use github fast path with `rev = \"{}\"`", rev);
                return Ok(FastPathRev::Indeterminate);
            }
        }
    };

    // This expects GitHub urls in the form `github.com/user/repo` and nothing
    // else
    let mut pieces = url
        .path_segments()
        .ok_or_else(|| anyhow!("no path segments on url"))?;
    let username = pieces
        .next()
        .ok_or_else(|| anyhow!("couldn't find username"))?;
    let repository = pieces
        .next()
        .ok_or_else(|| anyhow!("couldn't find repository name"))?;
    if pieces.next().is_some() {
        anyhow::bail!("too many segments on URL");
    }

    // Trim off the `.git` from the repository, if present, since that's
    // optional for GitHub and won't work when we try to use the API as well.
    let repository = repository.strip_suffix(".git").unwrap_or(repository);

    let url = format!(
        "https://api.github.com/repos/{}/{}/commits/{}",
        username, repository, github_branch_name,
    );
    let mut handle = gctx.http()?.lock().unwrap();
    debug!("attempting GitHub fast path for {}", url);
    handle.get(true)?;
    handle.url(&url)?;
    handle.useragent("cargo")?;
    handle.follow_location(true)?; // follow redirects
    handle.http_headers({
        let mut headers = List::new();
        headers.append("Accept: application/vnd.github.3.sha")?;
        if let Some(local_object) = local_object {
            headers.append(&format!("If-None-Match: \"{}\"", local_object))?;
        }
        headers
    })?;

    let mut response_body = Vec::new();
    let mut transfer = handle.transfer();
    transfer.write_function(|data| {
        response_body.extend_from_slice(data);
        Ok(data.len())
    })?;
    transfer.perform()?;
    drop(transfer); // end borrow of handle so that response_code can be called

    let response_code = handle.response_code()?;
    if response_code == 304 {
        debug!("github fast path up-to-date");
        Ok(FastPathRev::UpToDate)
    } else if response_code == 200 {
        let hex_str = str::from_utf8(&response_body)?;
        let oid_to_fetch = ObjectId::from_hex(hex_str.trim().as_bytes())?;
        debug!("github fast path fetch {oid_to_fetch}");
        Ok(FastPathRev::NeedsFetch(oid_to_fetch))
    } else {
        // Usually response_code == 404 if the repository does not exist, and
        // response_code == 422 if exists but GitHub is unable to resolve the
        // requested rev.
        debug!("github fast path bad response code {response_code}");
        Ok(FastPathRev::Indeterminate)
    }
}

/// Whether a `url` is one from GitHub.
fn is_github(url: &Url) -> bool {
    url.host_str() == Some("github.com")
}

// Give some messages on GitHub PR URL given as is
pub(crate) fn note_github_pull_request(url: &str) -> Option<String> {
    if let Ok(url) = url.parse::<Url>()
        && is_github(&url)
    {
        let path_segments = url
            .path_segments()
            .map(|p| p.into_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        if let [owner, repo, "pull", pr_number, ..] = path_segments[..] {
            let repo_url = format!("https://github.com/{owner}/{repo}.git");
            let rev = format!("refs/pull/{pr_number}/head");
            return Some(format!(
                concat!(
                    "\n\nnote: GitHub url {} is not a repository. \n",
                    "help: Replace the dependency with \n",
                    "       `git = \"{}\" rev = \"{}\"` \n",
                    "   to specify pull requests as dependencies' revision."
                ),
                url, repo_url, rev
            ));
        }
    }

    None
}

/// Whether a `rev` looks like a commit hash (ASCII hex digits).
fn looks_like_commit_hash(rev: &str) -> bool {
    rev.len() >= 7 && rev.chars().all(|ch| ch.is_ascii_hexdigit())
}

/// Whether `rev` is a shorter hash of `oid`.
fn is_short_hash_of(rev: &str, oid: ObjectId) -> bool {
    let long_hash = oid.to_string();
    match long_hash.get(..rev.len()) {
        Some(truncated_long_hash) => truncated_long_hash.eq_ignore_ascii_case(rev),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::absolute_submodule_url;

    #[test]
    fn test_absolute_submodule_url() {
        let cases = [
            (
                "ssh://git@gitub.com/rust-lang/cargo",
                "git@github.com:rust-lang/cargo.git",
                "git@github.com:rust-lang/cargo.git",
            ),
            (
                "ssh://git@gitub.com/rust-lang/cargo",
                "./",
                "ssh://git@gitub.com/rust-lang/cargo/",
            ),
            (
                "ssh://git@gitub.com/rust-lang/cargo",
                "../",
                "ssh://git@gitub.com/rust-lang/",
            ),
            (
                "ssh://git@gitub.com/rust-lang/cargo",
                "./foo",
                "ssh://git@gitub.com/rust-lang/cargo/foo",
            ),
            (
                "ssh://git@gitub.com/rust-lang/cargo/",
                "./foo",
                "ssh://git@gitub.com/rust-lang/cargo/foo",
            ),
            (
                "ssh://git@gitub.com/rust-lang/cargo/",
                "../foo",
                "ssh://git@gitub.com/rust-lang/foo",
            ),
            (
                "ssh://git@gitub.com/rust-lang/cargo",
                "../foo",
                "ssh://git@gitub.com/rust-lang/foo",
            ),
            (
                "ssh://git@gitub.com/rust-lang/cargo",
                "../foo/bar/../baz",
                "ssh://git@gitub.com/rust-lang/foo/baz",
            ),
            (
                "git@github.com:rust-lang/cargo.git",
                "ssh://git@gitub.com/rust-lang/cargo",
                "ssh://git@gitub.com/rust-lang/cargo",
            ),
            (
                "git@github.com:rust-lang/cargo.git",
                "./",
                "git@github.com:rust-lang/cargo.git/./",
            ),
            (
                "git@github.com:rust-lang/cargo.git",
                "../",
                "git@github.com:rust-lang/cargo.git/../",
            ),
            (
                "git@github.com:rust-lang/cargo.git",
                "./foo",
                "git@github.com:rust-lang/cargo.git/./foo",
            ),
            (
                "git@github.com:rust-lang/cargo.git/",
                "./foo",
                "git@github.com:rust-lang/cargo.git/./foo",
            ),
            (
                "git@github.com:rust-lang/cargo.git",
                "../foo",
                "git@github.com:rust-lang/cargo.git/../foo",
            ),
            (
                "git@github.com:rust-lang/cargo.git/",
                "../foo",
                "git@github.com:rust-lang/cargo.git/../foo",
            ),
            (
                "git@github.com:rust-lang/cargo.git",
                "../foo/bar/../baz",
                "git@github.com:rust-lang/cargo.git/../foo/bar/../baz",
            ),
        ];

        for (base_url, submodule_url, expected) in cases {
            let url = absolute_submodule_url(base_url, submodule_url).unwrap();
            assert_eq!(
                expected, url,
                "base `{base_url}`; submodule `{submodule_url}`"
            );
        }
    }
}

/// Turns a full commit hash revision into an ObjectId.
///
/// Git object ID is supposed to be a hex string of 20 (SHA1) or 32 (SHA256) bytes.
/// Its length must be double to the underlying bytes (40 or 64).
///
/// See:
///
/// * <https://github.com/rust-lang/cargo/issues/13188>
/// * <https://github.com/rust-lang/cargo/issues/13968>
pub(super) fn rev_to_oid(rev: &str) -> Option<ObjectId> {
    ObjectId::from_hex(rev.as_bytes())
        .ok()
        .filter(|oid| oid.as_bytes().len() * 2 == rev.len())
}

// =============================================================================
// Gitoxide fetch helpers (formerly in oxide.rs)
// =============================================================================

/// Wraps a gitoxide fetch operation with retry logic and a progress bar.
fn with_retry_and_progress(
    repo_path: &std::path::Path,
    gctx: &GlobalContext,
    repo_remote_url: &str,
    cb: &(
         dyn Fn(
        &std::path::Path,
        &AtomicBool,
        &mut gix::progress::tree::Item,
        &mut dyn FnMut(&gix::bstr::BStr),
    ) -> Result<(), crate::sources::git::fetch::Error>
             + Send
             + Sync
     ),
) -> CargoResult<()> {
    std::thread::scope(|s| {
        let mut progress_bar = Progress::new("Fetch", gctx);
        let is_shallow = gctx.cli_unstable().git.map_or(false, |features| {
            features.shallow_deps || features.shallow_index
        });
        network::retry::with_retry(gctx, || {
            let progress_root: Arc<gix::progress::tree::Root> =
                gix::progress::tree::root::Options {
                    initial_capacity: 10,
                    message_buffer_capacity: 10,
                }
                .into();
            let root = Arc::downgrade(&progress_root);
            let thread = s.spawn(move || {
                let mut progress = progress_root.add_child("operation");
                let mut urls = RefCell::new(Default::default());
                let res = cb(
                    &repo_path,
                    &AtomicBool::default(),
                    &mut progress,
                    &mut |url| {
                        *urls.borrow_mut() = Some(url.to_owned());
                    },
                );
                amend_authentication_hints(res, repo_remote_url, urls.get_mut().take())
            });
            translate_progress_to_bar(&mut progress_bar, root, is_shallow)?;
            thread.join().expect("no panic in scoped thread")
        })
    })
}

fn translate_progress_to_bar(
    progress_bar: &mut Progress<'_>,
    root: Weak<gix::progress::tree::Root>,
    is_shallow: bool,
) -> CargoResult<()> {
    let remote_progress: gix::progress::Id = gix::remote::fetch::ProgressId::RemoteProgress.into();
    let read_pack_bytes: gix::progress::Id =
        gix::odb::pack::bundle::write::ProgressId::ReadPackBytes.into();
    let delta_index_objects: gix::progress::Id =
        gix::odb::pack::index::write::ProgressId::IndexObjects.into();
    let resolve_objects: gix::progress::Id =
        gix::odb::pack::index::write::ProgressId::ResolveObjects.into();

    // We choose `N=10` here to make a `300ms * 10slots ~= 3000ms`
    // sliding window for tracking the data transfer rate (in bytes/s).
    let mut last_percentage_update = Instant::now();
    let mut last_fast_update = Instant::now();
    let mut counter = MetricsCounter::<10>::new(0, last_percentage_update);

    let mut tasks = Vec::with_capacity(10);
    let slow_check_interval = std::time::Duration::from_millis(300);
    let fast_check_interval = Duration::from_millis(50);
    let sleep_interval = Duration::from_millis(10);
    debug_assert_eq!(
        slow_check_interval.as_millis() % fast_check_interval.as_millis(),
        0,
        "progress should be smoother by keeping these as multiples of each other"
    );
    debug_assert_eq!(
        fast_check_interval.as_millis() % sleep_interval.as_millis(),
        0,
        "progress should be smoother by keeping these as multiples of each other"
    );

    let num_phases = if is_shallow { 3 } else { 2 }; // indexing + delta-resolution, both with same amount of objects to handle
    while let Some(root) = root.upgrade() {
        std::thread::sleep(sleep_interval);
        let needs_update = last_fast_update.elapsed() >= fast_check_interval;
        if !needs_update {
            continue;
        }
        let now = Instant::now();
        last_fast_update = now;

        root.sorted_snapshot(&mut tasks);

        fn progress_by_id(
            id: gix::progress::Id,
            task: &gix::progress::Task,
        ) -> Option<(&str, &gix::progress::Value)> {
            (task.id == id)
                .then(|| task.progress.as_ref())
                .flatten()
                .map(|value| (task.name.as_str(), value))
        }
        fn find_in<K>(
            tasks: &[(K, gix::progress::Task)],
            cb: impl Fn(&gix::progress::Task) -> Option<(&str, &gix::progress::Value)>,
        ) -> Option<(&str, &gix::progress::Value)> {
            tasks.iter().find_map(|(_, t)| cb(t))
        }

        if let Some((_, objs)) = find_in(&tasks, |t| progress_by_id(resolve_objects, t)) {
            // Phase 3: Resolving deltas.
            let objects = objs.step.load(Ordering::Relaxed);
            let total_objects = objs.done_at.expect("known amount of objects");
            let msg = format!(", ({objects}/{total_objects}) resolving deltas");

            progress_bar.tick(
                (total_objects * (num_phases - 1)) + objects,
                total_objects * num_phases,
                &msg,
            )?;
        } else if let Some((objs, read_pack)) =
            find_in(&tasks, |t| progress_by_id(read_pack_bytes, t)).and_then(|read| {
                find_in(&tasks, |t| progress_by_id(delta_index_objects, t))
                    .map(|delta| (delta.1, read.1))
            })
        {
            // Phase 2: Receiving objects.
            let objects = objs.step.load(Ordering::Relaxed);
            let total_objects = objs.done_at.expect("known amount of objects");
            let received_bytes = read_pack.step.load(Ordering::Relaxed);

            let needs_percentage_update = last_percentage_update.elapsed() >= slow_check_interval;
            if needs_percentage_update {
                counter.add(received_bytes, now);
                last_percentage_update = now;
            }
            let rate = HumanBytes(counter.rate() as u64);
            let msg = format!(", {rate:.2}/s");

            progress_bar.tick(
                (total_objects * (num_phases - 2)) + objects,
                total_objects * num_phases,
                &msg,
            )?;
        } else if let Some((action, remote)) =
            find_in(&tasks, |t| progress_by_id(remote_progress, t))
        {
            if !is_shallow {
                continue;
            }
            // phase 1: work on the remote side

            // Resolving deltas.
            let objects = remote.step.load(Ordering::Relaxed);
            if let Some(total_objects) = remote.done_at {
                let msg = format!(", ({objects}/{total_objects}) {action}");
                progress_bar.tick(objects, total_objects * num_phases, &msg)?;
            }
        }
    }
    Ok(())
}

fn amend_authentication_hints(
    res: Result<(), crate::sources::git::fetch::Error>,
    remote_url: &str,
    last_url_for_authentication: Option<BString>,
) -> CargoResult<()> {
    let Err(err) = res else { return Ok(()) };
    let e = match &err {
        crate::sources::git::fetch::Error::PrepareFetch(
            gix::remote::fetch::prepare::Error::RefMap(gix::remote::ref_map::Error::Handshake(err)),
        ) => Some(err),
        _ => None,
    };

    if let Some(e) = e {
        let auth_message = match e {
            gix::protocol::handshake::Error::Credentials(_) => {
                "\n* attempted to find username/password via \
                     git's `credential.helper` support, but failed"
                    .into()
            }
            gix::protocol::handshake::Error::InvalidCredentials { .. } => {
                "\n* attempted to find username/password via \
                     `credential.helper`, but maybe the found \
                     credentials were incorrect"
                    .into()
            }
            gix::protocol::handshake::Error::Transport(_) => {
                let msg = format!(
                    concat!(
                        "network failure seems to have happened\n",
                        "if a proxy or similar is necessary `net.git-fetch-with-cli` may help here\n",
                        "https://doc.rust-lang.org/cargo/reference/config.html#netgit-fetch-with-cli",
                        "{}"
                    ),
                    note_github_pull_request(remote_url).unwrap_or_default()
                );
                return Err(anyhow::Error::from(err).context(msg));
            }
            _ => None,
        };
        if let Some(auth_message) = auth_message {
            let mut msg = "failed to authenticate when downloading \
                       repository"
                .to_string();
            if let Some(url) = last_url_for_authentication {
                msg.push_str(": ");
                msg.push_str(url.to_str_lossy().as_ref());
            }
            msg.push('\n');
            msg.push_str(auth_message);
            msg.push_str("\n\n");
            msg.push_str("if the git CLI succeeds then `net.git-fetch-with-cli` may help here\n");
            msg.push_str(
                "https://doc.rust-lang.org/cargo/reference/config.html#netgit-fetch-with-cli",
            );
            return Err(anyhow::Error::from(err).context(msg));
        }
    }
    Err(err.into())
}

/// The reason we are opening a git repository.
///
/// This can affect the way we open it and the cost associated with it.
pub enum OpenMode {
    /// We need `git_binary` configuration as well for being able to see credential helpers
    /// that are configured with the `git` installation itself.
    /// However, this is slow on windows (~150ms) and most people won't need it as they use the
    /// standard index which won't ever need authentication, so we only enable this when needed.
    ForFetch,
}

impl OpenMode {
    /// Sometimes we don't need to pay for figuring out the system's git installation, and this tells
    /// us if that is the case.
    pub fn needs_git_binary_config(&self) -> bool {
        match self {
            OpenMode::ForFetch => true,
        }
    }
}

/// Produce a repository with everything pre-configured according to `config`. Most notably this includes
/// transport configuration. Knowing its `purpose` helps to optimize the way we open the repository.
/// Use `config_overrides` to configure the new repository.
pub fn open_repo(
    repo_path: &std::path::Path,
    config_overrides: Vec<BString>,
    purpose: OpenMode,
) -> Result<gix::Repository, gix::open::Error> {
    gix::open_opts(repo_path, {
        let mut opts = gix::open::Options::default();
        opts.permissions.config = gix::open::permissions::Config::all();
        opts.permissions.config.git_binary = purpose.needs_git_binary_config();
        opts.with(gix::sec::Trust::Full)
            .config_overrides(config_overrides)
    })
}

/// Convert `git` related cargo configuration into the respective `git` configuration which can be
/// used when opening new repositories.
pub fn cargo_config_to_gitoxide_overrides(gctx: &GlobalContext) -> CargoResult<Vec<BString>> {
    use gix::config::tree::{Core, Http, Key, gitoxide};
    let timeout = HttpTimeout::new(gctx)?;
    let http = gctx.http_config()?;

    let mut values = vec![
        gitoxide::Http::CONNECT_TIMEOUT.validated_assignment_fmt(&timeout.dur.as_millis())?,
        Http::LOW_SPEED_LIMIT.validated_assignment_fmt(&timeout.low_speed_limit)?,
        Http::LOW_SPEED_TIME.validated_assignment_fmt(&timeout.dur.as_secs())?,
        // Assure we are not depending on committer information when updating refs after cloning.
        Core::LOG_ALL_REF_UPDATES.validated_assignment_fmt(&false)?,
    ];
    if let Some(proxy) = &http.proxy {
        values.push(Http::PROXY.validated_assignment_fmt(proxy)?);
    }
    if let Some(check_revoke) = http.check_revoke {
        values.push(Http::SCHANNEL_CHECK_REVOKE.validated_assignment_fmt(&check_revoke)?);
    }
    if let Some(cainfo) = &http.cainfo {
        values.push(
            Http::SSL_CA_INFO.validated_assignment_fmt(&cainfo.resolve_path(gctx).display())?,
        );
    }

    values.push(if let Some(user_agent) = &http.user_agent {
        Http::USER_AGENT.validated_assignment_fmt(user_agent)
    } else {
        Http::USER_AGENT.validated_assignment_fmt(&format!("cargo {}", crate::version()))
    }?);
    if let Some(ssl_version) = &http.ssl_version {
        use crate::util::context::SslVersionConfig;
        match ssl_version {
            SslVersionConfig::Single(version) => {
                values.push(Http::SSL_VERSION.validated_assignment_fmt(&version)?);
            }
            SslVersionConfig::Range(range) => {
                values.push(
                    gitoxide::Http::SSL_VERSION_MIN
                        .validated_assignment_fmt(&range.min.as_deref().unwrap_or("default"))?,
                );
                values.push(
                    gitoxide::Http::SSL_VERSION_MAX
                        .validated_assignment_fmt(&range.max.as_deref().unwrap_or("default"))?,
                );
            }
        }
    } else if cfg!(windows) {
        // This text is copied from https://github.com/rust-lang/cargo/blob/39c13e67a5962466cc7253d41bc1099bbcb224c3/src/cargo/ops/registry.rs#L658-L674 .
        // This is a temporary workaround for some bugs with libcurl and
        // schannel and TLS 1.3.
        //
        // Our libcurl on Windows is usually built with schannel.
        // On Windows 11 (or Windows Server 2022), libcurl recently (late
        // 2022) gained support for TLS 1.3 with schannel, and it now defaults
        // to 1.3. Unfortunately there have been some bugs with this.
        // https://github.com/curl/curl/issues/9431 is the most recent. Once
        // that has been fixed, and some time has passed where we can be more
        // confident that the 1.3 support won't cause issues, this can be
        // removed.
        //
        // Windows 10 is unaffected. libcurl does not support TLS 1.3 on
        // Windows 10. (Windows 10 sorta had support, but it required enabling
        // an advanced option in the registry which was buggy, and libcurl
        // does runtime checks to prevent it.)
        values.push(gitoxide::Http::SSL_VERSION_MIN.validated_assignment_fmt(&"default")?);
        values.push(gitoxide::Http::SSL_VERSION_MAX.validated_assignment_fmt(&"tlsv1.2")?);
    }
    if let Some(debug) = http.debug {
        values.push(gitoxide::Http::VERBOSE.validated_assignment_fmt(&debug)?);
    }
    if let Some(multiplexing) = http.multiplexing {
        let http_version = multiplexing.then(|| "HTTP/2").unwrap_or("HTTP/1.1");
        // Note that failing to set the HTTP version in `gix-transport` isn't fatal,
        // which is why we don't have to try to figure out if HTTP V2 is supported in the
        // currently linked version (see `try_old_curl!()`)
        values.push(Http::VERSION.validated_assignment_fmt(&http_version)?);
    }

    Ok(values)
}

/// Reinitializes a given Git repository at a path. This is useful when a Git repository
/// seems corrupted and we want to start over.
pub fn reinitialize_at_path(git_dir: &Path) -> CargoResult<()> {
    // Here we want to blow away the existing git folder and recreate the repo.
    debug!("reinitializing git repo at {:?}", git_dir);
    let bare = !git_dir.ends_with(".git");

    // Remove all contents
    for entry in git_dir.read_dir()? {
        let entry = entry?;
        let path = entry.path();
        drop(paths::remove_file(&path).or_else(|_| paths::remove_dir_all(&path)));
    }

    // Reinitialize
    if bare {
        gix::init_bare(git_dir)?;
    } else {
        gix::init(git_dir)?;
    }

    Ok(())
}
