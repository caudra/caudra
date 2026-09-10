use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

use caudra_providers::ModelPurpose;
use caudra_providers::dynamic;
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider::ProviderKind;

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::{Cadence, Dirty, Watch};
use crate::theme;

const TARGET_SECTION: &str = "Binding";
const DEFAULT_ROW: &str = "Default";
const DEFAULT_DETAIL: &str = "unbound";
const SAME_DETAIL: &str = "follows another slot";
const SAVED_DETAIL: &str = "saved exact model";
const RECENT_SECTION: &str = "Recent";
const FREE_LABEL: &str = "Free";
const DETAIL_SEPARATOR: &str = " · ";
const LOADING_MODELS: &str = "Loading models...";
const NO_MATCHES: &str = "No matches";

fn model_footer_line() -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::styled("  Enter", t.keybind_key),
        Span::styled(" select", t.tool_dim),
        Span::styled("  Tab/Shift+Tab", t.keybind_key),
        Span::styled(" purpose", t.tool_dim),
    ])
}

fn assignment_footer_line() -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::styled("  Enter", t.keybind_key),
        Span::styled(" assign", t.tool_dim),
        Span::styled("  R", t.keybind_key),
        Span::styled(" reset", t.tool_dim),
        Span::styled("  Tab/Shift+Tab", t.keybind_key),
        Span::styled(" purpose", t.tool_dim),
    ])
}

fn is_reset_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('R'))
        || (key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::SHIFT))
}

pub enum ModelPickerAction {
    Consumed,
    Select(String),
    Bind(ModelPurpose, Binding),
    Unbind(ModelPurpose),
    Close,
}

struct ModelEntry {
    spec: String,
    id: String,
    provider_display: String,
    suffix: Option<String>,
    detail: String,
    /// Every slot pointed at this row, so one accent cannot hide that a model
    /// serves several workloads.
    claimed_by: Vec<ModelPurpose>,
    /// What selecting this row binds the open purpose to. `None` unbinds.
    binds: Option<Binding>,
    selected: bool,
    free: bool,
}

impl PickerItem for ModelEntry {
    fn label(&self) -> &str {
        &self.id
    }

    fn suffix(&self) -> Option<&str> {
        self.suffix.as_deref()
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }

    fn section(&self) -> Option<&str> {
        Some(self.provider_display.as_str())
    }

    fn is_highlighted(&self) -> bool {
        !self.claimed_by.is_empty() || self.selected
    }
}

fn title_for(purpose: ModelPurpose) -> String {
    format!(" Models · {} ", purpose.label())
}

fn shifted(purpose: ModelPurpose, backwards: bool) -> ModelPurpose {
    let all = ModelPurpose::ALL;
    let index = all.iter().position(|p| *p == purpose).unwrap_or(0);
    let next = if backwards {
        index.checked_sub(1).unwrap_or(all.len() - 1)
    } else {
        (index + 1) % all.len()
    };
    all[next]
}

pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<Vec<String>>>,
    available: Watch<Vec<String>>,
    recents: Vec<String>,
    current_spec: String,
    purpose: ModelPurpose,
    binding: Option<Binding>,
    needs_rebuild: bool,
    /// User-moved entry to restore on refresh: `(was_recent, spec)`.
    anchor: Option<(bool, String)>,
}

