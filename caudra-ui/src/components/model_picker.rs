use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use caudra_config::ModelPolicy;
use caudra_providers::dynamic;
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider::ProviderKind;
use caudra_providers::{Model, ModelPurpose};

use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::{Hint, Overlay};
use crate::repaint::{Cadence, Dirty, Watch};

const HOME_TITLE: &str = " Models ";
const JOBS_SECTION: &str = "Jobs";
const TARGET_SECTION: &str = "Binding";
const DEFAULT_ROW: &str = "Default";
const DEFAULT_DETAIL: &str = "automatic resolution";
const SAME_PREFIX: &str = "Same as ";
const SAME_DETAIL: &str = "follow this job";
const SAVED_DETAIL: &str = "saved exact model";
const RECENT_SECTION: &str = "Recent";
const FREE_LABEL: &str = "Free";
const DETAIL_SEPARATOR: &str = " · ";
const LOADING_MODELS: &str = "Loading models...";
const NO_MATCHES: &str = "No matches";
const DEFAULT_BINDING: &str = "default";
const PINNED_BINDING: &str = "pinned";
const UNAVAILABLE_PREFIX: &str = "Unavailable: ";
const JOB_KEY_PREFIX: &str = "@job:";
const PICKER_WIDTH_PERCENT: u16 = 90;
const UNBIND_LABEL: &str = "R";

fn home_footer_line() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "select/open"),
        Hint::bind(key::ESC, "close"),
    ]
}

fn assignment_footer_line() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "assign"),
        Hint::char(UNBIND_LABEL, "unbind"),
        Hint::bind(key::ESC, "back"),
    ]
}

fn is_reset_key(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('R') => key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT,
        KeyCode::Char('r') => key.modifiers == KeyModifiers::SHIFT,
        _ => false,
    }
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
    search_text: String,
    claimed_by: Vec<ModelPurpose>,
    binds: Option<Binding>,
    job: Option<ModelPurpose>,
    selected: bool,
    free: bool,
}

impl ModelEntry {
    fn rebuild_search_text(&mut self) {
        self.search_text = format!(
            "{} {} {} {} {}",
            self.id,
            self.spec,
            self.provider_display,
            self.suffix.as_deref().unwrap_or_default(),
            self.detail
        );
    }

    fn with_search_text(mut self) -> Self {
        self.rebuild_search_text();
        self
    }

    fn identity(&self) -> RowIdentity {
        if let Some(purpose) = self.job {
            return RowIdentity::Job(purpose);
        }
        if self.suffix.is_some() {
            return RowIdentity::RecentModel(self.spec.clone());
        }
        match &self.binds {
            Some(Binding::Same(target)) => RowIdentity::Binding(Some(*target)),
            Some(Binding::Exact(_)) => RowIdentity::ExactModel(self.spec.clone()),
            None => RowIdentity::Binding(None),
        }
    }
}

impl PickerItem for ModelEntry {
    fn label(&self) -> &str {
        &self.id
    }

