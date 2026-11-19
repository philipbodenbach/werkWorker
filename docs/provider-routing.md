# Host-authorized inference routing

Use `--werk-config` for a single Werk Protocol origin or
`--openai-compatible-config` for a single third-party origin. The third-party
mode connects directly to its configured model provider. Existing single-origin
configuration files and explicit scripted runs continue to work.

To select from more than one connection, pass `--provider-routing-env NAME`.
The named variable in the trusted CLI process contains the version 1 JSON
policy. The published [JSON Schema](provider-routing.schema.json) describes its fields. The SDK exposes the same selection through
`ProviderRouting::connect(&secrets)`. The worker reads this policy only when
the host explicitly selects it. Each candidate embeds its connection
configuration at startup, so changes to project preferences cannot redirect it.
Both transports use the same `ModelProvider` interface and the same local
execution supervisor.

## Configure preference and disclosure

This PowerShell example uses two connection files already configured in the
user's trusted settings directory. The files use the existing
[connection format](../examples/werk-config.json). Set each file's model ID,
context ceiling and supported capabilities for its actual deployment.

~~~powershell
$settingsDir = Join-Path $env:LOCALAPPDATA 'werkworker-connections'
$local = Get-Content -Raw -LiteralPath (Join-Path $settingsDir 'werk.json') | ConvertFrom-Json
$vendor = Get-Content -Raw -LiteralPath (Join-Path $settingsDir 'vendor.json') | ConvertFrom-Json
$env:MY_INFERENCE_ROUTES = @{
    version = 1
    prefer_loopback = $true
    required_capabilities = @('tool_calling')
    candidates = @(
        @{ name = 'local'; transport = 'werk_protocol'; config = $local
           secret_env = 'MY_WERK_KEY'; max_disclosure = 'workspace_context' }
        @{ name = 'vendor'; transport = 'openai_compatible'; config = $vendor
           secret_env = 'MY_VENDOR_KEY'; max_disclosure = 'metadata_only' }
    )
} | ConvertTo-Json -Depth 10
ww --provider-routing-env MY_INFERENCE_ROUTES diagnose
~~~

`workspace_context` authorizes the exact endpoint and selected model/deployment
to receive the agent conversation, tool definitions and model-facing tool
results. This can include private files, images and user instructions.
`metadata_only` supplies no such authorization. The worker skips that candidate
before resolving its credential or sending a request. All agent sessions require
the workspace class because even admission and token counting can contain the
full prompt. To authorize the vendor in the example for this data, the operator
must explicitly change that candidate's disclosure class. The provider offers
and policy fields contain no credential values. An omitted `secret_env` chooses
unauthenticated access only for that candidate.

In the default order, the worker checks the 1–8 configured candidates from
first to last. With `prefer_loopback=true`, literal loopback origins are tried
first, preserving the order within each group. This describes the first HTTP
hop. A Werk server can route inference to another runtime. The host must
authorize disclosure for the server and its configured downstream routing.
Both adapters disable HTTP redirects. A single origin can use only one
transport within the same policy.

Requirements can include `tool_calling`, `streaming`, `images`,
`structured_output`, `parallel_tools` and `reasoning`. Host model capability
assertions provide the model-specific input to routing. The worker also checks
the advertised model ID and available context limit. Werk Protocol negotiates
the model catalogue and the chosen complete/stream operation before selection.
Authentication, malformed protocol replies and uncertain transport identity
errors stop selection. An unavailable model, definite network refusal, failed
discovery deadline, rate limit or missing capability can advance to another
eligible candidate.

## Session lifetime and migration

Selection finishes before any agent content is submitted. The chosen connection
stays pinned through generation, cancellation, tool calls and child sessions.
A declined or interrupted chat is never replayed on another provider. Runnable
tool permissions, grants and sandbox selection remain the responsibility of the
trusted worker. The `diagnose` result includes `discovery.routing` with the
selected route and content-free attempt outcomes.

Routed sessions persist a fingerprint of their transport, origin, model,
deployment, disclosure class and credential reference. A different selection on
restart cannot resume that history. Use a new session to change destinations.
Keep each credential reference assigned to the same provider principal.
Single-provider HTTP sessions also bind their transport, origin, model,
deployment and credential reference at creation. The CLI passes the name from
`--secret-env` or the setup screen. SDK hosts can pass a stable principal
reference to `WerkProvider::connect_explicit`. An unauthenticated connection
can resume with the same endpoint settings. A raw key supplied without a
reference authorizes only that live connection. Reuse of a principal reference
assumes the credential continues to represent the same account.

Older unbound histories have no recorded recipient. The CLI can inspect
or export them locally using `ww sessions inspect ID` or `ww sessions export ID`.
To continue with inference, start a fresh session on the intended connection
and explicitly supply the context to share. Scripted offline sessions remain
available offline. Project or subagent model overrides cannot expand a routed
session's model selection. Standalone providers retain host-selected subagent
models on their original service.
