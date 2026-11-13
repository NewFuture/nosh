# nosh desktop UI prototype

A dependency-free, interactive design prototype. Open `index.html` directly in
a browser, or serve this directory from the repository root:

```powershell
python -m http.server 4173 --bind 127.0.0.1 --directory tools\desktop-prototype
```

Open `http://127.0.0.1:4173`. No Rust build, model download or npm install is
needed. This is not a desktop application or a production frontend.

The separate [native Rust desktop](../../docs/DESKTOP.md) now implements real
local nosh and OpenSSH terminals. This browser prototype remains simulated;
its enhanced remote Agent cards are not capabilities of the native application.

## What to try

- Switch between local and staging tabs; drafts and history belong to a session.
- Approve or deny the staging assistant's read-only port inspection.
- Insert a suggestion into the terminal, then press Enter to simulate running it.
- Try `pwd`, `ls`, `git status`, `clear` or `help` in the terminal.
- Select production to preview SSH host verification and basic SSH mode.
- Add a demo SSH connection, disconnect it and reconnect without replaying commands.
- Open history, search connections with Ctrl/Cmd+K, or change theme and text size.
- Collapse the connections or assistant panel; narrow windows use a single pane.

## Explicit boundaries

Everything is simulated and held in memory. Reloading resets it. Commands,
SSH connections, fingerprints, model state and AI replies are illustrative.
No credentials are accepted, no real command executes and no host is contacted.
The prototype has no external fonts, scripts, analytics or other network services.

The staging fixture illustrates an enhanced remote nosh session; new connections
illustrate basic SSH with suggestions only. Approval is always Confirm.
Suggestions never execute on insertion or overwrite a nonempty terminal draft.
Changing tabs does not move a task or permission to another host.

This directory is intentionally independent of the Rust workspace. It does not
change the current CLI or claim that a desktop/remote runtime is implemented.
