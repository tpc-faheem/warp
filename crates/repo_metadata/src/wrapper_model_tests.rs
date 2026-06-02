use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::future::FutureExt as _;
use tempfile::TempDir;
use warp_core::HostId;
use warp_util::standardized_path::StandardizedPath;
use warpui::App;

use super::{
    RemoteMetadataQueryProvider, RepoContentsQueryBudget, RepoContentsQueryError,
    RepoMetadataModel, RepoMetadataQuery,
};
use crate::entry::{DirectoryEntry, Entry, FileMetadata};
use crate::file_tree_store::{FileTreeEntryState, FileTreeState};
use crate::local_model::OwnedRepoContent;
use crate::repositories::DetectedRepositories;
use crate::repository_identifier::{RemoteRepositoryIdentifier, RepositoryIdentifier};

fn directory(path: StandardizedPath, loaded: bool, children: Vec<Entry>) -> Entry {
    Entry::Directory(DirectoryEntry {
        path,
        children,
        ignored: false,
        loaded,
    })
}

fn file(path: StandardizedPath) -> Entry {
    Entry::File(FileMetadata::new(path.to_local_path_lossy(), false))
}

#[test]
#[cfg(feature = "local_fs")]
fn authoritative_local_query_discovers_matches_without_materializing_tree() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());
        let model = app.add_model(RepoMetadataModel::new);
        let temp_dir = TempDir::new().unwrap();
        let repo = dunce::canonicalize(temp_dir.path()).unwrap();
        let repo_path = StandardizedPath::try_from_local(&repo).unwrap();
        let source_directory = StandardizedPath::try_from_local(&repo.join("src")).unwrap();
        let match_path = StandardizedPath::try_from_local(&repo.join("src/deep/WARP.md")).unwrap();
        fs::create_dir_all(repo.join("src/deep")).unwrap();
        fs::write(repo.join("src/deep/WARP.md"), "rules").unwrap();

        let state = FileTreeState::new(
            directory(
                repo_path.clone(),
                true,
                vec![directory(source_directory.clone(), false, Vec::new())],
            ),
            Vec::new(),
            None,
        );
        let id = RepositoryIdentifier::local(repo_path.clone());
        model.update(&mut app, |model, ctx| {
            model.insert_test_state(repo_path, state, ctx);
        });

        let query = model.update(&mut app, |model, ctx| {
            model.get_repo_contents(
                id.clone(),
                RepoMetadataQuery::FilesNamed {
                    names: vec!["WARP.md".to_string()],
                },
                RepoContentsQueryBudget::default(),
                None,
                ctx,
            )
        });
        let contents = query.await.unwrap();

        assert!(contents.iter().any(
            |content| matches!(content, OwnedRepoContent::File { path, .. } if path == &match_path)
        ));
        model.read(&app, |model, ctx| {
            let state = model.get_repository(&id, ctx).unwrap();
            assert!(matches!(
                state.entry.get(&source_directory),
                Some(FileTreeEntryState::Directory(directory)) if !directory.loaded
            ));
            assert!(!state.entry.contains(&match_path));
        });
    });
}

#[test]
#[cfg(all(feature = "local_fs", unix))]
fn authoritative_project_skill_query_discovers_symlink_without_materializing_tree() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());
        let model = app.add_model(RepoMetadataModel::new);
        let repo_dir = TempDir::new().unwrap();
        let target_dir = TempDir::new().unwrap();
        let repo = dunce::canonicalize(repo_dir.path()).unwrap();
        let repo_path = StandardizedPath::try_from_local(&repo).unwrap();
        let agents_directory = StandardizedPath::try_from_local(&repo.join(".agents")).unwrap();
        let provider_directory =
            StandardizedPath::try_from_local(&repo.join(".agents/skills")).unwrap();
        let symlink_skill_path =
            StandardizedPath::try_from_local(&repo.join(".agents/skills/linked/SKILL.md")).unwrap();
        fs::create_dir_all(repo.join(".agents/skills")).unwrap();
        fs::create_dir_all(target_dir.path().join("linked")).unwrap();
        fs::write(target_dir.path().join("linked/SKILL.md"), "linked skill").unwrap();
        std::os::unix::fs::symlink(
            target_dir.path().join("linked"),
            repo.join(".agents/skills/linked"),
        )
        .unwrap();

        let state = FileTreeState::new(
            directory(
                repo_path.clone(),
                true,
                vec![directory(
                    agents_directory,
                    true,
                    vec![directory(provider_directory, true, Vec::new())],
                )],
            ),
            Vec::new(),
            None,
        );
        let id = RepositoryIdentifier::local(repo_path.clone());
        model.update(&mut app, |model, ctx| {
            model.insert_test_state(repo_path, state, ctx);
        });

        let query = model.update(&mut app, |model, ctx| {
            model.get_repo_contents(
                id.clone(),
                RepoMetadataQuery::ProjectSkillFiles {
                    provider_paths: vec![".agents/skills".to_string()],
                },
                RepoContentsQueryBudget::default(),
                None,
                ctx,
            )
        });
        let contents = query.await.unwrap();

        assert!(contents.iter().any(
            |content| matches!(content, OwnedRepoContent::File { path, .. } if path == &symlink_skill_path)
        ));
        model.read(&app, |model, ctx| {
            let state = model.get_repository(&id, ctx).unwrap();
            assert!(!state.entry.contains(&symlink_skill_path));
        });
    });
}

