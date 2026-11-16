# Wisp fork plan

Build a Mac-style dock for Windows 11 with a local-first runtime, safe launcher actions, and measurable resource use. Start from Wisp main at 104e842. Keep its native Rust/Win32 dock, Direct2D renderer, asynchronous search, taskbar watchdog, and MIT license notices. Use DockHub as a visual reference.

## 1. Security and privacy first

### What the source currently does

| Surface | Observed behavior | Decision for the fork |
| --- | --- | --- |
| Settings updates | Settings checks GitHub automatically when opened outside onboarding. Installation is separately triggered and verifies both executables against the release checksum file. The reviewed v0.5.0 executables matched that file and were unsigned. | Remove the upstream update check, update button, download/install path, and dependencies used only for that path. A fork should never replace itself with upstream Wisp. Install fork builds manually for the first release. |
| Web search and links | Selecting the web result opens a configured search URL in the default browser. The default is Google and the URL includes the query. Settings also has an external project link. | Remove the web-search result and retire its configuration from the UI. Keep explicitly labeled documentation links only if wanted. Normal app launching remains available. |
| Local data | Wisp stores app launch counts and timestamps in history.toml, uses Windows Search for files, and reads the clipboard when the user pastes. | Default launch history off. Provide Clear History and honor the setting across restarts. Keep file search local and clipboard access tied to explicit copy/paste. Keep search text, clipboard contents, file paths, and launch history out of logs. |
| Shell and system commands | The launcher supports explicit command execution and elevation. Power actions include permanent Recycle Bin deletion. | Remove the shell-command and administrator launch actions from the initial fork. Remove destructive power shortcuts until deliberate confirmation is implemented and tested. Resolve retained system tools from the Windows system directory, including Settings' Notepad launcher. |
| Startup and Explorer | Start with Windows writes a per-user startup entry. Taskbar replacement changes Explorer state and runs a watchdog. | Keep startup and taskbar replacement off by default in the fork. Make each an explicit setting. Preserve watchdog recovery when replacement is enabled. |

