# Model configuration

wisp-science calls remote LLM APIs through model profiles. Desktop users
configure these in **Settings -> Models**. Each row is a model profile with its
own display name, provider, API URL, model ID, advanced options, and API key.

The composer model picker binds the selected HTTP model to the current
conversation. Switching one populated conversation asks for confirmation and
does not change any other conversation. Empty conversations switch immediately
without a warning. The active profile in Settings remains the default for new
conversations.

Model profiles describe model access and capabilities for the **built-in Wisp
agent**. External coding agents (Codex / Claude via ACP) are configured under
**Settings → Models → ACP Agents** — see [ACP Agents](acp-agents.md). Do not put
an ACP launch command in an HTTP model profile.

For image workflows, mark an API profile as **Supports image input** and
optionally **Use for image analysis**. Image attachments are sent directly to a
visual input model. When the input model is non-visual, Wisp first calls the
assigned vision model and passes its text observations to the input model.
`view_image` and image reads use the assigned vision model in the same way.
Raster image input supports PNG, JPEG, GIF, and WebP files up to 5 MiB.

When switching a populated conversation to a non-visual model, the confirmation
explains that previously sent images will be omitted from future requests to
that model. This substitution happens only while preparing the API request; it
does not delete or rewrite the saved conversation. A new image attached after
the switch is analyzed through the assigned vision model. Without an assigned
vision model, Wisp rejects that new image before starting the main model turn.

Image generation is a separate model role. Create an OpenAI profile with model
ID `gpt-image-2`, then enable **Use for image generation**. The built-in
**Scientific Illustrator** calls OpenAI's Image API and saves a PNG under
`figures/` when that role is assigned and PNG or image-model generation is
requested. An explicit SVG/vector/editable request always uses the specialist's
direct-SVG path, even when `gpt-image-2` is configured: it writes SVG, renders
that exact SVG to a PNG preview, inspects the preview, and iterates on the SVG.
An explicit PNG request requires the configured image-generation model; it is
not silently replaced with SVG. The configured generation tool is also
available in ordinary built-in-agent
conversations, so a direct request for the Scientific Illustrator or
`gpt-image-2` can generate the image without preselecting the specialist. While
the request runs, the conversation shows an image placeholder and replaces it
with the generated PNG. When the user does not specify a format, the specialist
uses the assigned image-generation profile to create PNG if present. Otherwise
it uses the same SVG -> PNG preview -> SVG correction workflow and delivers SVG
under `figures/`. Image-only profiles do not appear in chat, Reviewer,
specialist, delegation, or side-chat model pickers.

An image-generation assignment does not also provide image analysis.
`gpt-image-2` may consume an input image for editing, but its Image API returns
generated pixels rather than the textual observations required by `view_image`
and a non-visual chat model. Configure a chat/Responses profile with **Supports
image input** and **Use for image analysis** for that role; it may use the same
provider credentials, but it remains a separate API capability.

The **Valid** action checks `gpt-image-2` access through OpenAI's model metadata
endpoint. It does not send the image-only model to Responses/Chat Completions
and does not generate a billable validation image.

## API providers

| Provider | Use when | Required fields |
| --- | --- | --- |
| OpenAI-compatible | DeepSeek, GLM, local gateways, or any `/chat/completions` compatible endpoint | API URL, Model ID, API key |
| OpenAI (Responses API) | OpenAI reasoning/tool-call models through `/v1/responses` | API URL, Model ID, API key |
| Anthropic | Claude API through `/v1/messages` | API URL, Model ID, API key |

For OpenAI-compatible and Responses API profiles, Wisp sends its internal
`python` REPL tool as `wisp_python` and maps returned calls back to `python`.
This avoids the reserved `python` function-name collision on Codex models,
including when the request is translated by gateways such as CLIProxyAPI.

API keys are stored in the OS keyring. They are not stored in SQLite.

The desktop app stores model profile metadata in `.wisp/wisp.sqlite`. Existing single-model installs are migrated into a `default` model profile the first time settings are loaded.

## Headless CLI

The `wisp-science` headless CLI uses environment variables and supports API providers:

```powershell
$env:WISP_PROVIDER = "openai"           # openai, openai_responses, or anthropic
$env:WISP_API_URL  = "https://api.deepseek.com"
$env:WISP_MODEL    = "deepseek-v4-pro"
$env:WISP_API_KEY  = "<your provider key>"
cargo run -p wisp-cli
```
