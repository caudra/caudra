use caudra_config::Feature;
use caudra_ui::keybindings::{
    ALT_SEP, KEYBINDS, KeyLabel, Keybind, KeybindContext, LEADER_PREFIX, all_contexts,
};

use crate::page_header;

const TEXT_FIELDS_INTRO: &str = "\
These keys work the same in every text field: the composer, a picker's filter, \
forms and prompts, `/btw`, and the workbench editor with its fields. Where a \
surface binds one of them itself, the surface wins. `/btw` keeps `Ctrl+Y` to copy \
the answer, and the permissions picker keeps `Ctrl+E` to edit, `Ctrl+K` to revoke, \
and `Ctrl+Y` to copy a rule. In a list, `Home` and `End` move the cursor in its \
filter, and `Ctrl+Home` and `Ctrl+End` pick the first and last item.\n\n\
`Ctrl+Z` is undo everywhere, so it never suspends Caudra. A masked field, such as \
an API key, refuses to select all, copy, or cut. A terminal paste goes to the field \
that has focus. `Super` chords need a terminal that reports them, such as one with \
the kitty keyboard protocol.\n\n";

const MAIN_CONTEXTS: &[KeybindContext] = &[
    KeybindContext::General,
    KeybindContext::Editing,
    KeybindContext::TextFields,
    KeybindContext::PasteEditor,
    KeybindContext::Review,
    KeybindContext::Streaming,
    KeybindContext::FormInput,
    KeybindContext::Picker,
    KeybindContext::Docs,
    KeybindContext::SandboxManager,
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
    match kb
        .feature()
        .filter(|&feature| kb.context.feature() != Some(feature))
    {
        Some(feature) => format!("{} (needs `{feature}`)", kb.description),
        None => kb.description.to_string(),
    }
}

fn experiment_note(ctx: KeybindContext) -> Option<String> {
    ctx.feature().map(|feature| {
        format!(
            "These keys exist only with `{} = true` under `[experimental]`. See \
             [Experimental features](/docs/configuration/#experimental-features).\n\n",
            feature.key()
        )
    })
}

fn write_table_2col(out: &mut String, rows: &[(String, String)]) {
    out.push_str("| Key | Action |\n|-----|--------|\n");
    for (key, desc) in rows {
        out.push_str(&format!("| {key} | {desc} |\n"));
    }
}

