# aduana
LLM gateway written in Rust. A single entry point for your apps to Ollama and other model backends, with request routing, prompt/response logging and Prometheus metrics (latency, tokens, throughput).

> **Implemented today:** authenticated proxy to Ollama, encrypted request log and a web panel. Other backends, routing and Prometheus metrics are not implemented yet.

## How it works

```
client ──Bearer key──▶ aduana (Docker) ──plaintext──▶ ollama (native, on the host)
                         │
                         ├─ encrypts request and response (age), stores them in SQLite
                         └─ web panel: login, decrypts for display
```

For each request, in this order: check the API key, store the encrypted request, forward it to Ollama, store the encrypted response and metrics, answer the client.

## Quick start

Requires Docker and [Ollama](https://ollama.com/download) installed on the host and running.

```sh
docker compose build
(umask 077; docker compose run --rm -T aduana gen-env > .env)
docker compose up -d

# Models are not downloaded automatically. Install any Ollama model on the host:
ollama pull llama3.2:1b
```

`.env` now holds the API key, the panel password and the private key. **Back it up:** without `ADUANA_AGE_IDENTITY`, stored requests cannot be recovered.

## API

`POST /api/chat` and `POST /api/generate` accept the [Ollama API](https://github.com/ollama/ollama/blob/main/docs/api.md) body, with any model installed in Ollama. Answers are returned as Ollama produced them.

```sh
curl http://localhost:8080/api/chat \
  -H "Authorization: Bearer $API_KEY" \
  -d '{"model":"llama3.2:1b","messages":[{"role":"user","content":"Hola"}]}'
```

| Status | Meaning |
| --- | --- |
| 400 | Invalid body, or `"stream": true`: streaming is not supported |
| 401 | Missing or invalid API key |
| 404 | Model not installed in Ollama |
| 413 | Body larger than `ADUANA_MAX_BODY_BYTES` |
| 502 / 504 | Ollama unavailable / timed out |

Requests rejected with 400, 401 or 413 are not stored.

## Web panel

Open <http://localhost:8080> and log in with `ADUANA_ADMIN_USER` and `ADUANA_ADMIN_PASSWORD`.

## What is protected

| Data | In the database |
| --- | --- |
| Request and response bodies | Encrypted with age (X25519) |
| Client name, model, endpoint, status, dates, sizes, metrics | In clear |

The private key lives in `.env`, never in the database, so a stolen database or backup reveals no prompts. The running service does hold the key in order to decrypt for the panel, so someone who compromises the host or the container can read them.

A copy of a body can also be decrypted without aduana, using the `age` CLI and the key.

The service is published on `127.0.0.1` only. There is no TLS: to expose it beyond localhost, put an HTTPS reverse proxy in front and set `ADUANA_COOKIE_SECURE=true`.

Ollama has no authentication of its own, and running on the host, it can be called directly by anything on that machine, bypassing aduana and its log. Keep its default `127.0.0.1` binding: do not set `OLLAMA_HOST=0.0.0.0`, which would open it to your network.

## Metrics per request

Latency, Ollama time and model load time, input and output tokens, tokens per second, and request and response sizes.

CPU, memory and temperature are not recorded: from inside a container on macOS, only Docker's Linux VM is visible, not the machine running Ollama.

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `ADUANA_API_KEYS` | required | `name:key,name2:key2`; keys of at least 32 characters |
| `ADUANA_ADMIN_USER` | `admin` | Panel user |
| `ADUANA_ADMIN_PASSWORD` | required | Panel password, at least 12 characters |
| `ADUANA_AGE_IDENTITY` | required | Private key (`AGE-SECRET-KEY-1…`) |
| `ADUANA_COOKIE_SECURE` | `true` | Send the session cookie over HTTPS only |
| `ADUANA_OLLAMA_URL` | `http://host.docker.internal:11434` | Ollama address, seen from the container |
| `ADUANA_OLLAMA_TIMEOUT_SECS` | `600` | Limit for one Ollama call |
| `ADUANA_MAX_BODY_BYTES` | `33554432` | Maximum request size |
| `ADUANA_PORT` | `8080` | Host port (Compose) |

Run `docker compose up -d` after editing `.env`. Stored requests are never deleted automatically.

## Development

```sh
cargo test
```
