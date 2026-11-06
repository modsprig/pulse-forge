# Security Policy

PulseForge runs with your privileges and observes other processes. Security and
privacy are release gates, not add-ons. The full model lives in
the private security model (`docs/.local/security-model.md`, not distributed).

## Supported versions

Pre-1.0: only the latest `main` and tagged releases receive security fixes.

## Reporting a vulnerability

**Please report privately** — do not open a public issue for vulnerabilities.

1. Use GitHub's *Report a vulnerability* form in the Security tab, or
2. email the maintainers listed in the security advisory contacts.

Include reproduction steps, affected component (CLI / daemon / MCP / GUI),
privilege context (normal user / elevated), and impact. You will receive an
acknowledgement within 72 hours. Please avoid public disclosure until a fix and
advisory are published. We credit reporters in the advisory unless you prefer
otherwise.

## Threat model summary

| Asset | Adversary | Control |
|---|---|---|
| Other users' process metadata | same-machine unprivileged user | OS ACLs; least-privilege handles; never bypass |
| Trace files (paths, args, symbols) | other local users | private storage dir; no world-readable defaults |
| MCP-exposed tools | untrusted agent host | daemon-side policy; launch allowlist off by default |
| Imported traces / symbols | malicious files | bounded parsers, checksums, no auto network fetch |
| System tracing sessions | buggy/malicious tooling | only sessions PulseForge created are ever stopped |

## Hard rules

- No automatic admin elevation, ever.
- No implicit shell execution (`cmd /c`, `bash -c`) for any launch feature.
- No remote listener enabled by default; MCP is stdio-only in v1.
- No debug-symbol download without explicit opt-in.
- Stopping a recording never kills the target process.
