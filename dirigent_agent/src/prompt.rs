use crate::config::Dirs;
use crate::tool::ToolSpec;
use dirigent_protocol::visuals::{FENCE, WIDTH};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SERVERS: &str = "These MCP servers' tools are not declared to you: call them from codemode scripts. In a script, \
`describeNamespace(\"<server>\")` says how to use a server and names its tools, `searchTools(query)` finds tools, and \
`describeTool(name)` has a tool's arguments.";

const IMAGES: &str = "## Showing images
Your replies are rendered as markdown in a desktop app. To show the user an image file on this machine (a screenshot you took, a \
chart you rendered), put it in your reply as a markdown image with its path: `![what it shows](/absolute/path.png)`. It appears \
inline in the reply. png, jpg, gif, webp and bmp work; a relative path is taken from the working directory.

Images the user attaches to a message are also saved as files on this machine: the `[image-N]: /path` lines at the end of the \
message give the file of each `[image-N]`, for when you need the image itself (to upload, edit or attach it).";

fn visuals() -> String {
    format!(
        "## Showing visuals
When a picture says it better than text and markdown can (a chart, a diagram, a layout, a timeline), write it as HTML and CSS in a \
fenced code block with the info string `{FENCE}`:

```{FENCE}
<div style=\"display: flex; gap: 8px\">…</div>
```

It is drawn inline in the reply, as a picture {WIDTH} CSS px wide and as tall as its content: the page has no height of its own, so \
`100vh` is zero.

- HTML and CSS only (flexbox, grid, tables, inline SVG): no JavaScript, no animation or interaction, and nothing loaded from \
elsewhere (images, fonts, stylesheets).
- The background is transparent over the app's theme, dark or light, and text has the theme's colour and font. Take colours from \
the theme's variables: `--fg`, `--muted`, `--border`, `--surface`, `--accent`, `--info`, `--success`, `--warning`, `--danger`.
- In an SVG, style elements with their attributes (`fill=\"var(--accent)\"`, `stroke=\"currentColor\"`) rather than CSS: the page's \
rules don't reach an SVG, and the theme's variables only work in its attributes. Set no `font-family` there: text in a font the \
machine lacks is not drawn."
    )
}

const CWD: &str = "Every tool call starts in this directory: bash commands run in it, and relative paths are taken from it. So no \
command needs a `cd` to it first; `cd` is for running something in another directory.";

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prompt {
    instructions: Vec<(PathBuf, String)>,
    skills: Vec<Skill>,
    cwd: PathBuf,
}

impl Prompt {
    pub fn load(dirs: &Dirs, cwd: &Path) -> Self {
        let mut instructions: Vec<(PathBuf, String)> = instructions_in(&dirs.config).into_iter().collect();
        let above: Vec<_> =
            cwd.ancestors().filter_map(instructions_in).filter(|(path, _)| !instructions.iter().any(|(p, _)| p == path)).collect();
        instructions.extend(above.into_iter().rev());
        Self { instructions, skills: skills(&dirs.config.join("skills")), cwd: cwd.to_owned() }
    }

