# Animation and material rules

Read this file before changing Start, dock, activity island, tray or Controls motion. Preserve the existing actions, settings, accessibility and Remote behavior. These rules describe the current architecture; they do not certify native rendering or performance.

## Shared animation owner

- Use `src/animation.rs` for bounded visibility transitions and spring math. Do not add a private easing loop for each feature. `Motion` exposes Hidden, Opening, Open and Closing; `Spring` handles changing island outlines and content values.
- Retarget from the current position and velocity. Do not reset an interrupted open/close to its initial endpoint. Elapsed wall time advances the transition, including after a stalled message loop; never discard a slow frame to make an animation last longer. Normalize the trajectory to the requested duration so a slower setting changes intermediate motion, not just the final deadline.
- One surface owns its progress, timer and native lifetime. Hidden/settled surfaces stop animation timers. Local surfaces use the shared 15 ms timer interval; Remote surfaces use 30 ms. These are scheduling intervals, not guaranteed display frame rates.
- Respect Windows client-animation preferences and existing zero-duration settings. Settle immediately when animation is disabled. A timer failure must finish the intended state rather than strand an invisible window. Evaluate completion after native presentation: a rejected opacity update may settle the motion, and a closing window must retire its native lifetime and timer on that same tick.
- Keep the logical input state separate from the native closing lifetime. A closing Start still owns a Start surface. It must never fall through into drawing a dock; only the actual shared compact-launcher surface may draw shared dock content.

## Rendering and glass

- Prepare complete foreground pixels before showing a window or publishing its blur. Retain the last successfully presented state. Do not publish new blur geometry for failed or unfinished foreground pixels.
- Use one progress value for contents, border and material. Native Start uses whole-window opacity; its contents are rendered at full opacity, and its prepared window is reused during the fade. Restore the launcher opacity style only after Start has hidden.
- Layered menus pair their foreground with one retained blur host. Use the same owner group as their foreground, validate native visibility/Z-order, and retire the host before foreground destruction. Do not change owner groups when another surface opens.
- Share each surface's corner radius between foreground contour, painted border and blur clip. The top-bar tray uses `render::TOPBAR_FLYOUT_RADIUS` in DIPs, with matching animation scale, DPI conversion and half-size clamping; keep its conservative native blur inset.
- Keep menu dimensions stable during entrance/exit. A short fade and rise avoids repeatedly resizing software bitmaps and clipping regions. Island width and menu-content height changes may still animate when they convey actual state changes.
- Reuse cached pixels for opacity-only tray/app-menu frames. Repaint when content, hover/focus, DPI, theme or dimensions change. Skip unchanged native placement and clipping. Island frames publish foreground and then matching blur in the same callback; ordinary clicks must not hide stationary glass.
- Follow the shared transparency and optional-blur recipe in [MATERIALS.md](MATERIALS.md). Glass is a blurred preset of the same transparent base; keep user-selected background transparency separate from animation opacity and text opacity.
- A successful API call is not visual proof that blur appeared. Preserve the opaque accessibility/failed-effect policy, the island blur switch and the user's independent island tint setting. Clear user-selected backgrounds can reduce contrast; keep the default readable recipes and their relevant contrast checks.

## Work outside frame callbacks

- Wi-Fi/media discovery, icon decoding, app indexing and filesystem work must not run in an animation tick. Reuse existing background services and prepared assets; do not introduce per-frame requests, resource creation or blocking waits.
- Invalidate only affected surfaces. Window opacity/translation alone does not require repainting the Start app grid. A reentrant Start paint that cannot obtain its state borrow must request a later paint after the borrow releases.
- Keep diagnostics bounded and private. Use existing file diagnostics for failures and perf tracing for detailed timings; never write a log record on every frame. Timing reports must distinguish request handling, successfully submitted foreground pixels and completed motion. They must not include search text, titles, account names or network names.

## Required checks and evidence

Run formatting, workspace unit checks, both strict Clippy configurations and a production workspace build. Preserve unrelated checkout changes. Report known baseline failures rather than claiming every check passed.

Unit coverage must include opening/closing, rapid reversal, zero/reduced motion, delayed frames, Local/Remote duration caps, timer/native failure state and foreground/material ownership. Verify all actions and keyboard paths remain reachable. Start icon artwork retains its existing 60% nominal size. Home app tiles are square, and paint, hit testing and context anchors share their bounds; keep the separate search/surface capacity when compacting Home. Profile, Settings and Power use equal square icon buttons; preserve their internal identities and keyboard/menu actions.

Native desktop verification requires explicit authorization. When authorized, check repeated toggles, outside-click/Esc dismissal, app-menu handoff, the island during Start activation, multiple DPI/monitors, Remote entry/exit and bright/dark backgrounds. Measure input-to-first-frame delay and slow frames against the display budget. Build/unit checks alone cannot establish smoothness, actual acrylic/whole-window-opacity compatibility, compositor ordering, Remote glass quality or GPU stability. Do not describe those outcomes as verified without that evidence.
