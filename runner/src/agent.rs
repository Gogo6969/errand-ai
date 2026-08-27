//! Carrying out a task with any model that can call tools.
//!
//! The other executor shells out to the Claude command line tool and lets it
//! drive itself. This one keeps the loop here: Errand hands the model a tool
//! list, takes back what it wants run, runs it through `mcp::dispatch`, and
//! feeds the result in again until the model calls finish or fail.
//!
//! CONTAINMENT. This path is tighter than the CLI one, not looser. The model
//! is a text endpoint at the end of a socket: it cannot start a process, read a
//! file, or open a connection. The only things that happen are the ones this
//! loop performs, and it performs nothing except the tools in
//! `mcp::tool_definitions()`. A name that is not in that list is refused with a
//! sentence, never passed through, so a tool the model invents or reads off a
//! web page reaches nothing. Every call that does run goes through the same
//! `dispatch` as the CLI path, which is where the budget, the domain allowlist,
//! the side-effect fence, the dry-run rule and the redaction all live.
//!
//! What is genuinely weaker here is the model, not the containment: a small
//! local model will call the wrong tool more often. That costs a failed run,
//! which is the failure mode this design is built to make cheap.

use errand_core::models::{Event, StepKind};
use errand_core::providers::Provider;
use serde_json::{json, Value};

use crate::executor::ExecError;
use crate::mcp::Outcome;
use crate::state::AppState;

/// How many times round the loop before Errand gives up.
///
/// A ceiling on turns rather than on time or money, because those two are the
/// task's own limits and are checked separately every turn. This one exists so
/// a model that is confused rather than expensive still stops.
const MAX_TURNS: usize = 40;

/// How many identical turns before the model is told it is repeating itself,
/// and how many before the run stops. Two different numbers because a model
/// that is merely stuck sometimes recovers when it is told plainly; one that
/// does not is not going to finish the task at all.
const SAME_CALL_WARN: usize = 2;
const SAME_CALL_STOP: usize = 5;

/// How much of one tool result the model is shown.
///
/// A page snapshot can be enormous and a model on somebody's desk may have a
/// small context window, in which case an untrimmed result loses the whole
/// conversation rather than the tail of one page.
const MAX_TOOL_RESULT_CHARS: usize = 6000;

/// Said once, in the system prompt, because a model that has never driven this
/// protocol will otherwise write out its plan and wait to be asked.
const HOW_TO_WORK: &str = "\nHow this run works:\n\
     - Everything you can do is a tool call. There is no shell, no file system, and no way to \
     reach the internet except the browser tools below.\n\
     - Answer with tool calls, not with a description of what you would do. Nothing happens \
     until you call something.\n\
     - When the job is done call finish. When you cannot do it call fail. The run does not end \
     until you call one of them.\n";

/// What a model is told when it replies with prose instead of acting.
const NUDGE: &str = "Nothing happened, because that was a message rather than a tool call. Use \
                     the tools: call the one you need next, or call finish if the job is done, \
                     or fail if you cannot do it.";