    pub fn system(&self, tools: &[ToolSpec], servers: &[(String, Option<String>)]) -> String {
        let mut out = String::from(
            "You are a coding agent running in Dirigent, a desktop app for working with coding agents. You work on the user's \
             machine, in their project: inspect files, run commands and change code to do what they ask.",
        );
        let mut section = |name: &str, body: &str| out.push_str(&format!("\n\n<{name}>\n{body}\n</{name}>"));
        let listed: Vec<String> = tools.iter().filter_map(|t| Some(format!("- {}: {}", t.name, t.snippet.as_ref()?))).collect();
        section(
            "tools",
            &format!("{}\n\nAdditional tools may be available.", if listed.is_empty() { "(none)".into() } else { listed.join("\n") }),
        );
        let has = |name: &str| tools.iter().any(|t| t.name == name);
        let mut rules: Vec<&str> = vec![];
        if has("bash") {
            rules.push("Use bash for file operations like ls, rg, find");
        }
        rules.extend(tools.iter().flat_map(|t| t.guidelines.iter().map(String::as_str)));
        rules.extend(["Be concise in your responses", "Show file paths clearly when working with files"]);
        let mut seen = HashSet::new();
        let rules: Vec<String> = rules.into_iter().filter(|r| seen.insert(*r)).map(|r| format!("- {r}")).collect();
        section("rules", &rules.join("\n"));
        if !servers.is_empty() {
            let listed: Vec<String> = servers
                .iter()
                .map(|(name, about)| match about {
                    Some(about) => format!("- {name}: {}", about.lines().next().unwrap_or_default()),
                    None => format!("- {name}"),
                })
                .collect();
            section(
                "mcp_servers",
                &format!(
                    "{SERVERS}

{}",
                    listed.join(
                        "
"
                    )
                ),
            );
        }
        section("addendum", &format!("{IMAGES}\n\n{}", visuals()));
        if !self.instructions.is_empty() {
            let files: Vec<String> = self
                .instructions
                .iter()
                .map(|(path, text)| format!("<project_instructions path=\"{}\">\n{text}\n</project_instructions>", path.display()))
                .collect();
            section("project_context", &format!("Project-specific instructions and guidelines:\n\n{}", files.join("\n\n")));
        }
        let reader = if has("read") {
            Some("Use the read tool to load")
        } else if has("bash") {
            Some("Use bash to load")
        } else {
            None
        };
        if let Some(reader) = reader.filter(|_| !self.skills.is_empty()) {
            let mut lines = vec![
                "The following skills provide specialized instructions for specific tasks.".to_owned(),
                format!("{reader} a skill's file when the task matches its description."),
                "Resolve relative paths against the skill's directory (parent of SKILL.md). Use absolute paths in tool calls.".to_owned(),
                String::new(),
                "<available_skills>".to_owned(),
            ];
            for skill in &self.skills {
                lines.push(format!(
                    "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>",
                    xml(&skill.name),
                    xml(&skill.description),
                    xml(&skill.path.to_string_lossy())
                ));
            }
            lines.push("</available_skills>".to_owned());
            section("skills", &lines.join("\n"));
        }
        section("cwd", &format!("{}\n\n{CWD}", self.cwd.to_string_lossy().replace('\\', "/")));
        out
    }
}

fn instructions_in(dir: &Path) -> Option<(PathBuf, String)> {
    ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD", "agents.md", "CLAUDE.md", "CLAUDE.MD"].iter().find_map(|name| {
        let path = dir.join(name);
        let text = std::fs::read_to_string(&path).ok()?;
        Some((path, text.trim_start_matches('\u{FEFF}').to_owned()))
    })
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn skills(dir: &Path) -> Vec<Skill> {
    let mut found = vec![];
    collect(dir, true, &mut found);
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|b, a| a.name == b.name);
    found
}

fn collect(dir: &Path, top: bool, found: &mut Vec<Skill>) {
    if let Some(skill) = ["SKILL.md", "skill.md"].iter().find_map(|name| skill(&dir.join(name))) {
        found.push(skill);
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        // `is_dir` follows links: a skill may be linked in from elsewhere.
        if path.is_dir() {
            collect(&path, false, found);
        } else if top && name.ends_with(".md") {
            found.extend(skill(&path));
        }
    }
}

fn skill(path: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(path).ok()?;
    let fields = frontmatter(&text);
    let field = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.trim()).filter(|v| !v.is_empty());
    if field("disable-model-invocation") == Some("true") {
        return None;
    }
    let description = field("description")?.to_owned();
    let dir_name = path.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned());
    Some(Skill { name: field("name").map(str::to_owned).or(dir_name)?, description, path: path.to_owned() })
}

