## 0.0.11 (draft)

- Lowercase `agents.md` and `skill.md` are read too ([#199])
- The migration from pi is gone: start 0.0.10 once on an install of 0.0.9 or older before updating it ([#200])
- `agent.toml` no longer holds the proxy's key, so machines that all run 0.0.11 can share it: the proxy moved to `proxy.json` in the data dir ([#201])
- If you published an `agent.toml` of 0.0.10, change your proxy's API key: it was in that file ([#201])
- A warning in the sidebar and in Settings › Proxy when your CLIProxyAPI is 10 releases behind, on hosts that hold its management key ([#204])
- Running timers all change at the same moment, once a second ([#205])
- Remove a project, or archive, delete, rename, pin and snooze a thread, while its host is offline: the host gets the change when it is back ([#203])
- The Projects list says which hosts are offline ([#203])
- Fixed: editing a host's address or key dropped its drafts ([#203])
- A new thread starts with the project, model and reasoning you last picked for one, not those of the thread or project page you came from ([#207])
- Esc takes the focus off the composer where it used to close the right panel, and `i` focuses the composer again; its ring shows while it has focus ([#208])
- Fixed: while you edited a message and scrolled it out of view, Esc and every shortcut did nothing ([#212])
- Toasts: a repeat counts up instead of stacking, a click dismisses one, and the sidebar keeps the session's history ([#211])
- Computer use shows the agent's cursor on your desktop (Windows, X11), over the window it works in; Settings › Agent tools turns it off ([#206])

[#199]: https://forge.sebba.dev/izvir/dirigent/changes/199
[#200]: https://forge.sebba.dev/izvir/dirigent/changes/200
[#201]: https://forge.sebba.dev/izvir/dirigent/changes/201
[#203]: https://forge.sebba.dev/izvir/dirigent/changes/203
[#204]: https://forge.sebba.dev/izvir/dirigent/changes/204
[#205]: https://forge.sebba.dev/izvir/dirigent/changes/205
[#206]: https://forge.sebba.dev/izvir/dirigent/changes/206
[#207]: https://forge.sebba.dev/izvir/dirigent/changes/207
[#208]: https://forge.sebba.dev/izvir/dirigent/changes/208
[#211]: https://forge.sebba.dev/izvir/dirigent/changes/211
[#212]: https://forge.sebba.dev/izvir/dirigent/changes/212

## 0.0.10

Pi was irrational, so we wrote our own agent: MCP servers, codemode and computer use.

- Dirigent's own agent is built in: pi, bun and the bundled runtime are gone ([#191])
- The first start brings your threads, `AGENTS.md`, skills, MCP servers and proxy over, after a backup to `dirigent.bck` ([#191])
- Update the app and every host together: older versions can't talk to this one; Settings › Hosts says which to update ([#191])
- Models come through your CLIProxyAPI alone, set up in Settings › Proxy; Settings › Providers is gone ([#191])
- Gone with pi: `dirigent pi`, packages and extensions, `/skill:name`, prompt templates, `.pi/` settings, `/mcp` ([#191])
- The agent's settings are Settings and `agent.toml` in the config dir ([#191])
- Computer use: agents run and operate GUI apps, on a private screen (Linux, with Xvfb installed) or your desktop ([#178])
- The right panel shows the window an agent works in, with its cursor; click to enlarge ([#178])
- Computer use needs Cua Driver: Windows and Linux installs ship it, on macOS install it yourself ([#178])
- Settings › Agent tools turns computer use and codemode off ([#172], [#178])
- Settings › MCP servers: add, edit and turn off servers, see their tools or why one fails, sign in with OAuth ([#176])
- Settings › MCP servers: a client name, for a URL server that only lets clients it knows sign in, like Figma's ([#180])
- Codemode, on by default: agents batch tool calls in one script ([#165], [#172])
- Dirigent Nightly: the nightly channel is an app of its own, next to a stable Dirigent ([#183])
- Agents can add a project: "clone this repo and add it to dirigent" ([#160])
- Projects: expand one to see its hosts and paths; link with a search, unlink one directory at a time ([#162])
- Several new threads can start at once ([#173])
- A message for a model the host's proxy doesn't serve comes back to the composer, with how to fix it ([#173])
- Warnings show as toasts: an MCP server that won't connect, a failed nix shell or title ([#165])
- Usage and spend count what compacting a thread's context cost ([#191])
- Drag the sidebar and the right panel as wide as you like ([#177])
- Screenshots load when you open them, so a slow connection stays responsive ([#178])
- Fixed: a thread scrolled up while loading jumped to its top ([#159])
- Fixed: adding a folder that already is a project made a second one ([#160])
- Fixed: a stopped thread started again when one of its jobs ended; stop ends them too ([#164])
- Fixed: a steer sent mid-reply showed above that reply in the work log ([#166])
- Fixed: a run started by a job or sub-agent ending kept the last run's work log and timer ([#167], [#191])
- Fixed: on Wayland, ctrl or shift pressed while a key was held did nothing ([#169])
- Fixed: on a remote host, a thread picked up after 30 idle minutes missed the prompt cache ([#170])
- Fixed: a sub-agent whose model refused its first message was never heard of again ([#173])
- Fixed: Settings froze on an MCP server with its tools unfolded ([#184])

[#159]: https://forge.sebba.dev/izvir/dirigent/changes/159
[#160]: https://forge.sebba.dev/izvir/dirigent/changes/160
[#162]: https://forge.sebba.dev/izvir/dirigent/changes/162
[#164]: https://forge.sebba.dev/izvir/dirigent/changes/164
[#165]: https://forge.sebba.dev/izvir/dirigent/changes/165
[#166]: https://forge.sebba.dev/izvir/dirigent/changes/166
[#167]: https://forge.sebba.dev/izvir/dirigent/changes/167
[#169]: https://forge.sebba.dev/izvir/dirigent/changes/169
[#170]: https://forge.sebba.dev/izvir/dirigent/changes/170
[#172]: https://forge.sebba.dev/izvir/dirigent/changes/172
[#173]: https://forge.sebba.dev/izvir/dirigent/changes/173
[#176]: https://forge.sebba.dev/izvir/dirigent/changes/176
[#177]: https://forge.sebba.dev/izvir/dirigent/changes/177
[#178]: https://forge.sebba.dev/izvir/dirigent/changes/178
[#180]: https://forge.sebba.dev/izvir/dirigent/changes/180
[#183]: https://forge.sebba.dev/izvir/dirigent/changes/183
[#184]: https://forge.sebba.dev/izvir/dirigent/changes/184
[#191]: https://forge.sebba.dev/izvir/dirigent/changes/191

## 0.0.9

The CLIProxyAPI provider extension works on installs.

- Fixed: models behind the CLIProxyAPI provider extension failed with "Codex protocol stream is unavailable" on installs ([#156])
- The model picker lists saved presets by model, then effort ([#155])

[#155]: https://forge.sebba.dev/izvir/dirigent/changes/155
[#156]: https://forge.sebba.dev/izvir/dirigent/changes/156

## 0.0.8

A newer pi, and your CLIProxyAPI accounts' limits.

- Pi 1.5.1: Claude Fable 5.2, warmer prompt caches ([#151])
- Extension providers show before a thread opens ([#152])
- Usage › Proxy: your CLIProxyAPI accounts' limits, set up in Settings › Proxy ([#153])
- Usage: a taller chart ([#153])
- Leave the leaderboard in Settings › Forge providers ([#153])

[#151]: https://forge.sebba.dev/izvir/dirigent/changes/151
[#152]: https://forge.sebba.dev/izvir/dirigent/changes/152
[#153]: https://forge.sebba.dev/izvir/dirigent/changes/153

## 0.0.7

Hosts over ssh or the internet, a forge sign-in, and long commands that don't block.

- `ssh://` hosts, tunneled with your own ssh ([#140])
- Public hosts behind a TLS proxy, with API keys or your forge account ([#140], [#144])
- Settings › Hosts: add and edit hosts, hover a failing one for why ([#140])
- Sign in to the forge in Settings › Forge providers, or with `dirigent login` ([#144])
- The leaderboard uses that sign-in: sign in and Join again, your place and usage stay ([#144])
- Long `bash` commands move to the background after 10 s, so the agent hears you mid-build ([#145])
- Show or hide the lineage and the work log in the right panel ([#148])
- The window reopens where you left it ([#116])
- Windows: `dirigent` is on your PATH ([#142])
- Shift+Backspace deletes like Backspace ([#143])

[#140]: https://forge.sebba.dev/izvir/dirigent/changes/140
[#144]: https://forge.sebba.dev/izvir/dirigent/changes/144
[#145]: https://forge.sebba.dev/izvir/dirigent/changes/145
[#148]: https://forge.sebba.dev/izvir/dirigent/changes/148
[#116]: https://forge.sebba.dev/izvir/dirigent/changes/116
[#142]: https://forge.sebba.dev/izvir/dirigent/changes/142
[#143]: https://forge.sebba.dev/izvir/dirigent/changes/143

## 0.0.6

A first-run setup, offline hosts you can see, and quieter copy buttons.

- A setup on first launch: theme, provider and project ([#136])
- The transparent theme is Linux-only for now ([#136])
- Send shows `host offline` while the host is down, and keeps your message ([#135])
- The workspace chip stays put while a thread runs ([#134])
- The project picker lists names, with their hosts on the right ([#133])
- Inbox and Workpool chevrons always show, counts beside them ([#131])
- Copy buttons show a check instead of a toast ([#138])
- No more toasts for images the host doesn't have ([#130])
- Usage chart values no longer wrap ([#137])

[#136]: https://forge.sebba.dev/izvir/dirigent/changes/136
[#135]: https://forge.sebba.dev/izvir/dirigent/changes/135
[#134]: https://forge.sebba.dev/izvir/dirigent/changes/134
[#133]: https://forge.sebba.dev/izvir/dirigent/changes/133
[#131]: https://forge.sebba.dev/izvir/dirigent/changes/131
[#138]: https://forge.sebba.dev/izvir/dirigent/changes/138
[#130]: https://forge.sebba.dev/izvir/dirigent/changes/130
[#137]: https://forge.sebba.dev/izvir/dirigent/changes/137

## 0.0.5

Keyboard-driven menus and inputs, a ruler for long threads, and no more stray dev servers.

- Threads that leave a dev server running land in the Inbox; archiving stops it ([#112])
- Copy button on code blocks ([#117])
- A pill to jump back to the latest output when scrolled up ([#115])
- Move and select by word in text inputs ([#113])
- Every menu works from the keyboard ([#125], [#126])
- Project search lists name matches first ([#125])
- Drag the thread ruler to scrub; long threads keep it readable ([#123])
- One `ask_user` form at a time ([#124])
- Sidebar card popups wrap long lines instead of cutting them ([#127])
- Images in replies survive reopening a thread ([#128])

[#112]: https://forge.sebba.dev/izvir/dirigent/changes/112
[#117]: https://forge.sebba.dev/izvir/dirigent/changes/117
[#115]: https://forge.sebba.dev/izvir/dirigent/changes/115
[#113]: https://forge.sebba.dev/izvir/dirigent/changes/113
[#125]: https://forge.sebba.dev/izvir/dirigent/changes/125
[#126]: https://forge.sebba.dev/izvir/dirigent/changes/126
[#123]: https://forge.sebba.dev/izvir/dirigent/changes/123
[#124]: https://forge.sebba.dev/izvir/dirigent/changes/124
[#127]: https://forge.sebba.dev/izvir/dirigent/changes/127
[#128]: https://forge.sebba.dev/izvir/dirigent/changes/128

## 0.0.4

Provider sign-in without a terminal, ordered projects, and a usage leaderboard.

- Sign in to model providers in Settings › Providers, per host ([#96])
- `dirigent pi …` runs the bundled pi ([#105])
- Pi's settings move to `<config dir>/agent` ([#105])
- Messages sent as a thread finishes no longer get stuck or doubled ([#106])
- Drag to reorder projects ([#107])
- Search in the new-thread project menu ([#107])
- Pi 1.5.0: faster streaming, GPT-6.1 Terra ([#109])
- Usage leaderboard: race everyone on cost and tokens ([#110])
- Usage charts by cost, tokens, output or messages ([#110])

[#96]: https://forge.sebba.dev/izvir/dirigent/changes/96
[#105]: https://forge.sebba.dev/izvir/dirigent/changes/105
[#106]: https://forge.sebba.dev/izvir/dirigent/changes/106
[#107]: https://forge.sebba.dev/izvir/dirigent/changes/107
[#109]: https://forge.sebba.dev/izvir/dirigent/changes/109
[#110]: https://forge.sebba.dev/izvir/dirigent/changes/110

## 0.0.3

Edit and fork threads, a macOS build, and better handling of images.

- Edit a sent message and rerun the thread from there ([#101])
- Fork a thread from any message ([#101])
- macOS build for Apple silicon ([#95])
- This changelog after updates; click the version to reopen it ([#100])
- Stable and nightly release channels ([#100])
- Attached images are saved as files for the agent ([#97])
- Threads with many images load in about a second ([#99])
- Images load reliably over slow or dropped connections ([#99])
- Queued messages show and keep their images ([#98])
- Context compaction shows on the work line with a timer; a failed one leaves its error in the thread ([#104])

[#101]: https://forge.sebba.dev/izvir/dirigent/changes/101
[#95]: https://forge.sebba.dev/izvir/dirigent/changes/95
[#100]: https://forge.sebba.dev/izvir/dirigent/changes/100
[#97]: https://forge.sebba.dev/izvir/dirigent/changes/97
[#99]: https://forge.sebba.dev/izvir/dirigent/changes/99
[#98]: https://forge.sebba.dev/izvir/dirigent/changes/98
[#104]: https://forge.sebba.dev/izvir/dirigent/changes/104

## 0.0.2

Tidier threads: rename them, fold away archived sub-agents, and archive only finished work.

- Right-click a thread to rename it or regenerate its title ([#94])
- Only idle threads archive; a new message unarchives ([#87])
- The agent tree folds archived sub-agents into one line ([#93])
- `async_bash` waits for jobs and sub-agents ([#91])
- The work log remembers where you left it ([#88])
- Long text in one-line fields scrolls sideways ([#94])
- The version shows beside the sidebar's title ([#92])

[#94]: https://forge.sebba.dev/izvir/dirigent/changes/94
[#87]: https://forge.sebba.dev/izvir/dirigent/changes/87
[#93]: https://forge.sebba.dev/izvir/dirigent/changes/93
[#91]: https://forge.sebba.dev/izvir/dirigent/changes/91
[#88]: https://forge.sebba.dev/izvir/dirigent/changes/88
[#92]: https://forge.sebba.dev/izvir/dirigent/changes/92

## 0.0.1

Dirigent comes to Linux.

- Linux build (x86_64) that updates itself ([#85])
- ctrl+Backspace and ctrl+Delete delete a word ([#86])
- Renewing pi sign-ins works in the installed app ([#84])

[#85]: https://forge.sebba.dev/izvir/dirigent/changes/85
[#86]: https://forge.sebba.dev/izvir/dirigent/changes/86
[#84]: https://forge.sebba.dev/izvir/dirigent/changes/84

## 0.0.0

The best way to combobulate your codebase.
