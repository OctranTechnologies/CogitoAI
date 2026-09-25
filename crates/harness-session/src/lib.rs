use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use harness_core::{Error, SessionId};
use serde::{Deserialize, Serialize};

pub mod bus;
pub mod events;

pub use bus::{EventBus, EventSubscriber, EventSubscription};
pub use events::{EventId, EventPayload, EventType, FileChange, HarnessEvent, Timestamp};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub workspace_root: PathBuf,
    pub created_at: Timestamp,
    pub last_updated_at: Timestamp,
    pub events: Vec<HarnessEvent>,
}

impl Session {
    pub fn new(id: SessionId, workspace_root: PathBuf) -> Self {
        let now = Timestamp::now();
        Self {
            id,
            workspace_root,
            created_at: now,
            last_updated_at: now,
            events: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    Active,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: SessionId,
    pub status: SessionStatus,
    pub messages: Vec<ConversationMessage>,
    pub tool_events: usize,
    pub files_changed: usize,
    pub checkpoints_created: usize,
    pub verification_results: usize,
    pub context_compactions: usize,
    pub last_event_id: Option<EventId>,
}

impl Session {
    pub fn state(&self) -> Result<SessionState, Error> {
        reconstruct_state(&self.id, &self.events)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub workspace_root: PathBuf,
    pub status: SessionStatus,
    pub created_at: Timestamp,
    pub last_updated_at: Timestamp,
    pub event_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionWarning {
    pub line: usize,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionLoadReport {
    pub session: Session,
    pub warnings: Vec<SessionWarning>,
}

pub trait SessionStore: Send + Sync {
    fn create(&self, workspace_root: &Path) -> Result<Session, Error>;
    fn append_event(&self, session_id: &SessionId, event: HarnessEvent) -> Result<(), Error>;
    fn load(&self, session_id: &SessionId) -> Result<Session, Error>;
    fn resume(&self, session_id: &SessionId) -> Result<Session, Error> {
        self.load(session_id)
    }
    fn recent(&self, limit: usize) -> Result<Vec<SessionSummary>, Error>;
}

pub struct JsonlSessionStore {
    root: PathBuf,
    event_bus: EventBus,
    append_lock: Mutex<()>,
}

impl std::fmt::Debug for JsonlSessionStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JsonlSessionStore")
            .field("root", &self.root)
            .finish()
    }
}

impl JsonlSessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, Error> {
        Self::with_event_bus(root, EventBus::new())
    }

    pub fn with_event_bus(root: impl Into<PathBuf>, event_bus: EventBus) -> Result<Self, Error> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            event_bus,
            append_lock: Mutex::new(()),
        })
    }

    pub fn event_bus(&self) -> EventBus {
        self.event_bus.clone()
    }

    pub fn load_with_report(&self, session_id: &SessionId) -> Result<SessionLoadReport, Error> {
        let path = self.session_path(session_id)?;
        if !path.is_file() {
            return Err(Error::NotFound {
                kind: "session",
                id: session_id.clone(),
            });
        }
        let (events, warnings) = read_events(&path)?;
        let session = reconstruct_session(session_id.clone(), events)?;
        Ok(SessionLoadReport { session, warnings })
    }

    fn session_path(&self, session_id: &SessionId) -> Result<PathBuf, Error> {
        let value = session_id.as_str();
        if value.is_empty()
            || value == "."
            || value == ".."
            || value
                .chars()
                .any(|character| matches!(character, '/' | '\\' | ':'))
        {
            return Err(Error::InvalidId {
                reason: "session identifier cannot contain path separators".to_owned(),
            });
        }
        Ok(self.root.join(format!("{value}.jsonl")))
    }
}

