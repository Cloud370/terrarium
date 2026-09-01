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

The binary does not load `.env` files. Credentials must already be present in the process environment or be supplied by an external secret manager. Reads use the operating-system user's view, so keep secret files outside directories the model is asked to work in.

One diagnostic variable is supported across both configuration paths: setting `TERRARIUM_LLM_DEBUG` to any value other than `0` dumps every request body and each decoded SSE event to stderr. The dump shows no credential values — headers stay out of the log — but it does contain the full prompt and reasoning payloads, so it is a local debugging tool, not something to leave on or paste into shared logs.

## Filesystem and process modes

The model-driven agent is the default command:

```sh
terrarium [--config PATH] [--profile NAME] [--read-only | --full-access | --allow-write DIR|FILE]... [--allow-exec NAME]... [--offline] [--max-steps N] [--run-timeout-ms N] [message...]
terrarium --resume SESSION_ID [--read-only | --full-access | --allow-write DIR|FILE]... [--allow-exec NAME]... [--offline] [message...]
terrarium run [-e SOURCE | FILE] [--read-only | --full-access | --allow-write DIR|FILE]... [--allow-exec NAME]... [--offline] [--timeout-ms N]
```

`planned-write` is the agent default: each run's writes and commands require preauthorization through the `access` block (see [protocol.md](protocol.md)), with targets already covered by an `--allow-write` scope or executables covered by an `--allow-exec` name never prompting. `--read-only` denies every write and every process launch. `--full-access` removes the scope check, keeping only path validation and the current operating-system user's own permissions — it is the explicit trusted path for debugging, not a privilege escalation; it does not bypass OS permissions. `--allow-write` may be repeated and accepts one existing absolute directory (recursive prefix) or file (exact target) per flag; it cannot be combined with `--read-only` or `--full-access`, and combining those two is itself an error. Mode, write scopes, and exec grants apply only to the current invocation; they are never written to the session journal or restored from it.

`--allow-exec NAME` may be repeated, applies in `planned-write` (and direct `terrarium run` under `--allow-write` scopes), and matches the resolved executable identity — a bare name resolves through `PATH`, an absolute path through canonicalization — covering any argv for both `host.proc.exec` and `host.proc.spawn`. One warning belongs here: an executable that loads project code — build tools (`cargo`, `npm`, `make`) as much as interpreters (`sh`, `node`, `python`) — turns the workspace into its program, and the model can write into the workspace through authorized writes; allowing such an executable approaches full trust for it. There is no blacklist; the approval display and this rule are the defense.

`--offline` disables `host.net.fetch` for the whole invocation: every request fails closed with a visible error. It composes with every mode.

Direct `terrarium run` defaults to `read-only`: `--full-access` is the explicit trusted path, and `--allow-write DIR|FILE` scopes — or `--allow-exec NAME` grants alone — switch the invocation to `planned-write` with writes confined to those scopes (denied entirely without `--allow-write`) and commands confined to `--allow-exec` grants; there is no model access block in direct mode.

## Request payload

Requests are text-only; image file reading, base64/data URLs, artifact storage, and multimodal content parts are not implemented.

Provider responses are bounded at 4 MiB before JSON parsing. The transport performs no hidden retry; the agent journal authorizes at most one retry for a model step.
