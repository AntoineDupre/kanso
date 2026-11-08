# Security policy

Kanso is a single-user kanban board maintained by one person on a best-effort
basis. Security reports are welcome.

## Reporting a vulnerability

Report privately with GitHub's [Report a vulnerability form][report] on the
repository's **Security** tab. Do not open a public issue, discussion or pull
request that describes the problem before it is fixed.

[report]: https://github.com/dupremathieu/kanso/security/advisories/new

Include as far as you can:

- the affected version or commit (`kanso --version` or the image tag);
- the deployment shape: loopback, nginx with TLS, container;
- reproduction steps or a proof of concept;
- the impact and how you assessed it.

Never send real credentials, agent tokens, session cookies or live data.

## Supported versions

Only the newest `v*` release and the matching container image tag receive
security fixes; `latest` follows that release. Older versions are not
maintained.

## Process

- Acknowledgement within a week, then triage.
- A fix and a published GitHub security advisory; a CVE is requested when the
  impact warrants one.
- Coordinated disclosure: we agree on a publication date before details become
  public.
- There is no bug bounty.

## Scope

In scope: authentication and sessions, agent tokens and their scopes, one-time
secret handoffs, the web UI (including Markdown rendering and stored XSS), the
CLI and agent skill, and the shipped container and deploy templates.

Out of scope: vulnerabilities in third-party dependencies (report them upstream;
the release SBOM lists what is bundled), issues that require an already
compromised host or a malicious administrator, self-XSS, an attacker who already
holds the password, a session cookie or a token, hardening and TLS choices on a
self-hosted deployment, social engineering and physical access.

## EU Cyber Resilience Act

This document is the project's cybersecurity policy and vulnerability reporting
channel for the purposes of the [EU Cyber Resilience Act (Regulation
2024/2847)](https://eur-lex.europa.eu/eli/reg/2024/2847/oj/eng). Confirmed
vulnerabilities are handled and disclosed as described above. Security-relevant
design choices are recorded in [docs/DECISIONS.md](docs/DECISIONS.md); operator
responsibilities are in [docs/DEPLOY.md](docs/DEPLOY.md).
