# Contributing

## Setup

Follow [Build from source](README.md#build-from-source) in the README.

## Checks

Run the gate before you open a pull request:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\gate.ps1
```

It runs `cargo test --lib`, `cargo test --bins` and `cargo check --all-targets` in `src-tauri`
(any compiler warning fails it), then `pnpm check` and `pnpm test` (the interface's pure modules
in `tests/web` and the notes migration harness), then the uninstaller hook's harness
(`src-tauri\windows\test-hooks.ps1`) once a bundle has fetched Tauri's `makensis`. CI runs the
same script on every pull request, where the hook harness is skipped. If you change `relay/`,
also run `npm ci`, `npx tsc --noEmit -p .` and `npx vitest run` in that directory.

## Changes and commits

- A change to behaviour starts with a test that fails without it.
- One commit per logical change.
- The subject says in plain English what the commit does, for example
  `Keep each test's listener bound for the whole test`.
- The body says why the change is needed.

## Contributor License Agreement

Contributions are accepted under the [Contributor License Agreement](CLA.md). On your first pull
request, sign it by commenting:

```
I have read the CLA Document and I hereby sign the CLA
```

## Finding something to work on

Issues labelled `good first issue` are small and a good way to learn the code. Issues labelled
`advanced` need a working knowledge of the parts they touch.

## Security issues

Do not open a public issue for a vulnerability. See [SECURITY.md](SECURITY.md).

## Code of conduct

Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).
