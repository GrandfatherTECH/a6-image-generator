# Security Policy

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's private vulnerability reporting feature for this repository:

1. Open the repository's **Security** tab.
2. Choose **Report a vulnerability**.
3. Include affected versions, reproduction steps, impact, and any proposed mitigation.

If private vulnerability reporting is not enabled, contact the repository owner through the private contact method listed on the `grandfathertech` GitHub profile. Do not include API keys, bearer headers, complete provider responses, private prompts, generated images, or local database files in the initial report. Provide the minimum sanitized evidence needed to establish the issue.

There is currently no guaranteed response or remediation service-level agreement. Reports will be assessed for the current source release and default branch.

## Supported versions

Until a stable release line exists, only the latest tagged release and the current default branch are considered for security fixes.

## Sensitive data handled by the application

A6 Image Studio handles API credentials, user prompts, generated images, provider errors, and local history. Security changes must preserve these boundaries:

- API keys stay in the process environment or system keyring and must not enter settings, SQLite, logs, URLs, screenshots, tests, or diagnostics.
- Authorization headers are sent only to the configured API endpoint, never to provider-returned image-download URLs.
- Diagnostic exports exclude prompts, provider response bodies, file paths, preferences, image payloads, internal IDs, and credentials.
- Error bodies are sanitized and size-bounded before persistence.
- Generated output and settings writes use atomic file operations to avoid exposing partial files.

## Dependency reports

Reports about vulnerable Rust dependencies should identify the advisory, affected dependency/version, reachable application path, and available fixed version. A dependency advisory is not automatically exploitable in this application, but it will be evaluated against the compiled feature set.
