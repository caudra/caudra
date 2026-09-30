use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use arc_swap::{ArcSwap, Guard};
use caudra_storage::StateDir;
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use syntect::highlighting::{
    Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, ThemeItem, ThemeSettings,
};

/// Applied on first run and whenever no theme has been picked or configured.
pub const DEFAULT_THEME: &str = "opencode";
const MESSAGE_BACKGROUND_TINT: f32 = 0.08;
const ELEMENT_BACKGROUND_TINT: f32 = 0.1;
const PANEL_BACKGROUND_TINT: f32 = 0.04;
const RESERVED_KEYS: &[&str] = &["palette", "ui", "inherits"];
const CURSOR_KEY: &str = "cursor";
/// The caret of a theme that names no `cursor`: the cell under it reversed,
/// which shows on any palette. Every text field draws its caret in `cursor`,
/// so an unset one would leave the composer with no caret at all.
const CURSOR_FALLBACK: Style = Style::new().add_modifier(Modifier::REVERSED);

/// Minimum contrast ratio for roles that carry text a user has to read.
const MIN_CONTRAST_TEXT: f32 = 3.0;
/// Minimum contrast ratio for purely decorative chrome (borders, rules, gutters).
const MIN_CONTRAST_CHROME: f32 = 2.0;
/// Roles held to [`MIN_CONTRAST_CHROME`] instead of [`MIN_CONTRAST_TEXT`].
const CHROME_ROLES: &[&str] = &[
    "panel_border",
    "table_border",
    "horizontal_rule",
    "plan_rule",
    "input_border",
    "diff_line_nr",
    "code_gutter",
    "index_line_nr",
];
/// Bisection steps used to lift a color to the contrast floor.
const CONTRAST_STEPS: u32 = 24;

const HELIX_TO_TEXTMATE: &[(&str, &str)] = &[
    ("comment", "comment, comment punctuation.definition.comment"),
    (
        "comment.line",
        "comment.line, comment.line punctuation.definition.comment",
    ),
    (
        "comment.block",
        "comment.block, comment.block punctuation.definition.comment",
    ),
    (
        "comment.line.documentation",
        "comment.line.documentation, comment.line.documentation punctuation.definition.comment",
    ),
    (
        "comment.block.documentation",
        "comment.block.documentation, comment.block.documentation punctuation.definition.comment",
    ),
    ("string", "string, string punctuation.definition.string"),
    (
        "string.regexp",
        "string.regexp, string.regexp punctuation.definition.string",
    ),
    (
        "string.special",
        "string.special, string.quoted.single punctuation.definition.string, string.quoted.double.raw punctuation.definition.string",
    ),
    ("function", "entity.name.function, variable.function"),
    ("function.builtin", "support.function"),
    (
        "function.call",
        "entity.name.function, variable.function, support.function",
    ),
    (
        "function.macro",
        "entity.name.function.macro, support.macro",
    ),
    (
        "function.method",
        "entity.name.function, meta.function-call",
    ),
    ("constructor", "entity.name.function.constructor"),
    (
        "type",
        "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.union, entity.name.impl, support.type, support.class, meta.generic",
    ),
    ("type.builtin", "support.type, storage.type.primitive"),
    ("type.enum.variant", "entity.name.type.enum"),
    ("tag", "entity.name.tag"),
    ("tag.attribute", "entity.other.attribute-name"),
    ("tag.delimiter", "punctuation.definition.tag"),
    ("variable", "variable.other"),
    ("variable.builtin", "variable.language"),
    ("variable.parameter", "variable.parameter"),
    (
        "variable.other.member",
        "variable.other.member, variable.other.property",
    ),
    (
        "constant",
        "constant, variable.other.constant, entity.name.constant",
    ),
    ("constant.builtin", "constant.language"),
    (
        "constant.builtin.boolean",
        "constant.language.boolean, constant.language",
    ),
    (
        "constant.character.escape",
        "constant.character.escape, constant.character.escaped",
    ),
    (
        "keyword.storage.type",
        "storage.type, keyword.declaration, keyword.declaration.function, keyword.declaration.class, keyword.declaration.struct, keyword.declaration.enum, keyword.declaration.trait, keyword.declaration.impl",
    ),
    ("keyword.storage.modifier", "storage.modifier"),
    (
        "keyword.function",
        "keyword.declaration.function, storage.type.function",
    ),
    (
        "keyword.control.import",
        "keyword.control.import, keyword.other",
    ),
    ("keyword.return", "keyword.control.return, keyword.control"),
    ("keyword.directive", "meta.preprocessor"),
    ("keyword.control.exception", "keyword.control.exception"),
    ("punctuation", "punctuation, punctuation.accessor.dot"),
    (
        "punctuation.special",
        "punctuation.section.embedded, punctuation.section.interpolation, punctuation.separator.namespace, punctuation.accessor",
    ),
    ("label", "entity.name.label, storage.modifier.lifetime"),
    (
        "attribute",
        "entity.other.attribute-name, meta.annotation, variable.annotation, meta.annotation punctuation.definition.annotation, meta.annotation punctuation.section.group",
    ),
    (
        "namespace",
        "entity.name.namespace, entity.name.module, meta.path",
    ),
    (
        "markup.raw",
        "markup.raw, markup.raw.inline, markup.raw.block",
    ),
    ("markup.link.url", "markup.underline.link"),
    ("operator", "keyword.operator"),
];

pub struct ThemeEntry {
    pub name: &'static str,
    pub toml: &'static str,
}

/// Two themes that are the same design drawn for opposite backgrounds.
pub struct ThemePair {
    pub dark: &'static str,
    pub light: &'static str,
}

impl ThemePair {
    fn covers(&self, name: &str) -> bool {
        self.dark == name || self.light == name
    }
}

/// Themes that ship as a light and dark pair. Choosing either half is what
/// turns on following the terminal background, so a theme listed here needs
/// no configuration to track the terminal, and one that is not listed never
/// changes on its own.
///
/// Only canonical pairs belong here. Themes with several dark variants keep
/// the one that shares the light half's name, and `ui.theme_light` covers
/// any other combination.
pub static THEME_PAIRS: &[ThemePair] = &[
    ThemePair {
        dark: "ayu_dark",
        light: "ayu_light",
    },
    ThemePair {
        dark: "catppuccin_mocha",
        light: "catppuccin_latte",
    },
    ThemePair {
        dark: "gruvbox",
        light: "gruvbox_light",
    },
    ThemePair {
        dark: "opencode",
        light: "opencode_light",
    },
    ThemePair {
        dark: "rose_pine",
        light: "rose_pine_dawn",
    },
    ThemePair {
        dark: "solarized_dark",
        light: "solarized_light",
    },
];

/// The pair `name` belongs to, whichever half it names.
pub fn pair_for(name: &str) -> Option<&'static ThemePair> {
    THEME_PAIRS.iter().find(|pair| pair.covers(name))
}

