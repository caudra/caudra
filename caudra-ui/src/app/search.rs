//! The search modal's app side: the fuzzy search of the open chat, the past-session searches it
//! runs off the UI thread, and going to what they found.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use caudra_agent::CancelToken;
use caudra_agent::CancelTrigger;
use caudra_agent::decisions::DecisionFeature;
use caudra_agent::history::{
    self, HistoryProject, HistoryResults, HistorySearch, HistoryStore, MAX_LIMIT, Ranking,
    parse_filters,
};
use caudra_agent::memory::relevance::Judge;
use caudra_config::decisions::FeatureMode;
use caudra_storage::history_search::{
    HistoryQuery, HistoryRef, HistoryScope, HistoryStream, highlight_history,
};
use caudra_storage::id::CaudraId;
use serde_json::json;

use super::App;
use super::tasks::MAIN_TASK_ID;
use crate::chat::Chat;
use crate::components::Action;
use crate::components::search_modal::{
    HistoryPreview, HistoryStatus, SearchAction, SearchModal, SearchTab,
};
use crate::repaint::Dirty;

/// How long typing has to pause before a past-session search runs.
const QUERY_DEBOUNCE: Duration = Duration::from_millis(120);
/// How long the keyword answer stands before the decision engine is asked to rerank it, so a
/// query still being typed never costs a rerank.
const RERANK_SETTLE: Duration = Duration::from_millis(600);
/// How long a jump waits for the transcript it lands in to load.
const JUMP_WAIT: Duration = Duration::from_secs(5);
const PREVIEW_BYTES: usize = 16 * 1024;
const PREVIEW_AROUND: usize = 2;
const SURFACE_KEY: &str = "surface";
const SURFACE: &str = "search_modal";
pub(super) const EPHEMERAL: &str =
    "Past sessions aren't searchable: this session's state directory is ephemeral";
pub(super) const NEAREST_MESSAGE: &str =
    "The match isn't shown in the transcript; this is the nearest message before it";
pub(super) const INSIDE_TASK: &str = "The match is inside this task's conversation";
pub(super) const MATCH_GONE: &str = "The match is no longer in this session's transcript";

type SearchTask = smol::Task<Result<HistoryResults, String>>;
type PreviewTask = smol::Task<Result<Option<HistoryPreview>, String>>;

/// The past-session searches in flight for the open modal. Dropping a task cancels it, and
/// dropping a rerank's trigger cancels its questions.
#[derive(Default)]
pub(super) struct HistorySearchRun {
    due: Option<Instant>,
    page: Option<(bool, SearchTask)>,
    rerank_due: Option<Instant>,
    rerank: Option<(CancelTrigger, SearchTask)>,
    preview: Option<(HistoryRef, PreviewTask)>,
    project: Option<HistoryProject>,
    /// A hit this session is still loading the transcript of, and when to give up on it.
    jump: Option<(HistoryRef, Instant)>,
    /// A task transcript being read from the archive, and the item to land on in it.
    archived: Option<(String, CaudraId)>,
}

impl HistorySearchRun {
    fn cancel_searches(&mut self) {
        self.due = None;
        self.page = None;
        self.rerank_due = None;
        self.rerank = None;
    }
}

impl App {
    /// Opens the search modal on `tab`, with `query` typed.
    pub(super) fn open_search(&mut self, tab: SearchTab, query: &str) {
        let chat = &self.chats[self.active_chat];
        let (top, auto) = (chat.scroll_top(), chat.auto_scroll());
        let project = HistoryProject::of_session(
            &self.permissions.project_cwd(),
            self.workspace_session.as_ref(),
        );
        self.search_modal.open_on(tab, query, top, auto);
        self.search_modal.bind_history(
            (!self.storage.is_ephemeral()).then_some(self.state.session.id),
            project.label.clone(),
            self.rerank_offered(),
        );
        self.history_search = HistorySearchRun {
            project: Some(project),
            ..HistorySearchRun::default()
        };
        if tab.searches_history() {
            self.schedule_history_search(Instant::now());
        } else if !query.is_empty() {
            self.handle_search_action(SearchAction::QueryChanged);
        }
    }

