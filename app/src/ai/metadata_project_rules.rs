use std::collections::HashMap;
use std::sync::Arc;

use ai::project_context::model::{ProjectContextModel, ProjectRule};
use futures::future::{BoxFuture, FutureExt as _};
use remote_server::proto::{
    file_context_proto, FileContextProto, ReadFileContextFile, ReadFileContextRequest,
};
use repo_metadata::{
    OwnedRepoContent, RemoteMetadataQueryProvider, RepoContentsQueryBudget, RepoContentsQueryError,
    RepoMetadataModel, RepoMetadataQuery, RepositoryIdentifier,
};
use warp_util::local_or_remote_path::LocalOrRemotePath;
use warp_util::remote_path::RemotePath;
use warpui::{AppContext, Entity, ModelContext, SingletonEntity};

use crate::remote_server::manager::RemoteServerManager;
use crate::remote_server::repo_metadata_proto::{
    proto_query_repo_metadata_response_to_contents, repo_metadata_query_to_proto,
};

pub(crate) struct MetadataProjectRulesModel {
    refresh_generations: HashMap<RepositoryIdentifier, u64>,
    next_refresh_generation: u64,
}
const PROJECT_RULE_MAX_ENTRIES_SCANNED: usize = 100_000;
type ProjectRuleContentsFuture =
    BoxFuture<'static, anyhow::Result<Vec<(LocalOrRemotePath, String)>>>;

impl MetadataProjectRulesModel {
    pub(crate) fn new(ctx: &mut ModelContext<Self>) -> Self {
        let repo_metadata = RepoMetadataModel::handle(ctx);
        ctx.subscribe_to_model(&repo_metadata, |me, event, ctx| {
            me.handle_repo_metadata_event(event, ctx);
        });

        let repo_metadata = RepoMetadataModel::as_ref(ctx);
        let mut repo_ids = repo_metadata.local_repository_ids(ctx);
        repo_ids.extend(
            repo_metadata
                .remote_repository_ids(ctx)
                .cloned()
                .map(RepositoryIdentifier::Remote),
        );
        let mut model = Self {
            refresh_generations: HashMap::new(),
            next_refresh_generation: 0,
        };
        for repo_id in repo_ids {
            model.refresh_project_rules_for_repo(&repo_id, ctx);
        }
        model
    }

    fn handle_repo_metadata_event(
        &mut self,
        event: &repo_metadata::wrapper_model::RepoMetadataEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        use repo_metadata::wrapper_model::RepoMetadataEvent;

        match event {
            RepoMetadataEvent::RepositoryUpdated { id: repo_id }
            | RepoMetadataEvent::FileTreeEntryUpdated { id: repo_id } => {
                self.refresh_project_rules_for_repo(repo_id, ctx);
            }
            RepoMetadataEvent::FileTreeUpdated { ids } => {
                for repo_id in ids {
                    self.refresh_project_rules_for_repo(repo_id, ctx);
                }
            }
            RepoMetadataEvent::RepositoryRemoved { id: repo_id } => {
                self.clear_project_rules_for_removed_repository(repo_id, ctx);
            }
            RepoMetadataEvent::UpdatingRepositoryFailed { id } => {
                log::warn!("Project rule discovery unavailable after repository indexing failed: {id:?}");
            }
            RepoMetadataEvent::IncrementalUpdateReady { .. } => {}
        }
    }
    fn refresh_project_rules_for_repo(
        &mut self,
        repo_id: &RepositoryIdentifier,
        ctx: &mut ModelContext<Self>,
    ) {
        let refresh_generation = self.advance_refresh_generation(repo_id);
        let Some(root_path) = repo_id.to_local_or_remote_path() else {
            return;
        };
        let repo_id_for_result = repo_id.clone();
        let remote_query_provider = remote_project_metadata_query_provider(repo_id, ctx);
        let query = RepoMetadataModel::as_ref(ctx).get_repo_contents(
            repo_id.clone(),
            project_rule_query(),
            RepoContentsQueryBudget {
                max_entries_scanned: PROJECT_RULE_MAX_ENTRIES_SCANNED,
            },
            remote_query_provider,
            ctx,
        );
        ctx.spawn(query, move |me, result, ctx| match result {
            Ok(contents) => {
                me.hydrate_discovered_project_rules_if_current(
                    &repo_id_for_result,
                    refresh_generation,
                    root_path,
                    contents,
                    ctx,
                );
            }
            Err(err) => {
                log::warn!("Failed to discover project rules: {err}");
            }
        });
    }

