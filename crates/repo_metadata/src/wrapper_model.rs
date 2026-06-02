//! Unified repository metadata model.
//!
//! [`RepoMetadataModel`] is the singleton entry point for all repository metadata
//! queries. It holds handles to [`LocalRepoMetadataModel`] and
//! [`RemoteRepoMetadataModel`] and dispatches operations based on
//! [`RepositoryIdentifier`].

#[cfg(feature = "local_fs")]
use std::path::Path;

use futures::future::{self, BoxFuture, FutureExt as _};
use serde::{Deserialize, Serialize};
#[cfg(feature = "local_fs")]
use walkdir::WalkDir;
use warp_core::HostId;
use warp_util::standardized_path::StandardizedPath;
use warpui_core::{AppContext, ModelContext, ModelHandle, SingletonEntity};
use crate::file_tree_store::{FileTreeEntry, FileTreeEntryState, FileTreeState};
use crate::file_tree_update::{MetadataUpdateType, RepoMetadataUpdate};
use crate::local_model::{
    GetContentsArgs, IndexedRepoState, LocalRepoMetadataModel, OwnedRepoContent, RepoContent,
    RepositoryMetadataEvent,
};
use crate::remote_model::{RemoteRepoMetadataModel, RemoteRepositoryMetadataEvent};
use crate::repository_identifier::{RemoteRepositoryIdentifier, RepositoryIdentifier};
use crate::RepoMetadataError;

/// Default maximum file-system traversal work permitted by an authoritative contents query.
const DEFAULT_AUTHORITATIVE_QUERY_MAX_ENTRIES_SCANNED: usize = 100_000;

/// Maximum file-system traversal work permitted by an authoritative contents query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoContentsQueryBudget {
    pub max_entries_scanned: usize,
}

impl Default for RepoContentsQueryBudget {
    fn default() -> Self {
        Self {
            max_entries_scanned: DEFAULT_AUTHORITATIVE_QUERY_MAX_ENTRIES_SCANNED,
        }
    }
}

/// Serializable descriptions of authoritative metadata discovery requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoMetadataQuery {
    /// Finds `SKILL.md` files below registered provider roots and returns provider directories
    /// as well so local callers can supplement indexed paths with symlinked skill directories.
    ProjectSkillFiles { provider_paths: Vec<String> },
    /// Finds files whose basename equals one of the requested names.
    FilesNamed { names: Vec<String> },
}
#[cfg(test)]
#[path = "wrapper_model_tests.rs"]
mod tests;

/// Explicit failure states for completeness-aware repository-content queries.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RepoContentsQueryError {
    #[error("Repository metadata was not found for {0:?}")]
    RepositoryNotFound(RepositoryIdentifier),
    #[error("Repository metadata indexing is still pending for {0:?}")]
    RepositoryPending(RepositoryIdentifier),
    #[error("Repository metadata indexing failed for {id:?}: {message}")]
    RepositoryIndexingFailed {
        id: RepositoryIdentifier,
        message: String,
    },
    #[error("No remote metadata query provider is available for {0:?}")]
    RemoteQueryUnavailable(RemoteRepositoryIdentifier),
    #[error("Repository metadata query failed: {message}")]
    QueryFailed { message: String },
    #[error("Repository metadata query budget exceeded after scanning {limit} entries")]
    QueryBudgetExceeded { limit: usize },
}

/// App-provided transport boundary for authoritative remote metadata discovery.
///
/// `repo_metadata` owns query semantics; app-layer consumers supply a provider backed by the
/// connected remote-server client without introducing a reverse crate dependency. Returned
/// query matches are discovery output and do not mutate the canonical metadata tree.
pub type RemoteMetadataQueryProvider = Box<
    dyn FnOnce(
            RemoteRepositoryIdentifier,
            RepoMetadataQuery,
            RepoContentsQueryBudget,
        ) -> BoxFuture<'static, Result<Vec<OwnedRepoContent>, RepoContentsQueryError>>
        + Send,
>;

