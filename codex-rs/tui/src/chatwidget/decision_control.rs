use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use codex_core::protocol::Op;
use codex_protocol::mcp::RequestId;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::app_event::AppEvent;
use crate::app_event::ExternalDecisionInput;
use crate::app_event::ExternalDecisionSource;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ExternalApprovalAction;

const DECISION_CONTROL_DIRNAME: &str = "decision-control";
const PENDING_FILE_SUFFIX: &str = ".pending.json";
const CHOOSE_FILE_SUFFIX: &str = ".choose.json";
const FILE_FORMAT_VERSION: u32 = 1;

const EXEC_CHOICES: [&str; 2] = ["approve", "deny"];
const EXEC_CHOICES_WITH_ALWAYS: [&str; 3] = ["approve", "approve_always", "deny"];
const PATCH_CHOICES: [&str; 3] = ["approve", "approve_for_session", "deny"];
const ELICITATION_CHOICES: [&str; 3] = ["accept", "decline", "cancel"];

#[derive(Debug, Clone)]
enum PendingDecisionKind {
    Exec {
        id: String,
        allow_always: bool,
    },
    Patch {
        id: String,
    },
    Elicitation {
        server_name: String,
        request_id: RequestId,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct PendingDecision {
    pub(crate) decision_id: String,
    pub(crate) token: String,
    pub(crate) summary: String,
    kind: PendingDecisionKind,
}

impl PendingDecision {
    pub(crate) fn new_exec(id: String, command: String, allow_always: bool) -> Self {
        Self {
            decision_id: format!("exec:{id}"),
            token: generate_control_token(),
            summary: format!("Approval requested: {command}"),
            kind: PendingDecisionKind::Exec { id, allow_always },
        }
    }

    pub(crate) fn new_patch(id: String, num_files: usize) -> Self {
        let target = if num_files == 1 {
            "1 file".to_string()
        } else {
            format!("{num_files} files")
        };
        Self {
            decision_id: format!("patch:{id}"),
            token: generate_control_token(),
            summary: format!("Codex wants to edit {target}"),
            kind: PendingDecisionKind::Patch { id },
        }
    }

    pub(crate) fn new_elicitation(server_name: String, request_id: RequestId) -> Self {
        Self {
            decision_id: format!("elicitation:{server_name}:{request_id}"),
            token: generate_control_token(),
            summary: format!("Approval requested by {server_name}"),
            kind: PendingDecisionKind::Elicitation {
                server_name,
                request_id,
            },
        }
    }

    pub(crate) fn choices(&self) -> &'static [&'static str] {
        match self.kind {
            PendingDecisionKind::Exec { allow_always, .. } => {
                if allow_always {
                    &EXEC_CHOICES_WITH_ALWAYS
                } else {
                    &EXEC_CHOICES
                }
            }
            PendingDecisionKind::Patch { .. } => &PATCH_CHOICES,
            PendingDecisionKind::Elicitation { .. } => &ELICITATION_CHOICES,
        }
    }

    fn matches_op(&self, op: &Op) -> bool {
        match (&self.kind, op) {
            (PendingDecisionKind::Exec { id, .. }, Op::ExecApproval { id: op_id, .. }) => {
                op_id == id
            }
            (PendingDecisionKind::Patch { id }, Op::PatchApproval { id: op_id, .. }) => op_id == id,
            (
                PendingDecisionKind::Elicitation {
                    server_name,
                    request_id,
                },
                Op::ResolveElicitation {
                    server_name: op_server_name,
                    request_id: op_request_id,
                    ..
                },
            ) => op_server_name == server_name && op_request_id == request_id,
            _ => false,
        }
    }

    fn action_for_choice(&self, choice: &str) -> Option<ExternalApprovalAction> {
        match &self.kind {
            PendingDecisionKind::Exec { allow_always, .. } => {
                let allowed = if *allow_always {
                    &EXEC_CHOICES_WITH_ALWAYS[..]
                } else {
                    &EXEC_CHOICES[..]
                };
                match resolve_choice_alias(choice, allowed)? {
                    "approve" => Some(ExternalApprovalAction::Approve),
                    "approve_always" => Some(ExternalApprovalAction::ApproveAlways),
                    "deny" => Some(ExternalApprovalAction::Deny),
                    _ => None,
                }
            }
            PendingDecisionKind::Patch { .. } => {
                match resolve_choice_alias(choice, &PATCH_CHOICES)? {
                    "approve" => Some(ExternalApprovalAction::Approve),
                    "approve_for_session" => Some(ExternalApprovalAction::ApproveForSession),
                    "deny" => Some(ExternalApprovalAction::Deny),
                    _ => None,
                }
            }
            PendingDecisionKind::Elicitation { .. } => {
                match resolve_choice_alias(choice, &ELICITATION_CHOICES)? {
                    "accept" => Some(ExternalApprovalAction::Accept),
                    "decline" => Some(ExternalApprovalAction::Decline),
                    "cancel" => Some(ExternalApprovalAction::Cancel),
                    _ => None,
                }
            }
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct DecisionQueue {
    pending: VecDeque<PendingDecision>,
}

impl DecisionQueue {
    pub(crate) fn head(&self) -> Option<&PendingDecision> {
        self.pending.front()
    }

    pub(crate) fn enqueue_exec(&mut self, id: String, command: String, allow_always: bool) {
        self.pending
            .push_back(PendingDecision::new_exec(id, command, allow_always));
    }

    pub(crate) fn enqueue_patch(&mut self, id: String, num_files: usize) {
        self.pending
            .push_back(PendingDecision::new_patch(id, num_files));
    }

    pub(crate) fn enqueue_elicitation(&mut self, server_name: String, request_id: RequestId) {
        self.pending
            .push_back(PendingDecision::new_elicitation(server_name, request_id));
    }

    pub(crate) fn resolve_from_op(&mut self, op: &Op) -> bool {
        if self
            .pending
            .front()
            .is_some_and(|decision| decision.matches_op(op))
        {
            self.pending.pop_front();
            true
        } else {
            false
        }
    }

    pub(crate) fn validate_external_input(
        &self,
        input: &ExternalDecisionInput,
    ) -> Option<ExternalApprovalAction> {
        let head = self.pending.front()?;
        if head.token != input.token {
            return None;
        }
        if let Some(decision_id) = input.decision_id.as_ref()
            && decision_id != &head.decision_id
        {
            return None;
        }
        head.action_for_choice(input.choice.as_str())
    }

    pub(crate) fn pop_head(&mut self) -> Option<PendingDecision> {
        self.pending.pop_front()
    }
}

fn resolve_choice_alias(choice: &str, canonical_choices: &[&'static str]) -> Option<&'static str> {
    let normalized = choice.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    if let Ok(index) = normalized.parse::<usize>() {
        let index = index.checked_sub(1)?;
        return canonical_choices.get(index).copied();
    }
    canonical_choices
        .iter()
        .find(|&&canonical| canonical == normalized.as_str())
        .copied()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingDecisionFile {
    pub(crate) version: u32,
    pub(crate) session_id: String,
    pub(crate) decision_id: String,
    pub(crate) token: String,
    pub(crate) summary: String,
    pub(crate) choices: Vec<String>,
}

impl PendingDecisionFile {
    fn from_pending(session_id: String, pending: &PendingDecision) -> Self {
        Self {
            version: FILE_FORMAT_VERSION,
            session_id,
            decision_id: pending.decision_id.clone(),
            token: pending.token.clone(),
            summary: pending.summary.clone(),
            choices: pending
                .choices()
                .iter()
                .map(|choice| (*choice).to_string())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DecisionChoiceFile {
    pub(crate) version: u32,
    pub(crate) session_id: String,
    pub(crate) decision_id: String,
    pub(crate) token: String,
    pub(crate) choice: String,
}

pub(crate) fn pending_decision_path(codex_home: &Path, session_id: &str) -> PathBuf {
    decision_control_dir(codex_home).join(format!("{session_id}{PENDING_FILE_SUFFIX}"))
}

fn choose_decision_path(codex_home: &Path, session_id: &str) -> PathBuf {
    decision_control_dir(codex_home).join(format!("{session_id}{CHOOSE_FILE_SUFFIX}"))
}

pub(crate) fn sync_pending_decision_file(
    codex_home: &Path,
    session_id: &str,
    pending: Option<&PendingDecision>,
) -> io::Result<()> {
    let pending_path = pending_decision_path(codex_home, session_id);
    let choose_path = choose_decision_path(codex_home, session_id);

    if let Some(pending) = pending {
        ensure_control_dir(&decision_control_dir(codex_home))?;
        let payload = PendingDecisionFile::from_pending(session_id.to_string(), pending);
        write_json_atomic(&pending_path, &payload)
    } else {
        remove_if_exists(&pending_path)?;
        remove_if_exists(&choose_path)
    }
}

pub(crate) fn clear_session_control_files(codex_home: &Path, session_id: &str) -> io::Result<()> {
    remove_if_exists(&pending_decision_path(codex_home, session_id))?;
    remove_if_exists(&choose_decision_path(codex_home, session_id))
}

pub(crate) fn spawn_cli_choice_poller(
    codex_home: PathBuf,
    session_id: String,
    app_event_tx: AppEventSender,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let choose_path = choose_decision_path(&codex_home, &session_id);
        loop {
            match take_choice_submission(&choose_path) {
                Ok(Some(submission)) => {
                    app_event_tx.send(AppEvent::ExternalDecisionInput(ExternalDecisionInput {
                        decision_id: Some(submission.decision_id),
                        token: submission.token,
                        choice: submission.choice,
                        source: ExternalDecisionSource::CommandLine,
                    }));
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        path = %choose_path.display(),
                        error = %err,
                        "failed to read command-line decision submission"
                    );
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

fn decision_control_dir(codex_home: &Path) -> PathBuf {
    codex_home.join(DECISION_CONTROL_DIRNAME)
}

fn ensure_control_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let serialized = serde_json::to_vec(value).map_err(io::Error::other)?;
    let temp_path = path.with_extension(format!("{}.tmp", Uuid::new_v4().as_simple()));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path)?;
    file.write_all(&serialized)?;
    file.sync_all()?;
    std::fs::rename(temp_path, path)?;
    Ok(())
}

fn take_choice_submission(path: &Path) -> io::Result<Option<DecisionChoiceFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)?;
    remove_if_exists(path)?;
    let parsed = serde_json::from_str(&content).map_err(io::Error::other)?;
    Ok(Some(parsed))
}

pub(crate) fn generate_control_token() -> String {
    Uuid::new_v4().simple().to_string()[0..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::DecisionQueue;
    use crate::app_event::ExternalDecisionInput;
    use crate::app_event::ExternalDecisionSource;
    use crate::bottom_pane::ExternalApprovalAction;
    use pretty_assertions::assert_eq;

    #[test]
    fn validate_external_input_rejects_mismatch_token() {
        let mut queue = DecisionQueue::default();
        queue.enqueue_exec("call-1".to_string(), "ls".to_string(), false);

        let action = queue.validate_external_input(&ExternalDecisionInput {
            decision_id: Some("exec:call-1".to_string()),
            token: "wrong".to_string(),
            choice: "approve".to_string(),
            source: ExternalDecisionSource::CommandLine,
        });
        assert_eq!(action, None);
    }

    #[test]
    fn validate_external_input_accepts_head_choice() {
        let mut queue = DecisionQueue::default();
        queue.enqueue_exec("call-1".to_string(), "ls".to_string(), false);
        let head = queue.head().expect("head decision");

        let action = queue.validate_external_input(&ExternalDecisionInput {
            decision_id: Some(head.decision_id.clone()),
            token: head.token.clone(),
            choice: "approve".to_string(),
            source: ExternalDecisionSource::CommandLine,
        });
        assert_eq!(action, Some(ExternalApprovalAction::Approve));
    }

    #[test]
    fn validate_external_input_accepts_execpolicy_choice_index() {
        let mut queue = DecisionQueue::default();
        queue.enqueue_exec("call-1".to_string(), "ls".to_string(), true);
        let head = queue.head().expect("head decision");

        let action = queue.validate_external_input(&ExternalDecisionInput {
            decision_id: Some(head.decision_id.clone()),
            token: head.token.clone(),
            choice: "2".to_string(),
            source: ExternalDecisionSource::CommandLine,
        });
        assert_eq!(action, Some(ExternalApprovalAction::ApproveAlways));
    }
}
