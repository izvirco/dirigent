use super::*;

struct HighlightLoader {
    configs: Vec<LanguageConfig>,
    names: HashMap<&'static str, Language>,
}

impl HighlightLoader {
    fn new() -> Self {
        let mut loader = Self {
            configs: Vec::new(),
            names: HashMap::new(),
        };
        macro_rules! add {
            ($names:expr, $grammar:expr, $highlights:expr, $injections:expr, $locals:expr) => {{
                let names: &[&'static str] = &$names;
                match Grammar::try_from($grammar) {
                    Ok(grammar) => match LanguageConfig::new(grammar, $highlights, $injections, $locals) {
                        Ok(config) => {
                            let language = Language::new(loader.configs.len() as u32);
                            config.configure(|capture| highlight_for_capture(capture));
                            loader.configs.push(config);
                            for &name in names { loader.names.insert(name, language); }
                        }
                        Err(error) => {
                            tracing::warn!(language = names[0], error = %error, "could not load syntax query");
                        }
                    },
                    Err(error) => {
                        tracing::warn!(language = names[0], error = %error, "could not load syntax grammar");
                    }
                }
            }};
        }
        add!(
            ["rust", "rs"],
            tree_sitter_rust::LANGUAGE,
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
            ""
        );
        add!(
            ["javascript", "js", "jsx"],
            tree_sitter_javascript::LANGUAGE,
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            "",
            tree_sitter_javascript::LOCALS_QUERY
        );
        add!(
            ["typescript", "ts"],
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
            tree_sitter_typescript::LOCALS_QUERY
        );
        add!(
            ["tsx"],
            tree_sitter_typescript::LANGUAGE_TSX,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
            tree_sitter_typescript::LOCALS_QUERY
        );
        add!(
            ["json"],
            tree_sitter_json::LANGUAGE,
            tree_sitter_json::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["toml"],
            tree_sitter_toml_ng::LANGUAGE,
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["python", "py"],
            tree_sitter_python::LANGUAGE,
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["go"],
            tree_sitter_go::LANGUAGE,
            tree_sitter_go::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["bash", "sh"],
            tree_sitter_bash::LANGUAGE,
            tree_sitter_bash::HIGHLIGHT_QUERY,
            "",
            ""
        );
        add!(
            ["c"],
            tree_sitter_c::LANGUAGE,
            tree_sitter_c::HIGHLIGHT_QUERY,
            "",
            ""
        );
        add!(
            ["cpp", "c++"],
            tree_sitter_cpp::LANGUAGE,
            tree_sitter_cpp::HIGHLIGHT_QUERY,
            "",
            ""
        );
        add!(
            ["html"],
            tree_sitter_html::LANGUAGE,
            tree_sitter_html::HIGHLIGHTS_QUERY,
            tree_sitter_html::INJECTIONS_QUERY,
            ""
        );
        add!(
            ["css"],
            tree_sitter_css::LANGUAGE,
            tree_sitter_css::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["yaml", "yml"],
            tree_sitter_yaml::LANGUAGE,
            tree_sitter_yaml::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["markdown", "md"],
            tree_sitter_md::LANGUAGE,
            tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            tree_sitter_md::INJECTION_QUERY_BLOCK,
            ""
        );
        add!(
            ["markdown_inline"],
            tree_sitter_md::INLINE_LANGUAGE,
            tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
            tree_sitter_md::INJECTION_QUERY_INLINE,
            ""
        );
        loader
    }

    fn language_for_path(&self, path: &str) -> Option<Language> {
        let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
        self.names.get(extension.as_str()).copied()
    }

    fn marker_name<'a>(&self, marker: tree_house::InjectionLanguageMarker<'a>) -> Option<String> {
        match marker {
            tree_house::InjectionLanguageMarker::Name(name) => Some(name.to_ascii_lowercase()),
            tree_house::InjectionLanguageMarker::Match(text)
            | tree_house::InjectionLanguageMarker::Filename(text)
            | tree_house::InjectionLanguageMarker::Shebang(text) => {
                Some(text.to_string().trim().to_ascii_lowercase())
            }
        }
    }
}

impl LanguageLoader for HighlightLoader {
    fn language_for_marker(
        &self,
        marker: tree_house::InjectionLanguageMarker<'_>,
    ) -> Option<Language> {
        let marker = self.marker_name(marker)?;
        self.names.get(marker.as_str()).copied().or_else(|| {
            Path::new(&marker)
                .extension()
                .and_then(|ext| ext.to_str())
                .and_then(|ext| self.names.get(ext).copied())
        })
    }

    fn get_config(&self, lang: Language) -> Option<&LanguageConfig> {
        self.configs.get(lang.idx())
    }
}

#[derive(Clone, Copy)]
#[repr(usize)]
enum SyntaxCapture {
    Annotation,
    Attribute,
    Comment,
    Constant,
    ConstantCharacter,
    ConstantCharacterEscape,
    ConstantMacro,
    Constructor,
    Function,
    FunctionBuiltin,
    FunctionMacro,
    Keyword,
    KeywordControlImport,
    Label,
    Module,
    Namespace,
    Operator,
    Punctuation,
    Special,
    String,
    StringRegexp,
    StringSpecial,
    StringSymbol,
    Tag,
    Type,
    Variable,
    VariableBuiltin,
    VariableOtherMember,
    VariableParameter,
}