/// Unified events emitted by the [`RepoMetadataModel`] wrapper.
///
/// These are mapped from the sub-model events into a common enum keyed by
/// [`RepositoryIdentifier`].
#[derive(Debug)]
pub enum RepoMetadataEvent {
    /// A repository was added or updated.
    RepositoryUpdated { id: RepositoryIdentifier },
    /// A repository was removed.
    RepositoryRemoved { id: RepositoryIdentifier },
    /// File trees for repositories were updated.
    FileTreeUpdated { ids: Vec<RepositoryIdentifier> },
    /// A file tree entry was updated.
    FileTreeEntryUpdated {
        id: RepositoryIdentifier,
        /// Specifies whether this event contains a precise delta or requires a conservative
        /// refresh because the entry was replaced without one.
        update_type: MetadataUpdateType,
    },
    /// Updating a repository failed.
    UpdatingRepositoryFailed { id: RepositoryIdentifier },
    /// An incremental file tree update is ready to be sent to the remote
    /// client. Only emitted when the local model has
    /// `emit_incremental_updates` enabled.
    IncrementalUpdateReady { update: RepoMetadataUpdate },
}

/// Singleton wrapper that provides a unified API over local and remote
/// repository metadata models.
///
/// All consumers should interact with this type rather than accessing the
/// sub-models directly. The wrapper does **not** expose `.local()` or
/// `.remote()` accessors — encapsulation ensures consumers are decoupled
/// from the local/remote split.
pub struct RepoMetadataModel {
    local: ModelHandle<LocalRepoMetadataModel>,
    remote: ModelHandle<RemoteRepoMetadataModel>,
}

impl RepoMetadataModel {
    /// Creates a new `RepoMetadataModel`, instantiating both sub-models and
    /// subscribing to their events for forwarding.
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        let local = ctx.add_model(LocalRepoMetadataModel::new);
        let remote = ctx.add_model(RemoteRepoMetadataModel::new);

        ctx.subscribe_to_model(&local, Self::forward_local_event);
        ctx.subscribe_to_model(&remote, Self::forward_remote_event);

