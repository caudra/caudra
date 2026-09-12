use std::time::{Duration, Instant};

use caudra_providers::{Model, ResolvedThinking, ThinkingConfig};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::{Cadence, Dirty};

pub(crate) const TITLE: &str = " Thinking ";
const MAX_VISIBLE: u16 = 10;
/// How long a keyboard cycle leaves the picker on screen as feedback. Long
/// enough to read the row that just changed, short enough that it never feels
/// like a window that has to be dismissed.
const FLASH: Duration = Duration::from_millis(1000);
const OFF_LABEL: &str = "off";

pub enum ThinkingPickerAction {
    Consumed,
    Select(ThinkingConfig),
    Closed,
}

struct ThinkingItem {
    label: String,
    /// What the row resolves to on this model, when that is not what the row is
    /// called: a budget model spells `high` as a token count.
    detail: Option<String>,
    config: ThinkingConfig,
    resolved: ResolvedThinking,
}

impl ThinkingItem {
    fn new(label: String, config: ThinkingConfig, model: &Model) -> Self {
        let resolved = config.resolve(model);
        let rendered = resolved.to_string();
        let detail = (rendered != label).then_some(rendered);
        Self {
            label,
            detail,
            config,
            resolved,
        }
    }
}

impl PickerItem for ThinkingItem {
    fn label(&self) -> &str {
        &self.label
    }

    fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// The reasoning modes the current model offers. Opened interactively from the
/// status bar chip, and flashed by the keyboard cycle as live feedback: while
/// flashing it draws but routes nothing, so a repeated shortcut keeps cycling
/// and the composer keeps focus.
pub struct ThinkingPicker {
    picker: ListPicker<ThinkingItem>,
    flash_until: Option<Instant>,
}

impl ThinkingPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_max_visible(MAX_VISIBLE),
            flash_until: None,
        }
    }

    pub fn open(&mut self, model: &Model, current: &ThinkingConfig) {
        self.flash_until = None;
        self.fill(model, current);
    }

    pub fn flash(&mut self, model: &Model, current: &ThinkingConfig) {
        self.fill(model, current);
        self.flash_until = Some(Instant::now() + FLASH);
    }

    /// Drops a preview without touching a picker the user opened themselves.
    pub fn clear_flash(&mut self) {
        if self.flash_until.take().is_some() {
            self.picker.close();
        }
    }

    /// True only while the picker answers to input. A flash is drawn without
    /// being open, which is what keeps it out of `any_overlay_open`, off the
    /// modal path, and away from every routing site.
    pub fn is_open(&self) -> bool {
        self.picker.is_open() && self.flash_until.is_none()
    }

    pub fn is_visible(&self) -> bool {
        self.picker.is_open()
    }

    /// Expires the preview on the clock. Here rather than in `view`, which
    /// stays a pure render; see [`crate::repaint`].
    pub fn tick(&mut self) -> Dirty {
        match self.flash_until {
            Some(deadline) if Instant::now() >= deadline => {
                self.close();
                Dirty::YES
            }
            _ => Dirty::NO,
        }
    }

    pub fn close(&mut self) {
        self.flash_until = None;
        self.picker.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ThinkingPickerAction {
        Self::map_action(self.picker.handle_key(key))
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> ThinkingPickerAction {
        Self::map_action(self.picker.handle_mouse(event))
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }

    #[cfg(test)]
    pub fn selected_label(&self) -> Option<&str> {
        self.picker.selected_item().map(|item| item.label.as_str())
    }

    /// The current mode is found by what it resolves to, not by what it is
    /// called: a model that cannot stop reasoning resolves `off` to its
    /// shallowest level, and a budget model resolves every level to a token
    /// count. Matching labels would leave both of them on the wrong row.
    fn fill(&mut self, model: &Model, current: &ThinkingConfig) {
        let items = Self::rows(model);
        self.picker.open(items, TITLE);
        let resolved = current.resolve(model);
        if !self.picker.select_item_by(|item| item.resolved == resolved) {
            self.picker.select(0);
        }
    }

    /// The modes the keyboard cycle walks, so the two never disagree.
    fn rows(model: &Model) -> Vec<ThinkingItem> {
        let mut items = Vec::new();
        if !model.requires_thinking() {
            items.push(ThinkingItem::new(
                OFF_LABEL.to_owned(),
                ThinkingConfig::Off,
                model,
            ));
        }
        items.extend(
            model
                .reasoning_options()
                .effort_ladder()
                .into_iter()
                .map(|level| {
                    ThinkingItem::new(
                        level.to_owned(),
                        ThinkingConfig::Effort(level.into()),
                        model,
                    )
                }),
        );
        items
    }

    fn map_action(action: PickerAction<ThinkingItem>) -> ThinkingPickerAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => ThinkingPickerAction::Consumed,
            PickerAction::Select(item) => ThinkingPickerAction::Select(item.config),
            PickerAction::Close => ThinkingPickerAction::Closed,
        }
    }
}

impl Overlay for ThinkingPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        match self.flash_until {
            Some(deadline) => Cadence::due(deadline.saturating_duration_since(Instant::now())),
            None => self.picker.cadence(),
        }
    }
}

#[cfg(test)]
mod tests {
    use caudra_providers::{ReasoningOption, ReasoningOptions, ThinkingSupport};
    use crossterm::event::KeyCode;
    use test_case::test_case;