    pub(super) fn handle_search_action(&mut self, action: SearchAction) -> Vec<Action> {
        let history = self.search_modal.tab().searches_history();
        match action {
            SearchAction::Consumed => {}
            SearchAction::QueryChanged if history => {
                self.schedule_history_search(Instant::now() + QUERY_DEBOUNCE);
            }
            SearchAction::QueryChanged => {
                let chat = &mut self.chats[self.active_chat];
                let texts = chat.segment_search_texts();
                self.search_modal.update_matches(&texts);
                sync_search_highlight(&self.search_modal, chat);
            }
            SearchAction::Navigate if history => self.follow_history_selection(),
            SearchAction::Navigate => {
                sync_search_highlight(&self.search_modal, &mut self.chats[self.active_chat]);
            }
            SearchAction::Copy(text) => self.copy_to_clipboard(&text),
            SearchAction::Cut(text) => {
                self.copy_to_clipboard(&text);
                return self.handle_search_action(SearchAction::QueryChanged);
            }
            SearchAction::TabChanged(saved) => {
                let chat = &mut self.chats[self.active_chat];
                chat.set_highlight_segment(None);
                if let Some((top, auto)) = saved {
                    chat.restore_scroll(top, auto);
                }
                self.history_search.cancel_searches();
                self.history_search.preview = None;
                match history {
                    true => self.schedule_history_search(Instant::now()),
                    false => return self.handle_search_action(SearchAction::QueryChanged),
                }
            }
            SearchAction::Select(idx) => {
                let chat = &mut self.chats[self.active_chat];
                chat.scroll_to_segment(idx);
                chat.set_highlight_segment(None);
                self.close_search();
            }
            SearchAction::Close(saved) => {
                let chat = &mut self.chats[self.active_chat];
                chat.set_highlight_segment(None);
                if let Some((top, auto)) = saved {
                    chat.restore_scroll(top, auto);
                }
                self.close_search();
            }
            SearchAction::Open(hit) => {
                self.chats[self.active_chat].set_highlight_segment(None);
                self.close_search();
                let reference = hit.reference;
                if reference.session == self.state.session.id {
                    self.jump_to_history(reference);
                    return Vec::new();
                }
                let cwd = Path::new(&hit.project)
                    .is_absolute()
                    .then(|| PathBuf::from(&hit.project));
                return vec![Action::OpenHistoryHit { reference, cwd }];
            }
        }
        Vec::new()
    }

    fn close_search(&mut self) {
        self.search_modal.close();
        let jump = self.history_search.jump.take();
        let archived = self.history_search.archived.take();
        self.history_search = HistorySearchRun {
            jump,
            archived,
            ..HistorySearchRun::default()
        };
    }

    /// Whether the decision engine reranks past-session searches for the reader: it is on for
    /// them and acts on its answers.
    fn rerank_offered(&self) -> bool {
        self.permissions.decisions().is_some_and(|decisions| {
            decisions.enabled(&DecisionFeature::HistorySearch)
                && *decisions.mode(&DecisionFeature::HistorySearch) != FeatureMode::Shadow
        })
    }

    fn schedule_history_search(&mut self, at: Instant) {
        self.history_search.cancel_searches();
        let status = if self.storage.is_ephemeral() {
            HistoryStatus::Unavailable(EPHEMERAL.to_owned())
        } else if self.history_request(0, false).is_some() {
            self.history_search.due = Some(at);
            HistoryStatus::Searching
        } else {
            HistoryStatus::Empty
        };
        let pane = self.search_modal.history_mut();
        pane.set_reranking(false);
        pane.set_status(status);
    }

