//! The move from pi, made once: the first start of a build with its own agent finds an
//! install as pi left it (`pending`), and brings it over where it is (`run`):
//!
//! 1. Backup. What is about to change is copied to new directories beside the data and
//!    the config dir, `<dir>.bck` (`.bck.2` where there is one already, and so on): the
//!    dbs of the data dir (the service's, the desktop's) and the whole config dir. Not the
//!    rest of the data dir: workspaces are most of it, and stay as they are.
//! 2. Threads. Pi's `service.db` is converted (`storage::migrate`).
//! 3. Settings. Pi's agent dir, `<config dir>/agent`, becomes the agent's files
//!    (`agent::migrate`) and is removed, with all of it in the backup and nothing that
//!    was brought over leading into it.
//!
//! What can't be brought over doesn't stop the rest: it is a line for the user
//! (`Event::Left`). A step that fails as a whole (no room for the backup, a db that won't
//! convert) stops the migration (`Report::error`) with the data as that step found it.
//! The next start goes on from there, with the backup there is: what was there before
//! the first try is what to keep, and a service that is started over and over makes one.
//!
//! It is of an install, and that one's alone: one migration at a time has the data dir
//! (`alone`), no other Dirigent has the db (`storage::settle`), and an instance with
//! only one of its two dirs pointed elsewhere is refused (`run`), as the other is the
//! real install's.
//!
//! A desktop makes it in a window before it starts anything else (`dirigent_desktop`'s
//! `migration.rs`); a service finds it made, or makes it and says it in its log
//! (`headless`). Either way the report is left in the backups, as `migration.txt`.

use crate::{agent, storage};
use anyhow::{Context, Result};
use dirigent_agent::config::Dirs;
use dirigent_agent::import::Imported;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// The dbs of the data dir: the service's and the desktop's.
const DBS: [&str; 2] = ["service.db", "desktop.db"];

/// The report's file in a backup.
const REPORT: &str = "migration.txt";

/// The file in a backup whose migration has not come to its end (it stopped, or was
/// killed): the next run goes on with that backup.
const UNFINISHED: &str = "unfinished";

/// The file in the data dir that a migration holds locked while it runs (`alone`).
const LOCK: &str = "migration.lock";

/// What the report's list of what could not be brought over begins with.
const LEFT: &str = "\nNot brought over:\n";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    Backup,
    Threads,
    Settings,
}

/// What `run` tells as it goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The steps there are to make, before the first.
    Steps(Vec<Step>),
    /// A step begins.
    Step(Step),
    /// How far the step under way is: `done` threads of `total`.
    Progress { done: usize, total: usize },
    /// What was done: a line for the log.
    Done(String),
    /// What could not be brought over, and what to do about it: a line for the user.
    Left(String),
}

/// What a migration came to.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    /// The directories that hold what was there before.
    pub backups: Vec<PathBuf>,
    /// The `Event::Done` lines.
    pub done: Vec<String>,
    /// The `Event::Left` lines.
    pub left: Vec<String>,
    /// Why it stopped short, if it did.
    pub error: Option<String>,
}

fn dirs() -> Dirs {
    Dirs { config: crate::config_dir(), data: crate::data_dir() }
}

/// This install is as pi left it, in part at least: there is a migration to make.
pub fn pending() -> bool {
    pending_in(&dirs())
}

fn pending_in(dirs: &Dirs) -> bool {
    // (A db that can't be read is not pi's to convert: opening it says what is wrong.)
    storage::from_pi(&dirs.data.join("service.db")).unwrap_or(false) || agent::pending(dirs)
}

/// Make the migration, telling of it as it goes: a desktop before it starts its service,
/// a service once it has its port.
pub fn run(tell: impl FnMut(Event)) -> Report {
    // With one of the two dirs pointed elsewhere the other is the real install's, which may
    // be running: not this instance's to convert, nor to remove pi's dir from.
    if crate::half_elsewhere() {
        let error = "only one of --data-dir and --config-dir is given, and the other directory is as pi left it: pass both \
                     (a test instance two of its own; an install that keeps one elsewhere, the other as well)";
        return Report { error: Some(error.into()), ..Default::default() };
    }
    run_in(&dirs(), tell)
}

