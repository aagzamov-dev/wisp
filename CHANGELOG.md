# Changelog

User-facing changes in each release. Each release on GitHub lists these notes and its pull requests.

## Unreleased

## 1.0.0 - 2026-11-24

- Add a per-user Windows Setup with a Start menu entry and an uninstall option in Windows Settings.
- Keep using **Settings → About** to check for and install Wisp updates.

## 0.7.0 - 2026-11-24

- Add Soft Rise, Bottom Reveal and Soft Scale dock animations.
- Add a dock-only gradient with four built-in themes, editable colors, direction and transparency, a live Settings preview and saved custom themes.
- Reuse the calendar grid when focus or selection moves within the displayed month, and skip redraws when wheel scrolling reaches a limit.
- Show four audio-reactive bars in the top-bar island, tinted with the cover art when readable, and show the track title for five seconds when a track appears or changes.

## 0.6.0 - 2026-11-23

- Check for and install stable Wisp updates from **Settings → About**. Optional automatic updates are off by default.

- Replace the alternate Remote Desktop layout with a pause that restores the Windows taskbar and brings Wisp back at the physical console. Use one layout and animation path when Wisp is active.

- Keep top-bar tray clicks tied to the app you pressed, let app-owned menus receive clicks, and add **Show taskbar** to the top-bar tray chevron’s context menu.
- Add a **Recently installed** Start page to the right of **Pinned**, reachable by horizontal scrolling or the page tabs.

- Keep dock window previews in opening order, oldest first, across hovers and focus changes.
- Add an editable global Quit Wisp shortcut under Settings → General → Shortcuts. The default is Ctrl+Win+Alt+X.

- Replace the dock's Windows panes with a font-independent Mac-style Command mark, preserving the Start action, hover behavior and theme color.

- Give compact search the same Shared material background and reversible opening/closing fade as Start, retaining search actions and the shared-dock capture path. Keep top-bar tray icons at full opacity in cached frames so an initially invisible opening frame cannot leave a blank tray. Native appearance remains unverified under build/unit-only checks.

- Add a background transparency percentage and optional blur for Transparent and Glass under Appearance → Shared material. Glass uses the same base recipe as Transparent with blur enabled by default. Transparent defaults to 85% and Glass to 36%, and explicit settings are shared between the presets. Dock, Start, Controls, tray, previews and Wisp menus follow these settings. The top bar and island have independent settings. Rounded clear menus retain their input shape, and accessibility and failed requested effects use the opaque fallback.


- Make Start app tiles square, tighten Home row spacing and height, and give Profile the same square icon button as Settings and Power. Retain the current app artwork size, search/surface budget, app menus and keyboard actions; native appearance remains unverified.


- Match the top-bar tray blur clip to its existing 12 DIP rounded foreground and border. Native appearance still requires desktop verification.


- Share bounded animation math across Start, dock, tray and menu surfaces. Prevent Start closing from painting a duplicate dock; fade its full native window and reuse prepared pixels. Keep menu entrance dimensions fixed, animate bottom-tray dismissal/reversal, and publish island pixels before matching blur. Keep the island material in its top-bar owner group and retain a readable Glass tint. Reduce Start pin artwork by 40% without shrinking hit targets. Native smoothness, glass appearance and the reported island tint still require desktop verification.


- Keep island glass paired with its completed foreground frame and revalidate cached companion visibility/Z-order after activation. Reduce unrelated top-bar redraws, honor elapsed animation time and requested Controls duration, bound tray-menu motion, and retain faint Start content through native-glass transitions. Native desktop smoothness and the recorded blank-island cause remain unverified by live tests.

- Make Start's app menu a compact Shared-material glass popup with cached short fades, keyboard navigation, scrolling on short displays, and menu-scoped UI Automation. Retain all eight actions, disabled reasons and snapshot validation; native screen-reader, desktop and RDP behavior remain unverified by live testing.

- Open Start app menus through right-click or Menu/Shift+F10 without visible three-dot buttons. Retain Start/Dock pinning, Copy path, Windows Installed apps access, unavailable actions, glass transitions and keyboard accessibility. Resolve desktop shortcut metadata in the background for administrator/file-location actions, and compact searches with few results while retaining home and live surfaces.

- Refine Start with a 12-pin grid, selected-app actions, persistent utility controls and independent Center/Bottom placement. Compact Controls retains its actions and subpages. Add a shared Glass/Frosted/Solid material preference across the dock, Start and Wisp-owned menus, retaining legacy values, cached fades and accessibility fallbacks. Start opens and closes in 220/180 ms.

- Keep native dock context menus above the dock, including during periodic status updates and tray-menu completion.

- Prepare Control Center before showing it, keep Controls toggles bound to the current panel, and dismiss on outside clicks or Esc even when Windows denies keyboard focus.

- Smooth Start opening and closing with reversible native-window motion, and add Open, administrator, file-location and pin actions to app context menus.
- In Smart mode, auto-hide the dock for the focused maximized app on its own monitor while retaining bottom-strip reveal, leave delay and open-menu holds. Existing fullscreen suppression also applies.

