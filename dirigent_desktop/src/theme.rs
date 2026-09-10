//! Loads, watches, and exposes the application's visual theme.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    time::Duration,
};

use async_channel::Sender;
use notify::{EventKind, RecursiveMode, Watcher as _};
use serde::Deserialize;

use crate::platform;

const DEFAULT_FONT: &str = "Lilex";
const CONFIG_FILE: &str = "config.toml";
static TELEMETRY: AtomicBool = AtomicBool::new(true);

pub(crate) fn telemetry_enabled() -> bool {
    TELEMETRY.load(Ordering::Relaxed)
}

pub(crate) fn set_telemetry(enabled: bool) -> Result<(), String> {
    update_config("telemetry", toml::Value::Boolean(enabled))?;
    TELEMETRY.store(enabled, Ordering::Relaxed);
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct Appearance {
    pub(crate) font: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    config_version: u32,
    font: String,
    theme: String,
    telemetry: bool,
}

#[derive(Clone, Copy, Debug)]
struct Color(u32);

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let digits = value
            .strip_prefix('#')
            .or_else(|| value.strip_prefix("0x"))
            .unwrap_or(&value);
        if !matches!(digits.len(), 6 | 8) || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom(format!(
                "expected an RGB or RGBA color such as \"#77a7ff\" or \"#77a7ff80\", got {value:?}"
            )));
        }
        u32::from_str_radix(digits, 16)
            .map(|color| {
                Self(if digits.len() == 6 {
                    color << 8 | 0xff
                } else {
                    color
                })
            })
            .map_err(serde::de::Error::custom)
    }
}

macro_rules! define_syntax_theme {
    ($(($field:ident, $name:literal, $storage:ident, $getter:ident, $default:expr)),+ $(,)?) => {
        #[derive(Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SyntaxThemeFile {
            $(#[serde(default, rename = $name)]
            $field: Option<Color>,)+
        }

        $(static $storage: AtomicU32 = AtomicU32::new(($default << 8) | 0xff);)+

        $(pub(crate) fn $getter() -> u32 {
            $storage.load(Ordering::Relaxed)
        })+

        fn apply_syntax(theme: Option<&SyntaxThemeFile>) {
            $($storage.store(
                theme
                    .and_then(|theme| theme.$field)
                    .map_or(($default << 8) | 0xff, |color| color.0),
                Ordering::Relaxed,
            );)+
        }
    };
}

