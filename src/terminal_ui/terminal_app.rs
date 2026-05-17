use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use chrono::{DateTime, Local, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::agent_catalog::{self, AgentCatalogEntry};

use super::client::{
    AgentRunSummary, ChatRequest, ChatStreamEvent, ConversationMessage, DashboardStats,
    EmailListQuery, EmailListResponse, EmailRecord, FolderNode, NetworkCommand, NetworkResult,
    StatusSnapshot, ThreadSummary, DEFAULT_EMAIL_LIMIT,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppTab {
    Chat,
    Emails,
    Folders,
    Status,
}

impl AppTab {
    pub fn title(self) -> &'static str {
        match self {
            AppTab::Chat => "Chat",
            AppTab::Emails => "Emails",
            AppTab::Folders => "Folders",
            AppTab::Status => "Status",
        }
    }

    pub fn index(self) -> usize {
        match self {
            AppTab::Chat => 0,
            AppTab::Emails => 1,
            AppTab::Folders => 2,
            AppTab::Status => 3,
        }
    }

    pub fn from_digit(digit: char) -> Option<Self> {
        match digit {
            '1' => Some(Self::Chat),
            '2' => Some(Self::Emails),
            '3' => Some(Self::Folders),
            '4' => Some(Self::Status),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatFocus {
    Threads,
    Transcript,
    Composer,
}

pub type AgentOption = AgentCatalogEntry;

pub const CATEGORY_OPTIONS: [&str; 10] = [
    "All",
    "personal",
    "work",
    "volunteering",
    "financial",
    "shopping",
    "social",
    "travel",
    "health",
    "education",
];

pub const SPAM_OPTIONS: [&str; 3] = ["All", "not-spam", "spam"];
const STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum AppCommand {
    Quit,
    Network(NetworkCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayState {
    Help,
    AgentPicker { selected: usize },
    DeleteConfirm { thread_id: String },
}

#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    pub label: String,
    pub body: String,
    pub kind: TranscriptEntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptEntryKind {
    User,
    Agent,
    Status,
}

#[derive(Debug, Clone)]
pub struct ThreadListItem {
    pub title: String,
    pub subtitle: String,
    pub is_active: bool,
    pub has_context: bool,
}

#[derive(Debug, Clone)]
pub struct FolderListItem {
    pub name: String,
    pub depth: usize,
    pub message_count: i32,
    pub has_children: bool,
    pub expanded: bool,
}

#[derive(Debug, Clone)]
pub struct App {
    api_url: String,
    pub active_tab: AppTab,
    pub chat_focus: ChatFocus,
    pub overlay: Option<OverlayState>,
    pub agents: Vec<AgentOption>,
    pub threads: Vec<ThreadSummary>,
    pub selected_thread_idx: usize,
    pub active_thread_id: Option<String>,
    pub draft_mode: bool,
    pub messages_by_thread: HashMap<String, Vec<ConversationMessage>>,
    pub composer: String,
    pub draft_agent_idx: usize,
    pub draft_context_email_id: Option<String>,
    pub threads_loading: bool,
    pub messages_loading: bool,
    pub send_pending: bool,
    pub connection_ok: bool,
    pub banner_message: Option<String>,
    pub status_message: String,
    pub transcript_scroll: u16,
    pub pending_user_message: Option<String>,
    pub streaming_response: String,
    pub stream_log: Vec<String>,
    pub pending_stream_thread_id: Option<String>,
    pub emails: Vec<EmailRecord>,
    pub emails_loading: bool,
    pub emails_total_count: i64,
    pub selected_email_idx: usize,
    pub selected_email_id: Option<String>,
    pub email_detail: Option<EmailRecord>,
    pub email_detail_loading: bool,
    pub email_search: String,
    pub email_search_input: String,
    pub email_search_mode: bool,
    pub email_category_idx: usize,
    pub email_spam_idx: usize,
    pub email_folder_filter: Option<String>,
    pub email_detail_expanded: bool,
    pub folders: Vec<FolderNode>,
    pub folders_loading: bool,
    pub selected_folder_idx: usize,
    expanded_folders: HashSet<String>,
    pub status_loading: bool,
    pub status_stats: Option<DashboardStats>,
    pub status_runs: Vec<AgentRunSummary>,
    last_status_refresh: Option<Instant>,
    awaiting_second_g: bool,
}

impl App {
    pub fn new(api_url: String) -> (Self, Vec<AppCommand>) {
        let agents = builtin_agents();
        let default_agent_idx = default_agent_index(&agents);
        (
            Self {
                api_url,
                active_tab: AppTab::Chat,
                chat_focus: ChatFocus::Threads,
                overlay: None,
                agents,
                threads: Vec::new(),
                selected_thread_idx: 0,
                active_thread_id: None,
                draft_mode: false,
                messages_by_thread: HashMap::new(),
                composer: String::new(),
                draft_agent_idx: default_agent_idx,
                draft_context_email_id: None,
                threads_loading: true,
                messages_loading: false,
                send_pending: false,
                connection_ok: false,
                banner_message: None,
                status_message: "Connecting to local API...".to_string(),
                transcript_scroll: 0,
                pending_user_message: None,
                streaming_response: String::new(),
                stream_log: Vec::new(),
                pending_stream_thread_id: None,
                emails: Vec::new(),
                emails_loading: false,
                emails_total_count: 0,
                selected_email_idx: 0,
                selected_email_id: None,
                email_detail: None,
                email_detail_loading: false,
                email_search: String::new(),
                email_search_input: String::new(),
                email_search_mode: false,
                email_category_idx: 0,
                email_spam_idx: 0,
                email_folder_filter: None,
                email_detail_expanded: false,
                folders: Vec::new(),
                folders_loading: false,
                selected_folder_idx: 0,
                expanded_folders: HashSet::new(),
                status_loading: false,
                status_stats: None,
                status_runs: Vec::new(),
                last_status_refresh: None,
                awaiting_second_g: false,
            },
            vec![
                AppCommand::Network(NetworkCommand::LoadAgents),
                AppCommand::Network(NetworkCommand::LoadThreads),
            ],
        )
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn active_thread(&self) -> Option<&ThreadSummary> {
        self.active_thread_id.as_ref().and_then(|thread_id| {
            self.threads
                .iter()
                .find(|thread| &thread.thread_id == thread_id)
        })
    }

    pub fn selected_thread(&self) -> Option<&ThreadSummary> {
        self.threads.get(self.selected_thread_idx)
    }

    pub fn selected_email(&self) -> Option<&EmailRecord> {
        self.selected_email_id.as_ref().and_then(|message_id| {
            self.emails
                .iter()
                .find(|email| &email.message_id == message_id)
        })
    }

    pub fn current_agent(&self) -> AgentOption {
        if let Some(thread) = self.active_thread() {
            return self
                .agents
                .iter()
                .find(|agent| agent.id.eq_ignore_ascii_case(&thread.agent_name))
                .cloned()
                .unwrap_or_else(|| default_chat_agent(&self.agents));
        }
        self.agents
            .get(self.draft_agent_idx)
            .cloned()
            .unwrap_or_else(|| default_chat_agent(&self.agents))
    }

    pub fn find_agent(&self, agent_id: &str) -> AgentOption {
        find_agent(&self.agents, agent_id)
    }

    pub fn connection_label(&self) -> &'static str {
        if self.connection_ok {
            "Connected"
        } else {
            "Disconnected"
        }
    }

    pub fn tab_titles(&self) -> Vec<&'static str> {
        vec!["Chat", "Emails", "Folders", "Status"]
    }

    pub fn thread_items(&self) -> Vec<ThreadListItem> {
        self.threads
            .iter()
            .map(|thread| {
                let title = thread
                    .title
                    .clone()
                    .unwrap_or_else(|| self.find_agent(&thread.agent_name).label);
                let subtitle = format_thread_subtitle(&self.agents, thread);
                ThreadListItem {
                    title,
                    subtitle,
                    is_active: self
                        .active_thread_id
                        .as_deref()
                        .map(|active| active == thread.thread_id)
                        .unwrap_or(false),
                    has_context: thread.context_email_id.is_some(),
                }
            })
            .collect()
    }

    pub fn transcript_entries(&self) -> Vec<TranscriptEntry> {
        let mut entries = Vec::new();

        if let Some(thread) = self.active_thread() {
            let messages = self
                .messages_by_thread
                .get(&thread.thread_id)
                .cloned()
                .unwrap_or_default();
            for message in messages {
                entries.push(TranscriptEntry {
                    label: format_message_label(
                        &self.agents,
                        &message.role,
                        message.created_at,
                        message.agent_name.as_deref(),
                    ),
                    body: message.content,
                    kind: if message.role == "user" {
                        TranscriptEntryKind::User
                    } else {
                        TranscriptEntryKind::Agent
                    },
                });
            }
        }

        if let Some(content) = &self.pending_user_message {
            entries.push(TranscriptEntry {
                label: "user • pending".to_string(),
                body: content.clone(),
                kind: TranscriptEntryKind::User,
            });
        }

        for event in &self.stream_log {
            entries.push(TranscriptEntry {
                label: "system".to_string(),
                body: event.clone(),
                kind: TranscriptEntryKind::Status,
            });
        }

        if !self.streaming_response.is_empty() {
            entries.push(TranscriptEntry {
                label: format!("{} • in progress", self.current_agent().label),
                body: self.streaming_response.clone(),
                kind: TranscriptEntryKind::Agent,
            });
        }

        if entries.is_empty() {
            entries.push(TranscriptEntry {
                label: "system".to_string(),
                body: if self.draft_mode {
                    "New Mail Assistant draft ready. Type a message below and press Enter to start."
                        .to_string()
                } else if self.threads_loading {
                    "Loading threads from the local API...".to_string()
                } else {
                    "No conversation selected. Press n to start a new thread or choose one from the list.".to_string()
                },
                kind: TranscriptEntryKind::Status,
            });
        }

        entries
    }

    pub fn header_title(&self) -> String {
        if let Some(thread) = self.active_thread() {
            return thread
                .title
                .clone()
                .unwrap_or_else(|| self.current_agent().label.to_string());
        }

        if self.draft_mode {
            return format!("New thread: {}", self.current_agent().label);
        }

        "Conversation".to_string()
    }

    pub fn header_subtitle(&self) -> String {
        if let Some(thread) = self.active_thread() {
            let mut subtitle = format!(
                "{} • {} messages",
                self.current_agent().label,
                thread.message_count
            );
            if let Some(email_id) = &thread.context_email_id {
                subtitle.push_str(&format!(" • context {}", shorten_id(email_id)));
            }
            return subtitle;
        }

        if self.draft_mode {
            if let Some(email_id) = &self.draft_context_email_id {
                return format!("Draft with context {}", shorten_id(email_id));
            }
            return "Draft thread".to_string();
        }

        "Select a thread or start a new one".to_string()
    }

    pub fn emails_filter_summary(&self) -> String {
        format!(
            "Search: {} | Category: {} | Spam: {} | Folder: {} | {} emails",
            display_or_all(&self.email_search),
            CATEGORY_OPTIONS[self.email_category_idx],
            SPAM_OPTIONS[self.email_spam_idx],
            self.email_folder_filter
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("All"),
            self.emails_total_count
        )
    }

    pub fn folders_flattened(&self) -> Vec<FolderListItem> {
        let mut items = Vec::new();
        for folder in &self.folders {
            flatten_folder(folder, 0, &self.expanded_folders, &mut items);
        }
        items
    }

    pub fn folder_filter_label(&self) -> &str {
        self.email_folder_filter.as_deref().unwrap_or("All folders")
    }

    pub fn help_lines(&self) -> Vec<&'static str> {
        match self.active_tab {
            AppTab::Chat => vec![
                "1-4: switch tabs",
                "Tab: switch focus between threads, transcript, and composer",
                "Enter: open thread or send message depending on focus",
                "n: start a new Mail Assistant thread",
                "d: delete the selected thread",
                "j/k or arrows: navigate",
                "r: refresh current tab",
                "F1 or ?: toggle help",
                "q or Ctrl-C: quit",
            ],
            AppTab::Emails => vec![
                "1-4: switch tabs",
                "j/k or arrows: move through emails",
                "/: edit the email search filter",
                "[: previous category | ]: next category",
                "s: cycle spam filter",
                "x: clear folder filter",
                "Enter: toggle the detail pane expanded view",
                "c: open a chat draft about the selected email",
                "gg / G: jump to top or bottom",
                "r: refresh current tab",
            ],
            AppTab::Folders => vec![
                "1-4: switch tabs",
                "j/k or arrows: move through folders",
                "h/l or left/right: collapse or expand a folder",
                "Enter: filter the Emails tab by the selected folder",
                "gg / G: jump to top or bottom",
                "r: refresh current tab",
            ],
            AppTab::Status => vec![
                "1-4: switch tabs",
                "Status refreshes every 5 seconds while active",
                "j/k or arrows: move through recent runs",
                "r: refresh current tab immediately",
                "F1 or ?: toggle help",
                "q or Ctrl-C: quit",
            ],
        }
    }

    pub fn footer_hint(&self) -> String {
        match self.active_tab {
            AppTab::Chat => {
                if self.send_pending {
                    "Waiting for Mail Assistant...".to_string()
                } else {
                    "Type a message and press Enter".to_string()
                }
            }
            AppTab::Emails => {
                "Use / for search, [ ] for category, s for spam, c to chat about the selected email.".to_string()
            }
            AppTab::Folders => {
                "Enter filters the Emails tab by the selected folder; h/l collapse and expand.".to_string()
            }
            AppTab::Status => "Recent runs auto-refresh while this tab is active.".to_string(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return vec![AppCommand::Quit];
        }

        if let Some(overlay) = self.overlay.clone() {
            return self.handle_overlay_key(overlay, key);
        }

        if matches!(key.code, KeyCode::F(1)) {
            self.toggle_help();
            return Vec::new();
        }

        if self.is_text_input_mode() {
            return match self.active_tab {
                AppTab::Chat => self.handle_chat_key(key),
                AppTab::Emails => self.handle_emails_key(key),
                AppTab::Folders | AppTab::Status => Vec::new(),
            };
        }

        if matches!(key.code, KeyCode::Char('?')) {
            self.toggle_help();
            return Vec::new();
        }

        if let KeyCode::Char(character) = key.code {
            if let Some(tab) = AppTab::from_digit(character) {
                return self.switch_tab(tab);
            }
        }

        if matches!(key.code, KeyCode::Char('q')) {
            return vec![AppCommand::Quit];
        }

        if matches!(key.code, KeyCode::Char('r')) {
            self.awaiting_second_g = false;
            return self.refresh_commands();
        }

        match self.active_tab {
            AppTab::Chat => self.handle_chat_key(key),
            AppTab::Emails => self.handle_emails_key(key),
            AppTab::Folders => self.handle_folders_key(key),
            AppTab::Status => self.handle_status_key(key),
        }
    }

    pub fn on_tick(&mut self) -> Vec<AppCommand> {
        if self.active_tab == AppTab::Status
            && !self.status_loading
            && self
                .last_status_refresh
                .map(|instant| instant.elapsed() >= STATUS_REFRESH_INTERVAL)
                .unwrap_or(true)
        {
            self.status_loading = true;
            self.last_status_refresh = Some(Instant::now());
            return vec![AppCommand::Network(NetworkCommand::LoadStatus)];
        }
        Vec::new()
    }

    pub fn handle_network_result(&mut self, result: NetworkResult) -> Vec<AppCommand> {
        match result {
            NetworkResult::AgentsLoaded(result) => self.on_agents_loaded(result),
            NetworkResult::ThreadsLoaded(result) => self.on_threads_loaded(result),
            NetworkResult::MessagesLoaded { thread_id, result } => {
                self.on_messages_loaded(&thread_id, result)
            }
            NetworkResult::ThreadDeleted { thread_id, result } => {
                self.on_thread_deleted(&thread_id, result)
            }
            NetworkResult::ChatEvent(event) => self.on_chat_event(event),
            NetworkResult::EmailsLoaded(result) => self.on_emails_loaded(result),
            NetworkResult::EmailDetailLoaded { message_id, result } => {
                self.on_email_detail_loaded(&message_id, *result)
            }
            NetworkResult::FoldersLoaded(result) => self.on_folders_loaded(result),
            NetworkResult::StatusLoaded(result) => self.on_status_loaded(result),
        }
    }

    fn handle_overlay_key(&mut self, overlay: OverlayState, key: KeyEvent) -> Vec<AppCommand> {
        match overlay {
            OverlayState::Help => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {
                    self.overlay = None;
                    Vec::new()
                }
                _ => Vec::new(),
            },
            OverlayState::AgentPicker { selected } => {
                let mut selected = selected;
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                        self.overlay = Some(OverlayState::AgentPicker { selected });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(self.agents.len().saturating_sub(1));
                        self.overlay = Some(OverlayState::AgentPicker { selected });
                    }
                    KeyCode::Enter => {
                        self.start_draft_thread(selected);
                        self.overlay = None;
                        self.status_message =
                            format!("Draft thread ready for {}.", self.current_agent().label);
                    }
                    KeyCode::Esc => {
                        self.overlay = None;
                    }
                    _ => {}
                }
                Vec::new()
            }
            OverlayState::DeleteConfirm { thread_id } => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.overlay = None;
                    self.status_message = "Deleting thread...".to_string();
                    vec![AppCommand::Network(NetworkCommand::DeleteThread {
                        thread_id,
                    })]
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.overlay = None;
                    self.status_message = "Delete cancelled.".to_string();
                    Vec::new()
                }
                _ => Vec::new(),
            },
        }
    }

    fn switch_tab(&mut self, tab: AppTab) -> Vec<AppCommand> {
        self.awaiting_second_g = false;
        self.active_tab = tab;
        self.status_message = format!("Switched to {}.", tab.title());
        match tab {
            AppTab::Chat => Vec::new(),
            AppTab::Emails => {
                if self.emails.is_empty() && !self.emails_loading {
                    self.emails_loading = true;
                    vec![AppCommand::Network(NetworkCommand::LoadEmails {
                        query: self.email_query(),
                    })]
                } else {
                    Vec::new()
                }
            }
            AppTab::Folders => {
                if self.folders.is_empty() && !self.folders_loading {
                    self.folders_loading = true;
                    vec![AppCommand::Network(NetworkCommand::LoadFolders)]
                } else {
                    Vec::new()
                }
            }
            AppTab::Status => {
                self.status_loading = true;
                self.last_status_refresh = Some(Instant::now());
                vec![AppCommand::Network(NetworkCommand::LoadStatus)]
            }
        }
    }

    fn toggle_help(&mut self) {
        if matches!(self.overlay, Some(OverlayState::Help)) {
            self.overlay = None;
        } else {
            self.overlay = Some(OverlayState::Help);
        }
    }

    fn is_text_input_mode(&self) -> bool {
        matches!(self.active_tab, AppTab::Chat) && matches!(self.chat_focus, ChatFocus::Composer)
            || matches!(self.active_tab, AppTab::Emails) && self.email_search_mode
    }

    fn handle_chat_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        if matches!(key.code, KeyCode::Tab) {
            self.chat_focus = match self.chat_focus {
                ChatFocus::Threads => ChatFocus::Transcript,
                ChatFocus::Transcript => ChatFocus::Composer,
                ChatFocus::Composer => ChatFocus::Threads,
            };
            self.awaiting_second_g = false;
            return Vec::new();
        }

        if !matches!(self.chat_focus, ChatFocus::Composer)
            && matches!(key.code, KeyCode::Char('n'))
            && !self.send_pending
        {
            if self.agents.len() > 1 {
                self.overlay = Some(OverlayState::AgentPicker {
                    selected: self.draft_agent_idx,
                });
                self.status_message = "Choose an assistant for the new draft thread.".to_string();
            } else {
                self.start_draft_thread(default_chat_agent_index(&self.agents));
                self.status_message = "New Mail Assistant draft ready.".to_string();
            }
            self.awaiting_second_g = false;
            return Vec::new();
        }

        if !matches!(self.chat_focus, ChatFocus::Composer)
            && matches!(key.code, KeyCode::Char('d'))
            && !self.send_pending
        {
            if let Some(thread) = self.selected_thread().cloned() {
                self.overlay = Some(OverlayState::DeleteConfirm {
                    thread_id: thread.thread_id.clone(),
                });
                self.status_message = format!(
                    "Confirm deletion of '{}'.",
                    thread_title(&self.agents, &thread)
                );
            }
            self.awaiting_second_g = false;
            return Vec::new();
        }

        match self.chat_focus {
            ChatFocus::Threads => self.handle_chat_threads_key(key),
            ChatFocus::Transcript => self.handle_chat_transcript_key(key),
            ChatFocus::Composer => self.handle_chat_composer_key(key),
        }
    }

    fn handle_chat_threads_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        self.awaiting_second_g = false;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_thread_idx = self.selected_thread_idx.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.threads.is_empty() {
                    self.selected_thread_idx =
                        (self.selected_thread_idx + 1).min(self.threads.len() - 1);
                }
                Vec::new()
            }
            KeyCode::Enter => {
                if let Some(thread) = self.selected_thread().cloned() {
                    self.draft_mode = false;
                    self.active_thread_id = Some(thread.thread_id.clone());
                    self.messages_loading = true;
                    self.transcript_scroll = 0;
                    self.pending_user_message = None;
                    self.streaming_response.clear();
                    self.stream_log.clear();
                    self.status_message = format!(
                        "Loading transcript for '{}'.",
                        thread_title(&self.agents, &thread)
                    );
                    vec![AppCommand::Network(NetworkCommand::LoadMessages {
                        thread_id: thread.thread_id,
                    })]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn handle_chat_transcript_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        self.awaiting_second_g = false;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.transcript_scroll = self.transcript_scroll.saturating_add(1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.transcript_scroll = self.transcript_scroll.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Enter => {
                self.chat_focus = ChatFocus::Composer;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_chat_composer_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        self.awaiting_second_g = false;
        match key.code {
            KeyCode::Enter if !self.send_pending => self.send_chat(),
            KeyCode::Backspace => {
                self.composer.pop();
                Vec::new()
            }
            KeyCode::Esc => {
                self.chat_focus = ChatFocus::Threads;
                Vec::new()
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.composer.push(character);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_emails_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        if self.email_search_mode {
            return self.handle_email_search_key(key);
        }

        match key.code {
            KeyCode::Char('/') => {
                self.email_search_mode = true;
                self.email_search_input = self.email_search.clone();
                self.status_message = "Editing email search. Press Enter to apply.".to_string();
                self.awaiting_second_g = false;
                Vec::new()
            }
            KeyCode::Char('[') => {
                self.awaiting_second_g = false;
                self.email_category_idx = self.email_category_idx.saturating_sub(1);
                self.reload_emails()
            }
            KeyCode::Char(']') => {
                self.awaiting_second_g = false;
                self.email_category_idx =
                    (self.email_category_idx + 1).min(CATEGORY_OPTIONS.len() - 1);
                self.reload_emails()
            }
            KeyCode::Char('s') => {
                self.awaiting_second_g = false;
                self.email_spam_idx = (self.email_spam_idx + 1) % SPAM_OPTIONS.len();
                self.reload_emails()
            }
            KeyCode::Char('x') => {
                self.awaiting_second_g = false;
                self.email_folder_filter = None;
                self.reload_emails()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.awaiting_second_g = false;
                self.move_email_selection(-1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.awaiting_second_g = false;
                self.move_email_selection(1)
            }
            KeyCode::Char('g') => {
                if self.awaiting_second_g {
                    self.awaiting_second_g = false;
                    self.select_email_at(0)
                } else {
                    self.awaiting_second_g = true;
                    Vec::new()
                }
            }
            KeyCode::Char('G') => {
                self.awaiting_second_g = false;
                if self.emails.is_empty() {
                    Vec::new()
                } else {
                    self.select_email_at(self.emails.len() - 1)
                }
            }
            KeyCode::Enter => {
                self.awaiting_second_g = false;
                self.email_detail_expanded = !self.email_detail_expanded;
                Vec::new()
            }
            KeyCode::Char('c') => {
                self.awaiting_second_g = false;
                self.open_chat_about_selected_email()
            }
            _ => {
                self.awaiting_second_g = false;
                Vec::new()
            }
        }
    }

    fn handle_email_search_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        match key.code {
            KeyCode::Enter => {
                self.email_search_mode = false;
                self.email_search = self.email_search_input.trim().to_string();
                self.reload_emails()
            }
            KeyCode::Esc => {
                self.email_search_mode = false;
                self.status_message = "Email search edit cancelled.".to_string();
                Vec::new()
            }
            KeyCode::Backspace => {
                self.email_search_input.pop();
                Vec::new()
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.email_search_input.push(character);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_folders_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        let folders = self.folders_flattened();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.awaiting_second_g = false;
                self.selected_folder_idx = self.selected_folder_idx.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.awaiting_second_g = false;
                if !folders.is_empty() {
                    self.selected_folder_idx =
                        (self.selected_folder_idx + 1).min(folders.len() - 1);
                }
                Vec::new()
            }
            KeyCode::Char('g') => {
                if self.awaiting_second_g {
                    self.awaiting_second_g = false;
                    self.selected_folder_idx = 0;
                } else {
                    self.awaiting_second_g = true;
                }
                Vec::new()
            }
            KeyCode::Char('G') => {
                self.awaiting_second_g = false;
                if !folders.is_empty() {
                    self.selected_folder_idx = folders.len() - 1;
                }
                Vec::new()
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.awaiting_second_g = false;
                if let Some(folder) = folders.get(self.selected_folder_idx) {
                    if folder.has_children {
                        self.expanded_folders.insert(folder.name.clone());
                    }
                }
                Vec::new()
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.awaiting_second_g = false;
                if let Some(folder) = folders.get(self.selected_folder_idx) {
                    self.expanded_folders.remove(&folder.name);
                }
                Vec::new()
            }
            KeyCode::Enter => {
                self.awaiting_second_g = false;
                if let Some(folder) = folders.get(self.selected_folder_idx) {
                    self.email_folder_filter = Some(folder.name.clone());
                    return self.switch_tab(AppTab::Emails);
                }
                Vec::new()
            }
            _ => {
                self.awaiting_second_g = false;
                Vec::new()
            }
        }
    }

    fn handle_status_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        self.awaiting_second_g = false;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => Vec::new(),
            KeyCode::Down | KeyCode::Char('j') => Vec::new(),
            _ => Vec::new(),
        }
    }

    fn send_chat(&mut self) -> Vec<AppCommand> {
        let message = self.composer.trim().to_string();
        if message.is_empty() {
            self.status_message = "Type a message before sending.".to_string();
            return Vec::new();
        }

        let request = ChatRequest {
            thread_id: self.active_thread_id.clone(),
            agent_name: self.current_agent().id.to_string(),
            message: message.clone(),
            context_email_id: self
                .active_thread()
                .and_then(|thread| thread.context_email_id.clone())
                .or_else(|| self.draft_context_email_id.clone()),
        };

        self.send_pending = true;
        self.pending_user_message = Some(message);
        self.pending_stream_thread_id = request.thread_id.clone();
        self.streaming_response.clear();
        self.stream_log.clear();
        self.status_message = "Sending message to Mail Assistant...".to_string();
        self.connection_ok = true;
        self.banner_message = None;

        vec![AppCommand::Network(NetworkCommand::SendChat { request })]
    }

    fn open_chat_about_selected_email(&mut self) -> Vec<AppCommand> {
        let Some(email) = self.selected_email().cloned() else {
            self.status_message = "Select an email first.".to_string();
            return Vec::new();
        };

        self.active_tab = AppTab::Chat;
        self.draft_mode = true;
        self.active_thread_id = None;
        self.draft_agent_idx = default_chat_agent_index(&self.agents);
        self.draft_context_email_id = Some(email.message_id.clone());
        self.chat_focus = ChatFocus::Composer;
        self.composer.clear();
        self.pending_user_message = None;
        self.streaming_response.clear();
        self.stream_log.clear();
        self.pending_stream_thread_id = None;
        self.status_message = format!(
            "Chat draft ready for '{}'.",
            email.subject.as_deref().unwrap_or("selected email")
        );
        Vec::new()
    }

    fn refresh_commands(&mut self) -> Vec<AppCommand> {
        self.connection_ok = true;
        self.banner_message = None;
        self.awaiting_second_g = false;
        match self.active_tab {
            AppTab::Chat => {
                self.status_message = "Refreshing chat state...".to_string();
                let mut commands = vec![
                    AppCommand::Network(NetworkCommand::LoadAgents),
                    AppCommand::Network(NetworkCommand::LoadThreads),
                ];
                if let Some(thread_id) = self.active_thread_id.clone() {
                    self.messages_loading = true;
                    commands.push(AppCommand::Network(NetworkCommand::LoadMessages {
                        thread_id,
                    }));
                }
                commands
            }
            AppTab::Emails => self.reload_emails(),
            AppTab::Folders => {
                self.folders_loading = true;
                self.status_message = "Refreshing folders...".to_string();
                vec![AppCommand::Network(NetworkCommand::LoadFolders)]
            }
            AppTab::Status => {
                self.status_loading = true;
                self.last_status_refresh = Some(Instant::now());
                self.status_message = "Refreshing status...".to_string();
                vec![AppCommand::Network(NetworkCommand::LoadStatus)]
            }
        }
    }

    fn reload_emails(&mut self) -> Vec<AppCommand> {
        self.emails_loading = true;
        self.email_detail_loading = false;
        self.email_detail = None;
        self.selected_email_id = None;
        self.status_message = "Refreshing emails...".to_string();
        vec![AppCommand::Network(NetworkCommand::LoadEmails {
            query: self.email_query(),
        })]
    }

    fn email_query(&self) -> EmailListQuery {
        EmailListQuery {
            search: (!self.email_search.trim().is_empty()).then(|| self.email_search.clone()),
            folder: self.email_folder_filter.clone(),
            category: (self.email_category_idx > 0)
                .then(|| CATEGORY_OPTIONS[self.email_category_idx].to_string()),
            spam_status: (self.email_spam_idx > 0)
                .then(|| SPAM_OPTIONS[self.email_spam_idx].to_string()),
            limit: DEFAULT_EMAIL_LIMIT,
            offset: 0,
        }
    }

    fn move_email_selection(&mut self, direction: isize) -> Vec<AppCommand> {
        if self.emails.is_empty() {
            return Vec::new();
        }
        let current = self.selected_email_idx as isize;
        let max = self.emails.len().saturating_sub(1) as isize;
        let next = (current + direction).clamp(0, max) as usize;
        self.select_email_at(next)
    }

    fn select_email_at(&mut self, index: usize) -> Vec<AppCommand> {
        if self.emails.is_empty() {
            return Vec::new();
        }

        self.selected_email_idx = index.min(self.emails.len() - 1);
        let message_id = self.emails[self.selected_email_idx].message_id.clone();
        if self.selected_email_id.as_deref() == Some(message_id.as_str())
            && self.email_detail.is_some()
        {
            return Vec::new();
        }

        self.selected_email_id = Some(message_id.clone());
        self.email_detail_loading = true;
        self.status_message = "Loading email detail...".to_string();
        vec![AppCommand::Network(NetworkCommand::LoadEmailDetail {
            message_id,
        })]
    }

    fn on_agents_loaded(&mut self, result: Result<Vec<AgentOption>, String>) -> Vec<AppCommand> {
        match result {
            Ok(agents) if !agents.is_empty() => {
                let agents = user_facing_agents(agents);
                let previous_agent_id = self
                    .agents
                    .get(self.draft_agent_idx)
                    .map(|agent| agent.id.clone())
                    .unwrap_or_else(|| agent_catalog::DEFAULT_AGENT_ID.to_string());
                self.connection_ok = true;
                self.banner_message = None;
                self.agents = agents;
                self.draft_agent_idx = index_for_agent_id(&self.agents, &previous_agent_id)
                    .unwrap_or_else(|| default_agent_index(&self.agents));
                self.status_message = "Loaded agent catalog.".to_string();
            }
            Ok(_) => {
                self.status_message =
                    "API returned no agents; keeping the built-in catalog.".to_string();
            }
            Err(error) => {
                self.connection_ok = false;
                self.banner_message = Some(error.clone());
                self.status_message =
                    "Unable to load agent catalog. Using built-in defaults.".to_string();
            }
        }
        Vec::new()
    }

    fn on_threads_loaded(&mut self, result: Result<Vec<ThreadSummary>, String>) -> Vec<AppCommand> {
        self.threads_loading = false;
        match result {
            Ok(threads) => {
                self.connection_ok = true;
                self.banner_message = None;
                let previous_active = self.active_thread_id.clone();
                let previous_selected_id = self
                    .selected_thread()
                    .map(|thread| thread.thread_id.clone());
                self.threads = threads;

                if let Some(selected_id) = previous_selected_id {
                    if let Some(index) = self
                        .threads
                        .iter()
                        .position(|thread| thread.thread_id == selected_id)
                    {
                        self.selected_thread_idx = index;
                    } else {
                        self.selected_thread_idx = 0;
                    }
                } else {
                    self.selected_thread_idx = 0;
                }

                if let Some(active_id) = previous_active {
                    if self
                        .threads
                        .iter()
                        .any(|thread| thread.thread_id == active_id)
                    {
                        self.active_thread_id = Some(active_id);
                    } else if !self.draft_mode {
                        self.active_thread_id = None;
                    }
                }

                if !self.draft_mode && self.active_thread_id.is_none() {
                    if let Some(thread) = self.threads.first() {
                        self.active_thread_id = Some(thread.thread_id.clone());
                        self.selected_thread_idx = 0;
                        self.messages_loading = true;
                        self.status_message = format!("Loaded {} threads.", self.threads.len());
                        return vec![AppCommand::Network(NetworkCommand::LoadMessages {
                            thread_id: thread.thread_id.clone(),
                        })];
                    }
                }

                self.status_message = format!("Loaded {} threads.", self.threads.len());
                Vec::new()
            }
            Err(error) => {
                self.connection_ok = false;
                self.banner_message = Some(error.clone());
                self.status_message = "Unable to load threads. Press r to retry.".to_string();
                Vec::new()
            }
        }
    }

    fn on_messages_loaded(
        &mut self,
        thread_id: &str,
        result: Result<Vec<ConversationMessage>, String>,
    ) -> Vec<AppCommand> {
        self.messages_loading = false;
        match result {
            Ok(messages) => {
                self.connection_ok = true;
                self.banner_message = None;
                self.messages_by_thread
                    .insert(thread_id.to_string(), messages);
                self.transcript_scroll = 0;
                self.status_message = "Transcript refreshed.".to_string();
            }
            Err(error) => {
                self.connection_ok = false;
                self.banner_message = Some(error.clone());
                self.status_message = "Unable to load transcript. Press r to retry.".to_string();
            }
        }
        Vec::new()
    }

    fn on_thread_deleted(
        &mut self,
        thread_id: &str,
        result: Result<(), String>,
    ) -> Vec<AppCommand> {
        match result {
            Ok(()) => {
                self.connection_ok = true;
                self.banner_message = None;
                self.messages_by_thread.remove(thread_id);
                self.threads.retain(|thread| thread.thread_id != thread_id);
                if self.selected_thread_idx >= self.threads.len() && !self.threads.is_empty() {
                    self.selected_thread_idx = self.threads.len() - 1;
                }

                let mut commands = vec![AppCommand::Network(NetworkCommand::LoadThreads)];
                if self.active_thread_id.as_deref() == Some(thread_id) {
                    if let Some(thread) = self.threads.get(self.selected_thread_idx) {
                        self.active_thread_id = Some(thread.thread_id.clone());
                        self.messages_loading = true;
                        commands.push(AppCommand::Network(NetworkCommand::LoadMessages {
                            thread_id: thread.thread_id.clone(),
                        }));
                    } else {
                        self.active_thread_id = None;
                        self.draft_mode = true;
                    }
                }
                self.status_message = "Thread deleted.".to_string();
                commands
            }
            Err(error) => {
                self.connection_ok = false;
                self.banner_message = Some(error.clone());
                self.status_message = "Delete failed. Press d to retry.".to_string();
                Vec::new()
            }
        }
    }

    fn on_chat_event(&mut self, event: ChatStreamEvent) -> Vec<AppCommand> {
        match event {
            ChatStreamEvent::Ready => Vec::new(),
            ChatStreamEvent::ThreadReady { thread_id, .. } => {
                self.pending_stream_thread_id = Some(thread_id.clone());
                self.status_message =
                    "Thread created. Waiting for the agent run to start...".to_string();
                self.stream_log
                    .push(format!("Thread {} is ready.", shorten_id(&thread_id)));
                Vec::new()
            }
            ChatStreamEvent::RunStarted {
                run_id,
                agent_name,
                visible_agent_name,
                ..
            } => {
                let display_agent_id = visible_agent_name.as_deref().unwrap_or(&agent_name);
                self.status_message = format!(
                    "{} is running ({})...",
                    self.find_agent(display_agent_id).label,
                    shorten_id(&run_id)
                );
                self.stream_log.push(format!(
                    "{} started run {}.",
                    self.find_agent(display_agent_id).label,
                    shorten_id(&run_id)
                ));
                Vec::new()
            }
            ChatStreamEvent::StepStarted { step } => {
                self.status_message = format!("Processing step {}...", step);
                self.stream_log.push(format!("Processing step {}.", step));
                Vec::new()
            }
            ChatStreamEvent::ToolCall {
                step, tool_name, ..
            } => {
                self.status_message = format!("Step {} calling {}...", step, tool_name);
                self.stream_log
                    .push(format!("Tool call: {} (step {}).", tool_name, step));
                Vec::new()
            }
            ChatStreamEvent::ToolResult {
                tool_name,
                latency_ms,
                ..
            } => {
                self.status_message = format!("{} completed in {} ms.", tool_name, latency_ms);
                self.stream_log.push(format!(
                    "Tool result: {} completed in {} ms.",
                    tool_name, latency_ms
                ));
                Vec::new()
            }
            ChatStreamEvent::AssistantDelta { delta } => {
                self.streaming_response.push_str(&delta);
                self.status_message = format!("{} is responding...", self.current_agent().label);
                Vec::new()
            }
            ChatStreamEvent::AssistantCompleted { thread_id, .. } => {
                self.send_pending = false;
                self.connection_ok = true;
                self.banner_message = None;
                self.status_message = "Response received.".to_string();
                self.composer.clear();
                self.pending_user_message = None;
                self.streaming_response.clear();
                self.stream_log.clear();
                self.draft_mode = false;
                self.active_thread_id = Some(thread_id.clone());
                self.pending_stream_thread_id = Some(thread_id.clone());
                vec![
                    AppCommand::Network(NetworkCommand::LoadThreads),
                    AppCommand::Network(NetworkCommand::LoadMessages { thread_id }),
                ]
            }
            ChatStreamEvent::Error { message } => {
                self.send_pending = false;
                self.connection_ok = false;
                self.banner_message = Some(message.clone());
                self.status_message = "Chat stream failed. Press Enter to retry.".to_string();
                self.streaming_response.clear();
                self.stream_log.push(format!("Error: {}", message));

                let mut commands = Vec::new();
                if let Some(thread_id) = self.pending_stream_thread_id.clone() {
                    commands.push(AppCommand::Network(NetworkCommand::LoadThreads));
                    commands.push(AppCommand::Network(NetworkCommand::LoadMessages {
                        thread_id,
                    }));
                }
                commands
            }
            ChatStreamEvent::Done => {
                self.send_pending = false;
                Vec::new()
            }
        }
    }

    fn on_emails_loaded(&mut self, result: Result<EmailListResponse, String>) -> Vec<AppCommand> {
        self.emails_loading = false;
        match result {
            Ok(response) => {
                self.connection_ok = true;
                self.banner_message = None;
                let previous_id = self.selected_email_id.clone();
                self.emails_total_count = response.total_count;
                self.emails = response.emails;

                if self.emails.is_empty() {
                    self.selected_email_idx = 0;
                    self.selected_email_id = None;
                    self.email_detail = None;
                    self.status_message = "No emails matched the current filters.".to_string();
                    return Vec::new();
                }

                if let Some(previous_id) = previous_id {
                    if let Some(index) = self
                        .emails
                        .iter()
                        .position(|email| email.message_id == previous_id)
                    {
                        self.selected_email_idx = index;
                    } else {
                        self.selected_email_idx = 0;
                    }
                } else {
                    self.selected_email_idx = 0;
                }

                self.status_message = format!("Loaded {} emails.", self.emails_total_count);
                self.select_email_at(self.selected_email_idx)
            }
            Err(error) => {
                self.connection_ok = false;
                self.banner_message = Some(error.clone());
                self.status_message = "Unable to load emails. Press r to retry.".to_string();
                Vec::new()
            }
        }
    }

    fn on_email_detail_loaded(
        &mut self,
        message_id: &str,
        result: Result<EmailRecord, String>,
    ) -> Vec<AppCommand> {
        self.email_detail_loading = false;
        match result {
            Ok(email) => {
                self.connection_ok = true;
                self.banner_message = None;
                if self.selected_email_id.as_deref() == Some(message_id) {
                    self.email_detail = Some(email);
                }
                self.status_message = "Email detail loaded.".to_string();
            }
            Err(error) => {
                self.connection_ok = false;
                self.banner_message = Some(error.clone());
                self.status_message = "Unable to load email detail. Press r to retry.".to_string();
            }
        }
        Vec::new()
    }

    fn on_folders_loaded(&mut self, result: Result<Vec<FolderNode>, String>) -> Vec<AppCommand> {
        self.folders_loading = false;
        match result {
            Ok(folders) => {
                self.connection_ok = true;
                self.banner_message = None;
                self.folders = folders;
                self.expanded_folders.clear();
                for folder in &self.folders {
                    self.expanded_folders.insert(folder.name.clone());
                }
                self.selected_folder_idx = self
                    .selected_folder_idx
                    .min(self.folders_flattened().len().saturating_sub(1));
                self.status_message = "Folders loaded.".to_string();
            }
            Err(error) => {
                self.connection_ok = false;
                self.banner_message = Some(error.clone());
                self.status_message = "Unable to load folders. Press r to retry.".to_string();
            }
        }
        Vec::new()
    }

    fn on_status_loaded(&mut self, result: Result<StatusSnapshot, String>) -> Vec<AppCommand> {
        self.status_loading = false;
        self.last_status_refresh = Some(Instant::now());
        match result {
            Ok(snapshot) => {
                self.connection_ok = true;
                self.banner_message = None;
                self.status_stats = Some(snapshot.stats);
                self.status_runs = snapshot.runs;
                self.status_message = "Status refreshed.".to_string();
            }
            Err(error) => {
                self.connection_ok = false;
                self.banner_message = Some(error.clone());
                self.status_message = "Unable to load status. Press r to retry.".to_string();
            }
        }
        Vec::new()
    }

    fn start_draft_thread(&mut self, agent_idx: usize) {
        self.draft_agent_idx = agent_idx.min(self.agents.len().saturating_sub(1));
        self.draft_mode = true;
        self.active_thread_id = None;
        self.pending_user_message = None;
        self.streaming_response.clear();
        self.stream_log.clear();
        self.pending_stream_thread_id = None;
        self.chat_focus = ChatFocus::Composer;
        self.active_tab = AppTab::Chat;
        self.transcript_scroll = 0;
    }
}

fn flatten_folder(
    folder: &FolderNode,
    depth: usize,
    expanded: &HashSet<String>,
    items: &mut Vec<FolderListItem>,
) {
    let is_expanded = expanded.contains(&folder.name);
    items.push(FolderListItem {
        name: folder.name.clone(),
        depth,
        message_count: folder.message_count,
        has_children: !folder.children.is_empty(),
        expanded: is_expanded,
    });

    if is_expanded {
        for child in &folder.children {
            flatten_folder(child, depth + 1, expanded, items);
        }
    }
}

fn builtin_agents() -> Vec<AgentOption> {
    agent_catalog::user_facing_agents(false)
}

fn user_facing_agents(agents: Vec<AgentOption>) -> Vec<AgentOption> {
    let visible = agents
        .into_iter()
        .filter(|agent| !agent.advanced_only)
        .collect::<Vec<_>>();
    if visible.is_empty() {
        builtin_agents()
    } else {
        visible
    }
}

fn fallback_agent() -> AgentOption {
    AgentOption {
        id: "custom-agent".to_string(),
        label: "Custom Agent".to_string(),
        description: "Custom agent thread".to_string(),
        tier: agent_catalog::AgentTier::Worker,
        is_default: false,
        advanced_only: false,
        sort_order: i32::MAX,
    }
}

fn default_agent_index(agents: &[AgentOption]) -> usize {
    agents
        .iter()
        .position(|agent| agent.is_default)
        .unwrap_or(0)
}

fn default_chat_agent_index(agents: &[AgentOption]) -> usize {
    index_for_agent_id(agents, agent_catalog::DEFAULT_AGENT_ID)
        .unwrap_or_else(|| default_agent_index(agents))
}

fn default_chat_agent(agents: &[AgentOption]) -> AgentOption {
    agents
        .get(default_chat_agent_index(agents))
        .cloned()
        .or_else(|| agent_catalog::find_agent(agent_catalog::DEFAULT_AGENT_ID))
        .unwrap_or_else(fallback_agent)
}

fn index_for_agent_id(agents: &[AgentOption], agent_id: &str) -> Option<usize> {
    agents
        .iter()
        .position(|agent| agent.id.eq_ignore_ascii_case(agent_id))
}

pub fn find_agent(agents: &[AgentOption], agent_id: &str) -> AgentOption {
    agents
        .iter()
        .find(|agent| agent.id.eq_ignore_ascii_case(agent_id))
        .cloned()
        .unwrap_or_else(|| default_chat_agent(agents))
}

fn thread_title(agents: &[AgentOption], thread: &ThreadSummary) -> String {
    thread
        .title
        .clone()
        .unwrap_or_else(|| find_agent(agents, &thread.agent_name).label)
}

fn format_thread_subtitle(agents: &[AgentOption], thread: &ThreadSummary) -> String {
    let agent = find_agent(agents, &thread.agent_name);
    let when = thread
        .last_message_at
        .or(Some(thread.created_at))
        .map(format_timestamp)
        .unwrap_or_else(|| "unknown time".to_string());
    format!(
        "{} • {} • {}",
        agent.label,
        agent_visibility_label(&agent),
        when
    )
}

fn agent_visibility_label(agent: &AgentOption) -> &'static str {
    if agent.advanced_only {
        "advanced"
    } else if agent.is_default {
        "default"
    } else {
        "standard"
    }
}

fn format_message_label(
    agents: &[AgentOption],
    role: &str,
    created_at: DateTime<Utc>,
    agent_name: Option<&str>,
) -> String {
    let who = match role {
        "user" => "user".to_string(),
        "agent" => agent_name
            .map(|agent| find_agent(agents, agent).label)
            .unwrap_or_else(|| "agent".to_string()),
        other => other.to_string(),
    };
    format!("{} • {}", who, format_timestamp(created_at))
}

fn format_timestamp(value: DateTime<Utc>) -> String {
    value
        .with_timezone(&Local)
        .format("%b %d %H:%M")
        .to_string()
}

fn shorten_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn display_or_all(value: &str) -> &str {
    if value.trim().is_empty() {
        "All"
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn switching_to_emails_requests_initial_load() {
        let (mut app, _) = App::new("http://127.0.0.1:3100".to_string());
        let commands = app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.active_tab, AppTab::Emails);
        assert!(matches!(
            commands.as_slice(),
            [AppCommand::Network(NetworkCommand::LoadEmails { .. })]
        ));
    }

    #[test]
    fn chat_about_selected_email_prepares_draft_context() {
        let (mut app, _) = App::new("http://127.0.0.1:3100".to_string());
        app.active_tab = AppTab::Emails;
        let email = EmailRecord {
            message_id: "msg-1".to_string(),
            subject: Some("Receipt".to_string()),
            sender: Some("store@example.com".to_string()),
            received_date: Some(Utc::now()),
            spam_status: "not-spam".to_string(),
            human_summary: None,
            category: Some("shopping".to_string()),
            topic: None,
            location: Some("INBOX".to_string()),
            raw_email_content: None,
            body_text: None,
            action_status: None,
        };
        app.emails = vec![email.clone()];
        app.selected_email_id = Some(email.message_id.clone());

        let commands = app.handle_key(key(KeyCode::Char('c')));
        assert!(commands.is_empty());
        assert_eq!(app.active_tab, AppTab::Chat);
        assert!(app.draft_mode);
        assert_eq!(app.draft_context_email_id.as_deref(), Some("msg-1"));
    }

    #[test]
    fn new_thread_flow_enters_draft_mode() {
        let (mut app, _) = App::new("http://127.0.0.1:3100".to_string());
        let commands = app.handle_key(key(KeyCode::Char('n')));
        assert!(commands.is_empty());
        assert!(app.draft_mode);
        assert_eq!(app.chat_focus, ChatFocus::Composer);
        assert!(app.active_thread_id.is_none());
        assert!(app.overlay.is_none());
        assert_eq!(app.current_agent().id, agent_catalog::DEFAULT_AGENT_ID);
    }

    #[test]
    fn loaded_catalog_hides_advanced_agents_in_tui() {
        let (mut app, _) = App::new("http://127.0.0.1:3100".to_string());
        let result = app.on_agents_loaded(Ok(agent_catalog::builtin_agents()));

        assert!(result.is_empty());
        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.agents[0].id, agent_catalog::DEFAULT_AGENT_ID);
        assert!(!app.agents.iter().any(|agent| agent.advanced_only));
    }

    #[test]
    fn active_legacy_specialist_thread_chats_through_mail_assistant() {
        let (mut app, _) = App::new("http://127.0.0.1:3100".to_string());
        app.threads = vec![ThreadSummary {
            thread_id: "thread-1".to_string(),
            agent_name: "digest-agent".to_string(),
            title: None,
            context_email_id: None,
            created_at: Utc::now(),
            last_message_at: None,
            message_count: 0,
        }];
        app.active_thread_id = Some("thread-1".to_string());

        assert_eq!(app.current_agent().id, agent_catalog::DEFAULT_AGENT_ID);
    }

    #[test]
    fn chat_composer_allows_typing_n_without_opening_new_thread() {
        let (mut app, _) = App::new("http://127.0.0.1:3100".to_string());
        app.active_tab = AppTab::Chat;
        app.chat_focus = ChatFocus::Composer;

        let commands = app.handle_key(key(KeyCode::Char('n')));

        assert!(commands.is_empty());
        assert_eq!(app.composer, "n");
        assert!(app.overlay.is_none());
    }
}
