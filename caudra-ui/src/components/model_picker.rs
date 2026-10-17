use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use caudra_config::ModelPolicy;
use caudra_config::decisions::{DecisionEndpoint, DecisionKeySource};
use caudra_grab::grab_scope;
use caudra_providers::dynamic;
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider::ProviderKind;
use caudra_providers::{Model, ModelPurpose};

use crate::components::docs_link::DocsTopic;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::tooltip::Tip;
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
const FAVORITES_SECTION: &str = "Favorites";
const FAVORITE_MARK: &str = "★";
const RECENT_MARK: &str = "↺";
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
const DECISIONS_ROW: &str = "Decisions";
const DECISIONS_TITLE: &str = " Models · Decisions ";
const DECISIONS_SECTION: &str = "Decision endpoint";
const DECISIONS_OFF: &str = "off";
const DECISIONS_OFF_DETAIL: &str = "no endpoint screens tool calls";
const DECISION_OFF_ROW: &str = "Off";
const DECISION_ADD_ROW: &str = "Add decision endpoint…";
const DECISION_ADD_DETAIL: &str = "opens /login";
const DECISION_INVALID: &str = "invalid";
const DECISION_KEY_PREFIX: &str = "key: ";
const DECISION_SPEC_PREFIX: &str = "@decision:";

fn home_footer_line() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "select/open"),
        Hint::bind(key::TOGGLE_FAVORITE, "favorite"),
        Hint::bind(key::ESC, "close"),
    ]
}

fn assignment_footer_line() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "assign"),
        Hint::bind(key::TOGGLE_FAVORITE, "favorite"),
        Hint::char(UNBIND_LABEL, "unbind"),
        Hint::bind(key::ESC, "back"),
    ]
}

fn decisions_footer_line() -> Vec<Hint> {
    vec![Hint::bind(key::ENTER, "use"), Hint::bind(key::ESC, "back")]
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
    ToggleFavorite(String),
    /// The endpoint to screen with, `None` for off.
    SelectDecision(Option<String>),
    AddDecisionEndpoint,
    Close,
    Copy(String),
}

/// One `[decisions.<name>]` table as the Decisions page lists it, read once
/// when the page opens.
pub struct DecisionChoice {
    pub name: String,
    /// Why the table does not resolve, shown in place of its details.
    pub endpoint: Result<DecisionEndpoint, String>,
    pub key: Option<DecisionKeySource>,
}

