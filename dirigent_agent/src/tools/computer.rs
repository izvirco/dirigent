//! Computer use: the `computer` tool opens a screen for the agent to run and operate GUI apps
//! on, when a task needs one. Nothing runs until it is called. A screen is one of two, and
//! the agent picks (the user may say which):
//!
//! - private: an X display of the thread's own (Xvfb), which nobody sees but in the thread:
//!   the place for the agent's own work (trying an app, testing a UI). Linux only, and only
//!   where there is an `Xvfb`: on a machine that can have none the desktop stands in, and
//!   the agent is told so. One that doesn't start is an error, never the desktop. It closes
//!   with the apps on it.
//! - desktop: the desktop of the machine the thread runs on, the user's own. The apps on it
//!   stay when it closes, the ones the agent launched too: the user may be working in them.
//!
//! Each is operated through Cua Driver (<https://github.com/trycua/cua>, MIT), started as an
//! MCP server of the session when its screen opens (`Servers::add`): `private_screen` or
//! `user_desktop`. The model lists windows, reads a window's accessibility tree and
//! screenshot, clicks and types, on the desktop in the background where the app allows.
//! `cua-driver` and `Xvfb` are the ones next to this executable, where an install ships
//! them, else the ones on PATH (`program`): both in a dev checkout (the nix dev shell has
//! them), the driver on macOS, Xvfb where an install leaves it to the system
//! (`packaging/README.md`).
//!
//! The driver is made for a desktop: its guide and most of its tools go by windows and their
//! accessibility trees, which a private screen (no window manager, apps drawn in software)
//! mostly hasn't got. So a private screen has a guide of ours in the driver's place
//! (`private_guide`: operate it by its pixels) and only the tools that takes
//! (`PRIVATE_TOOLS`). The desktop has the driver as it comes.
//!
//! The thread shows what the agent does there. The driver's own recorder writes a folder per
//! action (`turn-NNNNN/`: the target window before and after, and what was done where),
//! which `Recording` reads while a tool of that screen runs and hands on as `Out::Screen`:
//! the window's picture when it changed, and each action with where it clicked. The
//! recorder only keeps the calls of the connection that started it, and that connection is
//! the session's: `Recorder` watches it (`mcp::Watch`) and starts the recording on it. The
//! pictures the agent takes itself are shown as well: the recorder's is taken as the action
//! ends, before an app that draws slowly (in software, on a private screen) shows what it did.

use crate::mcp::{Added, Client, Server, Servers, Watch};
use crate::session::{CODEMODE, Session};
use crate::tool::{Call, Tool, ToolOutput, ToolSpec};
use crate::{Out, Screen};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

/// The tool's name, and its switch in a thread's `tools`.
pub const COMPUTER: &str = "computer";

/// The driver's tools that answer with a picture of what the recorder shows: the screen, a window.
const LOOKS: [&str; 2] = ["get_desktop_state", "get_window_state"];
/// The tools whose picture is to show what the last action did (`Recorder`): those, and a
/// region of a window.
const PICTURES: [&str; 3] = ["get_desktop_state", "get_window_state", "zoom"];
/// How long an app gets to draw what an action did before a picture of it is taken
/// (`Recorder`). Dirigent's own window, drawn in software on a private screen, shows a hover
/// some 70 ms after the pointer moved; this leaves room for a busy machine.
const SETTLE: Duration = Duration::from_millis(250);
/// How long a new connection waits for the recording to start before going ahead without.
const RECORDING_START: Duration = Duration::from_secs(3);
/// How often the recording is read while a computer tool runs.
const POLL: Duration = Duration::from_millis(150);
/// The longest text an action's label quotes, in characters.
const LABEL_CHARS: usize = 48;
/// A private screen's size unless the agent asks for another, and the sizes it may ask for.
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const SIZES: std::ops::RangeInclusive<f64> = 16.0..=8192.0;
/// How long Xvfb gets to come up, and to go; and the driver to say its version.
const START: Duration = Duration::from_secs(5);

/// The driver's tools a private screen offers, a quarter of them: what operating a screen by
/// its pixels takes. The others are for what a private screen hasn't got or dirigent does
/// itself (browser pages over their own protocol, acting through the accessibility tree
/// alone, the driver's sessions, cursor, recorder, settings and updates), and every one of
/// them is a long description for the agent to read past. `click` does the double and the
/// right click too: `double_click` and `right_click` take no `target`. Names are the
/// driver's: check them with every update of it (`packaging/README.md`), a tool it renamed
/// is one gone missing.
const PRIVATE_TOOLS: [&str; 15] = [
    "get_desktop_state",
    "get_window_state",
    "zoom",
    "click",
    "move_cursor",
    "scroll",
    "drag",
    "type_text",
    "press_key",
    "hotkey",
    "launch_app",
    "kill_app",
    "list_windows",
    "clipboard_read",
    "clipboard_write",
];

/// Where a screen is: a display of the thread's own, or the user's desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Where {
    Private,
    Desktop,
}

impl Where {
    /// The MCP server each is operated through: its tools are `mcp__private_screen__…` and
    /// `mcp__user_desktop__…`. Names a server of the user's own is unlikely to have: with
    /// one in `agent.toml` by the same name, the screen doesn't open, and says so.
    fn server(self) -> &'static str {
        match self {
            Self::Private => "private_screen",
            Self::Desktop => "user_desktop",
        }
    }
}

impl fmt::Display for Where {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Private => "private",
            Self::Desktop => "desktop",
        })
    }
}

