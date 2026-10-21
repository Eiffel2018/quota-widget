# Windows validation — 2026-10-03

Windows 10 x64, build 19045 (user computer Win2021). Official portable Node 24.21.0 (archive SHA256 checked), npm 11.19.0, Electron 44.5.1, electron-builder 26.15.3, official installed Codex CLI 0.156.1.

- Final source checks passed: syntax, core typecheck, **57/57 tests**. Windows path assumptions in tests and source-ASAR verification were corrected. The source archive now includes LICENSE.
- Native NSIS build completed with exit 0: x64, ARM64, and combined installers. **Only x64 was executed on this computer; ARM64 and macOS remain untested.**
- x64 installer: silent installation in a dedicated QA directory exited 0; installed app.asar equals the validated package. Installed executable demo, theme, refresh, hide/second-instance reopen, sandbox bridge, CSP and screenshot capture passed. Temporary installation was removed after testing (see release/evidence). No OS auto-start was configured.
- Native Electron source UI with isolated synthetic Codex stdio: Traditional Chinese text visually inspected; light/dark demo, min/wide resize, real Windows tray registration, hide/close/reopen through tray callbacks, settings and refresh passed. Synthetic observations over 125 seconds produced percentage-points/min, credits/min and conditional ETA; reset restarted the rate segment.
- Focused native history/security continuation passed: reset/top-up evidence, coupon tags, CSV formula protection, clear cancel/confirm persistence, offline/reconnect, invalid executable, demo isolation, unauthorized sender/subframe IPC, remote fetch and popup denial. Native file dialogs were replaced by deterministic destinations/choices in the test harness; physical dialog interaction and physical tray-menu mouse selection remain manual checks.
- Existing official login was inspected through account/read with refreshToken:false only. No login flow was started; no token files were inspected or credentials logged. Native app quota windows, percentages, reset timestamps and credits matched a fresh documented account/rateLimits/read response. One bucket was returned; no independent model buckets were invented. No AI work or credit spending was requested. Real-usage screenshots were not saved.
- Fixed stale UI: offline/expired observations suppress both rates and ETA. Failed manual tag writes now return errors and restore the prior tag. Demo history mutation/export is blocked in main, independently of disabled UI controls.
- Packaged executable fuses verified: RunAsNode, NODE_OPTIONS and Node CLI inspector disabled; embedded ASAR validation and OnlyLoadAppFromAsar enabled. All packaged app source and LICENSE compared byte-for-byte; x64/ARM64 PE architecture checked. No node_modules, user profiles, credentials, QA hooks or sessions are inside app.asar.
- Installer and app are **NotSigned**. Builder's signing progress messages do not mean a valid signature exists. No certificate, signing service or public publishing was used.
- Production dependency audit: 0 vulnerabilities. Full build audit: 8 high findings through unpatched http-cache-semantics GHSA-ch52-4w7c-c8xp; official advisory still lists no fix. Built using dedicated unauthenticated npm/Electron/builder caches, with no shared authenticated download cache. This is isolation, not a patch. Recheck before a production release.

Remaining manual/platform checks: actual OS notification delivery (alerts left off), physical tray/menu and file-dialog interaction, sleep/wake, workspace-switch identification, ARM64 execution, macOS build/install/signing/notarization, and comparison against the official graphical UI. Coupon/global labels record observations and cannot establish causation or double token capacity.

The first broad native QA attempts exposed asynchronous test-harness waits and a collapsed details panel; those incomplete reports are retained outside the source. The observed 125-second numeric evidence was retained, and focused history/security and live-account runs then passed.

## Previous Linux milestone


Validated in a Linux cloud workspace, 2026-10-03. Node.js 24.19.0; npm 11.9.0. Official app-server TypeScript schema generated without account access using Codex CLI 0.159.2.

## Passed

- `npm run check`: JavaScript syntax checks, TypeScript check of core JavaScript, deterministic unit/integration tests. Main-process tests run the actual code with inert Electron/RPC/filesystem boundaries; renderer tests use jsdom. Final aggregate suite: 54 tests.
- Protocol initialization/correlation, partial JSONL, malformed scalar/null responses, timeout/disconnect, safe generic errors, stream errors and method allowlist.
- Quota normalization, unknown fields, real precision, separate credits, notifications and unsafe numeric balances.
- Irregular time intervals, observed zero rates, missing data, stale data, resets/top-ups and bounded recent history.
- Local history bounds/restoration, session isolation, reset tagging, CSV formula protection, wall-clock gaps and timestamp rollback.
- Account and reconnect races, cancelled/stale login flows, hostile auth URLs, history write/rename failure and rollback, demo isolation.
- Renderer setup, bridge calls, untrusted label escaping, demo/state transition behavior.
- `npm run build`: clean, allowlisted `release/app.asar` source archive, with archive contents checked. This is NOT a standalone executable or installer.

## Not yet verified

- Windows/macOS native installers, menu-bar/tray integration, OS notifications, code signing, Apple notarization, or installation/uninstallation.
- Real-account quota data or successful ChatGPT login. No credentials were read or copied and no OAuth flow was started during development.
- Visual screenshot inspection: headless Chromium exited 134 with `socket() failed: Operation not permitted` in this sandbox. Cloud browser disallowed file URLs and returned `net::ERR_BLOCKED_BY_CLIENT` for the local HTTP preview. DOM behavior checks are not a substitute for native visual QA.

## Packaging attempts

An Electron unpacked Linux package attempt downloaded the runtime, then failed with `EROFS: read-only file system, mkdir '/home/agent/.cache/electron'`. The writable-cache retry (`XDG_CACHE_HOME=/tmp/quota-cache ELECTRON_BUILDER_CACHE=/tmp/quota-builder npm run pack:native`) succeeded with exit code 0 and generated `release/linux-unpacked`. This validates actual Electron packaging on Linux only; the Linux artifact is not a supported-platform deliverable. Even a successful Linux unpacked build does not verify either supported native platform. Windows/macOS builds are configured in `.github/workflows/build.yml` but no external CI job has been started and no repository has been published.

## Dependency audit

`npm audit --omit=dev` reported zero vulnerabilities. A full audit of the pinned electron-builder 26.15.3 build toolchain reported eight high-severity transitive findings, all through unpatched `http-cache-semantics` <=4.2.0 (registry latest 4.2.0), advisory [GHSA-ch52-4w7c-c8xp](https://github.com/advisories/GHSA-ch52-4w7c-c8xp). The path is electron-builder → app-builder-lib → @electron/get → got → cacheable-request → http-cache-semantics. It concerns cross-user cached-response disclosure while downloading build assets. These packages are not used by the application at runtime.

The suggested builder downgrade to 26.5.0 was tested and rejected: it introduced additional vulnerabilities, including critical tar findings. The latest pinned builder was restored. Do not interpret the production-only audit as an all-dependencies clean bill of health. Build in a fresh isolated runner with no account data or personal credentials; avoid shared authenticated caches. The provided workflow uses read-only repository permission and no signing secrets. Recheck/fix the build-chain advisory before a production release.

No paid service, public repository, installer distribution, credentials, or live account was used.
