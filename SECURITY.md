# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities **privately** — do not open a public
issue.

- **Preferred:** [GitHub Security Advisories](https://github.com/tupical/daruma/security/advisories)
  (private vulnerability reporting).
- If you cannot use GitHub advisories, contact the maintainers through the
  channels listed on <https://github.com/tupical> and ask for a private
  reporting channel.

Please include:

- affected version(s) and component (CLI, MCP server, plugin, docs);
- steps to reproduce or a proof of concept;
- impact assessment (what an attacker can achieve).

## Disclosure policy

- We follow coordinated disclosure: please give us time to fix before any
  public disclosure.
- We aim to acknowledge reports within **3 business days** and provide an
  initial assessment within **7 days**.
- We aim to release a fix for confirmed vulnerabilities within **90 days**;
  if that is not possible, we will share the timeline and mitigations.
- Fixed vulnerabilities are disclosed in the release notes / changelog, with
  credit to the reporter unless they prefer to stay anonymous.

## Supported versions

Daruma is pre-1.0. Only the latest release receives security fixes; older
versions are not supported. Upgrade to the latest release when a fix is
published.

| Version | Supported          |
| ------- | ------------------ |
| 0.3.x   | :white_check_mark: |
| < 0.3   | :x:                |

## Scope

In scope: the `daruma` CLI, the MCP server, and the bundled agent plugins in
this repository. Out of scope: vulnerabilities in third-party MCP clients or
hosts (Claude Code, Codex, Cursor, etc.) — report those to their vendors.