fn run_in(dirs: &Dirs, tell: impl FnMut(Event)) -> Report {
    let mut run = Run { dirs, tell, report: Report::default(), kept: None };
    let made = alone(dirs).and_then(|lock| {
        let made = run.steps();
        // Gone while it is still held: nobody locks a file that is on its way out.
        let _ = std::fs::remove_file(dirs.data.join(LOCK));
        drop(lock);
        made
    });
    if let Err(e) = made {
        run.report.error = Some(format!("{e:#}"));
    }
    write_report(&run.report);
    run.report
}

/// Have the data dir for one migration: two at once would throw away each other's backup
/// and db in the making. Until the file it returns is dropped.
fn alone(dirs: &Dirs) -> Result<std::fs::File> {
    std::fs::create_dir_all(&dirs.data)?;
    let lock = std::fs::File::create(dirs.data.join(LOCK))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!("another Dirigent is migrating {}: start again once it is through", dirs.data.display())
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("lock the data dir for the migration"),
    }
}

/// The migration a service makes by itself, said in its log. An error is one that stopped
/// short: the service doesn't start on what is half pi's.
pub(crate) fn headless() -> Result<()> {
    if !pending() {
        return Ok(());
    }
    info!("this install is as pi left it: migrating");
    let mut tenth = 0;
    let report = run(|event| match event {
        Event::Steps(_) => {}
        Event::Step(step) => info!("migration: {step:?}"),
        // Every tenth of the way.
        Event::Progress { done, total } => {
            if done * 10 / total.max(1) > tenth {
                tenth = done * 10 / total.max(1);
                info!("migration: {done} of {total} threads");
            }
        }
        Event::Done(line) => info!("migration: {line}"),
        Event::Left(line) => warn!("migration: {line}"),
    });
    match report.error {
        Some(error) => anyhow::bail!("the migration from pi stopped: {error}"),
        None => Ok(()),
    }
}

struct Run<'a, T> {
    dirs: &'a Dirs,
    tell: T,
    report: Report,
    /// Pi's agent dir in the backup this run made of the config dir, with all of it
    /// there: the one in the config dir can go. (Of an earlier run's backup this one
    /// can't tell.)
    kept: Option<PathBuf>,
}