    /// What the modal's query asks for, from `offset`. `None` while it asks for nothing.
    fn history_request(&self, offset: usize, rerank: bool) -> Option<HistorySearch> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let (terms, filters) = parse_filters(&self.search_modal.query(), now_ms);
        if terms.trim().is_empty() && filters.is_empty() {
            return None;
        }
        let scope = match self.search_modal.tab() {
            SearchTab::ThisChat => return None,
            SearchTab::Project => {
                HistoryScope::Project(self.history_search.project.as_ref()?.scope.clone())
            }
            SearchTab::AllProjects => HistoryScope::All,
        };
        let mut query = HistoryQuery::new(terms, scope);
        filters.apply(&mut query);
        query.sort = self.search_modal.history().sort();
        query.offset = offset;
        Some(HistorySearch {
            query,
            rerank,
            limit: Some(MAX_LIMIT),
        })
    }

    fn spawn_search(&self, search: HistorySearch, judge: Option<Judge>) -> SearchTask {
        let store = HistoryStore::new(self.storage.clone());
        smol::spawn(async move {
            history::search(&store, search, judge.as_ref())
                .await
                .map_err(|error| format!("History search failed: {error}"))
        })
    }

    fn search_judge(&self, cancel: CancelToken) -> Option<Judge> {
        Some(Judge {
            decisions: self.permissions.decisions()?,
            context: self
                .permissions
                .decision_context(json!({ SURFACE_KEY: SURFACE })),
            permissions: None,
            cancel,
        })
    }

    /// The preview follows the selection, and reaching the last hit loads the next page.
    fn follow_history_selection(&mut self) {
        let pane = self.search_modal.history();
        let wants_page = pane.at_end()
            && pane.more()
            && self.history_search.page.is_none()
            && self.history_search.rerank.is_none()
            && matches!(
                pane.status,
                HistoryStatus::Ready {
                    ranking: Ranking::Keyword { .. },
                    ..
                }
            );
        if wants_page && let Some(search) = self.history_request(pane.hits().len(), false) {
            self.history_search.page = Some((true, self.spawn_search(search, None)));
        }
        self.request_preview();
    }

    fn request_preview(&mut self) {
        let pane = self.search_modal.history();
        let Some(hit) = pane.selected_hit() else {
            self.history_search.preview = None;
            return;
        };
        let reference = hit.reference.clone();
        let current = pane.preview_ref() == Some(&reference)
            || self
                .history_search
                .preview
                .as_ref()
                .is_some_and(|(pending, _)| *pending == reference);
        if current {
            return;
        }
        let store = HistoryStore::new(self.storage.clone());
        let query = parse_filters(&self.search_modal.query(), 0).0;
        let read = reference.clone();
        self.history_search.preview = Some((
            reference,
            smol::spawn(async move {
                store
                    .read(move |database| {
                        let Some(entry) =
                            database.read_history(&read, 0, PREVIEW_BYTES, PREVIEW_AROUND)?
                        else {
                            return Ok(None);
                        };
                        let highlights = highlight_history(&entry.text, &query)?;
                        Ok(Some(HistoryPreview { entry, highlights }))
                    })
                    .await
                    .map_err(|error| format!("Could not read the match: {error}"))
            }),
        ));
    }

    /// Runs what is due and takes in what finished. `now` is the clock the debounce and the
    /// rerank delay are read against.
    pub(super) fn poll_history_search(&mut self, now: Instant) -> Dirty {
        let mut dirty = self.poll_history_jump(now);
        if !self.search_modal.is_open() {
            return dirty;
        }
        let run = &mut self.history_search;
        if run.due.is_some_and(|due| due <= now) {
            run.due = None;
            if let Some(search) = self.history_request(0, false) {
                self.history_search.page = Some((false, self.spawn_search(search, None)));
            }
        }
        if self
            .history_search
            .page
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
            && let Some((append, task)) = self.history_search.page.take()
        {
            let result = smol::block_on(task);
            self.take_history_page(result, append, now);
            dirty = Dirty::YES;
        }
        if self.history_search.rerank_due.is_some_and(|due| due <= now) {
            self.history_search.rerank_due = None;
            let (trigger, cancel) = CancelToken::new();
            if let Some(search) = self.history_request(0, true)
                && let Some(judge) = self.search_judge(cancel)
            {
                self.history_search.rerank =
                    Some((trigger, self.spawn_search(search, Some(judge))));
                self.search_modal.history_mut().set_reranking(true);
                dirty = Dirty::YES;
            }
        }
        if self
            .history_search
            .rerank
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
            && let Some((_, task)) = self.history_search.rerank.take()
        {
            self.take_reranked(smol::block_on(task));
            dirty = Dirty::YES;
        }
        if self
            .history_search
            .preview
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
            && let Some((reference, task)) = self.history_search.preview.take()
        {
            let pane = self.search_modal.history_mut();
            if pane.selected_hit().map(|hit| &hit.reference) == Some(&reference) {
                match smol::block_on(task) {
                    Ok(preview) => pane.set_preview(preview),
                    Err(error) => self.flash(error),
                }
            }
            dirty = Dirty::YES;
        }
        dirty
    }

    fn take_history_page(
        &mut self,
        result: Result<HistoryResults, String>,
        append: bool,
        now: Instant,
    ) {
        let pane = self.search_modal.history_mut();
        let results = match result {
            Ok(results) => results,
            Err(error) => {
                pane.set_status(HistoryStatus::Failed(error));
                return;
            }
        };
        let status = ready(&results);
        let wants_rerank = !append && pane.wants_rerank() && !results.hits.is_empty();
        pane.set_hits(results.hits, results.more, append, status);
        let has_terms = !parse_filters(&self.search_modal.query(), 0)
            .0
            .trim()
            .is_empty();
        if wants_rerank && has_terms {
            self.history_search.rerank_due = Some(now + RERANK_SETTLE);
        }
        self.request_preview();
    }

    fn take_reranked(&mut self, result: Result<HistoryResults, String>) {
        let pane = self.search_modal.history_mut();
        pane.set_reranking(false);
        match result {
            Ok(results) if matches!(results.ranking, Ranking::Reranked { .. }) => {
                let status = ready(&results);
                pane.set_hits(results.hits, results.more, false, status);
                self.request_preview();
            }
            Ok(results) => {
                if let HistoryStatus::Ready { note, .. } = &mut pane.status {
                    *note = results.note;
                }
            }
            Err(error) => self.flash(error),
        }
    }

    /// Goes to `reference` in this session: the main conversation or a task's.
    pub(crate) fn jump_to_history(&mut self, reference: HistoryRef) {
        match &reference.stream {
            HistoryStream::Main => {
                if let Err(error) = self.focus_task(MAIN_TASK_ID) {
                    self.flash(error);
                    return;
                }
                self.history_search.jump = Some((reference, Instant::now() + JUMP_WAIT));
                let _ = self.poll_history_jump(Instant::now());
            }
            HistoryStream::Subagent(key) => self.jump_into_task(key.clone(), reference.item),
        }
    }

    /// A task's transcript: its chat while it is open, else read back from the archive, else
    /// the task's card in the main conversation.
    /// `stream` names the transcript by the call that ran it, or by the task for its first run.
    fn jump_into_task(&mut self, stream: String, item: CaudraId) {
        let open = self
            .chats
            .iter()
            .find(|chat| {
                chat.parent_tool_use_id().is_some_and(|id| **id == *stream)
                    || chat.task_id().is_some_and(|task| **task == *stream)
            })
            .and_then(|chat| chat.task_id().map(|task| task.to_string()));
        if let Some(task) = open
            && self.focus_task(&task).is_ok()
        {
            if let Some(note) = land(&mut self.chats[self.active_chat], item) {
                self.flash(note.to_owned());
            }
            return;
        }
        let cards = self.task_history_cards();
        let card = cards
            .iter()
            .find(|card| card.call_id == stream)
            .or_else(|| cards.iter().find(|card| card.task_id == stream));
        let call = card.map_or(stream.as_str(), |card| card.call_id.as_str());
        if let Some(card) = card
            && self.load_archived_chat(&card.task_id)
        {
            self.history_search.archived = Some((card.task_id.clone(), item));
            return;
        }
        if self.focus_task(MAIN_TASK_ID).is_err() {
            return;
        }
        let chat = &mut self.chats[self.active_chat];
        match chat.message_for_tool(call) {
            Some(index) => {
                chat.land_on_message(index);
                self.flash(INSIDE_TASK.to_owned());
            }
            None => self.flash(MATCH_GONE.to_owned()),
        }
    }

    /// Lands on an archived task transcript that just opened, when a search asked for it.
    pub(super) fn land_archived_jump(&mut self, task: &str) {
        let Some((_, item)) = self
            .history_search
            .archived
            .take_if(|(waiting, _)| waiting == task)
        else {
            return;
        };
        if let Some(note) = land(&mut self.chats[self.active_chat], item) {
            self.flash(note.to_owned());
        }
    }

    /// Lands a pending jump once the main transcript has loaded, or gives up when it never does.
    fn poll_history_jump(&mut self, now: Instant) -> Dirty {
        let Some((reference, deadline)) = &self.history_search.jump else {
            return Dirty::NO;
        };
        let chat = &mut self.chats[self.active_chat];
        if chat.message_count() == 0 && now < *deadline {
            return Dirty::NO;
        }
        let item = reference.item;
        self.history_search.jump = None;
        if let Some(note) = land(chat, item) {
            self.flash(note.to_owned());
        }
        Dirty::YES
    }
}