    fn hydrate_discovered_project_rules_if_current(
        &mut self,
        repo_id: &RepositoryIdentifier,
        refresh_generation: u64,
        root_path: LocalOrRemotePath,
        contents: Vec<OwnedRepoContent>,
        ctx: &mut ModelContext<Self>,
    ) {
        if self.refresh_generations.get(repo_id) != Some(&refresh_generation) {
            return;
        }
        let rule_paths = find_project_rule_files_in_contents(repo_id, contents);
        if rule_paths.is_empty() {
            self.apply_project_rules_from_metadata_if_current(
                repo_id,
                refresh_generation,
                root_path,
                Vec::new(),
                ctx,
            );
            return;
        }
        self.spawn_read_project_rules_from_files(
            repo_id.clone(),
            refresh_generation,
            root_path,
            rule_paths,
            ctx,
        );
    }
    fn spawn_read_project_rules_from_files(
        &mut self,
        repo_id: RepositoryIdentifier,
        refresh_generation: u64,
        root_path: LocalOrRemotePath,
        rule_paths: Vec<LocalOrRemotePath>,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(read_rule_contents) = read_project_rule_contents(rule_paths, ctx) else {
            return;
        };
        ctx.spawn(
            async move {
                let rule_contents = read_rule_contents.await?;
                Ok::<Vec<ProjectRule>, anyhow::Error>(build_project_rules(rule_contents))
            },
            move |me, rules, ctx| match rules {
                Ok(rules) => {
                    me.apply_project_rules_from_metadata_if_current(
                        &repo_id,
                        refresh_generation,
                        root_path,
                        rules,
                        ctx,
                    );
                }
                Err(err) => log::warn!("Failed to read project rules: {err}"),
            },
        );
    }

    fn clear_project_rules_for_removed_repository(
        &mut self,
        repo_id: &RepositoryIdentifier,
        ctx: &mut ModelContext<Self>,
    ) {
        self.refresh_generations.remove(repo_id);
        let Some(root_path) = repo_id.to_local_or_remote_path() else {
            return;
        };
        ProjectContextModel::handle(ctx).update(ctx, |model, ctx| match root_path {
            LocalOrRemotePath::Local(local_root) => {
                model.clear_local_project_rules_for_removed_metadata_root(local_root, ctx);
            }
            LocalOrRemotePath::Remote(remote_root) => {
                model.clear_remote_project_rules_for_removed_metadata_root(remote_root, ctx);
            }
        });
    }

    fn advance_refresh_generation(&mut self, repo_id: &RepositoryIdentifier) -> u64 {
        self.next_refresh_generation += 1;
        self.refresh_generations
            .insert(repo_id.clone(), self.next_refresh_generation);
        self.next_refresh_generation
    }

    fn apply_project_rules_from_metadata_if_current(
        &mut self,
        repo_id: &RepositoryIdentifier,
        refresh_generation: u64,
        root_path: LocalOrRemotePath,
        rules: Vec<ProjectRule>,
        ctx: &mut ModelContext<Self>,
    ) {
        if self.refresh_generations.get(repo_id) != Some(&refresh_generation) {
            return;
        }

        ProjectContextModel::handle(ctx).update(ctx, |model, ctx| match root_path {
            LocalOrRemotePath::Local(local_root) => {
                model.replace_local_project_rules_from_metadata(local_root, rules, ctx);
            }
            LocalOrRemotePath::Remote(remote_root) => {
                model.replace_remote_project_rules_from_metadata(remote_root, rules, ctx);
            }
        });
    }
}

impl Entity for MetadataProjectRulesModel {
    type Event = ();
}

impl SingletonEntity for MetadataProjectRulesModel {}

fn project_rule_query() -> RepoMetadataQuery {
    RepoMetadataQuery::FilesNamed {
        names: vec!["WARP.md".to_string(), "AGENTS.md".to_string()],
    }
}

