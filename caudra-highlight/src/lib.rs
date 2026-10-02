use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, OnceLock, RwLock};

use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter as SynHighlighter, Style as SynStyle,
    Theme,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use heredoc::Embedding;

mod heredoc;

const TOKEN_ALIASES: &[(&str, &str)] = &[("jsx", "js")];
/// Every shell grammar's scope starts with this, and only shell lines can
/// hand a heredoc body to another language.
const SHELL_SCOPE: &str = "source.shell";
pub const TAB_SPACES: &str = "  ";

type Rgb = (u8, u8, u8);

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<RwLock<Arc<Theme>>> = OnceLock::new();
static UI_COLORS: OnceLock<RwLock<HashMap<String, Rgb>>> = OnceLock::new();

fn theme_lock() -> &'static RwLock<Arc<Theme>> {
    THEME.get_or_init(|| RwLock::new(Arc::new(Theme::default())))
}

pub fn warmup() {
    syntax_set();
    theme_lock();
    let mut hl = Highlighter::for_token("bash");
    hl.highlight_line("x");
}

pub fn is_ready() -> bool {
    SYNTAX_SET.get().is_some()
}

pub fn set_theme(theme: Theme) {
    *theme_lock().write().unwrap_or_else(|e| e.into_inner()) = Arc::new(theme);
}

pub fn theme() -> Arc<Theme> {
    theme_lock()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn ui_colors_lock() -> &'static RwLock<HashMap<String, Rgb>> {
    UI_COLORS.get_or_init(RwLock::default)
}

pub fn set_ui_colors(colors: HashMap<String, Rgb>) {
    *ui_colors_lock().write().unwrap_or_else(|e| e.into_inner()) = colors;
}

pub fn theme_color(name: &str) -> Option<Rgb> {
    if let Some(&c) = ui_colors_lock()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
    {
        return Some(c);
    }
    let settings = &theme().settings;
    let map = serde_json::to_value(settings).ok()?;
    let obj = map.as_object()?;
    let val = obj.get(name)?;
    let obj = val.as_object()?;
    let r = obj.get("r")?.as_u64()? as u8;
    let g = obj.get("g")?.as_u64()? as u8;
    let b = obj.get("b")?.as_u64()? as u8;
    Some((r, g, b))
}

pub fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

pub fn normalize_text(text: &str) -> String {
    text.trim_end_matches('\n').replace('\t', TAB_SPACES)
}

pub fn syntax_for_path(path: &str) -> &'static SyntaxReference {
    syntax_set()
        .find_syntax_for_file(path)
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            let ext = path.rsplit('.').next().unwrap_or(path);
            syntax_for_token(ext)
        })
}

pub fn syntax_for_token(lang: &str) -> &'static SyntaxReference {
    let ss = syntax_set();
    ss.find_syntax_by_token(lang)
        .or_else(|| {
            TOKEN_ALIASES
                .iter()
                .find(|(from, _)| *from == lang)
                .and_then(|(_, to)| ss.find_syntax_by_token(to))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text())
}

pub struct Highlighter {
    theme: Arc<Theme>,
    state: HighlighterState,
}

/// Everything a highlighter carries from one line to the next.
#[derive(Clone)]
pub struct HighlighterState {
    parse: ParseState,
    highlight: HighlightState,
    /// `None` outside the shell grammars, the only ones that hand lines to
    /// another language.
    embedding: Option<Embedding>,
}

impl HighlighterState {
    /// One line through its grammar. Inside a heredoc body written in another
    /// language the colours come from that language, but the shell grammar
    /// still reads the line, so it is where it should be when the body ends.
    fn raw_highlight_line<'a>(
        &mut self,
        syn_hl: &SynHighlighter,
        text: &'a str,
    ) -> Result<Vec<(SynStyle, &'a str)>, syntect::Error> {
        let set = syntax_set();
        let ops = self.parse.parse_line(text, set)?;
        let ranges = HighlightIterator::new(&mut self.highlight, &ops, text, syn_hl);
        let Some(embedding) = &mut self.embedding else {
            return Ok(ranges.collect());
        };
        if let Embedding::Heredoc(heredoc) = embedding
            && !heredoc.closed_by(text)
        {
            let Some((parse, highlight)) = &mut heredoc.body else {
                return Ok(ranges.collect());
            };
            ranges.for_each(drop);
            let ops = parse.parse_line(text, set)?;
            return Ok(HighlightIterator::new(highlight, &ops, text, syn_hl).collect());
        }
        let ranges = ranges.collect();
        *embedding = Embedding::after(text, syn_hl);
        Ok(ranges)
    }

    fn segments(&mut self, syn_hl: &SynHighlighter, text: &str) -> Vec<StyledSegment> {
        match self.raw_highlight_line(syn_hl, text) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, text)| StyledSegment::from_syntect(style, normalize_text(text)))
                .collect(),
            Err(_) => vec![StyledSegment::fallback(normalize_text(text))],
        }
    }
}

