use super::CrateVersion;
use serde_json;
use std::path::Path;

use git2::{
    build::RepoBuilder, Delta, DiffFormat, Error as GitError, ErrorClass, Object, ObjectType, Oid,
    Reference, Repository, Tree,
};
use std::str;

static INDEX_GIT_URL: &str = "https://github.com/rust-lang/crates.io-index";
static LAST_SEEN_REFNAME: &str = "refs/heads/crates-index-diff_last-seen";
static EMPTY_TREE_HASH: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
static LINE_ADDED_INDICATOR: char = '+';

/// A wrapper for a repository of the crates.io index.
pub struct Index {
    /// The name and path of the reference used to keep track of the last seen state of the
    /// crates.io repository. The default value is `refs/heads/crates-index-diff_last-seen`.
    pub seen_ref_name: &'static str,
    /// The crates.io repository.
    repo: Repository,
}

/// Options for use in `Index::from_path_or_cloned_with_options`
pub struct CloneOptions {
    repository_url: String,
}

impl Index {
    /// Return the crates.io repository.
    pub fn repository(&self) -> &Repository {
        &self.repo
    }

    /// Return the reference pointing to the state we have seen after calling `fetch_changes()`.
    pub fn last_seen_reference(&self) -> Result<Reference, GitError> {
        self.repo.find_reference(self.seen_ref_name)
    }

    /// Return a new `Index` instance from the given `path`, which should contain a bare or non-bare
    /// clone of the `crates.io` index.
    /// If the directory does not contain the repository or does not exist, it will be cloned from
    /// the official location automatically (with complete history).
    ///
    /// An error will occour if the repository exists and the remote URL does not match the given repository URL.
    pub fn from_path_or_cloned_with_options(
        path: impl AsRef<Path>,
        options: CloneOptions,
    ) -> Result<Index, GitError> {
        let mut repo_did_exist = true;
        let repo = Repository::open(path.as_ref()).or_else(|err| {
            if err.class() == ErrorClass::Repository {
                repo_did_exist = false;
                RepoBuilder::new()
                    .bare(true)
                    .clone(&options.repository_url, path.as_ref())
            } else {
                Err(err)
            }
        })?;

        if repo_did_exist {
            let remote = repo.find_remote("origin")?;
            let actual_remote_url = remote
                .url()
                .ok_or_else(|| GitError::from_str("did not obtain URL of remote named 'origin'"))?;
            if actual_remote_url != options.repository_url {
                return Err(GitError::from_str(&format!(
                    "Actual 'origin' remote url {:#?} did not match desired one at {:#?}",
                    actual_remote_url, options.repository_url
                )));
            }
        }

        Ok(Index {
            repo,
            seen_ref_name: LAST_SEEN_REFNAME,
        })
    }

    /// Return a new `Index` instance from the given `path`, which should contain a bare or non-bare
    /// clone of the `crates.io` index.
    /// If the directory does not contain the repository or does not exist, it will be cloned from
    /// the official location automatically (with complete history).
    pub fn from_path_or_cloned(path: impl AsRef<Path>) -> Result<Index, GitError> {
        Index::from_path_or_cloned_with_options(
            path,
            CloneOptions {
                repository_url: INDEX_GIT_URL.into(),
            },
        )
    }

    /// As `peek_changes_with_options`, but without the options.
    pub fn peek_changes(&self) -> Result<(Vec<CrateVersion>, git2::Oid), GitError> {
        self.peek_changes_with_options(None)
    }

