<p align="center">
  <img src="assets/icon.svg" width="96" height="96" alt="Wisp logo">
</p>

<h1 align="center">Wisp</h1>

<p align="center">
  <b>A dock and a launcher for Windows.</b><br>
  Replaces the taskbar. One ~1.5 MB executable.
</p>

<p align="center">
  <a href="https://github.com/aagzamov-dev/wisp/releases/latest"><img src="https://img.shields.io/github/v/release/aagzamov-dev/wisp?style=flat-square&color=0891b2" alt="Latest release"></a>
  <a href="https://github.com/aagzamov-dev/wisp/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/aagzamov-dev/wisp/ci.yml?branch=main&style=flat-square" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/SumirSeth/wisp?style=flat-square" alt="MIT license"></a>
  <img src="https://img.shields.io/badge/platform-Windows%2011-0078d4?style=flat-square" alt="Windows 11">
</p>

<p align="center">
  <img src="assets/dock-to-launcher.webp" width="700" alt="The dock slides up, then grows into the launcher">
</p>

Wisp can hide the Windows taskbar and show a dock in its place. Tap the Windows key or click the dock search icon to open Wisp Start above the dock. <kbd>Alt</kbd>+<kbd>Space</kbd> opens the compact launcher.

It is written in Rust with Win32 and Direct2D. It needs no .NET, web view or PowerToys.

## Install

