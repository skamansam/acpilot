# Retiring `tui-shell-menus-themes`: the file-by-file evidence

`tui-shell-menus-themes` has 156 commits that `main` does not contain, so
`git rev-list --count main..tui-shell-menus-themes` prints `156`, and the check in
`CONTRIBUTING.md` for dropping a branch fails. This note answers a different
question: does `main` hold every byte that the branch carries?

## Method

`tui-shell` is the line that landed the same work. PR #296 merged it into `main`
on 2026-10-31, and `tui-shell` is an ancestor of `main`. The branch and
`tui-shell` share `0c57cd6` (that merge commit) as their merge base.

For each path that the branch changed after `0c57cd6`, compare the blob ids:

```bash
git rev-parse "tui-shell-menus-themes:$PATH"
git rev-parse "tui-shell:$PATH"
```

Equal ids mean identical bytes.

## Result

The branch changed 263 paths. 261 are byte-identical to `tui-shell`. Two differ,
and the branch holds the older text in both:

- `.github/workflows/ci.yml` — `tui-shell` limits `on: push` to `main` (commit
  `50c12f5`), and the branch still triggers on every branch.
- `CONTRIBUTING.md` — `tui-shell` adds 11 lines that say a pull request publishes
  the required check name, and the branch lacks them.

The table below lists every path. `identical` means the two blob ids match.

## The table

