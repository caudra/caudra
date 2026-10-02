use caudra_agent::permissions::{
    COMMAND_TEMPLATE_PREFIX, ComposedRow, PermissionAnswer, PermissionLifetime, PermissionRequest,
    PermissionRowGrant, grade_command_pattern,
};

use super::choices::{NO, ONCE_ONLY, grant_label};
use super::decision::{default_grant, grant_option, offers, per_row, row_positions, step};
use super::notes::{RowStatus, row_status};
use super::{Panel, PermissionDecision, PermissionPrompt, PromptState, RowChoice, front_request};

const REMEMBER_AS_LISTED: &str = "Yes, and remember as listed";
const RUN_THEM_ONCE: &str = "Yes, run them once";
const MORE_OPTIONS: &str = "More options for the whole script…";
pub(super) const OWN_PATTERN: &str = "Your own pattern…";
const SUGGESTED: &str = "suggested";
const EXACT_PAGE_LABEL: &str = "This exact command";
const ONCE_PAGE_LABEL: &str = "This time only";

/// What one line of a page offers: a place on the row's ladder, or writing
/// a pattern of one's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PageItem {
    Grant(Option<PermissionRowGrant>),
    OwnPattern,
}

/// The answers on Review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReviewChoice {
    Remember,
    Deny,
    More,
}

pub(super) const REVIEW_CHOICES: [ReviewChoice; 3] = [
    ReviewChoice::Remember,
    ReviewChoice::Deny,
    ReviewChoice::More,
];

/// A draft answer built one command at a time. It starts from the main
/// view's rows, and only Review applies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StepThrough {
    /// The row whose page is shown, or `None` while Review is.
    pub page: Option<usize>,
    pub rows: Vec<RowChoice>,
    pub lifetimes: Vec<PermissionLifetime>,
    pub visited: Vec<bool>,
    /// The highlighted item of the page, or the highlighted Review choice.
    pub highlight: usize,
}

/// The lifetimes a remembered rung allows, longest last.
pub(super) fn rung_lifetimes(
    request: &PermissionRequest,
    row: usize,
    grant: &PermissionRowGrant,
    project: bool,
) -> Vec<PermissionLifetime> {
    let Some(option) = grant_option(request, row, grant) else {
        return Vec::new();
    };
    [
        PermissionLifetime::Conversation,
        PermissionLifetime::Project,
        PermissionLifetime::Global,
    ]
    .into_iter()
    .filter(|lifetime| {
        option.allowed_lifetimes.contains(lifetime)
            && (project || *lifetime != PermissionLifetime::Project)
    })
    .collect()
}

pub(crate) fn lifetime_phrase(lifetime: &PermissionLifetime) -> &'static str {
    match lifetime {
        PermissionLifetime::Once => ONCE_ONLY,
        PermissionLifetime::Conversation => "this conversation",
        PermissionLifetime::Project => "this project",
        PermissionLifetime::Global => "all projects",
    }
}

impl StepThrough {
    pub(super) fn grant(
        &self,
        request: &PermissionRequest,
        row: usize,
    ) -> Option<PermissionRowGrant> {
        match self.rows.get(row)? {
            RowChoice::Chosen(grant) => grant.clone(),
            RowChoice::Default => default_grant(request, row),
        }
    }

    /// The answer Review would give: every remembered row for its own
    /// lifetime.
    pub(super) fn answer(&self, request: &PermissionRequest) -> PermissionAnswer {
        let rows: Vec<Option<ComposedRow>> = (0..self.rows.len())
            .map(|row| {
                Some(ComposedRow {
                    grant: self.grant(request, row)?,
                    lifetime: self.lifetimes.get(row)?.clone(),
                })
            })
            .collect();
        if rows.iter().all(Option::is_none) {
            PermissionAnswer::AllowOnce
        } else {
            PermissionAnswer::AllowComposed { rows }
        }
    }

    /// The draft fitted to a newer version of the request: a withdrawn rung
    /// falls back to the row's default and a lifetime it no longer allows
    /// shortens.
    pub(super) fn reconciled(mut self, request: &PermissionRequest, project: bool) -> Self {
        let count = request.resources.len();
        self.rows.resize(count, RowChoice::Default);
        self.lifetimes
            .resize(count, PermissionLifetime::Conversation);
        self.visited.resize(count, false);
        for row in 0..count {
            if let RowChoice::Chosen(Some(grant)) = &self.rows[row]
                && !offers(request, row, grant)
            {
                self.rows[row] = RowChoice::Default;
            }
            self.clamp_lifetime(request, row, project);
        }
        if self.page.is_some_and(|row| {
            row >= count || !per_row(request) || row_status(request, row) != RowStatus::New
        }) {
            self.page = None;
        }
        self
    }

