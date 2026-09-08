//! Independently framed deterministic LSP fixture for acceptance tests.

use std::{
    collections::BTreeMap,
    env,
    fs::File,
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

const BODY_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Standard,
    Fragmented,
    Delayed,
    DelayedInitialization,
    NotificationFlood,
    OutOfOrder,
    MalformedHeader,
    MalformedJson,
    ConflictingLength,
    OversizedFrame,
    Crash,
    Hang,
}

impl Scenario {
    fn from_arguments() -> Result<Self, String> {
        let value = env::args()
            .find_map(|argument| argument.strip_prefix("--scenario=").map(str::to_owned))
            .unwrap_or_else(|| "standard".to_owned());
        match value.as_str() {
            "standard" => Ok(Self::Standard),
            "fragmented" => Ok(Self::Fragmented),
            "delayed" => Ok(Self::Delayed),
            "delayed-initialization" => Ok(Self::DelayedInitialization),
            "notification-flood" => Ok(Self::NotificationFlood),
            "out-of-order" => Ok(Self::OutOfOrder),
            "malformed-header" => Ok(Self::MalformedHeader),
            "malformed-json" => Ok(Self::MalformedJson),
            "conflicting-length" => Ok(Self::ConflictingLength),
            "oversized-frame" => Ok(Self::OversizedFrame),
            "crash" => Ok(Self::Crash),
            "hang" => Ok(Self::Hang),
            _ => Err(format!("unknown scenario: {value}")),
        }
    }
}

fn main() -> ExitCode {
    let event_log =
        env::args().find_map(|argument| argument.strip_prefix("--event-log=").map(PathBuf::from));
    match Scenario::from_arguments() {
        Ok(Scenario::Crash) => {
            eprintln!("fixture server crashed before initialization");
            ExitCode::from(42)
        }
        Ok(Scenario::Hang) => {
            thread::sleep(Duration::from_secs(30));
            ExitCode::SUCCESS
        }
        Ok(Scenario::MalformedHeader) => raw(b"Content-Length nope\r\n\r\n"),
        Ok(Scenario::MalformedJson) => raw(b"Content-Length: 9\r\n\r\n{not json"),
        Ok(Scenario::ConflictingLength) => raw(b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}"),
        Ok(Scenario::OversizedFrame) => raw(b"Content-Length: 67108865\r\n\r\n"),
        Ok(scenario) => serve(scenario, event_log),
        Err(error) => {
            eprintln!("lspctl fake server: {error}");
            ExitCode::from(2)
        }
    }
}

