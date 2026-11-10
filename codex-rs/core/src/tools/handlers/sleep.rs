use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_extension_items::ExtensionItem;
use codex_extension_items::sleep::SleepItem;
use codex_protocol::items::TurnItem;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;
use std::time::Instant;

const NAMESPACE: &str = "clock";
const TOOL_NAME: &str = "sleep";
const MAX_SLEEP_DURATION_MS: u64 = 12 * 60 * 60 * 1000;
/// How often a sleep checks whether the background terminals it may be waiting for are done.
const BACKGROUND_POLL: Duration = Duration::from_millis(500);

/// Why a sleep ended.
enum Woke {
    Completed,
    NewInput,
    /// ByteAsk: every background terminal running when the sleep started has exited.
    BackgroundExited(Vec<String>),
}

/// Resolves once none of `watched` (background terminal ids) is running any more; never when
/// `watched` is empty. A sleep is the model waiting for something, and a background job that
/// has finished is not worth waiting for.
async fn background_exited(
    session: &crate::session::session::Session,
    watched: Vec<String>,
) -> Vec<String> {
    if watched.is_empty() {
        return std::future::pending().await;
    }
    loop {
        tokio::time::sleep(BACKGROUND_POLL).await;
        let running = session.services.unified_exec_manager.list_processes().await;
        if !running
            .iter()
            .any(|terminal| watched.contains(&terminal.process_id))
        {
            return watched;
        }
    }
}

pub struct SleepHandler;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SleepArgs {
    duration_ms: u64,
}

fn create_sleep_tool() -> ToolSpec {
    let properties = BTreeMap::from([(
        "duration_ms".to_string(),
        JsonSchema::number(Some(format!(
            "How long to sleep in milliseconds. Must be between 1 and {MAX_SLEEP_DURATION_MS}."
        ))),
    )]);

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: NAMESPACE.to_string(),
        description: "Tools for reading and waiting on time.".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Pause execution for a specified duration. The sleep ends early when new input arrives for the active turn. Returns the elapsed wall-clock time."
                .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["duration_ms".to_string()]),
                /*additional_properties*/ Some(false.into()),
            ),
            output_schema: None,
        })],
    })
}

impl ToolExecutor<ToolInvocation> for SleepHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(NAMESPACE, TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_sleep_tool()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                call_id,
                payload,
                ..
            } = invocation;
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{TOOL_NAME} handler received unsupported payload"
                )));
            };
            let args: SleepArgs = parse_arguments(&arguments)?;
            if !(1..=MAX_SLEEP_DURATION_MS).contains(&args.duration_ms) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "duration_ms must be between 1 and {MAX_SLEEP_DURATION_MS}"
                )));
            }

            let started = Instant::now();
            let item = TurnItem::Extension(ExtensionItem::Sleep(SleepItem {
                id: call_id,
                duration_ms: args.duration_ms,
            }));
            session.emit_turn_item_started(turn.as_ref(), &item).await;
            let turn_state = session
                .input_queue
                .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
                .await;
            let (mut activity_rx, pending_activity) = session
                .input_queue
                .subscribe_activity(turn_state.as_deref())
                .await;
            let watched: Vec<String> = session
                .services
                .unified_exec_manager
                .list_processes()
                .await
                .into_iter()
                .map(|terminal| terminal.process_id)
                .collect();
            let sleep_result: Result<Woke, FunctionCallError> = if pending_activity.is_some() {
                Ok(Woke::NewInput)
            } else {
                let sleep = session
                    .services
                    .time_provider
                    .sleep(session.thread_id, Duration::from_millis(args.duration_ms));
                tokio::pin!(sleep);
                let exited = background_exited(session.as_ref(), watched);
                tokio::pin!(exited);
                tokio::select! {
                    result = &mut sleep => result
                        .map(|()| Woke::Completed)
                        .map_err(|err| {
                            FunctionCallError::Fatal(format!("failed to sleep: {err:#}"))
                        }),
                    result = activity_rx.changed() => {
                        if result.is_ok() {
                            Ok(Woke::NewInput)
                        } else {
                            sleep
                                .await
                                .map(|()| Woke::Completed)
                                .map_err(|err| {
                                    FunctionCallError::Fatal(format!("failed to sleep: {err:#}"))
                                })
                        }
                    }
                    ids = &mut exited => Ok(Woke::BackgroundExited(ids)),
                }
            };
            session.emit_turn_item_completed(turn.as_ref(), item).await;
            let woke = sleep_result?;

            let message = match woke {
                Woke::NewInput => "Sleep interrupted by new input.".to_string(),
                Woke::Completed => "Sleep completed.".to_string(),
                Woke::BackgroundExited(ids) => format!(
                    "Sleep ended early: the background terminal session(s) running when it \
                     started ({}) have exited. Read their final output with write_stdin.",
                    ids.join(", ")
                ),
            };
            let wall_time_seconds = started.elapsed().as_secs_f64();
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                format!("Wall time: {wall_time_seconds:.4} seconds\n{message}"),
                /*success*/ Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for SleepHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}