    use super::{
        Cadence, Dirty, Instant, Model, OFF_LABEL, Overlay, ThinkingConfig, ThinkingPicker,
        ThinkingPickerAction,
    };
    use crate::components::{key, test_model};

    const OFF_OFFERED: &str = "a model that can stop reasoning must offer the off row";
    const OFF_WITHHELD: &str = "a model that cannot stop reasoning has no off row to offer";
    const PRESELECTED: &str = "the picker must open on the mode already in force";
    const BUDGET_SHOWN: &str = "a level that resolves to a token count must say which";
    const SELECT_APPLIES: &str = "picking a row must hand back the config it stands for";
    const FLASH_DRAWS: &str = "a flash must draw";
    const FLASH_IS_NOT_OPEN: &str = "a flash must not answer to input";
    const FLASH_EXPIRES: &str = "a flash must close itself on the clock";
    const FLASH_OWES_A_FRAME: &str = "a flash must owe the frame that clears it";

    fn budget_model() -> Model {
        Model {
            reasoning_options: ReasoningOptions::new(vec![ReasoningOption::BudgetTokens {
                min: Some(1024),
                max: Some(32_768),
            }]),
            ..test_model()
        }
    }

    fn labels(picker: &ThinkingPicker) -> Vec<String> {
        (0..)
            .map_while(|idx| picker.picker.item(idx).map(|item| item.label.clone()))
            .collect()
    }

    #[test]
    fn rows_omit_off_when_the_model_cannot_stop_reasoning() {
        let mut picker = ThinkingPicker::new();
        picker.open(&test_model(), &ThinkingConfig::Off);
        assert!(
            labels(&picker).contains(&OFF_LABEL.to_owned()),
            "{OFF_OFFERED}"
        );

        let required = Model {
            thinking_override: Some(ThinkingSupport::Required),
            ..test_model()
        };
        picker.open(&required, &ThinkingConfig::Off);
        assert!(
            !labels(&picker).contains(&OFF_LABEL.to_owned()),
            "{OFF_WITHHELD}"
        );
    }

    #[test_case(ThinkingConfig::Off, OFF_LABEL ; "off")]
    #[test_case(ThinkingConfig::Effort("medium".into()), "medium" ; "mid_ladder")]
    fn rows_preselect_the_current_mode(current: ThinkingConfig, expected: &str) {
        let mut picker = ThinkingPicker::new();
        picker.open(&test_model(), &current);
        let selected = picker.picker.selected_item().expect(PRESELECTED);
        assert_eq!(selected.label, expected, "{PRESELECTED}");
    }

    /// `off` resolves to the shallowest level the model offers, so the row that
    /// level names is the one already in force.
    #[test]
    fn a_model_that_cannot_stop_preselects_its_resolved_level() {
        let required = Model {
            thinking_override: Some(ThinkingSupport::Required),
            ..test_model()
        };
        let mut picker = ThinkingPicker::new();
        picker.open(&required, &ThinkingConfig::Off);
        let selected = picker.picker.selected_item().expect(PRESELECTED);
        assert_eq!(selected.label, "minimal", "{PRESELECTED}");
    }

    /// Two rows resolving to two budgets, so a label match would pick neither.
    #[test]
    fn budget_rows_show_the_token_count_they_resolve_to() {
        let model = budget_model();
        let mut picker = ThinkingPicker::new();
        picker.open(&model, &ThinkingConfig::Effort("max".into()));

        let selected = picker.picker.selected_item().expect(PRESELECTED);
        assert_eq!(selected.label, "max", "{PRESELECTED}");
        assert_eq!(selected.detail.as_deref(), Some("4096"), "{BUDGET_SHOWN}");
    }

    #[test]
    fn selecting_a_row_returns_its_config() {
        let mut picker = ThinkingPicker::new();
        picker.open(&test_model(), &ThinkingConfig::Off);
        picker.picker.select_item_by(|item| item.label == "high");

        let action = picker.handle_key(key(KeyCode::Enter));

        assert!(
            matches!(action, ThinkingPickerAction::Select(ThinkingConfig::Effort(ref level)) if &**level == "high"),
            "{SELECT_APPLIES}"
        );
    }

    #[test]
    fn a_flash_is_visible_but_not_open() {
        let mut picker = ThinkingPicker::new();
        picker.flash(&test_model(), &ThinkingConfig::Effort("low".into()));

        assert!(picker.is_visible(), "{FLASH_DRAWS}");
        assert!(!Overlay::is_open(&picker), "{FLASH_IS_NOT_OPEN}");
        assert_ne!(
            Overlay::cadence(&picker),
            Cadence::IDLE,
            "{FLASH_OWES_A_FRAME}"
        );
    }

    #[test]
    fn an_expired_flash_closes_on_tick() {
        let mut picker = ThinkingPicker::new();
        picker.flash(&test_model(), &ThinkingConfig::Effort("low".into()));
        assert_eq!(picker.tick(), Dirty::NO, "a live flash must stay on screen");

        picker.flash_until = Some(Instant::now());

        assert_eq!(picker.tick(), Dirty::YES, "{FLASH_OWES_A_FRAME}");
        assert!(!picker.is_visible(), "{FLASH_EXPIRES}");
    }

    #[test]
    fn clearing_a_flash_leaves_an_opened_picker_alone() {
        let mut picker = ThinkingPicker::new();
        picker.open(&test_model(), &ThinkingConfig::Off);

        picker.clear_flash();

        assert!(
            picker.is_open(),
            "a picker the user opened is not a preview"
        );
    }
}