fn capture_for_scope(scope: &str) -> Option<SyntaxCapture> {
    Some(match scope {
        "annotation" => SyntaxCapture::Annotation,
        "attribute" => SyntaxCapture::Attribute,
        "comment" => SyntaxCapture::Comment,
        "constant" => SyntaxCapture::Constant,
        "constant.character" | "character" => SyntaxCapture::ConstantCharacter,
        "constant.character.escape" | "escape" | "string.escape" => {
            SyntaxCapture::ConstantCharacterEscape
        }
        "constant.macro" => SyntaxCapture::ConstantMacro,
        "constructor" => SyntaxCapture::Constructor,
        "function" | "method" => SyntaxCapture::Function,
        "function.builtin" => SyntaxCapture::FunctionBuiltin,
        "function.macro" => SyntaxCapture::FunctionMacro,
        "keyword" => SyntaxCapture::Keyword,
        "keyword.control.import" | "import" | "include" => SyntaxCapture::KeywordControlImport,
        "label" => SyntaxCapture::Label,
        "module" => SyntaxCapture::Module,
        "namespace" => SyntaxCapture::Namespace,
        "operator" => SyntaxCapture::Operator,
        "punctuation" | "delimiter" => SyntaxCapture::Punctuation,
        "special" | "embedded" => SyntaxCapture::Special,
        "string" => SyntaxCapture::String,
        "string.regexp" => SyntaxCapture::StringRegexp,
        "string.special" => SyntaxCapture::StringSpecial,
        "string.symbol" => SyntaxCapture::StringSymbol,
        "tag" => SyntaxCapture::Tag,
        "type" => SyntaxCapture::Type,
        "variable" => SyntaxCapture::Variable,
        "variable.builtin" => SyntaxCapture::VariableBuiltin,
        "variable.other.member" | "property" => SyntaxCapture::VariableOtherMember,
        "variable.parameter" | "parameter" => SyntaxCapture::VariableParameter,
        "number" | "boolean" => SyntaxCapture::Constant,
        _ => return None,
    })
}

fn highlight_for_capture(capture: &str) -> Option<Highlight> {
    let mut scope = capture;
    loop {
        if let Some(capture) = capture_for_scope(scope) {
            return Some(Highlight::new(capture as u32));
        }
        let (parent, _) = scope.rsplit_once('.')?;
        scope = parent;
    }
}

fn capture_color(index: usize) -> u32 {
    const COLORS: [fn() -> u32; 29] = [
        theme::syntax_annotation,
        theme::syntax_attribute,
        theme::syntax_comment,
        theme::syntax_constant,
        theme::syntax_constant_character,
        theme::syntax_constant_character_escape,
        theme::syntax_constant_macro,
        theme::syntax_constructor,
        theme::syntax_function,
        theme::syntax_function_builtin,
        theme::syntax_function_macro,
        theme::syntax_keyword,
        theme::syntax_keyword_control_import,
        theme::syntax_label,
        theme::syntax_module,
        theme::syntax_namespace,
        theme::syntax_operator,
        theme::syntax_punctuation,
        theme::syntax_special,
        theme::syntax_string,
        theme::syntax_string_regexp,
        theme::syntax_string_special,
        theme::syntax_string_symbol,
        theme::syntax_tag,
        theme::syntax_type,
        theme::syntax_variable,
        theme::syntax_variable_builtin,
        theme::syntax_variable_other_member,
        theme::syntax_variable_parameter,
    ];
    COLORS
        .get(index)
        .map_or_else(theme::code_text, |color| color())
}

pub(super) fn highlight_text(path: &str, text: &str) -> Vec<SyntaxSpan> {
    static LOADER: std::sync::OnceLock<HighlightLoader> = std::sync::OnceLock::new();
    let loader = LOADER.get_or_init(HighlightLoader::new);
    let Some(language) = loader.language_for_path(path) else {
        return Vec::new();
    };
    if text.is_empty() || text.len() > u32::MAX as usize {
        return Vec::new();
    }
    let rope = Rope::from_str(text);
    let source: RopeSlice<'_> = rope.slice(..);
    let Ok(syntax) = Syntax::new(source, language, Duration::from_millis(500), loader) else {
        return Vec::new();
    };
    let mut highlighter = Highlighter::new(&syntax, source, loader, 0..text.len() as u32);
    let mut position = highlighter.next_event_offset();
    let mut active = Vec::new();
    let mut spans = Vec::new();
    while position < text.len() as u32 {
        let (event, highlights) = highlighter.advance();
        if event == HighlightEvent::Refresh {
            active.clear();
        }
        active.extend(highlights);
        let start = position;
        position = highlighter.next_event_offset().min(text.len() as u32);
        if position <= start {
            break;
        }
        if let Some(highlight) = active.last() {
            spans.push(SyntaxSpan {
                range: start as usize..position as usize,
                color: capture_color(highlight.idx()),
            });
        }
    }
    spans
}
