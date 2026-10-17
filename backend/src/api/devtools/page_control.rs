//! Commands for the page an HTML Input block renders (`HtmlPageCommand`):
//! run JavaScript, add CSS, freeze and resume. Sent straight to the page's
//! own DevTools endpoint on the loopback debug port, one short session per
//! command.

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use strom_types::devtools::HtmlPageCommand;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Id of the stylesheet [`HtmlPageCommand::SetCss`] manages in the page.
const CSS_ELEMENT_ID: &str = "strom-page-css";

/// How long a page has to answer.
const ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Why a page command did not complete.
#[derive(Debug, PartialEq)]
pub(super) enum PageError {
    /// The page could not be reached or did not answer in time.
    Unreachable(String),
    /// The page answered with an error, or the script threw.
    Failed(String),
}

/// The DevTools protocol message for `command`, with message id `id`.
pub(super) fn cdp_message(command: &HtmlPageCommand, id: i64) -> Value {
    match command {
        HtmlPageCommand::Evaluate { expression } => evaluate(id, expression),
        HtmlPageCommand::SetCss { css } => {
            // The CSS goes in as a JSON string literal, so nothing in it
            // can end the script.
            let css = serde_json::to_string(css).expect("a string serializes");
            let script = format!(
                "(() => {{ const css = {css}; let s = document.getElementById({id:?}); \
                 if (!css) {{ if (s) s.remove(); return true; }} \
                 if (!s) {{ s = document.createElement('style'); s.id = {id:?}; \
                 (document.head || document.documentElement).appendChild(s); }} \
                 s.textContent = css; return true; }})()",
                id = CSS_ELEMENT_ID
            );
            evaluate(id, &script)
        }
        HtmlPageCommand::Freeze => lifecycle(id, "frozen"),
        HtmlPageCommand::Resume => lifecycle(id, "active"),
    }
}

fn evaluate(id: i64, expression: &str) -> Value {
    json!({
        "id": id,
        "method": "Runtime.evaluate",
        "params": { "expression": expression, "returnByValue": true, "awaitPromise": true }
    })
}

fn lifecycle(id: i64, state: &str) -> Value {
    json!({ "id": id, "method": "Page.setWebLifecycleState", "params": { "state": state } })
}

/// The value in the page's answer to [`cdp_message`].
pub(super) fn answer_value(answer: &Value) -> Result<Value, PageError> {
    if let Some(error) = answer.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error");
        return Err(PageError::Failed(message.to_string()));
    }
    let result = answer.get("result").cloned().unwrap_or(Value::Null);
    if let Some(details) = result.get("exceptionDetails") {
        let text = details
            .pointer("/exception/description")
            .or_else(|| details.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("the script threw");
        return Err(PageError::Failed(text.to_string()));
    }
    Ok(result
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null))
}

