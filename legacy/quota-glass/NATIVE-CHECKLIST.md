# Before sharing an installer

Run on both supported platforms and document architecture/OS/CLI versions:

1. Fresh `npm ci`, `npm run check`, `npm run build`, then native `dist:win` / `dist:mac`.
2. Install, open demo, resize, switch appearance and hide/reopen from tray/menu bar. Verify text is legible and countdowns use current dates.
3. Connect your own official Codex login with explicit user permission. Compare returned bucket percentages, reset times and credits against official UI; do not assume bucket-to-model equivalence.
4. Test no CLI, signed-out/API-key account, browser login cancel, failed login, repeated buttons, connection loss/recovery and sleep/wake.
5. Observe at least two minutes of quota updates. Check rates, rounding limitations, stale state and reset/top-up segmentation; do not spend credits merely to test.
6. Enable notifications intentionally; check OS permission, threshold crossing and duplicate suppression. Disable again if not wanted.
7. Confirm history persists after relaunch, manual labels survive, CSV can be opened without formulas, and clear history actually removes old records after restart.
8. Check separate workspace/account behavior. The read-account schema does not guarantee a unique workspace identifier; reconnect when switching.
9. Sign/notarize using your own secure release process. Verify signature and installer contents. Keep certificates, passwords and account data outside this repository.
10. Recheck the build-dependency security advisory in VALIDATION.md before public release. Test installer/uninstaller and architecture-specific behavior before distributing.

The initial source delivery has NOT passed these native steps. No live login is authorized merely by running unit tests or a demo.

## Windows x64 milestone

See VALIDATION.md for completed native/install/uninstall and existing-account read-only checks. ARM64 and macOS are not executed; physical tray/menu/file dialogs, OS notifications and signing remain to be checked. No new login was started and no OS auto-start was configured.