define_syntax_theme!(
    (
        annotation,
        "annotation",
        SYNTAX_ANNOTATION,
        syntax_annotation,
        0xE3D7BB
    ),
    (
        attribute,
        "attribute",
        SYNTAX_ATTRIBUTE,
        syntax_attribute,
        0x93B686
    ),
    (comment, "comment", SYNTAX_COMMENT, syntax_comment, 0x8E8379),
    (
        constant,
        "constant",
        SYNTAX_CONSTANT,
        syntax_constant,
        0xD3869B
    ),
    (
        constant_character,
        "constant.character",
        SYNTAX_CONSTANT_CHARACTER,
        syntax_constant_character,
        0x93B686
    ),
    (
        constant_character_escape,
        "constant.character.escape",
        SYNTAX_CONSTANT_CHARACTER_ESCAPE,
        syntax_constant_character_escape,
        0xDC833B
    ),
    (
        constant_macro,
        "constant.macro",
        SYNTAX_CONSTANT_MACRO,
        syntax_constant_macro,
        0x93B686
    ),
    (
        constructor,
        "constructor",
        SYNTAX_CONSTRUCTOR,
        syntax_constructor,
        0xD3869B
    ),
    (
        function,
        "function",
        SYNTAX_FUNCTION,
        syntax_function,
        0xA4A43C
    ),
    (
        function_builtin,
        "function.builtin",
        SYNTAX_FUNCTION_BUILTIN,
        syntax_function_builtin,
        0xDB9B4D
    ),
    (
        function_macro,
        "function.macro",
        SYNTAX_FUNCTION_MACRO,
        syntax_function_macro,
        0x83A598
    ),
    (keyword, "keyword", SYNTAX_KEYWORD, syntax_keyword, 0xDD5F50),
    (
        keyword_control_import,
        "keyword.control.import",
        SYNTAX_KEYWORD_CONTROL_IMPORT,
        syntax_keyword_control_import,
        0x93B686
    ),
    (label, "label", SYNTAX_LABEL, syntax_label, 0xDD5F50),
    (module, "module", SYNTAX_MODULE, syntax_module, 0x93B686),
    (
        namespace,
        "namespace",
        SYNTAX_NAMESPACE,
        syntax_namespace,
        0xE3D7BB
    ),
    (
        operator,
        "operator",
        SYNTAX_OPERATOR,
        syntax_operator,
        0xD3869B
    ),
    (
        punctuation,
        "punctuation",
        SYNTAX_PUNCTUATION,
        syntax_punctuation,
        0xDC833B
    ),
    (special, "special", SYNTAX_SPECIAL, syntax_special, 0xB16286),
    (string, "string", SYNTAX_STRING, syntax_string, 0xA4A43C),
    (
        string_regexp,
        "string.regexp",
        SYNTAX_STRING_REGEXP,
        syntax_string_regexp,
        0xDC833B
    ),
    (
        string_special,
        "string.special",
        SYNTAX_STRING_SPECIAL,
        syntax_string_special,
        0xDC833B
    ),
    (
        string_symbol,
        "string.symbol",
        SYNTAX_STRING_SYMBOL,
        syntax_string_symbol,
        0xDB9B4D
    ),
    (tag, "tag", SYNTAX_TAG, syntax_tag, 0x93B686),
    (type_name, "type", SYNTAX_TYPE, syntax_type, 0xDB9B4D),
    (
        variable,
        "variable",
        SYNTAX_VARIABLE,
        syntax_variable,
        0xE3D7BB
    ),
    (
        variable_builtin,
        "variable.builtin",
        SYNTAX_VARIABLE_BUILTIN,
        syntax_variable_builtin,
        0xDC833B
    ),
    (
        variable_other_member,
        "variable.other.member",
        SYNTAX_VARIABLE_OTHER_MEMBER,
        syntax_variable_other_member,
        0x83A598
    ),
    (
        variable_parameter,
        "variable.parameter",
        SYNTAX_VARIABLE_PARAMETER,
        syntax_variable_parameter,
        0x83A598
    ),
);

macro_rules! define_theme {
    ($(($field:ident, $storage:ident, $getter:ident, $default:expr)),+ $(,)?) => {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ThemeFile {
            $($field: Color,)+
            #[serde(default)]
            syntax: Option<SyntaxThemeFile>,
            // Accept this removed color so existing generated and custom themes keep loading.
            #[serde(default, rename = "notice_background")]
            _legacy_notice_background: Option<Color>,
        }

        $(static $storage: AtomicU32 = AtomicU32::new(($default << 8) | 0xff);)+

        $(pub(crate) fn $getter() -> u32 {
            $storage.load(Ordering::Relaxed)
        })+

        fn apply(theme: &ThemeFile) {
            $($storage.store(theme.$field.0, Ordering::Relaxed);)+
            apply_syntax(theme.syntax.as_ref());
        }
    };
}

/// Converts a packed `0xRRGGBBAA` theme color to GPUI's color type.
pub(crate) fn rgb(color: u32) -> gpui::Rgba {
    gpui::rgba(color)
}

define_theme!(
    (background, BACKGROUND, bg, 0x09090b),
    (sidebar_background, SIDEBAR_BACKGROUND, sidebar_bg, 0x040405),
    (surface, SURFACE, surface, 0x15181e),
    (surface_hover, SURFACE_HOVER, surface_hover, 0x1b1f27),
    (menu_background, MENU_BACKGROUND, menu_bg, 0x0d0e11),
    (popup_background, POPUP_BACKGROUND, popup_bg, 0x151920),
    (selection, SELECTION, selection, 0x21447a),
    (text_selection, TEXT_SELECTION, text_selection, 0x477dca),
    (border, BORDER, border, 0x252a34),
    (
        border_emphasized,
        BORDER_EMPHASIZED,
        border_emphasized,
        0x303744
    ),
    (text, TEXT, theme_text, 0xe8eaf0),
    (code_text, CODE_TEXT, code_text, 0xc7cbd4),
    (detail_text, DETAIL_TEXT, detail_text, 0x9ba3b2),
    (secondary_text, SECONDARY_TEXT, secondary_text, 0xc0c3ca),
    (muted, MUTED, muted, 0x858c9b),
    (thinking_text, THINKING_TEXT, thinking_text, 0xaeb4c0),
    (faint, FAINT, faint, 0x555d6c),
    (accent, ACCENT, accent, 0x77a7ff),
    (accent_hover, ACCENT_HOVER, accent_hover, 0x94bbff),
    (accent_surface, ACCENT_SURFACE, accent_surface, 0x1d293d),
    (blue, BLUE, blue, 0x77a7ff),
    (orange, ORANGE, orange, 0xf09a4a),
    (green, GREEN, green, 0x7fd88f),
    (red, RED, red, 0xff7b72),
    (error_text, ERROR_TEXT, error_text, 0xff9999),
    (error_background, ERROR_BACKGROUND, error_bg, 0x2a1919),
    (warning_border, WARNING_BORDER, warning_border, 0xffb15e),
    (warning_background, WARNING_BACKGROUND, warning_bg, 0x2a2117),
    (warning_text, WARNING_TEXT, warning_text, 0xffc978),
    (yellow, YELLOW, yellow, 0xf0c674),
    (purple, PURPLE, purple, 0xb48ef7),
);