/// Run one task with a model that speaks the OpenAI chat format.
///
/// Returns what the Claude path returns, so the caller can pick either and the
/// rest of the run behaves identically.
pub async fn run_with_tools(
    state: &AppState,
    run_id: &str,
    provider: &Provider,
    model: &str,
    advice: Option<&str>,
) -> Result<Outcome, ExecError> {
    let base = provider.base_url.clone().unwrap_or_default();
    if base.is_empty() {
        return Err(ExecError::NoModel(format!(
            "{} has no address saved, so there is nowhere to send this task. Open Settings, give \
             it an address, then press Run now.",
            provider.label
        )));
    }
    let key = crate::models::key_for(&provider.id).await;

    let run = match errand_core::db::get_run(state.pool(), run_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(ExecError::Spawn("run not found".into())),
        Err(e) => return Err(ExecError::Spawn(e.to_string())),
    };
    let has_playbook = errand_core::db::get_task(state.pool(), &run.task_id)
        .await
        .ok()
        .flatten()
        .and_then(|t| t.playbook_version)
        .is_some();

    // The same list the other path gets, filtered to what this task was
    // actually granted. A tool a task cannot use is better absent than
    // present-and-refused: a model shown list_mail will try it, read the
    // refusal, and spend the rest of the run looking for a way round something
    // that is not a fault.
    let offered_tools = crate::mcp::tools_for_run(state, run_id).await;
    let tools = tools_for_chat(&offered_tools);
    let offered = tool_names();

    // The same prompt the Claude path uses, so a task behaves the same however
    // it is carried out, plus the bit about this protocol that the CLI would
    // otherwise supply itself.
    let mut system = crate::executor::system_prompt(has_playbook);
    if let Some(a) = advice {
        system.push_str("\n\n");
        system.push_str(a);
    }
    system.push_str(HOW_TO_WORK);

    // The same brief the agent would get from read_brief, seeded rather than
    // waited for. read_brief stays available: a model that calls it anyway gets
    // the same text back.
    let brief = tool_text(&crate::mcp::dispatch(state, run_id, "read_brief", &json!({})).await);

    let mut messages: Vec<Value> = vec![
        json!({ "role": "system", "content": system }),
        json!({ "role": "user", "content": format!("Carry out this errand.\n\n{brief}") }),
    ];

    state.emit(Event::StepStarted {
        run_id: run_id.to_string(),
        seq: 0,
        kind: StepKind::Plan,
        title: "Agent started, tool surface verified".into(),
    });

    let mut nudged = false;
    // A model that gets stuck repeating itself.
    //
    // A weaker model can settle into asking for the same thing with the same
    // arguments over and over. One local model listed the same mailbox
    // twenty-five times, a second apart, and then ran out of turns: every one
    // of those was a real Apple Event against a real 191,000-message mailbox,
    // and the run ended saying the agent "stopped without reporting whether it
    // finished", which describes the symptom and hides the cause. So the
    // repeat is named to the model when it starts, and the run is stopped with
    // a reason a person can act on if it carries on regardless.
    let mut same_call: Option<String> = None;
    let mut repeats = 0usize;

    for _ in 0..MAX_TURNS {
        // Checked before asking the model rather than only inside dispatch, so
        // a run that has hit its ceiling stops there instead of spending
        // another turn being refused.
        if crate::executor::budget_breach(state, run_id)
            .await
            .is_some()
        {
            note(
                state,
                run_id,
                "Stopped: this run reached a limit set for it.",
            )
            .await;
            break;
        }

        let turn =
            match crate::models::chat_with_tools(&base, model, &messages, &tools, key.as_deref())
                .await
            {
                Ok(t) => t,
                Err(e) if e.no_tool_support => {
                    return Err(ExecError::NoModel(cannot_use_tools(model, &e)))
                }
                Err(e) => {
                    return Err(ExecError::NoModel(format!(
                        "Errand could not get an answer from {} while carrying out this task: {e}",
                        provider.label
                    )))
                }
            };

        // The model's own narration, journalled the way the Claude path
        // journals it, so a run driven by a local model reads the same.
        if !turn.text.trim().is_empty() {
            note(state, run_id, turn.text.trim()).await;
        }
        messages.push(turn.as_message());

        if turn.tool_calls.is_empty() {
            if nudged {
                return Err(ExecError::NoModel(format!(
                    "{model} replies with words instead of using its tools, so it cannot carry \
                     out a task. Choose a different model."
                )));
            }
            nudged = true;
            messages.push(json!({ "role": "user", "content": NUDGE }));
            continue;
        }
        // It acted, so the next silent turn gets its own second chance rather
        // than inheriting one used up earlier in the run.
        nudged = false;

        let fingerprint = turn
            .tool_calls
            .iter()
            .map(|c| format!("{}({})", c.name, c.arguments))
            .collect::<Vec<_>>()
            .join("; ");
        if same_call.as_ref() == Some(&fingerprint) {
            repeats += 1;
        } else {
            repeats = 0;
            same_call = Some(fingerprint);
        }
        if repeats >= SAME_CALL_STOP {
            return Err(ExecError::NoModel(format!(
                "{model} asked for the same thing {} times over and got no further. Choose a \
                 different model for this task.",
                repeats + 1
            )));
        }
        // Answered rather than dispatched: repeating the call costs real time
        // against real mail, and the answer would be the same one it already
        // has.
        let repeating = repeats >= SAME_CALL_WARN;

        for call in &turn.tool_calls {
            // CONTAINMENT, and asked first, before anything else about the
            // call is considered: nothing outside the list Errand handed over
            // is ever dispatched, whether the model invented the name or read
            // it somewhere.
            let result = if !offered.iter().any(|t| t == &call.name) {
                let line = format!("Refused a tool that does not exist: {}", call.name);
                decided(state, run_id, &line).await;
                format!(
                    "There is no tool called '{}', and Errand will not run one that was not \
                     offered. The only tools that exist are: {}.",
                    call.name,
                    offered.join(", ")
                )
            } else if repeating {
                let line = format!("Asked for the same thing again: {}", call.name);
                decided(state, run_id, &line).await;
                format!(
                    "You have already called '{}' with exactly these arguments {} times in a \
                     row, and the answer will not change. Use the answer you already have, or do \
                     something different. If you have what you need, call finish.",
                    call.name,
                    repeats + 1
                )
            } else if let Some(raw) = &call.unreadable {
                format!(
                    "Errand could not read the arguments you sent for '{}'. They have to be JSON. \
                     You sent: {raw}",
                    call.name
                )
            } else {
                tool_text(&crate::mcp::dispatch(state, run_id, &call.name, &call.arguments).await)
            };

            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "name": call.name,
                "content": clip(&result, MAX_TOOL_RESULT_CHARS),
            }));
        }

        // finish and fail record the outcome through the same mechanism the
        // Claude path uses, and taking it is how this loop learns the run is
        // over.
        if let Some(outcome) = state.take_outcome(run_id) {
            return Ok(outcome);
        }
    }

    // Out of turns, or stopped by the budget. Either way nothing was confirmed,
    // and an outcome set in the last turn is still worth honouring.
    state.take_outcome(run_id).ok_or(ExecError::NoOutcome)
}

