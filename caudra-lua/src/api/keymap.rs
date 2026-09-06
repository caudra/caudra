use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use caudra_lua_macro::{lua_fn, lua_table};
use crossterm::event::{KeyCode, KeyModifiers};
use mlua::{Lua, RegistryKey, Result as LuaResult, Table};

static NEXT_KEYMAP_ID: AtomicU64 = AtomicU64::new(1);

/// What `<leader>` spells in `caudra.keymap.set`.
const LEADER_NOTATION: &str = "<leader>";

/// One key press, and whether the leader has to precede it. Leader chords are
/// a namespace of their own: `<leader>t` and `t` are different bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChord {
    pub key: KeyCode,
    pub modifiers: KeyModifiers,
    pub leader: bool,
}

#[derive(Clone, Debug)]
pub struct KeymapEntry {
    pub key: KeyCode,
    pub modifiers: KeyModifiers,
    pub leader: bool,
    pub desc: String,
    pub plugin: Arc<str>,
    pub id: u64,
}

#[derive(Clone, Default)]
pub struct KeymapSnapshot {
    pub entries: Vec<KeymapEntry>,
    pub generation: u64,
}

#[derive(Clone)]
pub struct KeymapReader(Arc<ArcSwap<KeymapSnapshot>>);

impl KeymapReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(KeymapSnapshot::default())))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<KeymapSnapshot>> {
        self.0.load()
    }
}

pub(crate) struct KeymapWriter {
    store: Arc<ArcSwap<KeymapSnapshot>>,
    generation: AtomicU64,
}

impl KeymapWriter {
    pub fn new() -> (Self, KeymapReader) {
        let inner = Arc::new(ArcSwap::from_pointee(KeymapSnapshot::default()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
            },
            KeymapReader(inner),
        )
    }

    pub fn publish(&self, entries: Vec<KeymapEntry>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(KeymapSnapshot {
            entries,
            generation,
        }));
    }
}

pub(crate) struct StoredKeymap {
    pub id: u64,
    pub chord: KeyChord,
    pub callback: RegistryKey,
    pub plugin: Arc<str>,
    pub desc: String,
}

pub(crate) struct KeymapStore {
    bindings: Vec<StoredKeymap>,
}

impl KeymapStore {
    pub fn new() -> Self {
        Self {
            bindings: Vec::new(),
        }
    }

    pub fn set(
        &mut self,
        chord: KeyChord,
        callback: RegistryKey,
        plugin: Arc<str>,
        desc: String,
    ) -> (u64, Option<RegistryKey>) {
        let id = NEXT_KEYMAP_ID.fetch_add(1, Ordering::Relaxed);
        let old = self
            .bindings
            .iter()
            .position(|b| b.chord == chord)
            .map(|pos| self.bindings.remove(pos).callback);
        self.bindings.push(StoredKeymap {
            id,
            chord,
            callback,
            plugin,
            desc,
        });
        (id, old)
    }

    pub fn del(&mut self, chord: KeyChord) -> Option<RegistryKey> {
        self.bindings
            .iter()
            .position(|b| b.chord == chord)
            .map(|pos| self.bindings.remove(pos).callback)
    }

    pub fn clear_plugin(&mut self, plugin: &str) -> Vec<RegistryKey> {
        let mut keys = Vec::new();
        let mut i = 0;
        while i < self.bindings.len() {
            if self.bindings[i].plugin.as_ref() == plugin {
                keys.push(self.bindings.remove(i).callback);
            } else {
                i += 1;
            }
        }
        keys
    }

    pub fn snapshot_entries(&self) -> Vec<KeymapEntry> {
        self.bindings
            .iter()
            .map(|b| KeymapEntry {
                key: b.chord.key,
                modifiers: b.chord.modifiers,
                leader: b.chord.leader,
                desc: b.desc.clone(),
                plugin: Arc::clone(&b.plugin),
                id: b.id,
            })
            .collect()
    }

    pub fn callback_for_id(&self, id: u64) -> Option<&RegistryKey> {
        self.bindings
            .iter()
            .find(|b| b.id == id)
            .map(|b| &b.callback)
    }
}

