//! Best-effort LaTeX math to Unicode for terminal display.
//!
//! Terminals have no KaTeX, so equations are approximated with Unicode:
//! `\alpha` becomes `α`, `x^2` becomes `x²`, `\frac{a}{b}` becomes `(a)/(b)`.
//! The conversion is total. It never panics and unknown commands degrade to
//! their bare name, so a caller always gets something readable; the original
//! LaTeX stays available through the span's source range.

use std::fmt::Write;
use std::mem;

/// Longer sources are left as raw LaTeX. Streaming re-renders the tail on
/// every chunk, so conversion has to stay cheap.
const MAX_SOURCE_LEN: usize = 4096;
const SUPERSCRIPT_MARKER: char = '^';
const SUBSCRIPT_MARKER: char = '_';
const GROUP_OPEN: char = '(';
const GROUP_CLOSE: char = ')';
const FRACTION_SLASH: char = '/';
const ROW_SEPARATOR: &str = "\\\\";
const ROW_SEPARATOR_NAME: &str = "\\";
const ROW_BREAK: &str = "; ";

/// Commands whose single argument renders verbatim, without math styling.
const TEXT_COMMANDS: &[&str] = &[
    "bm",
    "boldsymbol",
    "emph",
    "mathbf",
    "mathbin",
    "mathclose",
    "mathit",
    "mathnormal",
    "mathopen",
    "mathrel",
    "mathrm",
    "mathsf",
    "mathtt",
    "mbox",
    "operatorname",
    "overbrace",
    "substack",
    "text",
    "textbf",
    "textit",
    "textnormal",
    "textrm",
    "textsc",
    "textsf",
    "texttt",
    "textup",
    "underbrace",
];
/// Commands that only affect spacing.
const SPACING_COMMANDS: &[&str] = &[",", ";", ":", "!", " ", "quad", "qquad", "thinspace"];

/// Commands that consume an argument and render as a single space.
const SPACING_WITH_ARG: &[&str] = &["hspace", "vspace", "phantom", "hphantom", "vphantom"];

/// Commands whose one argument is metadata, not maths.
const DROPPED_ARG_COMMANDS: &[&str] = &["cite", "eqref", "label", "ref", "tag"];

/// Environments taking a column specification that is layout, not content.
const SPEC_ENVIRONMENTS: &[&str] = &["array", "tabular"];

/// Typesetting hints with no textual content, including the argument-less
/// TeX font switches that predate `\mathrm` and friends.
const STYLE_COMMANDS: &[&str] = &[
    "bf",
    "cal",
    "displaystyle",
    "it",
    "nonumber",
    "notag",
    "rm",
    "sc",
    "sf",
    "tt",
    "limits",
    "mathstrut",
    "nolimits",
    "scriptscriptstyle",
    "scriptstyle",
    "textstyle",
];

/// Combining marks, applied to every character of the argument so that
/// `\overline{AB}` draws a bar over both letters.
const ACCENTS: &[(&str, char)] = &[
    ("acute", '\u{301}'),
    ("bar", '\u{304}'),
    ("breve", '\u{306}'),
    ("check", '\u{30c}'),
    ("ddot", '\u{308}'),
    ("dot", '\u{307}'),
    ("grave", '\u{300}'),
    ("hat", '\u{302}'),
    ("mathring", '\u{30a}'),
    ("overleftarrow", '\u{20d6}'),
    ("overline", '\u{305}'),
    ("overrightarrow", '\u{20d7}'),
    ("tilde", '\u{303}'),
    ("underline", '\u{332}'),
    ("vec", '\u{20d7}'),
    ("widehat", '\u{302}'),
    ("widetilde", '\u{303}'),
];

/// `\not` before a relation. Anything else falls back to a combining slash.
const NEGATIONS: &[(&str, &str)] = &[
    ("<", "≮"),
    ("=", "≠"),
    (">", "≯"),
    ("\u{2208}", "∉"),
    ("\u{2223}", "∤"),
    ("\u{2282}", "⊄"),
    ("\u{2286}", "⊈"),
    ("\u{2264}", "≰"),
    ("\u{2265}", "≱"),
];
const COMBINING_SLASH: char = '\u{338}';

/// Matrix-like environments carry their own brackets.
const ENVIRONMENT_FENCES: &[(&str, &str, &str)] = &[
    ("Bmatrix", "{", "}"),
    ("bmatrix", "[", "]"),
    ("cases", "{", ""),
    ("pmatrix", "(", ")"),
    ("vmatrix", "|", "|"),
];