/// How the private screen on `display` (`:N`) is operated: what its opening says and what
/// `describeNamespace("mcp__private_screen")` returns, in the place of the driver's own guide
/// (`Added::instructions`), which says the opposite (act on a window's elements, check with
/// `verify_state`). Each line is something an agent got wrong without it. The files it names
/// are this user's, for this display: every thread's screen shares `/tmp`, a model uses the
/// names it is shown, and a file another user left there can't be written over.
fn private_guide(display: &str) -> String {
    let user = ["USER", "LOGNAME"].iter().find_map(|name| std::env::var(name).ok()).unwrap_or_else(|| "user".into());
    let number: String = display.chars().filter(char::is_ascii_digit).collect();
    let screen = format!("/tmp/screen-{user}-{number}.png");
    let part = screen.replace(".png", "-part.png");
    [
        "A private screen has no window manager: windows have no title bars and stay where they open, and input aimed at a window (`pid`, `window_id`) doesn't reach every app. So operate it by its pixels:",
        "- See it: `get_desktop_state({})` answers with the screen's picture: pass the image block in its `content` to `image()`.",
        "- Act on it with `target: {kind: \"desktop\", display_id: \"primary\"}` and the `x`, `y` you read off your latest picture of the screen (`get_desktop_state`): `click` (`button: \"right\"` for a right click, `count: 2` for a double click), `move_cursor` (to hover), `scroll` (`direction`, `amount`), `drag` (`from_x`, `from_y`, `to_x`, `to_y`), `type_text` (`text`), `press_key` (`key`, `modifiers`) and `hotkey` (`keys`). Keys go to the window under the pointer, so click into it first. `target` goes alone: with `scope`, `pid` or `window_id` next to it the call is refused.",
        "- See what it did: take the picture after the action in the same script (it is held a moment for the app to draw; after something slower, an animation, look again), or in a later message. Not in another tool call of the same message: those run at the same time, and the picture shows the screen as it was before.",
        "- An action here always answers `effect: \"unverifiable\"`, which says nothing: the picture is the check. A refused call doesn't throw: its result has `isError: true` and the reason in `content[0].text`.",
        &format!("- A cheaper look: `get_desktop_state({{screenshot_out_file: \"{screen}\"}})` writes the picture to that file and answers without it; crop the part you need (with ImageMagick, if it is installed: `magick {screen} -crop 400x200+0+600 {part}`) and read that. `max_image_dimension` answers with a smaller picture, and until your next `get_desktop_state` `x` and `y` are taken as pixels of that one: read them off the picture of the screen you took last, never off an earlier, larger one."),
        "- A window: `list_windows({})` has each one's `pid` and `window_id`, `get_window_state` its picture and, where the app has one (many here don't), its accessibility tree. Once that was called for a window (with `screenshot_out_file`, to keep its picture out of the answer), `zoom` answers with a region of it as it is now, as a small picture: `x1`, `y1`, `x2`, `y2` are pixels of that window's own picture, not of the screen's. A zoom is for reading: don't click by its pixels (`from_zoom` does nothing with this `target`).",
        "- What `describeTool` says of a tool is the driver's text, written for a desktop: where it prefers an `element_token` to pixels, or names a tool this screen doesn't have (`run_steps`, `get_guide`, `list_apps`), go by this guide.",
    ]
    .join("\n")
}

/// The program `name`: the one shipped next to this executable, else one on PATH.
fn program(name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let shipped = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join(&file)));
    let on_path = || std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(&file)).find(|path| path.is_file());
    shipped.filter(|path| path.is_file()).or_else(on_path)
}