pub fn parse_key_notation(input: &str) -> Result<KeyChord, String> {
    let mut s = input.trim();
    let leader = match s.len() >= LEADER_NOTATION.len()
        && s[..LEADER_NOTATION.len()].eq_ignore_ascii_case(LEADER_NOTATION)
    {
        true => {
            s = s[LEADER_NOTATION.len()..].trim_start();
            true
        }
        false => false,
    };
    if s.is_empty() {
        return Err("empty key notation".into());
    }

    let (key, modifiers) = if s.starts_with('<') && s.ends_with('>') {
        parse_bracketed(&s[1..s.len() - 1])?
    } else if s.chars().count() == 1 {
        (KeyCode::Char(s.chars().next().unwrap()), KeyModifiers::NONE)
    } else {
        return Err(format!("invalid key notation: {s}"));
    };

    Ok(KeyChord {
        key,
        modifiers,
        leader,
    })
}

fn parse_bracketed(inner: &str) -> Result<(KeyCode, KeyModifiers), String> {
    if inner.is_empty() {
        return Err("empty angle-bracket key notation".into());
    }

    let mut modifiers = KeyModifiers::NONE;
    let mut rest = inner;

    loop {
        let lower = rest.to_lowercase();
        if lower.starts_with("c-") {
            modifiers |= KeyModifiers::CONTROL;
            rest = &rest[2..];
        } else if lower.starts_with("ctrl-") {
            modifiers |= KeyModifiers::CONTROL;
            rest = &rest[5..];
        } else if lower.starts_with("a-") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[2..];
        } else if lower.starts_with("alt-") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[4..];
        } else if lower.starts_with("m-") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[2..];
        } else if lower.starts_with("s-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = &rest[2..];
        } else if lower.starts_with("shift-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = &rest[6..];
        } else {
            break;
        }
    }

    let key = parse_key_name(rest)?;
    Ok((key, modifiers))
}

fn parse_key_name(name: &str) -> Result<KeyCode, String> {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "cr" | "enter" | "return" => Ok(KeyCode::Enter),
        "space" => Ok(KeyCode::Char(' ')),
        "esc" | "escape" => Ok(KeyCode::Esc),
        "tab" => Ok(KeyCode::Tab),
        "bs" | "backspace" => Ok(KeyCode::Backspace),
        "del" | "delete" => Ok(KeyCode::Delete),
        "up" => Ok(KeyCode::Up),
        "down" => Ok(KeyCode::Down),
        "left" => Ok(KeyCode::Left),
        "right" => Ok(KeyCode::Right),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        "pageup" => Ok(KeyCode::PageUp),
        "pagedown" => Ok(KeyCode::PageDown),
        "insert" => Ok(KeyCode::Insert),
        s if s.starts_with('f') && s.len() > 1 => {
            let n: u8 = s[1..]
                .parse()
                .map_err(|_| format!("invalid function key: {name}"))?;
            if !(1..=12).contains(&n) {
                return Err(format!("function key out of range: {name}"));
            }
            Ok(KeyCode::F(n))
        }
        _ => {
            if name.len() == 1 {
                Ok(KeyCode::Char(name.chars().next().unwrap()))
            } else {
                Err(format!("unknown key: {name}"))
            }
        }
    }
}

fn publish_keymap_snapshot(lua: &Lua) {
    if let Some(store) = lua.app_data_ref::<KeymapStore>() {
        let entries = store.snapshot_entries();
        if let Some(writer) = lua.app_data_ref::<KeymapWriter>() {
            writer.publish(entries);
        }
    }
}