fn find_project_rule_files_in_contents(
    repo_id: &RepositoryIdentifier,
    contents: Vec<OwnedRepoContent>,
) -> Vec<LocalOrRemotePath> {
    contents
        .into_iter()
        .filter_map(|content| {
            let OwnedRepoContent::File { path, .. } = content else {
                return None;
            };
            match repo_id {
                RepositoryIdentifier::Local(_) => {
                    path.to_local_path().map(LocalOrRemotePath::Local)
                }
                RepositoryIdentifier::Remote(remote_root) => Some(LocalOrRemotePath::Remote(
                    RemotePath::new(remote_root.host_id.clone(), path),
                )),
            }
        })
        .collect()
}

fn remote_project_metadata_query_provider(
    repo_id: &RepositoryIdentifier,
    ctx: &AppContext,
) -> Option<RemoteMetadataQueryProvider> {
    let RepositoryIdentifier::Remote(remote_id) = repo_id else {
        return None;
    };
    let client = RemoteServerManager::as_ref(ctx)
        .client_for_host(&remote_id.host_id)?
        .clone();

    Some(Arc::new(move |remote_id, query, budget| {
        let client = client.clone();
        async move {
            let response = client
                .query_repo_metadata(
                    remote_id.path.to_string(),
                    repo_metadata_query_to_proto(query),
                    budget.max_entries_scanned as u64,
                )
                .await
                .map_err(|err| RepoContentsQueryError::QueryFailed {
                    message: err.to_string(),
                })?;
            proto_query_repo_metadata_response_to_contents(&response)
        }
        .boxed()
    }))
}
fn read_project_rule_contents(
    rule_paths: Vec<LocalOrRemotePath>,
    ctx: &AppContext,
) -> Option<ProjectRuleContentsFuture> {
    match rule_paths.first()? {
        LocalOrRemotePath::Local(_) => Some(Box::pin(async move {
            Ok(read_local_project_rule_contents(rule_paths).await)
        })),
        LocalOrRemotePath::Remote(remote) => {
            let client = RemoteServerManager::as_ref(ctx)
                .client_for_host(&remote.host_id)?
                .clone();
            Some(Box::pin(async move {
                let response = client
                    .read_file_context(remote_rule_read_request(&rule_paths))
                    .await?;
                Ok(read_remote_project_rule_contents(
                    rule_paths,
                    response.file_contexts,
                ))
            }))
        }
    }
}

fn remote_rule_read_request(rule_paths: &[LocalOrRemotePath]) -> ReadFileContextRequest {
    ReadFileContextRequest {
        files: rule_paths
            .iter()
            .filter_map(|path| match path {
                LocalOrRemotePath::Remote(remote) => Some(ReadFileContextFile {
                    path: remote.path.as_str().to_string(),
                    line_ranges: Vec::new(),
                }),
                LocalOrRemotePath::Local(_) => None,
            })
            .collect(),
        max_file_bytes: None,
        max_batch_bytes: None,
    }
}

async fn read_local_project_rule_contents(
    rule_paths: Vec<LocalOrRemotePath>,
) -> Vec<(LocalOrRemotePath, String)> {
    let mut rule_contents = Vec::new();
    for path in rule_paths {
        let Some(local_path) = path.to_local_path() else {
            continue;
        };
        match async_fs::read_to_string(local_path).await {
            Ok(content) => rule_contents.push((path, content)),
            Err(error) => log::warn!(
                "Failed to read metadata-backed local project rule {}: {error}",
                local_path.display()
            ),
        }
    }
    rule_contents
}

fn read_remote_project_rule_contents(
    rule_paths: Vec<LocalOrRemotePath>,
    file_contexts: Vec<FileContextProto>,
) -> Vec<(LocalOrRemotePath, String)> {
    let content_by_path = file_contexts
        .into_iter()
        .filter_map(|file_context| {
            let file_context_proto::Content::TextContent(content) = file_context.content? else {
                return None;
            };
            Some((file_context.file_name, content))
        })
        .collect::<HashMap<_, _>>();
    rule_paths
        .into_iter()
        .filter_map(|path| {
            let LocalOrRemotePath::Remote(remote) = &path else {
                return None;
            };
            let content = content_by_path.get(remote.path.as_str())?.clone();
            Some((path, content))
        })
        .collect()
}

fn build_project_rules(rule_contents: Vec<(LocalOrRemotePath, String)>) -> Vec<ProjectRule> {
    rule_contents
        .into_iter()
        .map(|(path, content)| ProjectRule { path, content })
        .collect()
}


#[cfg(test)]
#[path = "metadata_project_rules_tests.rs"]
mod tests;