    /// Return all `CrateVersion`s that are observed between the last time `fetch_changes(…)` was called
    /// and the latest state of the `crates.io` index repository, which is obtained by fetching
    /// the remote called `origin`.
    /// The `last_seen_reference()` will not be created or updated.
    /// The second field in the returned tuple is the commit object to which the changes were provided.
    /// If one would set the `last_seen_reference()` to that object, the effect is exactly the same
    /// as if `fetch_changes(…)` had been called.
    pub fn peek_changes_with_options(
        &self,
        options: Option<&mut git2::FetchOptions<'_>>,
    ) -> Result<(Vec<CrateVersion>, git2::Oid), GitError> {
        let from = self
            .last_seen_reference()
            .and_then(|r| {
                r.target().ok_or_else(|| {
                    GitError::from_str("last-seen reference did not have a valid target")
                })
            })
            .or_else(|_| Oid::from_str(EMPTY_TREE_HASH))?;
        let to = {
            self.repo.find_remote("origin").and_then(|mut r| {
                r.fetch(&["refs/heads/*:refs/remotes/origin/*"], options, None)
            })?;
            let latest_fetched_commit_oid =
                self.repo.refname_to_id("refs/remotes/origin/master")?;
            latest_fetched_commit_oid
        };

        Ok((
            self.changes_from_objects(
                &self.repo.find_object(from, None)?,
                &self.repo.find_object(to, None)?,
            )?,
            to,
        ))
    }

    /// As `fetch_changes_with_options`, but without the options.
    pub fn fetch_changes(&self) -> Result<Vec<CrateVersion>, GitError> {
        self.fetch_changes_with_options(None)
    }

    /// Return all `CrateVersion`s that are observed between the last time this method was called
    /// and the latest state of the `crates.io` index repository, which is obtained by fetching
    /// the remote called `origin`.
    /// The `last_seen_reference()` will be created or adjusted to point to the latest fetched
    /// state, which causes this method to have a different result each time it is called.
    pub fn fetch_changes_with_options(
        &self,
        options: Option<&mut git2::FetchOptions<'_>>,
    ) -> Result<Vec<CrateVersion>, GitError> {
        let (changes, to) = self.peek_changes_with_options(options)?;
        self.set_last_seen_reference(to)?;
        Ok(changes)
    }

    /// Set the last seen reference to the given Oid. It will be created if it does not yet exists.
    pub fn set_last_seen_reference(&self, to: Oid) -> Result<(), GitError> {
        self.last_seen_reference()
            .and_then(|mut seen_ref| {
                seen_ref.set_target(to, "updating seen-ref head to latest fetched commit")
            })
            .or_else(|_err| {
                self.repo.reference(
                    self.seen_ref_name,
                    to,
                    true,
                    "creating seen-ref at latest fetched commit",
                )
            })?;
        Ok(())
    }

    /// Return all `CreateVersion`s observed between `from` and `to`. Both parameter are ref-specs
    /// pointing to either a commit or a tree.
    /// Learn more about specifying revisions
    /// in the
    /// [official documentation](https://www.kernel.org/pub/software/scm/git/docs/gitrevisions.html)
    pub fn changes(
        &self,
        from: impl AsRef<str>,
        to: impl AsRef<str>,
    ) -> Result<Vec<CrateVersion>, GitError> {
        self.changes_from_objects(
            &self.repo.revparse_single(from.as_ref())?,
            &self.repo.revparse_single(to.as_ref())?,
        )
    }

    /// Similar to `changes()`, but requires `from` and `to` objects to be provided. They may point
    /// to either `Commit`s or `Tree`s.
    pub fn changes_from_objects(
        &self,
        from: &Object,
        to: &Object,
    ) -> Result<Vec<CrateVersion>, GitError> {
        fn into_tree<'a>(repo: &'a Repository, obj: &Object) -> Result<Tree<'a>, GitError> {
            repo.find_tree(match obj.kind() {
                Some(ObjectType::Commit) => obj
                    .as_commit()
                    .expect("object of kind commit yields commit")
                    .tree_id(),
                _ =>
                /* let it possibly fail later */
                {
                    obj.id()
                }
            })
        }
        let diff = self.repo.diff_tree_to_tree(
            Some(&into_tree(&self.repo, from)?),
            Some(&into_tree(&self.repo, to)?),
            None,
        )?;
        let mut res: Vec<CrateVersion> = Vec::new();
        diff.print(DiffFormat::Patch, |delta, _, diffline| {
            if diffline.origin() != LINE_ADDED_INDICATOR {
                return true;
            }

            if !match delta.status() {
                Delta::Added | Delta::Modified => true,
                _ => false,
            } {
                return true;
            }

            if let Ok(c) = serde_json::from_slice(diffline.content()) {
                res.push(c)
            }
            true
        })
        .map(|_| res)
    }
}