/// Bind a key to a Lua function, just like `vim.keymap.set`. Only
/// normal mode (`"n"`) is supported right now. If {lhs} is already
/// mapped, the old binding is replaced and a warning is logged.
///
/// Prefix {lhs} with `<leader>` to bind a two-key chord under `Ctrl+X`.
/// Leader chords are a separate namespace, so `<leader>t` and `t` can both
/// be bound.
///
/// @param mode string Mode letter. Currently only `"n"` is accepted.
/// @param lhs string Key in Vim notation, e.g. `"<C-t>"`, `"<Space>"`, `"a"`,
///   `"<leader>d"`.
/// @param rhs function Called when the key is pressed.
/// @param opts table? Options:
///   `desc` (string) short description shown in the keymap list.
/// @example
/// caudra.keymap.set("n", "<C-t>", function()
///   print("toggle!")
/// end, { desc = "Toggle panel" })
/// caudra.keymap.set("n", "<leader>d", function()
///   print("Ctrl+X then d")
/// end, { desc = "Deploy" })
#[lua_fn]
fn set(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    mode: String,
    lhs: String,
    rhs: mlua::Function,
    opts: Option<Table>,
) -> LuaResult<()> {
    if mode != "n" {
        return Err(mlua::Error::runtime(format!(
            "unsupported keymap mode: {mode}"
        )));
    }
    let chord = parse_key_notation(&lhs).map_err(mlua::Error::runtime)?;
    let desc = opts
        .as_ref()
        .and_then(|o| o.get::<String>("desc").ok())
        .unwrap_or_default();
    let registry_key = lua.create_registry_value(rhs)?;
    let (_, old) = lua
        .app_data_mut::<KeymapStore>()
        .ok_or_else(|| mlua::Error::runtime("keymap store not initialized"))?
        .set(chord, registry_key, Arc::clone(&plugin), desc);
    if let Some(old_key) = old {
        tracing::warn!(key = %lhs, plugin = %plugin, "keymap shadowed by plugin");
        let _ = lua.remove_registry_value(old_key);
    }
    publish_keymap_snapshot(lua);
    Ok(())
}

/// Remove the mapping for {lhs} in {mode}. Does nothing if no mapping
/// exists for that key.
///
/// @param mode string Mode letter (reserved for future modes).
/// @param lhs string Key to unmap, in Vim notation.
/// @example
/// caudra.keymap.del("n", "<C-t>")
#[lua_fn]
fn del(lua: &Lua, #[ctx] plugin: Arc<str>, mode: String, lhs: String) -> LuaResult<()> {
    let _ = (mode, &plugin);
    let chord = parse_key_notation(&lhs).map_err(mlua::Error::runtime)?;
    let old = lua
        .app_data_mut::<KeymapStore>()
        .and_then(|mut store| store.del(chord));
    if let Some(old_key) = old {
        let _ = lua.remove_registry_value(old_key);
    }
    publish_keymap_snapshot(lua);
    Ok(())
}