/// Sorted by name for binary search; `symbols_are_sorted` pins the order.
const SYMBOLS: &[(&str, &str)] = &[
    ("Delta", "Δ"),
    ("Gamma", "Γ"),
    ("Im", "ℑ"),
    ("Lambda", "Λ"),
    ("Leftarrow", "⇐"),
    ("Leftrightarrow", "⇔"),
    ("Longleftrightarrow", "⟺"),
    ("Omega", "Ω"),
    ("Phi", "Φ"),
    ("Pi", "Π"),
    ("Psi", "Ψ"),
    ("Re", "ℜ"),
    ("Rightarrow", "⇒"),
    ("Sigma", "Σ"),
    ("Theta", "Θ"),
    ("Upsilon", "Υ"),
    ("Vert", "‖"),
    ("Xi", "Ξ"),
    ("aleph", "ℵ"),
    ("alpha", "α"),
    ("angle", "∠"),
    ("approx", "≈"),
    ("ast", "∗"),
    ("asymp", "≍"),
    ("because", "∵"),
    ("beta", "β"),
    ("bigcap", "⋂"),
    ("bigcup", "⋃"),
    ("bigodot", "⨀"),
    ("bigoplus", "⨁"),
    ("bigotimes", "⨂"),
    ("bigsqcup", "⨆"),
    ("biguplus", "⨄"),
    ("bigvee", "⋁"),
    ("bigwedge", "⋀"),
    ("bmod", "mod"),
    ("bot", "⊥"),
    ("bullet", "∙"),
    ("cap", "∩"),
    ("cdot", "·"),
    ("cdots", "⋯"),
    ("chi", "χ"),
    ("circ", "∘"),
    ("cong", "≅"),
    ("cup", "∪"),
    ("dagger", "†"),
    ("dashv", "⊣"),
    ("ddots", "⋱"),
    ("delta", "δ"),
    ("diamond", "⋄"),
    ("div", "÷"),
    ("doteq", "≐"),
    ("dots", "…"),
    ("downarrow", "↓"),
    ("ell", "ℓ"),
    ("emptyset", "∅"),
    ("epsilon", "ε"),
    ("equiv", "≡"),
    ("eta", "η"),
    ("exists", "∃"),
    ("forall", "∀"),
    ("gamma", "γ"),
    ("ge", "≥"),
    ("geq", "≥"),
    ("gg", "≫"),
    ("hbar", "ℏ"),
    ("hookleftarrow", "↩"),
    ("hookrightarrow", "↪"),
    ("iff", "⇔"),
    ("iiint", "∭"),
    ("iint", "∬"),
    ("impliedby", "⟸"),
    ("implies", "⟹"),
    ("in", "∈"),
    ("infty", "∞"),
    ("int", "∫"),
    ("iota", "ι"),
    ("kappa", "κ"),
    ("lVert", "‖"),
    ("lambda", "λ"),
    ("land", "∧"),
    ("langle", "⟨"),
    ("lceil", "⌈"),
    ("ldots", "…"),
    ("le", "≤"),
    ("leftarrow", "←"),
    ("leftrightarrow", "↔"),
    ("leq", "≤"),
    ("lfloor", "⌊"),
    ("ll", "≪"),
    ("lnot", "¬"),
    ("longleftarrow", "⟵"),
    ("longleftrightarrow", "⟷"),
    ("longrightarrow", "⟶"),
    ("lor", "∨"),
    ("mapsto", "↦"),
    ("mid", "∣"),
    ("models", "⊨"),
    ("mp", "∓"),
    ("mu", "μ"),
    ("nabla", "∇"),
    ("ne", "≠"),
    ("neg", "¬"),
    ("neq", "≠"),
    ("nexists", "∄"),
    ("ngeq", "≱"),
    ("ni", "∋"),
    ("nleq", "≰"),
    ("nmid", "∤"),
    ("notin", "∉"),
    ("nrightarrow", "↛"),
    ("nsubseteq", "⊈"),
    ("nu", "ν"),
    ("odot", "⊙"),
    ("oint", "∮"),
    ("omega", "ω"),
    ("ominus", "⊖"),
    ("oplus", "⊕"),
    ("oslash", "⊘"),
    ("otimes", "⊗"),
    ("parallel", "∥"),
    ("partial", "∂"),
    ("perp", "⊥"),
    ("phi", "φ"),
    ("pi", "π"),
    ("pm", "±"),
    ("prec", "≺"),
    ("preceq", "⪯"),
    ("prime", "′"),
    ("prod", "∏"),
    ("propto", "∝"),
    ("psi", "ψ"),
    ("rVert", "‖"),
    ("rangle", "⟩"),
    ("rceil", "⌉"),
    ("rfloor", "⌋"),
    ("rho", "ρ"),
    ("rightarrow", "→"),
    ("rightleftharpoons", "⇌"),
    ("setminus", "∖"),
    ("sigma", "σ"),
    ("sim", "∼"),
    ("simeq", "≃"),
    ("sqcap", "⊓"),
    ("sqcup", "⊔"),
    ("sqsubseteq", "⊑"),
    ("star", "⋆"),
    ("subset", "⊂"),
    ("subseteq", "⊆"),
    ("subsetneq", "⊊"),
    ("succ", "≻"),
    ("succeq", "⪰"),
    ("sum", "∑"),
    ("supset", "⊃"),
    ("supseteq", "⊇"),
    ("supsetneq", "⊋"),
    ("tau", "τ"),
    ("therefore", "∴"),
    ("theta", "θ"),
    ("times", "×"),
    ("to", "→"),
    ("top", "⊤"),
    ("triangle", "△"),
    ("uparrow", "↑"),
    ("uplus", "⊎"),
    ("upsilon", "υ"),
    ("varepsilon", "ε"),
    ("varnothing", "∅"),
    ("varphi", "ϕ"),
    ("varrho", "ϱ"),
    ("varsigma", "ς"),
    ("vartheta", "ϑ"),
    ("vdots", "⋮"),
    ("vee", "∨"),
    ("wedge", "∧"),
    ("xi", "ξ"),
    ("zeta", "ζ"),
    ("|", "‖"),
];