fn raw(bytes: &[u8]) -> ExitCode {
    let mut output = io::stdout().lock();
    if output
        .write_all(bytes)
        .and_then(|()| output.flush())
        .is_ok()
    {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn serve(scenario: Scenario, event_log: Option<PathBuf>) -> ExitCode {
    let mut input = BufReader::new(io::stdin());
    let mut output = io::stdout().lock();
    let mut event_log = match event_log.map(File::create).transpose() {
        Ok(log) => log,
        Err(_) => return ExitCode::from(1),
    };
    let mut delayed = None;
    let mut workspace_uri = None;
    let mut partial_limit_request = None;
    let mut open_documents = BTreeMap::new();
    let mut related_mode = env::args().find_map(|argument| {
        argument
            .strip_prefix("--related-diagnostics=")
            .map(str::to_owned)
    });
    let mut diagnostic_revision = 0;
    loop {
        let message = match read_frame(&mut input) {
            Ok(Some(message)) => message,
            Ok(None) => return ExitCode::SUCCESS,
            Err(error) => {
                let _ = write_frame(
                    &mut output,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error}}),
                    scenario,
                );
                return ExitCode::from(65);
            }
        };
        if let (Some(log), Some(method)) = (
            event_log.as_mut(),
            message.get("method").and_then(Value::as_str),
        ) && writeln!(log, "{method}")
            .and_then(|()| log.flush())
            .is_err()
        {
            return ExitCode::from(1);
        }
        match message.get("method").and_then(Value::as_str) {
            Some("exit") => return ExitCode::SUCCESS,
            Some("initialize") => {
                workspace_uri = message
                    .pointer("/params/rootUri")
                    .and_then(Value::as_str)
                    .and_then(|uri| url::Url::parse(uri).ok());
                if scenario == Scenario::DelayedInitialization {
                    if let Some(gate) = env::args().find_map(|argument| {
                        argument
                            .strip_prefix("--initialization-gate=")
                            .map(PathBuf::from)
                    }) {
                        let deadline = Instant::now() + Duration::from_secs(10);
                        while !gate.is_file() {
                            if Instant::now() >= deadline {
                                return ExitCode::from(1);
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                    } else {
                        thread::sleep(Duration::from_millis(500));
                    }
                }
                for response in [
                    json!({"jsonrpc":"2.0","method":"window/logMessage","params":{"type":3,"message":"fixture initialized"}}),
                    json!({"jsonrpc":"2.0","method":"$/progress","params":{"token":"fixture-progress","value":{"kind":"begin","title":"fixture"}}}),
                    json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///fixture.rs","diagnostics":[]}}),
                    json!({"jsonrpc":"2.0","id":"fixture-callback","method":"client/registerCapability","params":{"registrations":[]}}),
                ] {
                    if write_frame(&mut output, &response, scenario).is_err() {
                        return ExitCode::from(1);
                    }
                }
                let capabilities = if matches!(
                    scenario,
                    Scenario::Standard | Scenario::DelayedInitialization
                ) {
                    json!({
                        "positionEncoding": "utf-16",
                        "textDocumentSync": {"openClose": true},
                        "definitionProvider": true,
                        "referencesProvider": true,
                        "hoverProvider": true,
                        "documentSymbolProvider": true,
                        "workspaceSymbolProvider": true,
                        "documentFormattingProvider": true,
                        "renameProvider": {"prepareProvider": true},
                        "codeActionProvider": {"resolveProvider": true},
                        "executeCommandProvider": {"commands": ["fixture.run"]},
                        "diagnosticProvider": {
                            "interFileDependencies": related_mode.is_some(),
                            "workspaceDiagnostics": true
                        }
                    })
                } else {
                    json!({})
                };
                if result(
                    &mut output,
                    &message,
                    json!({"capabilities": capabilities}),
                    scenario,
                )
                .is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("textDocument/definition")
            | Some("textDocument/references")
            | Some("textDocument/documentSymbol")
            | Some("textDocument/formatting")
            | Some("textDocument/codeAction") => {
                if result(&mut output, &message, json!([]), scenario).is_err() {
                    return ExitCode::from(1);
                }
            }
            Some("workspace/symbol")
                if message.pointer("/params/query").and_then(Value::as_str)
                    == Some("error-with-partial") =>
            {
                let progress = json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": {
                        "token": message.pointer("/params/partialResultToken").cloned().unwrap_or(Value::Null),
                        "value": [{"name": "partial-symbol", "kind": 12}]
                    }
                });
                let error = json!({
                    "jsonrpc": "2.0",
                    "id": message.get("id").cloned().unwrap_or(Value::Null),
                    "error": {"code": -32603, "message": "fixture failure", "data": {"fixture": true}}
                });
                if write_frame(&mut output, &progress, scenario).is_err()
                    || write_frame(&mut output, &error, scenario).is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("workspace/symbol")
                if matches!(
                    message.pointer("/params/query").and_then(Value::as_str),
                    Some(
                        "partial-chunks-success" | "partial-chunks-error" | "partial-chunks-limit"
                    )
                ) =>
            {
                let query = message["params"]["query"].as_str().unwrap();
                let uri = workspace_uri.as_ref().unwrap().join("partial.rs").unwrap();
                let symbol = |name| {
                    json!({
                        "name": name,
                        "kind": 12,
                        "location": {
                            "uri": uri.as_str(),
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 1}
                            }
                        }
                    })
                };
                let mut chunks = vec![json!([symbol("partial-first"), symbol("partial-second")])];
                if query != "partial-chunks-limit" {
                    chunks.push(json!([symbol("partial-third")]));
                }
                for chunk in chunks {
                    let progress = json!({
                        "jsonrpc": "2.0",
                        "method": "$/progress",
                        "params": {"token": message["params"]["partialResultToken"], "value": chunk}
                    });
                    if write_frame(&mut output, &progress, scenario).is_err() {
                        return ExitCode::from(1);
                    }
                }
                match query {
                    "partial-chunks-limit" => partial_limit_request = Some(message["id"].clone()),
                    "partial-chunks-error" => {
                        let error = json!({
                            "jsonrpc": "2.0",
                            "id": message["id"],
                            "error": {"code": -32603, "message": "fixture failure", "data": {"fixture": true}}
                        });
                        if write_frame(&mut output, &error, scenario).is_err() {
                            return ExitCode::from(1);
                        }
                    }
                    _ => {
                        if result(
                            &mut output,
                            &message,
                            json!([symbol("final-symbol")]),
                            scenario,
                        )
                        .is_err()
                        {
                            return ExitCode::from(1);
                        }
                    }
                }
            }
            Some("$/cancelRequest")
                if partial_limit_request.is_some()
                    && message.pointer("/params/id") == partial_limit_request.as_ref() =>
            {
                let error = json!({
                    "jsonrpc": "2.0",
                    "id": partial_limit_request.take().unwrap(),
                    "error": {"code": -32800, "message": "fixture request cancelled"}
                });
                if write_frame(&mut output, &error, scenario).is_err() {
                    return ExitCode::from(1);
                }
            }
            Some("workspace/symbol") => {
                if result(&mut output, &message, json!([]), scenario).is_err() {
                    return ExitCode::from(1);
                }
            }
            Some("textDocument/hover") | Some("textDocument/prepareRename") => {
                if result(&mut output, &message, Value::Null, scenario).is_err() {
                    return ExitCode::from(1);
                }
            }
            Some("test/related-diagnostics-mode") if related_mode.is_some() => {
                related_mode = Some(message["params"]["mode"].as_str().unwrap().to_owned());
                if result(&mut output, &message, Value::Null, scenario).is_err() {
                    return ExitCode::from(1);
                }
            }
            Some("textDocument/diagnostic" | "workspace/diagnostic")
                if related_mode.is_some() =>
            {
                match related_diagnostics(
                    &mut output,
                    &message,
                    workspace_uri.as_ref().unwrap(),
                    related_mode.as_deref().unwrap(),
                    &mut diagnostic_revision,
                    scenario,
                ) {
                    Ok(true) => partial_limit_request = Some(message["id"].clone()),
                    Ok(false) => {}
                    Err(()) => return ExitCode::from(1),
                }
            }
            Some("textDocument/diagnostic") => {
                if result(
                    &mut output,
                    &message,
                    json!({"kind":"full", "resultId":"fixture", "items":[]}),
                    scenario,
                )
                .is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("workspace/diagnostic") => {
                if result(&mut output, &message, json!({"items":[]}), scenario).is_err() {
                    return ExitCode::from(1);
                }
            }
            Some("textDocument/rename") => {
                let uri = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .unwrap_or("file:///fixture.rs");
                let new_name = message
                    .pointer("/params/newName")
                    .and_then(Value::as_str)
                    .unwrap_or("renamed");
                if result(
                    &mut output,
                    &message,
                    json!({"changes": {(uri): [{
                        "range": {
                            "start": {"line": 0, "character": 3},
                            "end": {"line": 0, "character": 6}
                        },
                        "newText": new_name
                    }]}}),
                    scenario,
                )
                .is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("codeAction/resolve") => {
                let action = message.get("params").cloned().unwrap_or_else(|| json!({}));
                if result(&mut output, &message, action, scenario).is_err() {
                    return ExitCode::from(1);
                }
            }
            Some("workspace/executeCommand") => {
                let callback_applied = message
                    .pointer("/params/arguments/0")
                    .and_then(Value::as_str)
                    .map(|uri| {
                        let callback = json!({
                            "jsonrpc": "2.0",
                            "id": "fixture-apply-edit",
                            "method": "workspace/applyEdit",
                            "params": {
                                "label": "fixture edit",
                                "edit": {"changes": {(uri): [{
                                    "range": {
                                        "start": {"line": 0, "character": 0},
                                        "end": {"line": 0, "character": 3}
                                    },
                                    "newText": "new"
                                }]}}
                            }
                        });
                        if write_frame(&mut output, &callback, scenario).is_err() {
                            return false;
                        }
                        read_callback_result(
                            &mut input,
                            &mut open_documents,
                            &json!("fixture-apply-edit"),
                        )
                        .and_then(|result| result.get("applied").and_then(Value::as_bool))
                        .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if result(
                    &mut output,
                    &message,
                    json!({"callbackApplied": callback_applied}),
                    scenario,
                )
                .is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("test/request-apply-edit") => {
                let uri = message
                    .pointer("/params/uri")
                    .and_then(Value::as_str)
                    .unwrap_or("file:///fixture.rs");
                let callback = json!({
                    "jsonrpc": "2.0",
                    "id": "fixture-preview-edit",
                    "method": "workspace/applyEdit",
                    "params": {
                        "label": "fixture preview",
                        "edit": {"changes": {(uri): [{
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 3}
                            },
                            "newText": "new"
                        }]}}
                    }
                });
                if write_frame(&mut output, &callback, scenario).is_err() {
                    return ExitCode::from(1);
                }
                let callback_response = read_callback_result(
                    &mut input,
                    &mut open_documents,
                    &json!("fixture-preview-edit"),
                )
                .unwrap_or(Value::Null);
                if result(
                    &mut output,
                    &message,
                    json!({"callbackResponse": callback_response}),
                    scenario,
                )
                .is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("test/start-progress") => {
                let callback_id = json!("fixture-progress-create");
                if write_frame(
                    &mut output,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": callback_id,
                        "method": "window/workDoneProgress/create",
                        "params": {"token": "fixture-indexing"}
                    }),
                    scenario,
                )
                .is_err()
                    || read_callback_result(&mut input, &mut open_documents, &callback_id).is_none()
                    || write_frame(
                        &mut output,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "$/progress",
                            "params": {
                                "token": "fixture-indexing",
                                "value": {
                                    "kind": "begin",
                                    "title": "Indexing",
                                    "message": "loading workspace",
                                    "percentage": 25,
                                    "cancellable": false
                                }
                            }
                        }),
                        scenario,
                    )
                    .is_err()
                    || result(&mut output, &message, json!({"fixture": true}), scenario).is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("test/server-request-limit") => {
                if write_configuration_request_burst(&mut output, "fixture-callback").is_err() {
                    return ExitCode::from(1);
                }
                let mut accepted = 0;
                let mut busy = 0;
                for _ in 0..=64 {
                    let Some(response) = read_callback_message(&mut input, &mut open_documents)
                    else {
                        return ExitCode::from(1);
                    };
                    if response
                        .pointer("/error/data/reason")
                        .and_then(Value::as_str)
                        == Some("client_busy")
                    {
                        busy += 1;
                    } else if response.get("result").is_some() {
                        accepted += 1;
                    }
                }
                if result(
                    &mut output,
                    &message,
                    json!({"accepted": accepted, "busy": busy}),
                    scenario,
                )
                .is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("test/cancel-server-request") => {
                let callback_id = json!("fixture-cancellable-63");
                if write_configuration_request_burst(&mut output, "fixture-cancellable").is_err() {
                    return ExitCode::from(1);
                }

                let mut capacity_observed = false;
                for _ in 0..=64 {
                    let Some(response) = read_callback_message(&mut input, &mut open_documents)
                    else {
                        return ExitCode::from(1);
                    };
                    if response
                        .pointer("/error/data/reason")
                        .and_then(Value::as_str)
                        == Some("client_busy")
                    {
                        capacity_observed = true;
                        break;
                    }
                }
                if !capacity_observed
                    || write_frame(
                        &mut output,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "$/cancelRequest",
                            "params": {"id": callback_id}
                        }),
                        scenario,
                    )
                    .is_err()
                {
                    return ExitCode::from(1);
                }

                let mut callback_response = None;
                for _ in 0..64 {
                    let Some(response) = read_callback_message(&mut input, &mut open_documents)
                    else {
                        return ExitCode::from(1);
                    };
                    if response.get("id") == Some(&callback_id) {
                        callback_response = Some(response);
                        break;
                    }
                }
                let Some(callback_response) = callback_response else {
                    return ExitCode::from(1);
                };
                if result(
                    &mut output,
                    &message,
                    json!({"callbackResponse": callback_response}),
                    scenario,
                )
                .is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("test/duplicate-server-request-id") => {
                let callback = json!({
                    "jsonrpc": "2.0",
                    "id": "fixture-duplicate-callback",
                    "method": "workspace/configuration",
                    "params": {"items": []}
                });
                let frame = match encode_frame(&callback) {
                    Ok(frame) => frame,
                    Err(()) => return ExitCode::from(1),
                };
                for _ in 0..=64 {
                    if output.write_all(&frame).is_err() {
                        return ExitCode::from(1);
                    }
                }
                if output.flush().is_err() {
                    return ExitCode::from(1);
                }
                thread::sleep(Duration::from_secs(30));
                return ExitCode::from(1);
            }
            Some("textDocument/didOpen" | "textDocument/didClose") => {
                update_open_documents(&message, &mut open_documents);
            }
            Some("test/open-documents") => {
                if result(
                    &mut output,
                    &message,
                    json!({"count": open_documents.len(), "uris": open_documents.keys().collect::<Vec<_>>()}),
                    scenario,
                )
                .is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("test/document-text") => {
                let uri = message.pointer("/params/uri").and_then(Value::as_str).unwrap_or("");
                if result(&mut output, &message, json!(open_documents.get(uri)), scenario).is_err() {
                    return ExitCode::from(1);
                }
            }
            Some("test/publish-diagnostic") => {
                let uri = message
                    .pointer("/params/uri")
                    .and_then(Value::as_str)
                    .unwrap_or("file:///fixture.rs");
                let publication = json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": uri,
                        "diagnostics": [{
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 1}
                            },
                            "severity": 2,
                            "source": "lspctl-fixture",
                            "message": format!("diagnostic for {uri}")
                        }]
                    }
                });
                if write_frame(&mut output, &publication, scenario).is_err()
                    || result(&mut output, &message, json!({"published": uri}), scenario).is_err()
                {
                    return ExitCode::from(1);
                }
            }
            Some("test/await-file-change") => {
                let marker = message
                    .pointer("/params/marker")
                    .and_then(Value::as_str)
                    .map(std::path::Path::new);
                if marker.is_none_or(|marker| std::fs::write(marker, b"ready\n").is_err()) {
                    return ExitCode::from(1);
                }
                if let Some(release) = message.pointer("/params/releaseMarker").and_then(Value::as_str) {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !std::path::Path::new(release).is_file() {
                        if Instant::now() >= deadline {
                            return ExitCode::from(1);
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                } else {
                    let sleep_ms = message
                        .pointer("/params/sleepMs")
                        .and_then(Value::as_u64)
                        .unwrap_or(100)
                        .min(5_000);
                    thread::sleep(Duration::from_millis(sleep_ms));
                }
                if result(&mut output, &message, json!({"fixture": true}), scenario).is_err() {
                    return ExitCode::from(1);
                }
            }
            Some("test/write-gate" | "test/write-dispatch-gate") => {
                // Keep the process alive independently of stdin, including on a broken pipe.
                thread::spawn(|| {
                    thread::sleep(Duration::from_secs(10));
                    std::process::exit(1);
                });
                let marker = PathBuf::from(message["params"]["marker"].as_str().unwrap());
                let lifetime = File::create(marker.with_extension("lock")).unwrap();
                lifetime.lock().unwrap();
                std::fs::write(&marker, "ready\n").unwrap();
                while !marker.with_extension("start").exists() {
                    thread::sleep(Duration::from_millis(2));
                }
                if message["params"]["breakInput"] == true {
                    // No further stdin access follows: transfer and close its process-owned handle.
                    #[cfg(unix)]
                    {
                        use std::os::fd::{AsRawFd, FromRawFd};
                        unsafe { drop(File::from_raw_fd(input.get_ref().as_raw_fd())) };
                    }
                    #[cfg(windows)]
                    {
                        use std::os::windows::io::{AsRawHandle, FromRawHandle};
                        unsafe { drop(File::from_raw_handle(input.get_ref().as_raw_handle())) };
                    }
                    std::fs::write(marker.with_extension("stalled"), "input closed\n").unwrap();
                    while !marker.with_extension("dispatch").exists() {
                        thread::sleep(Duration::from_millis(2));
                    }
                    if result(&mut output, &message, json!({"fixture": true}), scenario).is_err() {
                        return ExitCode::from(1);
                    }
                    thread::sleep(Duration::from_secs(10));
                    return ExitCode::SUCCESS;
                }
                std::fs::write(marker.with_extension("stalled"), "not reading\n").unwrap();
                while !marker.with_extension("dispatch").exists() {
                    thread::sleep(Duration::from_millis(2));
                }
                if result(&mut output, &message, json!({"fixture": true}), scenario).is_err() {
                    return ExitCode::from(1);
                }
                while !marker.with_extension("resume").exists() {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            Some("test/notification-flood") if scenario == Scenario::NotificationFlood => {
                return notification_flood(input, &mut output, event_log, &message);
            }
            Some("test/crash") => {
                eprintln!("fixture server crashed while handling test/crash");
                return ExitCode::from(42);
            }
            Some("test/slow") => delayed = Some(message),
            Some("test/fast") => {
                if result(&mut output, &message, json!("fast"), scenario).is_err()
                    || delayed.take().is_some_and(|request| {
                        result(&mut output, &request, json!("slow"), scenario).is_err()
                    })
                {
                    return ExitCode::from(1);
                }
            }
            Some("shutdown") => {
                return if result(&mut output, &message, Value::Null, scenario).is_ok() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                };
            }
            Some(_)
                if message.get("id").is_some()
                    && result(&mut output, &message, json!({"fixture":true}), scenario)
                        .is_err() =>
            {
                return ExitCode::from(1);
            }
            _ => {}
        }
    }
}

