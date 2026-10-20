# Architecture

## Workspace

- `crates/app`: CLI, first-run/explicit setup, per-instance client construction.
- `crates/tui`: dashboard, onboarding, picker, input/state/rendering, async orchestration.
- `crates/gitlab`: REST client, DTOs, pagination, reads and confirmed write actions.
- `crates/config`: validation, legacy/current TOML parsing, atomic bootstrap and updates.
- `crates/storage`: per-URL OS credentials, environment override, atomic generic JSON storage.

## Identity

The product, executable and Cargo packages are Invocra / `invocra`.
The config/cache directory is `~/.config/invocra`, the OS keyring service is
`invocra`, and the token environment variable is `INVOCRA_GITLAB_TOKEN`.
The 85×13 text composition (ASCII scene and Terminal Gothic glyphs encoded as Unicode Braille)
is embedded in the TUI crate and shared by the setup header and empty log pane. It is centered
without wrapping using the terminal's default foreground and background colors. Smaller areas
show the 28×4 Gothic wordmark alone, then a plain title if even the wordmark cannot fit.
The public source repository is https://github.com/dsszs3333/invocra;
all workspace packages inherit that URL in Cargo metadata.

## Startup and persistence

A missing configuration creates a bundled sample and opens onboarding. Existing configurations
never restart onboarding just because a token is missing: the CLI prints a re-login hint.
`setup` explicitly opens the wizard and merges the chosen instances; `a` does so from the dashboard.
Groups resolve on refresh; explicitly configured projects win duplicate IDs.

Only the active instance has a live `AppState`. Switching tabs persists its snapshots and ref
pins, then loads the target cache. Cache filenames use SHA-256 of the trimmed URL without trailing
slashes; the URL inside the file must match before restoring data. Unbound legacy caches are
left untouched and require manual recovery. Variables, logs, MR lists and transient UI state
are not persisted. A cache is a last-known snapshot, not a full offline GitLab client.

Tokens are keyed per URL; a legacy global credential is never adopted for an arbitrary server.
`INVOCRA_GITLAB_TOKEN` overrides keyring lookup for every configured instance. macOS uses Keychain,
Linux uses a kernel keyring (not persistent across reboot), Windows uses Credential Manager.

## Event loop and requests

Dashboard and onboarding each own a terminal session and one blocking input reader. Both close
the input queue, signal stop and join that reader on normal/error exits before another session starts.
Async network tasks send events to the current loop. An instance-session generation rejects results
from a previous visit to a tab. MR list/head results also carry the panel generation and list scope.

Project snapshots and viewed pipelines share a semaphore of eight project fetches. Auto-refresh
runs every three seconds for active pipelines and skips in-flight requests. Per-project generations
reject stale ref results; replacement reads are cancelled, and a slower full refresh cannot revert
a newer project request. UI clocks redraw every second; log follow requests every second.
Cache writes are debounced to the UI tick and flushed on exit.

State holds the latest project snapshot separately from a viewed history/downstream/MR pipeline.
Actions and logs use that viewed pipeline's actual project ID. Mutation confirmations freeze their
target; merging an MR includes its reviewed source SHA so a later push causes GitLab to reject it.
Write requests are not automatically retried or cancelled when switching tabs.

## HTTP and limits

API requests have a 20-second timeout, do not follow redirects and retry a GET once on HTTP 429,
honouring numeric `Retry-After` up to 60 seconds. Writes have no automatic retry.
Artifact downloads stream to a temporary file and atomically replace the destination on success;
they have a 20-second connect/read timeout without a total download timeout. Up to five redirects
are allowed; another origin must use HTTPS and never receives the GitLab token.

Paginated lists (including jobs and bridges) require advancing `x-next-page` and fail after 1000
pages instead of returning an incomplete success. MR lists intentionally show the latest 50 open
items; pipeline history shows 20. Logs keep at most 2 MiB. API permissions still control available
actions: read-only tokens cannot mutate, and CI/CD variables require Maintainer access.

The dashboard has five panels: projects, branches, pipeline/jobs, log, and merge requests.
Overlays provide variables, pipeline history, environments, tags, actions and confirmations.
Real GitLab/CDN integration, OS credential persistence and release publishing are separate
validation steps; unit tests use synthetic credentials and local HTTP mocks.