/// `\mathbb{R}` and friends. Only letters with a precomposed Unicode form
/// are listed; anything else falls back to the plain letter.
const BLACKBOARD: &[(char, &str)] = &[
    ('C', "ℂ"),
    ('H', "ℍ"),
    ('N', "ℕ"),
    ('P', "ℙ"),
    ('Q', "ℚ"),
    ('R', "ℝ"),
    ('Z', "ℤ"),
];
const SCRIPT: &[(char, &str)] = &[
    ('B', "ℬ"),
    ('E', "ℰ"),
    ('F', "ℱ"),
    ('H', "ℋ"),
    ('I', "ℐ"),
    ('L', "ℒ"),
    ('M', "ℳ"),
    ('R', "ℛ"),
];

const SUPERSCRIPTS: &[(char, char)] = &[
    ('(', '⁽'),
    (')', '⁾'),
    ('+', '⁺'),
    ('-', '⁻'),
    ('0', '⁰'),
    ('1', '¹'),
    ('2', '²'),
    ('3', '³'),
    ('4', '⁴'),
    ('5', '⁵'),
    ('6', '⁶'),
    ('7', '⁷'),
    ('8', '⁸'),
    ('9', '⁹'),
    ('=', '⁼'),
    ('a', 'ᵃ'),
    ('b', 'ᵇ'),
    ('c', 'ᶜ'),
    ('d', 'ᵈ'),
    ('e', 'ᵉ'),
    ('f', 'ᶠ'),
    ('g', 'ᵍ'),
    ('h', 'ʰ'),
    ('i', 'ⁱ'),
    ('j', 'ʲ'),
    ('k', 'ᵏ'),
    ('l', 'ˡ'),
    ('m', 'ᵐ'),
    ('n', 'ⁿ'),
    ('o', 'ᵒ'),
    ('p', 'ᵖ'),
    ('r', 'ʳ'),
    ('s', 'ˢ'),
    ('t', 'ᵗ'),
    ('u', 'ᵘ'),
    ('v', 'ᵛ'),
    ('w', 'ʷ'),
    ('x', 'ˣ'),
    ('y', 'ʸ'),
    ('z', 'ᶻ'),
    ('′', '′'),
    ('∘', '°'),
];
const SUBSCRIPTS: &[(char, char)] = &[
    ('(', '₍'),
    (')', '₎'),
    ('+', '₊'),
    ('-', '₋'),
    ('0', '₀'),
    ('1', '₁'),
    ('2', '₂'),
    ('3', '₃'),
    ('4', '₄'),
    ('5', '₅'),
    ('6', '₆'),
    ('7', '₇'),
    ('8', '₈'),
    ('9', '₉'),
    ('=', '₌'),
    ('a', 'ₐ'),
    ('e', 'ₑ'),
    ('h', 'ₕ'),
    ('i', 'ᵢ'),
    ('j', 'ⱼ'),
    ('k', 'ₖ'),
    ('l', 'ₗ'),
    ('m', 'ₘ'),
    ('n', 'ₙ'),
    ('o', 'ₒ'),
    ('p', 'ₚ'),
    ('r', 'ᵣ'),
    ('s', 'ₛ'),
    ('t', 'ₜ'),
    ('u', 'ᵤ'),
    ('v', 'ᵥ'),
    ('x', 'ₓ'),
];

