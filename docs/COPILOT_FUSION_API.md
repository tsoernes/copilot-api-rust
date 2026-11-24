# GitHub Copilot Fusion: observed browser API

## Status and confidence

This is a **provisional observation of GitHub's web application's internal API**, not a published or supported REST contract. It was derived from the user's local `github.com.har` export on 2026-11-24. The HAR was filtered locally; prompt text, response content, IDs, cookies, and token values were not included here.

GitHub's Fusion announcements describe model routing/orchestration and effort profiles, but do not document this HTTP API. The observed paths include `cmc_internal`, so callers should expect them to change. Do not equate Fusion with a standalone model ID or assume this is interchangeable with `/responses`.

## Observed task creation

Both observed creation requests went to `api.github.com` and returned **201 Created**:

```http
POST /cmc_internal/api/agents/tasks
Content-Type: text/plain;charset=UTF-8
Copilot-Integration-Id: copilot-web
X-Client-Application: copilot-cloud-web
```

The body was JSON text despite the HAR's `text/plain` content type. Two variants were observed:

```json
{
  "environment_id": "<redacted>",
  "prompt": "<redacted>",
  "compute": { "scope": "<redacted>" },
  "reserve": true
}
```

and

```json
{
  "environment_id": "<redacted>",
  "prompt": "<redacted>",
  "model": "auto",
  "compute": { "scope": "<redacted>" }
}
```

The reservation-shaped response included `reserved` and `reservation_expires_at`; the other response was a task record. These two examples do not establish which fields are required, whether reservation is a separate step, or whether `model: "auto"` uniquely denotes Fusion.

## Observed related routes

| Method | Host/path | HAR result | Notes |
|---|---|---:|---|
| `GET` | `api.enterprise.githubcopilot.com/agents/swe/models` | 200 | JSON with `object` and `data` array (23 entries in this capture); the list included `id: "auto"`. |
| `GET` | `api.github.com/cmc_internal/api/agents/environments/{environment_id}` | 200 | Environment detail; this capture does not establish how to enumerate/select an environment. |
| `GET` | `api.github.com/cmc_internal/api/agents/environments/{environment_id}/connect` | 200/202 | Connection/ready-state polling appeared in the browser flow. |
| `GET` | `api.github.com/cmc_internal/api/agents/grouped-tasks` | 200 | Grouped task listing. |
| `GET` | `api.github.com/cmc_internal/api/agents/pins` | 200 | Pinned-agent/task metadata. |
| `GET` | `api.github.com/cmc_internal/api/agents/tasks` | 200 | Task-list response. |
| `GET` | `api.github.com/cmc_internal/api/agents/tasks/{task_id}` | 200 | Task detail, including status/state and session metadata. |
| `GET` | `api.github.com/cmc_internal/api/agents/tasks/{task_id}/events` | 200 | JSON shaped as `{ "events": [...], "total": number }`. |
| `GET` | `api.github.com/cmc_internal/api/agents/sessions/{session_id}/events` | 200 | Session event stream/history. |

The task-detail schema included fields such as `status`, `state`, `compute`, `sessions`, `artifacts`, and timestamps. Session objects included `model` and `reasoning_effort` keys, but this capture did not establish usable values for those fields.

The HAR's models response had 23 entries. A separate authenticated read-only GET on 2026-11-24 returned 18 models. Items exposed keys including `id`, `name`, `vendor`, `version`, `billing`, `capabilities`, `model_picker_enabled`, `model_picker_category`, and `supported_endpoints`; the list included `auto`. Counts are account-/time-dependent, not a static catalog.

## Authentication and compatibility gaps

- CORS preflights listed `authorization`, `copilot-integration-id`, and `x-client-application` as requested headers. The observed request headers included `Copilot-Integration-Id: copilot-web` and `X-Client-Application: copilot-cloud-web`.
- A separate **read-only** GET of `https://api.enterprise.githubcopilot.com/agents/swe/models` with the project's stored GitHub OAuth token in a Bearer authorization header returned HTTP 200, `application/json`, and an `{ "object", "data" }` response with 18 models. The token value was not recorded. This verifies model-catalog authentication only.
- The exported HAR did not retain the Authorization value for task/environment requests. Task list, task detail/events, environment detail/connect, and especially task-create authentication remain unverified. The experimental proxy currently uses the GitHub OAuth Bearer token and the observed integration headers for those routes; this is an implementation assumption, not a verified upstream contract. No live task-create POST was sent.
- The browser's selected Fusion Standard profile did not appear as a distinct field in the observed task-creation body. The route does not add a model/effort field, so mapping Standard/High/Max remains unresolved.
- These task endpoints can start remote coding-agent work. Task creation is an explicit POST only, requires a configured local `COPILOT_API_KEY`, uses fixed upstream hosts/paths, and does not retry or automatically create tasks. Request/response bodies and credentials are not logged.

## Implementation decision

The Rust gateway exposes the following **experimental** routes under `/experimental/fusion/`:

| Local route | Upstream mapping |
|---|---|
| `GET /models` | Copilot credential's `api_base` + `/agents/swe/models`; GitHub OAuth Bearer token. This read-only auth path was verified. |
| `GET, POST /tasks` | `https://api.github.com/cmc_internal/api/agents/tasks` |
| `GET /tasks/{task_id}` and `GET /tasks/{task_id}/events` | Corresponding fixed `api.github.com/cmc_internal/api/agents/tasks/...` paths |
| `GET /environments/{environment_id}` and `/connect` | Corresponding fixed `api.github.com/cmc_internal/api/agents/environments/...` paths |

All local Fusion routes require `COPILOT_API_KEY`. Upstream task bodies must be JSON objects; the submitted bytes are passed through as `text/plain;charset=UTF-8` to match the browser observation. Queries are limited in size and forwarded only to the route's fixed host/path. Responses are streamed and upstream status codes are preserved. Mock-based tests verify request mapping and header handling; task-route authentication and Fusion effort selection are not verified. Do not treat this undocumented internal API as a stable or supported contract.