/// Scrolls `chat` to `item` and marks it. What to tell the reader, when the landing is not
/// exactly the match.
fn land(chat: &mut Chat, item: CaudraId) -> Option<&'static str> {
    match chat.message_for_item(item) {
        Some((index, exact)) => {
            chat.land_on_message(index);
            (!exact).then_some(NEAREST_MESSAGE)
        }
        None => Some(MATCH_GONE),
    }
}

fn ready(results: &HistoryResults) -> HistoryStatus {
    HistoryStatus::Ready {
        ranking: results.ranking,
        judged_unrelated: results.judged_unrelated(),
        unindexed_sessions: results.unindexed_sessions,
        note: results.note.clone(),
    }
}

fn sync_search_highlight(modal: &SearchModal, chat: &mut Chat) {
    let idx = modal.current_segment_index();
    if let Some(i) = idx {
        chat.scroll_to_segment(i);
    }
    chat.set_highlight_segment(idx);
}

#[cfg(test)]
mod tests {
    use caudra_providers::Message;
    use caudra_storage::history_search::{HistoryHit, HistoryKind, Snippet};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use test_case::test_case;

    use super::*;
    use crate::AppSession;
    use crate::app::Msg;
    use crate::app::mouse::SELECTION_COPIED;
    use crate::app::tests::{mouse_event, test_app};
    use crate::components::command::{ParsedCommand, SEARCH_COMMAND};
    use crate::components::{DisplayMessage, DisplayRole, DisplaySource};

