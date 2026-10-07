# Releasing

## 1. The version

The version appears in three files, and all three must name the same one:

- `package.json` (`version`)
- `src-tauri/Cargo.toml` (`version` under `[package]`)
- `src-tauri/tauri.conf.json` (`version`)

If the three already name a version that has not been released, release that version as it is.
Otherwise raise all three to the same new version, run `cargo check` in `src-tauri` so
`Cargo.lock` picks it up, and commit. The updater compares semantic versions and installs only a
version higher than the one installed.

## 2. The release notes

Before tagging, update `RELEASE_NOTES.md` at the repository root and commit it; the workflow
reads it at the tagged commit. Its first line must be exactly `Butterfly Speak X.Y.Z`, the version
being tagged without the `v`; otherwise the release job fails.

The workflow passes the whole file as the GitHub release's body, tauri-action writes that body
into the `notes` field of `latest.json`, and installed copies show it on the **Help & about** page
when they offer the update, as plain text and cut at 4,000 characters. The app shows the notes as
written, so keep each bullet on one line.

## 3. The tag

```powershell
git tag vX.Y.Z
git push origin vX.Y.Z
```

Any tag that starts with `v` starts `.github/workflows/release.yml`. On a Windows runner it
installs Rust and the Node dependencies, fetches the sherpa-onnx libraries with
`scripts\vendor-sherpa.ps1`, checks and reads `RELEASE_NOTES.md`, and runs
`tauri-apps/tauri-action`, which:

- builds the NSIS installer;
- signs the update bundle with the key in the `TAURI_SIGNING_PRIVATE_KEY` secret, producing a
  `.sig` file beside the installer;
- writes `latest.json`, with the release notes as its `notes`;
- creates a **draft** release named `Butterfly Speak vX.Y.Z` holding all three.

The job times out after 150 minutes.

## 4. Check the draft

Before publishing:

- Download the installer from the draft, install it, and check that **Help & about** shows the
  new version. Try each engine once: Cloud, Bring your own key and On-device.
- Open `latest.json`. `version` must be the new version, and the `url` under
  `platforms.windows-x86_64` must download this release's installer, and `notes` must be the
  text of `RELEASE_NOTES.md`.

The installer is not Authenticode-signed, so Windows SmartScreen warns about an unknown publisher
on first run. The update signature below is separate from that warning and does not remove it.

## 5. Publish

Publish the draft. Installed copies read
`https://github.com/Deveshu04/Butterfly-Speak/releases/latest/download/latest.json`
(`plugins.updater.endpoints` in `src-tauri/tauri.conf.json`), and GitHub's `latest` points to the
newest published release that is not a draft or a pre-release. From then on, each installed copy
offers the update at its next check: 30 seconds after launch, then every eight hours.

To stop a bad release from spreading, turn it back into a draft or delete it: `latest` then points
to the previous release, which installed copies ignore because its version is lower. Copies that
already updated get the fix with the next release.

## The signing key

Updates are signed with a minisign key pair made by `pnpm tauri signer generate`.

- The **public key** is `plugins.updater.pubkey` in `src-tauri/tauri.conf.json`. It is compiled
  into every build, and a build accepts only updates signed with the matching private key.
- The **private key** is a file the maintainer keeps outside the repository, and its contents are
  the `TAURI_SIGNING_PRIVATE_KEY` repository secret. The release workflow passes an empty
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, so the key must be one generated without a password; a key
  with a password needs a second secret and a change to the workflow.
- **Back it up offline**, on storage that is not the build machine and not a cloud account the
  repository depends on, and check that the backup can be read.
- **If it is lost, installed copies are stranded.** No later build can be signed so that they
  accept it; every user would have to download and install a build carrying a new public key by
  hand.
- If it leaks, anyone who can also get a `latest.json` in front of installed copies can make them
  install their build. Generate a new pair, ship the new public key in a release signed with the
  old key, and only then retire the old key.

The private key never goes into the repository, a log, an issue or a chat. Set the secret from the
file without printing it, for example:

```powershell
Get-Content <path to the key file> -Raw | gh secret set TAURI_SIGNING_PRIVATE_KEY -R Deveshu04/Butterfly-Speak
```

## Repository secrets

| Secret | Set by | Used for |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | The maintainer, under **Settings → Secrets and variables → Actions** | Signing the update bundle in `release.yml` |
| `GITHUB_TOKEN` | GitHub Actions, for every run | Creating the draft release and uploading its files (`release.yml`, with `contents: write`), and recording CLA signatures (`cla.yml`) |

Nothing else is needed. The CI workflow (`ci.yml`) uses no secrets.

## A signed build on your own machine

`pnpm tauri build` signs the update bundle too. It needs `TAURI_SIGNING_PRIVATE_KEY` set to the
key's contents or its path, and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` present and empty; without
the password variable the build stops and waits for a password. Without the key it fails at the
signing step.

In PowerShell, `$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ''` does not work: assigning an empty
string deletes the variable, and so does `[Environment]::SetEnvironmentVariable` with `''`.
Windows' own `SetEnvironmentVariable` keeps it present and empty for the processes the shell
starts:

```powershell
Add-Type -Namespace Win32 -Name Env -MemberDefinition '[DllImport("kernel32.dll", CharSet = CharSet.Unicode)] public static extern bool SetEnvironmentVariable(string name, string value);'
[Win32.Env]::SetEnvironmentVariable('TAURI_SIGNING_PRIVATE_KEY_PASSWORD', '') | Out-Null
$env:TAURI_SIGNING_PRIVATE_KEY = '<path to the key file>'
pnpm tauri build
```

PowerShell's own `Test-Path Env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD` still answers `False`
afterwards; programs it starts see the variable with an empty value.

To build an installer without signing, set `bundle.createUpdaterArtifacts` to `false` in
`src-tauri/tauri.conf.json` and do not commit that change.

## The relay

The relay in `relay/` is deployed separately with `wrangler`, not by a tag. See
[`relay/README.md`](../relay/README.md).