pub static BUNDLED_THEMES: &[ThemeEntry] = &[
    ThemeEntry {
        name: "ayu_dark",
        toml: include_str!("themes/ayu_dark.toml"),
    },
    ThemeEntry {
        name: "ayu_light",
        toml: include_str!("themes/ayu_light.toml"),
    },
    ThemeEntry {
        name: "ayu_mirage",
        toml: include_str!("themes/ayu_mirage.toml"),
    },
    ThemeEntry {
        name: "carbonfox",
        toml: include_str!("themes/carbonfox.toml"),
    },
    ThemeEntry {
        name: "catppuccin_frappe",
        toml: include_str!("themes/catppuccin_frappe.toml"),
    },
    ThemeEntry {
        name: "catppuccin_latte",
        toml: include_str!("themes/catppuccin_latte.toml"),
    },
    ThemeEntry {
        name: "catppuccin_macchiato",
        toml: include_str!("themes/catppuccin_macchiato.toml"),
    },
    ThemeEntry {
        name: "catppuccin_mocha",
        toml: include_str!("themes/catppuccin_mocha.toml"),
    },
    ThemeEntry {
        name: "dark_daltonized",
        toml: include_str!("themes/dark_daltonized.toml"),
    },
    ThemeEntry {
        name: "dracula",
        toml: include_str!("themes/dracula.toml"),
    },
    ThemeEntry {
        name: "everforest_dark",
        toml: include_str!("themes/everforest_dark.toml"),
    },
    ThemeEntry {
        name: "fleet_dark",
        toml: include_str!("themes/fleet_dark.toml"),
    },
    ThemeEntry {
        name: "github_dark",
        toml: include_str!("themes/github_dark.toml"),
    },
    ThemeEntry {
        name: "gruvbox",
        toml: include_str!("themes/gruvbox.toml"),
    },
    ThemeEntry {
        name: "gruvbox_light",
        toml: include_str!("themes/gruvbox_light.toml"),
    },
    ThemeEntry {
        name: "kanagawa",
        toml: include_str!("themes/kanagawa.toml"),
    },
    ThemeEntry {
        name: "kanagawa_ink",
        toml: include_str!("themes/kanagawa_ink.toml"),
    },
    ThemeEntry {
        name: "kanagawa_plum",
        toml: include_str!("themes/kanagawa_plum.toml"),
    },
    ThemeEntry {
        name: "material_darker",
        toml: include_str!("themes/material_darker.toml"),
    },
    ThemeEntry {
        name: "monokai_pro",
        toml: include_str!("themes/monokai_pro.toml"),
    },
    ThemeEntry {
        name: "night_owl",
        toml: include_str!("themes/night_owl.toml"),
    },
    ThemeEntry {
        name: "nightfox",
        toml: include_str!("themes/nightfox.toml"),
    },
    ThemeEntry {
        name: "nord",
        toml: include_str!("themes/nord.toml"),
    },
    ThemeEntry {
        name: "onedark",
        toml: include_str!("themes/onedark.toml"),
    },
    ThemeEntry {
        name: "opencode",
        toml: include_str!("themes/opencode.toml"),
    },
    ThemeEntry {
        name: "opencode_light",
        toml: include_str!("themes/opencode_light.toml"),
    },
    ThemeEntry {
        name: "rose_pine",
        toml: include_str!("themes/rose_pine.toml"),
    },
    ThemeEntry {
        name: "rose_pine_dawn",
        toml: include_str!("themes/rose_pine_dawn.toml"),
    },
    ThemeEntry {
        name: "rose_pine_midnight",
        toml: include_str!("themes/rose_pine_midnight.toml"),
    },
    ThemeEntry {
        name: "rose_pine_moon",
        toml: include_str!("themes/rose_pine_moon.toml"),
    },
    ThemeEntry {
        name: "solarized_dark",
        toml: include_str!("themes/solarized_dark.toml"),
    },
    ThemeEntry {
        name: "solarized_light",
        toml: include_str!("themes/solarized_light.toml"),
    },
    ThemeEntry {
        name: "tokyonight",
        toml: include_str!("themes/tokyonight.toml"),
    },
    ThemeEntry {
        name: "vscode_dark_plus",
        toml: include_str!("themes/vscode_dark_plus.toml"),
    },
    ThemeEntry {
        name: "zenburn",
        toml: include_str!("themes/zenburn.toml"),
    },
];

static THEME: LazyLock<ArcSwap<Theme>> =
    LazyLock::new(|| ArcSwap::from_pointee(Theme::load_or_bundled()));

static GENERATION: AtomicU64 = AtomicU64::new(0);

static CURRENT_NAME: Mutex<Option<String>> = Mutex::new(None);

pub fn current() -> Guard<Arc<Theme>> {
    THEME.load()
}

pub fn set(theme: Theme) {
    // Order matters: install colors before bumping the counter, otherwise a
    // reader could see the new generation but bake with the old palette.
    THEME.store(Arc::new(theme));
    crate::highlight::refresh_syntax_theme();
    GENERATION.fetch_add(1, Ordering::Release);
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

pub fn load_by_name(name: &str) -> Result<Theme, String> {
    if let Some(path) = user_themes_dir().map(|d| d.join(format!("{name}.toml")))
        && let Ok(toml) = std::fs::read_to_string(&path)
    {
        return Theme::from_toml(&toml).map_err(|e| format!("{}: {e}", path.display()));
    }
    BUNDLED_THEMES
        .iter()
        .find(|e| e.name == name)
        .map(|e| Theme::from_toml(e.toml))
        .unwrap_or_else(|| Err(format!("unknown theme: {name}")))
}

fn user_themes_dir() -> Option<PathBuf> {
    caudra_storage::paths::config_dir()
        .ok()
        .map(|d| d.join("themes"))
}

pub fn all_theme_names() -> Vec<String> {
    let user_names = user_themes_dir()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "toml" {
                return None;
            }
            Some(path.file_stem()?.to_str()?.to_owned())
        });
    merge_theme_names(user_names)
}

fn merge_theme_names(user_names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = BUNDLED_THEMES.iter().map(|e| e.name.to_owned()).collect();
    names.extend(user_names);
    names.sort_unstable();
    names.dedup();
    names
}

pub fn set_current_name(name: &str) {
    *CURRENT_NAME.lock().unwrap() = Some(name.to_owned());
}

pub fn persist_theme(name: &str) {
    set_current_name(name);
    if let Ok(dir) = StateDir::resolve() {
        caudra_storage::theme::persist_theme_name(&dir, name);
    }
}

fn read_theme_name() -> Option<String> {
    let dir = StateDir::resolve().ok()?;
    caudra_storage::theme::read_theme_name(&dir)
}

/// Memoized: the event loop asks every frame to notice an interactive pick,
/// and the persisted name must not be re-read from disk that often.
pub fn current_theme_name() -> String {
    let mut current = CURRENT_NAME.lock().unwrap();
    if let Some(name) = current.as_ref() {
        return name.clone();
    }
    let name = read_theme_name().unwrap_or_else(|| DEFAULT_THEME.to_owned());
    *current = Some(name.clone());
    name
}

pub fn style_by_name(name: &str) -> Style {
    let t = current();
    match name {
        "dim" | "tool_dim" => t.tool_dim,
        "path" | "tool_path" => t.tool_path,
        "tool" => t.tool,
        "tool_prefix" => t.tool_prefix,
        "tool_success" => t.tool_success,
        "tool_warning" => t.tool_warning,
        "tool_error" => t.tool_error,
        "tool_annotation" => t.tool_annotation,
        "spinner" => t.spinner,
        "error" => t.error,
        "bold" => t.bold,
        "italic" => t.italic,
        "bold_italic" => t.bold_italic,
        "inline_code" => t.inline_code,
        "math" => t.math,
        "diagram" => t.diagram,
        "strikethrough" => t.strikethrough,
        "heading" => t.heading,
        "list_marker" => t.list_marker,
        "horizontal_rule" => t.horizontal_rule,
        "code_gutter" => t.code_gutter,
        "table_border" => t.table_border,
        "keyword" | "index_keyword" => t.index_keyword,
        "section" | "index_section" => t.index_section,
        "line_nr" | "index_line_nr" => t.index_line_nr,
        "diff_old" => t.diff_old,
        "diff_new" => t.diff_new,
        "item" => t.item,
        "item_desc" => t.item_desc,
        "item_selected" | "selected" => t.item_selected,
        "item_match" | "match" => t.item_match,
        "item_match_selected" | "match_selected" => t.item_match_selected,
        "cursor" => t.cursor,
        "foreground" => Style::new().fg(t.foreground),
        "accent" => t.accent,
        "active" => t.active,
        "keybind_key" => t.keybind_key,
        "keybind_desc" => t.keybind_desc,
        "keybind_section" => t.keybind_section,
        "success" | "todo_completed" => t.todo_completed,
        "warning" | "todo_in_progress" => t.todo_in_progress,
        "todo_pending" | "pending" => t.todo_pending,
        "todo_cancelled" | "cancelled" => t.todo_cancelled,
        _ => Style::new().fg(t.foreground),
    }
}

#[derive(Debug)]
pub struct Theme {
    pub background: Color,
    pub foreground: Color,

    pub user: Style,
    pub assistant: Style,
    pub thinking: Style,
    pub tool_bg: Style,
    pub tool: Style,
    pub tool_path: Style,
    pub tool_annotation: Style,
    pub tool_prefix: Style,
    pub tool_success: Style,
    /// A call that worked and answered with nothing.
    pub tool_warning: Style,
    pub tool_error: Style,
    pub tool_dim: Style,
    pub error: Style,
    pub status_dim: Style,
    pub bold: Style,
    pub italic: Style,
    pub bold_italic: Style,
    pub inline_code: Style,
    pub math: Style,
    pub diagram: Style,
    pub code_block: Style,
    pub code_gutter: Style,
    pub strikethrough: Style,
    pub heading: Style,
    pub list_marker: Style,
    pub horizontal_rule: Style,
    pub plan_rule: Style,
    pub table_border: Style,
    pub diff_old: Style,
    pub diff_new: Style,
    pub diff_old_emphasis: Style,
    pub diff_new_emphasis: Style,
    pub diff_line_nr: Style,
    pub todo_completed: Style,
    pub todo_in_progress: Style,
    pub todo_pending: Style,
    pub todo_cancelled: Style,
    pub item_selected: Style,
    pub item: Style,
    pub item_desc: Style,
    pub item_match: Style,
    pub item_match_selected: Style,
    pub panel_border: Style,
    pub panel_title: Style,
    pub cursor: Style,
    pub input_border: Style,
    pub accent: Style,
    pub active: Style,
    pub keybind_key: Style,
    pub keybind_desc: Style,
    pub keybind_section: Style,
    pub mode_build: Color,
    pub mode_plan: Color,
    pub mode_bash: Color,
    pub queue: Style,
    pub plan_path: Style,
    pub status_notice: Style,
    pub status_retry_error: Style,
    pub status_retry_info: Style,
    pub input_placeholder: Style,
    pub timestamp: Style,
    pub spinner: Style,
    pub index_section: Style,
    pub index_line_nr: Style,
    pub index_keyword: Style,
    pub shell_prefix: Style,
    pub mention: Style,
    pub progress_bar: Style,