fn frontmatter(text: &str) -> Vec<(String, String)> {
    let mut lines = text.trim_start_matches('\u{FEFF}').lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return vec![];
    }
    let mut fields: Vec<(String, String)> = vec![];
    for line in lines.take_while(|l| l.trim_end() != "---") {
        let indented = line.starts_with([' ', '\t']);
        match (line.split_once(':'), fields.last_mut()) {
            (_, Some((_, value))) if indented => {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            (Some((key, value)), _) if !indented => {
                let value = value.trim();
                let value = if matches!(value, "|" | ">" | "|-" | ">-") { "" } else { value };
                let unquoted = ['"', '\''].iter().find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q)).unwrap_or(value);
                fields.push((key.trim().to_owned(), unquoted.to_owned()));
            }
            _ => {}
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn instructions_come_outermost_first_and_skills_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, repo) = (tmp.path().join("config"), tmp.path().join("repo"));
        let cwd = repo.join("crate");
        std::fs::create_dir_all(config.join("skills/ship")).unwrap();
        std::fs::create_dir_all(config.join("skills/nested/review")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(config.join("AGENTS.md"), "mine").unwrap();
        std::fs::write(repo.join("agents.md"), "the repo's").unwrap();
        std::fs::write(cwd.join("CLAUDE.md"), "the crate's").unwrap();
        std::fs::write(config.join("skills/ship/SKILL.md"), "---\nname: ship\ndescription: Use when asked to ship <changes>\n---\nbody")
            .unwrap();
        std::fs::write(config.join("skills/nested/review/SKILL.md"), "---\ndescription: >\n  Review a change,\n  carefully\n---\n")
            .unwrap();
        std::fs::create_dir_all(config.join("skills/lower")).unwrap();
        std::fs::write(config.join("skills/lower/skill.md"), "---\ndescription: In lowercase\n---\n").unwrap();
        std::fs::write(config.join("skills/hidden.md"), "---\ndescription: x\ndisable-model-invocation: true\n---\n").unwrap();
        std::fs::write(config.join("skills/notes.md"), "no frontmatter").unwrap();

        let prompt = Prompt::load(&Dirs { config: config.clone(), data: tmp.path().join("data") }, &cwd);
        let texts: Vec<&str> = prompt.instructions.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(texts, ["mine", "the repo's", "the crate's"]);
        let skills: Vec<(&str, &str)> = prompt.skills.iter().map(|s| (s.name.as_str(), s.description.as_str())).collect();
        assert_eq!(
            skills,
            [("lower", "In lowercase"), ("review", "Review a change, carefully"), ("ship", "Use when asked to ship <changes>")]
        );

        let read = ToolSpec {
            snippet: Some("Read file contents".into()),
            guidelines: vec!["Use read.".into()],
            ..ToolSpec::new("read", "", json!({}))
        };
        let bash = ToolSpec { guidelines: vec!["Use read.".into()], ..ToolSpec::new("bash", "", json!({})) };
        let system = prompt.system(&[read, bash], &[]);
        assert!(!system.contains("mcp_servers"));
        assert!(system.contains("<tools>\n- read: Read file contents\n\nAdditional tools may be available.\n</tools>"));
        assert!(
            system.contains("<rules>\n- Use bash for file operations like ls, rg, find\n- Use read.\n- Be concise in your responses\n")
        );
        assert!(system.contains(&format!(
            "<project_instructions path=\"{}\">\nthe repo's\n</project_instructions>",
            repo.join("agents.md").display()
        )));
        assert!(system.contains("<description>Use when asked to ship &lt;changes&gt;</description>"));
        assert!(system.contains("the info string `visual`:\n\n```visual\n") && system.contains("720 CSS px wide"));
        assert!(system.ends_with(&format!("<cwd>\n{}\n\n{CWD}\n</cwd>", cwd.display())));
        assert_eq!(
            system,
            prompt.system(
                &[
                    ToolSpec {
                        snippet: Some("Read file contents".into()),
                        guidelines: vec!["Use read.".into()],
                        ..ToolSpec::new("read", "", json!({}))
                    },
                    ToolSpec { guidelines: vec!["Use read.".into()], ..ToolSpec::new("bash", "", json!({})) },
                ],
                &[]
            )
        );
        let servers = [("mcp__docs".to_owned(), Some("Our docs.\nAll of them.".to_owned())), ("mcp__sentry".to_owned(), None)];
        assert!(prompt.system(&[], &servers).contains("\n\n- mcp__docs: Our docs.\n- mcp__sentry\n</mcp_servers>"));
    }
}