impl<T: FnMut(Event)> Run<'_, T> {
    fn steps(&mut self) -> Result<()> {
        let db = self.dirs.data.join("service.db");
        let pi = self.dirs.config.join("agent");
        // Pi's dir is brought over wherever it is: also beside an `agent.toml` that was
        // written before this (only what that lacks is taken).
        let (threads, settings) = (storage::from_pi(&db)?, pi.is_dir());
        if !threads && !settings {
            // Another Dirigent made it meanwhile.
            return Ok(());
        }
        let steps = [(true, Step::Backup), (threads, Step::Threads), (settings, Step::Settings)];
        (self.tell)(Event::Steps(steps.into_iter().filter(|(pending, _)| *pending).map(|(_, step)| step).collect()));

        (self.tell)(Event::Step(Step::Backup));
        if threads {
            // The file alone is all of the db, and nobody else is on it: it is copied, and
            // then replaced.
            storage::settle(&db)?;
        }
        self.backup()?;

        if threads {
            (self.tell)(Event::Step(Step::Threads));
            let tell = &mut self.tell;
            let migrated = storage::migrate(&db, |done, total| tell(Event::Progress { done, total }))?;
            let migration = migrated.context("the db was pi's a moment ago")?;
            self.done(format!("{} threads: {} of pi's {} entries are entries here", migration.threads, migration.kept, migration.entries));
            for (thread, why) in migration.locked {
                let why = why.join("; ");
                self.left(format!(
                    "The thread \"{thread}\" could not be converted whole ({why}): it opens and forks, but takes no message"
                ));
            }
        }

        if settings {
            (self.tell)(Event::Step(Step::Settings));
            let Imported { done, left } = agent::migrate(self.dirs)?;
            // Pi's dir goes with all of it in the backup, and nothing of ours leading into it.
            let stays = match &self.kept {
                None => Some("the backup may not have all of it".to_owned()),
                Some(_) => link_into(self.dirs, &pi).map(|link| format!("{} leads into it", link.display())),
            };
            // A file of pi's is named where it is from now on: in the backup, once the dir is gone.
            let within = |dir: &Path| format!("{}{}", dir.display(), std::path::MAIN_SEPARATOR);
            let kept = self.kept.as_deref().filter(|_| stays.is_none()).map(within);
            let named = |line: String| match &kept {
                Some(kept) => line.replace(&within(&pi), kept),
                None => line,
            };
            done.into_iter().map(&named).for_each(|line| self.done(line));
            left.into_iter().map(&named).for_each(|line| self.left(line));
            if let Some(why) = stays {
                self.left(format!("{} stays where it is ({why}): nothing reads it any more", pi.display()));
            } else if let Err(e) = std::fs::remove_dir_all(&pi) {
                self.left(format!("{} could not be removed ({e}): nothing reads it any more", pi.display()));
            }
        }
        Ok(())
    }

    fn done(&mut self, line: String) {
        self.report.done.push(line.clone());
        (self.tell)(Event::Done(line));
    }

    fn left(&mut self, line: String) {
        self.report.left.push(line.clone());
        (self.tell)(Event::Left(line));
    }

    /// Copy what the migration is about to change. Each backup is made under another name
    /// and gets its own once whole: a directory called a backup is one. One that is there
    /// is an earlier run's that stopped (`backups`), and has what was there before both.
    fn backup(&mut self) -> Result<()> {
        let (config, data) = backups(self.dirs)?;
        // What an earlier try could not bring over still is so, and this run's to say.
        if let Some(before) = [&data, &config].into_iter().find(|backup| backup.exists()) {
            left_before(before).into_iter().for_each(|line| self.left(line));
        }
        let dbs = DBS.iter().flat_map(|db| ["", "-wal", "-shm"].map(|beside| self.dirs.data.join(format!("{db}{beside}"))));
        let dbs: Vec<PathBuf> = dbs.filter(|file| file.is_file()).collect();
        if data.exists() {
            self.report.backups.push(data.clone());
        } else if !dbs.is_empty() {
            let part = part_of(&data);
            let copied = (|| {
                std::fs::create_dir_all(&part)?;
                for db in &dbs {
                    let copy = part.join(db.file_name().unwrap_or_default());
                    std::fs::copy(db, &copy)?;
                    // On the disk before pi's db is replaced.
                    std::fs::File::options().write(true).open(&copy)?.sync_all()?;
                }
                std::fs::write(part.join(UNFINISHED), "")?;
                as_open_as(&self.dirs.data, &part)?;
                std::fs::rename(&part, &data)
            })();
            if let Err(e) = copied {
                let _ = std::fs::remove_dir_all(&part);
                return Err(e).with_context(|| format!("back up the dbs of {} to {}", self.dirs.data.display(), data.display()));
            }
            self.report.backups.push(data.clone());
            self.done(format!("the dbs of {} are copied to {}", self.dirs.data.display(), data.display()));
        }
        if config.exists() {
            self.report.backups.push(config.clone());
        } else if self.dirs.config.is_dir() {
            let part = part_of(&config);
            let mut left = vec![];
            let copied = copy_tree(&self.dirs.config, &part, &self.dirs.data, &mut left)
                .and_then(|()| std::fs::write(part.join(UNFINISHED), ""))
                .and_then(|()| std::fs::rename(&part, &config));
            if let Err(e) = copied {
                let _ = std::fs::remove_dir_all(&part);
                return Err(e).with_context(|| format!("back up {} to {}", self.dirs.config.display(), config.display()));
            }
            self.report.backups.push(config.clone());
            self.done(format!("{} is copied to {}", self.dirs.config.display(), config.display()));
            self.kept = left.is_empty().then(|| config.join("agent"));
            left.into_iter().for_each(|line| self.left(line));
        }
        Ok(())
    }
}

