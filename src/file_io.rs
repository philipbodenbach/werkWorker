//! Request-local file pagination from verified provider generation limits.
use crate::{
    core::*,
    providers::{ModelProvider, ReasoningOptions},
};
use serde_json::json;

pub(crate) const BUDGET_KEY: &str = "x-werk-file-io";

pub(crate) struct FileIoBudget<'a> {
    context: usize,
    output_ceiling: usize,
    provider: &'a dyn ModelProvider,
    options: tokio::sync::Mutex<std::collections::BTreeMap<usize, Option<ReasoningOptions>>>,
    selected: Option<String>,
}

impl<'a> FileIoBudget<'a> {
    pub(crate) fn supported(provider: &dyn ModelProvider) -> bool {
        provider.max_output_tokens().is_some() && provider.capabilities().context_tokens.is_some()
    }

    pub(crate) async fn discover(
        session: &SessionData,
        run: &Run,
        provider: &'a dyn ModelProvider,
    ) -> Result<Option<Self>> {
        if !Self::supported(provider) {
            return Ok(None);
        }
        Ok(Some(Self {
            context: run
                .limits
                .context_tokens
                .min(provider.capabilities().context_tokens.unwrap()),
            output_ceiling: provider.max_output_tokens().unwrap(),
            provider,
            options: Default::default(),
            selected: crate::reasoning::selected(session),
        }))
    }

    fn reasoning(&self, options: Option<&ReasoningOptions>) -> Option<u64> {
        let options = options?;
        if self.selected.is_none() && options.native_effort.is_some() {
            return None;
        }
        let profile = options.profile.as_ref().filter(|p| p.supported)?;
        let selected = self.selected.as_deref().unwrap_or(&options.requested);
        let tier = if selected == "default" {
            profile.default_tier.as_deref()?
        } else {
            selected
        };
        profile
            .tiers
            .iter()
            .find(|t| t.id == tier && t.available)?
            .reasoning_tokens
    }

    pub(crate) async fn tools(
        &self,
        original: &[ToolDefinition],
        output: usize,
    ) -> Vec<ToolDefinition> {
        let output = output.min(self.output_ceiling);
        // Keep counting and dispatch identical, but resolve the profile again
        // when context planning changes the output allowance. Cache failures too
        // so a planning sweep does not repeatedly poll an unavailable endpoint.
        let mut cache = self.options.lock().await;
        if let std::collections::btree_map::Entry::Vacant(entry) = cache.entry(output) {
            let options = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                self.provider.reasoning_options(output),
            )
            .await
            .ok()
            .and_then(std::result::Result::ok)
            .flatten();
            entry.insert(options);
        }
        let reasoning = self.reasoning(cache.get(&output).and_then(Option::as_ref));
        drop(cache);
        // Token-to-character conversion is guidance, not a tokenizer claim or
        // an execution gate. Only the server's token ceiling is authoritative.
        let usable = output
            .saturating_sub(reasoning.map_or(output / 2, |n| n.min(usize::MAX as u64) as usize));
        let envelope_reserve = usable.div_ceil(4).clamp(96, 384);
        let target = usable
            .saturating_sub(envelope_reserve)
            .saturating_mul(2)
            .min(32768);
        let read_bytes = self
            .context
            .saturating_sub(output + 512)
            .saturating_div(4)
            .clamp(512, 8192);
        let metadata = json!({"source":"provider_generation_limits", "context_tokens":self.context,
            "output_tokens":output,"reasoning_tokens":reasoning,"target_new_characters":target,
            "target_is_estimate":true,"read_page_bytes":read_bytes});
        let mut tools = original.to_vec();
        let continuation = if tools.iter().any(|t| t.name == "files.insert") {
            "Use files.insert for adding sections and files.edit for replacing existing text."
        } else {
            "Use files.edit with an exact unique anchor to add or replace one section."
        };
        for tool in &mut tools {
            if !matches!(
                tool.name.as_str(),
                "files.read"
                    | "files.write"
                    | "files.edit"
                    | "files.insert"
                    | "files.find"
                    | "files.search"
            ) {
                continue;
            }
            tool.input_schema[BUDGET_KEY] = metadata.clone();
            if matches!(
                tool.name.as_str(),
                "files.read" | "files.find" | "files.search"
            ) {
                tool.description.push_str(&format!(" Request context: {} tokens; file result pages are capped at {read_bytes} serialized bytes. Follow next_start_line, next_start_byte or next_offset as returned, and reuse the file hash. Read the needed section, not the whole file repeatedly.", self.context));
            } else {
                tool.description.push_str(&format!(" Current total output allowance: {output} tokens, including reasoning and tool JSON; reasoning allowance: {}. Aim for roughly {target} characters per complete section (estimate, not a hard character limit). Submit one complete section per call, then continue using the returned sha256 and next_insert_after_line when present. {continuation} Preserve existing work and use its current hash for changes. Never truncate incomplete JSON/code to fit.", reasoning.map_or_else(|| "unknown; conservative estimate used".into(), |n| format!("{n} tokens"))));
            }
        }
        tools
    }
}
