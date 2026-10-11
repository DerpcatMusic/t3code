use super::*;

fn snapshot(cwd: &str, source: &Value, generated: &Value, device: &str) -> Result<Value, RpcError> {
    let patch = text(source, "diff").map_err(failed)?;
    let files = source["files"].as_array().cloned().unwrap_or_default();
    let additions: u64 = files.iter().filter_map(|f| f["additions"].as_u64()).sum();
    let deletions: u64 = files.iter().filter_map(|f| f["deletions"].as_u64()).sum();
    Ok(
        json!({"checkoutId":format!("t3-checkout:{cwd}"),"deviceId":device,"cwd":cwd,"patch":patch,
        "files":files.iter().map(|f|json!({"path":f["path"],"oldPath":f["previousPath"],"status":if f["previousPath"].is_null() {"modified"} else {"renamed"},"additions":f["additions"],"deletions":f["deletions"],"binary":false})).collect::<Vec<_>>(),
        "additions":additions,"deletions":deletions,"truncated":source["truncated"],"checksum":text(source,"diffHash").map_err(failed)?,"updatedAt":generated}),
    )
}

async fn preview(client: &RpcClient, params: &Value) -> Result<(Value, Value), RpcError> {
    let cwd = text(params, "cwd").map_err(failed)?;
    let branch = params["mode"] == "branch";
    if !params["mode"].is_null() && params["mode"] != "workingTree" && !branch {
        return Err(RpcError::BadParams(
            "Use T3's checkpoint controls for a turn or commit comparison.".into(),
        ));
    }
    let mut input = json!({"cwd":cwd});
    if branch {
        input["baseRef"] = params["baseRef"].clone();
    }
    let result = client.call("review.getDiffPreview", input).await?;
    let source = rows(&result, "sources")
        .map_err(failed)?
        .iter()
        .find(|s| {
            s["kind"]
                == if branch {
                    "branch-range"
                } else {
                    "working-tree"
                }
        })
        .cloned()
        .ok_or_else(|| failed("Diff preview unavailable"))?;
    Ok((result, source))
}

fn stacked_action_input(method: &str, cwd: &str, thread: &Value) -> Result<Value, RpcError> {
    let action = match method {
        "T3PushGit" => "push",
        "T3CreatePullRequest" => "create_pr",
        _ => return Err(RpcError::UnknownMethod(method.into())),
    };
    Ok(
        json!({"actionId":uuid::Uuid::new_v4().to_string(),"cwd":cwd,"action":action,
        "threadId":text(thread,"id").map_err(failed)?,"projectId":text(thread,"projectId").map_err(failed)?}),
    )
}

async fn current_status(client: &RpcClient, cwd: &str, remote: bool) -> Result<GitStats, RpcError> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut subscription = client
            .subscribe_checked(
                "subscribeVcsStatus",
                json!({"cwd":cwd,"includeRemote":remote}),
            )
            .await?;
        let mut status = None;
        while let Some(frame) = subscription.recv().await {
            GitStats::apply(&mut status, Some(&frame)).map_err(failed)?;
            if let Some(status) = &status {
                if !remote
                    || !status.is_repo
                    || !status.has_primary_remote
                    || status.remote.is_some()
                {
                    return Ok(status.clone());
                }
            }
        }
        Err(failed("Git status stream ended; refresh and retry"))
    })
    .await
    .map_err(|_| failed("Git status timed out; refresh and retry"))?
}

