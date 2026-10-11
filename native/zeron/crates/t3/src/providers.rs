//! Native settings use the same per-instance mutations as the canonical web client.
use super::{T3Service, failed};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use zeron_rpc::{RpcError, RpcReply};

const DRIVERS: &[(&str, &str)] = &[
    ("codex", "Codex"),
    ("claudeAgent", "Claude"),
    ("cursor", "Cursor"),
    ("grok", "Grok"),
    ("opencode", "OpenCode"),
    ("antigravity", "Antigravity"),
    ("pi", "Pi"),
    ("acpRegistry", "ACP Registry"),
];

fn bad(message: impl Into<String>) -> RpcError {
    RpcError::BadParams(message.into())
}

fn slug(value: &Value, key: &str) -> Result<String, RpcError> {
    let value = value[key]
        .as_str()
        .ok_or_else(|| bad(format!("Missing {key}")))?;
    if value.len() > 64
        || !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(bad(format!(
            "{key} must start with a letter and contain at most 64 letters, digits, '-' or '_'"
        )));
    }
    Ok(value.to_owned())
}

fn configured_instances(settings: &Value) -> Result<BTreeMap<String, Value>, RpcError> {
    let legacy = settings["providers"]
        .as_object()
        .ok_or_else(|| failed("Invalid T3 provider settings"))?;
    let explicit = settings["providerInstances"].as_object();
    let mut instances: BTreeMap<_, _> = legacy
        .iter()
        .map(|(id, config)| (id.clone(), json!({"driver": id, "config": config})))
        .collect();
    if let Some(explicit) = explicit {
        for (id, instance) in explicit {
            instances.insert(id.clone(), instance.clone());
        }
    }
    Ok(instances)
}

fn enabled(instance: &Value) -> bool {
    let envelope = instance["enabled"].as_bool();
    let config = instance["config"]["enabled"].as_bool();
    if envelope == Some(false) || config == Some(false) {
        return false;
    }
    envelope.or(config).unwrap_or_else(|| {
        !matches!(
            instance["driver"].as_str(),
            Some("cursor" | "grok" | "opencode" | "antigravity" | "pi")
        )
    })
}

// shortcut: RPC has no form schemas; mirror contracts/settings.ts until metadata is exposed.
fn fields(driver: &str) -> Value {
    let keys: &[(&str, &str, &str)] = match driver {
        "codex" => &[
            ("binaryPath", "Binary path", "text"),
            ("homePath", "CODEX_HOME path", "text"),
            ("shadowHomePath", "Shadow home path", "text"),
            ("launchArgs", "Launch arguments", "text"),
        ],
        "claudeAgent" => &[
            ("binaryPath", "Binary path", "text"),
            ("homePath", "CLAUDE_CONFIG_DIR path", "text"),
            ("autoCompactWindow", "Auto-compact after (tokens)", "text"),
            ("launchArgs", "Launch arguments", "text"),
        ],
        "cursor" => &[("CURSOR_API_KEY", "Cursor API key", "environmentPassword")],
        "grok" => &[("binaryPath", "Binary path", "text")],
        "pi" => &[
            ("binaryPath", "Binary path", "text"),
            ("launchArgs", "Launch arguments", "text"),
        ],
        "opencode" => &[
            ("binaryPath", "Binary path", "text"),
            ("serverUrl", "Server URL", "text"),
            ("serverPassword", "Server password", "password"),
        ],
        "antigravity" => &[
            ("authMethod", "Sign-in method", "select"),
            ("apiKey", "API key", "password"),
            ("gcpProject", "GCP project", "text"),
            ("gcpLocation", "GCP location", "text"),
            ("binaryPath", "Binary path", "text"),
        ],
        "acpRegistry" => &[
            ("source", "ACP source", "select"),
            ("agentId", "Registry agent ID", "text"),
            ("commandPath", "Executable override", "text"),
            ("authMethodId", "Authentication method", "text"),
        ],
        _ => &[],
    };
    json!(keys.iter().map(|(key, label, control)| {
        let options = match *key {
            "source" => json!([{"value":"registry","label":"ACP Registry"},{"value":"local","label":"Local command"}]),
            "authMethod" => json!([{"value":"oauth-personal","label":"Google account"},{"value":"oauth-business","label":"Gemini Enterprise"},{"value":"gemini-api-key","label":"Gemini API key"},{"value":"agent-platform","label":"Agent Platform (Vertex AI)"}]),
            _ => json!([]),
        };
        json!({"key":key,"label":label,"control":control,"options":options})
    }).collect::<Vec<_>>())
}