type DecisionSource = Box<dyn Fn() -> Vec<DecisionChoice>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum DecisionRow {
    /// The Home row that opens the Decisions page.
    Menu,
    Off,
    Endpoint(String),
    Add,
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
    decision: Option<DecisionRow>,
    selected: bool,
    free: bool,
    mark: Option<&'static str>,
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
        if let Some(row) = &self.decision {
            return RowIdentity::Decision(row.clone());
        }
        if let Some(purpose) = self.job {
            return RowIdentity::Job(purpose);
        }
        if self.suffix.is_some() {
            return RowIdentity::Shortlisted {
                section: self.provider_display.clone(),
                spec: self.spec.clone(),
            };
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

    fn mark(&self) -> Option<&str> {
        self.mark
    }
}

fn title_for(purpose: ModelPurpose) -> String {
    format!(" Models · {} ", purpose.label())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerPage {
    Home,
    Job(ModelPurpose),
    Decisions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RowIdentity {
    Job(ModelPurpose),
    Decision(DecisionRow),
    Binding(Option<ModelPurpose>),
    /// A row in Favorites or Recent, told apart by section because a model can
    /// sit in either.
    Shortlisted {
        section: String,
        spec: String,
    },
    ExactModel(String),
}

impl RowIdentity {
    fn model_spec(&self) -> Option<&str> {
        match self {
            Self::Shortlisted { spec, .. } | Self::ExactModel(spec) => Some(spec),
            Self::Job(_) | Self::Decision(_) | Self::Binding(_) => None,
        }
    }
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
    favorites: Vec<String>,
    current_model: Option<Model>,
    model_policy: Option<ModelPolicy>,
    page: PickerPage,
    binding: Option<Binding>,
    needs_rebuild: bool,
    anchor: Option<SelectionAnchor>,
    active_decision: Option<DecisionEndpoint>,
    decision_choices: Vec<DecisionChoice>,
    decision_source: DecisionSource,
}

impl ModelPicker {
    pub fn new(models: Arc<ArcSwapOption<Vec<String>>>) -> Self {
        Self {
            picker: ListPicker::new()
                .with_relevance_order()
                .with_width_percent(PICKER_WIDTH_PERCENT)
                .with_footer_builder(home_footer_line)
                .with_marks(),
            models,
            available: Watch::default(),
            recents: Vec::new(),
            favorites: Vec::new(),
            current_model: None,
            model_policy: None,
            page: PickerPage::Home,
            binding: None,
            needs_rebuild: false,
            anchor: None,
            active_decision: None,
            decision_choices: Vec::new(),
            decision_source: Box::new(Vec::new),
        }
    }

    /// Where the Decisions page reads its endpoints from when it opens.
    pub fn with_decision_source(
        mut self,
        source: impl Fn() -> Vec<DecisionChoice> + 'static,
    ) -> Self {
        self.decision_source = Box::new(source);
        self
    }

    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
        self.needs_rebuild = true;
    }

    /// Rebuilds at once rather than on the next tick, because the user just
    /// toggled one and is looking at the row.
    pub fn set_favorites(&mut self, favorites: Vec<String>) {
        self.favorites = favorites;
        if self.picker.is_open() {
            self.rebuild();
        }
    }

    pub fn favorites(&self) -> &[String] {
        &self.favorites
    }

    /// `active_decision` is the endpoint the session screens with, `None`
    /// while decisions are off.
    pub fn open(
        &mut self,
        current_model: &Model,
        model_policy: &ModelPolicy,
        active_decision: Option<DecisionEndpoint>,
    ) {
        self.current_model = Some(current_model.clone());
        self.model_policy = Some(model_policy.clone());
        self.active_decision = active_decision;
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
        active_decision: Option<DecisionEndpoint>,
        purpose: ModelPurpose,
    ) {
        self.open(current_model, model_policy, active_decision);
        if purpose != ModelPurpose::Chat {
            self.open_job_page(purpose);
        }
    }

    pub fn open_decisions(
        &mut self,
        current_model: &Model,
        model_policy: &ModelPolicy,
        active_decision: Option<DecisionEndpoint>,
    ) {
        self.open(current_model, model_policy, active_decision);
        self.open_decisions_page();
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

    fn open_decisions_page(&mut self) {
        self.page = PickerPage::Decisions;
        self.binding = None;
        self.anchor = None;
        self.decision_choices = (self.decision_source)();
        self.picker.set_footer_builder(decisions_footer_line);
        self.sync_empty_text();
        let entries = self.load_entries();
        self.picker.open(entries, DECISIONS_TITLE);
        self.preselect_page();
        self.capture_anchor();
    }

    /// Home again, with the cursor back on the row the page was opened from.
    fn return_home(&mut self, from: impl Fn(&ModelEntry) -> bool) {
        self.anchor = None;
        self.open_home_page();
        self.sync_empty_text();
        self.picker.select_item_by(from);
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
        self.rebuild();
        Dirty::YES
    }

    fn rebuild(&mut self) {
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
    }

    fn sync_empty_text(&mut self) {
        let models_listed = self.available.get().is_some() || self.page == PickerPage::Decisions;
        self.picker.set_empty_text(if models_listed {
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
            PickerPage::Decisions => return self.decision_entries(),
        };
        for entry in &mut entries {
            self.mark_selection(entry, &current_spec);
        }

        let favorites = self.shortlist(&self.favorites, FAVORITES_SECTION, &current_spec);
        let recents = self.shortlist(
            self.recents
                .iter()
                .filter(|spec| !self.favorites.contains(spec)),
            RECENT_SECTION,
            &current_spec,
        );
        let shortlisted = |spec: &str| {
            favorites
                .iter()
                .chain(&recents)
                .any(|entry| entry.spec == spec)
        };
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
            && !shortlisted(&current_spec)
            && !full.iter().any(|entry| entry.spec == current_spec)
            && let Some(mut current) = parse_model_entry(&current_spec)
        {
            self.mark_selection(&mut current, &current_spec);
            full.push(current);
        }
        sort_models(&mut full);
        if matches!(self.page, PickerPage::Job(_))
            && let Some(spec) = self.bound_spec()
            && !shortlisted(spec)
            && !full.iter().any(|entry| entry.spec == spec)
        {
            entries.push(saved_binding_entry(spec));
        }
        entries.extend(favorites);
        entries.extend(recents);
        entries.extend(full);
        for entry in &mut entries {
            if entry.job.is_none() && matches!(entry.binds, Some(Binding::Exact(_))) {
                entry.mark = self.mark_for(&entry.spec);
            }
        }
        entries
    }

    /// Rows for a section of hand-picked models: each names its provider as a
    /// suffix, since the section header no longer does.
    fn shortlist<'a>(
        &self,
        specs: impl IntoIterator<Item = &'a String>,
        section: &str,
        current_spec: &str,
    ) -> Vec<ModelEntry> {
        specs
            .into_iter()
            .filter_map(|spec| {
                let mut entry = parse_model_entry(spec)?;
                self.mark_selection(&mut entry, current_spec);
                entry.suffix = Some(std::mem::take(&mut entry.provider_display));
                entry.provider_display = section.to_string();
                entry.rebuild_search_text();
                Some(entry)
            })
            .collect()
    }

    fn mark_for(&self, spec: &str) -> Option<&'static str> {
        let listed = |specs: &[String]| specs.iter().any(|listed| listed == spec);
        if listed(&self.favorites) {
            Some(FAVORITE_MARK)
        } else if listed(&self.recents) {
            Some(RECENT_MARK)
        } else {
            None
        }
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
            .chain([decisions_menu_entry(self.active_decision.as_ref())])
            .collect()
    }

    /// Off, every configured endpoint in name order, then the row that adds
    /// one. The active choice is the selected row.
    fn decision_entries(&self) -> Vec<ModelEntry> {
        let active = self
            .active_decision
            .as_ref()
            .map(|endpoint| endpoint.name.as_str());
        let off = decision_entry(
            DecisionRow::Off,
            DECISION_OFF_ROW,
            None,
            DECISIONS_OFF_DETAIL.into(),
            active.is_none(),
        );
        let endpoints = self
            .decision_choices
            .iter()
            .map(|choice| decision_choice_entry(choice, active == Some(choice.name.as_str())));
        let add = decision_entry(
            DecisionRow::Add,
            DECISION_ADD_ROW,
            None,
            DECISION_ADD_DETAIL.into(),
            false,
        );
        std::iter::once(off).chain(endpoints).chain([add]).collect()
    }

    fn mark_selection(&self, entry: &mut ModelEntry, current_spec: &str) {
        entry.selected = match self.page {
            PickerPage::Home => entry.job.is_none() && entry.spec == current_spec,
            PickerPage::Job(_) => entry.job.is_none() && entry.binds == self.binding,
            PickerPage::Decisions => false,
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
            // Favorites are what the picker is opened for most of the time, so
            // the cursor starts there rather than above every job row.
            PickerPage::Home => {
                let current = self.current_model.as_ref().map(Model::spec);
                let favorite = |entry: &ModelEntry| entry.section() == Some(FAVORITES_SECTION);
                let current_favorite = |entry: &ModelEntry| {
                    favorite(entry) && current.as_deref() == Some(entry.spec.as_str())
                };
                if !self.picker.select_item_by(current_favorite)
                    && !self.picker.select_item_by(favorite)
                {
                    self.picker
                        .select_item_by(|entry| entry.job == Some(ModelPurpose::Chat));
                }
            }
            PickerPage::Job(_) | PickerPage::Decisions => {
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
        if self
            .picker
            .select_item_by(|entry| entry.identity() == anchor.row)
        {
            return true;
        }
        // A toggle moves a model between Favorites and Recent, and the
        // selection follows it to its first row that is still listed.
        let Some(spec) = anchor.row.model_spec() else {
            return false;
        };
        self.picker
            .select_item_by(|entry| entry.job.is_none() && entry.spec == spec)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.handle_text(text, true)
    }

    pub fn handle_text(&mut self, text: &str, paste: bool) -> bool {
        self.track_anchor(|p| p.picker.handle_text(text, paste))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        self.track_anchor(|p| p.handle_key_inner(key))
    }

    pub(crate) fn text_key(&self, key: Option<KeyEvent>) -> bool {
        !(key.is_some_and(is_reset_key) && matches!(self.page, PickerPage::Job(_)))
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> ModelPickerAction {
        self.track_anchor(|picker| {
            let action = picker.picker.handle_mouse(event);
            picker.map_picker_action(action)
        })
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> ModelPickerAction {
        if key.code == KeyCode::Esc && self.page != PickerPage::Home {
            match self.page {
                PickerPage::Job(purpose) => self.return_home(|entry| entry.job == Some(purpose)),
                _ => self.return_home(|entry| entry.decision == Some(DecisionRow::Menu)),
            }
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
        if key::TOGGLE_FAVORITE.matches(key) {
            return match self.picker.selected_item() {
                Some(ModelEntry {
                    job: None,
                    binds: Some(Binding::Exact(spec)),
                    ..
                }) => ModelPickerAction::ToggleFavorite(spec.clone()),
                _ => ModelPickerAction::Consumed,
            };
        }
        let action = self.picker.handle_key(key);
        self.map_picker_action(action)
    }

    fn map_picker_action(&mut self, action: PickerAction<ModelEntry>) -> ModelPickerAction {
        match action {
            PickerAction::Consumed => ModelPickerAction::Consumed,
            PickerAction::Select(ModelEntry {
                decision: Some(row),
                ..
            }) => match row {
                DecisionRow::Menu => {
                    self.open_decisions_page();
                    ModelPickerAction::Consumed
                }
                DecisionRow::Off => ModelPickerAction::SelectDecision(None),
                DecisionRow::Endpoint(name) => ModelPickerAction::SelectDecision(Some(name)),
                DecisionRow::Add => ModelPickerAction::AddDecisionEndpoint,
            },
            PickerAction::Select(entry) => match self.page {
                PickerPage::Decisions => ModelPickerAction::Consumed,
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
            PickerAction::Copy(text) => ModelPickerAction::Copy(text),
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("model_picker", area);
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
        decision: None,
        selected: false,
        free: false,
        mark: None,
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
        decision: None,
        selected: false,
        free: false,
        mark: None,
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
        decision: None,
        selected: true,
        free: false,
        mark: None,
    }
    .with_search_text()
}

/// The Jobs row that opens the Decisions page: the active endpoint's name,
/// protocol and model, or off.
fn decisions_menu_entry(active: Option<&DecisionEndpoint>) -> ModelEntry {
    let (suffix, detail) = match active {
        Some(endpoint) => (
            endpoint.name.clone(),
            format!(
                "{}{DETAIL_SEPARATOR}{}",
                endpoint.protocol.as_str(),
                endpoint.model
            ),
        ),
        None => (DECISIONS_OFF.to_owned(), DECISIONS_OFF_DETAIL.to_owned()),
    };
    let mut entry = decision_entry(
        DecisionRow::Menu,
        DECISIONS_ROW,
        Some(suffix),
        detail,
        false,
    );
    entry.provider_display = JOBS_SECTION.into();
    entry.rebuild_search_text();
    entry
}

/// An endpoint that does not resolve still lists, its error in place of the
/// details, so choosing it explains why it cannot screen.
fn decision_choice_entry(choice: &DecisionChoice, selected: bool) -> ModelEntry {
    let (suffix, detail) = match &choice.endpoint {
        Ok(endpoint) => (
            endpoint.protocol.as_str().to_owned(),
            format!(
                "{}{DETAIL_SEPARATOR}{}{DETAIL_SEPARATOR}{DECISION_KEY_PREFIX}{}",
                endpoint.model,
                endpoint.base_url.authority(),
                DecisionKeySource::label(choice.key)
            ),
        ),
        Err(error) => (DECISION_INVALID.to_owned(), error.clone()),
    };
    decision_entry(
        DecisionRow::Endpoint(choice.name.clone()),
        &choice.name,
        Some(suffix),
        detail,
        selected,
    )
}

fn decision_entry(
    row: DecisionRow,
    label: &str,
    suffix: Option<String>,
    detail: String,
    selected: bool,
) -> ModelEntry {
    ModelEntry {
        spec: format!("{DECISION_SPEC_PREFIX}{label}"),
        id: label.into(),
        provider_display: DECISIONS_SECTION.into(),
        suffix,
        detail,
        search_text: String::new(),
        claimed_by: Vec::new(),
        binds: None,
        job: None,
        decision: Some(row),
        selected,
        free: false,
        mark: None,
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
    fn docs_topic(&self) -> Option<DocsTopic> {
        Some(DocsTopic::Models)
    }

    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }

    fn tooltip(&self) -> Option<Tip> {
        self.picker.tooltip()
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
        if !config.defines_provider(provider_str) {
            return None;
        }
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
            decision: None,
            selected: false,
            free,
            mark: None,
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
    use caudra_config::decisions::DecisionProtocol;
    use caudra_providers::ModelInfo;
    use caudra_providers::ModelPricing;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    const SAME_SIZED_LIST: &str = "a republished list of the same length is still a new list";
    const SWAPPED_SPEC: &str = "zai/glm-5";
    const SONNET_SPEC: &str = "anthropic/claude-sonnet-4-5";
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
        picker.open(&current_model(spec), &ModelPolicy::default(), None);
    }

    fn open_job_picker(picker: &mut ModelPicker, spec: &str, purpose: ModelPurpose) {
        picker.open_purpose(&current_model(spec), &ModelPolicy::default(), None, purpose);
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
        assert_eq!(selected.label(), "claude-sonnet-4-5");
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
    fn a_job_filter_accepts_text_without_a_command_key() {
        let mut picker = ModelPicker::new(test_models());
        open_job_picker(&mut picker, SONNET_SPEC, ModelPurpose::Plan);
        assert!(picker.text_key(None));
        picker.handle_text(MODEL_QUERY, false);
        assert_eq!(picker.picker.search_text(), MODEL_QUERY);
        assert!(!picker.text_key(Some(key(KeyCode::Char('R')))));
        assert_eq!(picker.page, PickerPage::Job(ModelPurpose::Plan));
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
        assert_eq!(entry.id, "claude-sonnet-4-5");
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

    fn shortlisted_specs(p: &ModelPicker, section: &str) -> Vec<String> {
        p.load_entries()
            .into_iter()
            .filter(|entry| entry.section() == Some(section))
            .map(|entry| entry.spec)
            .collect()
    }

    fn selected_row(p: &ModelPicker) -> (String, Option<String>) {
        let entry = p.picker.selected_item().expect("a selected row");
        (entry.spec.clone(), entry.section().map(str::to_owned))
    }

    #[test]
    fn favorites_lead_and_leave_recent_without_leaving_their_providers() {
        let mut p = ModelPicker::new(test_models());
        p.set_recents(vec![SWAPPED_SPEC.into(), SONNET_SPEC.into()]);
        p.set_favorites(vec![OPUS_SPEC.into(), SONNET_SPEC.into()]);
        open_picker(&mut p, SONNET_SPEC);

        assert_eq!(
            shortlisted_specs(&p, FAVORITES_SECTION),
            [OPUS_SPEC, SONNET_SPEC]
        );
        assert_eq!(shortlisted_specs(&p, RECENT_SECTION), [SWAPPED_SPEC]);
        assert_eq!(shortlisted_specs(&p, "Anthropic"), [OPUS_SPEC, SONNET_SPEC]);

        let sections: Vec<_> = p
            .load_entries()
            .into_iter()
            .filter(|entry| entry.job.is_none())
            .filter_map(|entry| entry.section().map(str::to_owned))
            .collect();
        let first = |section: &str| sections.iter().position(|s| s == section).unwrap();
        assert!(first(FAVORITES_SECTION) < first(RECENT_SECTION));
        assert!(first(RECENT_SECTION) < first("Anthropic"));
    }

    #[test_case(&[], SONNET_SPEC, None; "no_favorites_keeps_the_chat_job")]
    #[test_case(&[OPUS_SPEC, SWAPPED_SPEC], SWAPPED_SPEC, Some(SWAPPED_SPEC); "current_favorite")]
    #[test_case(&[OPUS_SPEC, SWAPPED_SPEC], SONNET_SPEC, Some(OPUS_SPEC); "first_favorite")]
    fn opening_starts_on_the_favorites(favorites: &[&str], current: &str, expected: Option<&str>) {
        let mut p = ModelPicker::new(test_models());
        p.set_favorites(favorites.iter().map(|spec| spec.to_string()).collect());
        open_picker(&mut p, current);

        let selected = p.picker.selected_item().expect("a selected row");
        match expected {
            Some(spec) => assert_eq!(
                selected_row(&p),
                (spec.into(), Some(FAVORITES_SECTION.into()))
            ),
            None => assert_eq!(selected.job, Some(ModelPurpose::Chat)),
        }
    }

    #[test_case(OPUS_SPEC, Some(FAVORITE_MARK); "favorite")]
    #[test_case(SONNET_SPEC, Some(FAVORITE_MARK); "favorite_and_recent")]
    #[test_case(SWAPPED_SPEC, Some(RECENT_MARK); "recent_only")]
    #[test_case(UNCLASSIFIED_SPEC, None; "unlisted")]
    fn every_row_of_a_model_carries_its_mark(spec: &str, expected: Option<&str>) {
        let mut p = ModelPicker::new(test_models());
        p.set_recents(vec![SONNET_SPEC.into(), SWAPPED_SPEC.into()]);
        p.set_favorites(vec![OPUS_SPEC.into(), SONNET_SPEC.into()]);
        open_picker(&mut p, UNCLASSIFIED_SPEC);

        let marks: Vec<_> = p
            .load_entries()
            .into_iter()
            .filter(|entry| entry.job.is_none() && entry.spec == spec)
            .map(|entry| entry.mark)
            .collect();
        assert!(!marks.is_empty());
        assert!(marks.iter().all(|mark| *mark == expected), "{marks:?}");
    }

    #[test]
    fn toggle_favorite_names_the_model_row_and_ignores_job_rows() {
        let mut p = ModelPicker::new(test_models());
        open_picker(&mut p, SONNET_SPEC);
        assert!(matches!(
            p.handle_key(kb::TOGGLE_FAVORITE.to_key_event()),
            ModelPickerAction::Consumed
        ));

        navigate_to_model(&mut p, OPUS_SPEC, "Anthropic");
        assert!(matches!(
            p.handle_key(kb::TOGGLE_FAVORITE.to_key_event()),
            ModelPickerAction::ToggleFavorite(spec) if spec == OPUS_SPEC
        ));
        assert!(p.is_open());
    }

    #[test]
    fn the_selection_follows_a_model_between_recent_and_favorites() {
        let mut p = ModelPicker::new(test_models());
        p.set_recents(vec![SWAPPED_SPEC.into(), SONNET_SPEC.into()]);
        open_picker(&mut p, SONNET_SPEC);
        navigate_to_model(&mut p, SWAPPED_SPEC, RECENT_SECTION);

        p.set_favorites(vec![SWAPPED_SPEC.into()]);
        assert_eq!(
            selected_row(&p),
            (SWAPPED_SPEC.into(), Some(FAVORITES_SECTION.into()))
        );

        p.set_favorites(Vec::new());
        assert_eq!(
            selected_row(&p),
            (SWAPPED_SPEC.into(), Some(RECENT_SECTION.into()))
        );
    }

    #[test]
    fn a_job_page_lists_favorites_and_toggles_them() {
        let mut p = ModelPicker::new(test_models());
        p.set_favorites(vec![OPUS_SPEC.into()]);
        open_job_picker(&mut p, SONNET_SPEC, ModelPurpose::Goal);

        assert_eq!(shortlisted_specs(&p, FAVORITES_SECTION), [OPUS_SPEC]);
        navigate_to_model(&mut p, OPUS_SPEC, FAVORITES_SECTION);
        assert!(matches!(
            p.handle_key(kb::TOGGLE_FAVORITE.to_key_event()),
            ModelPickerAction::ToggleFavorite(spec) if spec == OPUS_SPEC
        ));
    }

    const FIRST_ENDPOINT: &str = "local";
    const SECOND_ENDPOINT: &str = "tunnel";
    const BROKEN_ENDPOINT: &str = "broken";
    const ENDPOINT_URL: &str = "http://127.0.0.1:8080/v1";
    const ENDPOINT_AUTHORITY: &str = "127.0.0.1:8080";
    const BROKEN_ERROR: &str = "base_url must use https";
    const SAVED_KEY_DETAIL: &str = "key: saved";
    const OPENAI_PROTOCOL: &str = "openai";
    const UNEXPECTED_DECISION_ACTION: &str = "unexpected action for the decision row";

    fn endpoint(name: &str) -> DecisionEndpoint {
        DecisionEndpoint::new(
            name,
            DecisionProtocol::OpenAI,
            ENDPOINT_URL.parse().unwrap(),
        )
    }

    fn decision_choices() -> Vec<DecisionChoice> {
        vec![
            DecisionChoice {
                name: BROKEN_ENDPOINT.into(),
                endpoint: Err(BROKEN_ERROR.into()),
                key: None,
            },
            DecisionChoice {
                name: FIRST_ENDPOINT.into(),
                endpoint: Ok(endpoint(FIRST_ENDPOINT)),
                key: Some(DecisionKeySource::Stored),
            },
            DecisionChoice {
                name: SECOND_ENDPOINT.into(),
                endpoint: Ok(endpoint(SECOND_ENDPOINT)),
                key: None,
            },
        ]
    }

    fn decisions_picker() -> ModelPicker {
        ModelPicker::new(test_models()).with_decision_source(decision_choices)
    }

    fn open_decisions(picker: &mut ModelPicker, active: Option<&str>) {
        picker.open_decisions(
            &current_model(SONNET_SPEC),
            &ModelPolicy::default(),
            active.map(endpoint),
        );
    }

    fn decision_rows(picker: &ModelPicker) -> Vec<DecisionRow> {
        picker
            .load_entries()
            .into_iter()
            .filter_map(|entry| entry.decision)
            .collect()
    }

    fn selected_decision(picker: &ModelPicker) -> Option<DecisionRow> {
        picker
            .picker
            .selected_item()
            .and_then(|entry| entry.decision.clone())
    }

    fn select_decision(picker: &mut ModelPicker, row: DecisionRow) {
        let selected = picker.track_anchor(|p| {
            p.picker
                .select_item_by(|entry| entry.decision == Some(row.clone()))
        });
        assert!(selected);
    }

    #[test_case(None, DECISIONS_OFF, DECISIONS_OFF_DETAIL; "off")]
    #[test_case(Some(SECOND_ENDPOINT), SECOND_ENDPOINT, OPENAI_PROTOCOL; "active")]
    fn home_lists_the_decisions_row_after_the_jobs(
        active: Option<&str>,
        suffix: &str,
        detail: &str,
    ) {
        let mut p = decisions_picker();
        p.open(
            &current_model(SONNET_SPEC),
            &ModelPolicy::default(),
            active.map(endpoint),
        );

        let entries = p.load_entries();
        let index = entries
            .iter()
            .position(|entry| entry.decision == Some(DecisionRow::Menu))
            .unwrap();
        let row = &entries[index];
        assert_eq!(index, ModelPurpose::ALL.len());
        assert_eq!(row.section(), Some(JOBS_SECTION));
        assert_eq!(row.label(), DECISIONS_ROW);
        assert_eq!(row.suffix(), Some(suffix));
        assert!(row.detail.starts_with(detail), "{}", row.detail);
    }

    #[test]
    fn the_decisions_row_opens_its_page_and_escape_returns_to_it() {
        let mut p = decisions_picker();
        open_picker(&mut p, SONNET_SPEC);
        select_decision(&mut p, DecisionRow::Menu);

        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            ModelPickerAction::Consumed
        ));
        assert_eq!(p.page, PickerPage::Decisions);
        assert!(render(&mut p).contains(DECISION_ADD_ROW));

        assert!(matches!(
            p.handle_key(key(KeyCode::Esc)),
            ModelPickerAction::Consumed
        ));
        assert_eq!(p.page, PickerPage::Home);
        assert_eq!(selected_decision(&p), Some(DecisionRow::Menu));
    }

    #[test_case(None, DecisionRow::Off; "off")]
    #[test_case(Some(SECOND_ENDPOINT), DecisionRow::Endpoint(SECOND_ENDPOINT.into()); "active")]
    fn the_page_lists_off_every_endpoint_and_add(active: Option<&str>, preselected: DecisionRow) {
        let mut p = decisions_picker();
        open_decisions(&mut p, active);

        assert_eq!(
            decision_rows(&p),
            [
                DecisionRow::Off,
                DecisionRow::Endpoint(BROKEN_ENDPOINT.into()),
                DecisionRow::Endpoint(FIRST_ENDPOINT.into()),
                DecisionRow::Endpoint(SECOND_ENDPOINT.into()),
                DecisionRow::Add,
            ]
        );
        assert_eq!(selected_decision(&p), Some(preselected));
        let marked: Vec<_> = p
            .load_entries()
            .into_iter()
            .filter(|entry| entry.selected)
            .filter_map(|entry| entry.decision)
            .collect();
        assert_eq!(marked.len(), 1);
    }

    #[test_case(FIRST_ENDPOINT, OPENAI_PROTOCOL, SAVED_KEY_DETAIL; "valid")]
    #[test_case(BROKEN_ENDPOINT, DECISION_INVALID, BROKEN_ERROR; "invalid")]
    fn endpoint_rows_say_how_they_connect(name: &str, suffix: &str, detail: &str) {
        let mut p = decisions_picker();
        open_decisions(&mut p, None);

        let row = p
            .load_entries()
            .into_iter()
            .find(|entry| entry.decision == Some(DecisionRow::Endpoint(name.into())))
            .unwrap();
        assert_eq!(row.suffix(), Some(suffix));
        assert!(row.detail.contains(detail), "{}", row.detail);
        if row.suffix() != Some(DECISION_INVALID) {
            assert!(row.detail.contains(ENDPOINT_AUTHORITY), "{}", row.detail);
        }
    }

    #[test_case(DecisionRow::Off, Some(None); "off")]
    #[test_case(DecisionRow::Endpoint(SECOND_ENDPOINT.into()), Some(Some(SECOND_ENDPOINT)); "endpoint")]
    #[test_case(DecisionRow::Add, None; "add")]
    fn enter_on_a_decision_row_names_the_choice(row: DecisionRow, expected: Option<Option<&str>>) {
        let mut p = decisions_picker();
        open_decisions(&mut p, Some(FIRST_ENDPOINT));
        select_decision(&mut p, row);

        match (p.handle_key(key(KeyCode::Enter)), expected) {
            (ModelPickerAction::SelectDecision(name), Some(expected)) => {
                assert_eq!(name.as_deref(), expected);
            }
            (ModelPickerAction::AddDecisionEndpoint, None) => {}
            _ => panic!("{UNEXPECTED_DECISION_ACTION}"),
        }
    }

    #[test]
    fn the_decision_filter_narrows_the_page() {
        let mut p = decisions_picker();
        open_decisions(&mut p, None);

        p.handle_paste(SECOND_ENDPOINT);

        assert_eq!(
            selected_decision(&p),
            Some(DecisionRow::Endpoint(SECOND_ENDPOINT.into()))
        );
    }

    #[test_case(true; "decisions_page")]
    #[test_case(false; "home_row")]
    fn toggling_a_favorite_on_a_decision_row_does_nothing(on_page: bool) {
        let mut p = decisions_picker();
        if on_page {
            open_decisions(&mut p, None);
            select_decision(&mut p, DecisionRow::Endpoint(FIRST_ENDPOINT.into()));
        } else {
            open_picker(&mut p, SONNET_SPEC);
            select_decision(&mut p, DecisionRow::Menu);
        }

        assert!(matches!(
            p.handle_key(kb::TOGGLE_FAVORITE.to_key_event()),
            ModelPickerAction::Consumed
        ));
        assert!(p.favorites().is_empty());
        assert!(p.is_open());
    }

    #[test]
    fn the_decision_selection_survives_a_refresh() {
        let models = test_models();
        let mut p = ModelPicker::new(Arc::clone(&models)).with_decision_source(decision_choices);
        open_decisions(&mut p, None);
        select_decision(&mut p, DecisionRow::Endpoint(SECOND_ENDPOINT.into()));

        models.store(Some(Arc::new(vec![SWAPPED_SPEC.into()])));
        assert_eq!(p.refresh(), Dirty::YES);

        assert_eq!(
            selected_decision(&p),
            Some(DecisionRow::Endpoint(SECOND_ENDPOINT.into()))
        );
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
        tiers: ModelPricing::UNTIERED,
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