/// Converts one expression to a single line. Returns `None` when the source
/// is too large, so the caller can show the raw LaTeX instead.
pub fn to_unicode(src: &str) -> Option<String> {
    if src.len() > MAX_SOURCE_LEN {
        return None;
    }
    let mut out = String::with_capacity(src.len());
    let mut scanner = Scanner { src, pos: 0 };
    scanner.render(&mut out);
    Some(collapse_spaces(&out))
}

/// Converts a display block, one output line per `\\` row separator.
pub fn to_unicode_rows(src: &str) -> Option<Vec<String>> {
    if src.len() > MAX_SOURCE_LEN {
        return None;
    }
    let rows: Vec<String> = split_rows(src)
        .into_iter()
        .filter_map(to_unicode)
        .filter(|row| !row.is_empty())
        .collect();
    Some(rows)
}

/// Splits on `\\` without breaking `\\\\`-escaped text or other commands.
fn split_rows(src: &str) -> Vec<&str> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut pos = 0;
    while let Some(found) = src[pos..].find(ROW_SEPARATOR) {
        let at = pos + found;
        rows.push(&src[start..at]);
        pos = at + ROW_SEPARATOR.len();
        start = pos;
    }
    rows.push(&src[start..]);
    rows
}

fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(ch);
    }
    out
}