/// Where this migration's backups go: `<dir>.bck` beside the config and the data dir, or
/// `.bck.2` (and so on) where an earlier one is in the way. The same for both. The backup
/// of a migration that did not finish is in nobody's way: it is this one's (what is
/// there now is in part what that run made of it), and no retry makes another.
fn backups(dirs: &Dirs) -> Result<(PathBuf, PathBuf)> {
    let named = |dir: &Path, n: u32| {
        let mut name = dir.file_name()?.to_owned();
        name.push(if n == 1 { ".bck".to_owned() } else { format!(".bck.{n}") });
        Some(dir.with_file_name(name))
    };
    let mut last: Option<(PathBuf, PathBuf)> = None;
    for n in 1.. {
        let (config, data) =
            named(&dirs.config, n).zip(named(&dirs.data, n)).context("the data and config dirs have no name to call a backup after")?;
        if !config.exists() && !data.exists() {
            let unfinished = |(config, data): &(PathBuf, PathBuf)| config.join(UNFINISHED).exists() || data.join(UNFINISHED).exists();
            return Ok(last.filter(unfinished).unwrap_or((config, data)));
        }
        last = Some((config, data));
    }
    unreachable!("some number is free")
}

/// The name a backup is made under, with nothing there.
fn part_of(backup: &Path) -> PathBuf {
    let mut name = backup.as_os_str().to_owned();
    name.push(".part");
    let part = PathBuf::from(name);
    let _ = std::fs::remove_dir_all(&part);
    part
}

/// What an earlier try could not bring over, from the report it left in its `backup`.
fn left_before(backup: &Path) -> Vec<String> {
    let report = std::fs::read_to_string(backup.join(REPORT)).unwrap_or_default();
    let list = report.split_once(LEFT).map(|(_, list)| list).unwrap_or_default();
    list.lines().take_while(|line| !line.is_empty()).filter_map(|line| line.strip_prefix("- ")).map(str::to_owned).collect()
}

/// A symlink among what was brought over (`AGENTS.md`, `skills/`) that leads into `pi`:
/// with that gone it would lead nowhere.
fn link_into(dirs: &Dirs, pi: &Path) -> Option<PathBuf> {
    fn find(at: &Path, pi: &Path) -> Option<PathBuf> {
        let meta = at.symlink_metadata().ok()?;
        if meta.is_symlink() {
            return std::fs::canonicalize(at).is_ok_and(|to| to.starts_with(pi)).then(|| at.to_owned());
        }
        if !meta.is_dir() {
            return None;
        }
        std::fs::read_dir(at).ok()?.flatten().find_map(|entry| find(&entry.path(), pi))
    }
    let pi = std::fs::canonicalize(pi).ok()?;
    ["AGENTS.md", "skills"].iter().find_map(|name| find(&dirs.config.join(name), &pi))
}

/// A copy of a directory is as open as the directory, and no more: a config dir may be
/// its owner's alone around a file that is not (pi's, with the proxy's key).
fn as_open_as(dir: &Path, copy: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(copy, std::fs::metadata(dir)?.permissions())
}

/// Copy the directory `from` as `to`, as far as it goes: what can't be copied is a line of
/// `left`, and the rest still is. A link stays one (on unix; elsewhere what it leads to is
/// copied). `skip` is left out (a data dir kept inside the config dir), and so is what is
/// neither a file nor a directory: a socket is nothing to keep.
fn copy_tree(from: &Path, to: &Path, skip: &Path, left: &mut Vec<String>) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if src == skip {
            continue;
        }
        let kind = entry.file_type()?;
        let followed = kind.is_symlink() && !cfg!(unix);
        let copied = if kind.is_symlink() && cfg!(unix) {
            #[cfg(unix)]
            let linked = std::fs::read_link(&src).and_then(|target| std::os::unix::fs::symlink(target, &dst));
            #[cfg(not(unix))]
            let linked = Ok(());
            linked
        } else if kind.is_dir() || (followed && src.is_dir()) {
            copy_tree(&src, &dst, skip, left)
        } else if kind.is_file() || followed {
            std::fs::copy(&src, &dst).map(drop)
        } else {
            Ok(())
        };
        if let Err(e) = copied {
            left.push(format!("{} is not in the backup: {e}", src.display()));
        }
    }
    // Last: what is in it may be more than its mode lets anyone add.
    as_open_as(from, to)
}