/// What to tell somebody whose model turns out not to do tool calling.
///
/// The next stage catches this before a run starts. This is the backstop, and
/// it has to be as plain as the up-front one, because it lands in a run the
/// person was relying on.
fn cannot_use_tools(model: &str, e: &crate::models::ChatError) -> String {
    format!(
        "{model} cannot carry out tasks: it does not support tool calling, which is the only way \
         Errand hands a model the browser and everything else. Pick a different model for \
         \"Doing the task\" on the AI screen. What the server said: {e}"
    )
}

/// The tool surface, in the shape a chat server expects.
///
/// Converted from `mcp::tool_definitions()`, never written out again: one list,
/// so a tool added there appears here and a tool removed there disappears from
/// both paths at once.
fn tools_for_chat(offered: &Value) -> Vec<Value> {
    offered
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t["name"],
                    "description": t["description"],
                    "parameters": t["inputSchema"],
                }
            })
        })
        .collect()
}

/// Every tool name Errand is prepared to run, from the same one list.
fn tool_names() -> Vec<String> {
    crate::mcp::tool_definitions()
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// The text out of a tool result, whichever blocks it came in.
fn tool_text(result: &Value) -> String {
    let blocks = result["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let text: Vec<&str> = blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .filter(|t| !t.is_empty())
        .collect();
    if text.is_empty() {
        "done".into()
    } else {
        text.join("\n")
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push_str("\n\n(cut short by Errand: ask for the part you need another way)");
    out
}

/// Journal a line of the model's own narration, scrubbed of anything secret.
async fn note(state: &AppState, run_id: &str, text: &str) {
    let clean = state.redactor(run_id).scrub(text);
    let _ = errand_core::db::append_step(
        state.pool(),
        run_id,
        "note",
        &crate::executor::truncate(&clean, 400),
        true,
        None,
    )
    .await;
}

/// Journal something Errand decided, rather than something the model said.
async fn decided(state: &AppState, run_id: &str, text: &str) {
    let _ = errand_core::db::append_step(state.pool(), run_id, "decide", text, false, None).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use errand_core::providers::Kind;

    /// A stand-in for a model server on somebody's network. Raw TCP rather than
    /// a web framework, for the same reason models.rs does it: the point is to
    /// prove Errand speaks the wire format, not to test a library.
    ///
    /// Hands back the canned replies in order and counts how many were asked
    /// for, which is how a test tells "it stopped" from "it kept going".
    struct Server {
        base: String,
        asked: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    fn serve(replies: Vec<String>, status: &'static str) -> Server {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = asked.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
            tx.send(l.local_addr().expect("the port it took").port())
                .expect("handing the port back");
            for stream in l.incoming() {
                let Ok(mut sock) = stream else { return };
                use std::io::{Read, Write};
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf);
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // The last reply is repeated, so a test for a model that never
                // stops does not run out of canned answers.
                let body = replies
                    .get(n)
                    .or_else(|| replies.last())
                    .cloned()
                    .unwrap_or_default();
                let res = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(res.as_bytes());
                let _ = sock.flush();
            }
        });
        let port = rx.recv().expect("the stub started");
        Server {
            base: format!("http://127.0.0.1:{port}"),
            asked,
        }
    }

    impl Server {
        fn times_asked(&self) -> usize {
            self.asked.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn provider(&self) -> Provider {
            Provider {
                id: "desk".into(),
                kind: Kind::OpenAiCompat.as_str().into(),
                label: "The model on my desk".into(),
                base_url: Some(self.base.clone()),
                model: Some("qwen3.5-27b".into()),
                enabled: true,
                discovered: false,
                health: None,
                health_detail: None,
            }
        }
        /// The same stub under a name of its own, for a test that needs two
        /// models and has to be able to tell which one did the work.
        fn provider_called(&self, id: &str, label: &str) -> Provider {
            Provider {
                id: id.into(),
                label: label.into(),
                ..self.provider()
            }
        }
    }

    /// Save a model the way the AI screen's own form does.
    async fn save_model(api: &crate::api::testkit::Api, p: &Provider, enabled: bool) {
        let (code, body) = api
            .post(
                "/v1/ai/providers",
                json!({
                    "id": p.id, "kind": p.kind, "label": p.label,
                    "base_url": p.base_url, "model": p.model, "enabled": enabled,
                }),
            )
            .await;
        assert_eq!(code, 200, "saving {} failed: {body}", p.label);
    }

    /// Tell a task which model should carry it out, the way its page does.
    /// `None` puts it back on whatever the AI screen is set to.
    async fn choose_model(
        api: &crate::api::testkit::Api,
        task_id: &str,
        model_id: Option<&str>,
    ) -> (u16, Value) {
        api.patch(
            &format!("/v1/tasks/{task_id}"),
            json!({ "model_id": model_id }),
        )
        .await
    }

    /// Choose the model for every task that has not chosen one, the way the AI
    /// screen does.
    async fn choose_default_model(api: &crate::api::testkit::Api, id: &str) {
        let (code, body) = api
            .post("/v1/ai/roles/executor", json!({ "provider_id": id }))
            .await;
        assert_eq!(code, 200, "choosing the default model failed: {body}");
    }

    /// One assistant turn that calls one tool, in the shape most servers send:
    /// arguments as a JSON string.
    fn calls(name: &str, args: Value) -> String {
        json!({ "choices": [{ "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": format!("c-{name}"),
                "type": "function",
                "function": { "name": name, "arguments": args.to_string() }
            }]
        }}]})
        .to_string()
    }

    fn says(text: &str) -> String {
        json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
    }

    /// A task and a run, set up the way the app sets one up.
    async fn a_run() -> (crate::api::testkit::Api, String) {
        let (api, _task_id, run_id) = a_task_and_a_run().await;
        (api, run_id)
    }

    /// The same, and which task the run is for, which is what the tests about
    /// choosing a model per task need in order to make the choice.
    async fn a_task_and_a_run() -> (crate::api::testkit::Api, String, String) {
        let api = crate::api::testkit::start().await;
        let task_id = crate::api::testkit::a_task(
            &api,
            json!({ "name": "Order the shopping", "description": "Put the usual order in." }),
        )
        .await;
        let run = errand_core::db::try_create_run(
            &api.pool,
            &task_id,
            &format!("manual/{}", errand_core::new_id()),
            "manual",
            errand_core::models::RunMode::NORMAL,
            None,
        )
        .await
        .expect("a run");
        (api, task_id, run.id)
    }

    async fn steps(api: &crate::api::testkit::Api, run_id: &str) -> Vec<String> {
        errand_core::db::list_steps(&api.pool, run_id)
            .await
            .expect("the journal")
            .into_iter()
            .map(|s| s.title)
            .collect()
    }

    #[tokio::test]
    async fn a_local_model_can_actually_carry_out_a_task() {
        // The whole point. A model that is not Claude calls a tool, the tool
        // really runs, and the run ends the way any other run ends.
        let (api, run_id) = a_run().await;
        let server = serve(
            vec![
                calls(
                    "journal",
                    json!({ "title": "Opening the shop", "kind": "plan" }),
                ),
                calls(
                    "finish",
                    json!({ "summary": "The usual order is in.", "answer": "Court 4 is booked for Wednesday at 19:00. Confirmation TC-88421." }),
                ),
            ],
            "200 OK",
        );
        let p = server.provider();

        let outcome = run_with_tools(&api.state, &run_id, &p, "qwen3.5-27b", None)
            .await
            .expect("the loop ran");

        assert!(
            matches!(outcome, Outcome::Finished { ref summary, .. } if summary.contains("usual order")),
            "the run should have finished with the model's own summary: {outcome:?}"
        );
        assert!(
            steps(&api, &run_id)
                .await
                .contains(&"Opening the shop".into()),
            "the tool call never reached dispatch: the journal has no step from it"
        );
    }

    #[tokio::test]
    async fn a_tool_nobody_offered_is_refused_rather_than_run() {
        // A model may name anything at all, including something it read on a
        // page. Only the tools Errand handed over can ever run.
        let (api, run_id) = a_run().await;
        let server = serve(
            vec![
                calls("run_shell_command", json!({ "command": "rm -rf ~" })),
                calls(
                    "finish",
                    json!({ "summary": "Nothing was done.", "answer": "Court 4 is booked for Wednesday at 19:00. Confirmation TC-88421." }),
                ),
            ],
            "200 OK",
        );
        let p = server.provider();

        let outcome = run_with_tools(&api.state, &run_id, &p, "qwen3.5-27b", None)
            .await
            .expect("a made-up tool must not break the run");

        assert!(matches!(outcome, Outcome::Finished { .. }));
        let journal = steps(&api, &run_id).await;
        assert!(
            journal
                .iter()
                .any(|s| s.contains("Refused") && s.contains("run_shell_command")),
            "the refusal has to be visible in the run: {journal:?}"
        );
    }

    #[tokio::test]
    async fn finishing_ends_the_loop_instead_of_going_round_again() {
        let (api, run_id) = a_run().await;
        let server = serve(
            vec![
                calls(
                    "finish",
                    json!({ "summary": "Done on the first turn.", "answer": "Court 4 is booked for Wednesday at 19:00. Confirmation TC-88421." }),
                ),
                calls("journal", json!({ "title": "This should never happen" })),
            ],
            "200 OK",
        );
        let p = server.provider();

        let outcome = run_with_tools(&api.state, &run_id, &p, "qwen3.5-27b", None)
            .await
            .expect("the loop ran");

        assert!(matches!(outcome, Outcome::Finished { .. }));
        assert_eq!(
            server.times_asked(),
            1,
            "the model was asked again after it had already finished"
        );
    }

    #[tokio::test]
    async fn a_model_that_only_ever_talks_is_stopped_rather_than_looped_for_ever() {
        // The failure that would otherwise burn a whole budget in silence.
        let (api, run_id) = a_run().await;
        let server = serve(vec![says("I would start by opening the shop.")], "200 OK");
        let p = server.provider();

        let e = run_with_tools(&api.state, &run_id, &p, "qwen3.5-27b", None)
            .await
            .expect_err("a model that never acts cannot succeed")
            .to_string();

        assert!(
            e.contains("qwen3.5-27b") && e.contains("different model"),
            "the failure must name the model and say what to do: {e}"
        );
        assert!(
            e.lines().count() == 1 && !e.contains("**"),
            "a failure is one plain line, not a formatted paragraph: {e}"
        );
        assert!(
            server.times_asked() <= 2,
            "it should be nudged once and then stopped, not asked {} times",
            server.times_asked()
        );
    }

    #[tokio::test]
    async fn a_model_stuck_asking_the_same_thing_is_stopped_rather_than_ground_to_a_halt() {
        // Seen for real: a 27B model listed the same mailbox twenty-five times,
        // one second apart, until it ran out of turns. Every repeat was a real
        // Apple Event against a real mailbox, and the run ended saying the
        // agent "stopped without reporting whether it finished" -- a sentence
        // about the ceiling that says nothing about the cause. The stub here
        // repeats its last reply for ever, which is exactly that model.
        let (api, run_id) = a_run().await;
        let server = serve(vec![calls("list_mail", json!({ "limit": 5 }))], "200 OK");
        let p = server.provider();

        let e = run_with_tools(&api.state, &run_id, &p, "qwen3.5-27b", None)
            .await
            .expect_err("a model that only repeats itself cannot finish")
            .to_string();

        assert!(
            e.contains("qwen3.5-27b") && e.contains("same thing"),
            "the failure has to name the model and say what it did: {e}"
        );
        assert!(
            server.times_asked() <= SAME_CALL_STOP + 2,
            "it should stop within a few turns, not run to the ceiling of \
             {MAX_TURNS}; it was asked {} times",
            server.times_asked()
        );
        // And the repeat is told to the model before the run is given up on,
        // so one that is merely stuck has a chance to do something else.
        let journal: Vec<String> = errand_core::db::list_steps(api.state.pool(), &run_id)
            .await
            .expect("the run's steps")
            .into_iter()
            .map(|s| s.title)
            .collect();
        assert!(
            journal
                .iter()
                .any(|l| l.contains("Asked for the same thing again")),
            "a person reading the run has to see why it stopped: {journal:?}"
        );
    }

    #[tokio::test]
    async fn a_model_that_cannot_call_tools_says_so_instead_of_flailing() {
        let (api, run_id) = a_run().await;
        let server = serve(
            vec![
                json!({ "error": { "message": "this model does not support tools" } }).to_string(),
            ],
            "400 Bad Request",
        );
        let p = server.provider();

        let e = run_with_tools(&api.state, &run_id, &p, "tiny-local", None)
            .await
            .expect_err("a model without tool calling cannot carry out a task")
            .to_string();

        assert!(
            e.contains("cannot carry out tasks") && e.contains("AI screen"),
            "it must say plainly that this model cannot do it, and what to do: {e}"
        );
    }

    #[tokio::test]
    async fn arguments_that_arrive_as_an_object_are_understood_too() {
        // Several local servers parse the arguments for you. Dropping those
        // calls would make Errand look broken on exactly the setups this whole
        // change exists for.
        let (api, run_id) = a_run().await;
        let already_parsed = json!({ "choices": [{ "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "c1",
                "type": "function",
                "function": { "name": "journal", "arguments": { "title": "Checked the basket" } }
            }]
        }}]})
        .to_string();
        let server = serve(
            vec![
                already_parsed,
                calls(
                    "finish",
                    json!({ "summary": "Checked it.", "answer": "Court 4 is booked for Wednesday at 19:00. Confirmation TC-88421." }),
                ),
            ],
            "200 OK",
        );
        let p = server.provider();

        run_with_tools(&api.state, &run_id, &p, "qwen3.5-27b", None)
            .await
            .expect("the loop ran");

        assert!(
            steps(&api, &run_id)
                .await
                .contains(&"Checked the basket".into()),
            "a call whose arguments arrived already parsed was dropped"
        );
    }

    #[tokio::test]
    async fn a_provider_with_no_address_is_refused_before_anything_is_sent() {
        let (api, run_id) = a_run().await;
        let p = Provider {
            id: "nowhere".into(),
            kind: Kind::OpenAiCompat.as_str().into(),
            label: "Half-filled-in model".into(),
            base_url: None,
            model: Some("m".into()),
            enabled: true,
            discovered: false,
            health: None,
            health_detail: None,
        };
        let e = run_with_tools(&api.state, &run_id, &p, "m", None)
            .await
            .expect_err("there is nowhere to send it")
            .to_string();
        assert!(e.contains("no address saved"), "{e}");
    }

    #[tokio::test]
    async fn switching_claude_off_leaves_the_model_on_your_own_network_doing_the_work() {
        // The complaint this whole loop exists for, taken end to end: somebody
        // whose only model is on their own network presses Run now, and the
        // task is carried out instead of being refused. Goes through carry_out,
        // which is what a real run goes through, rather than calling the loop
        // directly.
        let (api, run_id) = a_run().await;
        let server = serve(
            vec![
                calls("journal", json!({ "title": "Signed in", "kind": "act" })),
                calls(
                    "finish",
                    json!({ "summary": "Ordered, arriving Friday.", "answer": "Court 4 is booked for Wednesday at 19:00. Confirmation TC-88421." }),
                ),
            ],
            "200 OK",
        );
        errand_core::db::upsert_provider(&api.pool, &server.provider())
            .await
            .expect("saving the model the way Settings does");

        let outcome = crate::executor::carry_out(
            &api.state,
            &run_id,
            crate::executor::ExecOptions::default(),
        )
        .await
        .expect("a local model should be able to carry out a task");

        assert!(matches!(outcome, Outcome::Finished { .. }), "{outcome:?}");
        let journal = steps(&api, &run_id).await;
        assert!(
            journal.iter().any(|s| s.contains("qwen3.5-27b")),
            "the run has to say which model did the work: {journal:?}"
        );
        assert!(
            journal
                .iter()
                .any(|s| s.contains("Nothing about this task leaves your machine")),
            "a run on your own network should say so: {journal:?}"
        );
    }

    #[tokio::test]
    async fn a_local_model_does_not_promise_privacy_the_rest_of_the_run_will_not_keep() {
        // Choosing a model for one task only chooses who carries it out.
        // Writing the plan afterwards, explaining a failure and wording a
        // notification each resolve against the global choice, so a mail task
        // pointed at the machine on the desk can still have its journal -- who
        // wrote to the person, and about what -- handed to a service a moment
        // later. The line people read to check must not say otherwise.
        let (api, task_id, run_id) = a_task_and_a_run().await;
        let local = serve(
            vec![calls(
                "finish",
                json!({ "summary": "Done.", "answer": "Court 4 is booked for Wednesday at 19:00. Confirmation TC-88421." }),
            )],
            "200 OK",
        );
        save_model(&api, &local.provider(), true).await;

        // Something on the internet, enabled, which nothing has ruled out for
        // writing the plan or the summary.
        let away = errand_core::providers::Provider {
            id: "hosted".into(),
            kind: Kind::OpenAiCompat.as_str().into(),
            label: "A service on the internet".into(),
            base_url: Some("https://api.example.com".into()),
            model: Some("something-large".into()),
            enabled: true,
            discovered: false,
            health: None,
            health_detail: None,
        };
        assert!(
            !away.is_local(),
            "the second provider has to be off-machine"
        );
        save_model(&api, &away, true).await;

        // The task asks for the one on the desk, which is the whole point of
        // choosing a model per task.
        let (code, body) = choose_model(&api, &task_id, Some(&local.provider().id)).await;
        assert_eq!(code, 200, "{body}");

        let _ = crate::executor::carry_out(
            &api.state,
            &run_id,
            crate::executor::ExecOptions::default(),
        )
        .await;

        let journal = steps(&api, &run_id).await;
        assert!(
            !journal
                .iter()
                .any(|s| s.contains("Nothing about this task leaves your machine")),
            "the run promised privacy that only covers the model doing the work: {journal:?}"
        );
        assert!(
            journal
                .iter()
                .any(|s| s.contains("done on your machine") && s.contains("Keep everything")),
            "it has to say what is still not staying here, and how to change it: {journal:?}"
        );
    }

    #[tokio::test]
    async fn a_whole_run_really_ends_as_done_when_a_local_model_did_it() {
        // Through run_to_completion, which is what the scheduler and the Run
        // now button both call. Anything short of this can pass while the
        // feature is unreachable, which this repository has managed before.
        let (api, run_id) = a_run().await;
        let server = serve(
            vec![calls(
                "finish",
                json!({ "summary": "Booked the court for Wednesday.", "answer": "Court 4 is booked for Wednesday at 19:00. Confirmation TC-88421." }),
            )],
            "200 OK",
        );
        errand_core::db::upsert_provider(&api.pool, &server.provider())
            .await
            .expect("saving the model");

        crate::executor::run_to_completion(api.state.clone(), run_id.clone()).await;

        let run = errand_core::db::get_run(&api.pool, &run_id)
            .await
            .expect("reading the run")
            .expect("the run is there");
        assert_eq!(
            run.status, "succeeded",
            "a run a local model completed should read as done: {run:?}"
        );
        assert_eq!(
            run.summary.as_deref(),
            Some("Booked the court for Wednesday."),
            "the summary the person reads should be the model's own"
        );
    }

    #[tokio::test]
    async fn with_no_model_at_all_the_run_says_what_to_set_up() {
        let (api, run_id) = a_run().await;
        let e = crate::executor::carry_out(
            &api.state,
            &run_id,
            crate::executor::ExecOptions::default(),
        )
        .await
        .expect_err("there is nothing to run it with")
        .to_string();
        assert!(
            e.contains("AI screen") && e.contains("claude /login"),
            "the failure has to name what to configure: {e}"
        );
    }

    /// Two models that answer differently, so a test can tell which one did the
    /// work from the summary alone.
    fn two_models() -> (Server, Server) {
        (
            serve(
                vec![calls(
                    "finish",
                    json!({ "summary": "The usual model did it.", "answer": "Court 4 is booked for Wednesday at 19:00. Confirmation TC-88421." }),
                )],
                "200 OK",
            ),
            serve(
                vec![calls(
                    "finish",
                    json!({ "summary": "The model I chose did it.", "answer": "Court 4 is booked for Wednesday at 19:00. Confirmation TC-88421." }),
                )],
                "200 OK",
            ),
        )
    }

    #[tokio::test]
    async fn a_task_that_names_no_model_is_carried_out_by_the_one_chosen_in_settings() {
        let (api, _task_id, run_id) = a_task_and_a_run().await;
        let (usual, mine) = two_models();
        save_model(
            &api,
            &usual.provider_called("usual", "The usual model"),
            true,
        )
        .await;
        save_model(
            &api,
            &mine.provider_called("mine", "The model on my desk"),
            true,
        )
        .await;
        choose_default_model(&api, "usual").await;

        let outcome = crate::executor::carry_out(
            &api.state,
            &run_id,
            crate::executor::ExecOptions::default(),
        )
        .await
        .expect("the run went ahead");

        assert!(
            matches!(outcome, Outcome::Finished { ref summary, .. } if summary.contains("usual model")),
            "a task that names no model must use the one Settings chose: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_task_that_names_a_model_is_carried_out_by_that_one_and_not_the_usual_one() {
        // What the whole change is for. Whichever model does the work is the
        // one that reads what the tools hand back, so a task that reads a
        // mailbox has to be able to insist on a model of your own even where
        // everything else goes to a service.
        let (api, task_id, run_id) = a_task_and_a_run().await;
        let (usual, mine) = two_models();
        save_model(
            &api,
            &usual.provider_called("usual", "The usual model"),
            true,
        )
        .await;
        save_model(
            &api,
            &mine.provider_called("mine", "The model on my desk"),
            true,
        )
        .await;
        choose_default_model(&api, "usual").await;

        let (code, body) = choose_model(&api, &task_id, Some("mine")).await;
        assert_eq!(code, 200, "naming a model for this task failed: {body}");

        let outcome = crate::executor::carry_out(
            &api.state,
            &run_id,
            crate::executor::ExecOptions::default(),
        )
        .await
        .expect("the run went ahead");

        assert!(
            matches!(outcome, Outcome::Finished { ref summary, .. } if summary.contains("model I chose")),
            "the model the task names has to be the one that does the work: {outcome:?}"
        );
        let journal = steps(&api, &run_id).await;
        assert!(
            journal
                .iter()
                .any(|s| s.contains("The model on my desk") && s.contains("This task asks for")),
            "the run has to say the model was the task's own choice: {journal:?}"
        );
    }

    #[tokio::test]
    async fn changing_the_model_in_settings_leaves_a_task_that_named_its_own_alone() {
        // Otherwise the per-task choice is not a choice at all: it would last
        // only until somebody touched the AI screen for an unrelated reason.
        let (api, task_id, run_id) = a_task_and_a_run().await;
        let (usual, mine) = two_models();
        save_model(
            &api,
            &usual.provider_called("usual", "The usual model"),
            true,
        )
        .await;
        save_model(
            &api,
            &mine.provider_called("mine", "The model on my desk"),
            true,
        )
        .await;

        let (code, body) = choose_model(&api, &task_id, Some("mine")).await;
        assert_eq!(code, 200, "naming a model for this task failed: {body}");
        // Afterwards, and about every other task.
        choose_default_model(&api, "usual").await;

        let outcome = crate::executor::carry_out(
            &api.state,
            &run_id,
            crate::executor::ExecOptions::default(),
        )
        .await
        .expect("the run went ahead");

        assert!(
            matches!(outcome, Outcome::Finished { ref summary, .. } if summary.contains("model I chose")),
            "changing the default must not take a task's own model away: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_task_whose_model_can_no_longer_do_the_job_runs_on_one_that_can_and_says_so() {
        // A choice is a preference, not a requirement. A model switched off
        // after it was chosen, and one Errand cannot drive a task through at
        // all, are both passed over rather than allowed to fail the run: the
        // shopping still gets ordered, and the run says what happened.
        let (api, task_id, run_id) = a_task_and_a_run().await;
        let (usual, mine) = two_models();
        save_model(
            &api,
            &usual.provider_called("usual", "The usual model"),
            true,
        )
        .await;
        save_model(
            &api,
            &mine.provider_called("mine", "The model on my desk"),
            true,
        )
        .await;
        errand_core::db::upsert_provider(
            &api.pool,
            &Provider {
                id: "anthropic".into(),
                kind: Kind::AnthropicApi.as_str().into(),
                label: "Anthropic".into(),
                base_url: None,
                model: Some("claude-sonnet-4-5".into()),
                enabled: true,
                discovered: false,
                health: None,
                health_detail: None,
            },
        )
        .await
        .expect("saving a model Errand cannot drive a task through");

        let (code, body) = choose_model(&api, &task_id, Some("mine")).await;
        assert_eq!(code, 200, "naming a model for this task failed: {body}");
        // And then switched off, the way the AI screen switches one off.
        save_model(
            &api,
            &mine.provider_called("mine", "The model on my desk"),
            false,
        )
        .await;

        let outcome = crate::executor::carry_out(
            &api.state,
            &run_id,
            crate::executor::ExecOptions::default(),
        )
        .await
        .expect("a model that cannot be used must not stop the task");

        assert!(
            matches!(outcome, Outcome::Finished { ref summary, .. } if summary.contains("usual model")),
            "it should have fallen back to something that works: {outcome:?}"
        );
        let journal = steps(&api, &run_id).await;
        assert!(
            journal
                .iter()
                .any(|s| s.contains("The model on my desk") && s.contains("switched off")),
            "the run has to say the task did not get the model it asked for, and why: {journal:?}"
        );
    }

    #[test]
    fn the_tool_list_handed_over_is_the_one_list_and_nothing_else() {
        let offered = tool_names();
        assert_eq!(
            offered.len(),
            crate::mcp::qualified_tool_names().len(),
            "the two paths must offer the same tools"
        );
        for t in tools_for_chat(&crate::mcp::tool_definitions()) {
            assert_eq!(t["type"], "function");
            let name = t["function"]["name"].as_str().expect("a name");
            assert!(offered.contains(&name.to_string()));
            assert!(
                t["function"]["parameters"]["type"] == "object",
                "{name} was handed over without a usable schema"
            );
        }
    }
}