const DEFAULT_THEME_FILE: &str = include_str!("../asset/theme/default.toml");
const GRUVBOX_DARK_HARD_THEME_FILE: &str = include_str!("../asset/theme/gruvbox-dark-hard.toml");
const GRUVBOX_LIGHT_SOFT_THEME_FILE: &str = include_str!("../asset/theme/gruvbox-light-soft.toml");
const EXPERIMENTAL_THEME_FILE: &str = include_str!("../asset/theme/experimental.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supplied_palettes_parse() {
        for source in [DEFAULT_THEME_FILE, GRUVBOX_DARK_HARD_THEME_FILE, GRUVBOX_LIGHT_SOFT_THEME_FILE, EXPERIMENTAL_THEME_FILE] {
            toml::from_str::<ThemeFile>(source).expect("valid supplied palette");
        }
        let experimental: ThemeFile = toml::from_str(EXPERIMENTAL_THEME_FILE).unwrap();
        assert_eq!(experimental.background.0, 0x000000aa);
    }
}

pub(crate) struct ThemePreview {
    pub(crate) name: String,
    pub(crate) colors: [u32; 3],
    palette: ThemeFile,
}

pub(crate) fn theme_previews() -> Vec<ThemePreview> {
    let Ok(dir) = platform::config_dir() else { return Vec::new() };
    let Ok(entries) = fs::read_dir(dir.join("theme")) else { return Vec::new() };
    let mut previews: Vec<_> = entries.flatten().filter_map(|entry| {
        let path = entry.path();
        if path.extension()?.to_str()? != "toml" { return None; }
        let name = path.file_stem()?.to_str()?.to_string();
        let palette: ThemeFile = toml::from_str(&fs::read_to_string(&path).ok()?).ok()?;
        Some(ThemePreview {
            name,
            colors: [palette.background.0, palette.surface.0, palette.accent.0],
            palette,
        })
    }).collect();
    previews.sort_by(|a, b| {
        (a.name == "experimental")
            .cmp(&(b.name == "experimental"))
            .then_with(|| a.name.cmp(&b.name))
    });
    previews
}

pub(crate) fn set_font(font: &str) -> Result<(), String> {
    let font = font.trim();
    if font.is_empty() || font.contains(['\n', '\r']) {
        return Err("Enter a font family name.".into());
    }
    update_config("font", toml::Value::String(font.to_string()))
}

