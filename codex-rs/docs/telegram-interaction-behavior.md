# Telegram Interaction Behavior

## Scope
This document describes user-visible behavior of Telegram integration in Codex TUI.
It emphasizes runtime outcomes: what Codex does, what Telegram input is accepted or rejected, what changed from terminal-only behavior, and what stayed the same.

## Before Telegram Integration
Before Telegram support, Codex interaction was terminal-only:

1. User prompts were entered in terminal.
2. Approval decisions were made in terminal.
3. `request_user_input` flows were completed in terminal.
4. Turn completion and input-needed states were visible in terminal only.

There was no remote control path from Telegram.

## After Telegram Integration
When Telegram is enabled for a session:

1. Codex can send Telegram notifications for important states.
2. Codex can accept tokenized Telegram commands for selected interactive actions.
3. Telegram is additive and optional; terminal behavior is still the primary interface.
4. Telegram activation is session-scoped and not automatically persisted across resumed sessions.

## Session Setup and Bot Selection (`/tg`)

### Bot configuration source
Bot profiles are loaded from:

`~/.codex/telegram-bots.toml`

### What `/tg` does
`/tg` opens a selector that lets the user:

1. Enable one configured bot for the current session.
2. Disable Telegram for the current session.

### Lease/lock behavior
Each configured bot profile can be used by only one running session at a time:

1. If another session already holds a profile, it is shown as unavailable.
2. Switching bots updates the active target for subsequent send/poll activity.
3. Disabling Telegram or ending the process releases the profile lock.
4. If a process is killed, lock ownership is released with process termination.

### Config error behavior
If config is missing or malformed:

1. Telegram is not enabled.
2. Codex reports actionable local error messaging.
3. Local terminal usage continues normally.

## Outbound Telegram Messaging
When Telegram is active, Codex may send:

1. Assistant turn messages.
2. Approval-request messages.
3. Input-request helper messages.
4. Acknowledgments after Telegram-applied actions.

### `tg sent` behavior
`tg sent` is printed only after Telegram API success acknowledgment for that send path.

If send fails, returns non-success status, or has invalid response payload:

1. Codex does not crash.
2. Codex does not hang.
3. The failed send is skipped.
4. Runtime continues.

### Message ordering behavior
For copy/paste usability:

1. The main context message is sent first.
2. Helper command lines are sent shortly after.
3. For helper lines, a short delay (about 2 seconds) is used to reduce out-of-order display.

## Telegram Polling Reliability Behavior
Telegram updates are polled continuously while Telegram is enabled.

If polling fails due to transport error, non-success API status, or unsuccessful/undecodable API payload:

1. Polling does not stop permanently.
2. The loop waits about 2 seconds before retrying.
3. This avoids tight retry loops and reduces API/log spam.
4. Polling resumes normally after service recovers.

## Inbound Telegram Command Acceptance Model
Codex accepts Telegram commands only when all gate checks pass:

1. Message comes from the configured `chat_id`.
2. Sender is not a bot account.
3. Command format is strict and valid.
4. Token matches current active pending token.
5. Current UI state is compatible with that command type.

Anything else is ignored safely.

## Approval Decisions via Telegram (`/cx`)

### Command format
`/cx <TOKEN> <CHOICE>`

### When it is available
When an approval is pending, Codex sends:

1. A summary message with request context.
2. Separate copyable helper line(s), one per allowed raw choice:

`/cx <TOKEN> <raw_choice_name>`

### Accepted behavior
If token and choice match current pending decision:

1. Decision is applied exactly as if selected in terminal.
2. Codex sends Telegram acknowledgment that it took effect.
3. Decision state advances and old token usage is rejected.

### Rejected behavior
Input is ignored if:

1. Token is stale, malformed, or mismatched.
2. Choice is invalid for current decision.
3. No approval is pending anymore.
4. Chat/sender validation fails.
5. Command shape is invalid.

### Conflict behavior (terminal vs Telegram)
If both channels answer close together:

1. First valid input that reaches the still-pending state wins.
2. Later inputs become stale and are ignored.

## Text Input via Telegram (`/cxi`)

### Command format
`/cxi <TOKEN> <TEXT>`

### Helper message format
When Codex wants remote freeform text continuation, it sends:

`/cxi <TOKEN> to reply`

The user replaces `to reply` with desired content.

### Supported targets
`/cxi` can apply to:

1. Idle continuation after turn completion.
2. Interrupted conversation where Codex asks what to do differently.
3. `request_user_input` only when all are true: exactly one question, freeform text input, and non-secret input.

### Unsupported for Telegram text input
These remain terminal-only:

1. Option-selection `request_user_input`.
2. Multi-question `request_user_input`.
3. Secret/sensitive input prompts.

### Runtime semantics note
Accepted `/cxi` text follows the same normal input path as local prompt submission for that state.

## What Remains Unchanged
Telegram integration does not alter core terminal interaction semantics:

1. Terminal approval UX and choices stay unchanged.
2. Terminal `request_user_input` UX stays unchanged.
3. Terminal-only workflow remains fully supported without Telegram.

