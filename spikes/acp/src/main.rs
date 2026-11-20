//! Phase 0 spike: a generic ACP client that runs one prompt against any ACP
//! server launched as a subprocess. Permission requests are denied unless
//! `--yolo` is passed, so the trust decision is explicit.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PermissionOptionKind,
    PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, Client};

struct Args {
    command: String,
    prompt: String,
    cwd: PathBuf,
    yolo: bool,
    timeout: Duration,
    cancel_after: Option<Duration>,
    model: Option<String>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut command = None;
    let mut prompt = None;
    let mut cwd = std::env::current_dir()?;
    let mut yolo = false;
    let mut timeout = Duration::from_secs(120);
    let mut cancel_after = None;
    let mut model = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--command" => command = it.next(),
            "--cwd" => cwd = PathBuf::from(it.next().ok_or_else(|| anyhow::anyhow!("--cwd"))?),
            "--yolo" => yolo = true,
            "--timeout" => {
                let secs: u64 = it.next().ok_or_else(|| anyhow::anyhow!("--timeout"))?.parse()?;
                timeout = Duration::from_secs(secs);
            }
            "--model" => model = it.next(),
            "--cancel-after" => {
                let secs: u64 = it.next().ok_or_else(|| anyhow::anyhow!("--cancel-after"))?.parse()?;
                cancel_after = Some(Duration::from_secs(secs));
            }
            _ => prompt = Some(arg),
        }
    }
    Ok(Args {
        command: command.ok_or_else(|| anyhow::anyhow!("--command is required"))?,
        prompt: prompt.ok_or_else(|| anyhow::anyhow!("a prompt is required"))?,
        cwd,
        yolo,
        timeout,
        cancel_after,
        model,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let agent = AcpAgent::from_str(&args.command)?;
    let yolo = args.yolo;
    let cwd = args.cwd.clone();
    let prompt = args.prompt.clone();
    let cancel_after = args.cancel_after;
    let model = args.model.clone();

    let run = Client
        .v1()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                eprintln!("[update] {:?}", notification.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                eprintln!("[permission] {:?}", request.tool_call);
                let pick = |kinds: &[PermissionOptionKind]| {
                    kinds.iter().find_map(|kind| {
                        request
                            .options
                            .iter()
                            .find(|option| option.kind == *kind)
                            .map(|option| option.option_id.clone())
                    })
                };
                let choice = if yolo {
                    pick(&[PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways])
                } else {
                    None
                };
                match choice {
                    Some(id) => {
                        eprintln!("[permission] approved (--yolo)");
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                        ))
                    }
                    None => {
                        eprintln!("[permission] denied (fail closed)");
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            let init = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            eprintln!("[init] agent_info={:?}", init.agent_info);
            eprintln!("[init] capabilities={:?}", init.agent_capabilities);

            let session = connection
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await?;
            eprintln!("[session] id={:?}", session.session_id);
            if let Some(wanted) = &model {
                use agent_client_protocol::v1::schema::{
                    SessionConfigKind, SessionConfigOptionCategory, SessionConfigSelectOptions,
                    SessionConfigOptionValue, SetSessionConfigOptionRequest,
                };
                let option = session.config_options.as_ref().and_then(|options| {
                    options.iter().find(|option| {
                        option.category == Some(SessionConfigOptionCategory::Model)
                            || option.id.0.as_ref() == "model"
                    })
                });
                match option {
                    None => eprintln!("[model] agent exposes no model config option"),
                    Some(option) => {
                        let SessionConfigKind::Select(select) = &option.kind else {
                            eprintln!("[model] model option is not a select");
                            return Ok(());
                        };
                        let mut available: Vec<String> = Vec::new();
                        match &select.options {
                            SessionConfigSelectOptions::Ungrouped(list) => {
                                available.extend(list.iter().map(|o| o.value.0.to_string()))
                            }
                            SessionConfigSelectOptions::Grouped(groups) => available.extend(
                                groups
                                    .iter()
                                    .flat_map(|g| g.options.iter().map(|o| o.value.0.to_string())),
                            ),
                            _ => {}
                        }
                        if !available.iter().any(|value| value == wanted) {
                            eprintln!("[model] '{wanted}' not offered; available: {available:?}");
                            return Ok(());
                        }
                        let response = connection
                            .send_request(SetSessionConfigOptionRequest::new(
                                session.session_id.clone(),
                                option.id.clone(),
                                SessionConfigOptionValue::value_id(wanted.clone()),
                            ))
                            .block_task()
                            .await?;
                        eprintln!("[model] set {wanted}: {:?}", response.config_options);
                    }
                }
            }

            let session_id = session.session_id;
            let turn = connection
                .send_request(PromptRequest::new(
                    session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new(prompt))],
                ))
                .block_task();
            let response = match cancel_after {
                None => turn.await?,
                Some(delay) => {
                    tokio::pin!(turn);
                    tokio::select! {
                        result = &mut turn => result?,
                        _ = tokio::time::sleep(delay) => {
                            eprintln!("[cancel] sending session/cancel after {delay:?}");
                            connection.send_notification(CancelNotification::new(session_id.clone()))?;
                            turn.await?
                        }
                    }
                }
            };
            eprintln!("[done] stop_reason={:?}", response.stop_reason);
            Ok(())
        });

    match tokio::time::timeout(args.timeout, run).await {
        Ok(result) => result?,
        Err(_) => anyhow::bail!("timed out after {:?}", args.timeout),
    }
    Ok(())
}