#[test]
#[cfg(feature = "local_fs")]
fn authoritative_local_query_enforces_scan_budget_without_materializing_tree() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());
        let model = app.add_model(RepoMetadataModel::new);
        let temp_dir = TempDir::new().unwrap();
        let repo = dunce::canonicalize(temp_dir.path()).unwrap();
        let repo_path = StandardizedPath::try_from_local(&repo).unwrap();
        let source_directory = StandardizedPath::try_from_local(&repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();

        let state = FileTreeState::new(
            directory(
                repo_path.clone(),
                true,
                vec![directory(source_directory.clone(), false, Vec::new())],
            ),
            Vec::new(),
            None,
        );
        let id = RepositoryIdentifier::local(repo_path.clone());
        model.update(&mut app, |model, ctx| {
            model.insert_test_state(repo_path, state, ctx);
        });

        let query = model.update(&mut app, |model, ctx| {
            model.get_repo_contents(
                id.clone(),
                RepoMetadataQuery::FilesNamed {
                    names: vec!["WARP.md".to_string()],
                },
                RepoContentsQueryBudget {
                    max_entries_scanned: 0,
                },
                None,
                ctx,
            )
        });
        assert!(matches!(
            query.await,
            Err(RepoContentsQueryError::QueryBudgetExceeded { limit: 0 })
        ));
        model.read(&app, |model, ctx| {
            let state = model.get_repository(&id, ctx).unwrap();
            assert!(matches!(
                state.entry.get(&source_directory),
                Some(FileTreeEntryState::Directory(directory)) if !directory.loaded
            ));
        });
    });
}

#[test]
fn authoritative_remote_query_returns_matches_without_materializing_tree() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());
        let model = app.add_model(RepoMetadataModel::new);
        let host_id = HostId::new("host".to_string());
        let repo_path = StandardizedPath::try_new("/repo").unwrap();
        let source_directory = StandardizedPath::try_new("/repo/src").unwrap();
        let stale_path = StandardizedPath::try_new("/repo/src/stale.md").unwrap();
        let match_path = StandardizedPath::try_new("/repo/src/deep/WARP.md").unwrap();
        let remote_id = RemoteRepositoryIdentifier::new(host_id, repo_path.clone());
        let id = RepositoryIdentifier::Remote(remote_id.clone());
        let state = FileTreeState::new(
            directory(
                repo_path.clone(),
                true,
                vec![directory(
                    source_directory.clone(),
                    false,
                    vec![file(stale_path.clone())],
                )],
            ),
            Vec::new(),
            None,
        );
        model.update(&mut app, |model, ctx| {
            model.remote.update(ctx, |remote, _| {
                remote.insert_test_state(remote_id.clone(), state);
            });
        });

        let expected_remote_id = remote_id.clone();
        let match_path_for_provider = match_path.clone();
        let provider: RemoteMetadataQueryProvider = Box::new(move |requested_id, query, _| {
            assert_eq!(requested_id, expected_remote_id);
            assert_eq!(
                query,
                RepoMetadataQuery::FilesNamed {
                    names: vec!["WARP.md".to_string()]
                }
            );
            let match_path = match_path_for_provider.clone();
            async move {
                Ok(vec![OwnedRepoContent::File {
                    path: match_path,
                    extension: Some("md".to_string()),
                    ignored: false,
                }])
            }
            .boxed()
        });
        let query = model.update(&mut app, |model, ctx| {
            model.get_repo_contents(
                id.clone(),
                RepoMetadataQuery::FilesNamed {
                    names: vec!["WARP.md".to_string()],
                },
                RepoContentsQueryBudget::default(),
                Some(provider),
                ctx,
            )
        });
        let contents = query.await.unwrap();

        assert!(contents.iter().any(
            |content| matches!(content, OwnedRepoContent::File { path, .. } if path == &match_path)
        ));
        model.read(&app, |model, ctx| {
            let state = model.get_repository(&id, ctx).unwrap();
            assert!(matches!(
                state.entry.get(&source_directory),
                Some(FileTreeEntryState::Directory(directory)) if !directory.loaded
            ));
            assert!(state.entry.contains(&stale_path));
            assert!(!state.entry.contains(&match_path));
        });
    });
}

