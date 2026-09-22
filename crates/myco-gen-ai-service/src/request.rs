use std::collections::HashSet;

use serde_json::Value;

use crate::{Error, Message, Output, Protocol, Request, Response, ToolCall};

pub(crate) fn validate(request: &Request, protocol: Protocol) -> Result<(), Error> {
    if request.model.is_empty() || request.messages.is_empty() || request.max_output_tokens == 0 {
        return Err(Error::InvalidRequest(
            "model, messages, and a positive output limit are required".into(),
        ));
    }
    validate_tools(request)?;
    validate_history(&request.messages, protocol)
}

fn validate_tools(request: &Request) -> Result<(), Error> {
    let mut names = HashSet::new();
    for tool in &request.tools {
        if tool.name.is_empty() || !names.insert(&tool.name) || !tool.parameters.is_object() {
            return Err(Error::InvalidRequest(
                "tools require unique nonempty names and object schemas".into(),
            ));
        }
    }
    Ok(())
}

fn validate_history(messages: &[Message], protocol: Protocol) -> Result<(), Error> {
    let mut history = History::default();
    for message in messages {
        history.message(message, protocol)?;
    }
    if history.calls.len() != history.results.len() {
        return Err(Error::InvalidRequest(
            "history has tool calls without results".into(),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct History<'a> {
    calls: HashSet<&'a str>,
    results: HashSet<&'a str>,
}

impl<'a> History<'a> {
    fn message(&mut self, message: &'a Message, protocol: Protocol) -> Result<(), Error> {
        match message {
            Message::Assistant(response) => self.assistant(response, protocol),
            Message::ToolResult { call_id, .. } => self.result(call_id),
            Message::User(_) => Ok(()),
        }
    }

    fn assistant(&mut self, response: &'a Response, protocol: Protocol) -> Result<(), Error> {
        if response.provider().is_some_and(|p| p.protocol != protocol) {
            return Err(Error::InvalidRequest(
                "assistant continuation belongs to another provider protocol".into(),
            ));
        }
        for output in response.output() {
            if let Output::ToolCall(call) = output {
                self.call(call)?;
            }
        }
        Ok(())
    }

    fn call(&mut self, call: &'a ToolCall) -> Result<(), Error> {
        call.validate()
            .map_err(|e| Error::InvalidRequest(e.to_string()))?;
        if !self.calls.insert(&call.id) {
            return Err(Error::InvalidRequest(format!(
                "duplicate tool call ID {}",
                call.id
            )));
        }
        Ok(())
    }

    fn result(&mut self, id: &'a str) -> Result<(), Error> {
        if !self.calls.contains(id) || !self.results.insert(id) {
            return Err(Error::InvalidRequest(format!(
                "unmatched or duplicate tool result {id}"
            )));
        }
        Ok(())
    }
}

pub(crate) fn apply_options(body: &mut Value, request: &Request) -> Result<(), Error> {
    for (name, value) in &request.provider_options {
        if managed_field(name) {
            return Err(Error::InvalidRequest(format!(
                "provider option {name} overrides a managed request field"
            )));
        }
        body[name] = value.clone();
    }
    Ok(())
}

fn managed_field(name: &str) -> bool {
    matches!(
        name,
        "model"
            | "input"
            | "messages"
            | "instructions"
            | "system"
            | "tools"
            | "max_tokens"
            | "max_output_tokens"
            | "stream"
            | "store"
            | "include"
            | "previous_response_id"
            | "conversation"
            | "background"
    )
}