/// The driver's executable, asked for its version until one answers, then kept for the
/// process. The error is what the agent is told when this machine has none that runs
/// (macOS without the user's install, a Linux without the X libraries it loads).
async fn driver() -> Result<PathBuf> {
    static FOUND: tokio::sync::OnceCell<PathBuf> = tokio::sync::OnceCell::const_new();
    let find = || async {
        let Some(command) = program("cua-driver") else {
            bail!(
                "Cua Driver, which operates the screen, isn't installed on this machine (`cua-driver` is not on PATH: \
                 https://cua.ai/docs/cua-driver/quickstart)"
            );
        };
        let mut check = Command::new(&command);
        check.arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
        // No console window flashing up from a GUI app.
        #[cfg(windows)]
        check.creation_flags(0x0800_0000);
        let why = match tokio::time::timeout(START, check.output()).await {
            Ok(Ok(output)) if output.status.success() => return Ok(command),
            Ok(Ok(output)) => {
                let said = String::from_utf8_lossy(&output.stderr);
                format!("{}: {}", output.status, said.trim().lines().last().unwrap_or_default())
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!("no answer in {} s", START.as_secs()),
        };
        bail!("Cua Driver doesn't run on this machine ({}): {why}", command.display())
    };
    FOUND.get_or_try_init(find).await.cloned()
}

/// What the shell that holds a private screen's Xvfb runs: the X server (its arguments),
/// until its own stdin closes; then it ends it and waits until it is gone. Its stdout is
/// the X server's alone, so that closes if the server doesn't come up.
const HOLD: &str = r#""$@" & xvfb=$!; exec >/dev/null; read -r _; kill "$xvfb" 2>/dev/null; wait "$xvfb""#;

/// A private screen's X server.
struct Display {
    /// `:N`.
    name: String,
    /// The shell that holds it (`HOLD`), which ends it when its stdin closes: when this is
    /// dropped, or with this process, however that goes (a service may be killed outright).
    /// Its apps go with it.
    hold: Child,
}

impl Display {
    /// The X server private screens are made with. The error is why this machine can have
    /// none: there, and only there, the desktop stands in for one.
    fn xvfb() -> Result<PathBuf, &'static str> {
        if !cfg!(target_os = "linux") {
            return Err("private screens are made on Linux only");
        }
        program("Xvfb").ok_or("Xvfb is not installed")
    }

    /// Start an X display nobody sees (`xvfb`), on a number it picks. The error is why
    /// there is none.
    async fn start(xvfb: &Path, width: u32, height: u32) -> Result<Self> {
        let mut hold = Command::new("sh");
        // `-displayfd 1`: it writes its number to stdout once it takes connections.
        hold.args(["-c", HOLD, "sh"]).arg(xvfb).args(["-displayfd", "1", "-screen", "0", &format!("{width}x{height}x24")]);
        hold.args(["-nolisten", "tcp"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut hold = hold.spawn().map_err(|e| anyhow!("Xvfb did not start: {e}"))?;
        let mut said = String::new();
        // The pipe stays with the shell: nothing more is read from it, and nothing that
        // still writes to it is ended for that.
        let number = match &mut hold.stdout {
            Some(stdout) => tokio::time::timeout(START, BufReader::new(stdout).read_line(&mut said)).await.map(|_| said.trim()),
            None => Ok(""),
        };
        let display = Self { name: format!(":{}", number.unwrap_or_default()), hold };
        if display.name.len() < 2 || !display.name[1..].bytes().all(|b| b.is_ascii_digit()) {
            display.end().await;
            bail!("Xvfb did not start");
        }
        Ok(display)
    }

    /// End it, and wait until it is gone.
    async fn end(mut self) {
        drop(self.hold.stdin.take());
        if tokio::time::timeout(START, self.hold.wait()).await.is_err() {
            tracing::warn!("computer: the X display {} is slow to end", self.name);
        }
    }
}

/// `value` as an entry's `env` takes it to mean just that (`mcp::config::resolve`).
fn literal(value: &str) -> String {
    value.replace('$', "$$")
}

/// A screen's server entry, as one in `agent.toml` would be. Telemetry and the update check
/// are the driver's own and off: an install is updated with dirigent. So is the arrow the
/// driver draws on the screen itself for its agent (`--no-overlay`): the thread shows where
/// the agent clicks, and the user's desktop stays theirs. Its state lives in `home`, except
/// on macOS, where the driver in use is the user's own install and `mcp` talks to its daemon.
/// A private screen's driver, and the apps it launches, get that display and no Wayland one.
fn entry(command: &Path, place: Where, home: &Path, display: Option<&str>) -> Server {
    let mut env = vec![("CUA_TELEMETRY_ENABLED", "0".to_owned()), ("CUA_DRIVER_RS_UPDATE_CHECK", "0".to_owned())];
    if !cfg!(target_os = "macos") {
        env.push(("CUA_DRIVER_RS_HOME", literal(&home.to_string_lossy())));
    }
    if let Some(display) = display {
        env.extend([("DISPLAY", display.to_owned()), ("WAYLAND_DISPLAY", String::new()), ("XDG_SESSION_TYPE", "x11".to_owned())]);
    }
    let description = match place {
        Where::Private => {
            "Operate the apps on this thread's private screen by its pixels: launch them, see the screen, click, type, scroll, drag and press keys."
        }
        Where::Desktop => {
            "Operate the apps on the user's desktop: list and launch apps, read a window's accessibility tree and screenshot, click, type, scroll and press keys, in the background where the app allows."
        }
    };
    Server {
        command: Some(command.to_string_lossy().into_owned()),
        args: vec!["mcp".into(), "--no-overlay".into()],
        env: env.into_iter().map(|(name, value)| (name.to_owned(), value)).collect(),
        description: Some(description.into()),
        ..Server::default()
    }
}

/// How a screen's tools are used, for the model that just opened it: from its scripts
/// (`scripts`), or, in a thread without codemode, as tools it now has itself.
fn how(place: Where, scripts: bool) -> String {
    let server = place.server();
    let watched = "The user watches what you do there in this thread.";
    if !scripts {
        return format!(
            "Operate it with the `mcp__{server}__…` tools you have now: a screenshot comes as an image in a tool's result. {watched}"
        );
    }
    let guide = if place == Where::Private { "this screen's guide" } else { "the driver's guide" };
    format!(
        "Operate it from codemode with the `mcp__{server}__…` tools: `describeNamespace(\"mcp__{server}\")` has {guide} and the tool \
         names. A screenshot is an image block of a tool's result: pass it to `image()` to see it. {watched}"
    )
}

/// A screen the thread has open: its recording, and its display if it is a private one.
struct Open {
    recording: Arc<Recording>,
    display: Option<Display>,
}

impl Open {
    /// Close it: its server, its recording, and a private screen's display with the apps
    /// on it. The desktop's apps stay (`Added::leave_started`).
    async fn close(self, place: Where, mcp: &Servers) {
        mcp.remove(place.server()).await;
        self.recording.stop();
        if let Some(display) = self.display {
            display.end().await;
        }
    }
}

/// One thread's computer use: the `computer` tool, the screens it opened, and what they show.
///
/// A call that is dropped half-way (a stopped run lets go of one that takes too long) leaves
/// nothing half open: a display goes when the last handle to it does, and a server that was
/// being added is forgotten (`Servers::add`).
pub struct Computer {
    session: Weak<Session>,
    /// The screens the thread has open. Held for the whole of an open or a close: they take
    /// turns, where a run calls a reply's tools side by side.
    open: tokio::sync::Mutex<HashMap<Where, Open>>,
    shown: Shown,
}

#[derive(Deserialize)]
struct Input {
    screen: Where,
    action: Option<Action>,
    width: Option<f64>,
    height: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    Open,
    Close,
}

impl Computer {
    pub fn new(session: Weak<Session>) -> Self {
        Self { session, open: Default::default(), shown: Default::default() }
    }

    /// Open the screen `place`, or the desktop in a private one's place on a machine that
    /// can have none: what the model is told of it.
    async fn open(&self, session: &Session, open: &mut HashMap<Where, Open>, mut place: Where, width: u32, height: u32) -> Result<String> {
        let command = driver().await?;
        let (mut stood_in, mut display) = (String::new(), None);
        if place == Where::Private && !open.contains_key(&place) {
            match Display::xvfb() {
                // One that doesn't come up (a busy machine is slow to start it) is said: the
                // user's desktop is not the agent's to take because of it.
                Ok(xvfb) => match Display::start(&xvfb, width, height).await {
                    Ok(started) => display = Some(started),
                    Err(e) => bail!(
                        "The private screen didn't open: {e}. Try again, and tell the user if it still doesn't: their desktop is \
                         not yours to take in its place."
                    ),
                },
                Err(why) => {
                    place = Where::Desktop;
                    stood_in = format!(
                        "No private screen can be made on this machine ({why}), so this is the user's desktop in its place: tell them so. "
                    );
                }
            }
        }
        let how = how(place, session.is_on(CODEMODE));
        if let Some(opened) = open.get(&place) {
            let at = opened.display.as_ref().map(|display| format!(" (`DISPLAY={}`)", display.name)).unwrap_or_default();
            return Ok(format!("{stood_in}The {place} screen is open already{at}. {how}"));
        }
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        if place == Where::Desktop && cfg!(target_os = "linux") && !set("DISPLAY") && !set("WAYLAND_DISPLAY") {
            bail!("{stood_in}This machine has no desktop session.");
        }
        let dir = session.env.dirs.data.join("screens").join(&session.env.thread_id).join(place.to_string());
        let update = {
            let session = self.session.clone();
            move |screen| {
                if let Some(session) = session.upgrade() {
                    session.emit(Out::Screen(screen));
                }
            }
        };
        let recording = Recording::new(dir.join("recording"), self.shown.clone(), place == Where::Desktop, update);
        let name = display.as_ref().map(|display| display.name.clone());
        let with = Added {
            // Of its tools a private screen's agent gets `PRIVATE_TOOLS` and no other, and our guide.
            only: name.as_ref().map(|_| PRIVATE_TOOLS.map(str::to_owned).to_vec()),
            instructions: name.as_deref().map(private_guide),
            watch: Some(Arc::new(Recorder { recording: recording.clone(), acted: Mutex::default() })),
            // What the driver launched on the desktop is on the user's screen, maybe in use
            // by now: it stays when the driver goes. A private screen's apps go with it.
            leave_started: place == Where::Desktop,
        };
        let server = place.server();
        if let Err(e) = session.mcp.add(server, entry(&command, place, &dir.join("driver"), name.as_deref()), with).await {
            // Nothing of it is the session's (a server of that name there is another's).
            recording.stop();
            if let Some(display) = display {
                display.end().await;
            }
            bail!("{stood_in}{e}");
        }
        open.insert(place, Open { recording, display });
        let Some(name) = name else {
            return Ok(format!(
                "{stood_in}The user's desktop is open to you. {how} It is the screen they are working on: act only on what they asked \
                 for, and leave their other windows as they are. An app you launch here stays open when this screen closes: end it \
                 yourself if it was only for your work."
            ));
        };
        Ok(format!(
            "Your private screen is open: an X display of this thread's own (`DISPLAY={name}`, {width}x{height}), which nobody sees but \
             in this thread. {how}\n\
             Start apps on it with `mcp__{server}__launch_app`, or from bash, in the background, with `env -u WAYLAND_DISPLAY \
             DISPLAY={name} <command>`: anything started without that opens on the user's desktop.\n\
             {}\n\
             The screen closes, with the apps on it, when the thread has been idle for a while; open it again then.",
            private_guide(&name)
        ))
    }

    /// The tool's switch went off, or the session is going: every screen closes, one still
    /// opening once it has.
    pub(crate) async fn close_all(&self, mcp: &Servers) {
        let mut open = self.open.lock().await;
        for (place, screen) in open.drain() {
            screen.close(place, mcp).await;
        }
    }
}

#[async_trait]
impl Tool for Computer {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            snippet: Some("Open a private screen, or the user's desktop, to run and operate GUI apps (see windows, click, type)".into()),
            ..ToolSpec::new(
                COMPUTER,
                "Open a screen to run and operate GUI apps on, when a task needs one: a `private` screen of this thread's own, or the \
                 user's `desktop`. Prefer the private one for your own work; use the desktop when the user asks for it or the task is \
                 about what is on their screen. Opening one gives you the tools to see and operate it.",
                json!({
                    "type": "object",
                    "properties": {
                        "screen": {
                            "type": "string",
                            "enum": ["private", "desktop"],
                            "description": "`private`: an invisible screen of this thread's own, for your own work (trying an app or a \
                                            page, testing a UI). `desktop`: the user's real desktop, when they ask for it or the task \
                                            is about what is on their screen.",
                        },
                        "action": {
                            "type": "string",
                            "enum": ["open", "close"],
                            "description": "`open` (default) the screen, or `close` it. A private screen closes with the apps on \
                                            it; the apps on the desktop stay open, the ones you launched there too.",
                        },
                        "width": { "type": "number", "description": format!("A private screen's width in pixels (default {WIDTH}).") },
                        "height": { "type": "number", "description": format!("A private screen's height in pixels (default {HEIGHT}).") },
                    },
                    "required": ["screen"],
                }),
            )
        }
    }

    async fn call(&self, input: Value, _call: &Call) -> Result<ToolOutput> {
        let input: Input = serde_json::from_value(input)?;
        let session = self.session.upgrade().ok_or_else(|| anyhow!("the thread was unloaded"))?;
        // A size Xvfb won't take is said as that, not as an X server that didn't start.
        let size = |asked: Option<f64>, usual: u32, name: &str| match asked {
            Some(asked) if !SIZES.contains(&asked) => bail!("`{name}` must be between {} and {} pixels", SIZES.start(), SIZES.end()),
            asked => Ok(asked.map_or(usual, |asked| asked.round() as u32)),
        };
        let (width, height) = (size(input.width, WIDTH, "width")?, size(input.height, HEIGHT, "height")?);
        let screen = input.screen;
        let mut open = self.open.lock().await;
        let said = match input.action {
            Some(Action::Close) => match open.remove(&screen) {
                Some(opened) => {
                    opened.close(screen, &session.mcp).await;
                    match screen {
                        Where::Private => "Closed the private screen, and the apps on it.".to_owned(),
                        Where::Desktop => {
                            "Closed the desktop screen. The apps on it stay open, the ones you launched there too.".to_owned()
                        }
                    }
                }
                None => format!("The {screen} screen wasn't open."),
            },
            _ => self.open(&session, &mut open, screen, width, height).await?,
        };
        Ok(ToolOutput::text(said))
    }
}

