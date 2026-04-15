# Security Policy

## Reporting a vulnerability

If you find a security issue in kei, please report it through [GitHub's private security advisory](https://github.com/rhoopr/kei/security/advisories/new). This keeps the details private until a fix is ready.

If you can't use GitHub advisories, email robhooper@gmail.com with "kei security" in the subject.

## What counts

- Credential leakage (passwords, session tokens, cookies exposed in logs, temp files, or error output)
- Authentication bypass or session hijacking
- Data corruption or silent data loss
- Path traversal or arbitrary file writes outside the download directory

General bugs (crashes, incorrect downloads, UI issues) are fine as regular [GitHub issues](https://github.com/rhoopr/kei/issues).

## Response

kei is a solo-maintained project. I'll acknowledge reports within a few days and aim to ship fixes promptly, but there's no SLA. I won't publicly disclose details until a fix is released.

## Automated scanning

CI runs `cargo audit` on every pull request to catch known vulnerabilities in dependencies.