fn snapshot(settings: &Value, providers: &Value) -> Result<Value, RpcError> {
    let live = providers
        .as_array()
        .ok_or_else(|| failed("Invalid T3 provider status list"))?;
    let configured = configured_instances(settings)?;
    let mut drivers: BTreeMap<String, Value> = DRIVERS
        .iter()
        .map(|(id, label)| {
            (
                (*id).to_owned(),
                json!({"id":id,"label":label,"fields":fields(id),"known":true}),
            )
        })
        .collect();
    for provider in live {
        if let Some(driver) = provider["driver"].as_str() {
            drivers
                .entry(driver.to_owned())
                .or_insert_with(|| json!({"id":driver,"label":driver,"fields":[],"known":false}));
        }
    }
    let instances: Vec<_> = configured.into_iter().map(|(id, instance)| {
        let driver = instance["driver"].as_str().unwrap_or("");
        drivers.entry(driver.to_owned()).or_insert_with(|| json!({"id":driver,"label":driver,"fields":[],"known":false}));
        let provider = live.iter().find(|provider| provider["instanceId"] == id);
        let explicit = settings["providerInstances"].get(&id).is_some();
        json!({"instanceId":id,"enabled":enabled(&instance),"removable":explicit && id != driver,"instance":instance,"live":provider})
    }).collect();
    Ok(json!({"instances":instances,"drivers":drivers.into_values().collect::<Vec<_>>()}))
}