Evidence for outbound paths is in [Settings startup](settings/src/ui.rs#L279), [update client](settings/src/update.rs#L24), [web search](src/search.rs#L350), and [default search provider](src/default-config.toml#L25). See also [launch history](src/history.rs#L25), [shell command](src/launch.rs#L38), and [Settings Notepad launch](settings/src/system.rs#L153).

The source review found no evidence of intentional telemetry, covert uploads, or malicious code in the inspected paths. The table records actual outbound features and local privileges. Transitive dependencies and process-level traffic still need verification.

### Acceptance checks before using the fork daily

- Inventory network and process-launch APIs in both executables, dependencies, and build scripts. Remove confirmed unwanted outbound or harmful behavior. Audit locked dependencies against current advisories, record findings, and review build scripts and release workflow permissions.
- Capture network activity during startup, Settings, idle, search, paste, and normal dock interactions on Windows. Attribute traffic to the launcher, Settings, the watchdog, or a user-opened external app. Require zero app-initiated internet traffic in the default runtime workflows.
- Test executable and URI handling. Allow expected local launch targets and approved schemes. Block unexpected scripts, elevation paths, and unintended network paths in features that should be local. Verify that removing launcher shell actions preserves ordinary app launching.
- Give the fork distinct executable identity, settings folder, singleton/window identifiers, and startup entry before running it beside upstream Wisp. Existing configs should remain readable where practical. Import settings only by explicit choice. Verify one instance cannot quit, update, or overwrite the other.
- Review the Win32 tray proxy and watchdog as local trust boundaries. Check malformed WM_COPYDATA inputs, sender and window assumptions, timeouts, cleanup, and behavior after Explorer restarts. Keep normal-user execution. Test taskbar restoration after exit, forced termination, sleep, display changes, and Explorer restart.
- For distributed builds, verify release hashes and publisher signatures. Any future updater needs fork-owned HTTPS endpoints, authenticated artifacts, bounded downloads, rollback, and an explicit user check. Manual updates are the initial release workflow.

## 2. Fix confirmed correctness defects

1. **Held Enter can confirm a destructive command.** Require key release and a fresh press to confirm. Clear confirmation on focus loss, query change, Escape, or expiry. Use a fake action handler for tests. Cover keyboard auto-repeat, click, and cancellation. [Confirmation](src/app.rs#L1377) · [Keyboard handler](src/app.rs#L2410) · [Recycle Bin action](src/commands/power.rs#L108)
2. **A fresh search can launch a stale file result.** Bind each file result to its query generation. Keep old rows visible only if their activation is disabled. Preserve selection by result identity when rows reorder. [Search refresh](src/app.rs#L1236) · [Activation](src/app.rs#L1326)
3. **Excluded folders can crowd valid files out of search.** Apply exclusions before the candidate limit where supported, or fetch further pages within a bounded budget. Test a query with 100 newer excluded results ahead of an eligible document. [Ranking](src/files.rs#L191) · [Query limit](src/files.rs#L419)

## 3. Measure, then optimize

Use the same release build settings, PC, displays, refresh rate, DPI, app set, and dock options for comparisons. Establish a baseline before visual work. Use existing [benchmarks](bench/RESULTS.md) and [measurement tooling](scripts/measure-perf.ps1) as starting points. The published Wisp 0.2.0 benchmark measured 67.6 MB working set and 20.2 MB private memory for the launcher on another PC. Treat those figures as historical context.

- Record startup time, launcher first draw, key-to-results latency, animation frame times, idle CPU, and private memory plus working set. Measure the launcher, watchdog, and Settings separately. Compare hidden dock, visible dock, launcher open, and Settings open. Record median and p95 across repeated runs.
- Check memory after repeated launcher openings, searches, icon changes, and display changes. Investigate sustained growth and caches that retain stale icons. Profile the 100 ms dock poll before changing it. Keep timers and drawing inactive when their results are unused. [Dock polling](src/dock.rs#L71) · [Launcher frame pacing](src/app.rs#L352)
- Keep disk and shell lookups off the UI thread where profiling shows stalls. Preserve cancellation and coalescing for rapid search and layout changes.
- Set budgets after measuring the fork baseline. Use a provisional 10% ceiling for idle private memory and p95 launcher latency regression on comparable workloads. Target one frame interval for animation work at the tested refresh rate. Record any deliberate tradeoff with measurements.
- Run formatting, Clippy, and unit tests on Windows with the repository's pinned Rust toolchain. Schedule live benchmarks and Explorer recovery tests separately with explicit approval.

## 4. Build the Mac-style dock in layers

1. **Floating shelf.** Center it above the screen edge. Add a translucent rounded background, clear spacing, app icons, and running indicators. Reuse the existing renderer and per-monitor DPI handling.
2. **Magnification and motion.** Extend the existing hover animation to nearby icons. Keep hit targets stable, stop invalidation after motion settles, and support reduced motion. Compare a simple translucent fill with blur before enabling blur by default. [Hover animation](src/dock.rs#L996) · [Renderer](src/dock/render.rs#L183)
3. **Daily usability.** Add drag-to-reorder, pins, and folders where needed. Preserve keyboard navigation, visible focus, sensible overflow, and screen-reader semantics. Test small screens, mixed-DPI monitors, fullscreen apps, and monitor disconnects.
4. **Optional extensions.** Add native widgets only when needed. Make each opt-in and suspend its work while inactive where practical. Keep the initial feature set free of browser widgets, cloud sync, and credential storage.

## First implementation milestone

Ship the local-first changes, destructive-action safeguards, and safe defaults as a focused first patch. Verify search correctness and establish the performance baseline in the next patch. Start the visual redesign once the privacy and recovery checks pass.