#[test]
fn fully_loaded_remote_query_uses_loaded_tree_without_remote_provider() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());
        let model = app.add_model(RepoMetadataModel::new);
        let host_id = HostId::new("host".to_string());
        let repo_path = StandardizedPath::try_new("/repo").unwrap();
        let match_path = StandardizedPath::try_new("/repo/WARP.md").unwrap();
        let remote_id = RemoteRepositoryIdentifier::new(host_id, repo_path.clone());
        let id = RepositoryIdentifier::Remote(remote_id.clone());
        let state = FileTreeState::new(
            directory(repo_path, true, vec![file(match_path.clone())]),
            Vec::new(),
            None,
        );
        model.update(&mut app, |model, ctx| {
            model.remote.update(ctx, |remote, _| {
                remote.insert_test_state(remote_id, state);
            });
        });

        let called = Arc::new(AtomicBool::new(false));
        let called_for_provider = called.clone();
        let provider: RemoteMetadataQueryProvider = Box::new(move |_, _, _| {
            called_for_provider.store(true, Ordering::SeqCst);
            async move { Ok(Vec::new()) }.boxed()
        });
        let query = model.update(&mut app, |model, ctx| {
            model.get_repo_contents(
                id,
                RepoMetadataQuery::FilesNamed {
                    names: vec!["WARP.md".to_string()],
                },
                RepoContentsQueryBudget::default(),
                Some(provider),
                ctx,
            )
        });
        let contents = query.await.unwrap();

        assert!(!called.load(Ordering::SeqCst));
        assert!(contents.iter().any(
            |content| matches!(content, OwnedRepoContent::File { path, .. } if path == &match_path)
        ));
    });
}

#[test]
fn fully_loaded_remote_project_skill_query_uses_remote_provider() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());
        let model = app.add_model(RepoMetadataModel::new);
        let host_id = HostId::new("host".to_string());
        let repo_path = StandardizedPath::try_new("/repo").unwrap();
        let symlink_skill_path =
            StandardizedPath::try_new("/repo/.agents/skills/linked/SKILL.md").unwrap();
        let remote_id = RemoteRepositoryIdentifier::new(host_id, repo_path.clone());
        let id = RepositoryIdentifier::Remote(remote_id.clone());
        let state = FileTreeState::new(directory(repo_path, true, Vec::new()), Vec::new(), None);
        model.update(&mut app, |model, ctx| {
            model.remote.update(ctx, |remote, _| {
                remote.insert_test_state(remote_id.clone(), state);
            });
        });

        let called = Arc::new(AtomicBool::new(false));
        let called_for_provider = called.clone();
        let expected_remote_id = remote_id.clone();
        let expected_skill_path = symlink_skill_path.clone();
        let provider: RemoteMetadataQueryProvider = Box::new(move |requested_id, query, _| {
            assert_eq!(requested_id, expected_remote_id);
            assert_eq!(
                query,
                RepoMetadataQuery::ProjectSkillFiles {
                    provider_paths: vec![".agents/skills".to_string()]
                }
            );
            called_for_provider.store(true, Ordering::SeqCst);
            let path = expected_skill_path.clone();
            async move {
                Ok(vec![OwnedRepoContent::File {
                    extension: Some("md".to_string()),
                    path,
                    ignored: false,
                }])
            }
            .boxed()
        });
        let query = model.update(&mut app, |model, ctx| {
            model.get_repo_contents(
                id,
                RepoMetadataQuery::ProjectSkillFiles {
                    provider_paths: vec![".agents/skills".to_string()],
                },
                RepoContentsQueryBudget::default(),
                Some(provider),
                ctx,
            )
        });
        let contents = query.await.unwrap();

        assert!(called.load(Ordering::SeqCst));
        assert!(contents.iter().any(
            |content| matches!(content, OwnedRepoContent::File { path, .. } if path == &symlink_skill_path)
        ));
    });
}

#[test]
fn fully_loaded_remote_query_propagates_loaded_result_limit_failure() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());
        let model = app.add_model(RepoMetadataModel::new);
        let host_id = HostId::new("host".to_string());
        let repo_path = StandardizedPath::try_new("/repo").unwrap();
        let remote_id = RemoteRepositoryIdentifier::new(host_id, repo_path.clone());
        let id = RepositoryIdentifier::Remote(remote_id.clone());
        let children = (0..=100)
            .map(|index| {
                let path = format!("/repo/path-{index}/WARP.md");
                file(StandardizedPath::try_new(&path).unwrap())
            })
            .collect();
        let state = FileTreeState::new(directory(repo_path, true, children), Vec::new(), None);
        model.update(&mut app, |model, ctx| {
            model.remote.update(ctx, |remote, _| {
                remote.insert_test_state(remote_id, state);
            });
        });

        let query = model.update(&mut app, |model, ctx| {
            model.get_repo_contents(
                id,
                RepoMetadataQuery::FilesNamed {
                    names: vec!["WARP.md".to_string()],
                },
                RepoContentsQueryBudget::default(),
                None,
                ctx,
            )
        });

        assert!(matches!(
            query.await,
            Err(RepoContentsQueryError::QueryFailed { message })
                if message == "Result size exceeded maximum limit of 100"
        ));
    });
}