        Self { local, remote }
    }

    /// Creates a new `RepoMetadataModel` with incremental update emission
    /// enabled on the local sub-model. Used by the remote server.
    pub fn new_with_incremental_updates(ctx: &mut ModelContext<Self>) -> Self {
        let local = ctx.add_model(|ctx| {
            let mut model = LocalRepoMetadataModel::new(ctx);
            model.set_emit_incremental_updates(true);
            model
        });
        let remote = ctx.add_model(RemoteRepoMetadataModel::new);

        ctx.subscribe_to_model(&local, Self::forward_local_event);
        ctx.subscribe_to_model(&remote, Self::forward_remote_event);

        Self { local, remote }
    }

    // ── Event forwarding ─────────────────────────────────────────────

    fn forward_local_event(
        &mut self,
        event: &RepositoryMetadataEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        let unified = match event {
            RepositoryMetadataEvent::RepositoryUpdated { path } => {
                RepoMetadataEvent::RepositoryUpdated {
                    id: RepositoryIdentifier::local(path.clone()),
                }
            }
            RepositoryMetadataEvent::RepositoryRemoved { path } => {
                RepoMetadataEvent::RepositoryRemoved {
                    id: RepositoryIdentifier::local(path.clone()),
                }
            }
            RepositoryMetadataEvent::FileTreeUpdated { paths } => {
                RepoMetadataEvent::FileTreeUpdated {
                    ids: paths
                        .iter()
                        .map(|p| RepositoryIdentifier::local(p.clone()))
                        .collect(),
                }
            }
            RepositoryMetadataEvent::FileTreeEntryUpdated { path, update_type } => {
                RepoMetadataEvent::FileTreeEntryUpdated {
                    id: RepositoryIdentifier::local(path.clone()),
                    update_type: update_type.clone(),
                }
            }
            RepositoryMetadataEvent::UpdatingRepositoryFailed { path } => {
                RepoMetadataEvent::UpdatingRepositoryFailed {
                    id: RepositoryIdentifier::local(path.clone()),
                }
            }
            RepositoryMetadataEvent::IncrementalUpdateReady { update } => {
                RepoMetadataEvent::IncrementalUpdateReady {
                    update: update.clone(),
                }
            }
        };
        ctx.emit(unified);
    }

    fn forward_remote_event(
        &mut self,
        event: &RemoteRepositoryMetadataEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        let unified = match event {
            RemoteRepositoryMetadataEvent::RepositoryUpdated { id } => {
                RepoMetadataEvent::RepositoryUpdated {
                    id: RepositoryIdentifier::Remote(id.clone()),
                }
            }
            RemoteRepositoryMetadataEvent::RepositoryRemoved { id } => {
                RepoMetadataEvent::RepositoryRemoved {
                    id: RepositoryIdentifier::Remote(id.clone()),
                }
            }
            RemoteRepositoryMetadataEvent::FileTreeUpdated { ids } => {
                RepoMetadataEvent::FileTreeUpdated {
                    ids: ids
                        .iter()
                        .cloned()
                        .map(RepositoryIdentifier::Remote)
                        .collect(),
                }
            }
            RemoteRepositoryMetadataEvent::FileTreeEntryUpdated { id, update_type } => {
                RepoMetadataEvent::FileTreeEntryUpdated {
                    id: RepositoryIdentifier::Remote(id.clone()),
                    update_type: update_type.clone(),
                }
            }
        };
        ctx.emit(unified);
    }

    // ── Unified query API ────────────────────────────────────────────

    /// Returns the [`FileTreeState`] for a repository identified by `id`.
    pub fn get_repository<'a>(
        &self,
        id: &RepositoryIdentifier,
        ctx: &'a AppContext,
    ) -> Option<&'a FileTreeState> {
        match id {
            RepositoryIdentifier::Local(path) => self.local.as_ref(ctx).get_repository(path),
            RepositoryIdentifier::Remote(remote_id) => {
                self.remote.as_ref(ctx).get_repository(remote_id)
            }
        }
    }

    /// Returns authoritative repository contents for a typed query.
    ///
    /// A fully loaded canonical tree is used as an I/O-free fast path. Otherwise, local queries
    /// scan the repository with a work budget and remote queries are delegated through an
    /// app-provided query provider. Match-only authoritative results never modify canonical tree
    /// children.
    pub fn get_repo_contents(
        &self,
        id: RepositoryIdentifier,
        query: RepoMetadataQuery,
        budget: RepoContentsQueryBudget,
        remote_query_provider: Option<RemoteMetadataQueryProvider>,
        ctx: &AppContext,
    ) -> BoxFuture<'static, Result<Vec<OwnedRepoContent>, RepoContentsQueryError>> {
        let loaded_contents = match self.loaded_query_contents(&id, &query, ctx) {
            Ok(contents) => contents,
            Err(error) => return future::ready(Err(error)).boxed(),
        };
        if self
            .get_repository(&id, ctx)
            .is_some_and(|state| tree_is_fully_loaded(&state.entry, state.entry.root_directory()))
        {
            return future::ready(Ok(loaded_contents)).boxed();
        }
        match id {
            RepositoryIdentifier::Local(repo_root) => {
                async move { query_local_repo_contents(&repo_root, &query, budget) }.boxed()
            }
            RepositoryIdentifier::Remote(remote_id) => match remote_query_provider {
                Some(provider) => provider(remote_id, query, budget),
                None => future::ready(Err(RepoContentsQueryError::RemoteQueryUnavailable(
                    remote_id,
                )))
                .boxed(),
            },
        }
    }

    fn loaded_query_contents(
        &self,
        id: &RepositoryIdentifier,
        query: &RepoMetadataQuery,
        ctx: &AppContext,
    ) -> Result<Vec<OwnedRepoContent>, RepoContentsQueryError> {
        match self.repository_state(id, ctx) {
            Some(IndexedRepoState::Indexed(_)) => {}
            Some(IndexedRepoState::Pending(_)) => {
                return Err(RepoContentsQueryError::RepositoryPending(id.clone()));
            }
            Some(IndexedRepoState::Failed(error)) => {
                return Err(RepoContentsQueryError::RepositoryIndexingFailed {
                    id: id.clone(),
                    message: error.to_string(),
                });
            }
            None => return Err(RepoContentsQueryError::RepositoryNotFound(id.clone())),
        }
        Ok(self
            .get_loaded_repo_contents(id, get_loaded_query_args(query.clone()), ctx)
            .map_err(|error| RepoContentsQueryError::QueryFailed {
                message: error.to_string(),
            })?
            .iter()
            .map(RepoContent::to_owned)
            .collect())
    }

    /// Returns whether the given repository is indexed.
    pub fn has_repository(&self, id: &RepositoryIdentifier, ctx: &AppContext) -> bool {
        match id {
            RepositoryIdentifier::Local(path) => self.local.as_ref(ctx).has_repository(path),
            RepositoryIdentifier::Remote(remote_id) => {
                self.remote.as_ref(ctx).has_repository(remote_id)
            }
        }
    }

    /// Returns the current [`IndexedRepoState`] for a repository.
    pub fn repository_state<'a>(
        &self,
        id: &RepositoryIdentifier,
        ctx: &'a AppContext,
    ) -> Option<&'a IndexedRepoState> {
        match id {
            RepositoryIdentifier::Local(path) => self.local.as_ref(ctx).repository_state(path),
            RepositoryIdentifier::Remote(remote_id) => {
                self.remote.as_ref(ctx).repository_state(remote_id)
            }
        }
    }

    /// Returns a future that resolves once repository indexing has completed at least once.
    ///
    /// Callers should inspect [`Self::repository_state`] after awaiting this future to see whether
    /// indexing succeeded or failed.
    pub fn repository_indexed(
        &self,
        id: &RepositoryIdentifier,
        ctx: &mut ModelContext<Self>,
    ) -> futures::future::BoxFuture<'static, ()> {
        match id {
            RepositoryIdentifier::Local(path) => {
                let path = path.clone();
                self.local
                    .update(ctx, |local, _| local.repository_indexed(&path))
            }
            RepositoryIdentifier::Remote(remote_id) => {
                let remote_id = remote_id.clone();
                self.remote
                    .update(ctx, |remote, _| remote.repository_indexed(&remote_id))
            }
        }
    }

    /// Returns currently materialized repository contents without loading shallow directories.
    ///
    /// Consumers such as file search use this API intentionally so their existing latency and
    /// breadth remain unchanged. Features that require complete discovery should use
    /// [`Self::get_repo_contents`] instead.
    ///
    /// Returns an error if the number of materialized results exceeds MAX_REPO_CONTENTS_RESULTS.
    pub fn get_loaded_repo_contents<'a>(
        &self,
        id: &RepositoryIdentifier,
        args: GetContentsArgs,
        ctx: &'a AppContext,
    ) -> Result<Vec<RepoContent<'a>>, RepoMetadataError> {
        match id {
            RepositoryIdentifier::Local(path) => {
                self.local.as_ref(ctx).get_repo_contents(path, args)
            }
            RepositoryIdentifier::Remote(remote_id) => {
                self.remote.as_ref(ctx).get_repo_contents(remote_id, args)
            }
        }
    }

    /// Finds the repository root that contains the given local path.
    #[cfg(feature = "local_fs")]
    pub fn find_repository_for_path(
        &self,
        path: &Path,
        ctx: &AppContext,
    ) -> Option<StandardizedPath> {
        self.local.as_ref(ctx).find_repository_for_path(path)
    }

    // ── Local-specific operations ────────────────────────────────────
    // These delegate to the local sub-model. Remote equivalents will be
    // added once the remote client ↔ server sync layer is in place.

    /// Indexes a local repository from the given repository handle.
    #[cfg(feature = "local_fs")]
    pub fn index_directory(
        &self,
        repository: ModelHandle<crate::repository::Repository>,
        ctx: &mut ModelContext<Self>,
    ) -> Result<(), RepoMetadataError> {
        self.local
            .update(ctx, |local, ctx| local.index_directory(repository, ctx))
    }

    /// Lazily indexes a local standalone path with only the first level of children.
    #[cfg(feature = "local_fs")]
    pub fn index_lazy_loaded_path(
        &self,
        path: &StandardizedPath,
        ctx: &mut ModelContext<Self>,
    ) -> Result<(), RepoMetadataError> {
        let path = path.clone();
        self.local
            .update(ctx, |local, ctx| local.index_lazy_loaded_path(&path, ctx))
    }

    /// Loads a specific directory inside an already-tracked local tree.
    #[cfg(feature = "local_fs")]
    pub fn load_directory(
        &self,
        repo_root: &StandardizedPath,
        dir_path: &StandardizedPath,
        ctx: &mut ModelContext<Self>,
    ) -> Result<(), RepoMetadataError> {
        let repo_root = repo_root.clone();
        let dir_path = dir_path.clone();
        self.local.update(ctx, |local, ctx| {
            local.load_directory(&repo_root, &dir_path, ctx)
        })
    }

    /// Registers component-sequence paths that should be loaded even when ignored.
    ///
    /// This delegates to the local model because ignored-path matching happens
    /// while building local file trees. Remote repositories receive the resulting
    /// file-tree metadata over the existing remote sync protocol.
    pub fn register_ignored_path_interests(
        &self,
        interests: impl IntoIterator<Item = std::path::PathBuf>,
        ctx: &mut ModelContext<Self>,
    ) {
        let interests: Vec<_> = interests.into_iter().collect();
        self.local.update(ctx, |local, _| {
            local.register_ignored_path_interests(interests);
        });
    }

    /// Removes a lazily-loaded local standalone path from tracking.
    #[cfg(feature = "local_fs")]
    pub fn remove_lazy_loaded_path(&self, path: &StandardizedPath, ctx: &mut ModelContext<Self>) {
        let path = path.clone();
        self.local
            .update(ctx, |local, ctx| local.remove_lazy_loaded_path(&path, ctx));
    }

    // ── Remote-specific operations ─────────────────────────────────
    // These delegate to the remote sub-model and are called by the
    // RemoteServerManager event subscription in the app layer.

    /// Inserts or replaces a remote repository from a snapshot push event.
    pub fn insert_remote_snapshot(
        &self,
        host_id: HostId,
        update: &RepoMetadataUpdate,
        ctx: &mut ModelContext<Self>,
    ) {
        self.remote.update(ctx, |remote, ctx| {
            remote.insert_from_snapshot(host_id, update, ctx);
        });
    }

    /// Applies an incremental remote repo metadata update.
    pub fn apply_remote_incremental_update(
        &self,
        host_id: &HostId,
        update: &RepoMetadataUpdate,
        ctx: &mut ModelContext<Self>,
    ) {
        let host_id = host_id.clone();
        self.remote.update(ctx, |remote, ctx| {
            remote.apply_incremental_update(&host_id, update, ctx);
        });
    }

    /// Applies an authoritative remote directory-load response.
    ///
    /// Unlike incremental updates, a directory-load response replaces the
    /// complete immediate-child listing for each loaded directory and marks
    /// it `loaded == true`.
    pub fn apply_remote_loaded_directory_update(
        &self,
        host_id: &HostId,
        update: &RepoMetadataUpdate,
        ctx: &mut ModelContext<Self>,
    ) {
        let host_id = host_id.clone();
        self.remote.update(ctx, |remote, ctx| {
            remote.apply_loaded_directory_update(&host_id, update, ctx);
        });
    }

    /// Removes all remote repositories for the given host (e.g. on disconnect).
    pub fn remove_remote_repositories_for_host(
        &self,
        host_id: &HostId,
        ctx: &mut ModelContext<Self>,
    ) {
        let host_id = host_id.clone();
        self.remote.update(ctx, |remote, ctx| {
            remote.remove_repositories_for_host(&host_id, ctx);
        });
    }

    /// Removes a repository (local or remote) from tracking.
    pub fn remove_repository(
        &self,
        id: &RepositoryIdentifier,
        ctx: &mut ModelContext<Self>,
    ) -> Result<(), RepoMetadataError> {
        match id {
            RepositoryIdentifier::Local(path) => {
                let path = path.clone();
                self.local
                    .update(ctx, |local, ctx| local.remove_repository(&path, ctx))
            }
            RepositoryIdentifier::Remote(remote_id) => {
                let remote_id = remote_id.clone();
                self.remote
                    .update(ctx, |remote, ctx| remote.remove_repository(&remote_id, ctx));
                Ok(())
            }
        }
    }

    /// Returns all tracked remote repository identifiers.
    pub fn remote_repository_ids<'a>(
        &self,
        ctx: &'a AppContext,
    ) -> impl Iterator<Item = &'a RemoteRepositoryIdentifier> {
        self.remote.as_ref(ctx).remote_repository_ids()
    }

    /// Returns whether the given local path is tracked as a lazily-loaded standalone path.
    pub fn is_lazy_loaded_path(&self, path: &StandardizedPath, ctx: &AppContext) -> bool {
        self.local.as_ref(ctx).is_lazy_loaded_path(path)
    }
}