/// Leave the report in the backups: what the window showed or the log said, for later.
fn write_report(report: &Report) {
    let lines = |lines: &[String]| lines.iter().map(|line| format!("- {line}\n")).collect::<String>();
    let when = chrono::Local::now().format("%Y-%m-%d %H:%M");
    let mut text = format!("Dirigent {} moved this install from pi to its own agent, {when}.\n", dirigent_protocol::release::VERSION);
    text += "\nWhat was there before is in:\n";
    text += &lines(&report.backups.iter().map(|backup| backup.display().to_string()).collect::<Vec<_>>());
    text += "\nDone:\n";
    text += &lines(&report.done);
    if !report.left.is_empty() {
        text += LEFT;
        text += &lines(&report.left);
    }
    if let Some(error) = &report.error {
        text += &format!("\nIt stopped: {error}\nThe next start of Dirigent goes on from there.\n");
    }
    for backup in &report.backups {
        if let Err(e) = std::fs::write(backup.join(REPORT), &text) {
            warn!("{} not written: {e}", backup.join(REPORT).display());
        }
        if report.error.is_none() {
            let _ = std::fs::remove_file(backup.join(UNFINISHED));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_agent::config::Config;

    /// An install as pi left it: a db with a thread, the desktop's db and a workspace in
    /// the data dir; pi's agent dir and the desktop's settings in the config dir.
    fn pis(dirs: &Dirs) {
        std::fs::create_dir_all(dirs.data.join("workspaces/w")).unwrap();
        std::fs::write(dirs.data.join("workspaces/w/big"), "a checkout").unwrap();
        std::fs::write(dirs.data.join("desktop.db"), "the desktop's").unwrap();
        let conn = rusqlite::Connection::open(dirs.data.join("service.db")).unwrap();
        conn.execute_batch(storage::PI_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO projects VALUES ('p', '/x', NULL, 1, NULL);
             INSERT INTO threads (id, project_id, cwd, name, created_at, updated_at) VALUES ('t', 'p', '/x', 'Bug hunt', 1, 1);
             INSERT INTO entries VALUES ('t', 0, '', NULL, '', '{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"hi\"}}');
             INSERT INTO entries VALUES ('t', 1, '', NULL, '', '{\"type\":\"message\",\"id\":\"x\",\"parentId\":\"u\",\"message\":{\"role\":\"bashExecution\"}}');",
        )
        .unwrap();
        pis_settings(dirs);
    }

    fn pis_settings(dirs: &Dirs) {
        let pi = dirs.config.join("agent");
        std::fs::create_dir_all(&pi).unwrap();
        // The owner's alone, around a file with the proxy's key that is not.
        #[cfg(unix)]
        std::fs::set_permissions(&dirs.config, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        std::fs::write(dirs.config.join("config.toml"), "theme = \"x\"\n").unwrap();
        std::fs::write(pi.join("cliproxyapi.json"), r#"{"baseUrl":"http://127.0.0.1:8317","apiKey":"k"}"#).unwrap();
        std::fs::write(pi.join("AGENTS.md"), "Be brief.").unwrap();
        std::fs::write(pi.join("APPEND_SYSTEM.md"), "Be kind.").unwrap();
    }

    #[test]
    fn an_install_of_pis_is_backed_up_and_brought_over() {
        let tmp = tempfile::tempdir().unwrap();
        let at = |name: &str| tmp.path().join(name);
        let dirs = Dirs { config: at("config"), data: at("data") };
        // Nothing there: nothing to do.
        assert!(!pending_in(&dirs));
        pis(&dirs);
        assert!(pending_in(&dirs));

        let mut told = vec![];
        let report = run_in(&dirs, |event| told.push(event));
        assert_eq!(report.error, None);
        assert_eq!(told[..2], [Event::Steps(vec![Step::Backup, Step::Threads, Step::Settings]), Event::Step(Step::Backup)]);
        assert!(told.contains(&Event::Progress { done: 1, total: 1 }));

        // What was there is in the backups: the dbs, not the workspaces; all of the config dir.
        assert_eq!(report.backups, [at("data.bck"), at("config.bck")]);
        let names = |dir: PathBuf| {
            let mut names: Vec<_> =
                std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
            names.sort();
            names
        };
        assert_eq!(names(at("data.bck")), ["desktop.db", REPORT, "service.db"]);
        assert!(storage::from_pi(&at("data.bck/service.db")).unwrap());
        assert_eq!(names(at("config.bck")), ["agent", "config.toml", REPORT]);
        assert_eq!(std::fs::read_to_string(at("config.bck/agent/APPEND_SYSTEM.md")).unwrap(), "Be kind.");
        #[cfg(unix)]
        {
            let mode = |dir: PathBuf| std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(dir).unwrap().permissions()) & 0o777;
            assert_eq!(mode(at("config.bck")), 0o700);
        }

        // What is there now is the agent's: the db converted, pi's dir as its files.
        assert!(!pending_in(&dirs) && !dirs.config.join("agent").exists());
        assert_eq!(storage::Storage::open(&dirs.data.join("service.db")).unwrap().list_threads().unwrap().len(), 1);
        assert_eq!(Config::load(&dirs).unwrap().proxy.url, "http://127.0.0.1:8317");
        assert_eq!(std::fs::read_to_string(dirs.config.join("AGENTS.md")).unwrap(), "Be brief.");
        assert_eq!(std::fs::read_to_string(dirs.data.join("workspaces/w/big")).unwrap(), "a checkout");

        // What could not be brought over is said, and is in the report with the rest.
        assert_eq!(report.left.len(), 2, "{:?}", report.left);
        assert!(report.left[0].starts_with("The thread \"Bug hunt\" could not be converted whole ("), "{:?}", report.left);
        // Of pi's file where it is now: in the backup.
        let kept = at("config.bck/agent/APPEND_SYSTEM.md");
        assert!(report.left[1].starts_with(&format!("{} is not carried over", kept.display())), "{:?}", report.left);
        let written = std::fs::read_to_string(at("data.bck").join(REPORT)).unwrap();
        assert!(report.left.iter().chain(&report.done).all(|line| written.contains(line.as_str())), "{written}");
        assert_eq!(written, std::fs::read_to_string(at("config.bck").join(REPORT)).unwrap());

        // A later migration (only pi's settings are back) leaves that backup alone.
        std::fs::remove_file(dirs.config_file()).unwrap();
        pis_settings(&dirs);
        let report = run_in(&dirs, |_| {});
        assert_eq!((report.error, report.backups), (None, vec![at("data.bck.2"), at("config.bck.2")]));
        assert!(storage::from_pi(&at("data.bck/service.db")).unwrap() && !storage::from_pi(&at("data.bck.2/service.db")).unwrap());
    }

    #[test]
    fn a_migration_has_the_install_to_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        pis(&dirs);
        // Another one is at it.
        let other = alone(&dirs).unwrap();
        let report = run_in(&dirs, |_| {});
        assert!(report.error.as_ref().is_some_and(|error| error.contains("another Dirigent is migrating")), "{report:?}");
        assert!(report.backups.is_empty() && !tmp.path().join("data.bck").exists() && pending_in(&dirs));
        drop(other);
        // Another Dirigent runs on the db.
        let db = dirs.data.join("service.db");
        let running = rusqlite::Connection::open(&db).unwrap();
        running.pragma_update(None, "journal_mode", "WAL").unwrap();
        running.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0)).unwrap();
        let report = run_in(&dirs, |_| {});
        assert!(report.error.as_ref().is_some_and(|error| error.contains("is in use")), "{report:?}");
        assert!(report.backups.is_empty() && storage::from_pi(&db).unwrap());
        drop(running);
        assert_eq!(run_in(&dirs, |_| {}).error, None);
        assert!(!pending_in(&dirs) && !dirs.data.join(LOCK).exists());
    }

    /// What was brought over still leads into pi's dir: that stays.
    #[cfg(unix)]
    #[test]
    fn pis_dir_stays_while_a_link_leads_into_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        pis_settings(&dirs);
        let pi = dirs.config.join("agent");
        std::fs::rename(pi.join("AGENTS.md"), pi.join("mine.md")).unwrap();
        std::os::unix::fs::symlink("mine.md", pi.join("AGENTS.md")).unwrap();
        let report = run_in(&dirs, |_| {});
        assert_eq!(report.error, None);
        assert!(report.left.iter().any(|line| line.contains("stays where it is") && line.contains("leads into it")), "{:?}", report.left);
        assert_eq!(std::fs::read_to_string(dirs.config.join("AGENTS.md")).unwrap(), "Be brief.");
        // Its files are named where they are.
        assert!(report.left[0].starts_with(&pi.join("APPEND_SYSTEM.md").display().to_string()), "{:?}", report.left);
        assert!(!pending_in(&dirs));
    }

    /// A step that fails stops it there: what is pi's still is for the next start, which
    /// goes on with the backup there is.
    #[test]
    fn a_migration_that_stops_says_why_and_is_gone_on_with() {
        let tmp = tempfile::tempdir().unwrap();
        let at = |name: &str| tmp.path().join(name);
        let dirs = Dirs { config: at("config"), data: at("data") };
        pis(&dirs);
        // A db that won't convert: it has no entries.
        let db = dirs.data.join("service.db");
        let sql = |sql: &str| rusqlite::Connection::open(&db).unwrap().execute_batch(sql).unwrap();
        sql("ALTER TABLE entries RENAME TO lost");
        let report = run_in(&dirs, |_| {});
        assert!(report.error.as_ref().is_some_and(|error| error.contains("entries")), "{report:?}");
        assert_eq!(report.backups, [at("data.bck"), at("config.bck")]);
        assert!(std::fs::read_to_string(at("data.bck").join(REPORT)).unwrap().contains("It stopped: "));
        assert!(at("data.bck").join(UNFINISHED).exists() && at("config.bck").join(UNFINISHED).exists());
        // Nothing is half done: the db is pi's, and so are the settings.
        assert!(storage::from_pi(&db).unwrap() && dirs.config.join("agent").is_dir() && !dirs.config_file().exists());

        // With that fixed the next run converts the db, and stops over the settings: there
        // is something where `agent.toml` goes.
        sql("ALTER TABLE lost RENAME TO entries");
        std::fs::create_dir(dirs.config_file()).unwrap();
        let report = run_in(&dirs, |_| {});
        assert!(report.error.is_some() && !storage::from_pi(&db).unwrap() && dirs.config.join("agent").is_dir(), "{report:?}");
        let locked = report.left.iter().find(|line| line.starts_with("The thread \"Bug hunt\"")).unwrap().clone();

        // The one after finishes, with the same backup (there is no second one), and
        // says again what the one before could not bring over.
        std::fs::remove_dir(dirs.config_file()).unwrap();
        let report = run_in(&dirs, |_| {});
        assert_eq!((report.error, report.backups), (None, vec![at("data.bck"), at("config.bck")]));
        assert_eq!(report.left[0], locked);
        assert!(std::fs::read_to_string(at("data.bck").join(REPORT)).unwrap().contains(&locked));
        assert!(!at("data.bck.2").exists() && !at("config.bck.2").exists());
        assert!(!at("data.bck").join(UNFINISHED).exists() && !at("config.bck").join(UNFINISHED).exists());
        assert!(!pending_in(&dirs) && storage::from_pi(&at("data.bck/service.db")).unwrap());
        // Pi's dir stays: this run can't tell that all of it is in the backup.
        assert!(
            dirs.config.join("agent").is_dir() && report.left.iter().any(|line| line.contains("stays where it is")),
            "{:?}",
            report.left
        );
    }
}