    pub syntax: syntect::highlighting::Theme,
}

#[derive(Deserialize)]
struct StyleDef {
    fg: Option<String>,
    bg: Option<String>,
    #[serde(default)]
    modifiers: Vec<String>,
}

fn helix_to_textmate_scope(key: &str) -> &str {
    for &(helix, tm) in HELIX_TO_TEXTMATE {
        if key == helix {
            return tm;
        }
    }
    key
}

fn parse_hex_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn parse_hex(s: &str) -> Option<Color> {
    let (r, g, b) = parse_hex_rgb(s)?;
    Some(Color::Rgb(r, g, b))
}

fn parse_syn_color(s: &str, palette: &HashMap<String, String>) -> Option<SynColor> {
    let resolved = if s.starts_with('#') {
        s
    } else {
        palette.get(s)?.as_str()
    };
    let (r, g, b) = parse_hex_rgb(resolved)?;
    Some(SynColor { r, g, b, a: 0xFF })
}

fn resolve_color(name: &str, palette: &HashMap<String, Color>) -> Option<Color> {
    if name.starts_with('#') {
        parse_hex(name)
    } else {
        palette.get(name).copied()
    }
}

fn resolve_modifier(name: &str) -> Modifier {
    match name {
        "bold" => Modifier::BOLD,
        "italic" => Modifier::ITALIC,
        "underlined" => Modifier::UNDERLINED,
        "crossed_out" => Modifier::CROSSED_OUT,
        "dim" => Modifier::DIM,
        "reversed" => Modifier::REVERSED,
        _ => Modifier::empty(),
    }
}

