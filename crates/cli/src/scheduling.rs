//! JSON-only access to the model-router dispatch facade.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum Command {
    /// Validate and plan a task, group, or workflow request.
    Plan(RequestFile),
    /// Submit a task, group, or workflow request.
    Submit {
        #[command(flatten)]
        input: RequestFile,
        /// Stable idempotency key; required so an uncertain submit can be queried.
        #[arg(long, required = true)]
        request_id: String,
        /// Explicitly accept possible duplicate charges for an unknown prior outcome.
        #[arg(long)]
        acknowledge_unknown_risk: bool,
    },
    /// Read the current state and detail for a dispatch.
    Status(DispatchId),
    /// Read a bounded page of dispatch results.
    Collect {
        #[command(flatten)]
        id: DispatchId,
        #[arg(long, default_value_t = 0)]
        cursor: u64,
        #[arg(long, default_value_t = 12000, value_parser = clap::value_parser!(u32).range(2000..=30000))]
        max_chars: u32,
    },
    /// Retrieve bounded slices of a full worker report.
    Report {
        #[command(flatten)]
        id: DispatchId,
        /// Zero-based worker index.
        #[arg(long, required = true)]
        worker: u32,
        /// Required by the router for workflow dispatches.
        #[arg(long)]
        node_id: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 12000, value_parser = clap::value_parser!(u32).range(2000..=30000))]
        max_chars: u32,
    },
    /// Cancel a dispatch while preserving its recorded usage and result.
    Cancel(DispatchId),
    /// List recent dispatches.
    List {
        #[arg(long, default_value_t = 0)]
        cursor: u64,
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(1..=64))]
        limit: u32,
        #[arg(long)]
        workflow_id: Option<String>,
    },
}

#[derive(Args)]
pub struct RequestFile {
    /// JSON object with request payload. Task material is never passed in argv.
    #[arg(long, required = true)]
    request_file: PathBuf,
}

#[derive(Args)]
pub struct DispatchId {
    /// Dispatch identifier returned by submit.
    #[arg(
        long,
        conflicts_with = "request_id",
        required_unless_present = "request_id"
    )]
    dispatch_id: Option<String>,
    /// Idempotency identifier used at submit time.
    #[arg(
        long,
        conflicts_with = "dispatch_id",
        required_unless_present = "dispatch_id"
    )]
    request_id: Option<String>,
}

