# Configuration

## TOML profiles

The preferred configuration is a strict TOML file at `$XDG_CONFIG_HOME/terrarium/config.toml`, or `~/.config/terrarium/config.toml` on Unix when `XDG_CONFIG_HOME` is unset. Pass another file with `--config PATH`. A selected TOML file is authoritative; invalid TOML is an error and does not fall back to environment variables.

```toml
version = 1
default_profile = "main"

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[profiles.main]
provider = "openrouter"
protocol = "openai-chat-completions"
model = "anthropic/claude-sonnet-4"
max_output_tokens = 32000
reasoning_effort = "high"

# DeepSeek's Anthropic-compatible endpoint works the same way:
#
# [providers.deepseek-anthropic]
# base_url = "https://api.deepseek.com/anthropic"
# api_key_env = "DEEPSEEK_API_KEY"
#
# [profiles.claude]
# provider = "deepseek-anthropic"
# protocol = "anthropic-messages"
# model = "deepseek-v4-flash"
```

`protocol` is one of `openai-chat-completions`, `openai-responses`, or `anthropic-messages`; each appends its own request path and auth headers to the provider `base_url`. Profiles may also set `request_timeout_ms` (whole-attempt budget, default 300000), `idle_timeout_ms` (longest gap between stream chunks, default 120000), and `context_window` (declared window in tokens, used for budget reporting). Provider and profile names must match `[A-Za-z0-9][A-Za-z0-9._-]*`. Provider URLs must be HTTP or HTTPS and contain no credentials, query, or fragment; trailing slashes are normalized away. Unknown fields are errors. API key values are never accepted in configuration: `api_key_env` names the environment variable read only when a request is sent.

A new turn uses `--profile NAME` when supplied, otherwise the configured `default_profile`. A later turn without `--profile` copies the previous turn's frozen resolved profile, prompt, and limits. The profile is not changed while a turn is open.

## Compatibility fallback

When no TOML file is selected, these legacy variables remain supported:

| Variable | Meaning |
|---|---|
| `TERRARIUM_LLM_API_KEY` | Credential value; referenced by the compatibility profile |
| `TERRARIUM_LLM_BASE_URL` | OpenAI-compatible service root or legacy `/chat/completions` endpoint |
| `TERRARIUM_LLM_MODEL` | Exact model ID; defaults to `deepseek-v4-flash` |

The binary does not load `.env` files. Credentials must already be present in the process environment or be supplied by an external secret manager. Keep secret files outside mounted directories.

One diagnostic variable is supported across both configuration paths: setting `TERRARIUM_LLM_DEBUG` to any value other than `0` dumps every request body and each decoded SSE event to stderr. The dump shows no credential values — headers stay out of the log — but it does contain the full prompt and reasoning payloads, so it is a local debugging tool, not something to leave on or paste into shared logs.

## Access modes

The model-driven agent is the default command:

```sh
terrarium [--config PATH] [--profile NAME] [--read-only | --full-access] [--mount /virtual=real[:rw]] [message...]
terrarium --resume SESSION_ID [--read-only | --full-access] [--mount /virtual=real[:rw]] [message...]
terrarium run [-e SOURCE | FILE] [--read-only | --full-access] [--mount /virtual=real[:rw]] [--timeout-ms N]
```

`workspace` is the default. In this mode the current working root is mounted at its same absolute path. `--read-only` and `--full-access` are mutually exclusive and apply only to the current invocation; they are never written to the session journal or restored from it. An explicit mount applies to the complete invocation, including every model-selected run and recovery. `--mount /virtual=real` is read-only; append `:rw` to authorize writes. In `--full-access`, `/` maps to the current user's real filesystem view. `terrarium run` is the direct JavaScript entry point and uses the same invocation-only access flags.

## Declared model capabilities

The runtime keeps a small built-in capability declaration for the current examples:

| Model | Input | Output | Implemented request payload |
|---|---|---|---|
| `deepseek-v4-flash` | text | text | text |
| `deepseek-v4-flash-vision-exp` | text, image | text | text |

The second model's image capability is intentionally only a declaration in this phase. Image reading, base64/data URLs, artifact storage, and multimodal request parts are not implemented.

Provider responses are bounded at 4 MiB before JSON parsing. The transport performs no hidden retry; the agent journal authorizes at most one retry for a model step.