fn lookup(table: &[(char, &'static str)], key: char) -> Option<&'static str> {
    table
        .binary_search_by_key(&key, |(k, _)| *k)
        .ok()
        .map(|i| table[i].1)
}

fn script_char(table: &[(char, char)], key: char) -> Option<char> {
    table
        .binary_search_by_key(&key, |(k, _)| *k)
        .ok()
        .map(|i| table[i].1)
}

struct Scanner<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn eat(&mut self, ch: char) -> bool {
        if self.peek() == Some(ch) {
            self.pos += ch.len_utf8();
            return true;
        }
        false
    }

    fn skip_spaces(&mut self) {
        while self.peek().is_some_and(|c| c == ' ' || c == '\n') {
            self.pos += 1;
        }
    }

    /// Renders until the input ends or an unmatched `}` is reached.
    fn render(&mut self, out: &mut String) {
        while let Some(ch) = self.peek() {
            match ch {
                '}' => return,
                '{' => {
                    self.pos += 1;
                    self.render(out);
                    self.eat('}');
                }
                '\\' => self.command(out),
                '^' => {
                    self.pos += 1;
                    let arg = self.argument();
                    push_script(out, &arg, SUPERSCRIPTS, SUPERSCRIPT_MARKER);
                }
                '_' => {
                    self.pos += 1;
                    let arg = self.argument();
                    push_script(out, &arg, SUBSCRIPTS, SUBSCRIPT_MARKER);
                }
                '&' | '$' => {
                    self.pos += 1;
                    out.push(' ');
                }
                '~' => {
                    self.pos += 1;
                    out.push(' ');
                }
                _ => {
                    self.pos += ch.len_utf8();
                    out.push(ch);
                }
            }
        }
    }

    /// One argument: a braced group, a command, or a single character.
    fn argument(&mut self) -> String {
        self.skip_spaces();
        let mut out = String::new();
        match self.peek() {
            Some('{') => {
                self.pos += 1;
                self.render(&mut out);
                self.eat('}');
            }
            Some('\\') => self.command(&mut out),
            Some(ch) => {
                self.pos += ch.len_utf8();
                out.push(ch);
            }
            None => {}
        }
        out
    }

    /// Optional `[...]` argument, as in `\sqrt[3]{x}`.
    fn optional(&mut self) -> Option<String> {
        if !self.eat('[') {
            return None;
        }
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            if ch == ']' {
                break;
            }
            match ch {
                '\\' => self.command(&mut out),
                _ => {
                    self.pos += ch.len_utf8();
                    out.push(ch);
                }
            }
        }
        self.eat(']');
        Some(out)
    }

    fn name(&mut self) -> &'a str {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '*')
        {
            self.pos += 1;
        }
        if start == self.pos {
            // A non-alphabetic escape such as `\,` or `\{` is one character.
            self.bump();
        }
        &self.src[start..self.pos]
    }

    fn command(&mut self, out: &mut String) {
        self.pos += 1;
        // `\operatorname*` and `\section*` differ only in numbering.
        let name = self.name().trim_end_matches('*');
        match name {
            "" => {}
            "frac" | "dfrac" | "tfrac" => {
                let numerator = self.argument();
                let denominator = self.argument();
                push_fraction(out, &numerator, &denominator);
            }
            "sqrt" => {
                let index = self.optional();
                let radicand = self.argument();
                push_root(out, index.as_deref(), &radicand);
            }
            "mathbb" => push_alphabet(out, &self.argument(), BLACKBOARD),
            "binom" | "dbinom" | "tbinom" => {
                let n = self.argument();
                let k = self.argument();
                let _ = write!(out, "C({n}, {k})");
            }
            "pmod" => {
                let modulus = self.argument();
                let _ = write!(out, "(mod {modulus})");
            }
            "not" => push_negation(out, &self.argument()),
            ROW_SEPARATOR_NAME => {
                // `\\[2mm]` carries row spacing that is layout, not content.
                let _ = self.optional();
                out.push_str(ROW_BREAK);
            }
            "overset" | "stackrel" => self.annotate(out, SUPERSCRIPTS, SUPERSCRIPT_MARKER),
            "underset" => self.annotate(out, SUBSCRIPTS, SUBSCRIPT_MARKER),
            // Infix TeX primitives: everything rendered so far is the
            // numerator, everything left in the group is the denominator.
            "over" | "choose" => {
                let mut denominator = String::new();
                self.render(&mut denominator);
                let numerator = mem::take(out);
                let (numerator, denominator) = (numerator.trim(), denominator.trim());
                if name == "choose" {
                    let _ = write!(out, "C({numerator}, {denominator})");
                } else {
                    push_fraction(out, numerator, denominator);
                }
            }
            "mathcal" | "mathscr" => push_alphabet(out, &self.argument(), SCRIPT),
            "left" | "right" | "big" | "Big" | "bigg" | "Bigg" => {
                // Sizing only; the delimiter itself renders next.
                if self.peek() == Some('.') {
                    self.pos += 1;
                }
            }
            "begin" | "end" => {
                let environment = self.argument();
                if name == "begin" && SPEC_ENVIRONMENTS.contains(&environment.as_str()) {
                    let _ = self.optional();
                    let _ = self.argument();
                }
                // Matrix-likes carry brackets; other environments are pure
                // structure and leave nothing behind but a separator.
                match environment_fence(&environment, name == "begin") {
                    Some(fence) => out.push_str(fence),
                    None => out.push(' '),
                }
            }
            _ if STYLE_COMMANDS.contains(&name) => {}
            _ if DROPPED_ARG_COMMANDS.contains(&name) => {
                let _ = self.argument();
            }
            _ if TEXT_COMMANDS.contains(&name) => out.push_str(&self.argument()),
            _ if SPACING_COMMANDS.contains(&name) => out.push(' '),
            _ if SPACING_WITH_ARG.contains(&name) => {
                let _ = self.argument();
                out.push(' ');
            }
            _ => match ACCENTS.binary_search_by_key(&name, |(k, _)| *k) {
                Ok(i) => push_accent(out, &self.argument(), ACCENTS[i].1),
                Err(_) => self.symbol(out, name),
            },
        }
    }

    /// `\overset{a}{b}` puts `a` above `b`; inline that becomes a script,
    /// which also makes `\underset{x}{\min}` match plain `\min_x`.
    fn annotate(&mut self, out: &mut String, table: &[(char, char)], marker: char) {
        let annotation = self.argument();
        let base = self.argument();
        out.push_str(&base);
        push_script(out, &annotation, table, marker);
    }

    fn symbol(&mut self, out: &mut String, name: &str) {
        match SYMBOLS.binary_search_by_key(&name, |(k, _)| *k) {
            Ok(i) => out.push_str(SYMBOLS[i].1),
            // Unknown commands degrade to their bare name so the reader
            // still sees what was written.
            Err(_) => out.push_str(name),
        }
    }
}

fn environment_fence(environment: &str, opening: bool) -> Option<&'static str> {
    ENVIRONMENT_FENCES
        .iter()
        .find(|(name, _, _)| *name == environment)
        .map(|(_, open, close)| if opening { *open } else { *close })
}

fn push_accent(out: &mut String, arg: &str, mark: char) {
    for ch in arg.chars() {
        out.push(ch);
        out.push(mark);
    }
}

fn push_negation(out: &mut String, arg: &str) {
    match NEGATIONS.iter().find(|(k, _)| *k == arg) {
        Some((_, negated)) => out.push_str(negated),
        None => {
            out.push_str(arg);
            out.push(COMBINING_SLASH);
        }
    }
}

