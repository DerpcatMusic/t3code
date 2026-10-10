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

impl T3Service {
    pub(super) async fn git(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
        let client = self.client().await?;
        match method {
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