/// Forget what a host's agent before this one left of its screens (a service is killed, not
/// shut down, so its sessions never close them): pictures of the user's windows don't stay
/// on disk.
pub fn forget_screens(dirs: &crate::config::Dirs) {
    let _ = std::fs::remove_dir_all(dirs.data.join("screens"));
}

/// A screen's driver as the session's connection to it is watched: every connection starts
/// the driver's recorder, ahead of any tool call, so the first one's action is recorded too
/// (a recorder that won't start costs only the pictures). While a call runs the recording is
/// read, and the pictures the agent asks for (`LOOKS`) are handed to it as they are answered.
///
/// A picture (`PICTURES`) waits until `SETTLE` has passed since another call last went out
/// or was answered: a script that acts and then looks sees what the action did, not the
/// screen the app hadn't drawn yet. Not so two calls the model sent in one message, which
/// run side by side: the picture may go out first, or while an action that takes long (a
/// drag) is still under way. The guide says not to (`private_guide`).
struct Recorder {
    recording: Arc<Recording>,
    /// When a call that takes no picture last went out or was answered.
    acted: Mutex<Option<Instant>>,
}

#[async_trait]
impl Watch for Recorder {
    async fn connected(&self, client: &Client) {
        let dir = match self.recording.begin() {
            Ok(dir) => dir,
            Err(e) => return tracing::warn!("computer: no directory to record into: {e}"),
        };
        // No accessibility walk around every action: the pictures are what we show.
        let arguments = json!({"output_dir": dir.to_string_lossy(), "include_accessibility_tree": false});
        match client.call_tool("start_recording", arguments, RECORDING_START, &CancellationToken::new()).await {
            Ok(result) if result.get("isError").and_then(Value::as_bool) != Some(true) => {}
            failed => tracing::warn!("computer: start_recording failed: {failed:?}"),
        }
    }