fn push_alphabet(out: &mut String, arg: &str, table: &[(char, &'static str)]) {
    for ch in arg.chars() {
        match lookup(table, ch) {
            Some(mapped) => out.push_str(mapped),
            None => out.push(ch),
        }
    }
}

/// Uses Unicode script characters when every character has one, otherwise
/// falls back to `^(...)` so nothing is silently dropped.
fn push_script(out: &mut String, arg: &str, table: &[(char, char)], marker: char) {
    if arg.is_empty() {
        return;
    }
    if let Some(text) = arg
        .chars()
        .map(|c| script_char(table, c))
        .collect::<Option<String>>()
        .filter(|t| !t.is_empty())
    {
        out.push_str(&text);
        return;
    }
    // A lone symbol needs no grouping, and `x^∞` matches how LaTeX itself
    // reads `x^\infty`. Keeping parens for longer scripts is what tells
    // `x^{A}B` (`x^AB`) apart from `x^{AB}` (`x^(AB)`).
    out.push(marker);
    if arg.chars().count() == 1 {
        out.push_str(arg);
    } else {
        out.push(GROUP_OPEN);
        out.push_str(arg);
        out.push(GROUP_CLOSE);
    }
}

fn push_fraction(out: &mut String, numerator: &str, denominator: &str) {
    let wrap = |out: &mut String, part: &str| {
        if needs_parens(part) {
            let _ = write!(out, "({part})");
        } else {
            out.push_str(part);
        }
    };
    wrap(out, numerator);
    out.push(FRACTION_SLASH);
    wrap(out, denominator);
}

fn push_root(out: &mut String, index: Option<&str>, radicand: &str) {
    out.push(match index {
        Some("3") => '∛',
        Some("4") => '∜',
        _ => '√',
    });
    if needs_parens(radicand) {
        let _ = write!(out, "({radicand})");
    } else {
        out.push_str(radicand);
    }
}

/// Single terms stay bare; anything with an operator or a space needs
/// grouping so `\frac{a+b}{c}` does not read as `a+b/c`.
fn needs_parens(part: &str) -> bool {
    part.chars().count() > 1
        && part
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '+' | '-' | '/' | '·' | '±' | '∓'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    /// Truncated and malformed input arrives constantly while a reply
    /// streams, so every prefix must render without panicking.
    #[test_case(r"\frac{a}{b"; "unclosed_fraction")]
    #[test_case(r"x^{"; "unclosed_script")]
    #[test_case("{{{"; "unbalanced_open_braces")]
    #[test_case("}}}"; "unbalanced_close_braces")]
    #[test_case(r"\frac"; "fraction_without_arguments")]
    #[test_case(r"\sqrt"; "root_without_argument")]
    #[test_case("\\"; "lone_backslash")]
    #[test_case(r"\begin{"; "unclosed_environment")]
    #[test_case("^"; "lone_superscript")]
    #[test_case("_"; "lone_subscript")]
    fn malformed_input_renders_without_panicking(src: &str) {
        let _ = to_unicode(src);
    }

    #[test_case(r"x^{}", "x"; "empty_superscript")]
    #[test_case(r"x_", "x"; "dangling_subscript")]
    fn empty_scripts_leave_no_marker(src: &str, expected: &str) {
        assert_eq!(to_unicode(src).as_deref(), Some(expected));
    }

    #[test]
    fn symbols_are_sorted() {
        assert!(
            SYMBOLS.windows(2).all(|w| w[0].0 < w[1].0),
            "SYMBOLS must stay sorted for binary search"
        );
        for (name, sorted) in [
            ("BLACKBOARD", BLACKBOARD.windows(2).all(|w| w[0].0 < w[1].0)),
            ("SCRIPT", SCRIPT.windows(2).all(|w| w[0].0 < w[1].0)),
            (
                "SUPERSCRIPTS",
                SUPERSCRIPTS.windows(2).all(|w| w[0].0 < w[1].0),
            ),
            ("SUBSCRIPTS", SUBSCRIPTS.windows(2).all(|w| w[0].0 < w[1].0)),
            ("ACCENTS", ACCENTS.windows(2).all(|w| w[0].0 < w[1].0)),
        ] {
            assert!(sorted, "{name} must stay sorted for binary search");
        }
    }

    #[test_case(r"\alpha", "α"; "greek_lower")]
    #[test_case(r"\Gamma", "Γ"; "greek_upper")]
    #[test_case(r"x^2", "x²"; "superscript_digit")]
    #[test_case(r"a_1", "a₁"; "subscript_digit")]
    #[test_case(r"x^{10}", "x¹⁰"; "superscript_group")]
    #[test_case(r"e^{-x}", "e⁻ˣ"; "superscript_signed")]
    #[test_case(r"x^{(n)}", "x⁽ⁿ⁾"; "superscript_parens")]
    #[test_case(r"x^{i+j}", "xⁱ⁺ʲ"; "superscript_expression")]
    #[test_case(r"\hat{x}", "x\u{302}"; "accent_hat")]
    #[test_case(r"\vec{v}", "v\u{20d7}"; "accent_vector")]
    #[test_case(r"\bar{x}", "x\u{304}"; "accent_bar")]
    #[test_case(r"\overline{AB}", "A\u{305}B\u{305}"; "accent_spans_every_character")]
    #[test_case(r"\overrightarrow{AB}", "A\u{20d7}B\u{20d7}"; "accent_overrightarrow")]
    #[test_case(r"\mathbf{v}", "v"; "font_bold_passes_content_through")]
    #[test_case(r"\boldsymbol{\alpha}", "α"; "font_boldsymbol")]
    #[test_case(r"\textbf{bold}", "bold"; "font_textbf")]
    #[test_case(r"\displaystyle \frac{a}{b}", "a/b"; "style_command_vanishes")]
    #[test_case(r"\binom{n}{k}", "C(n, k)"; "binomial")]
    #[test_case(r"90^\circ", "90°"; "degree_sign")]
    #[test_case(r"a \not= b", "a ≠ b"; "negation_of_equals")]
    #[test_case(r"a \nleq b", "a ≰ b"; "negation_symbol")]
    #[test_case(r"a \bmod b", "a mod b"; "binary_mod")]
    #[test_case(r"a \equiv b \pmod{n}", "a ≡ b (mod n)"; "parenthesised_mod")]
    #[test_case(r"\|x\|", "‖x‖"; "double_bar_norm")]
    #[test_case(r"\lVert v \rVert", "‖ v ‖"; "named_norm")]
    #[test_case(r"\bigoplus_{i}", "⨁ᵢ"; "big_operator")]
    #[test_case(r"a \hspace{1cm} b", "a b"; "spacing_with_argument")]
    #[test_case(r"\overbrace{a+b}", "a+b"; "brace_annotation_keeps_content")]
    #[test_case(r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}", "( a b ; c d )"; "pmatrix_gets_brackets")]
    #[test_case(r"\begin{bmatrix} 1 & 0 \\ 0 & 1 \end{bmatrix}", "[ 1 0 ; 0 1 ]"; "bmatrix_gets_brackets")]
    #[test_case(r"\begin{array}{cc} a & b \\ c & d \end{array}", "a b ; c d"; "array_column_spec_is_dropped")]
    #[test_case(r"a \\[2mm] b", "a ; b"; "row_spacing_option_is_dropped")]
    #[test_case(r"x = 1 \tag{1}", "x = 1"; "equation_tag_is_dropped")]
    #[test_case(r"\label{eq:one} x", "x"; "label_is_dropped")]
    #[test_case(r"x \nonumber", "x"; "nonumber_is_dropped")]
    #[test_case(r"\overset{?}{=}", "=^?"; "overset_becomes_superscript")]
    #[test_case(r"\underset{x}{\min}", "minₓ"; "underset_becomes_subscript")]
    #[test_case(r"\operatorname*{argmin}_x f", "argminₓ f"; "starred_command_name")]
    #[test_case(r"{\rm d}x", "dx"; "old_tex_font_switch")]
    #[test_case(r"a \over b", "a/b"; "infix_over")]
    #[test_case(r"{n \choose k}", "C(n, k)"; "infix_choose")]
    #[test_case(r"f^{\prime}", "f′"; "prime_superscript")]
    #[test_case(r"x^{\prime\prime}", "x′′"; "double_prime")]
    #[test_case(r"\left. f \right|_0^1", "f |₀¹"; "evaluation_bar")]
    #[test_case(r"\left\| v \right\|", "‖ v ‖"; "sized_norm_delimiters")]
    #[test_case(r"1{,}000", "1,000"; "digit_grouping")]
    #[test_case(r"\text{if $x > 0$}", "if x > 0"; "math_inside_text")]
    #[test_case(r"\frac{\frac{a}{b}}{\frac{c}{d}}", "(a/b)/(c/d)"; "nested_fractions")]
    #[test_case(r"\frac{1}{1+\frac{1}{1+\frac{1}{x}}}", "1/(1+1/(1+1/x))"; "continued_fraction")]
    #[test_case(r"P(A \mid B) = \frac{P(B \mid A)P(A)}{P(B)}", "P(A ∣ B) = (P(B ∣ A)P(A))/P(B)"; "bayes_rule")]
    #[test_case(r"\mathcal{L} = -\sum_{i} y_i \log \hat{y}_i", "ℒ = -∑ᵢ yᵢ log y\u{302}ᵢ"; "cross_entropy_loss")]
    #[test_case(r"\|\mathbf{x} - \mathbf{y}\|_2^2", "‖x - y‖₂²"; "squared_euclidean_norm")]
    #[test_case(r"\text{softmax}(x_i) = \frac{e^{x_i}}{\sum_j e^{x_j}}", "softmax(xᵢ) = e^(xᵢ)/(∑ⱼ e^(xⱼ))"; "softmax")]
    #[test_case("α + β = γ", "α + β = γ"; "unicode_input_passes_through")]
    #[test_case(r"x^{\alpha}", "x^α"; "superscript_fallback_no_script_form")]
    #[test_case(r"\int_0^1 x^2 dx", "∫₀¹ x² dx"; "integral_numeric_limits")]
    #[test_case(r"\int_a^b f(x) dx", "∫ₐᵇ f(x) dx"; "integral_letter_limits")]
    #[test_case(r"\int_0^\infty e^{-x^2} dx", "∫₀^∞ e^(-x²) dx"; "integral_infinite_limit")]
    #[test_case(r"\oint_C F \cdot dr", "∮_C F · dr"; "contour_integral_single_char_subscript")]
    #[test_case(r"\iint_D f dA", "∬_D f dA"; "double_integral")]
    #[test_case(r"\sum_{i=1}^{\infty} \frac{1}{i^2}", "∑ᵢ₌₁^∞ 1/i²"; "series_to_infinity")]
    #[test_case(r"x^{A}B", "x^AB"; "single_char_script_needs_no_parens")]
    #[test_case(r"x^{AB}", "x^(AB)"; "multi_char_script_keeps_parens")]
    #[test_case(r"\frac{\int_0^1 f}{2}", "(∫₀¹ f)/2"; "integral_in_fraction_gets_parens")]
    #[test_case(r"\int_0^1 x^2 dx + 5", "∫₀¹ x² dx + 5"; "integral_needs_no_parens_dx_terminates")]
    #[test_case(r"\frac{1}{2}", "1/2"; "simple_fraction")]
    #[test_case(r"\frac{a+b}{c}", "(a+b)/c"; "fraction_needs_parens")]
    #[test_case(r"\sqrt{x}", "√x"; "square_root")]
    #[test_case(r"\sqrt[3]{x}", "∛x"; "cube_root")]
    #[test_case(r"\sum_{i=1}^{n} i", "∑ᵢ₌₁ⁿ i"; "sum_with_limits")]
    #[test_case(r"\mathbb{R}^n", "ℝⁿ"; "blackboard")]
    #[test_case(r"\mathcal{L}", "ℒ"; "script_alphabet")]
    #[test_case(r"a \cdot b", "a · b"; "spaced_operator")]
    #[test_case(r"\left( x \right)", "( x )"; "sizing_stripped")]
    #[test_case(r"\text{if } x > 0", "if x > 0"; "text_command")]
    #[test_case(r"a \le b", "a ≤ b"; "relation")]
    #[test_case(r"\int_0^1 f(x) dx", "∫₀¹ f(x) dx"; "integral")]
    #[test_case(r"E = mc^2", "E = mc²"; "einstein")]
    #[test_case(r"\unknowncmd", "unknowncmd"; "unknown_degrades_to_name")]
    #[test_case("", ""; "empty")]
    fn to_unicode_cases(input: &str, expected: &str) {
        assert_eq!(to_unicode(input).as_deref(), Some(expected));
    }

    #[test]
    fn oversized_source_is_rejected() {
        let big = "x".repeat(MAX_SOURCE_LEN + 1);
        assert!(to_unicode(&big).is_none());
        assert!(to_unicode_rows(&big).is_none());
    }

    #[test]
    fn rows_split_on_double_backslash() {
        let rows = to_unicode_rows(r"a = b \\ c = d").expect("converts");
        assert_eq!(rows, vec!["a = b", "c = d"]);
    }

    #[test]
    fn blank_rows_are_dropped() {
        let rows = to_unicode_rows("a \\\\ \\\\ b").expect("converts");
        assert_eq!(rows, vec!["a", "b"]);
    }

    #[test]
    fn conversion_never_panics_on_arbitrary_input() {
        let mut rng = fastrand::Rng::with_seed(0x1A7E5);
        for _ in 0..500 {
            let n = rng.usize(0..200);
            let mut s = String::with_capacity(n);
            for _ in 0..n {
                s.push(rng.char(..));
            }
            assert!(to_unicode(&s).is_some());
        }
    }

    #[test]
    fn unbalanced_braces_terminate() {
        assert_eq!(to_unicode("{{{a").as_deref(), Some("a"));
        assert_eq!(to_unicode("a}}}").as_deref(), Some("a"));
        assert_eq!(to_unicode(r"\frac{a").as_deref(), Some("a/"));
    }
}