## Safety Posture
The integration is intentionally strict:

1. Arbitrary Telegram chatter does not trigger actions.
2. Invalid format is rejected.
3. Wrong token or wrong state is rejected.
4. Network/API failures degrade gracefully instead of stopping local usage.

## Edge and Corner Cases

### Deferred idle helper
If a turn completes while app state is not yet ready for idle continuation, helper prompt arming can be deferred until state becomes eligible.

### Bot switch mid-session
After switching bot via `/tg`, newly scheduled traffic uses the new bot profile.
Already-scheduled delayed follow-up sends from the previous bot context may still arrive on the previous bot.

### Telegram disable mid-session
After disabling Telegram:

1. Outbound Telegram sends stop.
2. Inbound Telegram polling stops.
3. Terminal flow continues.

### Stale command replay
Reusing old `/cx` or `/cxi` token commands after state changes is ignored.

## Practical Usage

1. Use `/tg` at session start to choose bot for this session.
2. Use exact helper command lines from Telegram messages for approval/input.
3. If a command appears ignored, verify token freshness, command format, active state, and `chat_id` source.

## Medium Implementation Details
This section summarizes architecture-level behavior without low-level code walkthrough.

### Components and responsibilities

1. `TelegramNotifier` owns bot config loading, activation/deactivation, lock lifecycle, and outbound send helpers.
2. A Telegram poller task receives updates and maps accepted commands into app events.
3. `ChatWidget` owns runtime decision queue and external prompt state, and applies accepted external actions to active UI state.
4. Bottom-pane overlays remain the source of truth for approval and `request_user_input` interaction eligibility.

### Control flow for outbound messages

1. UI state changes trigger `ChatWidget` notification methods.
2. `ChatWidget` requests notifier send operations.
3. On API success, callback prints `tg sent`.
4. For ordered helper messaging, follow-up command lines are sent in a delayed second step.

### Control flow for inbound commands

1. Poller retrieves Telegram updates.
2. Updates are filtered by chat/sender and command syntax.
3. Parsed commands become `AppEvent` external-input messages.
4. Event handling in `ChatWidget` revalidates token and active state before apply.
5. Applied actions emit local info and Telegram acknowledgment messages.

### Token and state lifecycle

1. Tokens are generated for active pending approvals and pending external text prompts.
2. Tokens are bound to live state and are not treated as reusable session credentials.
3. Once state advances, old tokens become stale and are rejected.

### Failure handling model

1. Outbound send failures are non-fatal and do not block terminal workflow.
2. Poll failures trigger a fixed retry delay (about 2 seconds) before next attempt.
3. Configuration/activation failures remain local errors and do not crash the app.

### Concurrency model

1. Telegram control and terminal control converge on the same internal state handlers.
2. First valid action on a pending state wins.
3. Late arrivals are rejected as stale after state transition.

## Known Limitations

1. `/cxi` text currently follows the same submission path as normal local text for that state. As a result, if local input semantics evolve, Telegram text semantics will evolve with them.
2. For delayed helper follow-up messages, a bot switch/disable does not retroactively cancel messages that were already scheduled from the previous bot context.
3. `request_user_input` support over Telegram is intentionally limited to single-question, non-secret, freeform-text flows.
4. Polling retry currently uses a fixed delay (about 2 seconds), not adaptive/exponential backoff.
5. `tg sent` reflects successful send completion in the active send path, but it is not a global guarantee that all logically related messages in a multi-message sequence were delivered.

## Rebase Risk Areas
These are common areas where a later rebase can accidentally regress Telegram behavior.

1. Input submission path changes in `ChatWidget`:
Rebases that change how user text is submitted can unintentionally change `/cxi` semantics.
2. Approval queue/state handling changes:
Rebases that alter approval overlay ordering or pending-decision management can desynchronize `/cx` token-to-decision mapping.
3. Event plumbing changes (`AppEvent`, event dispatch, or bottom-pane routing):
Rebases that rename or reroute external input events can silently break Telegram command application.
4. Notifier callback and send sequencing changes:
Rebases that alter send callback timing can make `tg sent` semantics inconsistent or reorder helper messages.
5. Poll loop and fetch error handling changes:
Rebases can remove the fixed retry delay on unsuccessful polling responses, reintroducing tight retry loops.
6. Bot activation/lease lifecycle changes:
Rebases that change activation/deactivation and lock ownership behavior can cause bot reuse conflicts across sessions.

## Rebase Verification Checklist

1. Verify `/tg` still shows available/in-use/active bot states and can switch/disable in-session.
2. Verify `/cx <TOKEN> <CHOICE>` only applies for current pending decision and rejects stale token input.
3. Verify `/cxi <TOKEN> <TEXT>` works only for supported target states and rejects unsupported request-user-input shapes.
4. Verify failed polling responses still retry after about 2 seconds rather than tight-looping.
5. Verify `tg sent` appears only on successful Telegram API acknowledgment paths.
6. Verify delayed helper message ordering still preserves context-first readability.