fn validate_instance(instance: &Value) -> Result<(), RpcError> {
    slug(instance, "driver")?;
    if !instance.is_object() {
        return Err(bad("Provider instance must be an object"));
    }
    for key in ["displayName", "accentColor"] {
        if let Some(value) = instance.get(key) {
            if !value.as_str().is_some_and(|text| !text.trim().is_empty()) {
                return Err(bad(format!("{key} must be a non-empty string")));
            }
        }
    }
    if instance
        .get("enabled")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(bad("enabled must be boolean"));
    }
    let driver = instance["driver"].as_str().unwrap();
    if let Some(environment) = instance.get("environment") {
        let environment = environment
            .as_array()
            .ok_or_else(|| bad("Environment must be an array"))?;
        let mut names = std::collections::HashSet::new();
        for variable in environment {
            let name = variable["name"]
                .as_str()
                .ok_or_else(|| bad("Environment variable needs a name"))?;
            if name.len() > 128
                || !name
                    .as_bytes()
                    .first()
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
                || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                || !names.insert(name)
            {
                return Err(bad(
                    "Environment variable names must be unique letters, digits or underscores and start with a letter or underscore",
                ));
            }
            if !variable["value"].is_string()
                || !variable["sensitive"].is_boolean()
                || variable
                    .get("valueRedacted")
                    .is_some_and(|value| !value.is_boolean())
            {
                return Err(bad("Invalid environment variable value or sensitivity"));
            }
        }
    }
    if let Some(config) = instance.get("config") {
        if !DRIVERS.iter().any(|(id, _)| *id == driver) {
            return Ok(());
        }
        if !config.is_object() {
            return Err(bad("Provider configuration must be an object"));
        }
        if config
            .get("enabled")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(bad("Configuration enabled must be boolean"));
        }
        for field in fields(driver).as_array().unwrap() {
            if field["control"] == "environmentPassword" {
                continue;
            }
            let key = field["key"].as_str().unwrap();
            if let Some(value) = config.get(key) {
                if !value.is_string() {
                    return Err(bad(format!("{key} must be a string")));
                }
                if field["control"] == "select"
                    && !field["options"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|option| option["value"] == *value)
                {
                    return Err(bad(format!("Invalid {key}")));
                }
            }
        }
        if driver == "claudeAgent" {
            if let Some(window) = config
                .get("autoCompactWindow")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                if !window.bytes().all(|c| c.is_ascii_digit())
                    || !window
                        .parse::<u64>()
                        .is_ok_and(|value| (100_000..=1_000_000).contains(&value))
                {
                    return Err(bad(
                        "Auto-compact must be between 100,000 and 1,000,000 tokens",
                    ));
                }
            }
        }
        if driver == "acpRegistry" {
            let source = config["source"].as_str().unwrap_or("registry");
            let key = if source == "local" {
                "commandPath"
            } else {
                "agentId"
            };
            if !config[key]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty())
            {
                return Err(bad(format!("ACP requires {key}")));
            }
        }
        if let Some(models) = config.get("customModels") {
            let models = models
                .as_array()
                .ok_or_else(|| bad("Custom models must be an array"))?;
            let mut seen = std::collections::HashSet::new();
            for model in models {
                let id = model
                    .as_str()
                    .or_else(|| model["slug"].as_str())
                    .ok_or_else(|| bad("Each custom model needs a slug"))?;
                if id.trim().is_empty()
                    || !seen.insert(id)
                    || (driver == "acpRegistry" && !model.is_string())
                {
                    return Err(bad("Custom model slugs must be non-empty and unique"));
                }
                if model.get("name").is_some_and(|value| {
                    !value.as_str().is_some_and(|value| !value.trim().is_empty())
                }) || model
                    .get("capabilities")
                    .is_some_and(|value| !value.is_object())
                {
                    return Err(bad("Invalid custom model name or capabilities"));
                }
            }
        }
    }
    Ok(())
}

fn mutation(settings: &Value, method: &str, params: &Value) -> Result<Value, RpcError> {
    let id = slug(params, "instanceId")?;
    let instances = configured_instances(settings)?;
    let current = instances.get(&id);
    let expected = params
        .get("expectedInstance")
        .ok_or_else(|| bad("Missing expectedInstance; reload providers before editing"))?;
    if current.unwrap_or(&Value::Null) != expected {
        return Err(bad(
            "This provider changed elsewhere. Reload providers before saving.",
        ));
    }
    if method == "T3ProviderRemove" {
        let current = current.ok_or_else(|| bad("Provider no longer exists"))?;
        if current["driver"] == id || settings["providerInstances"].get(&id).is_none() {
            return Err(bad(
                "Default providers cannot be removed; disable the provider instead",
            ));
        }
        return Ok(json!({"operation":"remove","instanceId":id}));
    }
    let instance = params
        .get("instance")
        .ok_or_else(|| bad("Missing instance"))?;
    validate_instance(instance)?;
    if current.is_some_and(|value| value["driver"] != instance["driver"]) {
        return Err(bad("An existing provider's driver cannot be changed"));
    }
    Ok(
        json!({"operation":if current.is_none() {"create"} else {"upsert"},"instanceId":id,"instance":instance}),
    )
}

impl T3Service {
    pub(super) async fn provider_settings(
        &self,
        method: &str,
        params: Value,
    ) -> Result<RpcReply, RpcError> {
        let client = self.client().await?;
        let _write = if matches!(method, "T3ProviderUpsert" | "T3ProviderRemove") {
            Some(self.project_write.lock().await)
        } else {
            None
        };
        provider_request(&client, method, params).await
    }
}