impl ModelPicker {
    pub fn new(models: Arc<ArcSwapOption<Vec<String>>>) -> Self {
        Self {
            picker: ListPicker::new()
                .with_relevance_order()
                .with_footer_builder(model_footer_line),
            models,
            available: Watch::default(),
            recents: Vec::new(),
            current_spec: String::new(),
            purpose: ModelPurpose::Chat,
            binding: None,
            needs_rebuild: false,
            anchor: None,
        }
    }

    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
        self.needs_rebuild = true;
    }

    pub fn open(&mut self, current_spec: &str) {
        self.open_purpose(current_spec, ModelPurpose::Chat);
    }

    pub fn open_purpose(&mut self, current_spec: &str, purpose: ModelPurpose) {
        self.purpose = purpose;
        self.current_spec = current_spec.to_owned();
        self.binding = model_registry::binding(purpose);
        self.anchor = None;
        self.needs_rebuild = false;
        self.picker.set_footer_builder(self.footer_builder());
        let _ = self.available.poll(self.models.load_full());
        self.sync_empty_text();
        let entries = self.load_entries();
        self.picker.open(entries, title_for(purpose));
        self.preselect_purpose();
    }

    fn footer_builder(&self) -> fn() -> Line<'static> {
        if self.purpose == ModelPurpose::Chat {
            model_footer_line
        } else {
            assignment_footer_line
        }
    }

    /// Providers fetch their model lists in the background and drop them into
    /// a shared slot, which wakes nothing. An open picker has to notice on its
    /// own, so `App::tick` polls this instead of [`Self::view`] reading the
    /// slot mid render.
    pub fn refresh(&mut self) -> Dirty {
        if !self.picker.is_open() {
            return Dirty::NO;
        }
        let arrived = self.available.poll(self.models.load_full());
        if arrived == Dirty::NO && !self.needs_rebuild {
            return Dirty::NO;
        }
        self.needs_rebuild = false;
        self.sync_empty_text();
        let entries = self.load_entries();
        self.picker.replace_items(entries);
        if let Some((was_recent, spec)) = &self.anchor {
            self.picker
                .select_item_by(|e| e.spec == *spec && e.suffix().is_some() == *was_recent);
        } else {
            self.preselect_purpose();
        }
        Dirty::YES
    }

    fn sync_empty_text(&mut self) {
        self.picker
            .set_empty_text(if self.available.get().is_some() {
                NO_MATCHES
            } else {
                LOADING_MODELS
            });
    }

    fn load_entries(&self) -> Vec<ModelEntry> {
        let specs = self.available.get();
        let mut entries = if self.purpose == ModelPurpose::Chat {
            Vec::new()
        } else {
            binding_entries(self.purpose)
        };
        for entry in &mut entries {
            self.mark_selection(entry);
        }

        let mut recents = Vec::new();
        for spec in &self.recents {
            if let Some(mut e) = parse_model_entry(spec) {
                self.mark_selection(&mut e);
                e.suffix = Some(std::mem::take(&mut e.provider_display));
                e.provider_display = RECENT_SECTION.to_string();
                recents.push(e);
            }
        }
        let mut full: Vec<ModelEntry> = specs
            .map(|s| {
                s.iter()
                    .filter_map(|spec| {
                        let mut entry = parse_model_entry(spec)?;
                        self.mark_selection(&mut entry);
                        Some(entry)
                    })
                    .collect()
            })
            .unwrap_or_default();
        if self.purpose == ModelPurpose::Chat
            && !self.current_spec.is_empty()
            && !recents.iter().any(|entry| entry.spec == self.current_spec)
            && !full.iter().any(|entry| entry.spec == self.current_spec)
            && let Some(mut current) = parse_model_entry(&self.current_spec)
        {
            self.mark_selection(&mut current);
            full.push(current);
        }
        sort_models(&mut full);
        if let Some(spec) = self.bound_spec()
            && !recents.iter().any(|entry| entry.spec == spec)
            && !full.iter().any(|entry| entry.spec == spec)
        {
            entries.push(saved_binding_entry(spec));
        }
        entries.extend(recents);
        entries.extend(full);
        entries
    }

    /// Chat picks a session model, every other purpose picks a binding, so what
    /// counts as the current row differs.
    fn mark_selection(&self, entry: &mut ModelEntry) {
        entry.selected = if self.purpose == ModelPurpose::Chat {
            entry.spec == self.current_spec
        } else {
            entry.binds == self.binding
        };
    }

    fn bound_spec(&self) -> Option<&str> {
        match &self.binding {
            Some(Binding::Exact(spec)) => Some(spec),
            _ => None,
        }
    }

    fn preselect_purpose(&mut self) {
        if self.purpose == ModelPurpose::Chat {
            let spec = self.current_spec.clone();
            self.preselect_spec(&spec);
        } else if let Some(spec) = self.bound_spec().map(str::to_owned) {
            self.preselect_spec(&spec);
        } else {
            self.picker.select_item_by(|entry| entry.selected);
        }
    }

    fn preselect_spec(&mut self, spec: &str) {
        if !self
            .picker
            .select_item_by(|entry| entry.spec == spec && entry.suffix().is_none())
        {
            self.picker.select_item_by(|entry| entry.spec == spec);
        }
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    fn track_anchor<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let before = self.picker.selected_index();
        let result = f(self);
        if let (Some(before), Some(after)) = (before, self.picker.selected_index())
            && before != after
        {
            self.anchor = self
                .picker
                .selected_item()
                .map(|e| (e.suffix().is_some(), e.spec.clone()));
        }
        result
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.track_anchor(|p| p.picker.handle_paste(text))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        self.track_anchor(|p| p.handle_key_inner(key))
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> ModelPickerAction {
        self.track_anchor(|picker| {
            let action = picker.picker.handle_mouse(event);
            picker.map_picker_action(action)
        })
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> ModelPickerAction {
        let backwards = match key.code {
            KeyCode::BackTab => Some(true),
            KeyCode::Tab => Some(key.modifiers.contains(KeyModifiers::SHIFT)),
            _ => None,
        };
        if let Some(backwards) = backwards {
            self.purpose = shifted(self.purpose, backwards);
            self.binding = model_registry::binding(self.purpose);
            self.anchor = None;
            self.picker.set_title(title_for(self.purpose));
            self.picker.set_footer_builder(self.footer_builder());
            let entries = self.load_entries();
            self.picker.replace_items(entries);
            self.preselect_purpose();
            return ModelPickerAction::Consumed;
        }
        if is_reset_key(key) {
            self.anchor = None;
            self.picker.clear_search();
            self.needs_rebuild = true;
            if self.purpose == ModelPurpose::Chat {
                self.preselect_purpose();
                return ModelPickerAction::Consumed;
            }
            self.binding = None;
            return ModelPickerAction::Unbind(self.purpose);
        }
        let action = self.picker.handle_key(key);
        self.map_picker_action(action)
    }

    fn map_picker_action(&mut self, action: PickerAction<ModelEntry>) -> ModelPickerAction {
        match action {
            PickerAction::Consumed => ModelPickerAction::Consumed,
            PickerAction::Select(entry) if self.purpose == ModelPurpose::Chat => {
                ModelPickerAction::Select(entry.spec)
            }
            PickerAction::Select(entry) => {
                self.binding = entry.binds.clone();
                self.needs_rebuild = true;
                match entry.binds {
                    Some(binding) => ModelPickerAction::Bind(self.purpose, binding),
                    None => ModelPickerAction::Unbind(self.purpose),
                }
            }
            PickerAction::Close => ModelPickerAction::Close,
            PickerAction::Toggle(..) => ModelPickerAction::Consumed,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

/// Rows that bind a purpose to something other than one exact model: the unbound
/// default, plus every slot this purpose may follow.
fn binding_entries(purpose: ModelPurpose) -> Vec<ModelEntry> {
    std::iter::once(binding_row(DEFAULT_ROW, DEFAULT_DETAIL, None))
        .chain(
            ModelPurpose::CLASSES
                .into_iter()
                .filter(|target| *target != purpose)
                .map(|target| {
                    binding_row(target.label(), SAME_DETAIL, Some(Binding::Same(target)))
                }),
        )
        .collect()
}

fn binding_row(id: &str, detail: &str, binds: Option<Binding>) -> ModelEntry {
    let spec = match &binds {
        Some(Binding::Same(target)) => format!("@same:{target}"),
        _ => format!("@{DEFAULT_DETAIL}"),
    };
    ModelEntry {
        spec,
        id: id.to_string(),
        provider_display: TARGET_SECTION.into(),
        suffix: None,
        detail: detail.into(),
        claimed_by: Vec::new(),
        binds,
        selected: false,
        free: false,
    }
}

/// A bound spec the provider no longer lists still has to be visible, otherwise
/// the picker would show the purpose as unbound.
fn saved_binding_entry(spec: &str) -> ModelEntry {
    let id = spec
        .split_once('/')
        .map_or(spec, |(_, model_id)| model_id)
        .to_string();
    ModelEntry {
        spec: spec.to_string(),
        id,
        provider_display: TARGET_SECTION.into(),
        suffix: None,
        detail: SAVED_DETAIL.into(),
        claimed_by: Vec::new(),
        binds: Some(Binding::Exact(spec.to_string())),
        selected: true,
        free: false,
    }
}

fn sort_models(models: &mut [ModelEntry]) {
    models.sort_by(|a, b| {
        a.provider_display
            .cmp(&b.provider_display)
            .then_with(|| b.free.cmp(&a.free))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// The dim right-hand column: what the provider files this model as, then which
/// purposes point at it. A model nobody classified simply shows nothing there,
/// because the old tier label filled that gap with a guess.
fn row_detail(class: Option<ModelPurpose>, claimed_by: &[ModelPurpose], free: bool) -> String {
    let mut parts = Vec::new();
    if free {
        parts.push(FREE_LABEL.to_string());
    }
    if let Some(class) = class {
        parts.push(class.label().to_string());
    }
    if !claimed_by.is_empty() {
        parts.push(
            claimed_by
                .iter()
                .map(|purpose| purpose.as_str())
                .collect::<Vec<_>>()
                .join("/"),
        );
    }
    parts.join(DETAIL_SEPARATOR)
}

impl Overlay for ModelPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }
}

fn parse_model_entry(spec: &str) -> Option<ModelEntry> {
    let (provider_str, model_id) = spec.split_once('/')?;

    let provider_display = if let Ok(kind) = provider_str.parse::<ProviderKind>() {
        kind.display_name().to_string()
    } else if let Some(name) = dynamic::display_name(provider_str) {
        name.to_string()
    } else if let Some(info) = caudra_providers::catalog_provider_if_available(provider_str) {
        info.display_name.clone()
    } else if let Some(builtin) = caudra_config::providers::builtin_provider(provider_str) {
        builtin.display_name.to_string()
    } else {
        let config = caudra_config::providers::ProvidersConfig::load();
        config.get(provider_str)?;
        caudra_config::providers::resolve_display_name(provider_str, config.get(provider_str))
    };

    let claimed_by = model_registry::purposes_bound_to(spec);
    let free = caudra_providers::Model::from_spec(spec).is_ok_and(|model| model.is_free());
    let class = caudra_providers::Model::class_of(provider_str, model_id);
    Some(ModelEntry {
        spec: spec.to_string(),
        id: model_id.to_string(),
        provider_display,
        suffix: None,
        detail: row_detail(class, &claimed_by, free),
        claimed_by,
        binds: Some(Binding::Exact(spec.to_string())),
        selected: false,
        free,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::buffer_text;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use caudra_providers::ModelInfo;
    use caudra_providers::ModelPricing;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    const SAME_SIZED_LIST: &str = "a republished list of the same length is still a new list";
    const SWAPPED_SPEC: &str = "zai/glm-5";
    const SONNET_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const MISSING_SPEC: &str = "catalog-provider/vendor/model";
    const UNCLASSIFIED_SPEC: &str = "anthropic/claude-nothing-curated";
    const MODEL_QUERY: &str = "view";
    const BEST_MATCH_SPEC: &str = "anthropic/view";
    const WEAKER_MATCH_SPEC: &str = "zai/xxview";

    /// A provider that republishes the same number of specs has still changed
    /// the list. Comparing lengths calls that no change, and the picker goes on
    /// offering models that are gone.
    #[test]
    fn a_same_sized_model_list_owes_a_frame() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = ModelPicker::new(Arc::clone(&models));
        p.open("");
        assert_eq!(p.refresh(), Dirty::NO);

        models.store(Some(Arc::new(vec![SWAPPED_SPEC.into()])));
        assert_eq!(p.refresh(), Dirty::YES, "{SAME_SIZED_LIST}");
        assert_eq!(
            p.picker.selected_item().map(|e| e.spec.as_str()),
            Some(SWAPPED_SPEC),
            "{SAME_SIZED_LIST}"
        );
    }

    fn test_models() -> Arc<ArcSwapOption<Vec<String>>> {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        models
    }

    fn render(picker: &mut ModelPicker) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn empty_picker_distinguishes_loading_from_no_matches() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut picker = ModelPicker::new(Arc::clone(&models));
        picker.open("");
        assert!(render(&mut picker).contains(LOADING_MODELS));

        models.store(Some(Arc::new(Vec::new())));
        assert_eq!(picker.refresh(), Dirty::YES);

        assert!(render(&mut picker).contains(NO_MATCHES));
    }

    #[test_case(key(KeyCode::Esc)          ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event()    ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, ModelPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn refresh_updates_items_and_preserves_search() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = ModelPicker::new(models.clone());
        p.open("");

        p.handle_key(key(KeyCode::Char('o')));
        p.handle_key(key(KeyCode::Char('p')));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        let _ = p.refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s.contains("opus")),
            "after refresh, 'op' filter should match opus"
        );
    }

    #[test]
    fn open_preselects_current_model() {
        let mut p = ModelPicker::new(test_models());
        p.open("anthropic/claude-opus-4-6-20260101");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101")
        );
    }

    #[test]
    fn open_retains_current_model_missing_from_discovery() {
        let current = "anthropic/claude-sonnet-4-20250514";
        let models = Arc::new(ArcSwapOption::from_pointee(Vec::new()));
        let mut picker = ModelPicker::new(models);

        picker.open(current);
        let action = picker.handle_key(key(KeyCode::Enter));

        assert!(matches!(action, ModelPickerAction::Select(spec) if spec == current));
    }

    #[test]
    fn binding_picker_points_a_purpose_at_another_slot() {
        let mut p = ModelPicker::new(test_models());
        p.open_purpose("", ModelPurpose::Goal);
        p.handle_key(key(KeyCode::Down));

        let action = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            action,
            ModelPickerAction::Bind(ModelPurpose::Goal, Binding::Same(ModelPurpose::Fast))
        ));
    }

    #[test]
    fn binding_picker_binds_an_exact_model() {
        let mut p = ModelPicker::new(test_models());
        p.open_purpose("", ModelPurpose::Goal);
        p.picker
            .select_item_by(|entry| entry.spec == SWAPPED_SPEC && entry.suffix().is_none());

        let action = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            action,
            ModelPickerAction::Bind(ModelPurpose::Goal, Binding::Exact(spec)) if spec == SWAPPED_SPEC
        ));
    }

    /// A bound model the provider stopped listing still has to show up selected,
    /// or the purpose would read as unbound and the next Enter would change it.
    #[test]
    fn binding_picker_keeps_a_bound_model_missing_from_discovery() {
        let mut p = ModelPicker::new(test_models());
        p.open_purpose("", ModelPurpose::Goal);
        p.binding = Some(Binding::Exact(MISSING_SPEC.into()));
        p.needs_rebuild = true;

        assert_eq!(p.refresh(), Dirty::YES);
        let action = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            action,
            ModelPickerAction::Bind(ModelPurpose::Goal, Binding::Exact(spec)) if spec == MISSING_SPEC
        ));
    }

    /// A model row shows only its bare id, so a search has to surface the
    /// closest id rather than whichever provider the catalog happened to list
    /// first.
    #[test]
    fn search_surfaces_the_closest_model_id_first() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            WEAKER_MATCH_SPEC.into(),
            BEST_MATCH_SPEC.into(),
        ])));
        let mut p = ModelPicker::new(models);
        p.open("");
        for c in MODEL_QUERY.chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }

        assert_eq!(
            p.picker.selected_item().map(|e| e.spec.as_str()),
            Some(BEST_MATCH_SPEC)
        );
    }

    #[test]
    fn tab_switches_purpose_and_reset_unbinds() {
        let spec = "anthropic/claude-opus-4-6-20260101";
        let mut p = ModelPicker::new(test_models());
        p.open(spec);

        assert!(matches!(
            p.handle_key(key(KeyCode::Tab)),
            ModelPickerAction::Consumed
        ));
        p.picker
            .select_item_by(|entry| entry.spec == spec && entry.suffix().is_none());
        let assign = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            assign,
            ModelPickerAction::Bind(ModelPurpose::Fast, Binding::Exact(value)) if value == spec
        ));
        p.open_purpose(spec, ModelPurpose::Fast);
        let reset = p.handle_key(key(KeyCode::Char('R')));
        assert!(matches!(
            reset,
            ModelPickerAction::Unbind(ModelPurpose::Fast)
        ));
        assert!(p.is_open());
    }

    #[test]
    fn lowercase_g_remains_available_to_search() {
        let mut p = ModelPicker::new(test_models());
        p.open("anthropic/claude-opus-4-6-20260101");

        p.handle_key(key(KeyCode::Char('g')));
        p.handle_key(key(KeyCode::Char('l')));
        p.handle_key(key(KeyCode::Char('m')));
        let action = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            action,
            ModelPickerAction::Select(spec) if spec == SWAPPED_SPEC
        ));
    }

    #[test]
    fn parse_model_entry_valid() {
        let entry = parse_model_entry(SONNET_SPEC).unwrap();
        assert_eq!(entry.id, "claude-sonnet-4-20250514");
        assert_eq!(entry.provider_display, "Anthropic");
        assert_eq!(entry.binds, Some(Binding::Exact(SONNET_SPEC.into())));
    }

    /// The dim right-hand column carries the provider's own classification, so
    /// a row says what it is worth before you bind anything to it.
    #[test]
    fn a_curated_model_shows_its_class() {
        assert_eq!(
            parse_model_entry(SONNET_SPEC).unwrap().detail,
            ModelPurpose::Balanced.label()
        );
    }

    /// An unclassified model shows nothing rather than a guessed class.
    #[test]
    fn an_unclassified_model_shows_no_class() {
        assert!(
            parse_model_entry(UNCLASSIFIED_SPEC)
                .unwrap()
                .detail
                .is_empty()
        );
    }

    #[test]
    fn parse_model_entry_paid_model_not_marked_free() {
        let entry = parse_model_entry(SONNET_SPEC).unwrap();
        assert!(
            !entry.detail.starts_with(FREE_LABEL),
            "paid anthropic model must not be marked free"
        );
    }

    #[test]
    fn parse_model_entry_no_slash() {
        assert!(parse_model_entry("no-slash").is_none());
    }

    #[test_case(1, ModelPurpose::Fast     ; "fast")]
    #[test_case(2, ModelPurpose::Balanced ; "balanced")]
    #[test_case(3, ModelPurpose::Best     ; "best")]
    #[test_case(4, ModelPurpose::Title    ; "title")]
    #[test_case(5, ModelPurpose::Compact  ; "compact")]
    #[test_case(6, ModelPurpose::Goal     ; "goal")]
    fn tabbed_purpose_binds_selected_exact_model(tabs: usize, want: ModelPurpose) {
        let mut p = ModelPicker::new(test_models());
        p.open(SONNET_SPEC);
        for _ in 0..tabs {
            p.handle_key(key(KeyCode::Tab));
        }
        p.picker
            .select_item_by(|entry| entry.spec == SONNET_SPEC && entry.suffix().is_none());
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(&action, ModelPickerAction::Bind(purpose, Binding::Exact(spec))
                if spec == SONNET_SPEC && *purpose == want),
            "expected Bind({want:?}, {SONNET_SPEC}), got something else",
        );
        assert!(!p.is_open());
    }

    #[test]
    fn shift_tab_wraps_from_chat_to_the_last_purpose() {
        let mut p = ModelPicker::new(test_models());
        p.open(SONNET_SPEC);
        p.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        p.picker
            .select_item_by(|entry| entry.spec == SONNET_SPEC && entry.suffix().is_none());

        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            ModelPickerAction::Bind(ModelPurpose::Goal, Binding::Exact(spec)) if spec == SONNET_SPEC
        ));
    }

    #[test]
    fn uppercase_r_unbinds_while_lowercase_r_filters() {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        p.handle_key(key(KeyCode::Tab));

        assert!(matches!(
            p.handle_key(key(KeyCode::Char('r'))),
            ModelPickerAction::Consumed
        ));
        assert!(matches!(
            p.handle_key(key(KeyCode::Char('R'))),
            ModelPickerAction::Unbind(ModelPurpose::Fast)
        ));
        assert!(p.is_open());
    }

    #[test]
    fn uppercase_r_clears_chat_search_and_reselects_current_model() {
        let current = "anthropic/claude-sonnet-4-20250514";
        let mut p = ModelPicker::new(test_models());
        p.open(current);
        p.handle_paste("glm");

        assert!(matches!(
            p.handle_key(key(KeyCode::Char('R'))),
            ModelPickerAction::Consumed
        ));
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            ModelPickerAction::Select(spec) if spec == current
        ));
    }

    #[test]
    fn refresh_preserves_selection_for_current_model() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone());
        p.open("anthropic/claude-opus-4-6-20260101");

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101"),
            "after async model arrival, current model should still be selected"
        );
    }

    #[test]
    fn recents_include_current_model_preselected() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-opus-4-6-20260101");

        p.picker.select(0);
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "first entry should be the most recent model",
        );

        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("zai/glm-5");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "current model should be preselected in its provider section",
        );
    }

    #[test]
    fn reopen_preselects_current_model_in_provider_section() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        p.handle_key(key(KeyCode::Down));
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "selecting the provider entry should return its spec",
        );

        p.open("zai/glm-5");

        let entry = p.picker.selected_item().expect("selection on reopen");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(
            entry.section(),
            Some("Z.AI"),
            "selection should land on the provider entry, not the Recent copy",
        );
    }

    #[test]
    fn refresh_keeps_selection_on_provider_entry() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        p.handle_key(key(KeyCode::Down));
        p.needs_rebuild = true;

        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after refresh");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(
            entry.section(),
            Some("Z.AI"),
            "selection should stay on the provider entry, not jump to Recent",
        );
    }

    #[test]
    fn refresh_after_collapse_anchors_to_provider_entry() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");

        models.store(None);
        let _ = p.refresh();
        let entry = p.picker.selected_item().expect("selection during collapse");
        assert_eq!(entry.spec, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(entry.section(), Some("Recent"));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after arrival");
        assert_eq!(entry.spec, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(
            entry.section(),
            Some("Anthropic"),
            "cursor should migrate to the provider entry once it arrives",
        );
    }

    #[test]
    fn refresh_preserves_navigation_to_recent_entry() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        models.store(None);
        let _ = p.refresh();
        p.handle_key(key(KeyCode::Down));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after arrival");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(
            entry.section(),
            Some("Recent"),
            "user navigation to a Recent entry should survive refresh",
        );
    }

    #[test]
    fn refresh_preserves_selection_with_active_search() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        p.handle_paste("glm");

        models.store(None);
        let _ = p.refresh();
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after refresh");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(entry.section(), Some("Z.AI"));
    }

    fn discovered(id: &str, pricing: ModelPricing) -> ModelInfo {
        ModelInfo {
            pricing: Some(pricing),
            ..ModelInfo::id_only(id.into())
        }
    }

    const OX_SPEC: &str = "openrouter/stealth/ox-alpha";
    const PAID_ID: &str = "vendor/paid-model";
    const PAID_PRICING: ModelPricing = ModelPricing {
        tiers: Vec::new(),
        input: 3.0,
        output: 15.0,
        cache_write: 0.0,
        cache_read: 0.0,
        fast: None,
    };

    fn register_openrouter_models() {
        model_registry::set_known_models(
            "openrouter",
            vec![
                discovered("stealth/ox-alpha", ModelPricing::ZERO),
                discovered(PAID_ID, PAID_PRICING),
            ],
        );
    }

    #[test]
    fn zero_priced_discovery_marks_entry_free() {
        register_openrouter_models();
        let entry = parse_model_entry(OX_SPEC).unwrap();
        assert!(
            entry.detail.starts_with(FREE_LABEL),
            "zero-priced discovery must mark the entry free"
        );
    }

    #[test]
    fn paid_discovery_not_marked_free() {
        register_openrouter_models();
        let entry = parse_model_entry(&format!("openrouter/{PAID_ID}")).unwrap();
        assert!(
            !entry.detail.starts_with(FREE_LABEL),
            "paid discovery must not mark the entry free"
        );
    }

    #[test]
    fn free_models_sort_before_paid_within_a_provider() {
        register_openrouter_models();
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            format!("openrouter/{PAID_ID}"),
            OX_SPEC.into(),
        ])));
        let mut p = ModelPicker::new(models);
        p.open("");
        let entries = p.load_entries();
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["stealth/ox-alpha", PAID_ID]);
    }
}
