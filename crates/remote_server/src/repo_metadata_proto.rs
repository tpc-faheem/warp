//! Conversion between `repo_metadata` Rust types and proto-generated types.
//!
//! The Rust types in `repo_metadata::file_tree_update` were designed to mirror the
//! proto schema 1:1, so these conversions are straightforward field mappings.

use repo_metadata::file_tree_store::{FileTreeEntry, FileTreeEntryState};
use repo_metadata::file_tree_update::{
    DirectoryNodeMetadata, FileNodeMetadata, FileTreeEntryUpdate, RepoMetadataUpdate,
    RepoNodeMetadata,
};
use repo_metadata::{
    OwnedRepoContent, RepoContentsQueryBudget, RepoContentsQueryError, RepoMetadataQuery,
};
use warp_util::standardized_path::StandardizedPath;

use crate::proto;

// ── Rust → Proto ────────────────────────────────────────────

impl From<&RepoMetadataUpdate> for proto::RepoMetadataUpdatePush {
    fn from(update: &RepoMetadataUpdate) -> Self {
        Self {
            repo_path: update.repo_path.to_string(),
            remove_entries: update
                .remove_entries
                .iter()
                .map(|p| p.to_string())
                .collect(),
            update_entries: update
                .update_entries
                .iter()
                .map(proto::RepoMetadataEntryUpdate::from)
                .collect(),
        }
    }
}

impl From<&FileTreeEntryUpdate> for proto::RepoMetadataEntryUpdate {
    fn from(update: &FileTreeEntryUpdate) -> Self {
        Self {
            parent_path_to_replace: update.parent_path_to_replace.to_string(),
            subtree_metadata: update
                .subtree_metadata
                .iter()
                .map(proto::RepoNodeMetadata::from)
                .collect(),
        }
    }
}

impl From<&RepoNodeMetadata> for proto::RepoNodeMetadata {
    fn from(node: &RepoNodeMetadata) -> Self {
        let node_oneof = match node {
            RepoNodeMetadata::Directory(dir) => {
                proto::repo_node_metadata::Node::Directory(proto::DirectoryNodeMetadata {
                    path: dir.path.to_string(),
                    ignored: dir.ignored,
                    loaded: dir.loaded,
                })
            }
            RepoNodeMetadata::File(file) => {
                proto::repo_node_metadata::Node::File(proto::FileNodeMetadata {
                    path: file.path.to_string(),
                    extension: file.extension.clone(),
                    ignored: file.ignored,
                })
            }
        };
        Self {
            node: Some(node_oneof),
        }
    }
}

/// Serializes a full `FileTreeEntry`
/// messages suitable for a `RepoMetadataSnapshot`.
///
/// Walks the tree breadth-first from the root, producing one `RepoMetadataEntryUpdate`
/// per parent directory containing its immediate children as `RepoNodeMetadata`.
pub fn file_tree_entry_to_snapshot_proto(
    entry: &FileTreeEntry,
) -> Vec<proto::RepoMetadataEntryUpdate> {
    let mut result = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(entry.root_directory().clone());

    while let Some(current_path) = queue.pop_front() {
        let children: Vec<_> = entry.child_paths(&current_path).cloned().collect();
        if children.is_empty() {
            continue;
        }

        let mut subtree_metadata = Vec::with_capacity(children.len());
        for child_path in &children {
            match entry.get(child_path) {
                Some(FileTreeEntryState::Directory(dir)) => {
                    subtree_metadata.push(proto::RepoNodeMetadata {
                        node: Some(proto::repo_node_metadata::Node::Directory(
                            proto::DirectoryNodeMetadata {
                                path: dir.path.to_string(),
                                ignored: dir.ignored,
                                loaded: dir.loaded,
                            },
                        )),
                    });
                    // Enqueue for breadth-first traversal.
                    queue.push_back(child_path.clone());
                }
                Some(FileTreeEntryState::File(file)) => {
                    subtree_metadata.push(proto::RepoNodeMetadata {
                        node: Some(proto::repo_node_metadata::Node::File(
                            proto::FileNodeMetadata {
                                path: file.path.to_string(),
                                extension: file.extension.clone(),
                                ignored: file.ignored,
                            },
                        )),
                    });
                }
                None => {}
            }
        }

        if !subtree_metadata.is_empty() {
            result.push(proto::RepoMetadataEntryUpdate {
                parent_path_to_replace: current_path.to_string(),
                subtree_metadata,
            });
        }
    }

    result
}

// ── Proto → Rust ──────────────────────────────────────────────────