fn relative_luminance((r, g, b): (u8, u8, u8)) -> f32 {
    let channel = |c: u8| {
        let v = f32::from(c) / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Raises `style`'s foreground away from its backdrop until it clears `min`.
///
/// Themes routinely reuse one washed-out `comment` color for every secondary
/// role, which leaves text unreadable against the theme's own background. The
/// foreground is interpolated toward whichever of black or white contrasts more
/// with the backdrop, so hue is retained wherever the floor is already met.
fn ensure_contrast(style: Style, background: Color, min: f32) -> Style {
    let (Color::Rgb(fr, fg, fb), Color::Rgb(br, bg, bb)) = (
        style.fg.unwrap_or(Color::Reset),
        style.bg.unwrap_or(background),
    ) else {
        return style;
    };

    let (foreground, backdrop) = ((fr, fg, fb), (br, bg, bb));
    if contrast_ratio(foreground, backdrop) >= min {
        return style;
    }

    const WHITE: (u8, u8, u8) = (255, 255, 255);
    const BLACK: (u8, u8, u8) = (0, 0, 0);
    let target = if contrast_ratio(WHITE, backdrop) >= contrast_ratio(BLACK, backdrop) {
        WHITE
    } else {
        BLACK
    };

    // A mid-luminance backdrop can cap below the floor; take the best available.
    if contrast_ratio(target, backdrop) < min {
        return style.fg(Color::Rgb(target.0, target.1, target.2));
    }

    let blend = |t: f32| {
        (
            lerp_u8(foreground.0, target.0, t),
            lerp_u8(foreground.1, target.1, t),
            lerp_u8(foreground.2, target.2, t),
        )
    };

    // Contrast rises monotonically as the foreground approaches `target`, so the
    // least invasive passing blend is a bisection away.
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..CONTRAST_STEPS {
        let mid = f32::midpoint(lo, hi);
        if contrast_ratio(blend(mid), backdrop) >= min {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    let (r, g, b) = blend(hi);
    style.fg(Color::Rgb(r, g, b))
}

fn contrast_floor(role: &str) -> f32 {
    if CHROME_ROLES.contains(&role) {
        MIN_CONTRAST_CHROME
    } else {
        MIN_CONTRAST_TEXT
    }
}

/// How the characters a fuzzy search matched are picked out of the row they sit
/// in.
///
/// The accent is the first choice, but it is chosen to stand against the theme
/// background and nothing else, so on a selected row it lands on the selection
/// bar instead. Every bundled theme misses the text floor that way, and the two
/// opencode themes tint with the very colour the bar is painted in. Where the
/// accent cannot be read, the row keeps its own already-clamped foreground and
/// the match is carried by weight and an underline, which no palette can erase.
fn derive_match_style(base: Style, accent: Style, background: Color) -> Style {
    let tint = match (accent.fg, base.bg.unwrap_or(background)) {
        (Some(Color::Rgb(fr, fg, fb)), Color::Rgb(br, bg, bb))
            if contrast_ratio((fr, fg, fb), (br, bg, bb)) >= MIN_CONTRAST_TEXT =>
        {
            accent.fg
        }
        _ => None,
    };

    match tint {
        Some(fg) => base.fg(fg).add_modifier(Modifier::BOLD),
        None => base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    }
}

fn resolve_style(def: &StyleDef, palette: &HashMap<String, Color>) -> Style {
    let mut style = Style::new();
    if let Some(fg) = def.fg.as_ref().and_then(|n| resolve_color(n, palette)) {
        style = style.fg(fg);
    }
    if let Some(bg) = def.bg.as_ref().and_then(|n| resolve_color(n, palette)) {
        style = style.bg(bg);
    }
    for m in &def.modifiers {
        style = style.add_modifier(resolve_modifier(m));
    }
    style
}

fn scope_fg(
    full_table: &toml::Table,
    palette: &HashMap<String, Color>,
    raw_palette: &HashMap<String, String>,
    scope: &str,
) -> Option<Color> {
    let table = full_table.get(scope)?.as_table()?;
    let fg_val = table.get("fg")?.as_str()?;
    resolve_color(fg_val, palette).or_else(|| {
        let resolved = raw_palette.get(fg_val)?;
        parse_hex(resolved)
    })
}

fn resolve_font_style(modifiers: &[String]) -> FontStyle {
    let mut fs = FontStyle::empty();
    for m in modifiers {
        match m.as_str() {
            "bold" => fs |= FontStyle::BOLD,
            "italic" => fs |= FontStyle::ITALIC,
            "underlined" => fs |= FontStyle::UNDERLINE,
            _ => {}
        }
    }
    fs
}

fn style_def_to_syn(def: &StyleDef, raw_palette: &HashMap<String, String>) -> StyleModifier {
    let has_color = def.fg.is_some() || def.bg.is_some();
    StyleModifier {
        foreground: def
            .fg
            .as_ref()
            .and_then(|n| parse_syn_color(n, raw_palette)),
        background: def
            .bg
            .as_ref()
            .and_then(|n| parse_syn_color(n, raw_palette)),
        font_style: if def.modifiers.is_empty() {
            if has_color {
                Some(FontStyle::empty())
            } else {
                None
            }
        } else {
            Some(resolve_font_style(&def.modifiers))
        },
    }
}

fn build_syntax_theme(
    toml_table: &toml::Table,
    raw_palette: &HashMap<String, String>,
) -> syntect::highlighting::Theme {
    let fg = parse_syn_color("foreground", raw_palette);
    let bg = parse_syn_color("background", raw_palette);

    let settings = ThemeSettings {
        foreground: fg,
        background: bg,
        caret: fg,
        line_highlight: parse_syn_color("current_line", raw_palette)
            .or_else(|| parse_syn_color("selection", raw_palette)),
        selection: parse_syn_color("selection", raw_palette)
            .or_else(|| parse_syn_color("current_line", raw_palette)),
        ..Default::default()
    };

    let mut scopes = Vec::new();

    for (key, value) in toml_table {
        if RESERVED_KEYS.contains(&key.as_str()) || key.starts_with("ui.") {
            continue;
        }

        let Some(table) = value.as_table() else {
            continue;
        };

        let def: StyleDef = match toml::Value::Table(table.clone()).try_into() {
            Ok(d) => d,
            Err(_) => continue,
        };

        let tm_scope = helix_to_textmate_scope(key);

        let Ok(scope) = tm_scope.parse::<ScopeSelectors>() else {
            continue;
        };

        scopes.push(ThemeItem {
            scope,
            style: style_def_to_syn(&def, raw_palette),
        });
    }

    syntect::highlighting::Theme {
        name: None,
        author: None,
        settings,
        scopes,
    }
}

impl Theme {
    /// Base style for any surface that paints the theme background.
    ///
    /// Painting only `bg` leaves unstyled spans at `Color::Reset`, which resolves
    /// to the *terminal's* default foreground rather than the theme's, so a dark
    /// theme on a light terminal renders near-black text on a dark panel.
    pub(crate) fn surface_style(&self) -> Style {
        Style::new().fg(self.foreground).bg(self.background)
    }

    pub(crate) fn user_message_style(&self) -> Style {
        tinted_background(
            self.background,
            self.user.fg.unwrap_or(self.foreground),
            MESSAGE_BACKGROUND_TINT,
        )
    }

    pub(crate) fn panel_style(&self) -> Style {
        self.tool_bg.bg.map_or_else(
            || tinted_background(self.background, self.foreground, PANEL_BACKGROUND_TINT),
            |background| Style::new().bg(background),
        )
    }

    pub(crate) fn element_style(&self) -> Style {
        tinted_background(self.background, self.foreground, ELEMENT_BACKGROUND_TINT)
    }

    pub(crate) fn subtle_border_style(&self) -> Style {
        match (self.panel_border.fg, self.background) {
            (Some(Color::Rgb(fr, fg, fb)), Color::Rgb(br, bg, bb)) => Style::new().fg(Color::Rgb(
                lerp_u8(fr, br, 0.45),
                lerp_u8(fg, bg, 0.45),
                lerp_u8(fb, bb, 0.45),
            )),
            _ => self.panel_border,
        }
    }

    fn from_toml(toml_str: &str) -> Result<Self, String> {
        let full_table: toml::Table = toml::from_str(toml_str).map_err(|e| e.to_string())?;

        let raw_palette: HashMap<String, String> = full_table
            .get("palette")
            .and_then(|v| v.as_table())
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();

        let palette: HashMap<String, Color> = raw_palette
            .iter()
            .filter_map(|(k, v)| parse_hex(v).map(|c| (k.clone(), c)))
            .collect();

        let ui: HashMap<String, StyleDef> = full_table
            .get("ui")
            .and_then(|v| v.as_table())
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| {
                        let def: StyleDef = v.clone().try_into().ok()?;
                        Some((k.clone(), def))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let background = palette.get("background").copied().unwrap_or(Color::Reset);

        let style = |key: &str| -> Style {
            ui.get(key)
                .map(|d| {
                    ensure_contrast(resolve_style(d, &palette), background, contrast_floor(key))
                })
                .unwrap_or_default()
        };

        // A declared match role is patched onto the row it paints over before it
        // is clamped, so a theme that names only a foreground keeps the
        // selection bar and is measured against it rather than the background.
        let match_style = |ui_key: &str, base: Style| -> Style {
            match ui.get(ui_key) {
                Some(d) => ensure_contrast(
                    base.patch(resolve_style(d, &palette)),
                    background,
                    MIN_CONTRAST_TEXT,
                ),
                None => derive_match_style(base, style("accent"), background),
            }
        };

        let derived_color = |ui_key: &str, scopes: &[&str]| -> Color {
            if let Some(c) = palette.get(ui_key) {
                return *c;
            }
            for scope in scopes {
                if let Some(c) = scope_fg(&full_table, &palette, &raw_palette, scope) {
                    return c;
                }
            }
            Color::Reset
        };

        let derived_style = |ui_key: &str, scopes: &[&str], mods: Modifier| -> Style {
            let floor = contrast_floor(ui_key);
            if let Some(d) = ui.get(ui_key) {
                return ensure_contrast(resolve_style(d, &palette), background, floor);
            }
            for scope in scopes {
                if let Some(c) = scope_fg(&full_table, &palette, &raw_palette, scope) {
                    return ensure_contrast(
                        Style::new().fg(c).add_modifier(mods),
                        background,
                        floor,
                    );
                }
            }
            Style::default()
        };

        let syntax = build_syntax_theme(&full_table, &raw_palette);

        let color = |key: &str| -> Color { palette.get(key).copied().unwrap_or(Color::Reset) };

        let bold_style = derived_style(
            "bold",
            &["markup.bold", "variable.parameter"],
            Modifier::BOLD,
        );

        Ok(Self {
            background: color("background"),
            foreground: color("foreground"),

            user: style("user"),
            assistant: style("assistant"),
            thinking: ensure_contrast(
                brighten_toward(
                    style("thinking"),
                    color("comment"),
                    color("foreground"),
                    0.3,
                ),
                background,
                MIN_CONTRAST_TEXT,
            ),
            tool_bg: style("tool_bg"),
            tool: style("tool"),
            tool_path: style("tool_path"),
            tool_annotation: style("tool_annotation"),
            tool_prefix: style("tool_prefix"),
            tool_success: style("tool_success"),
            // Every bundled theme already names a yellow for a todo in
            // flight, and a warning is the same signal, so a theme only has
            // to say anything here to disagree.
            tool_warning: ui
                .get("tool_warning")
                .map(|d| ensure_contrast(resolve_style(d, &palette), background, MIN_CONTRAST_TEXT))
                .unwrap_or_else(|| style("todo_in_progress")),
            tool_error: style("tool_error"),
            tool_dim: style("tool_dim"),
            error: style("error"),
            status_dim: style("status_dim"),
            bold: bold_style,
            italic: ui
                .get("italic")
                .map(|d| ensure_contrast(resolve_style(d, &palette), background, MIN_CONTRAST_TEXT))
                .unwrap_or_else(|| Style::default().add_modifier(Modifier::ITALIC)),
            bold_italic: ui
                .get("bold_italic")
                .map(|d| ensure_contrast(resolve_style(d, &palette), background, MIN_CONTRAST_TEXT))
                .unwrap_or_else(|| bold_style.add_modifier(Modifier::ITALIC)),
            inline_code: derived_style(
                "inline_code",
                &["function.call", "function"],
                Modifier::empty(),
            ),
            math: derived_style(
                "math",
                &["constant.numeric", "constant", "function"],
                Modifier::empty(),
            ),
            diagram: derived_style(
                "diagram",
                &["comment", "variable.parameter", "string"],
                Modifier::empty(),
            ),
            code_block: style("code_block"),
            code_gutter: derived_style(
                "code_gutter",
                &["variable.parameter", "string"],
                Modifier::empty(),
            ),
            strikethrough: style("strikethrough"),
            heading: derived_style(
                "heading",
                &["keyword.storage.type", "keyword"],
                Modifier::BOLD,
            ),
            list_marker: derived_style(
                "list_marker",
                &["keyword.storage.type", "keyword"],
                Modifier::empty(),
            ),
            horizontal_rule: style("horizontal_rule"),
            plan_rule: style("plan_rule"),
            table_border: style("table_border"),
            diff_old: style("diff_old"),
            diff_new: style("diff_new"),
            diff_old_emphasis: style("diff_old_emphasis"),
            diff_new_emphasis: style("diff_new_emphasis"),
            diff_line_nr: style("diff_line_nr"),
            todo_completed: style("todo_completed"),
            todo_in_progress: style("todo_in_progress"),
            todo_pending: style("todo_pending"),
            todo_cancelled: style("todo_cancelled"),
            item_selected: style("item_selected"),
            item: style("item"),
            item_desc: style("item_desc"),
            item_match: match_style("item_match", style("item")),
            item_match_selected: match_style("item_match_selected", style("item_selected")),
            panel_border: style("panel_border"),
            panel_title: style("panel_title"),
            cursor: match ui.contains_key(CURSOR_KEY) {
                true => style(CURSOR_KEY),
                false => CURSOR_FALLBACK,
            },
            input_border: style("input_border"),
            accent: style("accent"),
            active: {
                let s = style("active");
                if s == Style::default() {
                    style("accent")
                } else {
                    s
                }
            },
            keybind_key: style("keybind_key"),
            keybind_desc: style("keybind_desc"),
            keybind_section: style("keybind_section"),
            mode_build: derived_color("mode_build", &["keyword.storage.type", "keyword"]),
            mode_plan: derived_color("mode_plan", &["keyword", "keyword.storage.type"]),
            mode_bash: derived_color("mode_bash", &["function.builtin", "function"]),
            queue: style("queue"),
            plan_path: style("plan_path"),
            status_notice: style("status_notice"),
            status_retry_error: style("status_retry_error"),
            status_retry_info: style("status_retry_info"),
            input_placeholder: style("input_placeholder"),
            timestamp: style("timestamp"),
            spinner: style("spinner"),
            index_section: derived_style(
                "index_section",
                &["keyword.storage.type", "keyword"],
                Modifier::BOLD,
            ),
            index_line_nr: derived_style("index_line_nr", &["comment"], Modifier::empty()),
            index_keyword: derived_style("index_keyword", &["keyword"], Modifier::empty()),
            shell_prefix: derived_style("shell_prefix", &["string"], Modifier::BOLD),
            mention: derived_style(
                "mention",
                &["markup.link.url", "string.special.path", "string"],
                Modifier::UNDERLINED,
            ),
            progress_bar: {
                let s = style("progress_bar");
                if s == Style::default() {
                    style("accent")
                } else {
                    s
                }
            },
            syntax,
        })
    }

    fn load_or_bundled() -> Self {
        if let Some(name) = read_theme_name()
            && let Ok(theme) = load_by_name(&name)
        {
            return theme;
        }
        load_by_name(DEFAULT_THEME).expect("default theme must parse")
    }
}

pub(crate) fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)) as u8
}

fn tinted_background(background: Color, tint: Color, factor: f32) -> Style {
    match (background, tint) {
        (Color::Rgb(br, bg, bb), Color::Rgb(tr, tg, tb)) => Style::new().bg(Color::Rgb(
            lerp_u8(br, tr, factor),
            lerp_u8(bg, tg, factor),
            lerp_u8(bb, tb, factor),
        )),
        _ => Style::new().bg(background),
    }
}

pub(crate) fn dim_style(style: Style, factor: f32) -> Style {
    let background = current().background;
    let dimmed = match (style.fg, background) {
        (Some(Color::Rgb(fr, fg, fb)), Color::Rgb(br, bg, bb)) => style.fg(Color::Rgb(
            lerp_u8(fr, br, factor),
            lerp_u8(fg, bg, factor),
            lerp_u8(fb, bb, factor),
        )),
        _ => style,
    };
    ensure_contrast(dimmed, background, MIN_CONTRAST_TEXT)
}

fn brighten_toward(style: Style, from: Color, to: Color, t: f32) -> Style {
    match (from, to) {
        (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) => style.fg(Color::Rgb(
            lerp_u8(fr, tr, t),
            lerp_u8(fg, tg, t),
            lerp_u8(fb, tb, t),
        )),
        _ => style,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn dracula_toml() -> &'static str {
        BUNDLED_THEMES
            .iter()
            .find(|e| e.name == "dracula")
            .expect("dracula theme must exist")
            .toml
    }

    fn dracula() -> Theme {
        Theme::from_toml(dracula_toml()).unwrap()
    }

    fn bundled(name: &str) -> Theme {
        let entry = BUNDLED_THEMES
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("theme '{name}' must exist"));
        Theme::from_toml(entry.toml).unwrap_or_else(|e| panic!("theme '{name}' must parse: {e}"))
    }

    #[test]
    fn dracula_theme_fields() {
        let t = dracula();
        assert_eq!(t.background, Color::Rgb(0x28, 0x2a, 0x36));
        assert_eq!(t.foreground, Color::Rgb(0xf8, 0xf8, 0xf2));
        assert_eq!(t.user.fg, Some(Color::Rgb(0x8b, 0xe9, 0xfd)));
        assert_eq!(t.error.fg, Some(Color::Rgb(0xff, 0x55, 0x55)));
        assert!(t.bold.add_modifier.contains(Modifier::BOLD));
        assert!(t.thinking.add_modifier.contains(Modifier::ITALIC));
        assert!(t.strikethrough.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(t.diff_old.bg, Some(Color::Rgb(0x4D, 0x1F, 0x1F)));
        assert_eq!(t.diff_new.bg, Some(Color::Rgb(0x1F, 0x3D, 0x1F)));
        assert_eq!(t.input_border.fg, Some(Color::Rgb(0x62, 0x72, 0xa4)));
        assert_eq!(
            t.user_message_style().bg,
            Some(Color::Rgb(0x2f, 0x39, 0x45))
        );
        assert_eq!(t.panel_style().bg, Some(Color::Rgb(0x22, 0x24, 0x30)));
        assert_eq!(t.element_style().bg, Some(Color::Rgb(0x3c, 0x3e, 0x48)));
    }

    #[test]
    fn dracula_derivations() {
        let t = dracula();
        assert_eq!(t.mode_build, Color::Rgb(0x8b, 0xe9, 0xfd));
        assert_eq!(t.mode_plan, Color::Rgb(0xff, 0x79, 0xc6));
        assert_eq!(t.heading.fg, Some(Color::Rgb(0x8b, 0xe9, 0xfd)));
        assert!(t.heading.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.inline_code.fg, Some(Color::Rgb(0x50, 0xfa, 0x7b)));
        assert_eq!(t.code_gutter.fg, Some(Color::Rgb(0xff, 0xb8, 0x6c)));
        assert_eq!(t.list_marker.fg, Some(Color::Rgb(0x8b, 0xe9, 0xfd)));
        assert_eq!(t.bold.fg, Some(Color::Rgb(0xff, 0xb8, 0x6c)));
    }

    #[test]
    fn dracula_syntax_scopes() {
        let t = dracula();
        assert!(!t.syntax.scopes.is_empty());
        assert!(t.syntax.settings.foreground.is_some());
        assert!(t.syntax.settings.background.is_some());
    }

    fn bundled_palette(name: &str) -> HashMap<String, Color> {
        let entry = BUNDLED_THEMES
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("theme '{name}' must exist"));
        theme_tables(entry.toml).0
    }

    /// Anchors the port against opencode's published defs so a stray edit to
    /// either file shows up as a failure rather than a slightly-off hue.
    ///
    /// Reads `[palette]` rather than the constructed `Theme`, because
    /// `ensure_contrast` lifts any role that misses its floor. Asserting the
    /// built styles would pin this test to our contrast floor instead of to
    /// opencode's values: `opencode_light` publishes `accent` at 2.75:1 on
    /// white, so every role drawn from it renders slightly darker than the def.
    #[test_case(
        "opencode",
        &[
            ("background", 0x0a, 0x0a, 0x0a),
            ("foreground", 0xee, 0xee, 0xee),
            ("comment", 0x80, 0x80, 0x80),
            ("primary", 0xfa, 0xb2, 0x83),
            ("secondary", 0x5c, 0x9c, 0xf5),
            ("accent", 0x9d, 0x7c, 0xd8),
            ("red", 0xe0, 0x6c, 0x75),
            ("green", 0x7f, 0xd8, 0x8f),
            ("panel", 0x14, 0x14, 0x14),
        ];
        "dark"
    )]
    // opencode swaps the two hues between branches: primary is blue here.
    #[test_case(
        "opencode_light",
        &[
            ("background", 0xff, 0xff, 0xff),
            ("foreground", 0x1a, 0x1a, 0x1a),
            ("comment", 0x8a, 0x8a, 0x8a),
            ("primary", 0x3b, 0x7d, 0xd8),
            ("secondary", 0x7b, 0x5b, 0xb6),
            ("accent", 0xd6, 0x8c, 0x27),
            ("red", 0xd1, 0x38, 0x3d),
            ("green", 0x3d, 0x9a, 0x57),
            ("panel", 0xfa, 0xfa, 0xfa),
        ];
        "light"
    )]
    fn opencode_palette_matches_upstream_defs(name: &str, expected: &[(&str, u8, u8, u8)]) {
        let palette = bundled_palette(name);
        for (key, r, g, b) in expected {
            assert_eq!(
                palette.get(*key),
                Some(&Color::Rgb(*r, *g, *b)),
                "{name}: palette.{key} drifted from opencode's def",
            );
        }
    }

    /// Background roles never pass through `ensure_contrast`, so these stay on
    /// the constructed theme where they also cover the wiring.
    #[test_case("opencode", 0x0a, 0x0a, 0x0a, 0x14, 0x14, 0x14; "dark")]
    #[test_case("opencode_light", 0xff, 0xff, 0xff, 0xfa, 0xfa, 0xfa; "light")]
    fn opencode_panel_lands_on_background_panel(
        name: &str,
        br: u8,
        bg: u8,
        bb: u8,
        pr: u8,
        pg: u8,
        pb: u8,
    ) {
        let t = bundled(name);
        assert_eq!(t.background, Color::Rgb(br, bg, bb));
        // tool_bg is backgroundPanel, so panels land on it exactly instead of
        // falling back to a tint of the background.
        assert_eq!(t.panel_style().bg, Some(Color::Rgb(pr, pg, pb)));
    }

    #[test]
    fn opencode_diff_backgrounds_match_upstream_defs() {
        let t = bundled("opencode");
        assert_eq!(t.diff_old.bg, Some(Color::Rgb(0x37, 0x22, 0x2c)));
        assert_eq!(t.diff_new.bg, Some(Color::Rgb(0x20, 0x30, 0x3b)));
    }

    /// A pair naming a theme that does not exist would silently stop the
    /// terminal from being followed, so both halves must be bundled.
    #[test]
    fn theme_pairs_name_bundled_themes() {
        for pair in THEME_PAIRS {
            for name in [pair.dark, pair.light] {
                assert!(
                    BUNDLED_THEMES.iter().any(|e| e.name == name),
                    "theme pair names '{name}', which is not bundled",
                );
            }
        }
    }

    /// Overlapping pairs would make `pair_for` depend on table order.
    #[test]
    fn theme_pairs_do_not_overlap() {
        let mut seen = Vec::new();
        for pair in THEME_PAIRS {
            for name in [pair.dark, pair.light] {
                assert!(!seen.contains(&name), "'{name}' appears in two pairs");
                seen.push(name);
            }
        }
    }

    /// The default has to follow the terminal without any configuration.
    #[test]
    fn default_theme_is_paired() {
        let pair = pair_for(DEFAULT_THEME).expect("the default theme must have a light half");
        assert_eq!(pair.dark, DEFAULT_THEME);
    }

    #[test_case("opencode", "opencode", "opencode_light"; "dark_half_finds_pair")]
    #[test_case("opencode_light", "opencode", "opencode_light"; "light_half_finds_pair")]
    #[test_case("gruvbox_light", "gruvbox", "gruvbox_light"; "light_only_name")]
    fn pair_for_resolves_either_half(name: &str, dark: &str, light: &str) {
        let pair = pair_for(name).unwrap_or_else(|| panic!("'{name}' must resolve to a pair"));
        assert_eq!(pair.dark, dark);
        assert_eq!(pair.light, light);
    }

    #[test_case("dracula"; "unpaired_dark_theme")]
    #[test_case("ayu_mirage"; "dark_variant_outside_the_canonical_pair")]
    #[test_case("nonexistent"; "unknown_name")]
    fn pair_for_returns_none_for_unpaired(name: &str) {
        assert!(pair_for(name).is_none());
    }

    #[test_case("opencode", Color::Rgb(0xfa, 0xb2, 0x83), Color::Rgb(0x9d, 0x7c, 0xd8), Color::Rgb(0x5c, 0x9c, 0xf5); "dark")]
    #[test_case("opencode_light", Color::Rgb(0x3b, 0x7d, 0xd8), Color::Rgb(0xd6, 0x8c, 0x27), Color::Rgb(0x7b, 0x5b, 0xb6); "light")]
    fn opencode_mode_colors_come_from_palette(name: &str, build: Color, plan: Color, bash: Color) {
        let t = bundled(name);
        assert_eq!(t.mode_build, build);
        assert_eq!(t.mode_plan, plan);
        assert_eq!(t.mode_bash, bash);
    }

    const COMMENT_COLOR: SynColor = SynColor {
        r: 0x62,
        g: 0x72,
        b: 0xa4,
        a: 0xFF,
    };
    const STRING_COLOR: SynColor = SynColor {
        r: 0xf1,
        g: 0xfa,
        b: 0x8c,
        a: 0xFF,
    };
    const PINK_COLOR: SynColor = SynColor {
        r: 0xff,
        g: 0x79,
        b: 0xc6,
        a: 0xFF,
    };
    const CYAN_COLOR: SynColor = SynColor {
        r: 0x8b,
        g: 0xe9,
        b: 0xfd,
        a: 0xFF,
    };

    fn resolve_color_for_scope(
        theme: &syntect::highlighting::Theme,
        scope_str: &str,
    ) -> Option<SynColor> {
        use syntect::parsing::ScopeStack;

        let stack: ScopeStack = scope_str.parse().unwrap();
        let mut best_item: Option<&ThemeItem> = None;
        let mut best_score: f64 = 0.0;
        for item in &theme.scopes {
            if let Some(score) = item.scope.does_match(stack.as_slice())
                && score.0 > best_score
            {
                best_score = score.0;
                best_item = Some(item);
            }
        }
        best_item.and_then(|item| item.style.foreground)
    }

    #[test]
    fn scope_resolution_maps_helix_to_textmate() {
        let t = dracula();
        let cases: &[(&str, SynColor)] = &[
            (
                "source.rust comment.line.double-slash.rust punctuation.definition.comment.rust",
                COMMENT_COLOR,
            ),
            ("source.rust comment.line.double-slash.rust", COMMENT_COLOR),
            (
                "source.rust string.quoted.double.rust punctuation.definition.string.begin.rust",
                STRING_COLOR,
            ),
            ("source.rust meta.generic.rust", CYAN_COLOR),
            (
                "source.rust meta.path.rust punctuation.accessor.rust",
                PINK_COLOR,
            ),
        ];
        for (scope, expected) in cases {
            assert_eq!(
                resolve_color_for_scope(&t.syntax, scope),
                Some(*expected),
                "scope {scope} should resolve correctly"
            );
        }
    }

    #[test]
    fn missing_ui_key_defaults_to_empty_style() {
        let toml = r#"
[palette]
[ui]
"#;
        let theme = Theme::from_toml(toml).unwrap();
        assert_eq!(theme.user, Style::default());
    }

    const CARET_HIDDEN: &str = "a theme that names no cursor must still draw a visible caret";

    #[test]
    fn a_theme_without_a_cursor_still_shows_the_caret() {
        let toml = r#"
[palette]
[ui]
"#;
        let theme = Theme::from_toml(toml).unwrap();
        assert_eq!(theme.cursor, CURSOR_FALLBACK, "{CARET_HIDDEN}");
    }

    #[test]
    fn invalid_toml_returns_error() {
        assert!(Theme::from_toml("not valid {{{{").is_err());
    }

    #[test]
    fn all_bundled_themes_parse() {
        for entry in BUNDLED_THEMES {
            let result = Theme::from_toml(entry.toml);
            assert!(
                result.is_ok(),
                "theme '{}' failed to parse: {}",
                entry.name,
                result.unwrap_err()
            );
        }
    }

    #[test]
    fn load_by_name_unknown() {
        assert!(load_by_name("nonexistent").is_err());
    }

    const DIFF_BANDS: &str = "a diff row is a band of background the syntax colours show through, so both shades must be backgrounds";
    const DIFF_SHADES: &str =
        "the characters that changed must be a stronger shade than the line they sit on";

    /// A diff paints the whole changed row in one shade and the characters that
    /// changed in another, over the top. Two shades that match leave an edit
    /// looking like a whole line rewritten, which is the thing the emphasis
    /// exists to disprove.
    #[test]
    fn every_theme_separates_a_changed_line_from_the_characters_that_changed() {
        for entry in BUNDLED_THEMES {
            let theme = Theme::from_toml(entry.toml).expect("theme must parse");
            for (line, emphasis, role) in [
                (theme.diff_old, theme.diff_old_emphasis, "diff_old"),
                (theme.diff_new, theme.diff_new_emphasis, "diff_new"),
            ] {
                let (line, emphasis) = (
                    line.bg
                        .unwrap_or_else(|| panic!("{}: {role} {DIFF_BANDS}", entry.name)),
                    emphasis
                        .bg
                        .unwrap_or_else(|| panic!("{}: {role}_emphasis {DIFF_BANDS}", entry.name)),
                );
                assert_ne!(
                    rgb(line),
                    rgb(emphasis),
                    "{}: {role} {DIFF_SHADES}",
                    entry.name
                );
            }
        }
    }

    fn theme_tables(toml_str: &str) -> (HashMap<String, Color>, toml::Table) {
        let table: toml::Table = toml::from_str(toml_str).expect("theme must parse");
        let palette = table
            .get("palette")
            .and_then(|v| v.as_table())
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| v.as_str().and_then(parse_hex).map(|c| (k.clone(), c)))
                    .collect()
            })
            .unwrap_or_default();
        let ui = table
            .get("ui")
            .and_then(|v| v.as_table())
            .cloned()
            .unwrap_or_default();
        (palette, ui)
    }

    fn rgb(color: Color) -> (u8, u8, u8) {
        match color {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("expected an rgb color, got {other:?}"),
        }
    }

    /// Every `[ui]` role must clear its contrast floor against its own backdrop.
    /// Themes habitually reuse one washed-out `comment` color for all secondary
    /// roles, which is unreadable without the clamp in `ensure_contrast`.
    #[test]
    fn bundled_themes_meet_contrast_floor() {
        for entry in BUNDLED_THEMES {
            let (palette, ui) = theme_tables(entry.toml);
            let background = *palette
                .get("background")
                .unwrap_or_else(|| panic!("{} must define a background", entry.name));

            for (key, value) in &ui {
                let def: StyleDef = value
                    .clone()
                    .try_into()
                    .unwrap_or_else(|e| panic!("{}: [ui].{key} is malformed: {e}", entry.name));
                let floor = contrast_floor(key);
                let style = ensure_contrast(resolve_style(&def, &palette), background, floor);
                let Some(fg) = style.fg else { continue };
                let ratio = contrast_ratio(rgb(fg), rgb(style.bg.unwrap_or(background)));
                assert!(
                    ratio >= floor,
                    "{}: [ui].{key} contrast {ratio:.2} is below its {floor:.1} floor",
                    entry.name,
                );
            }
        }
    }

    const MATCH_UNREADABLE: &str = "matched characters must clear the text contrast floor";
    const MATCH_INDISTINCT: &str = "matched characters must not be painted as the row around them";

    /// Matched characters are painted over the row they sit in, so the selected
    /// variant is measured against the selection bar and not the background.
    /// Every bundled theme fails that when the accent is used unconditionally,
    /// and the opencode pair tints with the bar's own colour.
    #[test]
    fn bundled_themes_match_highlight_is_readable() {
        for entry in BUNDLED_THEMES {
            let t = bundled(entry.name);
            for (role, style) in [
                ("item_match", t.item_match),
                ("item_match_selected", t.item_match_selected),
            ] {
                let fg = rgb(style
                    .fg
                    .unwrap_or_else(|| panic!("{}: {role} must set fg", entry.name)));
                let ratio = contrast_ratio(fg, rgb(style.bg.unwrap_or(t.background)));
                assert!(
                    ratio >= MIN_CONTRAST_TEXT,
                    "{}: {role} contrast {ratio:.2} is below {MIN_CONTRAST_TEXT:.1}: {MATCH_UNREADABLE}",
                    entry.name,
                );
            }
        }
    }

    /// Readable is not enough: a match painted in the row's own colour with no
    /// weight of its own says nothing about which characters were typed.
    #[test]
    fn bundled_themes_match_highlight_is_distinguishable() {
        for entry in BUNDLED_THEMES {
            let t = bundled(entry.name);
            for (role, matched, base) in [
                ("item_match", t.item_match, t.item),
                (
                    "item_match_selected",
                    t.item_match_selected,
                    t.item_selected,
                ),
            ] {
                assert!(
                    matched.fg != base.fg || matched.add_modifier != base.add_modifier,
                    "{}: {role} is identical to the row it paints over: {MATCH_INDISTINCT}",
                    entry.name,
                );
            }
        }
    }

    /// Both opencode themes name one colour for `accent` and for the selection
    /// bar, so the tint is exactly the bar it lands on and has to be given up.
    #[test_case("opencode"; "dark")]
    #[test_case("opencode_light"; "light")]
    fn colliding_accent_falls_back_to_modifiers(name: &str) {
        let t = bundled(name);
        assert_eq!(t.item_match_selected.fg, t.item_selected.fg);
        assert!(
            t.item_match_selected
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED),
            "{name}: {MATCH_INDISTINCT}",
        );
    }

    /// A theme that names a match role gets it, foreground only, over the row's
    /// own background rather than over the terminal's.
    #[test]
    fn declared_match_role_overrides_derivation() {
        const DECLARED: &str = r##"
            [palette]
            background = "#000000"
            foreground = "#ffffff"
            bar = "#3b7dd8"
            lime = "#c0ff00"

            [ui]
            item_selected = { fg = "background", bg = "bar" }
            item_match_selected = { fg = "lime" }
        "##;

        let t = Theme::from_toml(DECLARED).expect("theme must parse");
        assert_eq!(t.item_match_selected.fg, Some(Color::Rgb(0xc0, 0xff, 0x00)));
        assert_eq!(t.item_match_selected.bg, Some(Color::Rgb(0x3b, 0x7d, 0xd8)));
    }

    /// A `[ui]` entry naming a missing palette key is silently dropped by
    /// `resolve_style`, leaving the role unstyled and falling back to the
    /// terminal's own colors. Catppuccin shipped `accent = { fg = "peach" }`
    /// against a palette that only had `orange`.
    #[test]
    fn bundled_theme_palette_references_resolve() {
        for entry in BUNDLED_THEMES {
            let (palette, ui) = theme_tables(entry.toml);
            for (key, value) in &ui {
                let def: StyleDef = value
                    .clone()
                    .try_into()
                    .expect("style def must deserialize");
                for (slot, name) in [("fg", &def.fg), ("bg", &def.bg)] {
                    let Some(name) = name else { continue };
                    assert!(
                        resolve_color(name, &palette).is_some(),
                        "{}: [ui].{key}.{slot} = \"{name}\" resolves to nothing",
                        entry.name,
                    );
                }
            }
        }
    }

    #[test_case("ayu_light"; "light_theme_lifts_washed_out_roles")]
    #[test_case("catppuccin_latte"; "light_theme_with_low_contrast_palette")]
    #[test_case("material_darker"; "dark_theme_with_dimmest_comment")]
    #[test_case("solarized_light"; "light_theme_with_invisible_borders")]
    fn secondary_roles_are_readable(name: &str) {
        let theme = bundled(name);
        let background = rgb(theme.background);
        for (role, style) in [
            ("item_desc", theme.item_desc),
            ("input_placeholder", theme.input_placeholder),
            ("tool_dim", theme.tool_dim),
            ("timestamp", theme.timestamp),
            ("status_dim", theme.status_dim),
            ("thinking", theme.thinking),
            ("item", theme.item),
        ] {
            let fg = rgb(style
                .fg
                .unwrap_or_else(|| panic!("{name}: {role} must set fg")));
            let ratio = contrast_ratio(fg, background);
            assert!(
                ratio >= MIN_CONTRAST_TEXT,
                "{name}: {role} contrast {ratio:.2} is below {MIN_CONTRAST_TEXT:.1}",
            );
        }
    }

    const OUTCOME_MSG: &str = "an outcome colour that repeats another says nothing";

    /// No theme defines `tool_warning`, so all of them reach it through the
    /// yellow they already name for a todo in flight. That only works if the
    /// yellow is actually apart from the other two outcomes, which is not
    /// true by construction: `zenburn` greens its success with `yellow_green`
    /// and `dark_daltonized` drops red altogether.
    #[test]
    fn every_theme_tells_its_three_outcomes_apart() {
        for entry in BUNDLED_THEMES {
            let theme = bundled(entry.name);
            let outcomes = [
                ("tool_success", theme.tool_success),
                ("tool_warning", theme.tool_warning),
                ("tool_error", theme.tool_error),
            ];
            for (i, (role, style)) in outcomes.iter().enumerate() {
                let fg = style
                    .fg
                    .unwrap_or_else(|| panic!("{}: {role} must set fg", entry.name));
                for (other_role, other) in &outcomes[i + 1..] {
                    assert_ne!(
                        fg,
                        other.fg.unwrap(),
                        "{}: {role} and {other_role} are the same colour. {OUTCOME_MSG}",
                        entry.name,
                    );
                }
            }
        }
    }

    #[test]
    fn catppuccin_accent_resolves() {
        for name in [
            "catppuccin_latte",
            "catppuccin_frappe",
            "catppuccin_macchiato",
            "catppuccin_mocha",
        ] {
            assert!(
                bundled(name).accent.fg.is_some(),
                "{name}: accent must resolve or pickers lose their highlight color",
            );
        }
    }

    #[test]
    fn default_theme_is_bundled() {
        assert!(
            BUNDLED_THEMES.iter().any(|e| e.name == DEFAULT_THEME),
            "load_or_bundled unwraps DEFAULT_THEME, so it must exist",
        );
    }

    #[test_case(0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 21.0; "white_on_black_is_maximum")]
    #[test_case(0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 1.0; "identical_colors_have_no_contrast")]
    fn contrast_ratio_matches_wcag(fr: u8, fg: u8, fb: u8, br: u8, bg: u8, bb: u8, expected: f32) {
        let ratio = contrast_ratio((fr, fg, fb), (br, bg, bb));
        assert!(
            (ratio - expected).abs() < 0.01,
            "expected {expected}, got {ratio}",
        );
    }

    #[test]
    fn ensure_contrast_leaves_passing_styles_untouched() {
        let background = Color::Rgb(0x28, 0x2a, 0x36);
        let style = Style::new().fg(Color::Rgb(0xf8, 0xf8, 0xf2));
        assert_eq!(ensure_contrast(style, background, MIN_CONTRAST_TEXT), style);
    }

    #[test]
    fn ensure_contrast_measures_against_own_background() {
        // fg/bg are nearly identical, so the role fails despite the global
        // background being far away from both.
        let style = Style::new()
            .fg(Color::Rgb(0x30, 0x30, 0x30))
            .bg(Color::Rgb(0x35, 0x35, 0x35));
        let lifted = ensure_contrast(style, Color::Rgb(0xff, 0xff, 0xff), MIN_CONTRAST_TEXT);
        assert_ne!(lifted.fg, style.fg);
        assert_eq!(lifted.bg, style.bg);
        let ratio = contrast_ratio(rgb(lifted.fg.unwrap()), rgb(style.bg.unwrap()));
        assert!(ratio >= MIN_CONTRAST_TEXT, "got {ratio:.2}");
    }

    #[test]
    fn ensure_contrast_ignores_unset_foreground() {
        let style = Style::new().bg(Color::Rgb(0x28, 0x2a, 0x36));
        assert_eq!(
            ensure_contrast(style, Color::Rgb(0x28, 0x2a, 0x36), MIN_CONTRAST_TEXT),
            style,
        );
    }

    #[test]
    fn chrome_roles_use_the_lower_floor() {
        assert_eq!(contrast_floor("panel_border"), MIN_CONTRAST_CHROME);
        assert_eq!(contrast_floor("item_desc"), MIN_CONTRAST_TEXT);
    }

    #[test]
    fn merge_theme_names_dedups_user_override_and_sorts_in_custom() {
        let names = merge_theme_names(["dracula".to_owned(), "aaa_custom".to_owned()]);
        assert_eq!(names.iter().filter(|n| *n == "dracula").count(), 1);
        assert_eq!(names.first().map(String::as_str), Some("aaa_custom"));
    }

    #[test]
    fn current_theme_name_prefers_in_memory_name() {
        set_current_name("zenburn");
        assert_eq!(current_theme_name(), "zenburn");
    }

    #[test]
    fn helix_theme_loads_without_ui_section() {
        let toml = r##"
"keyword" = { fg = "pink" }
"string" = { fg = "yellow" }
"comment" = { fg = "comment" }

[palette]
foreground = "#f8f8f2"
background = "#282a36"
pink = "#ff79c6"
yellow = "#f1fa8c"
comment = "#6272a4"
"##;
        let theme = Theme::from_toml(toml).unwrap();
        assert!(!theme.syntax.scopes.is_empty());
        assert_eq!(theme.background, Color::Rgb(0x28, 0x2a, 0x36));
    }

    #[test]
    fn ui_override_takes_precedence_over_derivation() {
        let toml = r##"
"keyword.storage.type" = { fg = "cyan" }
"keyword" = { fg = "pink" }
"function.call" = { fg = "green" }

[palette]
foreground = "#f8f8f2"
background = "#282a36"
cyan = "#8be9fd"
pink = "#ff79c6"
green = "#50fa7b"
custom = "#aabbcc"

[ui]
heading = { fg = "custom", modifiers = ["bold"] }
"##;
        let theme = Theme::from_toml(toml).unwrap();
        assert_eq!(theme.heading.fg, Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(theme.mode_build, Color::Rgb(0x8b, 0xe9, 0xfd));
    }

    #[test]
    fn derivation_without_ui_section() {
        let toml = r##"
"keyword.storage.type" = { fg = "#8be9fd" }
"keyword" = { fg = "#ff79c6" }
"constant" = { fg = "#bd93f9" }
"function.call" = { fg = "#50fa7b" }
"variable.parameter" = { fg = "#ffb86c" }
"markup.bold" = { fg = "#ffb86c" }

[palette]
foreground = "#f8f8f2"
background = "#282a36"
"##;
        let theme = Theme::from_toml(toml).unwrap();
        assert_eq!(theme.mode_build, Color::Rgb(0x8b, 0xe9, 0xfd));
        assert_eq!(theme.mode_plan, Color::Rgb(0xff, 0x79, 0xc6));
        assert_eq!(theme.heading.fg, Some(Color::Rgb(0x8b, 0xe9, 0xfd)));
        assert!(theme.heading.add_modifier.contains(Modifier::BOLD));
        assert_eq!(theme.inline_code.fg, Some(Color::Rgb(0x50, 0xfa, 0x7b)));
        assert_eq!(theme.code_gutter.fg, Some(Color::Rgb(0xff, 0xb8, 0x6c)));
    }

    #[test]
    fn palette_override_takes_precedence_for_color() {
        let toml = r##"
"keyword.storage.type" = { fg = "#8be9fd" }

[palette]
foreground = "#f8f8f2"
background = "#282a36"
mode_build = "#112233"
"##;
        let theme = Theme::from_toml(toml).unwrap();
        assert_eq!(theme.mode_build, Color::Rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn style_by_name_resolves() {
        set(dracula());
        let t = current();
        assert_eq!(style_by_name("dim"), t.tool_dim);
        assert_eq!(style_by_name("tool_dim"), t.tool_dim);
        assert_eq!(style_by_name("path"), t.tool_path);
        assert_eq!(style_by_name("tool_path"), t.tool_path);
        assert_eq!(style_by_name("keyword"), t.index_keyword);
        assert_eq!(style_by_name("index_keyword"), t.index_keyword);
        assert_eq!(style_by_name("section"), t.index_section);
        assert_eq!(style_by_name("index_section"), t.index_section);
        assert_eq!(style_by_name("line_nr"), t.index_line_nr);
        assert_eq!(style_by_name("index_line_nr"), t.index_line_nr);
        assert_eq!(style_by_name("tool"), t.tool);
        assert_eq!(style_by_name("error"), t.error);
        assert_eq!(style_by_name("bold"), t.bold);
        assert_eq!(style_by_name("italic"), t.italic);
        assert_eq!(style_by_name("bold_italic"), t.bold_italic);
        assert_eq!(style_by_name("diff_old"), t.diff_old);
        assert_eq!(style_by_name("diff_new"), t.diff_new);
        assert_eq!(style_by_name("item_selected"), t.item_selected);
        assert_eq!(style_by_name("item"), t.item);
        assert_eq!(style_by_name("item_desc"), t.item_desc);
        assert_eq!(style_by_name("cursor"), t.cursor);
        assert_eq!(style_by_name("accent"), t.accent);
        assert_eq!(style_by_name("active"), t.active);
        assert_eq!(style_by_name("foreground"), Style::new().fg(t.foreground));
        assert_eq!(style_by_name("keybind_key"), t.keybind_key);
        assert_eq!(style_by_name("keybind_desc"), t.keybind_desc);
        assert_eq!(style_by_name("keybind_section"), t.keybind_section);
        assert_eq!(style_by_name("selected"), t.item_selected);
        assert_eq!(style_by_name("success"), t.todo_completed);
        assert_eq!(style_by_name("warning"), t.todo_in_progress);
        assert_eq!(style_by_name("match"), t.item_match);
        assert_eq!(style_by_name("match_selected"), t.item_match_selected);
    }

    /// Plugins pass `""` for plain text (see the question form), so the
    /// fallback has to name the theme's foreground. Leaving it unset resolves
    /// to the terminal's default color over a themed background.
    #[test_case("nonexistent_style")]
    #[test_case("")]
    #[test_case("typo_keyword")]
    fn style_by_name_unknown_uses_theme_foreground(name: &str) {
        set(dracula());
        assert_eq!(style_by_name(name), Style::new().fg(current().foreground));
    }

    const DRACULA_BG: Color = Color::Rgb(0x28, 0x2a, 0x36);
    const TOKYONIGHT_BG: Color = Color::Rgb(0x1a, 0x1b, 0x26);

    fn tokyonight() -> Theme {
        load_by_name("tokyonight").expect("tokyonight theme must exist")
    }

    #[test]
    fn set_advances_generation() {
        let before = generation();
        set(dracula());
        assert!(generation() > before);
    }

    #[test]
    fn set_installs_theme_before_generation_observed() {
        let theme = tokyonight();
        let expected_syntax_bg = theme.syntax.settings.background;
        let before = generation();

        set(theme);

        let observed = generation();
        assert!(observed > before);
        assert_eq!(current().background, TOKYONIGHT_BG);
        assert_eq!(
            caudra_highlight::theme().settings.background,
            expected_syntax_bg,
            "syntax palette must reflect the new theme once generation advances",
        );
    }

    #[test]
    fn set_generation_is_monotonic_across_switches() {
        let g0 = generation();
        set(dracula());
        let g1 = generation();
        assert!(g1 > g0);
        assert_eq!(current().background, DRACULA_BG);

        set(tokyonight());
        let g2 = generation();
        assert!(g2 > g1);
        assert_eq!(current().background, TOKYONIGHT_BG);
        assert_eq!(
            caudra_highlight::theme().settings.background,
            tokyonight().syntax.settings.background,
        );
    }
}
