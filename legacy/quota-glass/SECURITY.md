# Security

Quota Glass is a local application with a trusted Electron main process and a sandboxed, context-isolated renderer. Renderer navigation, popup creation, remote network requests and permission requests are denied. Values are rendered as text. IPC is restricted to the main window's main frame and a small method allowlist. Codex is launched directly without a shell, through stdio; no network listener is opened by Quota Glass.

The main process reads the current account only to verify a ChatGPT auth mode; account details never reach the renderer or a log. No auth files are read. No token, API key, password, email or raw stderr is stored. A bounded, numeric quota history and optional user-written notes are persisted locally; notes should not contain personal or secret information. CSV export requires an explicit user action and file destination. History clearing asks for confirmation and only succeeds after disk replacement succeeds. RPC failures expose generic messages, not raw response bodies. Login URLs are restricted to HTTPS on exact official hostnames and handed to the system browser. Login requires an explicit native confirmation and browser approval. The renderer receives only normalized quota fields and safe settings.

The user-selected Codex executable is trusted code. Select only an official installation. Quota Glass does not download, update, install, or verify that binary's signature for you. Codex has its own local configuration, credentials and network behavior, outside this utility's renderer restrictions. Account refreshes may cause Codex to refresh its existing credentials normally.

Threat limits: a compromised OS/user session, modified CLI or malicious local executable cannot be isolated by this utility. Sharing screenshots can expose usage/credits. Notification text appears through the OS and may be visible on the lock screen; alerts are disabled by default.

No secrets, signing materials, personal data, account fixtures or credentials belong in this repository. Report security concerns privately to the repository maintainer before public disclosure. There is no hosted reporting endpoint in this source-only initial project.

## Packaged executable hardening

Windows packages disable Electron RunAsNode, NODE_OPTIONS and Node CLI inspector fuses, require app.asar, and validate its embedded integrity. These measures do not replace code signing: the current installer and app are unsigned. Build-only dependency isolation and its remaining unpatched advisory are documented in VALIDATION.md. Demo history writes/exports are denied in the main process, and stale observations suppress rates and ETA.
