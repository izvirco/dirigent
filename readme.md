# Dirigent

Dirigent is a native desktop workspace for running and managing multiple [Pi coding agent](https://github.com/earendil-works/pi) sessions across projects.

Dirigent requires Pi 0.85.1 or newer, running on Node 22.13+ (Node 24 recommended). It launches Pi with a bundled, private extension for session-tree navigation and agent delegation; nothing is installed into the user's Pi configuration.

## Agent handoff

The bundled `dirigent_agents` tool supports `mode: "handoff"` alongside `wait` and `background`. A handoff workflow spawns children (or sends them feedback), returns their handles without waiting, and ends the parent's turn. You can chat normally with the parent while the children work.

Children receive instructions to call `agents.pingParent("question or review summary")` in their own handoff workflow when they need the parent. The message wakes the parent once the child settles, so the parent can immediately reply with `agents.send(...)`. Completion and failure also notify the parent automatically; notifications queue behind an ongoing conversation rather than interrupting it.

Use handoff as the only tool call in a batch. Stop, tree navigation, and session replacement disarm wake-ups; restarts never replay notifications or assignments.

## Windows installation

The per-user installer installs each channel into `%LOCALAPPDATA%\Programs\Dirigent\<channel>` and adds **Open dirigent here** to Explorer's folder and folder-background context menus (under **Show more options** on Windows 11). Non-stable entries include the channel name. The action reuses the running window for that user and channel (or starts one), adds the directory if needed, and focuses its new-thread composer. A plain relaunch brings the existing window forward without changing its current view. The equivalent command is `dirigent.exe --open-project "C:\path\to\project"`.

## Local data

Each build uses its own channel directory, without an intermediate `channels` folder:

| Data | Linux / macOS | Windows |
| --- | --- | --- |
| Configuration | `$XDG_CONFIG_HOME/dirigent/<channel>` (default `~/.config`) | `%APPDATA%\dirigent\<channel>` |
| State and logs | `$XDG_DATA_HOME/dirigent/<channel>/v0` (default `~/.local/share`) | `%APPDATA%\dirigent\<channel>\v0` |
| Cache | `$XDG_CACHE_HOME/dirigent/<channel>/v0` (default `~/.cache`) | `%LOCALAPPDATA%\dirigent\<channel>\v0` |
| Managed workspaces | `$XDG_DATA_HOME/dirigent/<channel>/workspace` | `%LOCALAPPDATA%\dirigent\<channel>\workspace` |

This layout starts fresh databases; older layouts and database schemas are not imported. Uninstalling a channel retains its configuration and data.