fn notification_flood(
    mut input: BufReader<io::Stdin>,
    output: &mut impl Write,
    mut event_log: Option<File>,
    request: &Value,
) -> ExitCode {
    // Independent of stdin and stdout: even a blocked fixture write cannot leak forever.
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(10));
        std::process::exit(1);
    });
    let marker = PathBuf::from(request["params"]["marker"].as_str().unwrap());
    let lifetime = File::create(marker.with_extension("lock")).unwrap();
    lifetime.lock().unwrap();
    std::fs::write(&marker, "ready\n").unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        while let Ok(Some(message)) = read_frame(&mut input) {
            if sender.send(message).is_err() {
                break;
            }
        }
    });
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "window/logMessage",
        "params": {"type": 3, "message": "fixture notification traffic"}
    });
    let mut next_notification = Instant::now();
    let mut cancelled = false;
    loop {
        match receiver.recv_timeout(next_notification.saturating_duration_since(Instant::now())) {
            Ok(message) => {
                if message["method"] == "$/cancelRequest"
                    && message["params"]["id"] == request["id"]
                {
                    cancelled = true;
                    if let Some(log) = event_log.as_mut() {
                        writeln!(log, "$/cancelRequest").unwrap();
                        log.flush().unwrap();
                    }
                    // Deliberately ignore cancellation: the Owner must enforce its grace.
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return ExitCode::SUCCESS,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= next_notification {
            if write_frame(output, &notification, Scenario::Standard).is_err() {
                return ExitCode::from(1);
            }
            if cancelled {
                if let Some(log) = event_log.as_mut() {
                    writeln!(log, "notification-after-cancel").unwrap();
                    log.flush().unwrap();
                }
                cancelled = false;
            }
            next_notification = Instant::now() + Duration::from_millis(5);
        }
    }
}

fn related_diagnostics(
    output: &mut impl Write,
    request: &Value,
    workspace: &url::Url,
    mode: &str,
    revision: &mut usize,
    scenario: Scenario,
) -> Result<bool, ()> {
    let workspace_report = request["method"] == "workspace/diagnostic";
    let main = workspace.join("main.rs").unwrap().to_string();
    let related = workspace.join("related.rs").unwrap().to_string();
    let previous = format!("main-{revision}");
    let expected = if workspace_report {
        request["params"]["previousResultIds"]
            .as_array()
            .is_some_and(|ids| {
                ids.iter()
                    .any(|entry| entry["uri"] == main && entry["value"] == previous)
                    && ids.iter().any(|entry| {
                        entry["uri"] == related && entry["value"] == format!("related-{revision}")
                    })
            })
    } else {
        request["params"]["previousResultId"] == previous
    };
    if *revision > 0 && !expected {
        write_frame(
            output,
            &json!({
                "jsonrpc":"2.0", "id":request["id"],
                "error":{"code":-32603, "message":"fixture received stale previous diagnostic IDs", "data": request["params"]}
            }),
            scenario,
        )?;
        return Ok(false);
    }
    let full = |name: &str, id: String| {
        json!({
            "kind":"full", "resultId":id,
            "items":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}, "message":name}]
        })
    };
    let next = *revision + 1;
    let report = |name: &str| {
        if *revision == 0 {
            full(name, format!("{name}-{next}"))
        } else {
            json!({"kind":"unchanged", "resultId":format!("{name}-{next}")})
        }
    };
    let mut main_report = report("main");
    let related_report = report("related");
    let progress = |value| {
        json!({
            "jsonrpc":"2.0", "method":"$/progress",
            "params":{"token":request["params"]["partialResultToken"], "value":value}
        })
    };
    match mode {
        "malformed" => {
            result(
                output,
                request,
                json!({"kind":"full","resultId":"poison","items":17}),
                scenario,
            )?;
        }
        "raw-malformed" => {
            result(output, request, json!(17), scenario)?;
        }
        "unresolved" => {
            let mut rejected = full("poison", "poison".to_owned());
            rejected["relatedDocuments"] = json!({
                (workspace.join("uncached.rs").unwrap().as_str()): {"kind":"unchanged","resultId":"poison"}
            });
            result(output, request, rejected, scenario)?;
        }
        "error" | "cancel" => {
            write_frame(
                output,
                &progress(json!({"relatedDocuments": {
                    (related): full("poison", "poison".to_owned())
                }})),
                scenario,
            )?;
            if mode == "cancel" {
                // The existing partial-byte limit initiates cancellation, then the main loop acknowledges it.
                return Ok(true);
            }
            write_frame(
                output,
                &json!({
                    "jsonrpc":"2.0", "id":request["id"],
                    "error":{"code":-32603,"message":"fixture diagnostic failure"}
                }),
                scenario,
            )?;
        }
        _ => {
            if workspace_report {
                main_report["uri"] = json!(main);
                main_report["version"] = Value::Null;
                let mut related_report = related_report;
                related_report["uri"] = json!(related);
                related_report["version"] = Value::Null;
                write_frame(
                    output,
                    &progress(json!({"items":[related_report]})),
                    scenario,
                )?;
                result(output, request, json!({"items":[main_report]}), scenario)?;
            } else {
                let related_reports = json!({(related): related_report});
                if mode == "partial" {
                    write_frame(
                        output,
                        &progress(json!({"relatedDocuments":related_reports})),
                        scenario,
                    )?;
                } else {
                    main_report["relatedDocuments"] = related_reports;
                }
                result(output, request, main_report, scenario)?;
            }
            *revision = next;
        }
    }
    Ok(false)
}