fn update_config(key: &str, value: toml::Value) -> Result<(), String> {
    let path = platform::config_dir()?.join(CONFIG_FILE);
    let source = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut config: toml::Value = toml::from_str(&source).map_err(|e| e.to_string())?;
    config[key] = value;
    fs::write(path, toml::to_string_pretty(&config).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn select_preview(preview: &ThemePreview) -> Result<(), String> {
    update_config("theme", toml::Value::String(preview.name.clone()))?;
    apply(&preview.palette);
    Ok(())
}

pub(crate) fn default_appearance() -> Appearance {
    Appearance {
        font: DEFAULT_FONT.to_string(),
    }
}

pub(crate) fn initialize() -> Result<(PathBuf, Appearance), String> {
    let config_dir = platform::config_dir()?;
    generate_config(&config_dir)?;
    let appearance = reload(&config_dir)?;
    Ok((config_dir, appearance))
}

fn generate_config(config_dir: &Path) -> Result<(), String> {
    let theme_dir = config_dir.join("theme");
    fs::create_dir_all(&theme_dir).map_err(|error| {
        format!(
            "could not create configuration directory {}: {error}",
            theme_dir.display()
        )
    })?;

    for (name, contents) in [
        ("default.toml", DEFAULT_THEME_FILE),
        ("gruvbox-dark-hard.toml", GRUVBOX_DARK_HARD_THEME_FILE),
        ("gruvbox-light-soft.toml", GRUVBOX_LIGHT_SOFT_THEME_FILE),
        ("experimental.toml", EXPERIMENTAL_THEME_FILE),
    ] {
        write_new_file(&theme_dir.join(name), contents)?;
    }

    write_new_file(
        &config_dir.join(CONFIG_FILE),
        "config_version = 0\nfont = \"Lilex\"\ntheme = \"default\"\ntelemetry = true\n",
    )
}

/// Seeds defaults without ever overwriting a file the user already owns.
fn write_new_file(path: &Path, contents: &str) -> Result<(), String> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file.write_all(contents.as_bytes()).map_err(|error| {
            format!(
                "could not write generated config {}: {error}",
                path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!(
            "could not create generated config {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn reload(config_dir: &Path) -> Result<Appearance, String> {
    let config_path = config_dir.join(CONFIG_FILE);
    let config_source = fs::read_to_string(&config_path)
        .map_err(|error| format!("could not read {}: {error}", config_path.display()))?;
    let config: ConfigFile = toml::from_str(&config_source)
        .map_err(|error| format!("could not parse {}: {error}", config_path.display()))?;
    if config.config_version != 0 {
        return Err(format!(
            "unsupported config_version {} in {}; expected 0",
            config.config_version,
            config_path.display()
        ));
    }
    if config.font.trim().is_empty() {
        return Err(format!("font cannot be empty in {}", config_path.display()));
    }
    validate_theme_name(&config.theme)?;

    let theme_path = config_dir
        .join("theme")
        .join(format!("{}.toml", config.theme));
    let theme_source = fs::read_to_string(&theme_path)
        .map_err(|error| format!("could not read theme {}: {error}", theme_path.display()))?;
    let theme: ThemeFile = toml::from_str(&theme_source)
        .map_err(|error| format!("could not parse theme {}: {error}", theme_path.display()))?;

    // Do not alter the active palette unless both files were read and validated successfully.
    apply(&theme);
    TELEMETRY.store(config.telemetry, Ordering::Relaxed);
    Ok(Appearance { font: config.font })
}

/// Restricts theme names to one normal path component inside the theme directory.
fn validate_theme_name(name: &str) -> Result<(), String> {
    let mut components = Path::new(name).components();
    let valid = !name.is_empty()
        && !name.contains(['/', '\\'])
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none();
    if valid {
        Ok(())
    } else {
        Err(format!("invalid theme name {name:?} in config.toml"))
    }
}

/// Watches the full Dirigent configuration tree and emits one event after a short debounce.
pub(crate) fn watch(config_dir: PathBuf, events: Sender<Result<(), String>>) -> Result<(), String> {
    let (raw_tx, raw_rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = raw_tx.send(event);
    })
    .map_err(|error| format!("could not create config watcher: {error}"))?;
    watcher
        .watch(&config_dir, RecursiveMode::Recursive)
        .map_err(|error| format!("could not watch {}: {error}", config_dir.display()))?;

    std::thread::Builder::new()
        .name("dirigent-config-watcher".into())
        .spawn(move || {
            let _watcher = watcher;
            while let Ok(first) = raw_rx.recv() {
                let mut changed = relevant_event(&first);
                let mut error = first.err().map(|error| error.to_string());
                while let Ok(next) = raw_rx.recv_timeout(Duration::from_millis(100)) {
                    changed |= relevant_event(&next);
                    if let Err(next_error) = next {
                        error = Some(next_error.to_string());
                    }
                }
                let event = if let Some(error) = error {
                    Err(format!("configuration watch error: {error}"))
                } else {
                    Ok(())
                };
                if changed && events.send_blocking(event).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| format!("could not start config watcher: {error}"))?;
    Ok(())
}

fn relevant_event(event: &notify::Result<notify::Event>) -> bool {
    event.as_ref().is_ok_and(|event| {
        matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        )
    })
}
