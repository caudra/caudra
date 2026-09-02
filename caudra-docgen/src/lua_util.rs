pub fn find_matching_brace(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, ch) in s[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

pub fn extract_lua_field(s: &str, field: &str) -> Option<String> {
    let dq = format!("{field} = \"");
    let sq = format!("{field} = '");
    if let Some(start) = s.find(&dq) {
        let after = &s[start + dq.len()..];
        let end = after.find('"')?;
        Some(unescape_lua_string(&after[..end]))
    } else {
        let start = s.find(&sq)?;
        let after = &s[start + sq.len()..];
        let end = after.find('\'')?;
        Some(unescape_lua_string(&after[..end]))
    }
}

fn unescape_lua_string(s: &str) -> String {
    s.replace("\\n", "\n")
}

pub struct LuaPluginCommand {
    pub name: String,
    pub description: String,
}

pub fn parse_lua_commands(source: &str) -> Vec<LuaPluginCommand> {
    let mut commands = Vec::new();
    let marker = "register_command({";
    let mut search = source;
    while let Some(start) = search.find(marker) {
        let block = &search[start + marker.len() - 1..];
        if let Some(end) = find_matching_brace(block, 0) {
            let inner = &block[1..end];
            let name = extract_lua_field(inner, "name");
            let desc = extract_lua_field(inner, "description");
            if let (Some(name), Some(description)) = (name, desc) {
                commands.push(LuaPluginCommand { name, description });
            }
            search = &block[end..];
        } else {
            break;
        }
    }
    commands
}

pub fn load_builtin_plugin_commands() -> Vec<LuaPluginCommand> {
    let mut commands = load_plugin_sources()
        .flat_map(|source| parse_lua_commands(&source))
        .collect::<Vec<_>>();
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

/// Anchored to the source tree rather than the working directory, so tests and
/// `just gen-docs` read the same plugins.
fn load_plugin_sources() -> impl Iterator<Item = String> {
    let plugins = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    std::fs::read_dir(plugins)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .flat_map(|plugin| {
            std::fs::read_dir(plugin.path())
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "lua"))
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .collect::<Vec<_>>()
        })
}

pub struct LuaPluginKeymap {
    pub key: String,
    pub description: String,
}

/// Built-in plugin keymaps shadow Caudra's own defaults, so the docs and the
/// collision check both read them from the plugin source rather than a list
/// somebody has to remember to update.
pub fn parse_lua_keymaps(source: &str) -> Vec<LuaPluginKeymap> {
    let marker = "keymap.set(";
    let mut keymaps = Vec::new();
    let mut search = source;
    while let Some(start) = search.find(marker) {
        let call = &search[start + marker.len()..];
        let line = call.lines().next().unwrap_or_default();
        if let Some(lhs) = quoted_args(line).nth(1)
            && let Some(description) = extract_lua_field(line, "desc")
            && let Some(key) = key_notation_label(lhs)
        {
            keymaps.push(LuaPluginKeymap { key, description });
        }
        search = call;
    }
    keymaps
}

pub fn load_builtin_plugin_keymaps() -> Vec<LuaPluginKeymap> {
    let mut keymaps = load_plugin_sources()
        .flat_map(|source| parse_lua_keymaps(&source))
        .collect::<Vec<_>>();
    keymaps.sort_by(|a, b| a.description.cmp(&b.description));
    keymaps
}

fn quoted_args(line: &str) -> impl Iterator<Item = &str> {
    line.split('"').skip(1).step_by(2)
}

/// Vim notation as `Bind::label` would spell it, so plugin rows and built-in
/// rows can be compared directly.
fn key_notation_label(notation: &str) -> Option<String> {
    let inner = notation.strip_prefix('<')?.strip_suffix('>')?;
    let (modifiers, name) = inner.rsplit_once('-')?;
    let mut label = String::new();
    for modifier in modifiers.split('-') {
        label.push_str(match modifier.to_ascii_lowercase().as_str() {
            "c" | "ctrl" => "Ctrl+",
            "a" | "alt" | "m" => "Alt+",
            "s" | "shift" => "Shift+",
            _ => return None,
        });
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    label.push(first.to_ascii_uppercase());
    label.extend(chars);
    Some(label)
}