    fn search_text(&self) -> &str {
        &self.search_text
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerPage {
    Home,
    Job(ModelPurpose),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RowIdentity {
    Job(ModelPurpose),
    Binding(Option<ModelPurpose>),
    RecentModel(String),
    ExactModel(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectionAnchor {
    page: PickerPage,
    row: RowIdentity,
}

pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<Vec<String>>>,
    available: Watch<Vec<String>>,
    recents: Vec<String>,
    current_model: Option<Model>,
    model_policy: Option<ModelPolicy>,
    page: PickerPage,
    binding: Option<Binding>,
    needs_rebuild: bool,
    anchor: Option<SelectionAnchor>,
}

impl ModelPicker {
    pub fn new(models: Arc<ArcSwapOption<Vec<String>>>) -> Self {
        Self {
            picker: ListPicker::new()
                .with_relevance_order()
                .with_width_percent(PICKER_WIDTH_PERCENT)
                .with_footer_builder(home_footer_line),
            models,
            available: Watch::default(),
            recents: Vec::new(),
            current_model: None,
            model_policy: None,
            page: PickerPage::Home,
            binding: None,
            needs_rebuild: false,
            anchor: None,
        }
    }

    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
        self.needs_rebuild = true;
    }

    pub fn open(&mut self, current_model: &Model, model_policy: &ModelPolicy) {
        self.current_model = Some(current_model.clone());
        self.model_policy = Some(model_policy.clone());
        self.anchor = None;
        self.needs_rebuild = false;
        let _ = self.available.poll(self.models.load_full());
        self.sync_empty_text();
        self.open_home_page();
    }

    pub fn open_purpose(
        &mut self,
        current_model: &Model,
        model_policy: &ModelPolicy,
        purpose: ModelPurpose,
    ) {
        self.open(current_model, model_policy);
        if purpose != ModelPurpose::Chat {
            self.open_job_page(purpose);
        }
    }

    fn open_home_page(&mut self) {
        self.page = PickerPage::Home;
        self.binding = None;
        self.picker.set_footer_builder(home_footer_line);
        let entries = self.load_entries();
        self.picker.open(entries, HOME_TITLE);
        self.preselect_page();
        self.capture_anchor();
    }

    fn open_job_page(&mut self, purpose: ModelPurpose) {
        self.page = PickerPage::Job(purpose);
        self.binding = model_registry::binding(purpose);
        self.anchor = None;
        self.picker.set_footer_builder(assignment_footer_line);
        let entries = self.load_entries();
        self.picker.open(entries, title_for(purpose));
        self.preselect_page();
        self.capture_anchor();
    }

    fn return_home(&mut self, purpose: ModelPurpose) {
        self.anchor = None;
        self.open_home_page();
        self.picker
            .select_item_by(|entry| entry.job == Some(purpose));
        self.capture_anchor();
    }

    fn select_current_model_row(&mut self) {
        if !self.picker.is_open() {
            self.open_home_page();
        }
        if let Some(spec) = self.current_model.as_ref().map(Model::spec) {
            self.preselect_spec(&spec);
            self.capture_anchor();
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
        if let PickerPage::Job(purpose) = self.page {
            self.binding = model_registry::binding(purpose);
        }
        self.sync_empty_text();
        let entries = self.load_entries();
        self.picker.replace_items(entries);
        if !self.restore_anchor() {
            self.preselect_page();
        }
        self.capture_anchor();
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
        let current_spec = self
            .current_model
            .as_ref()
            .map(Model::spec)
            .unwrap_or_default();
        let mut entries = match self.page {
            PickerPage::Home => self.job_entries(),
            PickerPage::Job(purpose) => binding_entries(purpose),
        };
        for entry in &mut entries {
            self.mark_selection(entry, &current_spec);
        }

        let mut recents = Vec::new();
        for spec in &self.recents {
            if let Some(mut e) = parse_model_entry(spec) {
                self.mark_selection(&mut e, &current_spec);
                e.suffix = Some(std::mem::take(&mut e.provider_display));
                e.provider_display = RECENT_SECTION.to_string();
                e.rebuild_search_text();
                recents.push(e);
            }
        }
        let mut full: Vec<ModelEntry> = specs
            .map(|s| {
                s.iter()
                    .filter_map(|spec| {
                        let mut entry = parse_model_entry(spec)?;
                        self.mark_selection(&mut entry, &current_spec);
                        Some(entry)
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !current_spec.is_empty()
            && !recents.iter().any(|entry| entry.spec == current_spec)
            && !full.iter().any(|entry| entry.spec == current_spec)
            && let Some(mut current) = parse_model_entry(&current_spec)
        {
            self.mark_selection(&mut current, &current_spec);
            full.push(current);
        }
        sort_models(&mut full);
        if matches!(self.page, PickerPage::Job(_))
            && let Some(spec) = self.bound_spec()
            && !recents.iter().any(|entry| entry.spec == spec)
            && !full.iter().any(|entry| entry.spec == spec)
        {
            entries.push(saved_binding_entry(spec));
        }
        entries.extend(recents);
        entries.extend(full);
        entries
    }

    fn job_entries(&self) -> Vec<ModelEntry> {
        let (Some(current_model), Some(model_policy)) =
            (self.current_model.as_ref(), self.model_policy.as_ref())
        else {
            return Vec::new();
        };
        ModelPurpose::ALL
            .into_iter()
            .map(|purpose| job_entry(purpose, current_model, model_policy))
            .collect()
    }

    fn mark_selection(&self, entry: &mut ModelEntry, current_spec: &str) {
        entry.selected = match self.page {
            PickerPage::Home => entry.job.is_none() && entry.spec == current_spec,
            PickerPage::Job(_) => entry.job.is_none() && entry.binds == self.binding,
        };
    }

    fn bound_spec(&self) -> Option<&str> {
        match &self.binding {
            Some(Binding::Exact(spec)) => Some(spec),
            _ => None,
        }
    }

    fn preselect_page(&mut self) {
        match self.page {
            PickerPage::Home => {
                self.picker
                    .select_item_by(|entry| entry.job == Some(ModelPurpose::Chat));
            }
            PickerPage::Job(_) => {
                if let Some(spec) = self.bound_spec().map(str::to_owned) {
                    self.preselect_spec(&spec);
                } else {
                    self.picker.select_item_by(|entry| entry.selected);
                }
            }
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
        self.capture_anchor();
    }

    fn track_anchor<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let result = f(self);
        self.capture_anchor();
        result
    }

    fn capture_anchor(&mut self) {
        self.anchor = self.picker.selected_item().map(|entry| SelectionAnchor {
            page: self.page,
            row: entry.identity(),
        });
    }

    fn restore_anchor(&mut self) -> bool {
        let Some(anchor) = self
            .anchor
            .clone()
            .filter(|anchor| anchor.page == self.page)
        else {
            return false;
        };
        self.picker
            .select_item_by(|entry| entry.identity() == anchor.row)
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
        if key.code == KeyCode::Esc
            && let PickerPage::Job(purpose) = self.page
        {
            self.return_home(purpose);
            return ModelPickerAction::Consumed;
        }
        if is_reset_key(key)
            && let PickerPage::Job(purpose) = self.page
        {
            self.binding = None;
            self.anchor = None;
            self.picker.clear_search();
            let entries = self.load_entries();
            self.picker.replace_items(entries);
            self.preselect_page();
            self.needs_rebuild = true;
            return ModelPickerAction::Unbind(purpose);
        }
        let action = self.picker.handle_key(key);
        self.map_picker_action(action)
    }

    fn map_picker_action(&mut self, action: PickerAction<ModelEntry>) -> ModelPickerAction {
        match action {
            PickerAction::Consumed => ModelPickerAction::Consumed,
            PickerAction::Select(entry) => match self.page {
                PickerPage::Home => match entry.job {
                    Some(purpose) => {
                        if purpose == ModelPurpose::Chat {
                            self.select_current_model_row();
                        } else {
                            self.open_job_page(purpose);
                        }
                        ModelPickerAction::Consumed
                    }
                    None => ModelPickerAction::Select(entry.spec),
                },
                PickerPage::Job(purpose) => {
                    self.binding = entry.binds.clone();
                    self.needs_rebuild = true;
                    match entry.binds {
                        Some(binding) => ModelPickerAction::Bind(purpose, binding),
                        None => ModelPickerAction::Unbind(purpose),
                    }
                }
            },
            PickerAction::Close => ModelPickerAction::Close,
            PickerAction::Toggle(..) => ModelPickerAction::Consumed,
            PickerAction::Key(key) => self.handle_key_inner(key),
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

fn job_entry(
    purpose: ModelPurpose,
    current_model: &Model,
    model_policy: &ModelPolicy,
) -> ModelEntry {
    let binding = model_registry::binding(purpose);
    job_entry_with_binding(purpose, binding, current_model, model_policy)
}

fn job_entry_with_binding(
    purpose: ModelPurpose,
    binding: Option<Binding>,
    current_model: &Model,
    model_policy: &ModelPolicy,
) -> ModelEntry {
    let suffix = match binding.as_ref() {
        None => DEFAULT_BINDING.to_string(),
        Some(Binding::Same(target)) => format!("same as {}", target.label()),
        Some(Binding::Exact(_)) => PINNED_BINDING.to_string(),
    };
    let detail = match Model::resolve_binding_if_available(
        purpose,
        binding.as_ref(),
        current_model,
        model_policy,
    ) {
        Ok(model) => model.spec(),
        Err(error) => {
            let unavailable = format!("{UNAVAILABLE_PREFIX}{error}");
            match binding.as_ref() {
                Some(Binding::Exact(spec)) => {
                    format!("{spec}{DETAIL_SEPARATOR}{unavailable}")
                }
                _ => unavailable,
            }
        }
    };
    ModelEntry {
        spec: format!("{JOB_KEY_PREFIX}{purpose}"),
        id: purpose.label().to_string(),
        provider_display: JOBS_SECTION.to_string(),
        suffix: Some(suffix),
        detail,
        search_text: String::new(),
        claimed_by: Vec::new(),
        binds: binding,
        job: Some(purpose),
        selected: false,
        free: false,
    }
    .with_search_text()
}

/// Rows that bind a purpose to something other than one exact model: the unbound
/// default, plus every slot this purpose may follow.
fn binding_entries(purpose: ModelPurpose) -> Vec<ModelEntry> {
    std::iter::once(binding_row(DEFAULT_ROW, DEFAULT_DETAIL, None))
        .chain(
            ModelPurpose::TARGETS
                .into_iter()
                .filter(|target| !model_registry::binding_would_cycle(purpose, *target))
                .map(|target| {
                    let label = format!("{SAME_PREFIX}{}", target.label());
                    binding_row(&label, SAME_DETAIL, Some(Binding::Same(target)))
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
        search_text: String::new(),
        claimed_by: Vec::new(),
        binds,
        job: None,
        selected: false,
        free: false,
    }
    .with_search_text()
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
        search_text: String::new(),
        claimed_by: Vec::new(),
        binds: Some(Binding::Exact(spec.to_string())),
        job: None,
        selected: true,
        free: false,
    }
    .with_search_text()
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
    Some(
        ModelEntry {
            spec: spec.to_string(),
            id: model_id.to_string(),
            provider_display,
            suffix: None,
            detail: row_detail(class, &claimed_by, free),
            search_text: String::new(),
            claimed_by,
            binds: Some(Binding::Exact(spec.to_string())),
            job: None,
            selected: false,
            free,
        }
        .with_search_text(),
    )
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
    const OPUS_SPEC: &str = "anthropic/claude-opus-4-6-20260101";
    const MISSING_SPEC: &str = "catalog-provider/vendor/model";
    const UNCLASSIFIED_SPEC: &str = "anthropic/claude-nothing-curated";
    const MODEL_QUERY: &str = "view";
    const BEST_MATCH_SPEC: &str = "anthropic/view";
    const WEAKER_MATCH_SPEC: &str = "zai/xxview";
    const NO_MODEL_QUERY: &str = "zzzz-no-model";
    const SHARED_MODEL_ID: &str = "shared-model";
    const ANTHROPIC_SHARED_SPEC: &str = "anthropic/shared-model";
    const ZAI_SHARED_SPEC: &str = "zai/shared-model";
    const ASYNC_SPEC: &str = "anthropic/search-only-model";

    fn current_model(spec: &str) -> Model {
        Model::from_spec(spec).unwrap()
    }

    fn open_picker(picker: &mut ModelPicker, spec: &str) {
        picker.open(&current_model(spec), &ModelPolicy::default());
    }

    fn open_job_picker(picker: &mut ModelPicker, spec: &str, purpose: ModelPurpose) {
        picker.open_purpose(&current_model(spec), &ModelPolicy::default(), purpose);
    }

    fn navigate_to_model(picker: &mut ModelPicker, spec: &str, section: &str) {
        let selected = picker.track_anchor(|picker| {
            picker
                .picker
                .select_item_by(|entry| entry.spec == spec && entry.section() == Some(section))
        });
        assert!(selected);
    }

    /// A provider that republishes the same number of specs has still changed
    /// the list. Comparing lengths calls that no change, and the picker goes on
    /// offering models that are gone.
    #[test]
    fn a_same_sized_model_list_owes_a_frame() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![OPUS_SPEC.into()])));
        let mut p = ModelPicker::new(Arc::clone(&models));
        open_picker(&mut p, SONNET_SPEC);
        assert_eq!(p.refresh(), Dirty::NO);

        models.store(Some(Arc::new(vec![SWAPPED_SPEC.into()])));
        assert_eq!(p.refresh(), Dirty::YES, "{SAME_SIZED_LIST}");
        let entries = p.load_entries();
        assert!(entries.iter().any(|entry| entry.spec == SWAPPED_SPEC));
        assert!(
            !entries.iter().any(|entry| entry.spec == OPUS_SPEC),
            "{SAME_SIZED_LIST}"
        );
    }

    fn test_models() -> Arc<ArcSwapOption<Vec<String>>> {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            SONNET_SPEC.into(),
            OPUS_SPEC.into(),
            SWAPPED_SPEC.into(),
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
        open_picker(&mut picker, SONNET_SPEC);
        picker.handle_paste(NO_MODEL_QUERY);
        assert!(render(&mut picker).contains(LOADING_MODELS));

        models.store(Some(Arc::new(Vec::new())));
        assert_eq!(picker.refresh(), Dirty::YES);

        assert!(render(&mut picker).contains(NO_MATCHES));
    }

    #[test_case(key(KeyCode::Esc)          ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event()    ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let mut p = ModelPicker::new(test_models());
        open_picker(&mut p, SONNET_SPEC);
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, ModelPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn refresh_updates_items_and_preserves_search() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![SONNET_SPEC.into()])));
        let mut p = ModelPicker::new(models.clone());
        open_picker(&mut p, SONNET_SPEC);

        p.handle_paste("search-only");

        models.store(Some(Arc::new(vec![SONNET_SPEC.into(), ASYNC_SPEC.into()])));
        let _ = p.refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == ASYNC_SPEC),
            "after refresh, the active filter should match the arrived model"
        );
    }

    #[test]
    fn home_selects_chat_and_marks_the_current_model() {
        let mut p = ModelPicker::new(test_models());
        open_picker(&mut p, OPUS_SPEC);

        assert_eq!(
            p.picker.selected_item().and_then(|entry| entry.job),
            Some(ModelPurpose::Chat)
        );
        let current = p
            .load_entries()
            .into_iter()
            .find(|entry| entry.spec == OPUS_SPEC && entry.suffix().is_none())
            .unwrap();
        assert!(current.selected);
    }

    #[test]
    fn open_retains_current_model_missing_from_discovery() {
        let models = Arc::new(ArcSwapOption::from_pointee(Vec::new()));
        let mut picker = ModelPicker::new(models);

        open_picker(&mut picker, SONNET_SPEC);
        navigate_to_model(&mut picker, SONNET_SPEC, "Anthropic");
        let action = picker.handle_key(key(KeyCode::Enter));

        assert!(matches!(action, ModelPickerAction::Select(spec) if spec == SONNET_SPEC));
    }

    #[test]
    fn home_starts_with_resolved_jobs_in_purpose_order() {
        let mut p = ModelPicker::new(test_models());
        open_picker(&mut p, SONNET_SPEC);

        let entries = p.load_entries();
        let jobs: Vec<_> = entries.iter().filter_map(|entry| entry.job).collect();
        assert_eq!(jobs, ModelPurpose::ALL);

        let chat = entries
            .iter()
            .find(|entry| entry.job == Some(ModelPurpose::Chat))
            .unwrap();
        assert_eq!(chat.section(), Some(JOBS_SECTION));
        assert_eq!(chat.suffix(), Some(DEFAULT_BINDING));
        assert_eq!(chat.detail(), Some(SONNET_SPEC));

        let screen = render(&mut p);
        assert!(screen.contains(JOBS_SECTION));
        assert!(screen.contains("Chat"));
        assert!(screen.contains(DEFAULT_BINDING));
        assert!(screen.contains(SONNET_SPEC));
    }

    #[test]
    fn job_rows_show_direct_binding_and_resolution_errors() {
        let current = current_model(SONNET_SPEC);
        let pinned = job_entry_with_binding(
            ModelPurpose::Goal,
            Some(Binding::Exact(OPUS_SPEC.into())),
            &current,
            &ModelPolicy::default(),
        );
        assert_eq!(pinned.suffix(), Some(PINNED_BINDING));
        assert_eq!(pinned.detail(), Some(OPUS_SPEC));

        let policy = ModelPolicy::new(&[], &[SONNET_SPEC.to_string()]).unwrap();
        let entry = job_entry_with_binding(
            ModelPurpose::Goal,
            Some(Binding::Exact(SONNET_SPEC.into())),
            &current,
            &policy,
        );

        assert_eq!(entry.suffix(), Some(PINNED_BINDING));
        assert!(entry.detail.starts_with(SONNET_SPEC));
        assert!(entry.detail.contains(UNAVAILABLE_PREFIX));

        let same = job_entry_with_binding(
            ModelPurpose::Goal,
            Some(Binding::Same(ModelPurpose::Fast)),
            &current,
            &ModelPolicy::default(),
        );
        assert_eq!(same.suffix(), Some("same as Fast"));
    }

    #[test]
    fn non_chat_job_drills_down_and_escape_returns_to_selected_job() {
        let mut p = ModelPicker::new(test_models());
        open_picker(&mut p, SONNET_SPEC);
        p.picker
            .select_item_by(|entry| entry.job == Some(ModelPurpose::Plan));

        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            ModelPickerAction::Consumed
        ));
        assert_eq!(p.page, PickerPage::Job(ModelPurpose::Plan));

        assert!(matches!(
            p.handle_key(key(KeyCode::Esc)),
            ModelPickerAction::Consumed
        ));
        assert_eq!(p.page, PickerPage::Home);
        assert_eq!(
            p.picker.selected_item().and_then(|entry| entry.job),
            Some(ModelPurpose::Plan)
        );

        assert!(matches!(
            p.handle_key(key(KeyCode::Esc)),
            ModelPickerAction::Close
        ));
        assert!(!p.is_open());
    }