/// Converts a `RepoMetadataUpdatePush` proto message into a `RepoMetadataUpdate`.
pub fn proto_to_repo_metadata_update(
    push: &proto::RepoMetadataUpdatePush,
) -> Option<RepoMetadataUpdate> {
    let repo_path = StandardizedPath::try_new(&push.repo_path).ok()?;

    let remove_entries: Vec<StandardizedPath> = push
        .remove_entries
        .iter()
        .filter_map(|p| match StandardizedPath::try_new(p) {
            Ok(path) => Some(path),
            Err(e) => {
                log::warn!("Skipping invalid remove_entry path {p:?}: {e}");
                None
            }
        })
        .collect();

    let update_entries: Vec<FileTreeEntryUpdate> = push
        .update_entries
        .iter()
        .filter_map(proto_to_entry_update)
        .collect();

    Some(RepoMetadataUpdate {
        repo_path,
        remove_entries,
        update_entries,
    })
}

/// Converts a `RepoMetadataSnapshot` proto into a `RepoMetadataUpdate`
/// (with no removals) that can be applied to a `RemoteRepoMetadataModel`.
pub fn proto_snapshot_to_update(
    snapshot: &proto::RepoMetadataSnapshot,
) -> Option<RepoMetadataUpdate> {
    let repo_path = StandardizedPath::try_new(&snapshot.repo_path).ok()?;

    let update_entries: Vec<FileTreeEntryUpdate> = snapshot
        .entries
        .iter()
        .filter_map(proto_to_entry_update)
        .collect();

    Some(RepoMetadataUpdate {
        repo_path,
        remove_entries: Vec::new(),
        update_entries,
    })
}

fn proto_to_entry_update(
    proto_update: &proto::RepoMetadataEntryUpdate,
) -> Option<FileTreeEntryUpdate> {
    let parent_path = StandardizedPath::try_new(&proto_update.parent_path_to_replace).ok()?;

    let subtree_metadata: Vec<RepoNodeMetadata> = proto_update
        .subtree_metadata
        .iter()
        .filter_map(proto_to_repo_node_metadata)
        .collect();

    Some(FileTreeEntryUpdate {
        parent_path_to_replace: parent_path,
        subtree_metadata,
    })
}

/// Converts a `LoadRepoMetadataDirectoryResponse` proto into a `RepoMetadataUpdate`
/// (with no removals) that can be applied to a `RemoteRepoMetadataModel`.
pub fn proto_load_repo_metadata_directory_response_to_update(
    resp: &proto::LoadRepoMetadataDirectoryResponse,
) -> Option<RepoMetadataUpdate> {
    let repo_path = StandardizedPath::try_new(&resp.repo_path).ok()?;

    let update_entries: Vec<FileTreeEntryUpdate> = resp
        .entries
        .iter()
        .filter_map(proto_to_entry_update)
        .collect();

    Some(RepoMetadataUpdate {
        repo_path,
        remove_entries: Vec::new(),
        update_entries,
    })
}

/// Serializes the immediate children of a directory in a `FileTreeEntry` as
/// `RepoMetadataEntryUpdate` protos. Used to build a `LoadRepoMetadataDirectoryResponse`.
pub fn file_tree_children_to_proto_entries(
    entry: &FileTreeEntry,
    dir_path: &StandardizedPath,
) -> Vec<proto::RepoMetadataEntryUpdate> {
    let children: Vec<_> = entry.child_paths(dir_path).cloned().collect();

    let mut subtree_metadata = Vec::with_capacity(children.len());
    for child_path in &children {
        match entry.get(child_path) {
            Some(FileTreeEntryState::Directory(dir)) => {
                subtree_metadata.push(proto::RepoNodeMetadata {
                    node: Some(proto::repo_node_metadata::Node::Directory(
                        proto::DirectoryNodeMetadata {
                            path: dir.path.to_string(),
                            ignored: dir.ignored,
                            loaded: dir.loaded,
                        },
                    )),
                });
            }
            Some(FileTreeEntryState::File(file)) => {
                subtree_metadata.push(proto::RepoNodeMetadata {
                    node: Some(proto::repo_node_metadata::Node::File(
                        proto::FileNodeMetadata {
                            path: file.path.to_string(),
                            extension: file.extension.clone(),
                            ignored: file.ignored,
                        },
                    )),
                });
            }
            None => {}
        }
    }

    vec![proto::RepoMetadataEntryUpdate {
        parent_path_to_replace: dir_path.to_string(),
        subtree_metadata,
    }]
}

pub fn repo_metadata_query_to_proto(query: RepoMetadataQuery) -> proto::RepoMetadataQuery {
    let query = match query {
        RepoMetadataQuery::ProjectSkillFiles { provider_paths } => {
            proto::repo_metadata_query::Query::ProjectSkillFiles(proto::ProjectSkillFilesQuery {
                provider_paths,
            })
        }
        RepoMetadataQuery::FilesNamed { names } => {
            proto::repo_metadata_query::Query::FilesNamed(proto::FilesNamedQuery { names })
        }
    };

    proto::RepoMetadataQuery { query: Some(query) }
}

