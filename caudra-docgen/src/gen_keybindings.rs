use caudra_ui::keybindings::{
    ALT_SEP, KEYBINDS, KeyLabel, Keybind, KeybindContext, LEADER_PREFIX, Platform, all_contexts,
};

const FRONTMATTER: &str = "\
+++
title = \"Keybindings\"
weight = 9
[extra]
group = \"Reference\"
+++";

const MAIN_CONTEXTS: &[KeybindContext] = &[
    KeybindContext::General,
    KeybindContext::Editing,
    KeybindContext::PasteEditor,
    KeybindContext::Review,
    KeybindContext::Streaming,
    KeybindContext::FormInput,
    KeybindContext::Picker,
    KeybindContext::Workbench,
];

fn label_str(label: KeyLabel) -> String {
    label
        .parts()
        .map(|part| format!("`{part}`"))
        .collect::<Vec<_>>()
        .join(ALT_SEP)
}

fn description_str(kb: &Keybind) -> String {
    match kb.platform {
        Platform::All => kb.description.to_string(),
        Platform::UnixOnly => format!("{} (Unix only)", kb.description),
    }
}

fn write_table_2col(out: &mut String, rows: &[(String, String)]) {
    out.push_str("| Key | Action |\n|-----|--------|\n");
    for (key, desc) in rows {
        out.push_str(&format!("| {key} | {desc} |\n"));
    }
}

fn write_section(out: &mut String, ctx: KeybindContext) {
    out.push_str(&format!("\n## {}\n\n", ctx.label()));

    let rows: Vec<_> = KEYBINDS
        .iter()
        .filter(|kb| kb.context == ctx)
        .map(|kb| (label_str(kb.label), description_str(kb)))
        .collect();

    if !rows.is_empty() {
        write_table_2col(out, &rows);
    }
}

fn write_leader(out: &mut String) {
    out.push_str(&format!(
        "`{}` is the leader. It acts as a prefix: press it, then press the \
         chord's second key. Nothing happens until that second key arrives, \
         and `Esc` cancels. Hold the leader for a moment and a panel lists \
         every chord available where you are.\n\n",
        LEADER_PREFIX
    ));
    out.push_str(
        "Leader chords are written as two keys below, and every one of them \
         is reachable on any terminal: Caudra ships no `Alt` defaults, \
         because macOS routes Option through the input method and never \
         reports it as Alt.\n",
    );
}

fn write_context_specific(out: &mut String) {
    let child_binds: Vec<_> = KEYBINDS
        .iter()
        .filter(|kb| kb.context.parent().is_some())
        .collect();

    if child_binds.is_empty() {
        return;
    }

    out.push_str("\n## Context-Specific\n\n");
    out.push_str("Some pickers add extra bindings on top of the defaults:\n\n");
    out.push_str("| Context | Key | Action |\n|---------|-----|--------|\n");

    for kb in &child_binds {
        let key = label_str(kb.label);
        out.push_str(&format!(
            "| {} | {key} | {} |\n",
            kb.context.label(),
            kb.description
        ));
    }
}

fn write_inheritance(out: &mut String) {
    let children: Vec<_> = all_contexts()
        .filter(|ctx| ctx.parent().is_some())
        .collect();

    if children.is_empty() {
        return;
    }

    out.push_str("\n## Context Inheritance\n\n");
    out.push_str("Child contexts inherit their parent's bindings and add their own.\n\n");

    let mut by_parent: Vec<(KeybindContext, Vec<&str>)> = Vec::new();
    for child in &children {
        let parent = child.parent().unwrap();
        if let Some(entry) = by_parent.iter_mut().find(|(p, _)| *p == parent) {
            entry.1.push(child.label());
        } else {
            by_parent.push((parent, vec![child.label()]));
        }
    }

    for (parent, kids) in &by_parent {
        let list = kids.join(", ");
        out.push_str(&format!(
            "- **{}** is the base for: {list}\n",
            parent.label()
        ));
    }
}

pub fn generate() -> String {
    let mut out = String::from(FRONTMATTER);
    out.push_str("\n\n# Keybindings\n\n");
    write_leader(&mut out);

    for &ctx in MAIN_CONTEXTS {
        write_section(&mut out, ctx);
    }

    write_context_specific(&mut out);
    write_inheritance(&mut out);
    write_overrides(&mut out);

    out
}

fn write_overrides(out: &mut String) {
    out.push_str("\n## Overriding Keybindings\n\n");
    out.push_str(
        "Plugins and `init.lua` can rebind keys at runtime with \
         `caudra.keymap.set` and `caudra.keymap.del`. The tables above are the \
         built-in defaults. An override on the same key wins, unless a \
         modal or overlay is open (help, plan form, permission prompt).\n\n",
    );
    out.push_str("Precedence, high to low:\n\n");
    out.push_str(
        "1. **Suspend** (`Ctrl+Z`, Unix). Always wins, non-remappable.\n\
         2. **Modal and overlay keys.** An open modal or picker consumes \
         its keys first, so they cannot be shadowed while open.\n\
         3. **Lua overrides** from `caudra.keymap.set`. Last set wins; \
         binding the same key twice warns.\n\
         4. **Built-in defaults.** An override on the same key shadows \
         them; `caudra.keymap.del` lifts the override so the default returns. \
         Suspend is the only binding outside this layer, so every key is \
         remappable except `Ctrl+Z`.\n\n",
    );
    out.push_str(
        "Only single-key bindings can be overridden. Multi-key combinations \
         and non-key rows (like `Type` to filter) cannot.\n\n",
    );
    out.push_str(
        "The `/help` modal and the splash show default labels, not live \
         overrides, but pressing the key still runs the override.\n\n",
    );
    out.push_str("### Recovering from a bad keymap\n\n");
    out.push_str(
        "If an override leaves Caudra stuck (a rebound `Ctrl+C`, a modal \
         that won't close, a plugin that throws on load), boot without \
         user `init.lua`:\n\n",
    );
    out.push_str("```bash\ncaudra --no-plugins\n```\n\n");
    out.push_str(
        "Skips user `init.lua` files (global and project). The Lua host \
         stays up and every built-in tool is native, so tools still work. \
         `permissions.toml`, custom commands, and env files load as \
         usual.\n\n",
    );
    out.push_str(
        "The default keymap lives in Rust, not Lua, so `--no-plugins` \
         never drops it.\n\n",
    );
    out.push_str("## Shell and images\n\n");
    out.push_str(
        "These are input conventions, not remappable key rows:\n\n\
         - Prefix a line with `!` to run a shell command yourself (5 minute \
         timeout). Use `!!` to hide the command and its output from the agent.\n\
         - `Ctrl+V` pastes an image from the clipboard into the prompt when the \
         model supports vision. You can also paste image file paths.\n\
         - Text pastes with at least 3 lines or more than 150 characters appear \
         as compact tokens. Focus one and press `Enter`, or click it, to edit \
         the complete pasted text.\n",
    );
}