    fn clamp_lifetime(&mut self, request: &PermissionRequest, row: usize, project: bool) {
        let Some(grant) = self.grant(request, row) else {
            return;
        };
        let allowed = rung_lifetimes(request, row, &grant, project);
        if let Some(lifetime) = self.lifetimes.get_mut(row)
            && !allowed.contains(lifetime)
        {
            *lifetime = allowed
                .first()
                .cloned()
                .unwrap_or(PermissionLifetime::Conversation);
        }
    }

    pub(super) fn items(&self, request: &PermissionRequest, row: usize) -> Vec<PageItem> {
        let current = self.grant(request, row);
        let mut items: Vec<PageItem> = row_positions(request, row, &current, true)
            .into_iter()
            .map(PageItem::Grant)
            .collect();
        items.push(PageItem::OwnPattern);
        items
    }
}

/// A rung's label on a page, capitalised as a choice, and the badges that
/// say where it came from.
pub(super) fn item_label(
    request: &PermissionRequest,
    row: usize,
    item: &PageItem,
) -> (String, Vec<String>) {
    let grant = match item {
        PageItem::OwnPattern => return (OWN_PATTERN.into(), Vec::new()),
        PageItem::Grant(None) => return (ONCE_PAGE_LABEL.into(), Vec::new()),
        PageItem::Grant(Some(grant)) => grant,
    };
    let mut badges = Vec::new();
    if let Some(option) = grant_option(request, row, grant) {
        if option.id.starts_with(COMMAND_TEMPLATE_PREFIX) {
            badges.push(SUGGESTED.into());
        }
        if let Some(seen) = option.seen {
            badges.push(format!("seen {seen}×"));
        }
    }
    let label = grant_label(request, row, grant);
    let label = match label.strip_prefix("this exact command") {
        Some(rest) => format!("{EXACT_PAGE_LABEL}{rest}"),
        None => label,
    };
    (label, badges)
}

impl PermissionPrompt {
    /// Opens the step-through at the focused row's page, its draft taken
    /// from the main view.
    pub(super) fn open_step_through(&mut self) {
        let Some(request) = self.current() else {
            return;
        };
        let count = request.resources.len();
        let page = self.focus_row.or_else(|| self.first_new_row());
        let mut step = StepThrough {
            page,
            rows: self.rows.clone(),
            lifetimes: vec![PermissionLifetime::Conversation; count],
            visited: vec![false; count],
            highlight: 0,
        };
        let project = self.project_available();
        for row in 0..count {
            step.clamp_lifetime(request, row, project);
        }
        self.step = Some(step);
        self.panel = Panel::StepThrough;
        self.highlight_current_item();
        self.scroll.reset();
        self.invalidate_controls();
    }

    /// Puts the page's highlight on the row's current rung.
    pub(super) fn highlight_current_item(&mut self) {
        let Some(request) = self.current() else {
            return;
        };
        let Some(step) = &self.step else {
            return;
        };
        let highlight = match step.page {
            Some(row) => {
                let current = PageItem::Grant(step.grant(request, row));
                step.items(request, row)
                    .iter()
                    .position(|item| *item == current)
                    .unwrap_or_default()
            }
            None => 0,
        };
        if let Some(step) = &mut self.step {
            step.highlight = highlight;
        }
        self.reveal = true;
    }

    /// Moves to the next or previous page without choosing, Review last.
    pub(super) fn turn_page(&mut self, forward: bool) {
        let pages = self.new_rows();
        let Some(step) = &mut self.step else {
            return;
        };
        let index = step
            .page
            .and_then(|row| pages.iter().position(|page| *page == row))
            .unwrap_or(pages.len());
        let next = if forward {
            (index + 1).min(pages.len())
        } else {
            index.saturating_sub(1)
        };
        step.page = pages.get(next).copied();
        self.highlight_current_item();
        self.scroll.reset();
        self.invalidate_controls();
    }

    pub(super) fn go_to_page(&mut self, page: Option<usize>) {
        if let Some(step) = &mut self.step {
            step.page = page;
        }
        self.highlight_current_item();
        self.scroll.reset();
        self.invalidate_controls();
    }

