# Dirigent

Dirigent is a native desktop workspace for running and managing multiple [Pi coding agent](https://github.com/earendil-works/pi) sessions across projects.

Dirigent requires Pi 0.85.1 or newer, running on Node 22.13+ (Node 24 recommended). It launches Pi with a bundled, private extension for session-tree navigation and agent delegation; nothing is installed into the user's Pi configuration.

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