fn update_open_documents(message: &Value, open_documents: &mut BTreeMap<String, String>) {
    let Some(uri) = message
        .pointer("/params/textDocument/uri")
        .and_then(Value::as_str)
    else {
        return;
    };
    match message.get("method").and_then(Value::as_str) {
        Some("textDocument/didOpen") => {
            if let Some(text) = message
                .pointer("/params/textDocument/text")
                .and_then(Value::as_str)
            {
                open_documents.insert(uri.to_owned(), text.to_owned());
            }
        }
        Some("textDocument/didClose") => {
            open_documents.remove(uri);
        }
        _ => {}
    }
}

fn read_callback_result<R: BufRead>(
    input: &mut R,
    open_documents: &mut BTreeMap<String, String>,
    callback_id: &Value,
) -> Option<Value> {
    loop {
        let message = read_frame(input).ok().flatten()?;
        if message.get("method").is_none() && message.get("id") == Some(callback_id) {
            return message.get("result").cloned();
        }
        update_open_documents(&message, open_documents);
    }
}

fn read_callback_message<R: BufRead>(
    input: &mut R,
    open_documents: &mut BTreeMap<String, String>,
) -> Option<Value> {
    loop {
        let message = read_frame(input).ok().flatten()?;
        if message.get("method").is_none() && message.get("id").is_some() {
            return Some(message);
        }
        update_open_documents(&message, open_documents);
    }
}