- Add a selectable active-media source list with exact-ID exclusions and an opt-in Firefox quick-add button that recognizes locally registered installations.

- Release dock state before native context menus and app actions so periodic status updates cannot panic while pinning or launching. Preserve popup state across nested destruction and make layered-surface resize failures safe.
- Add rotating production diagnostic files, panic backtraces and matching release symbols without opening a terminal.
- Add a separate current-monitor preview switch, including direct activation when an app has one local window.

- Keep the entire rounded activity island clickable at zero tint opacity, reduce its clock font, and add an optional Windows 11 native backdrop switch with the existing tint retained when unavailable.

- Add a configurable monitor-wide bottom reveal strip (1–128 physical px, default 8) with exact one-pixel controls. Keep the visible dock's full-width hover area, extended to cover taller reveal strips, to prevent repeated show/hide beside its icons.

- Keep calendar month browsing separate from date selection, reserve the blue marker for the real current date, and show a clear Back to today action when viewing another month.

- Refine island and menu transitions, animate the top bar tray overflow, and use consistent playback controls. Show reported Bluetooth output battery levels in Sound. Add opt-in music app exclusions with source-bound player commands.
- Keep window-card order stable and bind clicks to the pressed window and process. Show larger live thumbnails on hover without activating the source, and use main-folder labels for standard VS Code window titles.

- Repair hidden-tray menu first presentation, tune the island to restrained rounded corners, and let fresh Wi-Fi discovery proceed when an initial cached read is transiently unavailable.

- Repair island and menu corners using final per-pixel presentation without leaking native acrylic. Compact current Wi-Fi/sound rows, cap available networks at 15 with scrolling, and add fresh-scan Refresh/status and confirmed-radio-on refresh.


### New

- Add a local month calendar to the top bar clock, with Windows locale/week-start settings, date selection and keyboard navigation. Give Wi-Fi and Sound dedicated menu layouts and paint the activity island as a rounded island.

- Add an optional per-display top bar with a date/clock and music island, separate Wi-Fi and sound menus, independent transparency and height settings, and Ctrl-drag tray placement. The dock keeps Start and apps while the top bar is active.

- Hover an app with several windows to see its windows. Click one to switch to it, or middle-click to close it. (#21, by @JulienMaille)
- Set the app icon size in Settings. The dock layout, spacing and icons scale together. (#39, by @JulienMaille)

### Fixes

- Notification icons keep reaching Wisp when Explorer shows its taskbar. (#38, fixes #19)
- Right-clicking the dock no longer shows a "Wisp" title strip across its top. (#34, by @RayTrunk, fixes #33)
- Wisp hides the taskbar reliably when it starts. Before, the taskbar sometimes stayed visible for several seconds, especially with a second screen. (#30, by @RayTrunk, fixes #28)
- The Windows taskbar no longer flashes when you switch the dock between autohide and Keep the dock visible. (#24, by @RayTrunk, fixes #18)
- The best match stays selected while typing in the launcher when the mouse rests over the results. Move the mouse to highlight another result. (#36, fixes #35)
- Time searches now find English city names on non-English Windows, while still finding names in the Windows display language. (#37, fixes #26)

## 0.5.0 - 2026-11-07

### New

- **Keep the dock visible.** A new option in Settings keeps the dock on screen and reserves its space, so maximized windows stop above it instead of going behind it. Full-screen apps still hide it, including apps that go full screen from a maximized window. (#9)
- **Click the app in front to minimize it**, like a taskbar button. Click it again to bring it back. (#14, by @JulienMaille)

### Fixes

- Wisp now hides the taskbar when you start it from the Start menu. Before, it could fail with "Could not find primary taskbar". (#8)
- Clicking an app in the dock now switches to it while Action Center, Start or search is open. (#13, by @JulienMaille, fixes #11)
- Dock icons keep their place when clicked instead of moving to the front. (#5, by @JulienMaille)
- Tray icons are sharp instead of pixelated. (#3, by @JulienMaille, fixes #2)
- Windows 10: icons and text use the Windows 10 fonts instead of drawing as boxes. (#10, for #1)
- The tray no longer comes up empty after starting Wisp from the Start menu. (#9)
- Turning "Keep the dock visible" on or off no longer freezes the dock and launcher for up to two seconds. (#9)
- Taskbar hiding retries reliably after Explorer restarts and display changes. (#8)
- An icon no longer stays raised after hovering the hidden-icons arrow. (#16)
- The "Show hidden icons" tooltip no longer covers its own flyout. (#17)
- Configs from older versions pick up new settings from the defaults. Configs from Wisp 0.3 keep the taskbar as before. (#8)

### Thanks

@JulienMaille contributed their first pull requests to Wisp in this release: #3, #5, #13 and #14.

## Earlier releases

See the [releases page](https://github.com/SumirSeth/wisp/releases) for 0.4.2 and earlier.
