//! Explicit one-shot service attachment never opens a local runner or writer lock.

use std::future::Future;
use std::io::Write;
use std::time::Duration;

use myco::generative_model::Content;
use uuid::Uuid;

use super::{Args, print, service_client::Service};
use crate::service_protocol::{Output, Submit};

pub(crate) async fn run_service(args: Args) -> u8 {
    match run(args).await {
        Ok(code) => code,
        Err((code, error)) => {
            eprintln!("myco: {error}");
            code
        }
    }
}

async fn run(args: Args) -> Result<u8, (u8, String)> {
    if args.print.is_none() && args.observe.is_none() {
        return Err((
            2,
            "--server requires -p or --observe INSTANCE:REQUEST.".into(),
        ));
    }
    let argument = args.print.as_ref().and_then(Option::as_deref);
    let prompt = args
        .print
        .as_ref()
        .map(|_| print::read_prompt(argument))
        .transpose()
        .map_err(|e| (2, e))?;
    let id = args.resume.as_deref().expect("clap requires resume");
    let service = Service::connect(args.server.as_deref().unwrap(), id)
        .await
        .map_err(|e| (1, e))?;
    let request = if let Some(token) = &args.observe {
        reconnect(token, service.identity.instance).map_err(|e| (1, e))?
    } else {
        Uuid::new_v4()
    };
    eprintln!(
        "session={id}\nrun={}:{}\nworkspace={}",
        service.identity.instance, request, service.identity.workspace
    );
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    let mut cancelling = false;
    if args.observe.is_none() {
        let content = print::print_content(
            argument,
            prompt.unwrap(),
            service.image_limit().await.map_err(|e| (1, e))?,
        )
        .map_err(|e| (2, e))?;
        let mut input = Submit {
            instance: service.identity.instance,
            request_id: request,
            text: String::new(),
            images: vec![],
        };
        for part in content {
            match part {
                Content::Text { text } => input.text = text,
                Content::Image { source } => input.images.push(source),
                _ => {}
            }
        }
        let acceptance = service.submit(&input);
        tokio::pin!(acceptance);
        tokio::select! {
            result = &mut acceptance => result.map_err(|e| (1, e))?,
            result = &mut signal => {
                result.map_err(|e| (1, format!("listen for cancellation: {e}")))?;
                // Resolve the in-flight POST before cancelling, so a late acceptance
                // cannot start work after a successful cancellation response.
                let result = acceptance.await;
                service.cancel(request).await.map_err(|e| (1, e))?;
                result.map_err(|e| (1, e))?;
                cancelling = true;
            }
        }
        if args.detach && !cancelling {
            return Ok(0);
        }
    }
    observe(&service, request, cancelling, signal)
        .await
        .map_err(|e| (1, e))
}

fn reconnect(token: &str, instance: Uuid) -> Result<Uuid, String> {
    let (expected, request) = token
        .split_once(':')
        .ok_or("--observe needs INSTANCE:REQUEST from the service run announcement.")?;
    let expected = Uuid::parse_str(expected).map_err(|e| format!("Invalid instance: {e}"))?;
    let request = Uuid::parse_str(request).map_err(|e| format!("Invalid request: {e}"))?;
    if expected != instance {
        return Err("The service instance changed. The previous request's outcome is unknown; inspect the saved session before submitting new work. No request was replayed.".into());
    }
    Ok(request)
}

#[derive(Default)]
struct Cursor {
    offset: usize,
    revision: u64,
    last_byte: Option<u8>,
}

impl Cursor {
    fn write(&mut self, output: &Output, service: &Service, request: Uuid) -> Result<(), String> {
        if output.instance != service.identity.instance
            || output.request_id != request
            || output.offset != self.offset
            || output.revision < self.revision
            || output.offset.checked_add(output.output.len()) != Some(output.next_offset)
        {
            return Err(
                "Service output identity or ordering changed; reconnect with the printed token."
                    .into(),
            );
        }
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(output.output.as_bytes())
            .and_then(|_| stdout.flush())
            .map_err(|e| format!("write stdout: {e}"))?;
        self.offset = output.next_offset;
        self.revision = output.revision;
        self.last_byte = output.output.as_bytes().last().copied().or(self.last_byte);
        Ok(())
    }

    fn finish(&self) -> Result<(), String> {
        if self.last_byte.is_some_and(|byte| byte != b'\n') {
            std::io::stdout()
                .write_all(b"\n")
                .map_err(|e| format!("write stdout: {e}"))?;
        }
        Ok(())
    }
}

async fn observe(
    service: &Service,
    request: Uuid,
    mut cancelling: bool,
    signal: impl Future<Output = std::io::Result<()>>,
) -> Result<u8, String> {
    let mut cursor = Cursor::default();
    tokio::pin!(signal);
    let mut delay = false;
    loop {
        let output = tokio::select! {
            result = async {
                if delay { tokio::time::sleep(Duration::from_millis(100)).await; }
                service.output(request, cursor.offset).await
            } => result?,
            result = &mut signal, if !cancelling => {
                result.map_err(|e| format!("listen for cancellation: {e}"))?;
                service.cancel(request).await?;
                cancelling = true;
                continue;
            }
        };
        if let Err(error) = cursor.write(&output, service, request) {
            let cancelled = service.cancel(request).await;
            return Err(format!(
                "{error}{}",
                cancelled
                    .err()
                    .map(|e| format!("; {e}"))
                    .unwrap_or_default()
            ));
        }
        if let Some(code) = output.exit_code {
            cursor.finish()?;
            if let Some(error) = output.error {
                eprintln!("myco: {error}");
            }
            return Ok(if cancelling { 130 } else { code });
        }
        delay = output.output.is_empty();
    }
}