fn result(
    output: &mut impl Write,
    request: &Value,
    result: Value,
    scenario: Scenario,
) -> Result<(), ()> {
    let Some(id) = request.get("id") else {
        return Ok(());
    };
    if scenario == Scenario::Delayed {
        thread::sleep(Duration::from_millis(40));
    }
    write_frame(
        output,
        &json!({"jsonrpc":"2.0","id":id,"result":result}),
        scenario,
    )
}

fn read_frame(input: &mut impl BufRead) -> Result<Option<Value>, String> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if input
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            return if content_length.is_none() {
                Ok(None)
            } else {
                Err("truncated header".to_owned())
            };
        }
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:")
            && content_length
                .replace(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| "invalid Content-Length")?,
                )
                .is_some()
        {
            return Err("duplicate Content-Length".to_owned());
        }
    }
    let length = content_length.ok_or_else(|| "missing Content-Length".to_owned())?;
    if length > BODY_LIMIT {
        return Err("body too large".to_owned());
    }
    let mut body = vec![0; length];
    input
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&body).map_err(|error| error.to_string())
}

fn write_frame(output: &mut impl Write, message: &Value, scenario: Scenario) -> Result<(), ()> {
    let frame = encode_frame(message)?;
    for chunk in frame.chunks(if scenario == Scenario::Fragmented {
        3
    } else {
        frame.len().max(1)
    }) {
        output.write_all(chunk).map_err(|_| ())?;
        output.flush().map_err(|_| ())?;
    }
    Ok(())
}

fn write_configuration_request_burst(output: &mut impl Write, id_prefix: &str) -> Result<(), ()> {
    let mut frames = Vec::new();
    for id in 0..=64 {
        frames.extend(encode_frame(&json!({
            "jsonrpc": "2.0",
            "id": format!("{id_prefix}-{id}"),
            "method": "workspace/configuration",
            "params": {"items": [{"section": "fixture"}]}
        }))?);
    }
    output
        .write_all(&frames)
        .and_then(|()| output.flush())
        .map_err(|_| ())
}

fn encode_frame(message: &Value) -> Result<Vec<u8>, ()> {
    let body = serde_json::to_vec(message).map_err(|_| ())?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend(body);
    Ok(frame)
}