pub fn run(command: &Command) -> Result<()> {
    let (action, params) = match command {
        Command::Plan(input) => ("plan", json!({"request": read_request(input)?})),
        Command::Submit {
            input,
            request_id,
            acknowledge_unknown_risk,
        } => {
            if request_id.trim().is_empty() {
                bail!("--request-id 不能为空");
            }
            let request = read_request(input)?;
            let mut params = json!({"request": request});
            let fields = params
                .as_object_mut()
                .context("请求文件顶层必须是 JSON 对象")?;
            fields.insert("request_id".into(), json!(request_id));
            if *acknowledge_unknown_risk {
                fields.insert("acknowledge_unknown_risk".into(), json!(true));
            }
            ("submit", params)
        }
        Command::Status(id) => ("status", id.params()),
        Command::Collect {
            id,
            cursor,
            max_chars,
        } => {
            let mut params = id.params();
            params["cursor"] = json!(cursor);
            params["max_chars"] = json!(max_chars);
            ("collect", params)
        }
        Command::Report {
            id,
            worker,
            node_id,
            offset,
            max_chars,
        } => {
            let mut params = id.params();
            params["worker"] = json!(worker);
            if let Some(node_id) = node_id {
                if node_id.trim().is_empty() {
                    bail!("--node-id 不能为空");
                }
                params["node_id"] = json!(node_id);
            }
            params["offset"] = json!(offset);
            params["max_chars"] = json!(max_chars);
            ("report", params)
        }
        Command::Cancel(id) => ("cancel", id.params()),
        Command::List {
            cursor,
            limit,
            workflow_id,
        } => {
            let mut params = json!({"cursor": cursor, "limit": limit});
            if let Some(workflow_id) = workflow_id {
                if workflow_id.trim().is_empty() {
                    bail!("--workflow-id 不能为空");
                }
                params["workflow_id"] = json!(workflow_id);
            }
            ("list", params)
        }
    };
    let result = globaltokentracker_core::scheduler::dispatch(action, params)?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn read_request(input: &RequestFile) -> Result<Value> {
    let data = std::fs::read(&input.request_file)
        .with_context(|| format!("无法读取请求文件 {}", input.request_file.display()))?;
    let value: Value = serde_json::from_slice(&data)
        .with_context(|| format!("请求文件 {} 不是有效 JSON", input.request_file.display()))?;
    if !value.is_object() {
        bail!("请求文件顶层必须是 JSON 对象");
    }
    validate_dispatch_request(&value)?;
    Ok(value)
}

fn validate_dispatch_request(value: &Value) -> Result<()> {
    let valid = match value["kind"].as_str() {
        Some("task") => value["task"].is_object(),
        Some("group") => value["request"].is_object(),
        Some("workflow") => {
            (value["definition"].is_object()
                || value["saved_workflow_id"]
                    .as_str()
                    .is_some_and(|id| !id.trim().is_empty()))
                && value.get("input_text").is_none_or(Value::is_string)
                && value.get("workflow_id").is_none_or(Value::is_string)
        }
        _ => false,
    };
    if !valid {
        bail!("请求必须是 task、group 或 workflow 类型，并包含对应的 task/request/definition 字段");
    }
    Ok(())
}

impl DispatchId {
    fn params(&self) -> Value {
        if let Some(id) = &self.dispatch_id {
            json!({"dispatch_id": id})
        } else {
            json!({"request_id": self.request_id.as_deref().unwrap_or_default()})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: Command,
    }

    #[test]
    fn help_exposes_all_dispatch_actions_and_safe_request_file_input() {
        let mut help = Vec::new();
        TestCli::command().write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        for expected in [
            "plan", "submit", "status", "collect", "report", "cancel", "list",
        ] {
            assert!(
                help.contains(expected),
                "missing {expected} in help: {help}"
            );
        }
        let mut plan_help = Vec::new();
        TestCli::command()
            .find_subcommand_mut("plan")
            .unwrap()
            .write_long_help(&mut plan_help)
            .unwrap();
        assert!(
            String::from_utf8(plan_help)
                .unwrap()
                .contains("--request-file")
        );
    }

    #[test]
    fn submit_requires_explicit_id_and_dispatch_queries_require_one_identifier() {
        assert!(
            TestCli::try_parse_from(["gtt", "submit", "--request-file", "request.json"]).is_err()
        );
        assert!(TestCli::try_parse_from(["gtt", "status"]).is_err());
        assert!(
            TestCli::try_parse_from(["gtt", "status", "--dispatch-id", "d1", "--request-id", "r1"])
                .is_err()
        );
        assert!(TestCli::try_parse_from(["gtt", "status", "--request-id", "r1"]).is_ok());
        assert!(
            TestCli::try_parse_from([
                "gtt",
                "collect",
                "--dispatch-id",
                "d1",
                "--max-chars",
                "1999"
            ])
            .is_err()
        );
    }

    #[test]
    fn dispatch_request_validation_accepts_the_three_supported_shapes() {
        for request in [
            json!({"kind":"task","task":{"kind":"extract"}}),
            json!({"kind":"group","request":{"topic":"review"}}),
            json!({"kind":"workflow","saved_workflow_id":"wf-1","input_text":"sample"}),
        ] {
            validate_dispatch_request(&request).unwrap();
        }
        for request in [
            json!({}),
            json!({"kind":"task"}),
            json!({"kind":"workflow","definition":null}),
        ] {
            assert!(validate_dispatch_request(&request).is_err());
        }
    }
}
