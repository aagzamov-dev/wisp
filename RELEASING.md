# Releasing Wisp

Wisp ships one Windows x64 installer and a portable ZIP. The installer places the launcher and Settings together, creates Start menu shortcuts, and registers Wisp in Windows Installed Apps. The update service still uses the two versioned executable assets from each stable GitHub release.

## Build a candidate

Use the pinned Rust toolchain, the Windows SDK, and Inno Setup 6 on Windows x64. GitHub Actions builds the release from its tag. For a local candidate, run from the repository root in Git Bash:

```bash
WISP_RELEASE_REPOSITORY=aagzamov-dev/wisp cargo build --locked --release --workspace
pwsh -NoProfile -File scripts/package-release.ps1 -OutputDir dist
```

The output is `dist/WispSetup.exe`, a versioned ZIP, the two updater executables, and `SHA256SUMS.txt`. Wisp's current Windows packages are unsigned. Verify the download against the checksum on the release page.

## Verify installation and updates

Use disposable Windows environments. Keep at least one test installation on the previous stable release for the post-publish update check. Check Windows 10 before approving v1.0.0, as required by [MAINTAINING.md](MAINTAINING.md).

| Scenario | Expected result |
| --- | --- |
| Clean install as a regular user | Setup installs both executables in `%LOCALAPPDATA%\Programs\Wisp`, registers Wisp in Installed Apps, and adds working Start menu entries. Launching Wisp starts the dock, and its Settings command opens Wisp Settings. |
| Upgrade the v0.7.0 ZIP installation | Quit Wisp and Settings, then run Setup in the existing install folder. Both executables are replaced. `config.toml`, `dock-pins.toml`, and `start-pins.toml` in `%APPDATA%\wisp` keep their contents. Open Wisp and check the saved settings and both pin lists. |
| Uninstall | Windows Installed Apps removes the installed executables and shortcuts. User settings and pins remain in `%APPDATA%\wisp`. |
| Update through Settings | After publication, open **Settings → About → Check for updates** in the previous stable release. Choose **Install and restart**. Confirm the installed version updates, the two binaries match the release, Wisp restarts if it was running, and settings and pins are preserved. |

GitHub Actions verifies packaging and the clean-install and v0.7.0 upgrade scenarios with real installer and release files. The updater tests cover release metadata, payload validation, and replacing the executable pair. The live GitHub update requires the new stable release to be visible to the previous version.

## Publish

1. Update both package versions and their entries in `Cargo.lock`. Move the release notes from **Unreleased** into a dated version heading and leave an empty **Unreleased** heading.
2. Open a release pull request targeting `main` in `aagzamov-dev/wisp`. Label it `release`, record the Windows test results, and wait for CI to pass.
3. Rebase-merge the pull request. Once the release is approved, tag the merged commit and push the tag to the fork. For v1.0.0:

   ```bash
   git tag -a v1.0.0 <merged-commit> -m "Wisp v1.0.0"
   git push https://github.com/aagzamov-dev/wisp.git refs/tags/v1.0.0
   ```

4. Wait for the **Release** workflow. Verify the public page shows `WispSetup.exe`, the ZIP, both versioned executables, and `SHA256SUMS.txt`. Run the live Settings update check from the previous stable release.

Keep the exact filenames `wisp-v<version>-x86_64.exe` and `wisp-settings-v<version>-x86_64.exe` in stable releases. Older versions match those names and verify the SHA-256 digests supplied by GitHub. If a published release needs a correction, ship a patch version and keep the original tag intact.