lua_table! {
    /// Key mappings, modeled after `vim.keymap`. If you have written a
    /// Neovim keymap plugin before, this will feel familiar.
    ///
    /// ```lua
    /// caudra.keymap.set("n", "<C-t>", function()
    ///   print("hello")
    /// end, { desc = "Say hello" })
    /// ```
    "caudra.keymap" => pub(crate) fn create_keymap_table(plugin: Arc<str>), DOCS [
        set(plugin), del(plugin),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use test_case::test_case;

    const PLAIN_NOTATION_IS_NOT_A_CHORD: &str = "only <leader> asks for the leader prefix";
    const LEADER_NOTATION_IS_A_CHORD: &str = "<leader> must bind under the leader, not bare";
    const SEPARATE_NAMESPACES: &str = "a leader chord and the bare key are different bindings";

    fn chord(key: KeyCode, modifiers: KeyModifiers) -> KeyChord {
        KeyChord {
            key,
            modifiers,
            leader: false,
        }
    }

    #[test_case("<C-t>", KeyCode::Char('t'), KeyModifiers::CONTROL ; "ctrl_t")]
    #[test_case("<C-T>", KeyCode::Char('T'), KeyModifiers::CONTROL ; "ctrl_shift_t")]
    #[test_case("<A-x>", KeyCode::Char('x'), KeyModifiers::ALT ; "alt_x")]
    #[test_case("<M-x>", KeyCode::Char('x'), KeyModifiers::ALT ; "meta_x")]
    #[test_case("<S-Tab>", KeyCode::Tab, KeyModifiers::SHIFT ; "shift_tab")]
    #[test_case("<CR>", KeyCode::Enter, KeyModifiers::NONE ; "enter_cr")]
    #[test_case("<Enter>", KeyCode::Enter, KeyModifiers::NONE ; "enter_full")]
    #[test_case("<Space>", KeyCode::Char(' '), KeyModifiers::NONE ; "space")]
    #[test_case("<Esc>", KeyCode::Esc, KeyModifiers::NONE ; "escape")]
    #[test_case("<Tab>", KeyCode::Tab, KeyModifiers::NONE ; "tab")]
    #[test_case("<BS>", KeyCode::Backspace, KeyModifiers::NONE ; "backspace_short")]
    #[test_case("<Backspace>", KeyCode::Backspace, KeyModifiers::NONE ; "backspace_full")]
    #[test_case("<Del>", KeyCode::Delete, KeyModifiers::NONE ; "delete_short")]
    #[test_case("<Delete>", KeyCode::Delete, KeyModifiers::NONE ; "delete_full")]
    #[test_case("<Up>", KeyCode::Up, KeyModifiers::NONE ; "up")]
    #[test_case("<Down>", KeyCode::Down, KeyModifiers::NONE ; "down")]
    #[test_case("<Left>", KeyCode::Left, KeyModifiers::NONE ; "left")]
    #[test_case("<Right>", KeyCode::Right, KeyModifiers::NONE ; "right")]
    #[test_case("<Home>", KeyCode::Home, KeyModifiers::NONE ; "home")]
    #[test_case("<End>", KeyCode::End, KeyModifiers::NONE ; "end_key")]
    #[test_case("<PageUp>", KeyCode::PageUp, KeyModifiers::NONE ; "page_up")]
    #[test_case("<PageDown>", KeyCode::PageDown, KeyModifiers::NONE ; "page_down")]
    #[test_case("<Insert>", KeyCode::Insert, KeyModifiers::NONE ; "insert")]
    #[test_case("<F1>", KeyCode::F(1), KeyModifiers::NONE ; "f1")]
    #[test_case("<F12>", KeyCode::F(12), KeyModifiers::NONE ; "f12")]
    #[test_case("a", KeyCode::Char('a'), KeyModifiers::NONE ; "plain_a")]
    #[test_case("z", KeyCode::Char('z'), KeyModifiers::NONE ; "plain_z")]
    #[test_case("<C-S-a>", KeyCode::Char('a'), KeyModifiers::from_bits_truncate(KeyModifiers::CONTROL.bits() | KeyModifiers::SHIFT.bits()) ; "ctrl_shift_a")]
    #[test_case("<Ctrl-x>", KeyCode::Char('x'), KeyModifiers::CONTROL ; "ctrl_long_x")]
    #[test_case("<Alt-j>", KeyCode::Char('j'), KeyModifiers::ALT ; "alt_long_j")]
    #[test_case("<Shift-Tab>", KeyCode::Tab, KeyModifiers::SHIFT ; "shift_long_tab")]
    #[test_case("<Return>", KeyCode::Enter, KeyModifiers::NONE ; "return_key")]
    #[test_case("<Escape>", KeyCode::Esc, KeyModifiers::NONE ; "escape_full")]
    fn parse_key_notation_cases(input: &str, code: KeyCode, mods: KeyModifiers) {
        let chord = parse_key_notation(input).unwrap();
        assert_eq!(chord.key, code);
        assert_eq!(chord.modifiers, mods);
        assert!(!chord.leader, "{PLAIN_NOTATION_IS_NOT_A_CHORD}");
    }

    #[test_case("<leader>d", KeyCode::Char('d'), KeyModifiers::NONE ; "leader_letter")]
    #[test_case("<Leader><C-d>", KeyCode::Char('d'), KeyModifiers::CONTROL ; "leader_is_case_insensitive")]
    #[test_case("<leader><Tab>", KeyCode::Tab, KeyModifiers::NONE ; "leader_named_key")]
    fn leader_notation_binds_a_chord(input: &str, code: KeyCode, mods: KeyModifiers) {
        let chord = parse_key_notation(input).unwrap();
        assert_eq!(chord.key, code);
        assert_eq!(chord.modifiers, mods);
        assert!(chord.leader, "{LEADER_NOTATION_IS_A_CHORD}");
    }

    #[test]
    fn a_leader_chord_does_not_collide_with_the_bare_key() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        for lhs in ["d", "<leader>d"] {
            let f = lua.create_function(|_, ()| Ok(())).unwrap();
            let k = lua.create_registry_value(f).unwrap();
            let (_, old) = store.set(
                parse_key_notation(lhs).unwrap(),
                k,
                Arc::from("p"),
                String::new(),
            );
            assert!(old.is_none(), "{SEPARATE_NAMESPACES}");
        }

        assert_eq!(store.bindings.len(), 2, "{SEPARATE_NAMESPACES}");
    }

    #[test]
    fn parse_key_notation_errors() {
        assert!(parse_key_notation("").is_err());
        assert!(parse_key_notation("<>").is_err());
        assert!(parse_key_notation("<F0>").is_err());
        assert!(parse_key_notation("<F13>").is_err());
        assert!(parse_key_notation("abc").is_err());
        assert!(parse_key_notation("<leader>").is_err());
    }

    #[test]
    fn keymap_store_set_and_shadow() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        let f1 = lua.create_function(|_, ()| Ok(())).unwrap();
        let k1 = lua.create_registry_value(f1).unwrap();
        let (id1, old1) = store.set(
            chord(KeyCode::Char('t'), KeyModifiers::CONTROL),
            k1,
            Arc::from("plug"),
            "toggle".into(),
        );
        assert!(old1.is_none());

        let f2 = lua.create_function(|_, ()| Ok(())).unwrap();
        let k2 = lua.create_registry_value(f2).unwrap();
        let (id2, old2) = store.set(
            chord(KeyCode::Char('t'), KeyModifiers::CONTROL),
            k2,
            Arc::from("plug2"),
            "toggle v2".into(),
        );
        assert!(old2.is_some());
        assert_ne!(id1, id2);
        assert_eq!(store.bindings.len(), 1);
    }

    #[test]
    fn keymap_store_del() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let k = lua.create_registry_value(f).unwrap();
        store.set(
            chord(KeyCode::Char('x'), KeyModifiers::ALT),
            k,
            Arc::from("p"),
            String::new(),
        );
        assert_eq!(store.bindings.len(), 1);

        let removed = store.del(chord(KeyCode::Char('x'), KeyModifiers::ALT));
        assert!(removed.is_some());
        assert!(store.bindings.is_empty());

        let missing = store.del(chord(KeyCode::Char('x'), KeyModifiers::ALT));
        assert!(missing.is_none());
    }

    #[test]
    fn keymap_store_clear_plugin() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        let f1 = lua.create_function(|_, ()| Ok(())).unwrap();
        let f2 = lua.create_function(|_, ()| Ok(())).unwrap();
        let k1 = lua.create_registry_value(f1).unwrap();
        let k2 = lua.create_registry_value(f2).unwrap();
        store.set(
            chord(KeyCode::Char('t'), KeyModifiers::CONTROL),
            k1,
            Arc::from("a"),
            String::new(),
        );
        store.set(
            chord(KeyCode::Char('x'), KeyModifiers::CONTROL),
            k2,
            Arc::from("b"),
            String::new(),
        );

        let removed = store.clear_plugin("a");
        assert_eq!(removed.len(), 1);
        assert_eq!(store.bindings.len(), 1);
        assert_eq!(store.bindings[0].plugin.as_ref(), "b");
    }

    #[test]
    fn snapshot_reader_writer() {
        let (writer, reader) = KeymapWriter::new();
        assert!(reader.load().entries.is_empty());

        writer.publish(vec![KeymapEntry {
            key: KeyCode::Char('t'),
            modifiers: KeyModifiers::CONTROL,
            leader: false,
            desc: "test".into(),
            plugin: Arc::from("p"),
            id: 1,
        }]);

        let snap = reader.load();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.generation, 1);
    }
}