```text
| Path | Branch blob | tui-shell blob | Result |
| --- | --- | --- | --- |
| .github/scripts/todo_issues.py | 42e0860fe836 | 42e0860fe836 | identical |
| .github/workflows/ci.yml | d6940f656ce3 | a835b83bed2f | **differs** |
| .github/workflows/release.yml | 158a7e3a7193 | 158a7e3a7193 | identical |
| .gitignore | 977d9698a0cc | 977d9698a0cc | identical |
| AGENTS.md | 30f8757254d0 | 30f8757254d0 | identical |
| CONTRIBUTING.md | d351fcd8d1e2 | 6e4686fa9409 | **differs** |
| Cargo.lock | a2e632ac1525 | a2e632ac1525 | identical |
| Cargo.toml | e8ab74dc4d44 | e8ab74dc4d44 | identical |
| README.md | 09372b86ecb5 | 09372b86ecb5 | identical |
| TODO.md | d4428aa210f2 | d4428aa210f2 | identical |
| crates/acpilot-agent/Cargo.toml | 76613c0fbd48 | 76613c0fbd48 | identical |
| crates/acpilot-agent/src/keyring.rs | 7562e24e24a2 | 7562e24e24a2 | identical |
| crates/acpilot-agent/src/lib.rs | e5f652489823 | e5f652489823 | identical |
| crates/acpilot-agent/src/manifest.rs | ac8defc98926 | ac8defc98926 | identical |
| crates/acpilot-agent/src/permission.rs | fd347e9ad130 | fd347e9ad130 | identical |
| crates/acpilot-agent/src/pty_agent.rs | 4073b8b37bf5 | 4073b8b37bf5 | identical |
| crates/acpilot-agent/src/registry.rs | 77877c6473b3 | 77877c6473b3 | identical |
| crates/acpilot-agent/src/session.rs | bcda6f7b411f | bcda6f7b411f | identical |
| crates/acpilot-agent/tests/fixtures/mock_agent.rs | 095861cc77af | 095861cc77af | identical |
| crates/acpilot-agent/tests/fixtures/tui_agent.sh | 53fe7ad6428b | 53fe7ad6428b | identical |
| crates/acpilot-agent/tests/pty_agent.rs | 16c9635ed34b | 16c9635ed34b | identical |
| crates/acpilot-agent/tests/registry.rs | 2564241051fc | 2564241051fc | identical |
| crates/acpilot-agent/tests/session.rs | 8f0eeae98475 | 8f0eeae98475 | identical |
| crates/acpilot-cli/Cargo.toml | 014ec7f2dd10 | 014ec7f2dd10 | identical |
| crates/acpilot-cli/src/cli.rs | 695c553fdddf | 695c553fdddf | identical |
| crates/acpilot-cli/src/doctor.rs | 697ebe0b98b0 | 697ebe0b98b0 | identical |
| crates/acpilot-cli/src/export.rs | af51aa29b5c3 | af51aa29b5c3 | identical |
| crates/acpilot-cli/src/lib.rs | 25bb5ce875ae | 25bb5ce875ae | identical |
| crates/acpilot-cli/src/secret.rs | 17a1dc3c292e | 17a1dc3c292e | identical |
| crates/acpilot-cli/tests/cli.rs | 940d157522f4 | 940d157522f4 | identical |
| crates/acpilot-cli/tests/precedence.rs | d663e30b845c | d663e30b845c | identical |
| crates/acpilot-cli/tests/snapshots/cli__ext_help.snap | 6b920e688106 | 6b920e688106 | identical |
| crates/acpilot-cli/tests/snapshots/cli__help.snap | 7418802683b5 | 7418802683b5 | identical |
| crates/acpilot-cli/tests/snapshots/cli__secret_help.snap | 1cc6625d3fbe | 1cc6625d3fbe | identical |
| crates/acpilot-cli/tests/snapshots/cli__serve_help.snap | 9638c8945a13 | 9638c8945a13 | identical |
| crates/acpilot-client/src/daemon.rs | d13303e39c20 | d13303e39c20 | identical |
| crates/acpilot-client/src/lib.rs | 0740d318b96f | 0740d318b96f | identical |
| crates/acpilot-client/tests/terminal.rs | 1d114c2ac8da | 1d114c2ac8da | identical |
| crates/acpilot-core/src/error.rs | eea1095dcb2f | eea1095dcb2f | identical |
| crates/acpilot-demo/src/lib.rs | fa6e87a84fa3 | fa6e87a84fa3 | identical |
| crates/acpilot-demo/src/palette.rs | ae2a0a54824e | ae2a0a54824e | identical |
| crates/acpilot-demo/src/tree.rs | 34f0ae7b26a0 | 34f0ae7b26a0 | identical |
| crates/acpilot-extensions/tests/themes.rs | 4e77c8979b66 | 4e77c8979b66 | identical |
| crates/acpilot-git/src/lib.rs | c6b7e8a014e2 | c6b7e8a014e2 | identical |
| crates/acpilot-gui/Cargo.toml | c3ebb9c42d92 | c3ebb9c42d92 | identical |
| crates/acpilot-gui/src/app.rs | 89fca822cacb | 89fca822cacb | identical |
| crates/acpilot-gui/src/daemon.rs | 648ea956ea2a | 648ea956ea2a | identical |
| crates/acpilot-gui/src/keymap.rs | 72530d076944 | 72530d076944 | identical |
| crates/acpilot-gui/src/lib.rs | 8bd4d2ec4477 | 8bd4d2ec4477 | identical |
| crates/acpilot-gui/src/main.rs | 7f7f15aed2c2 | 7f7f15aed2c2 | identical |
| crates/acpilot-gui/src/palette.rs | 35b9d0051d67 | 35b9d0051d67 | identical |
| crates/acpilot-gui/src/profiles.rs | b2741476a627 | b2741476a627 | identical |
| crates/acpilot-gui/src/source.rs | f4f8af2628c2 | f4f8af2628c2 | identical |
| crates/acpilot-host/Cargo.toml | d484af6b2169 | d484af6b2169 | identical |
| crates/acpilot-host/src/install.rs | b2fa28bc005e | b2fa28bc005e | identical |
| crates/acpilot-host/src/lib.rs | be31567ba13b | be31567ba13b | identical |
| crates/acpilot-host/src/token.rs | 2dac66e11c90 | 2dac66e11c90 | identical |
| crates/acpilot-lsp/Cargo.toml | 3f7dfb2d97df | 3f7dfb2d97df | identical |
| crates/acpilot-lsp/src/client.rs | 78c64901631d | 78c64901631d | identical |
| crates/acpilot-lsp/src/lib.rs | 2e52b253bfc6 | 2e52b253bfc6 | identical |
| crates/acpilot-lsp/src/session.rs | cafd1c8230fe | cafd1c8230fe | identical |
| crates/acpilot-lsp/tests/session.rs | 00876a4b2d3a | 00876a4b2d3a | identical |
| crates/acpilot-profile/Cargo.toml | 032b06e4f34e | 032b06e4f34e | identical |
| crates/acpilot-profile/defaults.toml | 884f8769d138 | 884f8769d138 | identical |
| crates/acpilot-profile/src/catalog.rs | bc66f6fe2b4f | bc66f6fe2b4f | identical |
| crates/acpilot-profile/src/export.rs | 0923c172a7b6 | 0923c172a7b6 | identical |
| crates/acpilot-profile/src/launch.rs | adb32bd8ccb9 | adb32bd8ccb9 | identical |
| crates/acpilot-profile/src/lib.rs | 549611bc11d9 | 549611bc11d9 | identical |
| crates/acpilot-profile/src/prefs.rs | 29882e4c90fc | 29882e4c90fc | identical |
| crates/acpilot-profile/src/project.rs | 2490533e9ac2 | 2490533e9ac2 | identical |
| crates/acpilot-profile/src/settings.rs | d314be88af74 | d314be88af74 | identical |
| crates/acpilot-profile/src/watch.rs | fe9b1b4ebef1 | fe9b1b4ebef1 | identical |
| crates/acpilot-proto/src/event.rs | 9f3d309e51cb | 9f3d309e51cb | identical |
| crates/acpilot-proto/src/file.rs | e11ead9e5c9a | e11ead9e5c9a | identical |
| crates/acpilot-proto/src/git.rs | c6ec6f296364 | c6ec6f296364 | identical |
| crates/acpilot-proto/src/host.rs | 125f94a186b5 | 125f94a186b5 | identical |
| crates/acpilot-proto/src/lib.rs | e6992e4cbc47 | e6992e4cbc47 | identical |
| crates/acpilot-proto/src/lsp.rs | 92bf5e82b939 | 92bf5e82b939 | identical |
| crates/acpilot-proto/src/profile.rs | 99fb2c774ef7 | 99fb2c774ef7 | identical |
| crates/acpilot-proto/src/project.rs | d25350200b28 | d25350200b28 | identical |
| crates/acpilot-proto/src/session.rs | c812772c78b3 | c812772c78b3 | identical |
| crates/acpilot-proto/src/terminal.rs | 087e1e1e67f9 | 087e1e1e67f9 | identical |
| crates/acpilot-proto/src/workspace.rs | b327c2904b3d | b327c2904b3d | identical |
| crates/acpilot-proto/tests/snapshots/openapi_snapshot__openapi.snap | 919dbc0bf1ba | 919dbc0bf1ba | identical |
| crates/acpilot-proto/tests/wire_contract.rs | c9dd5adceeeb | c9dd5adceeeb | identical |
| crates/acpilot-showcase/src/lib.rs | 5575842403dc | 5575842403dc | identical |
| crates/acpilot-store/Cargo.toml | caeca2054865 | caeca2054865 | identical |
| crates/acpilot-store/migrations/0001_init.sql | e7a8d2b36cc6 | e7a8d2b36cc6 | identical |
| crates/acpilot-store/migrations/0002_agent_pins.sql | c8b4cfeb3cc2 | c8b4cfeb3cc2 | identical |
| crates/acpilot-store/migrations/0003_session_profile.sql | e4651284c1b7 | e4651284c1b7 | identical |
| crates/acpilot-store/migrations/index_0001.sql | b5c63d8f6a25 | b5c63d8f6a25 | identical |
| crates/acpilot-store/src/lib.rs | 305946251074 | 305946251074 | identical |
| crates/acpilot-text/src/document.rs | 7753b3defdf1 | 7753b3defdf1 | identical |
| crates/acpilot-text/src/highlight.rs | 5b5a7ce77173 | 5b5a7ce77173 | identical |
| crates/acpilot-text/src/lib.rs | 596a2c61b01e | 596a2c61b01e | identical |
| crates/acpilot-text/src/outline.rs | bed41b0eaa89 | bed41b0eaa89 | identical |
| crates/acpilot-text/src/pane.rs | 33bd4ea3d02f | 33bd4ea3d02f | identical |
| crates/acpilot-text/src/references.rs | 90b7ef1fef06 | 90b7ef1fef06 | identical |
| crates/acpilot-tui/src/app.rs | 18d9ddcbd4c5 | 18d9ddcbd4c5 | identical |
| crates/acpilot-tui/src/backend.rs | 75652bf6529f | 75652bf6529f | identical |
| crates/acpilot-tui/src/editor.rs | ff5a813a5fee | ff5a813a5fee | identical |
| crates/acpilot-tui/src/keymap.rs | 398102a7d228 | 398102a7d228 | identical |
| crates/acpilot-tui/src/lib.rs | 0a155f76e332 | 0a155f76e332 | identical |
| crates/acpilot-tui/src/markdown.rs | ae323ef311d1 | ae323ef311d1 | identical |
| crates/acpilot-tui/src/ui.rs | 4be27c13f217 | 4be27c13f217 | identical |
| crates/acpilot-tui/tests/fixtures/colour_probe.rs | c5790bcae0b9 | c5790bcae0b9 | identical |
| crates/acpilot-web/assets/index.html | 8a310420591b | 8a310420591b | identical |
| crates/acpilot-web/src/lib.rs | ca67d7efd862 | ca67d7efd862 | identical |
| crates/acpilotd-helper/Cargo.toml | 2b74338acea5 | 2b74338acea5 | identical |
| crates/acpilotd-helper/src/base64.rs | 0841b9d2261b | 0841b9d2261b | identical |
| crates/acpilotd-helper/src/lsp.rs | 33ee5a13831e | 33ee5a13831e | identical |
| crates/acpilotd-helper/src/main.rs | 838ef419d885 | 838ef419d885 | identical |
| crates/acpilotd-helper/src/output.rs | 081a2b68129a | 081a2b68129a | identical |
| crates/acpilotd-helper/src/pty.rs | 2c326c73e339 | 2c326c73e339 | identical |
| crates/acpilotd-helper/tests/stdio.rs | 43469929acd7 | 43469929acd7 | identical |
| crates/acpilotd/Cargo.toml | d2875cbb3bc8 | d2875cbb3bc8 | identical |
| crates/acpilotd/build.rs | fd8d263850e7 | fd8d263850e7 | identical |
| crates/acpilotd/src/auth.rs | e3aa857e64e3 | e3aa857e64e3 | identical |
| crates/acpilotd/src/base64.rs | a8dbb3559b08 | a8dbb3559b08 | identical |
| crates/acpilotd/src/commit.rs | bb7dc6ab22c7 | bb7dc6ab22c7 | identical |
| crates/acpilotd/src/config.rs | 2a2c86830472 | 2a2c86830472 | identical |
| crates/acpilotd/src/embedded.rs | 848c8a212fa6 | 848c8a212fa6 | identical |
| crates/acpilotd/src/files.rs | 680b95cda9d9 | 680b95cda9d9 | identical |
| crates/acpilotd/src/git.rs | 71bcaccf590a | 71bcaccf590a | identical |
| crates/acpilotd/src/helper.rs | e1690dfb66ef | e1690dfb66ef | identical |
| crates/acpilotd/src/hosts.rs | b572c9e718a0 | b572c9e718a0 | identical |
| crates/acpilotd/src/lib.rs | 3889b6af45b5 | 3889b6af45b5 | identical |
| crates/acpilotd/src/lsp.rs | 7b1dee7d779c | 7b1dee7d779c | identical |
| crates/acpilotd/src/lsp_remote.rs | 81390ad70e07 | 81390ad70e07 | identical |
| crates/acpilotd/src/main.rs | f4e5af41a181 | f4e5af41a181 | identical |
| crates/acpilotd/src/pidfile.rs | f278d25a99ed | f278d25a99ed | identical |
| crates/acpilotd/src/platform/mod.rs | 1ebb14eb92e2 | 1ebb14eb92e2 | identical |
| crates/acpilotd/src/platform/unix.rs | c0f66437ad20 | c0f66437ad20 | identical |
| crates/acpilotd/src/platform/windows.rs | f47dc7cfdfb0 | f47dc7cfdfb0 | identical |
| crates/acpilotd/src/profiles.rs | 6f736b34f3e0 | 6f736b34f3e0 | identical |
| crates/acpilotd/src/project.rs | a33d0a3e4df4 | a33d0a3e4df4 | identical |
| crates/acpilotd/src/pty_remote.rs | 8b05b9a5d600 | 8b05b9a5d600 | identical |
| crates/acpilotd/src/sessions.rs | ca6b34323786 | ca6b34323786 | identical |
| crates/acpilotd/src/stats.rs | be09a7dcd399 | be09a7dcd399 | identical |
| crates/acpilotd/src/terminals.rs | f4e3d8ac7cbf | f4e3d8ac7cbf | identical |
| crates/acpilotd/src/trust.rs | 74ec82663955 | 74ec82663955 | identical |
| crates/acpilotd/src/workspaces.rs | 3690a76016f4 | 3690a76016f4 | identical |
| crates/acpilotd/tests/acceptance.rs | cae9409eb5ee | cae9409eb5ee | identical |
| crates/acpilotd/tests/agent_terminals.rs | 7d1d41989094 | 7d1d41989094 | identical |
| crates/acpilotd/tests/auth.rs | a82cfdfd003f | a82cfdfd003f | identical |
| crates/acpilotd/tests/bundled.rs | 5e9219306c5b | 5e9219306c5b | identical |
| crates/acpilotd/tests/commit_message.rs | 45cea3857019 | 45cea3857019 | identical |
| crates/acpilotd/tests/common/mod.rs | 5c4d5762b9d5 | 5c4d5762b9d5 | identical |
| crates/acpilotd/tests/events.rs | c604d4b93b1c | c604d4b93b1c | identical |
| crates/acpilotd/tests/files.rs | 570d96601149 | 570d96601149 | identical |
| crates/acpilotd/tests/files_mutation.rs | bb8007b77e85 | bb8007b77e85 | identical |
| crates/acpilotd/tests/git.rs | fce4f6699cb8 | fce4f6699cb8 | identical |
| crates/acpilotd/tests/helper_install.rs | 7f9ffe494de5 | 7f9ffe494de5 | identical |
| crates/acpilotd/tests/helper_probe.rs | 828c954266fe | 828c954266fe | identical |
| crates/acpilotd/tests/hosts.rs | c40dc0e4f6a5 | c40dc0e4f6a5 | identical |
| crates/acpilotd/tests/http.rs | ab9bcc50ea35 | ab9bcc50ea35 | identical |
| crates/acpilotd/tests/layers.rs | d19e5fe0c235 | d19e5fe0c235 | identical |
| crates/acpilotd/tests/lsp_diagnostics.rs | 901522a0a9c8 | 901522a0a9c8 | identical |
| crates/acpilotd/tests/lsp_references.rs | 91545f4dd1e5 | 91545f4dd1e5 | identical |
| crates/acpilotd/tests/lsp_status.rs | 64294e2bc062 | 64294e2bc062 | identical |
| crates/acpilotd/tests/mode.rs | c62caa5203c1 | c62caa5203c1 | identical |
| crates/acpilotd/tests/profiles.rs | 7dbf38a247f9 | 7dbf38a247f9 | identical |
| crates/acpilotd/tests/project.rs | 822e6d6e3d8e | 822e6d6e3d8e | identical |
| crates/acpilotd/tests/remote_files.rs | 8611d84baf22 | 8611d84baf22 | identical |
| crates/acpilotd/tests/remote_git.rs | 56282854106f | 56282854106f | identical |
| crates/acpilotd/tests/remote_terminals.rs | 022cb57796b7 | 022cb57796b7 | identical |
| crates/acpilotd/tests/session_stream.rs | ae5ccbe85293 | ae5ccbe85293 | identical |
| crates/acpilotd/tests/sessions.rs | d8c783baa054 | d8c783baa054 | identical |
| crates/acpilotd/tests/snapshots/git__diff_scopes.snap | 47255619b3cb | 47255619b3cb | identical |
| crates/acpilotd/tests/terminal_attach.rs | 491725443f79 | 491725443f79 | identical |
| crates/acpilotd/tests/terminal_process.rs | 5953323aef90 | 5953323aef90 | identical |
| crates/acpilotd/tests/terminal_routes.rs | 4c5d19e1e1b8 | 4c5d19e1e1b8 | identical |
| crates/acpilotd/tests/terminals.rs | e180356b4d42 | e180356b4d42 | identical |
| crates/acpilotd/tests/trust.rs | 5f08e7302749 | 5f08e7302749 | identical |
| crates/acpilotd/tests/workspaces.rs | aaa9bb1f7606 | aaa9bb1f7606 | identical |
| crates/acpilotd/tests/workspaces_remote.rs | 15790c08b7a4 | 15790c08b7a4 | identical |
| docs/adr/0001-record-architecture-decisions.md | fadbb20b40d2 | fadbb20b40d2 | identical |
| docs/adr/0007-profiles-isolation-and-storage.md | cac72879c40b | cac72879c40b | identical |
| docs/adr/0010-install-provenance.md | 24581bca56ea | 24581bca56ea | identical |
| docs/adr/0012-profile-inheritance.md | f4964544e3cc | f4964544e3cc | identical |
| docs/api.md | 436c017e9938 | 436c017e9938 | identical |
| docs/notes/acp-transcripts.md | ab2acd6eeb43 | ab2acd6eeb43 | identical |
| docs/notes/gui-bench.md | c53d3741d5e4 | c53d3741d5e4 | identical |
| docs/notes/handoff-2026-10-21.md | fca8f5f654a4 | fca8f5f654a4 | identical |
| docs/notes/lsp-servers.md | 23167979ff26 | 23167979ff26 | identical |
| docs/notes/pty-agents.md | b98a61950c9c | b98a61950c9c | identical |
| docs/notes/ratcn.md | 22f49f4ed743 | 22f49f4ed743 | identical |
| docs/notes/ssh-agent.md | c833625504d5 | c833625504d5 | identical |
| docs/notes/tui-shell.md | 87a9a1923fce | 87a9a1923fce | identical |
| docs/notes/wsl-helper.md | 9aaad665e05a | 9aaad665e05a | identical |
| extensions/acpilot.agents/agents.toml | 4d9840105026 | 4d9840105026 | identical |
| gstack-howto.md | 95a7bf7b0bd9 | 95a7bf7b0bd9 | identical |
| marketplace/README.md | a53cd2cff244 | a53cd2cff244 | identical |
| probes/fixtures/acp/README.md | 8553ac711a1d | 8553ac711a1d | identical |
| site/README.md | 129f2d6a6125 | 129f2d6a6125 | identical |
| site/package-lock.json | 23f52a577f23 | 23f52a577f23 | identical |
| site/src/content/docs/faq.md | 166fbab7dfc2 | 166fbab7dfc2 | identical |
| site/src/content/docs/flight-deck/api.md | 5f28283b2227 | 5f28283b2227 | identical |
| site/src/content/docs/flight-deck/cli.md | 374752a27468 | 374752a27468 | identical |
| site/src/content/docs/flight-deck/desktop.md | 48b81527e110 | 48b81527e110 | identical |
| site/src/content/docs/flight-deck/tui.md | 0710a58b64f2 | 0710a58b64f2 | identical |
| site/src/content/docs/flight-deck/web.md | 0fcba3b6a7f0 | 0fcba3b6a7f0 | identical |
| site/src/content/docs/flight-test/index.md | f890314851b5 | f890314851b5 | identical |
| site/src/content/docs/flight-test/model-to-agent.md | c92d119fcf85 | c92d119fcf85 | identical |
| site/src/content/docs/flight-test/providers.md | 14676da17d4a | 14676da17d4a | identical |
| site/src/content/docs/flight-test/recipes.md | 2e3115d7d008 | 2e3115d7d008 | identical |
| site/src/content/docs/flight-test/remote-control.md | f69734fea45e | f69734fea45e | identical |
| site/src/content/docs/flight-test/workbench.md | 4c7c38af1730 | 4c7c38af1730 | identical |
| site/src/content/docs/ground-school/first-flight.md | 3cc6a0b1a18f | 3cc6a0b1a18f | identical |
| site/src/content/docs/ground-school/installation.md | 6230d850abf9 | 6230d850abf9 | identical |
| site/src/content/docs/index.mdx | 094640f11d7e | 094640f11d7e | identical |
| site/src/content/docs/mod-shop/index.md | 311dcc84395d | 311dcc84395d | identical |
| site/src/content/docs/mod-shop/manifest.md | ae3d3724f5bb | ae3d3724f5bb | identical |
| site/src/content/docs/mod-shop/marketplace.md | 03486b584f97 | 03486b584f97 | identical |
| site/src/content/docs/mod-shop/planned.md | eb81a132cba7 | eb81a132cba7 | identical |
| site/src/content/docs/mod-shop/registry.md | 92da3eb04d14 | 92da3eb04d14 | identical |
| site/src/content/docs/mod-shop/themes.mdx | cd68abc23055 | cd68abc23055 | identical |
| site/src/content/docs/reference/config.md | bab817297d7e | bab817297d7e | identical |
| site/src/content/docs/reference/manifest.md | ef0c3367892a | ef0c3367892a | identical |
| site/src/content/docs/safety/permissions.md | ada6f70ad2aa | ada6f70ad2aa | identical |
| site/src/content/docs/safety/secrets.md | 994acd6a6409 | 994acd6a6409 | identical |
| site/src/content/docs/showcase.mdx | 07bbc32fc81c | 07bbc32fc81c | identical |
| site/src/content/docs/status.md | 78e908c6e864 | 78e908c6e864 | identical |
| site/src/content/docs/systems/agents.md | 79f6f10a3ea1 | 79f6f10a3ea1 | identical |
| site/src/content/docs/systems/hosts.md | 331ef58ef071 | 331ef58ef071 | identical |
| site/src/content/docs/systems/language-servers.md | d2432e6fd601 | d2432e6fd601 | identical |
| site/src/content/docs/systems/profiles.md | 9a9e4083bba2 | 9a9e4083bba2 | identical |
| site/src/content/docs/systems/program-as-agent.md | 1fc8cee15b78 | 1fc8cee15b78 | identical |
| site/src/data/desktop.json | f1acf36bb014 | f1acf36bb014 | identical |
| site/src/data/screens.json | a573349cc9b1 | a573349cc9b1 | identical |
| web/.npmrc | 7f8fbb3d0571 | 7f8fbb3d0571 | identical |
| web/e2e/cockpit.spec.ts | 8f3425b8a13e | 8f3425b8a13e | identical |
| web/package-lock.json | efaf0a7bf3df | efaf0a7bf3df | identical |
| web/package.json | f4cfbe9bf87e | f4cfbe9bf87e | identical |
| web/playwright.config.ts | 2b8fc37eb86b | 2b8fc37eb86b | identical |
| web/src/App.svelte | 9c5220212806 | 9c5220212806 | identical |
| web/src/App.test.ts | a22ad7329b43 | a22ad7329b43 | identical |
| web/src/Editor.svelte | 604641f27add | 604641f27add | identical |
| web/src/Editor.test.ts | bbc44700d0b9 | bbc44700d0b9 | identical |
| web/src/Session.svelte | 0a2ac06bf990 | 0a2ac06bf990 | identical |
| web/src/SessionView.test.ts | 5322b128025f | 5322b128025f | identical |
| web/src/SourceControl.svelte | 1b661594dcc1 | 1b661594dcc1 | identical |
| web/src/SourceControl.test.ts | 3df4a53beebe | 3df4a53beebe | identical |
| web/src/Terminal.svelte | 1e0697aee0fe | 1e0697aee0fe | identical |
| web/src/api.test.ts | c070625c6655 | c070625c6655 | identical |
| web/src/api.ts | b7aaf3eccb14 | b7aaf3eccb14 | identical |
| web/src/api/openapi.json | 77e822e065af | 77e822e065af | identical |
| web/src/api/schema.d.ts | a1a0cbcd420e | a1a0cbcd420e | identical |
| web/src/app.css | bd01da834670 | bd01da834670 | identical |
| web/src/files.test.ts | bed65f69d2f1 | bed65f69d2f1 | identical |
| web/src/files.ts | 79632396eefa | 79632396eefa | identical |
| web/src/git-gutter.test.ts | 0e4452769750 | 0e4452769750 | identical |
| web/src/git-gutter.ts | 9ff7f7743606 | 9ff7f7743606 | identical |
| web/src/git.ts | f4eedcb8bd81 | f4eedcb8bd81 | identical |
| web/src/routes.test.ts | 642ed1d33ddf | 642ed1d33ddf | identical |
| web/src/routes.ts | a3ce36703dab | a3ce36703dab | identical |
| web/src/session.test.ts | 8f93776280b3 | 8f93776280b3 | identical |
| web/src/session.ts | 7f88efcd2d44 | 7f88efcd2d44 | identical |
| web/src/terminals.test.ts | 4646b55b8add | 4646b55b8add | identical |
| web/src/terminals.ts | e34a1580d16f | e34a1580d16f | identical |
| web/src/test-fetch.ts | c305836b1d39 | c305836b1d39 | identical |
| xtask/Cargo.toml | 206b423fca8a | 206b423fca8a | identical |
| xtask/src/main.rs | 6754bbeb79a9 | 6754bbeb79a9 | identical |
```

## Why `git cherry` reports 155

`git cherry main tui-shell-menus-themes` marks 155 commits with `+`, which means
"not upstream". A patch id changes when the commit that lands the work differs
from the commit that wrote it. `tui-shell` carries the same trees under other
hashes, and the table compares content rather than patch ids. The count is
therefore not evidence of missing work.

## Conclusion

No work on the branch is missing from `main`. Two files carry older text that
`main` replaced. The branch still fails the commits check in `CONTRIBUTING.md`,
so it stays. A person can decide that tree equivalence is enough, and that
decision belongs in the record next to the deletion.

## Commands

```bash
mb=$(git merge-base main tui-shell-menus-themes)
git diff --name-only "$mb" tui-shell-menus-themes | while read -r p; do
  printf '%s %s %s\n' "$p" \
    "$(git rev-parse "tui-shell-menus-themes:$p")" \
    "$(git rev-parse "tui-shell:$p")"
done
git diff --name-only tui-shell tui-shell-menus-themes
```