    async fn call_starts(&self, _client: &Client, tool: &str, _input: &Value, call: &Call) {
        if PICTURES.contains(&tool) {
            // Again after the wait: an action may have been answered meanwhile.
            loop {
                let since = self.acted.lock().unwrap().map(|acted| acted.elapsed());
                let Some(wait) = since.and_then(|since| SETTLE.checked_sub(since)).filter(|wait| !wait.is_zero()) else { break };
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    // The user stopped the run: the call goes no further than its connection.
                    () = call.cancel.cancelled() => break,
                }
            }
        } else {
            *self.acted.lock().unwrap() = Some(Instant::now());
        }
        self.recording.calls(true);
    }

    fn call_ended(&self, tool: &str, result: Option<&Value>) {
        if !PICTURES.contains(&tool) {
            *self.acted.lock().unwrap() = Some(Instant::now());
        }
        if let Some(result) = result.filter(|_| LOOKS.contains(&tool)) {
            self.recording.looked(result);
        }
        self.recording.calls(false);
    }
}

/// The picture a thread last sent, whichever of its screens it was of (its hash): the same
/// again isn't sent.
type Shown = Arc<Mutex<Option<u64>>>;

/// A recording's reader. The recorder (`start_recording`) makes a `turn-NNNNN/` folder per
/// action in `begin`'s directory: `before.png` (the target window) as the action starts,
/// then `action.json`, `after.png` and, last and written whole, `evidence.json` once it is
/// done. A turn is read when it starts (its picture, if the window has changed since the
/// last one shown) and when it is done (the action, and the picture after it), then
/// deleted. Turns may finish out of order.
struct Recording {
    root: PathBuf,
    shown: Shown,
    /// It is of the user's desktop, which every step of it says.
    desktop: bool,
    /// Takes the steps of what the agent does on the screen.
    update: Box<dyn Fn(Screen) + Send + Sync>,
    reading: Mutex<Reading>,
}

#[derive(Default)]
struct Reading {
    /// Where the recorder writes now; `None` before the first connection, and once stopped.
    dir: Option<PathBuf>,
    recordings: u32,
    /// Turns whose `before.png` has been read.
    begun: HashSet<String>,
    /// Turns read to their end: one whose folder won't go isn't read again.
    done: HashSet<String>,
    /// How many calls of the screen's tools run, and what reads the recording while any does.
    running: usize,
    poll: Option<tokio::task::JoinHandle<()>>,
}

impl Recording {
    fn new(root: PathBuf, shown: Shown, desktop: bool, update: impl Fn(Screen) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { root, shown, desktop, update: Box::new(update), reading: Mutex::default() })
    }

    /// A new connection to the driver records into a fresh directory: turns count from 1 again.
    fn begin(&self) -> std::io::Result<PathBuf> {
        let mut reading = self.reading.lock().unwrap();
        reading.recordings += 1;
        let dir = self.root.join(reading.recordings.to_string());
        reading.begun.clear();
        reading.done.clear();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        reading.dir = Some(dir.clone());
        Ok(dir)
    }

    /// A call of one of the screen's tools started, or ended: the recording is read while
    /// any runs, and once after.
    fn calls(self: &Arc<Self>, started: bool) {
        {
            let mut reading = self.reading.lock().unwrap();
            reading.running = if started { reading.running + 1 } else { reading.running.saturating_sub(1) };
            if reading.running > 0 {
                let this = Arc::downgrade(self);
                reading.poll.get_or_insert_with(|| {
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(POLL).await;
                            let Some(recording) = this.upgrade() else { return };
                            recording.read();
                        }
                    })
                });
                return;
            }
            if let Some(poll) = reading.poll.take() {
                poll.abort();
            }
        }
        self.read();
    }

    /// The agent took a picture (the `result` of one of `LOOKS`): it is shown, whether it came
    /// in the answer or went to a file the agent named (`screenshot_out_file`). Not one named
    /// by a relative path: that is the driver's, under the thread's directory, not ours.
    fn looked(&self, result: &Value) {
        let blocks = result.get("content").and_then(Value::as_array);
        let block = blocks.into_iter().flatten().find(|block| block["type"] == "image" && block["mimeType"] == "image/png");
        let path = result.pointer("/structuredContent/screenshot_file_path").and_then(Value::as_str).map(Path::new);
        let png = match (block.and_then(|block| block["data"].as_str()), path) {
            (Some(data), _) => BASE64.decode(data).ok(),
            (None, Some(path)) if path.is_absolute() => picture(path),
            _ => None,
        };
        if let Some(image) = png.and_then(|png| self.changed(png)) {
            (self.update)(Screen { image: Some(image), desktop: self.desktop, ..Screen::default() });
        }
    }

    /// Nothing is read any more, and what was recorded goes.
    fn stop(&self) {
        let mut reading = self.reading.lock().unwrap();
        if let Some(poll) = reading.poll.take() {
            poll.abort();
        }
        reading.dir = None;
        let _ = std::fs::remove_dir_all(&self.root);
    }

    fn read(&self) {
        let mut reading = self.reading.lock().unwrap();
        let Some(dir) = reading.dir.clone() else { return };
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => return tracing::warn!("computer: reading the recording failed: {e}"),
        };
        let mut turns: Vec<String> = entries.flatten().filter_map(|entry| entry.file_name().into_string().ok()).collect();
        turns.retain(|name| name.starts_with("turn-"));
        turns.sort();
        for name in turns {
            let turn = dir.join(&name);
            if reading.done.contains(&name) || turn.join("evidence.json").exists() {
                if reading.done.insert(name.clone()) {
                    reading.begun.remove(&name);
                    // Each turn on its own: one that can't be read doesn't hold up those after it.
                    if let Err(e) = self.finish(&turn) {
                        tracing::warn!("computer: reading {name} of the recording failed: {e:#}");
                    }
                }
                // A folder that won't go (a file of it still open, on Windows) is tried again.
                if std::fs::remove_dir_all(&turn).is_ok() {
                    reading.done.remove(&name);
                }
            } else if !reading.begun.contains(&name)
                && let Some(before) = picture(&turn.join("before.png"))
            {
                reading.begun.insert(name);
                if let Some(image) = self.changed(before) {
                    (self.update)(Screen { image: Some(image), desktop: self.desktop, ..Screen::default() });
                }
            }
        }
    }

    /// A finished turn: its action, where it clicked, and the window after it.
    fn finish(&self, turn: &Path) -> Result<()> {
        // The recorder's `action.json`, as far as we read it: `tool`, `arguments`,
        // `result_error`, and where a click was aimed (`click_point`), in pixels of the
        // picture taken before it: `click_point_image`, when that isn't `before.png`.
        let action: Value = serde_json::from_slice(&std::fs::read(turn.join("action.json"))?)?;
        let mut step = Screen { action: Some(label(&action)), desktop: self.desktop, ..Screen::default() };
        let point = action.get("click_point").and_then(|point| Some((point.get("x")?.as_f64()?, point.get("y")?.as_f64()?)));
        // Not where a click was refused: a cursor there would say it happened.
        if let Some((x, y)) = point.filter(|_| action["result_error"] != true) {
            let aimed = picture(&turn.join(action["click_point_image"].as_str().unwrap_or("before.png")));
            step.cursor = aimed.as_deref().and_then(dimensions).map(|(width, height)| [(x / width) as f32, (y / height) as f32]);
        }
        step.image = picture(&turn.join("after.png")).and_then(|after| self.changed(after));
        (self.update)(step);
        Ok(())
    }

    /// `png` as a picture to send, unless it is the picture last sent.
    fn changed(&self, png: Vec<u8>) -> Option<Vec<u8>> {
        let mut hasher = DefaultHasher::new();
        hasher.write(&png);
        let hash = Some(hasher.finish());
        let mut shown = self.shown.lock().unwrap();
        (*shown != hash).then(|| {
            *shown = hash;
            png
        })
    }
}

