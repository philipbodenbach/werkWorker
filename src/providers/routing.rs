//! Host-authorized connection selection. Every agent session can contain private
//! workspace data as early as token counting, before the first chat completion.
use super::*;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disclosure {
    MetadataOnly,
    WorkspaceContext,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceCapability {
    ToolCalling,
    Streaming,
    Images,
    StructuredOutput,
    ParallelTools,
    Reasoning,
}

impl InferenceCapability {
    fn supported(self, config: &WerkConfig) -> bool {
        let caps = &config.capabilities;
        match self {
            Self::ToolCalling => caps.tool_calling == Support::Supported,
            Self::Streaming => config.stream && caps.streaming == Support::Supported,
            Self::Images => caps.images == Support::Supported,
            Self::StructuredOutput => caps.structured_output == Support::Supported,
            Self::ParallelTools => caps.parallel_tools == Support::Supported,
            Self::Reasoning => caps.reasoning == Support::Supported,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRoute {
    pub name: String,
    pub transport: ProviderTransport,
    pub config: WerkConfig,
    pub secret_env: Option<String>,
    pub max_disclosure: Disclosure,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRouting {
    pub version: u32,
    #[serde(default)]
    pub prefer_loopback: bool,
    #[serde(default)]
    pub required_capabilities: Vec<InferenceCapability>,
    pub candidates: Vec<ProviderRoute>,
}

impl ProviderRouting {
    /// Only host code supplies this policy. Selection performs discovery without
    /// receiving prompt contents, then pins the returned provider for the run.
    pub async fn connect(self, secrets: &dyn SecretResolver) -> Result<WerkProvider> {
        if self.version != 1 || !(1..=8).contains(&self.candidates.len()) {
            return Err(WorkerError::new(
                "invalid_provider_routing",
                "Expected routing version 1 and one to eight candidates",
            ));
        }
        let mut names = BTreeSet::new();
        let mut origins = BTreeMap::new();
        let mut routes = Vec::with_capacity(self.candidates.len());
        for route in self.candidates {
            validate_id(&route.name)?;
            if !names.insert(route.name.clone())
                || route.config.model.trim().is_empty()
                || route.config.model.len() > 512
                || route.config.model.chars().any(char::is_control)
            {
                return Err(WorkerError::new(
                    "invalid_provider_routing",
                    "Candidate names must be unique and each candidate needs a model ID",
                ));
            }
            if let Some(reference) = &route.secret_env {
                validate_id(reference)?;
            }
            let client = WerkClient::for_transport(&route.config, None, route.transport)?;
            if origins
                .insert(client.origin(), route.transport)
                .is_some_and(|previous| previous != route.transport)
            {
                return Err(WorkerError::new(
                    "invalid_provider_routing",
                    "One origin cannot represent both Werk and an external provider",
                ));
            }
            routes.push((route, client));
        }
        if self.prefer_loopback {
            // Stable sort retains explicit order within each preference group.
            routes.sort_by_key(|(_, client)| !is_literal_loopback(&client.base));
        }

        let mut attempts = Vec::new();
        let mut last_error = WorkerError::new(
            "provider_disclosure_denied",
            "No candidate is authorized to receive workspace context",
        );
        for (route, mut client) in routes {
            if route.max_disclosure != Disclosure::WorkspaceContext {
                attempts.push(json!({"candidate":route.name,"status":"disclosure_denied"}));
                continue;
            }
            if route.config.stream && route.config.capabilities.streaming != Support::Supported
                || !self
                    .required_capabilities
                    .iter()
                    .all(|cap| cap.supported(&route.config))
            {
                attempts.push(json!({"candidate":route.name,"status":"unsupported_capability"}));
                last_error = WorkerError::new(
                    "unsupported_capability",
                    "Candidate does not meet the configured capability requirements",
                );
                continue;
            }
            if let Some(reference) = &route.secret_env {
                let value = secrets
                    .resolve(reference)?
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        WorkerError::new(
                            "auth_error",
                            "The selected candidate's configured credential is unavailable",
                        )
                    })?;
                client.secret = Some(Arc::new(value));
            }
            client.route_binding = Some(fingerprint(&(
                route.transport,
                client.origin(),
                &route.config.model,
                &route.config.deployment,
                &route.secret_env,
                route.max_disclosure,
            )));
            let result = WerkProvider::connect_client(route.config, client)
                .await
                .and_then(|provider| {
                    let models =
                        provider.discovery["models"]["data"]
                            .as_array()
                            .ok_or_else(|| {
                                WorkerError::new(
                                    "invalid_model_catalog",
                                    "Provider returned an invalid model catalogue",
                                )
                            })?;
                    if models
                        .iter()
                        .any(|entry| entry["id"].as_str().is_none_or(str::is_empty))
                    {
                        return Err(WorkerError::new(
                            "invalid_model_catalog",
                            "Model catalogue contains an invalid model ID",
                        ));
                    }
                    if !models
                        .iter()
                        .any(|entry| entry["id"].as_str() == Some(&provider.config.model))
                    {
                        return Err(WorkerError::new(
                            "model_unavailable",
                            "Candidate model is absent from the provider's catalogue",
                        ));
                    }
                    if provider.capabilities().context_tokens.is_none() {
                        return Err(WorkerError::new(
                            "context_limit_unknown",
                            "Candidate has no known per-request context limit",
                        ));
                    }
                    Ok(provider)
                });
            match result {
                Ok(mut provider) => {
                    attempts.push(json!({"candidate":route.name,"status":"selected"}));
                    provider.discovery["routing"] = json!({"selected":route.name,"transport":route.transport,
                        "origin":provider.client.origin(),"model":provider.config.model,"deployment":provider.config.deployment,
                        "disclosure":"workspace_context","attempts":attempts});
                    return Ok(provider);
                }
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "timeout"
                            | "generation_limits_timeout"
                            | "unsupported_capability"
                            | "model_unavailable"
                            | "context_limit_unknown"
                            | "provider_unavailable"
                            | "rate_limit"
                            | "werk_protocol_unavailable"
                            | "werk_protocol_unsupported"
                    ) =>
                {
                    attempts.push(json!({"candidate":route.name,"status":error.code}));
                    last_error = error;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error)
    }
}