impl T3Service {
    pub(super) async fn git(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
        let client = self.client().await?;
        match method {
            "T3RefreshGit"
            | "T3ListGitBranches"
            | "T3CheckoutBranch"
            | "T3PushGit"
            | "T3CreatePullRequest"
            | "T3InitializeGit" => {
                let thread = self
                    .thread(text(&params, "chatId").map_err(failed)?)
                    .await?;
                let cwd = self.workspace_cwd(&thread)?;
                if method == "T3RefreshGit" {
                    return client
                        .call("vcs.refreshStatus", json!({"cwd":cwd}))
                        .await
                        .map(RpcReply::Value);
                }
                if method == "T3ListGitBranches" {
                    return client.call("vcs.listRefs", json!({"cwd":cwd,"refKind":"all","limit":100,"cursor":params["cursor"].as_u64().unwrap_or(0)})).await.map(RpcReply::Value);
                }
                let git = current_status(
                    &client,
                    &cwd,
                    matches!(method, "T3PushGit" | "T3CreatePullRequest"),
                )
                .await?;
                if method == "T3InitializeGit" {
                    if git.is_repo {
                        return Err(RpcError::BadParams("Git is already initialized".into()));
                    }
                    return client
                        .call("vcs.init", json!({"cwd":cwd}))
                        .await
                        .map(RpcReply::Value);
                }
                if method == "T3CheckoutBranch" {
                    // Active agents must retain the checkout they are operating on.
                    let shell = self.shell.borrow().clone();
                    if shell.all_threads().any(|other| {
                        other["activityRunStatus"].as_str().is_some()
                            && self
                                .workspace_cwd(other)
                                .is_ok_and(|other_cwd| other_cwd == cwd)
                    }) {
                        return Err(RpcError::BadParams(
                            "Stop agents using this checkout before switching branches".into(),
                        ));
                    }
                    if !git.is_repo || git.has_working_tree_changes {
                        return Err(RpcError::BadParams(
                            "Commit or stash changes before switching branches".into(),
                        ));
                    }
                    let ref_name = text(&params, "refName").map_err(failed)?;
                    if ref_name.trim().is_empty() {
                        return Err(RpcError::BadParams("Choose a branch".into()));
                    }
                    let result = client
                        .call("vcs.switchRef", json!({"cwd":cwd,"refName":ref_name}))
                        .await?;
                    self.dispatch(json!({"type":"thread.metadata.update","threadId":thread["id"],"branch":result["refName"]})).await?;
                    return Ok(RpcReply::Value(result));
                }
                let reason = if method == "T3PushGit" {
                    git.push_disabled_reason()
                } else {
                    git.pr_disabled_reason()
                };
                if let Some(reason) = reason {
                    return Err(RpcError::BadParams(reason.into()));
                }
                let input = stacked_action_input(method, &cwd, &thread)?;
                let result = client.call("git.runStackedAction", input).await?;
                if let Some(branch) = result["branch"]["name"].as_str() {
                    self.dispatch(json!({"type":"thread.metadata.update","threadId":thread["id"],"branch":branch})).await?;
                }
                Ok(RpcReply::Value(result))
            }
            methods::WATCH_CHECKOUT_DIFFS => {
                let cwd = text(&params, "cwd").map_err(failed)?.to_owned();
                let rx = client
                    .subscribe_checked(
                        "subscribeVcsStatus",
                        json!({"cwd":cwd,"includeRemote":false}),
                    )
                    .await?;
                let device = self.engine_info.device_id.clone();
                Ok(RpcReply::Stream(Box::pin(stream::unfold(
                    (rx, client, cwd, device),
                    |(mut rx, client, cwd, device)| async move {
                        rx.recv().await?;
                        match preview(&client, &json!({"cwd":cwd})).await.and_then(
                            |(result, source)| {
                                snapshot(&cwd, &source, &result["generatedAt"], &device)
                            },
                        ) {
                            Ok(diff) => Some((diff, (rx, client, cwd, device))),
                            Err(error) => {
                                tracing::warn!(%error,"T3 diff preview unavailable");
                                None
                            }
                        }
                    },
                ))))
            }
            methods::GET_CHECKOUT_DIFF => {
                let (result, source) = preview(&client, &params).await?;
                snapshot(
                    text(&params, "cwd").map_err(failed)?,
                    &source,
                    &result["generatedAt"],
                    &self.engine_info.device_id,
                )
                .map(RpcReply::Value)
            }
            methods::GET_CHECKOUT_FILE_DIFF_TEXT => {
                let (_, source) = preview(&client, &params).await?;
                if source["diffHash"] != params["diffChecksum"] {
                    return Ok(RpcReply::Value(
                        json!({"diffChecksum":params["diffChecksum"],"binary":false,"truncated":false,"stale":true}),
                    ));
                }
                let path = text(&params, "path").map_err(failed)?;
                let file = source["files"]
                    .as_array()
                    .and_then(|files| files.iter().find(|f| f["path"] == path));
                let old = file
                    .and_then(|f| f["previousPath"].as_str())
                    .unwrap_or(path);
                let contents=client.call("review.getDiffFileContents",json!({"cwd":params["cwd"],"sourceKind":source["kind"],"baseRef":source["baseRef"],"headRef":source["headRef"],"oldPath":old,"newPath":path,"changeType":if old != path {"rename-changed"} else {"change"}})).await?;
                Ok(RpcReply::Value(
                    json!({"diffChecksum":params["diffChecksum"],"oldText":contents["oldContents"],"newText":contents["newContents"],"binary":false,"truncated":false,"stale":false}),
                ))
            }
            methods::LIST_BRANCHES => {
                let result=client.call("vcs.listRefs",json!({"cwd":text(&params,"repoPath").map_err(failed)?,"refKind":"all","limit":100})).await?;
                let refs = rows(&result, "refs")
                    .map_err(failed)?
                    .iter()
                    .filter_map(|r| r["name"].as_str())
                    .collect::<Vec<_>>();
                RpcReply::value(&refs)
            }
            _ => Err(RpcError::UnknownMethod(method.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mutation_status_waits_for_authoritative_remote_stream_data() {
        struct Status;
        #[async_trait]
        impl RpcService for Status {
            async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
                assert_eq!(method, "subscribeVcsStatus");
                assert_eq!(params, json!({"cwd":"/fixture","includeRemote":true}));
                Ok(RpcReply::Stream(Box::pin(stream::iter(vec![
                    json!({"_tag":"snapshot","local":{"isRepo":true,"refName":"feature","hasPrimaryRemote":true,"isDefaultRef":false,"hasWorkingTreeChanges":false,"workingTree":{"files":[],"insertions":0,"deletions":0},"branchChanges":{"insertions":12,"deletions":3}},"remote":null}),
                    json!({"_tag":"remoteUpdated","remote":{"hasUpstream":true,"aheadCount":2,"behindCount":0,"aheadOfDefaultCount":2,"pr":null}}),
                ]))))
            }
        }
        let client = zeron_rpc::memory_client(Arc::new(Status));
        let status = current_status(&client, "/fixture", true).await.unwrap();
        assert_eq!(status.branch.as_deref(), Some("feature"));
        assert_eq!(status.additions, 12);
        assert_eq!(status.remote.unwrap().ahead_count, 2);
    }
    #[test]
    fn stacked_actions_link_thread_without_an_implicit_commit_or_feature_branch() {
        let thread = json!({"id":"thread","projectId":"project"});
        for (method, action) in [("T3PushGit", "push"), ("T3CreatePullRequest", "create_pr")] {
            let input = stacked_action_input(method, "/worktree", &thread).unwrap();
            assert_eq!(input["cwd"], "/worktree");
            assert_eq!(input["threadId"], "thread");
            assert_eq!(input["projectId"], "project");
            assert_eq!(input["action"], action);
            assert!(uuid::Uuid::parse_str(input["actionId"].as_str().unwrap()).is_ok());
            assert!(input.get("commitMessage").is_none());
            assert!(input.get("featureBranch").is_none());
        }
        assert!(stacked_action_input("T3PushGit", "/worktree", &json!({"id":"thread"})).is_err());
        assert!(stacked_action_input("T3CommitGit", "/worktree", &thread).is_err());
    }

    #[test]
    fn native_diff_keeps_complete_statistics_and_t3_patch_identity() {
        let source = json!({"diff":"a bounded patch","diffHash":"hash","truncated":true,"files":[{"path":"new.rs","previousPath":"old.rs","additions":42,"deletions":17}]});
        let result = snapshot(
            "/workspace",
            &source,
            &json!("2026-10-10T00:00:00Z"),
            "host",
        )
        .unwrap();
        let diff: zeron_proto::CheckoutDiff = serde_json::from_value(result).unwrap();
        assert_eq!(diff.additions, 42);
        assert_eq!(diff.deletions, 17);
        assert_eq!(diff.files[0].old_path.as_deref(), Some("old.rs"));
        assert!(diff.truncated);
        assert_eq!(diff.checksum, "hash");
    }
}
