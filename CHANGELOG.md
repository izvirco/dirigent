## 0.0.3 (draft)

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

[#101]: https://forge.sebba.dev/izvir/dirigent/changes/101
[#95]: https://forge.sebba.dev/izvir/dirigent/changes/95
[#100]: https://forge.sebba.dev/izvir/dirigent/changes/100
[#97]: https://forge.sebba.dev/izvir/dirigent/changes/97
[#99]: https://forge.sebba.dev/izvir/dirigent/changes/99
[#98]: https://forge.sebba.dev/izvir/dirigent/changes/98

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

Dirigent runs pi coding agents in your projects: a thread per task, an inbox for the ones waiting on you,
sub-agents, and projects on other machines next to your own. This first release is for Windows.