pub fn proto_to_repo_metadata_query(query: &proto::RepoMetadataQuery) -> Option<RepoMetadataQuery> {
    match query.query.as_ref()? {
        proto::repo_metadata_query::Query::ProjectSkillFiles(query) => {
            Some(RepoMetadataQuery::ProjectSkillFiles {
                provider_paths: query.provider_paths.clone(),
            })
        }
        proto::repo_metadata_query::Query::FilesNamed(query) => {
            Some(RepoMetadataQuery::FilesNamed {
                names: query.names.clone(),
            })
        }
    }
}

pub fn query_repo_metadata_response_to_proto(
    matches: &[OwnedRepoContent],
    budget: RepoContentsQueryBudget,
) -> proto::QueryRepoMetadataResponse {
    proto::QueryRepoMetadataResponse {
        matches: matches.iter().map(owned_repo_content_to_proto).collect(),
        status: proto::RepoMetadataQueryStatus::Complete as i32,
        max_entries_scanned: budget.max_entries_scanned as u64,
    }
}

pub fn query_repo_metadata_budget_exceeded_response_to_proto(
    budget: RepoContentsQueryBudget,
) -> proto::QueryRepoMetadataResponse {
    proto::QueryRepoMetadataResponse {
        matches: Vec::new(),
        status: proto::RepoMetadataQueryStatus::BudgetExceeded as i32,
        max_entries_scanned: budget.max_entries_scanned as u64,
    }
}

pub fn proto_query_repo_metadata_response_to_contents(
    response: &proto::QueryRepoMetadataResponse,
) -> Result<Vec<OwnedRepoContent>, RepoContentsQueryError> {
    let budget = response.max_entries_scanned as usize;
    match proto::RepoMetadataQueryStatus::try_from(response.status).ok() {
        Some(proto::RepoMetadataQueryStatus::Complete) => response
            .matches
            .iter()
            .map(proto_to_owned_repo_content)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| RepoContentsQueryError::QueryFailed {
                message: "remote metadata query returned an invalid match".to_string(),
            }),
        Some(proto::RepoMetadataQueryStatus::BudgetExceeded) => {
            Err(RepoContentsQueryError::QueryBudgetExceeded { limit: budget })
        }
        Some(proto::RepoMetadataQueryStatus::Unspecified) | None => {
            Err(RepoContentsQueryError::QueryFailed {
                message: "remote metadata query returned an unspecified status".to_string(),
            })
        }
    }
}

fn owned_repo_content_to_proto(content: &OwnedRepoContent) -> proto::RepoNodeMetadata {
    let node = match content {
        OwnedRepoContent::Directory {
            path,
            ignored,
            loaded,
        } => proto::repo_node_metadata::Node::Directory(proto::DirectoryNodeMetadata {
            path: path.to_string(),
            ignored: *ignored,
            loaded: *loaded,
        }),
        OwnedRepoContent::File {
            path,
            extension,
            ignored,
        } => proto::repo_node_metadata::Node::File(proto::FileNodeMetadata {
            path: path.to_string(),
            extension: extension.clone(),
            ignored: *ignored,
        }),
    };

    proto::RepoNodeMetadata { node: Some(node) }
}

fn proto_to_owned_repo_content(node: &proto::RepoNodeMetadata) -> Option<OwnedRepoContent> {
    match node.node.as_ref()? {
        proto::repo_node_metadata::Node::Directory(directory) => {
            Some(OwnedRepoContent::Directory {
                path: StandardizedPath::try_new(&directory.path).ok()?,
                ignored: directory.ignored,
                loaded: directory.loaded,
            })
        }
        proto::repo_node_metadata::Node::File(file) => Some(OwnedRepoContent::File {
            path: StandardizedPath::try_new(&file.path).ok()?,
            extension: file.extension.clone(),
            ignored: file.ignored,
        }),
    }
}

fn proto_to_repo_node_metadata(proto_node: &proto::RepoNodeMetadata) -> Option<RepoNodeMetadata> {
    match proto_node.node.as_ref()? {
        proto::repo_node_metadata::Node::Directory(dir) => {
            let path = StandardizedPath::try_new(&dir.path).ok()?;
            Some(RepoNodeMetadata::Directory(DirectoryNodeMetadata {
                path,
                ignored: dir.ignored,
                loaded: dir.loaded,
            }))
        }
        proto::repo_node_metadata::Node::File(file) => {
            let path = StandardizedPath::try_new(&file.path).ok()?;
            Some(RepoNodeMetadata::File(FileNodeMetadata {
                path,
                extension: file.extension.clone(),
                ignored: file.ignored,
            }))
        }
    }
}

#[cfg(test)]
#[path = "repo_metadata_proto_tests.rs"]
mod tests;
