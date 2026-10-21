# Quota Glass

A quiet, local-only Windows tray / macOS menu-bar monitor for ChatGPT Work quotas. Unofficial, independent, MIT-licensed. No telemetry, hosted backend, API key entry, conversation access, or AI requests.

## What works

- Connects to an installed official Codex CLI using its documented `app-server` JSONL protocol over private stdio.
- Uses the existing local ChatGPT login. Optional user-confirmed browser sign-in is managed entirely by Codex.
- Shows **remaining = 100 − usedPercent**, the exact precision reported by the source, primary/secondary windows, reset countdowns, separate extra credits, and last successful refresh.
- Displays all returned buckets using their actual labels. **Sol / Luna / Astra can only be separated when the server supplies distinct buckets.** It never guesses a model mapping or extrapolates decimals.
- Polls every 30–900 seconds (60 by default), refreshes on relevant server events and wake, and retries broken connections.
- Optional low-quota notifications, deduplicated per bucket/window/reset/threshold. No demo notifications.
- Sample/demo mode requires no login. Samples are explicitly labeled and do not represent your account.
- Closing hides to the tray; use the tray menu to quit. No automatic OS startup setting is changed.

## First run

1. Install the official [Codex CLI](https://learn.chatgpt.com/docs/cli) separately. Quota Glass does not bundle a CLI, credentials, or user settings.
2. Open Quota Glass. Choose **Connect existing login**. On first launch it does not read any account until you choose Connect. After a successful connection it remembers to reconnect on later launches.
3. If Codex is not found, select its actual executable with **Choose executable**. On macOS, common Homebrew and `~/.local/bin` locations are also checked. On Windows, select the native `codex.exe`, not an npm `.cmd` wrapper. A Desktop-only installation is not guaranteed to put Codex on PATH.
4. If you have no ChatGPT login, choose **Sign in** and approve the local confirmation, then complete the official browser flow yourself. Codex stores and refreshes the session using its own configured credential store. Signing in may change the account used by your other local Codex clients.
5. Enable alerts only if desired. Your operating system may also require notification permission. Keep the utility running for countdowns and alerts.

If your CLI is installed through npm on Windows, its platform-specific package contains `codex.exe`; select that executable rather than running a command shell. If your installed CLI lacks `account/rateLimits/read`, update it through its official installation route.

## Pace and reset comparisons

Recent burn rates use first-to-last change divided by actual elapsed time over up to 15 minutes, requiring at least 60 seconds of fresh observations. Percentage pace is **percentage points/minute**, while credits use credits/minute. A measured zero rate is distinct from missing data; ETA is only an estimate if the observed pace continues. Reset/top-up, missing readings, long gaps, account changes and reconnects segment the rate history. Rates cover the account, including other devices/tasks, and do not measure active compute time.

Quota increases, reset timestamp changes and credit top-ups create unclassified events. Tag an event as coupon/global/scheduled/unknown and optionally note the model/workload. Local CSV exports include source precision, session IDs, timestamps, wall-clock elapsed time and gaps, with spreadsheet formula injection protection. Comparing similar workloads can reveal patterns; percentages alone cannot prove different absolute token allocations. No reset credits are redeemed by this utility.

History sessions use random identifiers and do not compare across launches/connections. The account schema may expose only email and plan, so it cannot guarantee detection of a silent workspace switch with identical fields. Known account updates start a new session; reconnect after switching workspaces. Historical sessions remain on this computer until you clear history. Avoid sensitive information in notes. CSV exports contain usage and notes: share them only deliberately.

## Run from source

Requires Node.js 24 and npm. On Windows/macOS:

```sh
npm ci
npm run check
npm run build # verifies a source-only app.asar archive, not an installer
npm start
# Explore without accessing any account:
npm run demo
```

## Build installers

```sh
# On Windows:
npm run dist:win
# On macOS:
npm run dist:mac
```

Outputs go to `release/`: Windows NSIS `.exe` (x64 / arm64), macOS `.dmg` and `.zip` (Apple Silicon / Intel). The included GitHub Actions workflow builds on native runners when you push this source to your own repository or run the workflow. It does not publish releases or upload anything to an app store. CI artifacts have GitHub retention limits.

**Unsigned development builds:** No signing certificate or Apple notarization credentials are included. Windows SmartScreen and macOS Gatekeeper may warn or prevent launch. Production distribution needs your own trusted signing/notarization pipeline. Do not disable OS security protections. Only open a build whose source and provenance you trust, using the operating system's supported per-app verification flow where available.

## Privacy and security

See [SECURITY.md](SECURITY.md). Package contents use an explicit allowlist: only application source, icons, LICENSE and package metadata. Preferences contain a CLI executable path, reconnect choice and notification settings. Bounded quota history (up to 3,000 numeric samples and 100 events) is stored locally, including user-entered source tags/workload notes. No account identifiers, tokens, email addresses or raw logs are saved. Demo data is never recorded and saved real history is hidden in demo mode. The CLI independently manages its own data.

A friend uses their own locally installed Codex and their own login. Share a built installer or this clean source archive, **never a copy of your home directory, Codex profile, `auth.json`, credential store, or app user-data directory**.

## Architecture

`src/main/`: Electron trusted process and narrow isolated preload bridge.
`src/core/`: JSONL client, defensive response normalization and alert deduplication.
`src/renderer/`: local HTML/CSS/JS interface, no Node access.
`tests/`: protocol/normalizer tests. `npm run lint` is a syntax check; `typecheck` checks core JS with TypeScript. Native UI and real-account compatibility require testing on each supported OS.

The app-server is spawned with `shell: false`; no external web content can execute in the renderer. Only initialize, read account, read quotas, start/cancel user-requested login methods are allowed. Server requests are rejected. No credit purchase/reset redemption, logout, thread, prompt or command execution endpoint is exposed.

## API limitations and source

Verified 2026-10-03 against [official app-server documentation](https://learn.chatgpt.com/docs/app-server#6-rate-limits-chatgpt) and locally generated TypeScript schema (`codex app-server generate-ts`). Fields can vary by account and CLI version. Missing values stay unknown; an expired countdown waits for fresh source data instead of resetting the quota locally. Extra credits are displayed per returned bucket and are never summed, since repeated bucket balances may share one workspace balance.

The [authentication documentation](https://learn.chatgpt.com/docs/app-server#auth-endpoints) permits continued local/open-source use, recommends Sign in with ChatGPT, and says app-server authentication is not permitted for commercial/hosted services. This project is a local open-source utility, not a hosted service. Recheck official policies before commercializing it or changing its authentication architecture.

## Verification status

See [VALIDATION.md](VALIDATION.md) for exact results. Windows x64 installer/install/uninstall, native UI, tray registration, security and read-only existing-account API checks passed. No login flow or token-file inspection was performed. Builds are unsigned; physical tray/dialog interaction, OS notifications, ARM64 execution and macOS remain unverified.
