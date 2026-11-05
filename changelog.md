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
