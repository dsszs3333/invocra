# Security Policy

## Supported versions

Only the latest released version receives security fixes. The project is
pre-1.0, so APIs and behaviour may change between releases.

## Reporting a vulnerability

Please report security issues **privately**, not via public issues:

- Email: dsszs3333@gmail.com

Include steps to reproduce and the affected version. You can expect an initial
response within a few days.

## Handling of secrets

invocra works with GitLab Personal Access Tokens:

- Tokens are stored in the OS keyring (macOS Keychain / Linux kernel keyring / Windows
  Credential Manager), not in plaintext config files.
- As a fallback, a token may be supplied via the `INVOCRA_GITLAB_TOKEN` environment
  variable — avoid this on shared machines.
- Linux uses the `linux-native` kernel keyring backend. Its entries do not survive
  a reboot and may expire with the user session. Run `invocra auth login --url <url>`
  again when needed; project/group selections are preserved. Secret Service is not enabled.
- Tokens are never written to logs or to the offline cache.

Token scope, least privilege:

- `read_api` is enough for read-only use (just viewing dashboards).
- `api` is needed only for the write actions (pipeline/job control, tags, MR approval/merge).
- Prefer a short expiration and revoke the token if you suspect exposure.
- For a single project, a **project access token** limits access to that project
  rather than your whole account.

The token is sent only to the configured GitLab instance, over HTTPS by default
(plain HTTP requires an explicit `allow_insecure_http` opt-in per instance), and
authenticated API requests never follow redirects. Artifact downloads handle up
to five redirects explicitly: the token is sent only to the original origin, and
a different origin must use HTTPS and receives no token. Downloads replace the
destination only after the archive has been received successfully.