/// The last bytes of every PNG: its `IEND` chunk.
const PNG_END: [u8; 8] = [0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82];

/// The PNG at `path` once it is all there: the recorder writes `before.png` in place.
fn picture(path: &Path) -> Option<Vec<u8>> {
    let png = std::fs::read(path).ok()?;
    (png.len() > 24 && png.ends_with(&PNG_END)).then_some(png)
}

/// A PNG's width and height, from its header.
fn dimensions(png: &[u8]) -> Option<(f64, f64)> {
    let number = |at: usize| Some(u32::from_be_bytes(png.get(at..at + 4)?.try_into().ok()?));
    let (width, height) = (number(16)?, number(20)?);
    (width > 0 && height > 0).then_some((width.into(), height.into()))
}

/// What an action was, for the screen's caption: `click`, `type "hello"`, `key Return`.
fn label(action: &Value) -> String {
    let tool = action["tool"].as_str().unwrap_or("action");
    let arg = |name: &str| action.get("arguments").and_then(|arguments| arguments.get(name)).filter(|value| !value.is_null());
    let plain = |value: &Value| value.as_str().map_or_else(|| value.to_string(), str::to_owned);
    // A text as a label quotes it: on one line, and no longer than `LABEL_CHARS`.
    let said = |text: String| {
        let mut said = String::new();
        for c in text.chars() {
            match c.is_whitespace() {
                false => said.push(c),
                true if !said.ends_with(' ') => said.push(' '),
                true => {}
            }
        }
        match said.char_indices().nth(LABEL_CHARS) {
            Some((at, _)) => format!("{}…", &said[..at]),
            None => said,
        }
    };
    let what = match tool {
        "type_text" => arg("text").map(|text| format!("type \"{}\"", said(plain(text)))),
        "press_key" => arg("key").map(|key| format!("key {}", said(plain(key)))),
        "hotkey" => {
            let keys = arg("keys").and_then(Value::as_array);
            keys.map(|keys| format!("key {}", said(keys.iter().map(plain).collect::<Vec<_>>().join("+"))))
        }
        "scroll" => arg("direction").map(|direction| format!("scroll {}", said(plain(direction)))),
        "set_value" => arg("value").map(|value| format!("set \"{}\"", said(plain(value)))),
        _ => None,
    };
    let what = what.unwrap_or_else(|| tool.replace('_', " "));
    if action["result_error"] == true { format!("{what} (failed)") } else { what }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As much of a PNG as the reader looks at: its size in the header, and its end.
    /// `shade` tells one picture from another.
    fn png(width: u32, height: u32, shade: u8) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(width.to_be_bytes());
        png.extend(height.to_be_bytes());
        png.extend([shade; 8]);
        png.extend(PNG_END);
        png
    }

    /// A recording under `root`, and the steps it hands on.
    fn recording(root: &Path, shown: &Shown, desktop: bool) -> (Arc<Recording>, impl Fn() -> Vec<Screen> + use<>) {
        let steps = Arc::new(Mutex::new(vec![]));
        let taken = steps.clone();
        let recording = Recording::new(root.to_owned(), shown.clone(), desktop, move |step| taken.lock().unwrap().push(step));
        (recording, move || std::mem::take(&mut *steps.lock().unwrap()))
    }

    fn picture_of(png: Vec<u8>) -> Screen {
        Screen { image: Some(png), ..Screen::default() }
    }

    #[test]
    fn a_label_says_what_an_action_was() {
        let label = |action: Value| label(&action);
        assert_eq!(label(json!({"tool": "click", "arguments": {"x": 1}})), "click");
        assert_eq!(label(json!({"tool": "launch_app", "arguments": {"name": "xterm"}})), "launch app");
        assert_eq!(label(json!({"tool": "type_text", "arguments": {"text": "hello\n  there"}})), "type \"hello there\"");
        assert_eq!(label(json!({"tool": "type_text", "arguments": {"text": "é".repeat(60)}})), format!("type \"{}…\"", "é".repeat(48)));
        assert_eq!(label(json!({"tool": "press_key", "arguments": {"key": "Return"}})), "key Return");
        assert_eq!(label(json!({"tool": "hotkey", "arguments": {"keys": ["ctrl", "shift", "t"]}})), "key ctrl+shift+t");
        assert_eq!(label(json!({"tool": "scroll", "arguments": {"direction": "down", "amount": 3}})), "scroll down");
        assert_eq!(label(json!({"tool": "set_value", "arguments": {"value": 5}})), "set \"5\"");
        // Without what it would quote, an action is its tool's name; a refused one says so.
        assert_eq!(label(json!({"tool": "type_text", "arguments": {}})), "type text");
        assert_eq!(label(json!({"tool": "click", "result_error": true})), "click (failed)");
        assert_eq!(label(json!({})), "action");
    }

    #[test]
    fn a_recording_is_read_turn_by_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("recording");
        let (recording, steps) = recording(&root, &Shown::default(), false);
        // Nothing is read before a connection records.
        recording.read();
        let dir = recording.begin().unwrap();
        assert_eq!(dir, root.join("1"));
        let turn = |n: u32| dir.join(format!("turn-{n:05}"));
        let write = |n: u32, file: &str, bytes: &[u8]| {
            std::fs::create_dir_all(turn(n)).unwrap();
            std::fs::write(turn(n).join(file), bytes).unwrap();
        };
        let done = |n: u32, action: Value| {
            write(n, "action.json", action.to_string().as_bytes());
            write(n, "evidence.json", b"{}");
        };

        // A turn that starts shows the window as it is, once its picture is all there.
        write(1, "before.png", &png(200, 100, 1)[..30]);
        recording.read();
        assert_eq!(steps(), []);
        write(1, "before.png", &png(200, 100, 1));
        recording.read();
        recording.read();
        assert_eq!(steps(), [picture_of(png(200, 100, 1))]);

        // Done, it says what was done and where, with the window after it, and its folder goes.
        write(1, "after.png", &png(200, 100, 2));
        done(1, json!({"tool": "click", "arguments": {"x": 50, "y": 25}, "click_point": {"x": 50.0, "y": 25.0}, "result_error": false}));
        recording.read();
        let clicked = Screen { image: Some(png(200, 100, 2)), action: Some("click".into()), cursor: Some([0.25, 0.25]), desktop: false };
        assert_eq!(steps(), [clicked]);
        assert!(!turn(1).exists());

        // Turns finish out of order. A window that looks as it did isn't sent again, a
        // refused click has no cursor, and a turn that can't be read doesn't hold up the next.
        write(2, "before.png", &png(200, 100, 2));
        write(3, "before.png", &png(200, 100, 2));
        write(3, "after.png", &png(200, 100, 2));
        done(3, json!({"tool": "click", "click_point": {"x": 50.0, "y": 25.0}, "result_error": true}));
        write(4, "evidence.json", b"{}");
        write(4, "action.json", b"{\"tool\": \"cli");
        write(5, "after.png", &png(200, 100, 3));
        done(5, json!({"tool": "type_text", "arguments": {"text": "hi"}}));
        recording.read();
        let (failed, typed) = (Some("click (failed)".to_owned()), Some("type \"hi\"".to_owned()));
        assert_eq!(steps(), [Screen { action: failed, ..Screen::default() }, Screen { action: typed, ..picture_of(png(200, 100, 3)) }]);
        assert!(turn(2).exists() && !turn(3).exists() && !turn(4).exists() && !turn(5).exists());
        // Where it clicked is a fraction of the picture the click was aimed in.
        write(2, "aimed.png", &png(400, 50, 9));
        done(2, json!({"tool": "click", "click_point": {"x": 100.0, "y": 25.0}, "click_point_image": "aimed.png"}));
        recording.read();
        assert_eq!(steps(), [Screen { action: Some("click".into()), cursor: Some([0.25, 0.5]), ..Screen::default() }]);

        // A new connection records into a fresh directory; stopped, what was recorded goes.
        assert_eq!(recording.begin().unwrap(), root.join("2"));
        recording.stop();
        recording.read();
        assert!(!root.exists() && steps().is_empty());
    }

    #[test]
    fn the_same_picture_is_not_sent_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let shown = Shown::default();
        let (private, steps) = recording(&tmp.path().join("private"), &shown, false);
        let (desktop, desktop_steps) = recording(&tmp.path().join("desktop"), &shown, true);
        let answer = |png: Vec<u8>| json!({"content": [{"type": "image", "mimeType": "image/png", "data": BASE64.encode(png)}, {"type": "text", "text": "✅"}]});
        // The pictures the agent takes are shown: one in the answer, once, ...
        private.looked(&answer(png(64, 48, 1)));
        private.looked(&answer(png(64, 48, 1)));
        // ... and one in a file the agent named, not one the driver put somewhere of its own.
        let file = tmp.path().join("shot.png");
        std::fs::write(&file, png(64, 48, 2)).unwrap();
        private.looked(&json!({"content": [], "structuredContent": {"screenshot_file_path": file.to_str()}}));
        private.looked(&json!({"content": [], "structuredContent": {"screenshot_file_path": "shot.png"}}));
        private.looked(&json!({"content": [{"type": "text", "text": "No such window"}], "isError": true}));
        assert_eq!(steps(), [picture_of(png(64, 48, 1)), picture_of(png(64, 48, 2))]);
        // What was sent last is the thread's, whichever of its screens it was of.
        desktop.looked(&answer(png(64, 48, 2)));
        desktop.looked(&answer(png(64, 48, 1)));
        private.looked(&answer(png(64, 48, 2)));
        assert_eq!(desktop_steps(), [Screen { desktop: true, ..picture_of(png(64, 48, 1)) }]);
        assert_eq!(steps(), [picture_of(png(64, 48, 2))]);
    }

    /// The processes that come of this one (what a session started, and what those started):
    /// their ids and command lines.
    #[cfg(target_os = "linux")]
    fn started() -> Vec<(u32, String)> {
        // In a process's `stat`, after its name (in brackets, spaces and all): its state, its parent.
        let parent = |pid: u32| -> Option<u32> {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?.rsplit_once(") ")?.1.split(' ').nth(1)?.parse().ok()
        };
        let me = std::process::id();
        let pids = std::fs::read_dir("/proc").unwrap().flatten().filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok());
        let ours = pids.filter(|pid| std::iter::successors(parent(*pid), |pid| parent(*pid)).any(|pid| pid == me));
        ours.map(|pid| (pid, std::fs::read_to_string(format!("/proc/{pid}/cmdline")).unwrap_or_default().replace('\0', " "))).collect()
    }

    /// The real thing, end to end: a private screen on an Xvfb of the test's own, operated
    /// through the real driver. Needs `Xvfb` and `cua-driver` on PATH (the nix dev shell has both):
    /// `nix develop -c cargo test -p dirigent_agent a_private_screen -- --ignored`
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "runs Xvfb and Cua Driver"]
    async fn a_private_screen_opens_shows_what_is_done_on_it_and_closes() {
        use crate::session::Prompt;
        use crate::testing::{Script, Step, call, prompt, session, text};
        let dir = tempfile::tempdir().unwrap();
        let target = json!({"kind": "desktop", "display_id": "primary"});
        let script = Script::new(vec![
            Step::Say(vec![call("c1", COMPUTER, json!({"screen": "private", "width": 640, "height": 480}))]),
            Step::Say(vec![call("c2", "mcp__private_screen__click", json!({"target": target, "x": 160, "y": 120}))]),
            Step::Say(vec![call("c3", "mcp__private_screen__get_desktop_state", json!({}))]),
            Step::Say(vec![call("c4", COMPUTER, json!({"screen": "private"}))]),
            Step::Say(vec![text("operated")]),
            Step::Say(vec![call("c5", COMPUTER, json!({"screen": "private", "action": "close"}))]),
            Step::Say(vec![call("c6", COMPUTER, json!({"screen": "private", "action": "close"}))]),
            Step::Say(vec![text("closed")]),
            Step::Say(vec![call("c7", COMPUTER, json!({"screen": "private"}))]),
            Step::Say(vec![text("open again")]),
        ]);
        let (host, session) = session(dir.path(), script.clone());
        let recordings = dir.path().join("data/screens/thread/private/recording");
        // The screen's processes (the shell that holds its X server, that, and the driver),
        // how many of them run, and whether the X server takes connections.
        let ours = || started().into_iter().filter(|(_, command)| command.contains("Xvfb") || command.contains("cua-driver"));
        let running = |pids: &[u32]| pids.iter().filter(|pid| Path::new(&format!("/proc/{pid}")).exists()).count();
        let socket = |display: &str| Path::new(&format!("/tmp/.X11-unix/X{}", &display[1..])).exists();
        let display_of = |opened: &str| opened.split_once("`DISPLAY=").unwrap().1.split_once('`').unwrap().0.to_owned();

        // Opened, it is an X display with the driver on it, and the model has the tools to operate it.
        session.prompt(Prompt { tools: Some(vec![COMPUTER.into()]), ..prompt("open a screen") }).await.unwrap();
        host.settled(1).await;
        let transcript = host.transcript();
        assert_eq!(transcript.len(), 10, "{transcript:#?}");
        let opened = &transcript[2];
        assert!(opened.starts_with("result: Your private screen is open: an X display of this thread's own (`DISPLAY=:"), "{opened}");
        assert!(opened.contains(", 640x480), which nobody sees but in this thread. Operate it with the `mcp__private_screen__…` tools"));
        assert!(opened.contains("\nA private screen has no window manager") && opened.ends_with("open it again then."), "{opened}");
        let display = display_of(opened);
        assert_eq!(
            transcript[8],
            format!("result: The private screen is open already (`DISPLAY={display}`). {}", how(Where::Private, false))
        );
        let pids: Vec<u32> = ours().map(|(pid, _)| pid).collect();
        let commands: Vec<String> = ours().map(|(_, command)| command).collect();
        let count = |what: &str| commands.iter().filter(|command| command.contains(what)).count();
        assert_eq!((count("Xvfb -displayfd 1 -screen 0 640x480x24"), count("cua-driver mcp --no-overlay")), (2, 1), "{commands:?}");
        assert!(socket(&display) && recordings.join("1").is_dir());
        // Of the driver's tools, the ones a private screen is operated with, and our guide to them.
        let tools = script.requests.lock().unwrap()[1].2.clone();
        let mut driver: Vec<&str> = tools.iter().filter_map(|tool| tool.strip_prefix("mcp__private_screen__")).collect();
        let mut wanted = PRIVATE_TOOLS;
        driver.sort();
        wanted.sort();
        assert_eq!(driver, wanted, "the driver's tools: were some renamed?");
        assert_eq!(session.mcp.namespace("mcp__private_screen").unwrap().instructions, Some(private_guide(&display)));

        // The click was sent, and the look answered with the screen's picture.
        assert!(transcript[4].starts_with("result: ✅ Sent screen-absolute click at (160,120)"), "{}", transcript[4]);
        assert!(transcript[6].starts_with("result: (image) ✅ Desktop screenshot 640x480"), "{}", transcript[6]);
        // The thread was shown the screen, and what was done on it where.
        let steps: Vec<Screen> =
            host.outs.lock().unwrap().iter().filter_map(|o| if let Out::Screen(s) = o { Some(s.clone()) } else { None }).collect();
        let pictures: Vec<&Vec<u8>> = steps.iter().filter_map(|step| step.image.as_ref()).collect();
        assert!(!pictures.is_empty() && pictures.iter().all(|png| dimensions(png) == Some((640., 480.))), "{} pictures", pictures.len());
        let actions: Vec<_> = steps.iter().filter_map(|step| Some((step.action.as_deref()?, step.cursor, step.desktop))).collect();
        assert_eq!(actions, [("click", Some([0.25, 0.25]), false)]);
        // The turn was read to its end, and nothing of it stays.
        assert_eq!(std::fs::read_dir(recordings.join("1")).unwrap().flatten().filter(|e| e.path().is_dir()).count(), 0);

        // Closed, nothing of it is left: no process, no display, no tools, no recording.
        session.prompt(prompt("close it")).await.unwrap();
        host.settled(2).await;
        let transcript = host.transcript();
        // What the two closes said (between them, the model's second call), then its reply.
        let said = [&transcript[12], &transcript[14], &transcript[15]];
        let closed = "result: Closed the private screen, and the apps on it.";
        assert_eq!(said, [closed, "result: The private screen wasn't open.", "assistant: closed"]);
        assert_eq!((running(&pids), socket(&display), recordings.exists()), (0, false, false), "{:?}", started());
        assert!(session.tool("mcp__private_screen__click").is_none());

        // A session that is unloaded closes what it has open.
        session.prompt(prompt("once more")).await.unwrap();
        host.settled(3).await;
        let display = display_of(host.transcript().iter().rev().find(|line| line.starts_with("result: Your private screen")).unwrap());
        let pids: Vec<u32> = ours().map(|(pid, _)| pid).collect();
        assert_eq!((running(&pids), socket(&display)), (3, true));
        session.unload();
        for _ in 0..100 {
            if running(&pids) > 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        assert_eq!((running(&pids), socket(&display), recordings.exists()), (0, false, false), "{:?}", started());
    }
}