    #[test]
    fn chat_job_enter_moves_to_the_current_exact_model() {
        let mut p = ModelPicker::new(test_models());
        open_picker(&mut p, SONNET_SPEC);
        p.picker
            .select_item_by(|entry| entry.job == Some(ModelPurpose::Chat));

        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            ModelPickerAction::Consumed
        ));
        assert_eq!(p.page, PickerPage::Home);
        assert!(p.is_open());
        let selected = p.picker.selected_item().unwrap();
        assert_eq!(selected.job, None);
        assert_eq!(selected.spec, SONNET_SPEC);
        assert_eq!(selected.section(), Some("Anthropic"));
    }

    #[test]
    fn purpose_open_has_home_beneath_it() {
        let mut p = ModelPicker::new(test_models());
        open_job_picker(&mut p, SONNET_SPEC, ModelPurpose::Goal);
        assert_eq!(p.page, PickerPage::Job(ModelPurpose::Goal));

        assert!(matches!(
            p.handle_key(key(KeyCode::Esc)),
            ModelPickerAction::Consumed
        ));
        assert_eq!(p.page, PickerPage::Home);
        assert_eq!(
            p.picker.selected_item().and_then(|entry| entry.job),
            Some(ModelPurpose::Goal)
        );
    }

    #[test]
    fn job_to_home_selection_survives_same_index_refresh() {
        let models = test_models();
        let mut p = ModelPicker::new(Arc::clone(&models));
        open_job_picker(&mut p, SONNET_SPEC, ModelPurpose::Goal);
        for _ in 0..5 {
            p.handle_key(key(KeyCode::Down));
        }
        assert_eq!(p.picker.selected_index(), Some(5));

        p.handle_key(key(KeyCode::Esc));
        assert_eq!(p.picker.selected_index(), Some(5));
        models.store(Some(Arc::new(vec![
            SONNET_SPEC.into(),
            OPUS_SPEC.into(),
            SWAPPED_SPEC.into(),
        ])));

        assert_eq!(p.refresh(), Dirty::YES);
        assert_eq!(
            p.picker.selected_item().and_then(|entry| entry.job),
            Some(ModelPurpose::Goal)
        );
    }

    #[test]
    fn scroll_anchors_the_selected_row_for_refresh() {
        let mut p = ModelPicker::new(test_models());
        open_picker(&mut p, SONNET_SPEC);
        p.picker
            .select_item_by(|entry| entry.job == Some(ModelPurpose::Plan));

        p.scroll(-1);
        p.needs_rebuild = true;

        assert_eq!(p.refresh(), Dirty::YES);
        assert_eq!(
            p.picker.selected_item().and_then(|entry| entry.job),
            Some(ModelPurpose::Plan)
        );
    }

    #[test]
    fn binding_picker_points_a_job_at_another_target() {
        let mut p = ModelPicker::new(test_models());
        open_job_picker(&mut p, SONNET_SPEC, ModelPurpose::Goal);
        p.picker
            .select_item_by(|entry| entry.binds == Some(Binding::Same(ModelPurpose::Chat)));

        let action = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            action,
            ModelPickerAction::Bind(ModelPurpose::Goal, Binding::Same(ModelPurpose::Chat))
        ));
    }

    #[test]
    fn exact_model_search_on_job_page_binds_the_model() {
        let mut p = ModelPicker::new(test_models());
        open_job_picker(&mut p, SONNET_SPEC, ModelPurpose::Goal);
        p.handle_paste("glm");

        let action = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            action,
            ModelPickerAction::Bind(ModelPurpose::Goal, Binding::Exact(spec)) if spec == SWAPPED_SPEC
        ));
    }

    #[test]
    fn current_model_is_an_exact_job_candidate_without_discovery() {
        let models = Arc::new(ArcSwapOption::from_pointee(Vec::new()));
        let mut p = ModelPicker::new(models);
        open_job_picker(&mut p, SONNET_SPEC, ModelPurpose::Goal);
        navigate_to_model(&mut p, SONNET_SPEC, "Anthropic");

        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            ModelPickerAction::Bind(ModelPurpose::Goal, Binding::Exact(spec))
                if spec == SONNET_SPEC
        ));
    }

    /// A bound model the provider stopped listing still has to show up selected,
    /// or the purpose would read as unbound and the next Enter would change it.
    #[test]
    fn binding_picker_keeps_a_bound_model_missing_from_discovery() {
        let mut p = ModelPicker::new(test_models());
        open_job_picker(&mut p, SONNET_SPEC, ModelPurpose::Goal);
        p.binding = Some(Binding::Exact(MISSING_SPEC.into()));
        p.picker.replace_items(p.load_entries());
        p.preselect_page();
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
        open_picker(&mut p, SONNET_SPEC);
        for c in MODEL_QUERY.chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }

        assert_eq!(
            p.picker.selected_item().map(|e| e.spec.as_str()),
            Some(BEST_MATCH_SPEC)
        );
    }

    #[test]
    fn search_accepts_a_qualified_documented_model_spec() {
        let mut p = ModelPicker::new(test_models());
        open_picker(&mut p, OPUS_SPEC);

        p.handle_paste(SONNET_SPEC);

        let selected = p.picker.selected_item().unwrap();
        assert_eq!(selected.spec, SONNET_SPEC);
        assert_eq!(selected.label(), "claude-sonnet-4-20250514");
    }

    #[test]
    fn qualified_search_disambiguates_identical_model_ids() {
        let models = Arc::new(ArcSwapOption::from_pointee(vec![
            ANTHROPIC_SHARED_SPEC.into(),
            ZAI_SHARED_SPEC.into(),
        ]));
        let mut p = ModelPicker::new(models);
        open_picker(&mut p, OPUS_SPEC);

        p.handle_paste(ZAI_SHARED_SPEC);

        let selected = p.picker.selected_item().unwrap();
        assert_eq!(selected.spec, ZAI_SHARED_SPEC);
        assert_eq!(selected.label(), SHARED_MODEL_ID);
    }

    #[test]
    fn search_accepts_provider_display_name() {
        let mut p = ModelPicker::new(test_models());
        open_picker(&mut p, OPUS_SPEC);

        p.handle_paste("Z.AI");

        assert_eq!(
            p.picker.selected_item().map(|entry| entry.spec.as_str()),
            Some(SWAPPED_SPEC)
        );
    }

    #[test]
    fn tab_and_shift_tab_do_not_change_pages() {
        let mut p = ModelPicker::new(test_models());
        open_picker(&mut p, OPUS_SPEC);

        assert!(matches!(
            p.handle_key(key(KeyCode::Tab)),
            ModelPickerAction::Consumed
        ));
        assert_eq!(p.page, PickerPage::Home);
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            ModelPickerAction::Consumed
        ));
        assert_eq!(p.page, PickerPage::Home);

        open_job_picker(&mut p, OPUS_SPEC, ModelPurpose::Plan);
        assert!(matches!(
            p.handle_key(key(KeyCode::Tab)),
            ModelPickerAction::Consumed
        ));
        assert_eq!(p.page, PickerPage::Job(ModelPurpose::Plan));
    }

    #[test]
    fn uppercase_r_unbinds_on_job_page_while_lowercase_r_searches() {
        let mut p = ModelPicker::new(test_models());
        open_job_picker(&mut p, SONNET_SPEC, ModelPurpose::Plan);

        assert!(matches!(
            p.handle_key(key(KeyCode::Char('r'))),
            ModelPickerAction::Consumed
        ));
        assert_eq!(p.picker.search_text(), "r");

        assert!(matches!(
            p.handle_key(key(KeyCode::Char('R'))),
            ModelPickerAction::Unbind(ModelPurpose::Plan)
        ));
        assert_eq!(p.page, PickerPage::Job(ModelPurpose::Plan));
        assert!(p.is_open());
    }

    #[test]
    fn assignment_targets_come_from_targets_and_exclude_self() {
        let entries = binding_entries(ModelPurpose::Plan);
        let targets: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry.binds {
                Some(Binding::Same(target)) => Some(target),
                _ => None,
            })
            .collect();

        assert_eq!(
            targets,
            [ModelPurpose::Chat, ModelPurpose::Fast, ModelPurpose::Best]
        );
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
            ModelPurpose::Best.label()
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

    #[test]
    fn refresh_preserves_selection_for_current_model() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone());
        open_picker(&mut p, OPUS_SPEC);
        navigate_to_model(&mut p, OPUS_SPEC, "Anthropic");

        models.store(Some(Arc::new(vec![
            SONNET_SPEC.into(),
            OPUS_SPEC.into(),
            SWAPPED_SPEC.into(),
        ])));
        let _ = p.refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == OPUS_SPEC),
            "after async model arrival, current model should still be selected"
        );
    }

    #[test]
    fn recents_include_current_model_preselected() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![SWAPPED_SPEC.into(), SONNET_SPEC.into()]);
        open_picker(&mut p, OPUS_SPEC);

        p.picker.select_item_by(|entry| {
            entry.spec == SWAPPED_SPEC && entry.section() == Some(RECENT_SECTION)
        });
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == SWAPPED_SPEC),
            "first entry should be the most recent model",
        );

        p.set_recents(vec![SWAPPED_SPEC.into(), SONNET_SPEC.into()]);
        open_picker(&mut p, SWAPPED_SPEC);
        navigate_to_model(&mut p, SWAPPED_SPEC, "Z.AI");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == SWAPPED_SPEC),
            "current model should be preselected in its provider section",
        );
    }

    #[test]
    fn provider_model_rows_keep_the_fast_selection_flow() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![SWAPPED_SPEC.into(), SONNET_SPEC.into()]);
        open_picker(&mut p, SONNET_SPEC);
        navigate_to_model(&mut p, SONNET_SPEC, "Anthropic");
        p.handle_key(key(KeyCode::Down));
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == SWAPPED_SPEC),
            "selecting the provider entry should return its spec",
        );

        open_picker(&mut p, SWAPPED_SPEC);
        navigate_to_model(&mut p, SWAPPED_SPEC, "Z.AI");
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            ModelPickerAction::Select(spec) if spec == SWAPPED_SPEC
        ));
    }

    #[test]
    fn refresh_keeps_selection_on_provider_entry() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![SWAPPED_SPEC.into(), SONNET_SPEC.into()]);
        open_picker(&mut p, SONNET_SPEC);
        navigate_to_model(&mut p, SONNET_SPEC, "Anthropic");
        p.handle_key(key(KeyCode::Down));
        p.needs_rebuild = true;

        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after refresh");
        assert_eq!(entry.spec, SWAPPED_SPEC);
        assert_eq!(
            entry.section(),
            Some("Z.AI"),
            "selection should stay on the provider entry, not jump to Recent",
        );
    }

    #[test]
    fn refresh_after_collapse_keeps_current_model_rows() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![SWAPPED_SPEC.into(), SONNET_SPEC.into()]);
        open_picker(&mut p, SONNET_SPEC);

        models.store(None);
        let _ = p.refresh();
        assert!(
            p.load_entries().iter().any(|entry| {
                entry.spec == SONNET_SPEC && entry.section() == Some(RECENT_SECTION)
            })
        );

        models.store(Some(Arc::new(vec![
            SONNET_SPEC.into(),
            OPUS_SPEC.into(),
            SWAPPED_SPEC.into(),
        ])));
        let _ = p.refresh();

        assert!(
            p.load_entries()
                .iter()
                .any(|entry| { entry.spec == SONNET_SPEC && entry.section() == Some("Anthropic") })
        );
    }

    #[test]
    fn refresh_preserves_navigation_to_recent_entry() {
        let models = test_models();
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![SWAPPED_SPEC.into(), SONNET_SPEC.into()]);
        open_picker(&mut p, SONNET_SPEC);
        models.store(None);
        let _ = p.refresh();
        p.handle_key(key(KeyCode::Up));
        p.handle_key(key(KeyCode::Up));

        models.store(Some(Arc::new(vec![
            SONNET_SPEC.into(),
            OPUS_SPEC.into(),
            SWAPPED_SPEC.into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after arrival");
        assert_eq!(entry.spec, SWAPPED_SPEC);
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
        p.set_recents(vec![SWAPPED_SPEC.into(), SONNET_SPEC.into()]);
        open_picker(&mut p, SONNET_SPEC);
        p.handle_paste("glm");

        models.store(None);
        let _ = p.refresh();
        models.store(Some(Arc::new(vec![
            SONNET_SPEC.into(),
            OPUS_SPEC.into(),
            SWAPPED_SPEC.into(),
        ])));
        let _ = p.refresh();

        let entry = p.picker.selected_item().expect("selection after refresh");
        assert_eq!(entry.spec, SWAPPED_SPEC);
        assert_eq!(entry.section(), Some(RECENT_SECTION));
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
        open_picker(&mut p, SONNET_SPEC);
        let entries: Vec<_> = p
            .load_entries()
            .into_iter()
            .filter(|entry| entry.spec.starts_with("openrouter/"))
            .collect();
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["stealth/ox-alpha", PAID_ID]);
    }
}
