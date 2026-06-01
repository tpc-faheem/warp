use std::sync::Arc;

use repo_metadata::file_tree_store::FileTreeEntry;
use repo_metadata::{
    OwnedRepoContent, RepoContentsQueryBudget, RepoContentsQueryError, RepoMetadataQuery,
};
use warp_util::standardized_path::StandardizedPath;

use super::{
    file_tree_children_to_proto_entries, proto_query_repo_metadata_response_to_contents,
    proto_to_repo_metadata_query, query_repo_metadata_budget_exceeded_response_to_proto,
    query_repo_metadata_response_to_proto, repo_metadata_query_to_proto,
};

#[test]
fn serializes_empty_loaded_directory_as_authoritative_replacement() {
    let directory = StandardizedPath::try_new("/repo/empty").unwrap();
    let tree = FileTreeEntry::new_for_directory(Arc::new(directory.clone()));

    let entries = file_tree_children_to_proto_entries(&tree, &directory);

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].parent_path_to_replace, directory.to_string());
    assert!(entries[0].subtree_metadata.is_empty());
}

#[test]
fn typed_query_round_trips_through_proto() {
    let query = RepoMetadataQuery::ProjectSkillFiles {
        provider_paths: vec![".agents/skills".to_string(), ".claude/skills".to_string()],
    };

    let proto = repo_metadata_query_to_proto(query.clone());

    assert_eq!(proto_to_repo_metadata_query(&proto), Some(query));
}

#[test]
fn authoritative_query_response_preserves_matches_and_budget_failure() {
    let budget = RepoContentsQueryBudget {
        max_entries_scanned: 7,
    };
    let matches = vec![OwnedRepoContent::File {
        path: StandardizedPath::try_new("/repo/.agents/skills/demo/SKILL.md").unwrap(),
        extension: Some("md".to_string()),
        ignored: false,
    }];

    let response = query_repo_metadata_response_to_proto(&matches, budget);
    assert_eq!(
        proto_query_repo_metadata_response_to_contents(&response).unwrap(),
        matches
    );

    let response = query_repo_metadata_budget_exceeded_response_to_proto(budget);
    assert!(matches!(
        proto_query_repo_metadata_response_to_contents(&response),
        Err(RepoContentsQueryError::QueryBudgetExceeded { limit: 7 })
    ));
}