impl SessionStore for JsonlSessionStore {
    fn create(&self, workspace_root: &Path) -> Result<Session, Error> {
        let workspace_root = fs::canonicalize(workspace_root)?;
        let _guard = self
            .append_lock
            .lock()
            .expect("session append lock poisoned");
        let session_id = SessionId::new(EventId::new().to_string())?;
        let path = self.session_path(&session_id)?;
        let event = HarnessEvent::new(
            session_id.clone(),
            EventPayload::SessionStarted {
                workspace_root: workspace_root.clone(),
            },
            None,
            None,
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        if let Err(error) = write_event(&mut file, &event) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        self.event_bus.publish(&event);
        reconstruct_session(session_id, vec![event])
    }

    fn append_event(&self, session_id: &SessionId, event: HarnessEvent) -> Result<(), Error> {
        if event.session_id != *session_id {
            return Err(Error::InvalidEvent {
                reason: "event session ID does not match the session".to_owned(),
            });
        }
        let path = self.session_path(session_id)?;
        if !path.is_file() {
            return Err(Error::NotFound {
                kind: "session",
                id: session_id.clone(),
            });
        }
        let _guard = self
            .append_lock
            .lock()
            .expect("session append lock poisoned");
        let report = self.load_with_report(session_id)?;
        if !report.warnings.is_empty() {
            return Err(Error::Session {
                reason: "cannot append to a session with an incomplete trailing record".to_owned(),
            });
        }
        let mut event_ids = report
            .session
            .events
            .iter()
            .map(|event| event.event_id.clone())
            .collect::<HashSet<_>>();
        validate_event(&event, &mut event_ids)?;
        let mut file = OpenOptions::new().append(true).open(&path)?;
        write_event(&mut file, &event)?;
        self.event_bus.publish(&event);
        Ok(())
    }

    fn load(&self, session_id: &SessionId) -> Result<Session, Error> {
        self.load_with_report(session_id)
            .map(|report| report.session)
    }

    fn recent(&self, limit: usize) -> Result<Vec<SessionSummary>, Error> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            let Ok(session_id) = SessionId::new(stem) else {
                continue;
            };
            let session = self.load(&session_id)?;
            let state = session.state()?;
            sessions.push(SessionSummary {
                id: session.id,
                workspace_root: session.workspace_root,
                status: state.status,
                created_at: session.created_at,
                last_updated_at: session.last_updated_at,
                event_count: session.events.len(),
            });
        }
        sessions.sort_by(|left, right| {
            right
                .last_updated_at
                .cmp(&left.last_updated_at)
                .then_with(|| right.created_at.cmp(&left.created_at))
        });
        sessions.truncate(limit);
        Ok(sessions)
    }
}