    /// Chooses the highlighted item of the page, or the Review choice.
    pub(super) fn choose_step_item(&mut self, index: usize) -> Option<PermissionDecision> {
        let request = front_request(&self.requests)?;
        let step = self.step.as_ref()?;
        let Some(row) = step.page else {
            return self.choose_review(*REVIEW_CHOICES.get(index)?);
        };
        let item = step.items(request, row).get(index)?.clone();
        let project = self.project_available();
        match item {
            PageItem::OwnPattern => {
                let seed = step
                    .grant(request, row)
                    .and_then(|grant| match grant {
                        PermissionRowGrant::Written(pattern) => Some(pattern),
                        _ => None,
                    })
                    .unwrap_or_default();
                self.state = PromptState::PatternEditing;
                self.field.set_text(&seed);
                self.invalidate_controls();
                None
            }
            PageItem::Grant(grant) => {
                let step = self.step.as_mut()?;
                step.rows[row] = RowChoice::Chosen(grant);
                step.visited[row] = true;
                step.clamp_lifetime(request, row, project);
                self.turn_page(true);
                None
            }
        }
    }

    /// Accepts a pattern typed on a page when it matches the page's command.
    pub(super) fn commit_page_pattern(&mut self) -> bool {
        let pattern = self.field.text().trim().to_owned();
        let project = self.project_available();
        let Some(request) = front_request(&self.requests) else {
            return false;
        };
        let Some(step) = self.step.as_mut() else {
            return false;
        };
        let Some(row) = step.page else {
            return false;
        };
        let Some(resource) = request.resources.get(row) else {
            return false;
        };
        if grade_command_pattern(&pattern, &resource.value).is_err() {
            return false;
        }
        step.rows[row] = RowChoice::Chosen(Some(PermissionRowGrant::Written(pattern)));
        step.visited[row] = true;
        step.clamp_lifetime(request, row, project);
        self.state = PromptState::Normal;
        self.field.clear();
        self.turn_page(true);
        true
    }

    /// The lifetimes the shown page's rung allows.
    pub(super) fn page_lifetimes(&self) -> Vec<PermissionLifetime> {
        let (Some(request), Some(step)) = (self.current(), &self.step) else {
            return Vec::new();
        };
        step.page
            .and_then(|row| Some((row, step.grant(request, row)?)))
            .map(|(row, grant)| rung_lifetimes(request, row, &grant, self.project_available()))
            .unwrap_or_default()
    }

    /// Steps `Remember for` along the lifetimes the page's rung allows.
    pub(super) fn step_lifetime(&mut self, forward: bool) {
        let allowed = self.page_lifetimes();
        let Some(current) = self
            .step
            .as_ref()
            .and_then(|step| step.lifetimes.get(step.page?).cloned())
        else {
            return;
        };
        if let Some(next) = step(&allowed, &current, forward) {
            self.set_page_lifetime(next);
        }
    }

    /// Sets `Remember for` on the shown page, when its rung allows `lifetime`.
    pub(super) fn set_page_lifetime(&mut self, lifetime: PermissionLifetime) {
        if !self.page_lifetimes().contains(&lifetime) {
            return;
        }
        if let Some(step) = self.step.as_mut()
            && let Some(row) = step.page
            && let Some(current) = step.lifetimes.get_mut(row)
        {
            *current = lifetime;
        }
        self.invalidate_controls();
    }

    fn choose_review(&mut self, choice: ReviewChoice) -> Option<PermissionDecision> {
        if let Some(index) = REVIEW_CHOICES.iter().position(|found| *found == choice)
            && let Some(step) = &mut self.step
        {
            step.highlight = index;
        }
        match choice {
            ReviewChoice::Deny => {
                self.open_guidance();
                None
            }
            ReviewChoice::More => {
                self.open_customize(true);
                None
            }
            ReviewChoice::Remember => {
                if self.awaiting_review {
                    return None;
                }
                let answer = self.step.as_ref()?.answer(self.current()?);
                self.decide_or_confirm(answer, None)
            }
        }
    }

    pub(super) fn review_sentence(&self, choice: ReviewChoice) -> &'static str {
        match choice {
            ReviewChoice::Remember => {
                let remembers =
                    self.step
                        .as_ref()
                        .zip(self.current())
                        .is_some_and(|(step, request)| {
                            step.answer(request) != PermissionAnswer::AllowOnce
                        });
                if remembers {
                    REMEMBER_AS_LISTED
                } else {
                    RUN_THEM_ONCE
                }
            }
            ReviewChoice::Deny => NO,
            ReviewChoice::More => MORE_OPTIONS,
        }
    }

    /// Leaves the step-through without applying its draft.
    pub(super) fn discard_step_through(&mut self) {
        self.step = None;
        self.panel = Panel::Main;
        self.state = PromptState::Normal;
        self.inspector = None;
        self.field.clear();
        self.scroll.reset();
        self.invalidate_controls();
    }
}
