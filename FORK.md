# Msprg/framework-control fork

This fork is upstream [ozturkkl/framework-control](https://github.com/ozturkkl/framework-control) with a small patch stack on top. It is maintained here and not offered upstream. The patches live as separate commits on the `mods` branch, rebased onto each upstream release.

## The mods

Oldest first, as they sit on top of upstream `main`:

1. **parser: read charge-limit minimum from framework_tool's one-line output.** framework_tool 0.6.6 prints `Minimum 0%, Maximum 85%`, and the old parser read 85 for both values. This touches `service/src/cli/framework_tool_parser.rs`.
2. **installer: keep user data across upgrades.** Upstream packages up to 0.5.5 remove the old product before installing the new one, which runs the uninstall cleanup and deletes `config.json`, `battery-history.json`, logs, shortcuts, the icon and `framework_tool.exe`.
   - The fix changes `MajorUpgrade` to `afterInstallExecute` and adds `REINSTALLMODE=amus`.
   - `service/scripts/test-msi-upgrade.ps1` tests upgrade, uninstall and fresh install, and asserts on the MSI logs.
   - Upstream `dev` (0.5.6) has the same `afterInstallExecute` change. After syncing to it, this commit shrinks to `REINSTALLMODE=amus` plus the test.
3. **installer: remove Desktop shortcuts on uninstall.** `CommonDesktopFolder` is not a Windows Installer folder, so `DesktopFolder` replaces it.
4. **service: in-process EC through framework_lib, CLI as fallback.**
   - New files: `service/src/cli/ec_lib.rs` and `ec_selftest.rs`.
   - `FrameworkTool` in `service/src/cli/framework_tool.rs` tries the library first, then the CLI.
   - Also: the `framework_lib` pin in `Cargo.toml`, `THIRD_PARTY_NOTICES.md` in both packages, and the "EC access" card in the device header.
   - Opt out with `FRAMEWORK_CONTROL_EC_BACKEND=cli`.
5. **fork: CI, upstream check, this file.** Contains `.github/workflows/fork-build.yml`, `.github/workflows/fork-upstream-check.yml` and `FORK.md`.

Upstream's `CHANGELOG.md` is left alone so release syncs don't conflict there. Fork changes are listed at the end of this file.

## Branches, tags, workflows

- **`main`** mirrors upstream `main`. Never commit to it. Update it with `gh repo sync Msprg/framework-control -b main`.
- **`mods`** is upstream `main` plus the patch stack. It is the fork's default branch.
- **`sync/<upstream-short-sha>`** is a candidate rebase. The fork build runs on it, and `mods` only moves to a candidate after that build is green.
- **`mods/<upstream-version>-<n>` tags** mark each verified `mods` tip. Since `mods` is rebased (force-pushed), the tags keep every installed build reachable. `archive/*` tags hold pre-fork-layout history.
- **Fork build** (`fork-build.yml`) runs on `mods` and `sync/**`. It runs check and test on Windows, Linux gnu and Linux musl, then lint, which uploads a `lint-patch` artifact with the fixes. It builds the MSI and the Linux tarball. Finally it runs the MSI upgrade test from upstream's latest stable release to the new MSI.
- **Upstream check** (`fork-upstream-check.yml`) runs daily and on demand, and never pushes.
  - It reports new upstream `main` and `dev` commits, changes to the files listed under Parity, whether the patch stack merges and rebases cleanly, and whether a newer framework_lib or framework_tool release exists.
  - It fails while `mods` is behind upstream `main`, so GitHub sends a notification.
- **Upstream workflows are disabled on the fork**: `release-service.yml`, `deploy-pages.yml` and `external-pr-notice.yml`. Without this, syncing `main` would publish unmodified upstream releases under the fork. Keep them disabled (`gh workflow list -R Msprg/framework-control --all`).
  - A workflow file that upstream adds later arrives enabled and can run on the very push that brings it in.
  - Before each sync, check `git diff --stat fork/main origin/main -- .github/workflows`. Disable anything that publishes or deploys as soon as it shows up in the list.
- **Don't use GitHub's "Sync fork" button on `mods`.** It merges upstream into the branch, or offers to discard the patch commits. The daily check's failure email links to the repo page where that button sits.

## Installing

Fork builds have in-app updates disabled (`FRAMEWORK_CONTROL_UPDATE_REPO` is empty). The upstream updater would install an upstream release over this build, which drops the in-process EC, and upstream releases before 0.5.6 also wipe the user data.

To install or update, download the `windows-msi` artifact from the green fork build of the tagged `mods` commit and install it. The `linux-tarball` artifact is the Linux equivalent.

Artifacts expire after 90 days. The tags keep the source, not the installers. Either keep the MSI you installed, or rebuild it with `gh workflow run fork-build.yml -R Msprg/framework-control --ref mods/<version>-<n>`.

The upgrade test installs upstream's latest stable release first. Once upstream releases a version newer than `mods`, that job fails on the MSI downgrade block until `mods` is synced. It means a sync is due, not that the installer is broken.

Installing an upstream MSI replaces the mods. Up to 0.5.5 it also deletes `C:\ProgramData\FrameworkControl\config.json` and `battery-history.json`, so back them up first.

## Syncing

Sync per upstream release, when upstream `main` moves. Watch `dev` through the daily check to see conflicts coming.

```sh
git fetch --multiple origin fork                  # origin = upstream, fork = Msprg
git diff --stat fork/main origin/main -- .github/workflows   # new upstream workflows? see above
gh repo sync Msprg/framework-control -b main      # mirror
sha=$(git rev-parse --short origin/main)
git switch -c sync/$sha mods
git rebase origin/main                            # resolve conflicts commit by commit
git push fork sync/$sha                           # fork build runs: tests, MSI, upgrade test
# green, and the parity review below is done:
git switch mods && git reset --hard sync/$sha
git push --force-with-lease fork mods
git tag mods/<upstream-version>-1 && git push fork mods/<upstream-version>-1
git push fork --delete sync/$sha
```

- **There is no local toolchain on the maintainer's machine.** CI is the compiler, formatter and lockfile generator.
  - Never move `mods` to a resolution CI hasn't built.
  - Apply the `lint-patch` artifact if lint fails.
- **For a `Cargo.lock` conflict**, take upstream's file. During a rebase that is `git checkout --ours service/Cargo.lock`, because "ours" is the branch being rebased onto. CI builds aren't `--locked`, so the next run regenerates the lockfile with the `framework_lib` entries. Commit the `Cargo.lock` from its `check-*` artifact.
- **Version numbers** stay upstream's. The fork does not publish releases.

## Parity

The in-process backend copies what `framework_tool` plus the parsers return. A clean rebase can still leave the two disagreeing. On every sync, read upstream's diff to these files, even when the rebase applied cleanly:

- **`service/src/cli/framework_tool.rs` and `framework_tool_parser.rs`:** port any change in what the CLI path returns to `ec_lib.rs` (`thermal_from_raw`, `power_from_raw`, `BatteryBlock`, `charge_rate_current_ma` and the `Ctx` methods), and extend the parity tests at the bottom of `ec_lib.rs`. The upstream code that resolves the CLI path is reworked by the EC commit (`at_path`, `unchecked`, `cli_versions`, `default_cli_path`), so port changes there by hand.
- **`service/src/routes.rs`:** anything new that calls `FrameworkTool` should go through `exec`/`with_fallback`.
- **`service/wix/main.wxs`:** keep `afterInstallExecute`, `REINSTALLMODE=amus`, `DesktopFolder`, the unconditioned `RuntimeDataCleanup` component with its fixed GUID, and the `ThirdPartyNotices` component.
- **A new `framework_tool` release:** the service downloads the latest one at runtime on Windows, so CLI output can change while the library pin doesn't. Check its changelog and run `framework-control-service --ec-selftest` on hardware.
- **Bumping `framework_lib`:** read its diff around the APIs `ec_lib.rs` uses (`CrosEc`, memmap offsets, `get_thermal`, `get_charge_limit`, `set_charge_*`, `fan_set_duty`, `autofanctrl`, `version_info`, smbios). Then bump the label in `ec_lib::label()` and the notices file, and run `--ec-selftest` on hardware.

### Known work for the next sync (upstream `dev`, 0.5.6)

- **`framework_tool.rs` conflicts with the EC commit.**
  - Port `TOOL_COMMANDS` (Snap's `framework-tool` name) into the reworked `candidate_paths`.
  - Port `run_accepting_failure_if` into the CLI fallback path.
- **No battery:** upstream's CLI `power()` now succeeds without a battery, with `battery_present = false` and `ac_present = true` (#93). `power_from_raw` still returns an error there, which only falls back to the CLI on every call. Return the same values as the CLI instead, and add a parity test.
- **Installer:** upstream's `main.wxs` already has `afterInstallExecute` and the removed condition. Keep `REINSTALLMODE=amus`, the `DesktopFolder` fix and the test.
- **Docs:** `PROJECT_SUMMARY.md` conflicts.
- **Not superseded:** `parse_charge_limit` is untouched upstream, so mod 1 still applies.

## Fork changelog

- **2026-11-13: fork layout.** The patch stack is reorganised onto `mods`. Fork builds disable in-app updates. A daily upstream check is added, and the upstream release, Pages and PR-notice workflows are disabled on the fork.
- **2026-11-10: Windows installer: upgrades no longer delete user data.**
  - Installing a new MSI over an existing one, including in-app updates, removed the old version first. That ran its uninstall cleanup and deleted `config.json`, `battery-history.json`, the service logs, the Start Menu and Desktop shortcuts and icon, and the downloaded `framework_tool.exe`. The `NOT UPGRADINGPRODUCTCODE` component condition never applied to removal.
  - The old version is now removed after the new one is installed (`afterInstallExecute`), so the cleanup component stays shared and its files are kept. This also protects upgrades from earlier versions.
  - A genuine uninstall still removes everything, now including Desktop shortcuts (`DesktopFolder` replaces the non-standard `CommonDesktopFolder`).
- **2026-11-10: Service: in-process EC access.**
  - Temperatures, fan speeds, battery info, charge limits and fan duty go through `framework_lib` (pinned to 0.6.6) inside the service. Before, the service spawned `framework_tool` for every call, about one process per second with the fan curve on.
  - Calls run on one dedicated thread using the Windows CrosEC driver or Linux `/dev/cros_ec`.
  - Any error, panic or hang falls back to the CLI for that call. The in-process path shuts itself off if the library stops responding.
  - Output matches the CLI's, so saved fan curves keep their sensors.
  - `FRAMEWORK_CONTROL_EC_BACKEND=cli` opts out. The device header shows which backend is in use, and `framework-control-service --ec-selftest` compares both backends on a machine.
  - Installers ship `THIRD_PARTY_NOTICES.md`.
- **2026-11-10: Charge limit:** the minimum is read correctly from framework_tool 0.6.6 output.