impl warpui_core::Entity for RepoMetadataModel {
    type Event = RepoMetadataEvent;
}

impl SingletonEntity for RepoMetadataModel {}

fn tree_is_fully_loaded(entry: &FileTreeEntry, current_path: &StandardizedPath) -> bool {
    let Some(FileTreeEntryState::Directory(directory)) = entry.get(current_path) else {
        return true;
    };
    if !directory.loaded {
        return false;
    }
    entry
        .child_paths(current_path)
        .all(|child| tree_is_fully_loaded(entry, child))
}

fn get_loaded_query_args(query: RepoMetadataQuery) -> GetContentsArgs {
    let include_folders = matches!(query, RepoMetadataQuery::ProjectSkillFiles { .. });
    GetContentsArgs {
        include_folders,
        ..GetContentsArgs::default()
    }
    .include_ignored()
    .with_filter(move |content| repo_content_matches_query(content, &query))
}

fn repo_content_matches_query(content: &RepoContent<'_>, query: &RepoMetadataQuery) -> bool {
    match content {
        RepoContent::File(file) => query_matches_path(&file.path, false, query),
        RepoContent::Directory(directory) => query_matches_path(&directory.path, true, query),
    }
}

fn query_matches_path(
    path: &StandardizedPath,
    is_directory: bool,
    query: &RepoMetadataQuery,
) -> bool {
    match query {
        RepoMetadataQuery::ProjectSkillFiles { provider_paths } => {
            if is_directory {
                return provider_paths
                    .iter()
                    .any(|provider_path| path_has_component_suffix(path, provider_path));
            }
            path.file_name() == Some("SKILL.md")
                && path
                    .parent()
                    .and_then(|skill_directory| skill_directory.parent())
                    .is_some_and(|skills_root| {
                        provider_paths.iter().any(|provider_path| {
                            path_has_component_suffix(&skills_root, provider_path)
                        })
                    })
        }
        RepoMetadataQuery::FilesNamed { names } => {
            !is_directory
                && path
                    .file_name()
                    .is_some_and(|file_name| names.iter().any(|name| name == file_name))
        }
    }
}

