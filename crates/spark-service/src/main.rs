use spark_service::{Service, ServiceOptions};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut options = ServiceOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--data-dir" => {
                options.data_dir = PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--data-dir requires a path"))?,
                )
            }
            "--ui-dir" => {
                options.ui_dir = PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--ui-dir requires a path"))?,
                )
            }
            "--gateway-port" => {
                options.gateway_port = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--gateway-port requires a port"))?
                    .parse()?
            }
            "--headless" => {}
            "--help" | "-h" => {
                eprintln!(
                    "spark-service --data-dir PATH --ui-dir assets --gateway-port 0 [--headless]"
                );
                return Ok(());
            }
            _ => anyhow::bail!("Unknown argument: {argument}"),
        }
    }
    let service = Service::start(options).await?;
    println!(
        "{}",
        serde_json::json!({"kind":"ready","url":service.url,"token":service.token})
    );
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {result=tokio::signal::ctrl_c()=>result?,_=terminate.recv()=>{}}
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    service.shutdown().await;
    Ok(())
}