async fn provider_request(
    client: &zeron_rpc::RpcClient,
    method: &str,
    params: Value,
) -> Result<RpcReply, RpcError> {
    if !matches!(
        method,
        "T3ProvidersGet"
            | "T3ProvidersRefresh"
            | "T3ProviderUpsert"
            | "T3ProviderRemove"
            | "T3ProviderUpdate"
    ) {
        return Err(bad("Unknown native provider method"));
    }
    let mut settings = client.call("server.getSettings", json!({})).await?;
    let statuses = match method {
        "T3ProviderUpsert" | "T3ProviderRemove" => {
            let change = mutation(&settings, method, &params)?;
            // shortcut: the server has no compare-and-swap RPC; this fresh
            // check detects earlier edits, upgrade when it exposes a revision.
            settings = client
                .call(
                    "server.updateSettings",
                    json!({"patch":{},"providerInstanceMutation":change}),
                )
                .await?;
            None
        }
        "T3ProvidersRefresh" => Some(
            client
                .call("server.refreshProviders", json!({"refreshModels":true}))
                .await?,
        ),
        "T3ProviderUpdate" => {
            let id = slug(&params, "instanceId")?;
            let instances = configured_instances(&settings)?;
            let instance = instances
                .get(&id)
                .ok_or_else(|| bad("Provider no longer exists"))?;
            Some(
                client
                    .call(
                        "server.updateProvider",
                        json!({"provider":instance["driver"],"instanceId":id}),
                    )
                    .await?,
            )
        }
        _ => None,
    };
    let providers = match statuses {
        Some(value) => value["providers"].clone(),
        None => client.call("server.getConfig", json!({})).await?["providers"].clone(),
    };
    RpcReply::value(&snapshot(&settings, &providers)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CanonicalMock {
        settings: std::sync::Mutex<Value>,
        calls: std::sync::Mutex<Vec<(String, Value)>>,
    }

    #[async_trait::async_trait]
    impl zeron_rpc::RpcService for CanonicalMock {
        async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_owned(), params.clone()));
            match method {
                "server.getSettings" => RpcReply::value(&*self.settings.lock().unwrap()),
                "server.getConfig" => RpcReply::value(&json!({"providers":[]})),
                "server.refreshProviders" | "server.updateProvider" => {
                    RpcReply::value(&json!({"providers":[]}))
                }
                "server.updateSettings" => {
                    assert_eq!(params["patch"], json!({}));
                    let change = &params["providerInstanceMutation"];
                    let mut settings = self.settings.lock().unwrap();
                    let id = change["instanceId"].as_str().unwrap();
                    // A concurrent client writes another instance after our read.
                    settings["providerInstances"]["other"] =
                        json!({"driver":"codex","config":{"launchArgs":"changed remotely"}});
                    if change["operation"] == "remove" {
                        settings["providerInstances"]
                            .as_object_mut()
                            .unwrap()
                            .remove(id);
                    } else {
                        settings["providerInstances"][id] = change["instance"].clone();
                    }
                    RpcReply::value(&*settings)
                }
                _ => Err(bad("Unexpected canonical RPC")),
            }
        }
    }

    #[tokio::test]
    async fn canonical_rpc_round_trip_preserves_concurrent_other_instances_and_routes_updates() {
        let original =
            json!({"driver":"pi","enabled":false,"config":{"customModels":["custom"],"future":42}});
        let service = std::sync::Arc::new(CanonicalMock {
            settings: std::sync::Mutex::new(
                json!({"providers":{},"providerInstances":{"work":original}}),
            ),
            calls: Default::default(),
        });
        let client = zeron_rpc::memory_client(service.clone());
        let mut changed = original.clone();
        changed["enabled"] = json!(true);
        let result = provider_request(
            &client,
            "T3ProviderUpsert",
            json!({"instanceId":"work","instance":changed,"expectedInstance":original}),
        )
        .await
        .unwrap();
        let RpcReply::Value(snapshot) = result else {
            panic!("Expected provider snapshot")
        };
        assert!(
            snapshot["instances"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["instanceId"] == "other")
        );
        assert_eq!(
            service.settings.lock().unwrap()["providerInstances"]["work"]["config"]["future"],
            42
        );
        let write_count = service
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "server.updateSettings")
            .count();
        assert!(
            provider_request(
                &client,
                "T3ProviderUpsert",
                json!({"instanceId":"work","instance":changed,"expectedInstance":original})
            )
            .await
            .is_err()
        );
        assert_eq!(
            service
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(method, _)| method == "server.updateSettings")
                .count(),
            write_count
        );
        provider_request(&client, "T3ProvidersRefresh", json!({}))
            .await
            .unwrap();
        provider_request(&client, "T3ProviderUpdate", json!({"instanceId":"work"}))
            .await
            .unwrap();
        let calls = service.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|(method, params)| method == "server.refreshProviders"
                    && params == &json!({"refreshModels":true}))
        );
        assert!(
            calls
                .iter()
                .any(|(method, params)| method == "server.updateProvider"
                    && params == &json!({"provider":"pi","instanceId":"work"}))
        );
    }

    #[test]
    fn instances_preserve_pi_custom_config_and_authoritative_status() {
        let instance = json!({"driver":"pi","enabled":true,"config":{"customModels":[{"slug":"x","name":"X","capabilities":{"future":true}}],"future":{"keep":true}},"environment":[{"name":"TOKEN","value":"","sensitive":true,"valueRedacted":true}]});
        let settings =
            json!({"providers":{"pi":{"enabled":false}},"providerInstances":{"pi_work":instance}});
        let live = json!([{"instanceId":"pi_work","driver":"pi","status":"warning","version":"1","auth":{"status":"unknown"}}]);
        let result = snapshot(&settings, &live).unwrap();
        let row = result["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["instanceId"] == "pi_work")
            .unwrap();
        assert_eq!(row["instance"], instance);
        assert_eq!(row["live"], live[0]);
        assert_eq!(row["enabled"], true);
        assert_eq!(row["removable"], true);
    }

    #[test]
    fn writes_are_granular_and_reject_stale_edits_and_duplicate_creates() {
        let original = json!({"driver":"pi","config":{"enabled":false,"future":42}});
        let settings = json!({"providers":{},"providerInstances":{"work":original,"other":{"driver":"codex"}}});
        let changed = json!({"driver":"pi","enabled":true,"config":{"enabled":true,"future":42}});
        let params = json!({"instanceId":"work","instance":changed,"expectedInstance":original});
        let update = mutation(&settings, "T3ProviderUpsert", &params).unwrap();
        assert_eq!(
            update,
            json!({"operation":"upsert","instanceId":"work","instance":changed})
        );
        assert!(update.get("providerInstances").is_none());
        let stale = json!({"instanceId":"work","instance":changed,"expectedInstance":null});
        assert!(mutation(&settings, "T3ProviderUpsert", &stale).is_err());
        let create = json!({"instanceId":"new_pi","instance":changed,"expectedInstance":null});
        assert_eq!(
            mutation(&settings, "T3ProviderUpsert", &create).unwrap()["operation"],
            "create"
        );
        assert_eq!(
            mutation(&settings, "T3ProviderRemove", &params).unwrap(),
            json!({"operation":"remove","instanceId":"work"})
        );
    }

    #[test]
    fn validation_rejects_bad_identity_fields_and_keeps_unknown_drivers() {
        for id in ["", "1pi", "pi/path", "pi work", "π"] {
            assert!(slug(&json!({"instanceId":id}), "instanceId").is_err());
        }
        assert!(validate_instance(&json!({"driver":"pi","config":{"launchArgs":[]}})).is_err());
        assert!(
            validate_instance(&json!({"driver":"antigravity","config":{"authMethod":"invented"}}))
                .is_err()
        );
        assert!(
            validate_instance(&json!({"driver":"futureDriver","config":{"future":[1,2]}})).is_ok()
        );
        assert!(!enabled(
            &json!({"driver":"pi","enabled":true,"config":{"enabled":false}})
        ));
        assert!(!enabled(&json!({"driver":"pi"})));
        assert!(enabled(&json!({"driver":"codex"})));
    }
}
