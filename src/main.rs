use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use std::path::PathBuf;
#[cfg(feature = "onnx")]
use std::{fs, time::Instant};

#[cfg(feature = "onnx")]
use fukidashi_mcp::vision::ocr::{PageWorkerMode, analyze_pages_concurrent};
use fukidashi_mcp::{
    config::{Config, ConfigArgs, ConfigureRequest},
    installer::{self, Client},
    legacy::{migrate_managed_fonts, recover_typeset_payloads},
    mcp::FukidashiServer,
    workflow::Workflow,
};

#[derive(Debug, Parser)]
#[command(
    name = "fukidashi-mcp",
    version,
    about = "Local comic analysis and typesetting over MCP"
)]
struct Cli {
    #[command(flatten)]
    config: ConfigArgs,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Debug, Subcommand)]
enum Command {
    /// Install the native MCP engine and configure detected clients.
    #[command(alias = "setup")]
    Install {
        /// One or more clients to configure. Omit to auto-detect.
        #[arg(long, value_enum, value_delimiter = ',')]
        client: Vec<Client>,
        /// Configure every supported client, even when it is not detected.
        #[arg(long)]
        all: bool,
    },
    /// Remove Fukidashi entries and owned installation files.
    Uninstall,
    /// Restore pre-install client configuration backups when unchanged.
    Rollback,
    /// Report the native engine and every supported client adapter.
    Status,
    Doctor,
    /// Print effective paths and provider, including the source of each value.
    ConfigShow,
    /// Persist user paths/preferences. Restart the MCP after changing them.
    ConfigSet {
        #[arg(long)]
        storage_root: Option<String>,
        #[arg(long)]
        models_dir: Option<String>,
        #[arg(long)]
        jobs_dir: Option<String>,
        #[arg(long)]
        cache_dir: Option<String>,
        #[arg(long)]
        temp_dir: Option<String>,
        #[arg(long)]
        runtime_dir: Option<String>,
        #[arg(long)]
        exports_dir: Option<String>,
        #[arg(long, value_delimiter = ',')]
        font_dirs: Vec<String>,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Explicitly recover saved typeset payloads from one Claude JSONL file.
    LegacyRecover {
        #[arg(long)]
        job: PathBuf,
        #[arg(long)]
        transcript: PathBuf,
        /// Apply the recovered bubbles atomically; omission performs a dry run.
        #[arg(long)]
        apply: bool,
    },
    /// Copy approved external fonts into a managed job and rewrite its state.
    FontMigrate {
        #[arg(long)]
        job: PathBuf,
        /// Apply atomically; omission performs a dry run.
        #[arg(long)]
        apply: bool,
    },
    #[cfg(feature = "onnx")]
    BenchmarkPages {
        #[arg(long)]
        fixture: PathBuf,
        #[arg(long, default_value = "cpu-gpu")]
        mode: String,
        /// Include complete OCR regions and stable translation IDs.
        #[arg(long)]
        full: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::resolve(&cli.config)?;
    config.ensure_runtime_dirs()?;
    if let Some(Command::Install { client, all }) = cli.command.as_ref() {
        println!(
            "{}",
            serde_json::to_string_pretty(&installer::install(client, *all)?)?
        );
        return Ok(());
    }
    if matches!(cli.command, Some(Command::Uninstall)) {
        println!(
            "{}",
            serde_json::to_string_pretty(&installer::uninstall()?)?
        );
        return Ok(());
    }
    if matches!(cli.command, Some(Command::Rollback)) {
        println!("{}", serde_json::to_string_pretty(&installer::rollback()?)?);
        return Ok(());
    }
    if matches!(cli.command, Some(Command::Status)) {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "config": config.config_report(),
                "installer": installer::installation_status()?,
            }))?
        );
        return Ok(());
    }
    if matches!(cli.command, Some(Command::Doctor)) {
        for line in config.doctor() {
            eprintln!("{line}");
        }
        for line in installer::doctor_lines()? {
            eprintln!("{line}");
        }
        return Ok(());
    }
    if matches!(cli.command, Some(Command::ConfigShow)) {
        println!("{}", serde_json::to_string_pretty(&config.config_report())?);
        return Ok(());
    }
    if let Some(Command::ConfigSet {
        storage_root,
        models_dir,
        jobs_dir,
        cache_dir,
        temp_dir,
        runtime_dir,
        exports_dir,
        font_dirs,
        provider,
    }) = cli.command.as_ref()
    {
        let report = config.configure_user(&ConfigureRequest {
            storage_root: storage_root.clone(),
            models_dir: models_dir.clone(),
            jobs_dir: jobs_dir.clone(),
            cache_dir: cache_dir.clone(),
            temp_dir: temp_dir.clone(),
            runtime_dir: runtime_dir.clone(),
            exports_dir: exports_dir.clone(),
            font_dirs: font_dirs.clone(),
            provider: provider.clone(),
        })?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if let Some(Command::LegacyRecover {
        job,
        transcript,
        apply,
    }) = cli.command.as_ref()
    {
        let workflow = Workflow::new(config.jobs_dir())?;
        let report =
            recover_typeset_payloads(&workflow, &job.to_string_lossy(), transcript, *apply)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if let Some(Command::FontMigrate { job, apply }) = cli.command.as_ref() {
        let workflow = Workflow::new(config.jobs_dir())?;
        let report = migrate_managed_fonts(&workflow, &job.to_string_lossy(), *apply)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    #[cfg(feature = "onnx")]
    if let Some(Command::BenchmarkPages {
        fixture,
        mode,
        full,
    }) = cli.command
    {
        let worker_mode = match mode.as_str() {
            "cpu-only" => PageWorkerMode::CpuOnly,
            "gpu-only" => PageWorkerMode::GpuOnly,
            "cpu-gpu" => PageWorkerMode::CpuAndGpu,
            other => anyhow::bail!("unknown benchmark mode {other:?}"),
        };
        let mut paths = fs::read_dir(&fixture)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|extension| {
                    matches!(
                        extension
                            .to_str()
                            .unwrap_or_default()
                            .to_ascii_lowercase()
                            .as_str(),
                        "png" | "jpg" | "jpeg" | "webp"
                    )
                })
            })
            .collect::<Vec<_>>();
        paths.sort();
        let started = Instant::now();
        let results = analyze_pages_concurrent(&config, &paths, worker_mode)?;
        let pages = results
            .iter()
            .map(|page| {
                let mut value = serde_json::json!({
                    "page_index": page.page_index,
                    "image_path": page.image_path,
                    "worker": page.worker,
                    "provider": page.provider,
                    "bubble_count": page.analysis.bubbles.len(),
                    "text_line_count": page.analysis.text_lines.len(),
                    "recognizers": page.analysis.bubbles.iter().map(|bubble| bubble.recognizer.clone()).collect::<std::collections::BTreeSet<_>>(),
                    "source_language": page.analysis.source_language,
                });
                if full {
                    value["analysis"] = serde_json::to_value(&page.analysis).unwrap_or_default();
                }
                value
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::json!({
                "fixture": fixture,
                "mode": mode,
                "elapsed_seconds": started.elapsed().as_secs_f64(),
                "page_count": pages.len(),
                "pages": pages,
            })
        );
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    FukidashiServer::new(config)?
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