/// Send `command` to the page at `ws_url` and wait for its answer.
pub(super) async fn run(ws_url: &str, command: &HtmlPageCommand) -> Result<Value, PageError> {
    let unreachable = |e: &dyn std::fmt::Display| PageError::Unreachable(e.to_string());
    let (mut socket, _) =
        tokio::time::timeout(ANSWER_TIMEOUT, tokio_tungstenite::connect_async(ws_url))
            .await
            .map_err(|e| unreachable(&e))?
            .map_err(|e| unreachable(&e))?;
    let id = 1;
    socket
        .send(WsMessage::Text(cdp_message(command, id).to_string().into()))
        .await
        .map_err(|e| unreachable(&e))?;
    let answer = tokio::time::timeout(ANSWER_TIMEOUT, async {
        while let Some(Ok(message)) = socket.next().await {
            if let WsMessage::Text(text) = message {
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                // Events carry no id; only our answer does.
                if value.get("id").and_then(Value::as_i64) == Some(id) {
                    return Some(value);
                }
            }
        }
        None
    })
    .await
    .map_err(|e| unreachable(&e))?
    .ok_or_else(|| PageError::Unreachable("the page closed the session".into()))?;
    let _ = socket.close(None).await;
    answer_value(&answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for one page's DevTools endpoint: answers each command
    /// with `reply(command)`, after an event the client must skip, and
    /// hands back what it received.
    async fn fake_page(
        reply: fn(&Value) -> Value,
    ) -> (String, tokio::sync::oneshot::Receiver<Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/devtools/page/ABC", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            if let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                let command: Value = serde_json::from_str(text.as_str()).unwrap();
                let event = json!({ "method": "Page.lifecycleEvent", "params": {} });
                ws.send(WsMessage::Text(event.to_string().into()))
                    .await
                    .unwrap();
                let mut answer = reply(&command);
                answer["id"] = command["id"].clone();
                ws.send(WsMessage::Text(answer.to_string().into()))
                    .await
                    .unwrap();
                let _ = tx.send(command);
            }
        });
        (url, rx)
    }

    #[tokio::test]
    async fn evaluate_returns_the_scripts_value() {
        let (url, sent) =
            fake_page(|_| json!({ "result": { "result": { "type": "number", "value": 42 } } }))
                .await;
        let command = HtmlPageCommand::Evaluate {
            expression: "window.next()".into(),
        };
        assert_eq!(run(&url, &command).await, Ok(json!(42)));
        let sent = sent.await.unwrap();
        assert_eq!(sent["method"], "Runtime.evaluate");
        assert_eq!(sent["params"]["expression"], "window.next()");
        assert_eq!(sent["params"]["awaitPromise"], true);
    }

    #[tokio::test]
    async fn a_script_that_throws_is_a_failure() {
        let (url, _) = fake_page(|_| {
            json!({ "result": {
                "result": { "type": "object" },
                "exceptionDetails": { "text": "Uncaught", "exception": { "description": "ReferenceError: next is not defined" } }
            } })
        })
        .await;
        let command = HtmlPageCommand::Evaluate {
            expression: "next()".into(),
        };
        assert_eq!(
            run(&url, &command).await,
            Err(PageError::Failed(
                "ReferenceError: next is not defined".into()
            ))
        );
    }

    #[tokio::test]
    async fn freeze_and_resume_set_the_lifecycle_state() {
        for (command, state) in [
            (HtmlPageCommand::Freeze, "frozen"),
            (HtmlPageCommand::Resume, "active"),
        ] {
            let (url, sent) = fake_page(|_| json!({ "result": {} })).await;
            assert_eq!(run(&url, &command).await, Ok(Value::Null));
            let sent = sent.await.unwrap();
            assert_eq!(sent["method"], "Page.setWebLifecycleState");
            assert_eq!(sent["params"]["state"], state);
        }
    }

    /// The CSS travels as a string literal: a quote or a closing script
    /// tag in it cannot break out of the script.
    #[test]
    fn css_is_quoted_into_the_script() {
        let css = "body { background: transparent } /* \"); alert(1); (\" */";
        let message = cdp_message(&HtmlPageCommand::SetCss { css: css.into() }, 7);
        let script = message["params"]["expression"].as_str().unwrap();
        assert!(script.contains(&serde_json::to_string(css).unwrap()));
        assert!(script.contains(CSS_ELEMENT_ID));
        assert_eq!(message["id"], 7);
    }

    #[tokio::test]
    async fn an_error_answer_is_a_failure() {
        let (url, _) =
            fake_page(|_| json!({ "error": { "code": -32000, "message": "Not allowed" } })).await;
        assert_eq!(
            run(&url, &HtmlPageCommand::Freeze).await,
            Err(PageError::Failed("Not allowed".into()))
        );
    }

    #[tokio::test]
    async fn a_page_that_is_gone_is_unreachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/devtools/page/X", listener.local_addr().unwrap());
        drop(listener);
        assert!(matches!(
            run(&url, &HtmlPageCommand::Resume).await,
            Err(PageError::Unreachable(_))
        ));
    }
}