impl Highlighter {
    fn new(syntax: &SyntaxReference, theme: Arc<Theme>) -> Self {
        let syn_hl = SynHighlighter::new(&theme);
        Self {
            state: HighlighterState {
                parse: ParseState::new(syntax),
                highlight: HighlightState::new(&syn_hl, ScopeStack::new()),
                embedding: syntax
                    .scope
                    .build_string()
                    .starts_with(SHELL_SCOPE)
                    .then_some(Embedding::Shell),
            },
            theme,
        }
    }

    /// Resumes mid-file from a state captured earlier. Syntect's parse state is
    /// a running fold over the lines before it, so this is the only way to
    /// highlight line N without walking 0..N again.
    pub fn from_state(theme: Arc<Theme>, state: HighlighterState) -> Self {
        Self { theme, state }
    }

    pub fn for_path(path: &str) -> Self {
        Self::new(syntax_for_path(path), theme())
    }

    pub fn for_syntax(syntax: &'static SyntaxReference) -> Self {
        Self::new(syntax, theme())
    }

    pub fn for_token(lang: &str) -> Self {
        Self::new(syntax_for_token(lang), theme())
    }

    pub fn highlight_line(&mut self, text: &str) -> Vec<StyledSegment> {
        let syn_hl = SynHighlighter::new(&self.theme);
        self.state.segments(&syn_hl, text)
    }