fn path_has_component_suffix(path: &StandardizedPath, suffix: &str) -> bool {
    let mut candidate = Some(path.clone());
    for expected in suffix
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .rev()
    {
        let Some(current) = candidate else {
            return false;
        };
        if current.file_name() != Some(expected) {
            return false;
        }
        candidate = current.parent();
    }
    true
}

#[cfg(feature = "local_fs")]
/// Runs bounded authoritative local discovery without mutating any loaded metadata tree.
pub fn query_local_repo_contents(
    repo_root: &StandardizedPath,
    query: &RepoMetadataQuery,
    budget: RepoContentsQueryBudget,
) -> Result<Vec<OwnedRepoContent>, RepoContentsQueryError> {
    let local_root =
        repo_root
            .to_local_path()
            .ok_or_else(|| RepoContentsQueryError::QueryFailed {
                message: format!("Local repository path has incompatible encoding: {repo_root}"),
            })?;
    let mut matches = Vec::new();
    let mut entries = WalkDir::new(local_root).follow_links(false).into_iter();
    let mut entries_scanned = 0usize;
    while let Some(entry) = entries.next() {
        let entry = entry.map_err(|error| RepoContentsQueryError::QueryFailed {
            message: error.to_string(),
        })?;
        if entry.file_type().is_dir() && entry.file_name() == ".git" {
            entries.skip_current_dir();
            continue;
        }
        if entries_scanned >= budget.max_entries_scanned {
            return Err(RepoContentsQueryError::QueryBudgetExceeded {
                limit: budget.max_entries_scanned,
            });
        }
        entries_scanned += 1;
        let path = StandardizedPath::try_from_local(entry.path()).map_err(|error| {
            RepoContentsQueryError::QueryFailed {
                message: error.to_string(),
            }
        })?;
        if entry.file_type().is_dir() && query_matches_path(&path, true, query) {
            matches.push(OwnedRepoContent::Directory {
                path,
                ignored: false,
                loaded: false,
            });
        } else if entry.file_type().is_file() && query_matches_path(&path, false, query) {
            matches.push(OwnedRepoContent::File {
                extension: path.extension().map(ToOwned::to_owned),
                path,
                ignored: false,
            });
        }
    }
    Ok(matches)
}

#[cfg(not(feature = "local_fs"))]
pub fn query_local_repo_contents(
    _repo_root: &StandardizedPath,
    _query: &RepoMetadataQuery,
    _budget: RepoContentsQueryBudget,
) -> Result<Vec<OwnedRepoContent>, RepoContentsQueryError> {
    Err(RepoContentsQueryError::QueryFailed {
        message: "Local filesystem metadata queries are unavailable".to_string(),
    })
}

#[cfg(any(test, feature = "test-util"))]
impl RepoMetadataModel {
    /// Inserts repository state directly into the local sub-model for testing.
    pub fn insert_test_state(
        &self,
        repo_path: StandardizedPath,
        state: FileTreeState,
        ctx: &mut ModelContext<Self>,
    ) {
        self.local.update(ctx, |local, _ctx| {
            local.insert_test_state(repo_path, state);
        });
    }
}
