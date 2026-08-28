# Configuration

## Current configuration

The current binary uses one OpenAI-compatible chat-completions connection configured through environment variables:

| Variable | Meaning |
|---|---|
| `TERRARIUM_LLM_API_KEY` | API key used by agent mode and `host.llm` |
| `TERRARIUM_LLM_BASE_URL` | Chat-completions endpoint; defaults to DeepSeek's endpoint |
| `TERRARIUM_LLM_MODEL` | Model ID sent upstream; defaults to `deepseek-v4-flash` |
| `TERRARIUM_LOG_RUNS` | Set to `1` to print executed run source to stderr |

The binary does not load `.env` files. Credentials must already be present in the process environment or be supplied by an external secret manager. The sandbox cannot read these variables. Keep secret files outside mounted directories.

Filesystem authorization is separate from LLM configuration and is declared at launch:

```sh
--mount /virtual=/real/path
--mount /virtual=/real/path:rw
```

The first form is read-only; the second permits `host.fs.write` below that virtual root.

## Declared model capabilities

The runtime keeps a small built-in capability declaration for the current examples:

| Model | Input | Output | Implemented request payload |
|---|---|---|---|
| `deepseek-v4-flash` | text | text | text |
| `deepseek-v4-flash-vision-exp` | text, image | text | text |

The second model's image capability is intentionally only a declaration in this phase. `host.llm.call` currently accepts text strings and sends text-only chat-completions messages. Image reading, base64/data URLs, artifact storage, and multimodal request parts are not implemented.

Provider responses are bounded at 4 MiB before JSON parsing. Error messages report status or a short parsing/read failure and do not echo provider response bodies.

## Future direction, not implemented

A future configuration file may define multiple named connections and model metadata. That design is deliberately not part of the current binary contract. When it becomes necessary, it should preserve these rules:

- credentials are environment references, never plaintext configuration values;
- a model ID is the exact ID sent upstream, not a hidden alias;
- endpoint, credential, and model selection are explicit and traceable;
- modality declarations are checked before a request shape is selected.