    /// Highlights a run of lines in one pass.
    ///
    /// [`SynHighlighter::new`] folds every scope in the theme into a selector
    /// table and sorts it, so building one per line costs about as much as the
    /// parsing it feeds. Here it is built once for the whole run. Syntect wants
    /// each line to carry its newline, which is added in a buffer the run
    /// reuses rather than a `String` per line.
    pub fn highlight_lines<'a>(
        &mut self,
        lines: impl IntoIterator<Item = &'a str>,
    ) -> Vec<Vec<StyledSegment>> {
        let syn_hl = SynHighlighter::new(&self.theme);
        let mut buffer = String::new();
        let mut out = Vec::new();
        for line in lines {
            buffer.clear();
            buffer.push_str(line);
            if !buffer.ends_with('\n') {
                buffer.push('\n');
            }
            out.push(self.state.segments(&syn_hl, &buffer));
        }
        out
    }

    pub fn advance(&mut self, text: &str) {
        let syn_hl = SynHighlighter::new(&self.theme);
        let _ = self.state.raw_highlight_line(&syn_hl, text);
    }

    pub fn state(self) -> HighlighterState {
        self.state
    }

    /// [`Self::state`] without giving up the highlighter, for a caller laying
    /// down checkpoints as it walks a file it is still walking.
    pub fn snapshot(&self) -> HighlighterState {
        self.state.clone()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StyledSegment {
    pub text: String,
    pub fg: (u8, u8, u8),
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl StyledSegment {
    fn from_syntect(style: SynStyle, text: String) -> Self {
        let f = style.foreground;
        Self {
            text,
            fg: (f.r, f.g, f.b),
            bold: style.font_style.contains(FontStyle::BOLD),
            italic: style.font_style.contains(FontStyle::ITALIC),
            underline: style.font_style.contains(FontStyle::UNDERLINE),
        }
    }

    fn fallback(text: String) -> Self {
        Self {
            text,
            fg: (204, 204, 204),
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

pub fn highlight_code(lang: &str, code: &str, prefix: &str) -> Vec<Vec<StyledSegment>> {
    let mut hl = Highlighter::for_token(lang);
    if !prefix.is_empty() {
        for line in LinesWithEndings::from(prefix) {
            hl.advance(line);
        }
    }
    LinesWithEndings::from(code)
        .map(|raw| hl.highlight_line(raw))
        .collect()
}

pub fn highlight_lines_independent(lang: &str, code: &str) -> Vec<Vec<StyledSegment>> {
    let syntax = syntax_for_token(lang);
    LinesWithEndings::from(code)
        .map(|raw| Highlighter::for_syntax(syntax).highlight_line(raw))
        .collect()
}

pub fn highlight_ansi(lang: &str, code: &str, bg: (u8, u8, u8)) -> String {
    let bg_code = format!("\x1b[48;2;{};{};{}m", bg.0, bg.1, bg.2);
    let mut hl = Highlighter::for_token(lang);
    let mut out = String::new();
    for line in LinesWithEndings::from(code) {
        out.push_str(&bg_code);
        for seg in hl.highlight_line(line) {
            let bold = if seg.bold { "1;" } else { "" };
            let _ = write!(
                out,
                "\x1b[{bold}38;2;{};{};{}m{}",
                seg.fg.0, seg.fg.1, seg.fg.2, seg.text
            );
        }
        out.push_str("\x1b[K\x1b[0m\n");
    }
    out
}

pub struct CodeHighlighter {
    checkpoint: HighlighterState,
    completed_lines: usize,
    cached_segments: Vec<Vec<StyledSegment>>,
}

impl CodeHighlighter {
    pub fn new(lang: &str) -> Self {
        Self {
            checkpoint: Highlighter::for_token(lang).state(),
            completed_lines: 0,
            cached_segments: Vec::new(),
        }
    }

    fn set_or_push(&mut self, index: usize, segments: Vec<StyledSegment>) {
        if index < self.cached_segments.len() {
            self.cached_segments[index] = segments;
        } else {
            self.cached_segments.push(segments);
        }
    }

    pub fn update(&mut self, code: &str) -> &[Vec<StyledSegment>] {
        let raw_lines: Vec<&str> = LinesWithEndings::from(code).collect();
        let total = raw_lines.len();
        if total == 0 {
            self.cached_segments.clear();
            self.completed_lines = 0;
            return &[];
        }

        let new_completed = if code.ends_with('\n') {
            total
        } else {
            total - 1
        };

        if new_completed > self.completed_lines {
            let mut hl = Highlighter::from_state(theme(), self.checkpoint.clone());

            for raw in &raw_lines[self.completed_lines..new_completed] {
                self.set_or_push(self.completed_lines, hl.highlight_line(raw));
                self.completed_lines += 1;
            }

            self.checkpoint = hl.state();
        }

        let line_count = new_completed + usize::from(new_completed < total);
        self.cached_segments.truncate(line_count);

        if new_completed < total {
            let mut hl = Highlighter::from_state(theme(), self.checkpoint.clone());
            self.set_or_push(new_completed, hl.highlight_line(raw_lines[new_completed]));
        }

        &self.cached_segments
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use syntect::highlighting::{Color, ScopeSelectors, StyleModifier, ThemeItem};
    use test_case::test_case;

    const PYTHON_BODY: &str = "import os\nprint(os.getcwd())\n";
    const KEYWORD: Color = Color {
        r: 200,
        g: 0,
        b: 0,
        a: 255,
    };
    const STRING: Color = Color {
        r: 0,
        g: 200,
        b: 0,
        a: 255,
    };

    fn segments_text(segs: &[StyledSegment]) -> String {
        segs.iter().map(|s| s.text.as_str()).collect()
    }

    fn lines_text(lines: &[Vec<StyledSegment>]) -> Vec<String> {
        lines.iter().map(|l| segments_text(l)).collect()
    }

    #[test]
    fn highlight_code_line_handling() {
        warmup();
        let single = highlight_code("rust", "fn main() {}\n", "");
        assert_eq!(single.len(), 1);
        assert_eq!(segments_text(&single[0]), "fn main() {}");

        let no_newline = highlight_code("rust", "let x = 1;", "");
        assert_eq!(no_newline.len(), 1);
        assert_eq!(segments_text(&no_newline[0]), "let x = 1;");

        let trailing = highlight_code("rust", "let x = 1;\n\n\n", "");
        assert_eq!(trailing.len(), 3);
        assert_eq!(segments_text(&trailing[1]), "");
    }

    #[test]
    fn highlight_lines_independent_ignores_cross_line_state() {
        warmup();
        let context_line = "/* start of block comment\n";
        let target_line = "let x = 42;\n";
        let combined = format!("{context_line}{target_line}");

        let stateful = highlight_code("rust", &combined, "");
        let independent = highlight_lines_independent("rust", &combined);

        assert_eq!(
            stateful.len(),
            independent.len(),
            "both should produce the same number of lines"
        );
        assert_ne!(
            stateful[1], independent[1],
            "inside a block comment the stateful highlighter should parse \
             `let x = 42;` differently than a fresh independent highlighter"
        );
    }

    #[test]
    fn syntax_for_token_fallback() {
        warmup();
        let plain = syntax_set().find_syntax_plain_text();
        assert_eq!(
            syntax_for_token("nonexistent_language_xyz").name,
            plain.name
        );
    }

    #[test]
    fn set_theme_applies_without_panic() {
        warmup();
        for _ in 0..3 {
            set_theme(Theme::default());
        }
        let mut hl = Highlighter::for_token("rust");
        assert!(!hl.highlight_line("let x = 1;\n").is_empty());
    }

    #[test]
    fn code_highlighter_streaming_consistency() {
        warmup();
        let full_code = "fn main() {\n    let x = 42;\n    println!(\"{}\", x);\n}\n";
        let full = highlight_code("rust", full_code, "");

        let mut ch = CodeHighlighter::new("rust");
        ch.update("fn main() {\n");
        ch.update("fn main() {\n    let x = 42;\n");
        let result = ch.update(full_code);

        assert_eq!(lines_text(&full), lines_text(result));
    }

    #[test]
    fn code_highlighter_partial_line() {
        warmup();
        let mut ch = CodeHighlighter::new("rust");

        ch.update("let x");
        let text1 = segments_text(&ch.update("let x")[0]);

        let text2 = segments_text(&ch.update("let x = 42")[0]);
        assert_ne!(
            text1, text2,
            "partial line should be re-highlighted as content changes"
        );
    }

    #[test]
    fn code_highlighter_shrinks() {
        warmup();
        let mut ch = CodeHighlighter::new("rust");
        ch.update("let a = 1;\nlet b = 2;\nlet c = 3;\n");
        let segs = ch.update("let a = 1;\n");
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn normalize_text_tabs_and_newlines() {
        assert_eq!(normalize_text("\t\t"), format!("{TAB_SPACES}{TAB_SPACES}"));
        assert_eq!(normalize_text("hello\n"), "hello");
        assert_eq!(normalize_text("a\tb"), format!("a{TAB_SPACES}b"));
        assert_eq!(normalize_text("hello world"), "hello world");
        assert_eq!(normalize_text(""), "");
    }

    #[test_case("test.rs" => "Rust"; "rust_extension")]
    #[test_case("test.py" => "Python"; "python_extension")]
    #[test_case("test.go" => "Go"; "go_extension")]
    #[test_case("Makefile" => "Makefile"; "makefile_no_ext")]
    fn syntax_for_path_resolves(path: &str) -> String {
        warmup();
        syntax_for_path(path).name.to_string()
    }

    #[test]
    fn syntax_for_path_unknown_falls_back() {
        warmup();
        let plain = syntax_set().find_syntax_plain_text();
        assert_eq!(syntax_for_path("file.totally_unknown_xyz").name, plain.name);
    }

    #[test]
    fn highlight_ansi_formatting() {
        warmup();
        let out = highlight_ansi("rust", "let x = 1;\nlet y = 2;\n", (30, 30, 30));
        let bg_count = out.matches("\x1b[48;2;30;30;30m").count();
        assert_eq!(bg_count, 2, "each line should get its own bg escape");
        assert!(out.ends_with("\x1b[K\x1b[0m\n"));
    }

    #[test]
    fn highlighter_advance_and_state_roundtrip() {
        warmup();
        let mut hl = Highlighter::for_token("rust");
        hl.advance("fn main() {\n");

        let mut from_state = Highlighter::from_state(theme(), hl.state());
        let seg_from_state = from_state.highlight_line("    let x = 1;\n");

        let mut fresh = Highlighter::for_token("rust");
        fresh.advance("fn main() {\n");
        let seg_fresh = fresh.highlight_line("    let x = 1;\n");

        assert_eq!(seg_from_state, seg_fresh);
    }

    #[test_case("jsx", "js"; "jsx_alias")]
    fn token_alias_resolves(alias: &str, canonical: &str) {
        warmup();
        let aliased = syntax_for_token(alias);
        let canonical_syntax = syntax_set().find_syntax_by_token(canonical).unwrap();
        assert_eq!(aliased.name, canonical_syntax.name);
    }

    /// The default theme colours every scope alike, which would make any two
    /// grammars agree.
    fn scoped_theme() -> Arc<Theme> {
        let item = |selector: &str, color| ThemeItem {
            scope: ScopeSelectors::from_str(selector).expect("selector"),
            style: StyleModifier {
                foreground: Some(color),
                background: None,
                font_style: None,
            },
        };
        Arc::new(Theme {
            scopes: vec![item("keyword", KEYWORD), item("string", STRING)],
            ..Theme::default()
        })
    }

    fn coloured(token: &str, embedding: bool, code: &str) -> Vec<Vec<StyledSegment>> {
        warmup();
        let mut highlighter = Highlighter::new(syntax_for_token(token), scoped_theme());
        if !embedding {
            highlighter.state.embedding = None;
        }
        highlighter.highlight_lines(code.lines())
    }

    #[test]
    fn a_heredoc_body_is_coloured_in_the_language_it_feeds() {
        let code = format!("python3 - <<'PY'\n{PYTHON_BODY}PY\necho done\n");
        let shell = coloured("bash", true, &code);

        assert_eq!(shell[1..3], coloured("python", true, PYTHON_BODY)[..]);
        assert_ne!(shell[1..3], coloured("bash", false, &code)[1..3]);
    }

    #[test]
    fn the_delimiter_line_returns_to_shell() {
        let code = format!("python3 - <<'PY'\n{PYTHON_BODY}PY\necho done\n");

        assert_eq!(
            coloured("bash", true, &code)[3..],
            coloured("bash", false, &code)[3..]
        );
    }

    /// The body leaves the shell grammar inside a substitution, so the lines
    /// after it keep the colours only a grammar that read the body gives.
    #[test]
    fn the_shell_reads_the_body_it_does_not_colour() {
        let code = "python3 - <<PY\nx = \"$(date\nPY\n)\"\necho done\n";

        assert_eq!(
            coloured("bash", true, code)[2..],
            coloured("bash", false, code)[2..]
        );
    }

    #[test]
    fn a_tab_stripped_delimiter_ends_the_body() {
        let code = "python3 - <<-PY\n\timport os\n\tPY\necho done\n";
        let shell = coloured("bash", true, code);

        assert_eq!(shell[1], coloured("python", true, "\timport os\n")[0]);
        assert_eq!(shell[2..], coloured("bash", false, code)[2..]);
    }

    #[test_case("cat <<EOF\nimport os\nEOF\n"; "a_command_with_no_language")]
    #[test_case("cat <<EOF\npython3 - <<PY\nimport os\nPY\nEOF\n"; "a_heredoc_written_inside_one")]
    #[test_case("python3 - <<<'import os'\nimport os\n"; "a_here_string")]
    fn a_body_with_no_language_stays_shell(code: &str) {
        assert_eq!(coloured("bash", true, code), coloured("bash", false, code));
    }

    #[test]
    fn a_resumed_state_keeps_the_heredoc_it_was_in() {
        warmup();
        let mut first = Highlighter::new(syntax_for_token("bash"), scoped_theme());
        first.highlight_line("python3 - <<'PY'\n");
        let mut resumed = Highlighter::from_state(scoped_theme(), first.snapshot());

        assert_eq!(
            resumed.highlight_line("import os\n"),
            coloured("python", true, "import os\n")[0]
        );
    }
}