    const WORD: &str = "ledger";
    const ELSEWHERE: &str = "/work/elsewhere";
    const MODEL: &str = "anthropic/claude-sonnet-4-5";
    /// Long enough for a loaded machine, and never reached when the search works.
    const SEARCH_DEADLINE: Duration = Duration::from_secs(30);
    const MISSED_LANDING: &str = "the jump did not land on the message holding the match";
    const MANY_HITS: usize = 60;
    const SCREEN: (u16, u16) = (140, 40);

    /// Saves a session working in `cwd` with one user message saying `text`.
    fn seed(app: &App, cwd: &str, text: &str) -> CaudraId {
        let mut session = AppSession::new(MODEL, cwd);
        session.replace_messages(crate::history_items(&[Message::user(text.to_owned())]));
        session.save(&app.storage).unwrap();
        session.id
    }

    /// Runs the debounced search and waits for its answer.
    fn settle(app: &mut App) {
        let deadline = Instant::now() + SEARCH_DEADLINE;
        loop {
            let _ = app.poll_history_search(Instant::now() + QUERY_DEBOUNCE);
            let run = &app.history_search;
            if run.due.is_none() && run.page.is_none() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the history search never finished"
            );
            std::thread::yield_now();
        }
    }

    /// Ids in the order items would have been made, however close together they were generated.
    fn ascending<const N: usize>() -> [CaudraId; N] {
        let mut ids = [(); N].map(|()| CaudraId::generate());
        ids.sort();
        ids
    }

    fn message(role: DisplayRole, source: DisplaySource) -> DisplayMessage {
        let mut message = DisplayMessage::new(role, WORD.to_owned());
        message.source = Some(source);
        message
    }

    fn main_reference(app: &App, item: CaudraId) -> HistoryRef {
        HistoryRef {
            session: app.state.session.id,
            stream: HistoryStream::Main,
            item,
        }
    }

    #[test_case("", SearchTab::Project ; "project_by_default")]
    #[test_case("chat ", SearchTab::ThisChat ; "chat")]
    #[test_case("Project ", SearchTab::Project ; "project_ignoring_case")]
    #[test_case("all ", SearchTab::AllProjects ; "all")]
    fn the_search_command_opens_the_named_tab_with_its_words(tab_word: &str, tab: SearchTab) {
        let mut app = test_app();

        app.execute_command(
            ParsedCommand {
                name: SEARCH_COMMAND.into(),
                args: format!(" {tab_word}{WORD} "),
            },
            0,
        );

        assert!(app.search_modal.is_open());
        assert_eq!(app.search_modal.tab(), tab);
        assert_eq!(app.search_modal.query(), WORD);
        if tab != SearchTab::ThisChat {
            assert_eq!(app.search_modal.history().status, HistoryStatus::Searching);
        }
    }

    #[test]
    fn typing_waits_out_the_debounce_before_searching() {
        let mut app = test_app();
        app.open_search(SearchTab::AllProjects, "");

        app.update(Msg::Paste(WORD.to_owned()));
        let _ = app.poll_history_search(Instant::now());

        assert!(app.history_search.page.is_none());
        assert!(app.history_search.due.is_some());
        let _ = app.poll_history_search(Instant::now() + QUERY_DEBOUNCE);
        assert!(app.history_search.page.is_some());
    }

    #[test]
    fn another_sessions_hit_is_opened_where_that_session_works() {
        let mut app = test_app();
        let other = seed(&app, ELSEWHERE, &format!("the {WORD} reconciles nightly"));
        app.open_search(SearchTab::AllProjects, WORD);
        settle(&mut app);
        let found = app.search_modal.history().hits();
        assert_eq!(found.len(), 1, "{:?}", app.search_modal.history().status);
        assert_eq!(found[0].reference.session, other);

        let actions = app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

        assert!(!app.search_modal.is_open());
        assert!(matches!(
            actions.as_slice(),
            [Action::OpenHistoryHit { reference, cwd }]
                if reference.session == other && cwd.as_deref() == Some(Path::new(ELSEWHERE))
        ));
    }

    #[test]
    fn a_hit_in_this_session_lands_on_its_message() {
        let mut app = test_app();
        let [before, item, after] = ascending();
        let chat = app.main_chat();
        chat.push(message(DisplayRole::User, DisplaySource::User(before)));
        chat.push(message(
            DisplayRole::Assistant,
            DisplaySource::AssistantText(item),
        ));
        chat.push(message(DisplayRole::User, DisplaySource::User(after)));

        app.jump_to_history(main_reference(&app, item));

        assert_eq!(app.main_chat().landing(), Some(1), "{MISSED_LANDING}");
        assert_eq!(app.status_bar.flash_text(), None);
    }

    #[test]
    fn a_hidden_item_lands_on_the_nearest_message_before_it() {
        let mut app = test_app();
        let [shown, hidden, later] = ascending();
        let chat = app.main_chat();
        chat.push(message(DisplayRole::User, DisplaySource::User(shown)));
        chat.push(message(DisplayRole::User, DisplaySource::User(later)));

        app.jump_to_history(main_reference(&app, hidden));

        assert_eq!(app.main_chat().landing(), Some(0), "{MISSED_LANDING}");
        assert_eq!(app.status_bar.flash_text(), Some(NEAREST_MESSAGE));
    }

    fn hits(count: usize) -> Vec<HistoryHit> {
        (0..count)
            .map(|index| HistoryHit {
                reference: HistoryRef {
                    session: CaudraId::generate(),
                    stream: HistoryStream::Main,
                    item: CaudraId::generate(),
                },
                session_title: format!("{WORD} {index}"),
                project: ELSEWHERE.to_owned(),
                kind: HistoryKind::User,
                tool: None,
                at_ms: 0,
                score: 1.0,
                snippet: Snippet {
                    text: WORD.to_owned(),
                    highlights: Vec::new(),
                },
            })
            .collect()
    }

    /// The history tab drawn wide enough for both panes, with more hits than fit.
    fn drawn_history(app: &mut App) -> [Rect; 2] {
        app.open_search(SearchTab::AllProjects, "");
        let status = HistoryStatus::Ready {
            ranking: Ranking::Keyword { matched: MANY_HITS },
            judged_unrelated: 0,
            unindexed_sessions: 0,
            note: None,
        };
        app.search_modal
            .history_mut()
            .set_hits(hits(MANY_HITS), false, false, status);
        let (width, height) = SCREEN;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.view(frame)).unwrap();
        app.search_modal.text_panes()
    }

    fn press(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        app.update(mouse_event(kind, column, row));
    }

    #[test]
    fn a_sweep_from_the_hits_stays_in_the_hits() {
        let mut app = test_app();
        let [list, preview] = drawn_history(&mut app);
        assert!(
            !preview.is_empty(),
            "the screen is too narrow for the preview"
        );

        press(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            list.x + 1,
            list.y,
        );
        press(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            preview.x + 4,
            preview.y + 2,
        );

        let selected = app.selection_state.as_ref().map(|state| state.sel().area);
        assert_eq!(
            selected,
            Some(list),
            "the selection left the pane it started in"
        );
    }

    #[test]
    fn dragging_a_scrollbar_selects_nothing() {
        let mut app = test_app();
        let [list, _] = drawn_history(&mut app);
        let bar = list.right();

        press(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            bar,
            list.y,
        );
        press(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            bar,
            list.bottom() - 1,
        );
        press(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            bar,
            list.bottom() - 1,
        );

        assert!(
            app.selection_state.is_none(),
            "the bar's drag selected text"
        );
        assert_ne!(app.status_bar.flash_text(), Some(SELECTION_COPIED));
    }

    #[test]
    fn a_key_press_clears_the_landing_mark() {
        let mut app = test_app();
        let item = CaudraId::generate();
        app.main_chat()
            .push(message(DisplayRole::User, DisplaySource::User(item)));
        app.jump_to_history(main_reference(&app, item));

        app.update(Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));

        assert_eq!(app.main_chat().landing(), None);
    }
}