1. Download the zip from the [latest release](https://github.com/aagzamov-dev/wisp/releases/latest).
2. Extract it to `%LOCALAPPDATA%\Programs\Wisp`.
3. Run `wisp.exe`. The settings app opens and sets your shortcut and **Start with Windows**.

To upgrade from a build without an updater, quit Wisp and Settings, then replace both executables with the ones in the new release ZIP. Keep them together in a writable local folder within your Windows user folder, such as `%LOCALAPPDATA%\Programs\Wisp`. Run Wisp with your regular user permissions to install updates from Settings.

In a release build, open **Settings → About → Check for updates**. When a newer release is available, choose **Install and restart**. **Install updates automatically** is off by default. Enable it to let Wisp check daily while it is active and Settings is closed. Updates use stable GitHub Releases from this fork, verify both executables, and restart Wisp if it was running. Your settings and pins are stored separately.

> [!NOTE]
> PowerToys Run also uses <kbd>Alt</kbd>+<kbd>Space</kbd>. Turn it off, or change Wisp's shortcut.

> [!NOTE]
> Wisp is not code-signed. If SmartScreen warns you, select **More info → Run anyway**.

## Dock

Wisp places a dock on each connected monitor. A hidden dock appears when the pointer enters its monitor's full-width bottom reveal strip. Set **Settings → General → Dock → Reveal strip height** from 1–128 physical pixels, default 8, using the slider or one-pixel −/+ buttons. Physical pixels do not grow with Windows display scaling. Existing configurations without this setting use the new 8 px default instead of the previous dock-height activation area.

Once visible, the dock stays open throughout a full-width area covering its resting height plus bottom gap, or the reveal strip when that is taller. This keeps it stable even beside the icons. **Hide for focused maximized apps** (`dock_smart_hide`) keeps the local dock visible outside focused maximized or fullscreen apps on its own display. With a maximized app focused, the dock uses its normal leave delay and bottom-strip reveal; open menus, previews and drag interactions keep it visible. Restored windows and background maximized apps do not trigger this hide. Turn this setting off for normal pointer auto-hide. True fullscreen keeps its existing suppression and does not reveal from bottom hover. The existing fullscreen dismissal delay, suspended docks and held mouse buttons still suppress hover-based reveal. **Keep the dock visible** reserves space on the main screen when Wisp replaces the taskbar.

Borderless fullscreen remains eligible when Windows reports the full monitor as its work area after a display/session change. Captioned or resizable maximized windows stay separate from true fullscreen and use the local Smart policy above. These cases are covered by unit tests; return-from-RDP behavior still needs desktop verification.

**Show apps from this monitor only** filters running apps to the current screen and keeps pinned apps visible. Window previews include all windows for a visible app by default.

Enable **Preview windows from this monitor only** to filter the chooser separately. With one local window, a normal app click activates that window directly. With several, the chooser shows only those local windows. With no local window, preview cards stay hidden and the app keeps its normal activation or launch behavior, which may switch monitors. The setting defaults off (`dock_preview_current_monitor_only = false`) and does not move windows between monitors.

Hover a window card briefly for a larger live DWM thumbnail above the chooser. Hovering does not activate or restore the source window; this is a thumbnail preview rather than Windows desktop Peek. The chooser lists windows in opening order, oldest first, across repeated hovers and focus changes. Windows already open when Wisp starts take their initial order from the first dock scan, back to front. A card click must begin and end on the same window and process, so refreshed or removed cards cancel the click. VS Code's standard window titles show only the main folder name when its published workspace title identifies it reliably. Custom or ambiguous titles remain unchanged. Other apps keep their published titles, including browser tabs. Account names appear only when an app includes them in its window title.

| Do this | To |
| --- | --- |
| Click an app | Launch it if closed, switch to its window, or choose from thumbnails when it has multiple windows |
| Click the app in front | Minimize it |
| Middle-click an app | Open a new window of it |
| Right-click an app | Open a menu above the dock to open a new window, pin or unpin the app, or close its windows |
| Drag an app icon | Reorder pins within the pinned section or running apps within the running section. Wisp saves the pin order. |
| Hover an app with open windows | See a thumbnail card for each window. Click to switch, or use the close button to close that window. Use the arrows or wheel when the cards span multiple pages. |
| Click a tray icon | Use it as you would on the taskbar |
| Click **⌄** | Show the hidden tray icons |
| Right-click **⌄** | Show the taskbar, or open taskbar settings |
| Click the controls icon | Open Control Center for Wi-Fi, Bluetooth, display and sound |
| Scroll over the controls icon | Change the volume |
| Right-click the controls icon | Toggle Control Center |
| Click the clock | Open notifications and the calendar |
| Click the battery | Open battery settings |
| Scroll anywhere else | Switch virtual desktops |
| <kbd>Shift</kbd>+scroll | Scroll through the apps |

Control Center uses a compact home panel for Wi-Fi, Bluetooth, display and sound, with the existing actions and subpages retained. It prepares its visible frame before opening. Click Controls again or click outside the panel to close it. Esc returns from a focused submenu to the home page, then closes the panel; it closes the calendar directly. If Windows keeps keyboard focus in another app, outside clicks and Esc still dismiss the panel.

Glass presentation reuses unchanged native placement and clipping during fades, and opening the top bar's tray menu leaves a stationary island's glass in place. Local diagnostics record Control Center preparation/show duration and the RDP pause/resume lifecycle without window titles or network names. These checks do not establish GPU driver stability or measured desktop latency.

Top-bar Control Center panels honor their requested opening duration, and tray overflow uses bounded 180/140 ms opening/closing motion. Slow frames advance animation by elapsed time. Island-only motion leaves the full-width bar's pixels untouched; native companion glass revalidates its visibility and position behind the foreground after activation. The island's glass opacity follows its last completed foreground frame. These rendering safeguards have unit/offscreen coverage; actual desktop smoothness and compositor ordering still require live verification.

The monitor icon in Control Center opens Display settings and brightness controls for monitors that expose DDC/CI. The Sound row shows the connected Bluetooth output’s battery percentage when Windows reports a level for the matching device. Do Not Disturb opens Windows notification settings.

A fullscreen app hides the dock on its own screen. The docks share the same app pins and Windows tray icons. Tray icons you pin in Windows appear inline, and the rest are behind **⌄**.

If Wisp quits or crashes, the taskbar comes back with every tray icon.

## Remote Desktop

**Settings → General → Pause Wisp during Remote Desktop** is on by default. When your Windows session connects over RDP, Wisp closes its dock, top bar, and launcher, releases its shortcuts and reserved screen space, and restores the Windows taskbar. A session listener waits for the same session to return to the physical console. Disconnecting the RDP client alone keeps Wisp paused.

Turn the option off to use the normal Wisp layout during RDP. Existing users who had explicitly chosen Remote mode **Off** retain that behavior until they set `pause_on_rdp` in `config.toml`. The old layout presets are retired.

## Music notch

Enable the music notch in **Wisp Settings → Notch**. It appears on the main display when music is playing. Click to expand the player and use the progress bar to seek when the playing app supports it. Fullscreen hides the notch on that display.

## Top bar

Enable **Wisp Settings → Top bar → Enable top bar** for a transparent desktop bar with app tray icons on the left, an activity island in the center, and Wi-Fi, sound, battery and Control Center on the right. The dock keeps Start and apps. Turning the top bar off restores the dock's tray and status controls. The top bar can also run with the dock disabled.

Set its height from 24–48 Windows-scaled pixels, adjust the bar's transparency independently of the darker island tint, and choose the main display, all displays, or individual displays. Enable **Blur behind island** for an optional Windows 11 native backdrop beneath the island. It defaults off; unsupported systems, high contrast, and disabled Windows transparency retain the existing tint. Windows controls the native material and can limit it through power policies. Selected displays keep their choices while disconnected. Each selected display reserves a top band for maximized windows and hides its bar during fullscreen. Windows transparency and reduced-motion preferences are respected.

The idle island shows the weekday, date, month and clock in your Windows locale. During music playback it shows artwork, the track name, previous, pause/play, next and the clock. Paused music keeps the controls available for 12 seconds before returning to the date. On narrow displays the track title and artwork yield space to the playback buttons. Click the top bar clock to toggle Wisp's calendar below the island. Wi-Fi and sound open their own menus; Control Center keeps the complete controls.

Hold **Ctrl** and drag a tray icon from the bar into its overflow menu to hide it, or from the overflow menu onto the bar to keep it visible. Keep Ctrl held until you release the mouse. **Esc** cancels. Choices are saved locally in `topbar-tray.toml`; icons without a persistent app identity cannot save a custom placement. The standalone music notch is suspended while any top bar is active.

Right-click the tray chevron and choose **Show taskbar** to temporarily bring back the Windows taskbar. Use it when Windows puts a pending administrator request on the taskbar. Windows presents the approval prompt.

In **Top bar → Music sources**, exclude an app by its complete displayed name or Windows media source ID. Matching ignores case and surrounding spaces. The active-media list reads available Windows media-session app identities in the background; Refresh updates the list, and Exclude saves the selected exact source ID. It shows up to 32 sources in a scrollable panel, without reading track titles, browser tabs or URLs. Chrome, Edge and Firefox have quick-add buttons; excluding a browser hides all media from that browser. Firefox quick-add includes installation identities registered locally by Mozilla. Portable or unregistered Firefox installations may require selecting their exact source ID from the list instead; an unknown hexadecimal ID is never assumed to be Firefox. URL-specific exclusions are unavailable because Windows media sessions do not expose a reliable URL. The default exclusion list is empty, and launcher media commands remain available. Player buttons validate the displayed source and track before sending a command.

Top bar config keys: `topbar` (default `false`), `topbar_height` (`32`), `topbar_opacity` (`0`), `topbar_island_opacity` (`70`), and `topbar_monitors` (`["primary"]`, `["all"]`, or a list of Windows display device names). Opacity is 0–100; changing it affects the background, not the icons. `topbar_island_blur` defaults to `false`. At 0% island opacity, the rounded interior keeps a minimal input alpha so the gaps between text and controls remain clickable; exterior corners stay transparent. The island clock uses a dedicated smaller font.

`music_excluded_apps` defaults to `[]` and applies to both the top bar and standalone music notch. For example, `["Google Chrome"]` hides Chrome media without changing browser settings.

The transparent bar now uses a per-pixel desktop surface, so its empty areas do not inherit the Windows accent/frame tint. High contrast or disabled Windows transparency uses an opaque accessible surface. The rounded island fits the measured localized date and clock. Clock/music changes crossfade their content, and Wi-Fi, sound and tray menus animate their complete surface from the clicked control and reverse on dismissal. Transitions can reverse without restarting, respect reduced motion, and stop their animation timer at rest. Ctrl-drag uses global input samples, so dropping outside the original nonactivating window works; release the mouse over the bar or overflow to save the choice.

The calendar shows a complete six-week month grid, your Windows first weekday and localized month/day labels. Month arrows and Page Up/Down browse without changing the selected date. Today has a blue marker; an explicitly selected date has a separate outline. When browsing another month, Back to today returns the view, selection and keyboard focus to the real current date. Arrow keys move keyboard focus, Home returns to today, Enter selects the focused day, and Esc closes the calendar. Adjacent-month dates remain selectable. Selection is local to the open panel; the calendar does not store appointments or poll media, network, or hardware state. Wi-Fi uses a compact connected-network row and up to 15 available network cards in a scrollable panel. Refresh starts a fresh scan with visible scanning/error feedback; enabling Wi-Fi refreshes the open network menu after the radio confirms it is on. Sound uses a compact current-device row above volume and output choices. The island uses restrained 7 DIP rounded corners. Its tray overflow prepares the actual per-pixel frame before showing, so opening never depends on a reentrant initial paint. Initial Wi-Fi discovery distinguishes checking/scanning, a successful empty result and a failed scan; transient cached-list failure does not prevent the requested fresh scan. The island, Control Center menus and top bar tray overflow use per-pixel rounded surfaces with transparent corners. Their dark/light tint avoids a rectangular native backdrop leaking outside the rounded shape. The optional island-only blur uses a separate clipped Windows Composition surface behind the interactive island. Wisp-owned menus follow Shared material; the top bar and island keep their independent settings. Native blur appearance and exact compositor-frame alignment require desktop verification.

## Start and launcher

Wisp Start shows up to 12 pinned apps in a compact six-column, two-row grid of square tiles beneath its search pill. Home height follows the grid width; search and live surfaces keep their existing height budget. It retains the launcher's app, file, command, calculator and window search, scopes, surfaces, keyboard shortcuts and drag and drop. Searches with few or no results use a shorter panel; live surfaces keep their full height.

**Recently installed** is the page to the right of **Pinned**. Scroll horizontally or select a tab to switch pages. Scroll vertically to browse the list of launchable apps with known dates, newest first. An installer date can reflect an update or repair, and **First seen** is the day Wisp first found an app added after its initial scan. Wisp keeps this history locally.

The compact launcher opened by **Alt+Space** follows the same Shared material background and bounded 220/180 ms opening/closing fade as Start. It retains its query, results and actions, respects reduced/disabled motion, and reverses a closing fade when reopened. Top-bar tray overflow keeps complete icon pixels cached even when its first opening frame is invisible. Native appearance remains unverified by build and unit checks.

Right-click an app result or pinned tile to open its compact Wisp popup. The Menu key or **Shift+F10** opens it for the focused app from the keyboard. There are no three-dot buttons. The menu retains **Open**, **Run as administrator**, **Open file location**, **Pin/Unpin Wisp Start**, **Pin/Unpin Wisp Dock**, **Copy path**, **App settings** and **Uninstall (Windows Settings)**. It follows Shared material, uses aligned glyphs and 34 DIP rows, and shows disabled reasons on a second line only when needed. Settings and Uninstall open Windows Installed apps, where you select the app and use Windows' available controls; Wisp does not run an uninstaller directly. Selected app results retain their three inline actions and Enter hint.

The popup supports arrows, Home/End and Tab to move between available actions, Enter to choose, and Esc or an outside click to close. Short work areas scroll, including when keyboard focus moves. Its prepared rounded frame fades using cached pixels in about 180/140 ms, respecting disabled/reduced motion. App/index, Start session, configuration, display or appearance changes cancel the open snapshot before an action can run. UI Automation exposes its menu items, disabled reasons, focus and invocation. Native screen-reader behavior, glass appearance and RDP performance still require desktop verification; only build and unit checks were authorized for this change. Other native Windows and third-party menus keep their system styling.

Desktop paths and shortcut launch metadata are resolved by the background app indexer. Elevation preserves the original shortcut when Windows exposes it, or the target's shell-provided arguments and working directory. Packaged apps cannot elevate; unresolved desktop targets remain disabled. Older app caches and Start/Dock pin files remain readable; rescanning refreshes app capabilities. Open keeps using AppsFolder for normal desktop and Store launches.

The persistent footer opens profile actions, Settings and Power, including Quit Wisp. During search, press **F6**, then **Tab** or **Shift+Tab** to visit the footer and available result actions; **Enter** activates the focused control. Tab from the query keeps its existing search actions. Restart, shut down and sign out require a separate confirmation.

Start uses shared bounded motion: 220/180 ms opening/closing, with a 10 DIP rise and whole-window fade. Its prepared native surface includes the foreground and background, avoiding a separate content-opacity floor; opacity/translation ticks reuse that surface. Rapid toggles reverse from current progress. Start never paints an extra dock while closing. Start timing remains separate from the dock sliders; reduced motion or `dock_open_ms = 0` disables it, and `dock_close_ms = 0` disables closing motion. Pin artwork retains its 40% reduction; app tile click targets are square. Profile, Settings and Power use matching square icon buttons, retaining their menus and keyboard actions. Native acrylic/opacity compatibility and actual smoothness still require authorized desktop verification.

Contributors must follow [animation and material rules](ANIMATION.md) for these surfaces. The island's blur shares its foreground's top-bar owner group, so opening Start does not deliberately reparent its material. This addresses an activation-order risk; the reported green tint has not been reproduced by a live test.

Choose **Settings → General → Start position → Bottom** to open Start above the dock, or **Center** to center it in the active work area. Start clamps its bounds for the selected monitor and display scaling. The `start_position` setting defaults to `"bottom"` when missing and is independent of `position`, which continues to control the compact Alt+Space launcher.


<p align="center">
  <img src="assets/screenshot-launcher.png" width="700" alt="The launcher searching for apps">
</p>

Type to search, then press <kbd>Enter</kbd>.

| Type | To |
| --- | --- |
| `code` | Find apps, then files. `vsc` finds Visual Studio Code. |
| `?invoice` | Find files only |
| `^vis` · `term$` · `'note` · `!edge` | Match the start · the end · exactly · exclude |
| `sqrt(2) * 10` | Calculate. <kbd>Enter</kbd> copies the result. |
| `> ipconfig` | Run a command in Command Prompt |
| `wifi off` · `bt` · `vol 40` · `mute` | Change a setting. <kbd>Tab</kbd> lists networks, devices or outputs. |
| `play` · `pause` · `next` | Control media |
| `w code` | Switch to an open window |
| `lock` · `sleep` · `restart` · `shut down` | Lock or power off. Some ask for a second <kbd>Enter</kbd>. |
| `time tokyo` · `date` | Copy a time or the date |
| `dark mode` · `display` · … | Open that Settings page |
| `now` · `sound` | Open live panels for status and audio |
| `quit` | Quit Wisp |

Apps you opened recently rank higher. With the box empty, the launcher shows your recent apps.

| Key | To |
| --- | --- |
| <kbd>Alt</kbd>+<kbd>Space</kbd> | Show or hide the launcher |
| <kbd>Ctrl</kbd>+<kbd>Win</kbd>+<kbd>Alt</kbd>+<kbd>X</kbd> | Quit Wisp and restore the Windows taskbar |
| <kbd>Enter</kbd> | Open the selected result |
| <kbd>Tab</kbd> | Show more actions, such as **Run as administrator** |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Enter</kbd> | Run as administrator |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>E</kbd> | Show in File Explorer |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>C</kbd> | Copy the path |
| <kbd>Esc</kbd> | Close |

### Drag and drop

Drag a result out of the launcher and drop it anywhere:

- **Files and folders** into a chat, an email, an upload box or a folder. The original stays where it is.
- **Apps** onto the desktop or into a folder, to make a shortcut.
- **Text**, such as a result, the time or a path, into any text box.
- **Links** into a browser or a chat.

Press <kbd>Esc</kbd> to cancel a drag.

## Settings

Open **Settings** from Wisp's tray icon, or edit `%APPDATA%\wisp\config.toml`. The tray icon also has **Rescan apps**, **Start with Windows** and **Quit**.

| Setting | Default | What it does |
| --- | --- | --- |
| `hotkey` | `"Alt+Space"` | Shows or hides the launcher |
| `quit_hotkey` | `"Ctrl+Win+Alt+X"` | Quits Wisp globally. Edit under **Settings → General → Shortcuts → Quit Wisp** |
| `dock` | `true` | Shows the dock |
| `dock_autohide` | `true` | Hides the dock until the pointer touches the bottom edge. `false` keeps it visible. |
| `dock_reveal_height` | `8` | Height of the monitor-wide bottom activation strip, 1–128 physical px. Independent of Windows display scaling; the visible dock keeps its larger hover area. |
| `window_list` | `"always"` | `"multiple"` lists an app's windows when it has more than one; `"always"` or `"off"` |
| `dock_icon_size` | `24` | App icon size in the dock, 24–40 DIP. The dock's height, spacing and status area scale with it. |
| `dock_open_ms` | `180` | Dock opening animation time, 0–500 ms. |
| `dock_close_ms` | `140` | Dock closing animation time, 0–500 ms. |
| `dock_animation` | `"slide"` | Dock motion in Appearance. Choose Slide, Fade, Rise + Fade, or Scale + Fade. |
| `hide_taskbar` | `true` | Hides the Windows taskbar while Wisp is active |
| `pause_on_rdp` | `true` | Pauses Wisp during Remote Desktop and resumes it at the physical console |
| `position` | `"bottom"` | Compact launcher only: `"bottom"` grows it from the dock; `"center"` opens it in the middle |
| `start_position` | `"bottom"` | Start only: `"bottom"` opens above the dock; `"center"` centers it in the active work area. Missing values use Bottom. |
| `theme` | `"system"` | `"system"`, `"dark"` or `"light"` |
| `backdrop` | `"acrylic"` | Shared material for the dock, Start and Wisp-owned menus. Appearance offers Glass, Frosted and Solid, plus Mica, Transparent, Liquid Glass and Custom color. Frosted retains the `"acrylic"` config value. |
| `backdrop_transparency` | mode default | Optional background transparency for Transparent and Glass: 0 is opaque, 100 is clear. Missing values use 85 for Transparent and 36 for Glass. |
| `backdrop_blur` | mode default | Optional blur override for Transparent and Glass. Missing values use off for Transparent and on for Glass. |
| `backdrop_color` | `"#252833"` | RGB hex color used for Custom color. |
| `win_start` | `true` | A bare Windows key tap opens Wisp Start. Turn this off to restore the Windows Start shortcut. |
| `max_results` | `8` | Results visible at once |
| `show_recent` | `true` | Shows recent apps when the box is empty |
| `file_search` | `true` | Searches files in the Windows Search index |
| `file_search_exclude` | `["node_modules", …]` | Folder names file search skips |

**Settings → Appearance → Shared material** applies the same material preference to the dock, Start, Controls and other Wisp-owned menus. Existing material values remain supported; Frosted is the display name for `acrylic`. Native Windows and third-party menus keep their system styling. Top bar transparency and island material settings remain independent. Windows accessibility, transparency and power settings, unsupported systems, or Remote Desktop may limit blur; Wisp retains a readable fallback. Native/RDP appearance and performance have not been verified with live desktop testing.

Transparent and Glass expose background transparency and a blur switch. The percentage affects the background; text and icons keep their normal opacity. Glass shares Transparent's rendering recipe with a blurred default. Explicit transparency and blur values carry between these two modes. Clearer backgrounds can reduce text contrast over bright content. Intentionally disabled blur keeps plain transparency, while failed requested effects and system accessibility policy retain the readable fallback. See [material rules](MATERIALS.md) before changing this behavior; the `wisp-materials` skill points to those same instructions.

Configs from Wisp 0.3 keep the taskbar. To switch, set `dock` and `hide_taskbar` to `true` and `position` to `"bottom"`.

To add your own panel, put a TOML file in `%APPDATA%\wisp\surfaces`. See [`src/surface/now.toml`](src/surface/now.toml) for an example.

## Diagnostics

Production builds save application error diagnostics, lifecycle records and Rust panic backtraces to `%APPDATA%\wisp\logs` (`wisp-nightly\logs` for nightly builds), without opening a terminal. Launcher, Settings and taskbar-watchdog sessions have distinct files and process IDs. Each role keeps at most five 2 MiB files, for a combined limit of 30 MiB; individual records are bounded. A separate panic sink avoids waiting on the ordinary log writer during a panic.

Records include UTC timestamps, build/session identity, operation context and error details. Search queries, window titles, URLs, credentials and document contents are not intentionally recorded; existing diagnostics can include OS error text and file paths. Logs are local files, not telemetry. A native crash may bypass the Rust panic hook; Windows Error Reporting can provide additional fault information. A session without a clean-exit record can also mean forced termination or interrupted shutdown.

Keep the matching `wisp.pdb`, `wisp_settings.pdb` and build manifest with the executables when retaining a production build for diagnosis. Production optimization and abort-on-panic remain enabled; line tables and matching symbols make later crashes easier to locate. Logs cannot guarantee a complete record after disk failure or abrupt process termination.

## Speed

Wisp 0.2.0 against PowerToys Run 0.94 on the same PC, median of 30 runs:

| | Wisp | PowerToys Run |
| --- | --- | --- |
| Typing → first results | **24 ms** | 200 ms |
| Shortcut → launcher drawn | **15 ms** | 30 ms |
| Memory | **20 MB** | 302 MB |
| Size on disk | **1.5 MB** (0.4.0) | 794 MB |

A hidden dock uses almost no CPU. [`bench/`](bench) has the method and full results.

## How it works

- **Tray icons:** Wisp creates its own hidden `Shell_TrayWnd`, which apps find first. Wisp copies each icon and passes the message on to Explorer.
- **Start:** the dock has its own window and stays visible while Start is open. Start shares the launcher’s renderer, index and search.
- **Blur:** Windows turns off blur for inactive windows. The dock is never active, so it uses an older blur API that stays on.
- **Drawing:** Direct2D draws in software. At this size it is fast and saves about 30 MB.
- **Apps:** Wisp reads `shell:AppsFolder`, the same list Start uses.
- **Files:** Wisp queries the Windows Search index on a worker thread.

## Build

Install [Rust](https://rustup.rs) and the Windows SDK, then:

```sh
git clone https://github.com/SumirSeth/wisp
cd wisp
cargo build --release --workspace
```

To try your changes without touching your installed Wisp, run `scripts\install-nightly.ps1`. It installs **Wisp Nightly** with its own folder, config and shortcut (<kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Space</kbd>).

To contribute, see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE) © Sumir Seth
