# Shared materials

Read this document before changing Wisp's shared background appearance. Animation ownership, rounding and presentation rules are in [ANIMATION.md](ANIMATION.md).

## User controls

Transparency describes the background: **0% is opaque; 100% is clear**. It must not fade text, icons or hit targets. Opening and closing animations may fade a complete window independently of this preference.

Transparent provides adjustable transparency and optional background blur. Glass uses the same transparent recipe with a blurred default. It must not grow a separate tint calculation or native backend. Absent overrides retain each mode's existing background strength: Transparent is 85% transparent and Glass is 36% transparent. Explicit preferences apply consistently to both modes.

The optional config keys are `backdrop_transparency` (a whole percentage from 0 to 100) and `backdrop_blur` (a boolean). Omitting a key keeps the selected mode's default. These overrides affect Transparent and Glass; other saved materials retain their existing behavior. Keep the two applications' parsing, range handling and Settings display consistent.

Shared material applies to the dock, Start, launcher, Controls, tray, window previews and Wisp-owned popups, including Remote mode. The topbar strip and activity island retain their independent tint and blur preferences. Native Windows and third-party menus retain their own appearance.

## Implementation rules

- Resolve mode defaults and user overrides into one immutable recipe. Pass it through native and per-pixel surface creation, theme updates and retained popup state.
- Intentional no-blur transparency does not need a blur host. Preserve its background alpha when no host was requested; this is different from failure to create a requested effect.
- Respect high contrast and Windows transparency policy. Keep the explicit readable fallback for a failed requested native effect; do not silently change recovery policy while editing appearance.
- Preserve input inside clear menus, including the space between text and controls. A minimal input alpha inside the rounded silhouette may be necessary for per-pixel Windows hit testing; exterior corners remain transparent.
- Share bounds, radius, DPI conversion and animation progress between foreground, border and blur. Retain native resources and avoid per-frame allocation or configuration reads.
- Keep existing material identifiers and configuration compatibility. Validate new preferences in both Wisp and Settings, preserve unknown keys/comments during edits, and propagate live preference changes to existing menus.
- User-selected clear backgrounds can reduce text contrast over bright content. Keep text fully opaque and preserve the accessible opaque policy. Do not claim arbitrary transparency guarantees contrast against every background.

## Verification

Check missing-key defaults, saved overrides, percentage boundaries, invalid values, mode changes, blur off versus setup failure, system policy, clear-surface input alpha and Remote/local restoration. Exercise existing clipping and presentation tests; run the applicable workspace build and checks.

Record build/unit results separately from native appearance. A native API returning success does not prove that Windows rendered blur. Actual compositing, input, RDP appearance and animation smoothness require authorized desktop verification.