fn write_event(file: &mut fs::File, event: &HarnessEvent) -> Result<(), Error> {
    let serialized = serde_json::to_string(event).map_err(|error| Error::Session {
        reason: format!("could not serialize event: {error}"),
    })?;
    file.write_all(serialized.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn read_events(path: &Path) -> Result<(Vec<HarnessEvent>, Vec<SessionWarning>), Error> {
    let contents = fs::read(path)?;
    let contents = String::from_utf8_lossy(&contents);
    let lines = contents
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let last_line = lines.last().map(|(line, _)| *line).unwrap_or(0);
    let mut events = Vec::new();
    let mut warnings = Vec::new();
    for (line_number, line) in lines {
        match serde_json::from_str::<HarnessEvent>(line) {
            Ok(event) => events.push(event),
            Err(error) if line_number == last_line => {
                warnings.push(SessionWarning {
                    line: line_number + 1,
                    reason: format!("ignored incomplete trailing record: {error}"),
                });
            }
            Err(error) => {
                return Err(Error::Session {
                    reason: format!(
                        "{}:{}: invalid JSONL record: {error}",
                        path.display(),
                        line_number + 1
                    ),
                });
            }
        }
    }
    Ok((events, warnings))
}

fn reconstruct_session(session_id: SessionId, events: Vec<HarnessEvent>) -> Result<Session, Error> {
    let first = events.first().ok_or_else(|| Error::Session {
        reason: "session history has no events".to_owned(),
    })?;
    let workspace_root = match &first.payload {
        EventPayload::SessionStarted { workspace_root } => workspace_root.clone(),
        _ => {
            return Err(Error::InvalidEvent {
                reason: "first session event must be session.started".to_owned(),
            })
        }
    };
    validate_events(&session_id, &events)?;
    let created_at = first.timestamp;
    let last_updated_at = events.last().map_or(created_at, |event| event.timestamp);
    Ok(Session {
        id: session_id,
        workspace_root,
        created_at,
        last_updated_at,
        events,
    })
}

fn validate_events(session_id: &SessionId, events: &[HarnessEvent]) -> Result<(), Error> {
    let mut event_ids = HashSet::new();
    for event in events {
        if event.session_id != *session_id {
            return Err(Error::InvalidEvent {
                reason: format!("event {} belongs to another session", event.event_id),
            });
        }
        validate_event(event, &mut event_ids)?;
    }
    Ok(())
}

fn validate_event(event: &HarnessEvent, event_ids: &mut HashSet<EventId>) -> Result<(), Error> {
    if event.schema_version != 1 {
        return Err(Error::InvalidEvent {
            reason: format!("unsupported schema version {}", event.schema_version),
        });
    }
    if event.event_type != event.payload.event_type() {
        return Err(Error::InvalidEvent {
            reason: format!("event type does not match payload for {}", event.event_id),
        });
    }
    if !event_ids.insert(event.event_id.clone()) {
        return Err(Error::InvalidEvent {
            reason: format!("duplicate event ID {}", event.event_id),
        });
    }
    Ok(())
}

pub fn reconstruct_state(
    session_id: &SessionId,
    events: &[HarnessEvent],
) -> Result<SessionState, Error> {
    validate_events(session_id, events)?;
    if !matches!(
        events.first().map(|event| &event.payload),
        Some(EventPayload::SessionStarted { .. })
    ) {
        return Err(Error::InvalidEvent {
            reason: "first session event must be session.started".to_owned(),
        });
    }
    let mut state = SessionState {
        session_id: session_id.clone(),
        status: SessionStatus::Active,
        messages: Vec::new(),
        tool_events: 0,
        files_changed: 0,
        checkpoints_created: 0,
        verification_results: 0,
        context_compactions: 0,
        last_event_id: None,
    };
    let mut terminal = false;
    for event in events {
        if terminal {
            return Err(Error::InvalidEvent {
                reason: "events cannot follow a terminal session event".to_owned(),
            });
        }
        match &event.payload {
            EventPayload::SessionStarted { .. } => {}
            EventPayload::UserMessage { text } => state.messages.push(ConversationMessage {
                role: MessageRole::User,
                text: text.clone(),
            }),
            EventPayload::AssistantMessage { text } => state.messages.push(ConversationMessage {
                role: MessageRole::Assistant,
                text: text.clone(),
            }),
            EventPayload::ToolRequested { .. }
            | EventPayload::ToolApproved { .. }
            | EventPayload::ToolDenied { .. }
            | EventPayload::ToolStarted { .. }
            | EventPayload::ToolOutput { .. }
            | EventPayload::ToolCompleted { .. }
            | EventPayload::ToolFailed { .. } => state.tool_events += 1,
            EventPayload::FileChanged { .. } => state.files_changed += 1,
            EventPayload::CheckpointCreated { .. } => state.checkpoints_created += 1,
            EventPayload::VerificationStarted { .. } => {}
            EventPayload::VerificationResult { .. } => state.verification_results += 1,
            EventPayload::ContextCompacted { .. } => state.context_compactions += 1,
            EventPayload::SessionCompleted { .. } => {
                state.status = SessionStatus::Completed;
                terminal = true;
            }
            EventPayload::SessionFailed { .. } => {
                state.status = SessionStatus::Failed;
                terminal = true;
            }
            EventPayload::ModelRequested { .. } | EventPayload::ModelResponse { .. } => {}
        }
        state.last_event_id = Some(event.event_id.clone());
    }
    Ok(state)
}
