use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

use caudra_providers::ModelTier;
use caudra_providers::dynamic;
use caudra_providers::model_registry::{self, CompactionTarget, GoalEvaluatorTarget};
use caudra_providers::provider::ProviderKind;

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::{Cadence, Dirty, Watch};
use crate::theme;

const TARGET_SECTION: &str = "Evaluator target";
const RECENT_SECTION: &str = "Recent";
const FREE_LABEL: &str = "Free";
const FREE_PREFIX: &str = "Free · ";
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
    SetGoalEvaluator(GoalEvaluatorTarget),
    SetCompaction(CompactionTarget),
    AssignTier(String, ModelTier),
    ResetTier(ModelTier),
    Close,
}

struct ModelEntry {
    spec: String,
    id: String,
    provider_display: String,
    suffix: Option<String>,
    tier: String,
    override_tiers: Vec<ModelTier>,
    goal_target: Option<GoalEvaluatorTarget>,
    goal_assigned: bool,
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
        Some(&self.tier)
    }

    fn section(&self) -> Option<&str> {
        Some(self.provider_display.as_str())
    }

    fn is_highlighted(&self) -> bool {
        !self.override_tiers.is_empty() || self.goal_assigned
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerMode {
    Chat,
    Goal,
    Compact,
    Fast,
    Balanced,
    Best,
}

impl PickerMode {
    const ALL: [Self; 6] = [
        Self::Chat,
        Self::Goal,
        Self::Compact,
        Self::Fast,
        Self::Balanced,
        Self::Best,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Goal => "Goal",
            Self::Compact => "Compact",
            Self::Fast => "Fast",
            Self::Balanced => "Balanced",
            Self::Best => "Best",
        }
    }

    fn title(self) -> String {
        format!(" Models · {} ", self.label())
    }

    fn preset_tier(self) -> Option<ModelTier> {
        match self {
            Self::Fast => Some(ModelTier::Weak),
            Self::Balanced => Some(ModelTier::Medium),
            Self::Best => Some(ModelTier::Strong),
            _ => None,
        }
    }

    fn shifted(self, backwards: bool) -> Self {
        let index = Self::ALL.iter().position(|mode| *mode == self).unwrap_or(0);
        let next = if backwards {
            index.checked_sub(1).unwrap_or(Self::ALL.len() - 1)
        } else {
            (index + 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<Vec<String>>>,
    available: Watch<Vec<String>>,
    recents: Vec<String>,
    current_spec: String,
    goal_target: GoalEvaluatorTarget,
    compaction_target: CompactionTarget,
    mode: PickerMode,
    needs_rebuild: bool,
    /// User-moved entry to restore on refresh: `(was_recent, spec)`.
    anchor: Option<(bool, String)>,
}

impl ModelPicker {
    pub fn new(models: Arc<ArcSwapOption<Vec<String>>>) -> Self {
        Self {
            picker: ListPicker::new().with_footer_builder(model_footer_line),
            models,
            available: Watch::default(),
            recents: Vec::new(),
            current_spec: String::new(),
            goal_target: GoalEvaluatorTarget::Auto,
            compaction_target: CompactionTarget::Auto,
            mode: PickerMode::Chat,
            needs_rebuild: false,
            anchor: None,
        }
    }

    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
        self.needs_rebuild = true;
    }

    pub fn open(&mut self, current_spec: &str) {
        self.mode = PickerMode::Chat;
        self.current_spec = current_spec.to_owned();
        self.goal_target = model_registry::goal_evaluator_target();
        self.compaction_target = model_registry::compaction_target();
        self.anchor = None;
        self.needs_rebuild = false;
        self.picker.set_footer_builder(model_footer_line);
        let _ = self.available.poll(self.models.load_full());
        self.sync_empty_text();
        let entries = self.load_entries();
        self.picker.open(entries, self.mode.title());
        self.preselect_mode();
    }

    pub fn open_goal(&mut self, current_spec: &str, target: GoalEvaluatorTarget) {
        self.mode = PickerMode::Goal;
        self.current_spec = current_spec.to_owned();
        self.goal_target = target;
        self.compaction_target = model_registry::compaction_target();
        self.anchor = None;
        self.needs_rebuild = false;
        self.picker.set_footer_builder(assignment_footer_line);
        let _ = self.available.poll(self.models.load_full());
        self.sync_empty_text();
        let entries = self.load_entries();
        self.picker.open(entries, self.mode.title());
        self.preselect_mode();
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
            self.preselect_mode();
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
        if self.mode == PickerMode::Goal {
            let mut entries = goal_target_entries(&self.goal_target);
            let mut models: Vec<ModelEntry> = specs
                .map(|specs| {
                    specs
                        .iter()
                        .filter_map(|spec| {
                            let mut entry = parse_model_entry(spec)?;
                            let target = GoalEvaluatorTarget::Model(spec.clone());
                            entry.goal_assigned = self.goal_target == target;
                            entry.goal_target = Some(target);
                            if entry.goal_assigned {
                                entry.tier = assignment_detail(&entry.tier, self.mode.label());
                            }
                            Some(entry)
                        })
                        .collect()
                })
                .unwrap_or_default();
            sort_models(&mut models);
            if let GoalEvaluatorTarget::Model(spec) = &self.goal_target
                && !models.iter().any(|entry| entry.spec == *spec)
            {
                entries.push(saved_goal_model_entry(spec));
            }
            entries.extend(models);
            return entries;
        }

        let mut entries = Vec::new();
        for spec in &self.recents {
            if let Some(mut e) = parse_model_entry(spec) {
                self.mark_assignment(&mut e);
                e.suffix = Some(std::mem::take(&mut e.provider_display));
                e.provider_display = RECENT_SECTION.to_string();
                entries.push(e);
            }
        }
        let mut full: Vec<ModelEntry> = specs
            .map(|s| {
                s.iter()
                    .filter_map(|spec| {
                        let mut entry = parse_model_entry(spec)?;
                        self.mark_assignment(&mut entry);
                        Some(entry)
                    })
                    .collect()
            })
            .unwrap_or_default();
        if self.mode == PickerMode::Chat
            && !self.current_spec.is_empty()
            && !entries.iter().any(|entry| entry.spec == self.current_spec)
            && !full.iter().any(|entry| entry.spec == self.current_spec)
            && let Some(mut current) = parse_model_entry(&self.current_spec)
        {
            self.mark_assignment(&mut current);
            full.push(current);
        }
        sort_models(&mut full);
        entries.extend(full);
        if let Some(spec) = self.assigned_spec()
            && !entries.iter().any(|entry| entry.spec == spec)
        {
            entries.insert(0, saved_assignment_entry(&spec, self.mode.label()));
        }
        entries
    }

    fn mark_assignment(&self, entry: &mut ModelEntry) {
        entry.goal_assigned = self.assigned_spec().is_some_and(|spec| spec == entry.spec);
        if entry.goal_assigned {
            entry.tier = assignment_detail(&entry.tier, self.mode.label());
        }
    }

    fn assigned_spec(&self) -> Option<String> {
        match self.mode {
            PickerMode::Goal => match &self.goal_target {
                GoalEvaluatorTarget::Model(spec) => Some(spec.clone()),
                _ => None,
            },
            PickerMode::Compact => match &self.compaction_target {
                CompactionTarget::Model(spec) => Some(spec.clone()),
                CompactionTarget::Auto => None,
            },
            mode => mode
                .preset_tier()
                .and_then(model_registry::override_spec_for_tier),
        }
    }

    fn preselect_mode(&mut self) {
        if self.mode == PickerMode::Goal {
            self.picker
                .select_item_by(|entry| entry.goal_target.as_ref() == Some(&self.goal_target));
        } else if let Some(spec) = self.assigned_spec() {
            self.preselect_spec(&spec);
        } else if self.mode == PickerMode::Chat {
            let spec = self.current_spec.clone();
            self.preselect_spec(&spec);
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
            self.mode = self.mode.shifted(backwards);
            self.anchor = None;
            self.picker.set_title(self.mode.title());
            self.picker
                .set_footer_builder(if self.mode == PickerMode::Chat {
                    model_footer_line
                } else {
                    assignment_footer_line
                });
            let entries = self.load_entries();
            self.picker.replace_items(entries);
            self.preselect_mode();
            return ModelPickerAction::Consumed;
        }
        if is_reset_key(key) {
            self.anchor = None;
            self.picker.clear_search();
            self.needs_rebuild = true;
            return match self.mode {
                PickerMode::Chat => {
                    self.preselect_mode();
                    ModelPickerAction::Consumed
                }
                PickerMode::Goal => {
                    self.goal_target = GoalEvaluatorTarget::Auto;
                    ModelPickerAction::SetGoalEvaluator(GoalEvaluatorTarget::Auto)
                }
                PickerMode::Compact => {
                    self.compaction_target = CompactionTarget::Auto;
                    ModelPickerAction::SetCompaction(CompactionTarget::Auto)
                }
                mode => ModelPickerAction::ResetTier(
                    mode.preset_tier().expect("preset mode must have a tier"),
                ),
            };
        }
        let action = self.picker.handle_key(key);
        self.map_picker_action(action)
    }

    fn map_picker_action(&self, action: PickerAction<ModelEntry>) -> ModelPickerAction {
        match action {
            PickerAction::Consumed => ModelPickerAction::Consumed,
            PickerAction::Select(entry) => match self.mode {
                PickerMode::Chat => ModelPickerAction::Select(entry.spec),
                PickerMode::Goal => ModelPickerAction::SetGoalEvaluator(
                    entry.goal_target.unwrap_or(GoalEvaluatorTarget::Auto),
                ),
                PickerMode::Compact => {
                    ModelPickerAction::SetCompaction(CompactionTarget::Model(entry.spec))
                }
                mode => ModelPickerAction::AssignTier(
                    entry.spec,
                    mode.preset_tier().expect("preset mode must have a tier"),
                ),
            },
            PickerAction::Close => ModelPickerAction::Close,
            PickerAction::Toggle(..) => ModelPickerAction::Consumed,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

fn goal_target_entries(current: &GoalEvaluatorTarget) -> Vec<ModelEntry> {
    [
        ("Default", "fast, then chat", GoalEvaluatorTarget::Auto),
        (
            "Fast",
            "global preset",
            GoalEvaluatorTarget::Tier(ModelTier::Weak),
        ),
        (
            "Balanced",
            "global preset",
            GoalEvaluatorTarget::Tier(ModelTier::Medium),
        ),
        (
            "Best",
            "global preset",
            GoalEvaluatorTarget::Tier(ModelTier::Strong),
        ),
    ]
    .into_iter()
    .map(|(label, detail, target)| ModelEntry {
        spec: format!("@goal:{target}"),
        id: label.into(),
        provider_display: TARGET_SECTION.into(),
        suffix: None,
        tier: detail.into(),
        override_tiers: Vec::new(),
        goal_assigned: &target == current,
        goal_target: Some(target),
        free: false,
    })
    .collect()
}

fn saved_goal_model_entry(spec: &str) -> ModelEntry {
    let id = spec
        .split_once('/')
        .map_or(spec, |(_, model_id)| model_id)
        .to_string();
    ModelEntry {
        spec: spec.to_string(),
        id,
        provider_display: TARGET_SECTION.into(),
        suffix: None,
        tier: "saved exact model".into(),
        override_tiers: Vec::new(),
        goal_target: Some(GoalEvaluatorTarget::Model(spec.to_string())),
        goal_assigned: true,
        free: false,
    }
}

fn saved_assignment_entry(spec: &str, purpose: &str) -> ModelEntry {
    let mut entry = saved_goal_model_entry(spec);
    entry.goal_target = None;
    entry.tier = format!("saved {} model", purpose.to_ascii_lowercase());
    entry
}

fn sort_models(models: &mut [ModelEntry]) {
    models.sort_by(|a, b| {
        a.provider_display
            .cmp(&b.provider_display)
            .then_with(|| b.free.cmp(&a.free))
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn assignment_detail(detail: &str, purpose: &str) -> String {
    let purpose = purpose.to_ascii_lowercase();
    if detail.is_empty() {
        purpose
    } else {
        format!("{detail}/{purpose}")
    }
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

    let override_tiers = model_registry::override_tiers(spec);
    let (tier, free) = match caudra_providers::Model::from_spec(spec) {
        Ok(m) => (m.tier.to_string(), m.is_free()),
        Err(_) => (String::new(), false),
    };
    let tier = if override_tiers.is_empty() {
        tier
    } else {
        override_tiers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("/")
    };
    let tier = match (free, tier.is_empty()) {
        (true, true) => FREE_LABEL.to_string(),
        (true, false) => format!("{FREE_PREFIX}{tier}"),
        (false, _) => tier,
    };
    let id = model_id.to_string();
    Some(ModelEntry {
        spec: spec.to_string(),
        id,
        provider_display,
        suffix: None,
        tier,
        override_tiers,
        goal_target: None,
        goal_assigned: false,
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
    fn goal_picker_selects_tier_target() {
        let mut p = ModelPicker::new(test_models());
        p.open_goal("", GoalEvaluatorTarget::Auto);
        p.handle_key(key(KeyCode::Down));

        let action = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            action,
            ModelPickerAction::SetGoalEvaluator(GoalEvaluatorTarget::Tier(ModelTier::Weak))
        ));
    }

    #[test]
    fn goal_picker_preselects_exact_model() {
        let mut p = ModelPicker::new(test_models());
        p.open_goal("", GoalEvaluatorTarget::Model(SWAPPED_SPEC.into()));

        let action = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            action,
            ModelPickerAction::SetGoalEvaluator(GoalEvaluatorTarget::Model(spec))
                if spec == SWAPPED_SPEC
        ));
    }

    #[test]
    fn goal_picker_preserves_saved_exact_model_missing_from_discovery() {
        let target = GoalEvaluatorTarget::Model("catalog-provider/vendor/model".into());
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models);
        p.open_goal("", target.clone());

        let action = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            action,
            ModelPickerAction::SetGoalEvaluator(selected) if selected == target
        ));
    }

    #[test]
    fn goal_picker_refresh_keeps_unavailable_saved_exact_model_selected() {
        let target = GoalEvaluatorTarget::Model("catalog-provider/vendor/model".into());
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(Arc::clone(&models));
        p.open_goal("", target.clone());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));

        assert_eq!(p.refresh(), Dirty::YES);
        let action = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            action,
            ModelPickerAction::SetGoalEvaluator(selected) if selected == target
        ));
    }

    #[test]
    fn tab_switches_to_goal_and_reset_restores_default() {
        let spec = "anthropic/claude-opus-4-6-20260101";
        let mut p = ModelPicker::new(test_models());
        p.open(spec);

        assert!(matches!(
            p.handle_key(key(KeyCode::Tab)),
            ModelPickerAction::Consumed
        ));
        p.picker
            .select_item_by(|entry| entry.spec == spec && entry.goal_target.is_some());
        let assign = p.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            assign,
            ModelPickerAction::SetGoalEvaluator(GoalEvaluatorTarget::Model(value)) if value == spec
        ));
        p.open_goal(spec, GoalEvaluatorTarget::Model(spec.into()));
        let reset = p.handle_key(key(KeyCode::Char('R')));
        assert!(matches!(
            reset,
            ModelPickerAction::SetGoalEvaluator(GoalEvaluatorTarget::Auto)
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
        let entry = parse_model_entry("anthropic/claude-sonnet-4-20250514").unwrap();
        assert_eq!(entry.id, "claude-sonnet-4-20250514");
        assert_eq!(entry.provider_display, "Anthropic");
        assert!(!entry.tier.is_empty());
    }

    #[test]
    fn parse_model_entry_paid_model_not_marked_free() {
        let entry = parse_model_entry("anthropic/claude-sonnet-4-20250514").unwrap();
        assert!(
            !entry.tier.starts_with(FREE_PREFIX),
            "paid anthropic model must not be marked free"
        );
    }

    #[test]
    fn parse_model_entry_no_slash() {
        assert!(parse_model_entry("no-slash").is_none());
    }

    #[test_case(3, ModelTier::Weak     ; "fast")]
    #[test_case(4, ModelTier::Medium   ; "balanced")]
    #[test_case(5, ModelTier::Strong   ; "best")]
    fn preset_modes_assign_selected_exact_model(tabs: usize, want: ModelTier) {
        let mut p = ModelPicker::new(test_models());
        p.open("anthropic/claude-sonnet-4-20250514");
        for _ in 0..tabs {
            p.handle_key(key(KeyCode::Tab));
        }
        p.picker.select_item_by(|entry| {
            entry.spec == "anthropic/claude-sonnet-4-20250514" && entry.suffix().is_none()
        });
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(&action, ModelPickerAction::AssignTier(s, t)
                if s == "anthropic/claude-sonnet-4-20250514" && *t == want),
            "expected AssignTier(claude-sonnet, {want:?}), got something else",
        );
        assert!(!p.is_open());
    }

    #[test]
    fn shift_tab_wraps_from_chat_to_best() {
        let spec = "anthropic/claude-sonnet-4-20250514";
        let mut p = ModelPicker::new(test_models());
        p.open(spec);
        p.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        p.picker
            .select_item_by(|entry| entry.spec == spec && entry.suffix().is_none());

        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            ModelPickerAction::AssignTier(value, ModelTier::Strong) if value == spec
        ));
    }

    #[test]
    fn uppercase_r_resets_preset_while_lowercase_r_filters() {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        for _ in 0..3 {
            p.handle_key(key(KeyCode::Tab));
        }

        assert!(matches!(
            p.handle_key(key(KeyCode::Char('r'))),
            ModelPickerAction::Consumed
        ));
        assert!(matches!(
            p.handle_key(key(KeyCode::Char('R'))),
            ModelPickerAction::ResetTier(ModelTier::Weak)
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
    fn compact_mode_assigns_and_resets_exact_model() {
        let spec = "anthropic/claude-sonnet-4-20250514";
        let mut p = ModelPicker::new(test_models());
        p.open(spec);
        p.handle_key(key(KeyCode::Tab));
        p.handle_key(key(KeyCode::Tab));
        p.picker
            .select_item_by(|entry| entry.spec == spec && entry.suffix().is_none());

        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            ModelPickerAction::SetCompaction(CompactionTarget::Model(value)) if value == spec
        ));

        p.open(spec);
        p.handle_key(key(KeyCode::Tab));
        p.handle_key(key(KeyCode::Tab));
        assert!(matches!(
            p.handle_key(key(KeyCode::Char('R'))),
            ModelPickerAction::SetCompaction(CompactionTarget::Auto)
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
            entry.tier.starts_with(FREE_PREFIX),
            "zero-priced discovery must mark the entry free"
        );
    }

    #[test]
    fn paid_discovery_not_marked_free() {
        register_openrouter_models();
        let entry = parse_model_entry(&format!("openrouter/{PAID_ID}")).unwrap();
        assert!(
            !entry.tier.starts_with(FREE_PREFIX),
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