fn write_section(out: &mut String, ctx: KeybindContext) {
    out.push_str(&format!("\n## {}\n\n", ctx.label()));
    if let Some(note) = experiment_note(ctx) {
        out.push_str(&note);
    }
    if ctx == KeybindContext::TextFields {
        out.push_str(TEXT_FIELDS_INTRO);
    }

    let rows: Vec<_> = KEYBINDS
        .iter()
        .filter(|kb| kb.context == ctx)
        .map(|kb| (label_str(kb.label), description_str(kb)))
        .collect();

    if !rows.is_empty() {
        write_table_2col(out, &rows);
    }
    if ctx == KeybindContext::SandboxManager {
        out.push_str(
            "\nSee [Managed Sandboxes](/docs/sandboxes/#tui-manager) for instance actions and \
             [image forms](/docs/sandboxes/#images-and-template-catalog) for the host picker and \
             approved probe. File uploads and downloads live in the \
             [workbench Transfer view](/docs/workbench/#transfer), available after attaching \
             to a sandbox. Leaving Transfer requests cancellation and waits for cleanup.\n",
        );
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

fn write_focus(out: &mut String) {
    out.push_str("\n## Focus\n\n");
    out.push_str(
        "`PageUp`, `PageDown`, `Home`, and `End` act on whatever holds the \
         keyboard. While you are typing they belong to the composer, so \
         `Home` and `End` move the text cursor and the page keys scroll a \
         draft too tall to fit. When the draft fits, a page key scrolls the \
         transcript and hands it the focus, so `Home` and `End` then reach \
         the top and bottom of the chat.\n\n",
    );
    out.push_str(
        "Typing anything takes the focus back, and so does `Esc`. Clicking \
         the transcript gives it the focus, and clicking the composer \
         returns it. The wheel scrolls whatever the pointer is over and \
         leaves the focus where it is. `Ctrl+U`, `Ctrl+G`, and `Ctrl+X b` \
         scroll the transcript wherever the focus sits, and an open modal \
         claims all four navigation keys for itself.\n\n",
    );
    out.push_str(
        "Anywhere a scrollbar is shown it can be dragged. Press the thumb \
         and the surface follows the pointer, press the track anywhere \
         else and the thumb jumps there and stays held. Hold `Alt` while \
         dragging to cover an eighth of the distance, which is what makes \
         a long transcript landable. Dragging the transcript bar shows \
         which message the thumb is on.\n\n",
    );
    out.push_str(
        "A modal whose lines run wider than the screen wears a second bar \
         along its bottom border, and drags the same way. `Shift+Left` and \
         `Shift+Right` move it by a column of a table at a time, and a \
         sideways wheel over the modal moves it too. The bar appears only \
         while there is something off the edge to reach.\n\n",
    );
    out.push_str(
        "That bar carries an arrow at each end, and pressing one moves half \
         a screen; an arrow dims once its direction is spent. The arrows \
         are the way across on a phone: Android terminals send no sideways \
         wheel at all, so a swipe that way reports nothing, and a tap is \
         the only gesture left. They are deliberately easy to hit, so a \
         press just above one still counts.\n\n",
    );
    out.push_str(
        "Holding `Alt` while turning the wheel scrolls four times as far. \
         A middle-click anchors the view and scrolls it on its own, faster \
         the further you then move the pointer from the mark, until you \
         middle-click again or touch anything else. `ui.scrollbar` set to \
         `false` hides the bars and with them the drag.\n",
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

    let mut experimental: Vec<(KeybindContext, Feature)> = Vec::new();
    for kb in &child_binds {
        let key = label_str(kb.label);
        out.push_str(&format!(
            "| {} | {key} | {} |\n",
            kb.context.label(),
            description_str(kb)
        ));
        if let Some(feature) = kb.context.feature()
            && !experimental.iter().any(|(ctx, _)| *ctx == kb.context)
        {
            experimental.push((kb.context, feature));
        }
    }

    if experimental.is_empty() {
        return;
    }
    out.push_str(
        "\nSome of these contexts belong to an \
         [experimental feature](/docs/configuration/#experimental-features) and exist only \
         while its switch is on:\n\n",
    );
    for (ctx, feature) in experimental {
        out.push_str(&format!("- {}: `{}`\n", ctx.label(), feature.key()));
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
    let mut out = page_header("Keybindings", "Defaults, precedence, rebinding from Lua.");
    write_leader(&mut out);
    write_focus(&mut out);

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
        "With [Lua plugins](/docs/configuration/#experimental-features) turned \
         on, plugins and `init.lua` can rebind keys at runtime with \
         `caudra.keymap.set` and `caudra.keymap.del`. The tables above are the \
         built-in defaults. An override on the same key wins, unless a \
         modal or overlay is open (help, plan form, permission prompt).\n\n",
    );
    out.push_str("Precedence, high to low:\n\n");
    out.push_str(
        "1. **Modal and overlay keys.** An open modal or picker consumes \
         its keys first, so they cannot be shadowed while open.\n\
         2. **Lua overrides** from `caudra.keymap.set`. Last set wins, and \
         binding the same key twice warns.\n\
         3. **Built-in defaults.** An override on the same key shadows \
         them, and `caudra.keymap.del` lifts the override so the default \
         returns. Every built-in key can be remapped this way.\n\n",
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
         that will not close, a plugin that throws on load), boot without Lua:\n\n",
    );
    out.push_str("```bash\ncaudra --no-plugins\n```\n\n");
    out.push_str(
        "This run skips every plugin and both `init.lua` files and starts \
         no Lua host, even with `lua_plugins` on. Every built-in tool is \
         native, so tools still work. `caudra.toml`, `permissions.toml`, \
         custom commands, and env files load as usual.\n\n",
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
