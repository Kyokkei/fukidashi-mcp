//! Native `egui` QA editor for Fukidashi.
//!
//! This is a **separate binary** from the headless MCP server. It reads the same
//! `project.json` the loopback editor produces and writes the same `review.json`
//! that `fukidashi_wait_for_review` consumes, so the two editors are
//! interchangeable from the MCP workflow's point of view.
//!
//! Build: `cargo build --features editor --bin fukidashi-editor`

use clap::Parser;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

mod editor;

/// Exit codes mirroring the plan's contract.
pub const EXIT_OK: i32 = 0; // approved / wrote approve_export
pub const EXIT_ERROR: i32 = 1; // fatal error
pub const EXIT_CLOSED: i32 = 2; // user closed without approving

/// Shared exit decision, read after `eframe::run_native` returns.
#[derive(Default, Clone)]
struct ExitState {
    /// Set when the user approves or requests fixes via the inspector buttons.
    action: Option<editor::ExitAction>,
    error: Option<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "fukidashi-editor",
    version,
    about = "Native egui QA editor for Fukidashi comic translations"
)]
struct Cli {
    /// Job directory containing `project.json` (and a `review.json` to write).
    job_dir: PathBuf,

    /// Review session id assigned by the MCP server. When present, the editor
    /// reuses the existing `review.json` instead of minting a fresh session.
    #[arg(long)]
    review_session_id: Option<String>,

    /// Review revision paired with `--review-session-id`.
    #[arg(long)]
    review_revision: Option<u64>,

    /// Lease reserved by the MCP parent and handed off to this process.
    #[arg(long)]
    editor_lease: Option<PathBuf>,
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let cli = Cli::parse();
    let job_dir = match std::fs::canonicalize(&cli.job_dir) {
        Ok(path) => path,
        Err(error) => {
            eprintln!(
                "cannot resolve job directory {}: {error}",
                cli.job_dir.display()
            );
            return EXIT_ERROR;
        }
    };

    let _lease = match (&cli.editor_lease, &cli.review_session_id) {
        (Some(path), Some(session_id)) => {
            let revision = cli.review_revision.or_else(|| {
                fukidashi_mcp::editor::read_review(&job_dir).map(|review| review.revision)
            });
            let Some(revision) = revision else {
                eprintln!("cannot adopt native editor lease without a review revision");
                return EXIT_ERROR;
            };
            match fukidashi_mcp::editor::adopt_editor_lease(path, session_id, revision) {
                Ok(lease) => Some(lease),
                Err(error) => {
                    eprintln!("cannot adopt native editor lease: {error}");
                    return EXIT_ERROR;
                }
            }
        }
        (None, Some(session_id)) => {
            let revision = cli.review_revision.or_else(|| {
                fukidashi_mcp::editor::read_review(&job_dir).map(|review| review.revision)
            });
            match revision {
                Some(revision) => match fukidashi_mcp::editor::acquire_native_editor_lease(
                    &job_dir, session_id, revision,
                ) {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        eprintln!("cannot acquire native editor lease: {error}");
                        return EXIT_ERROR;
                    }
                },
                None => None,
            }
        }
        (None, None) => None,
        (Some(_), None) => {
            eprintln!("--editor-lease requires --review-session-id");
            return EXIT_ERROR;
        }
    };

    let exit_state = Arc::new(Mutex::new(ExitState::default()));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("Fukidashi Editor"),
        ..Default::default()
    };

    let run_result = eframe::run_native(
        "Fukidashi Editor",
        options,
        Box::new({
            let job_dir = job_dir.clone();
            let session = cli.review_session_id.clone();
            let exit_state = Arc::clone(&exit_state);
            move |cc| {
                // egui's default fonts lack Vietnamese diacritic coverage, so
                // the inspector's text widgets render `□` for `ở`, `ề`, `ắ`,
                // `ị`. Register the bundled Patrick Hand face (full Vietnamese
                // coverage) as a Proportional fallback — egui tries faces
                // left-to-right, so Latin stays on the default fonts and only
                // missing glyphs fall through to Patrick Hand.
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "PatrickHand".to_owned(),
                    std::sync::Arc::new(egui::FontData::from_static(
                        fukidashi_mcp::fonts::PATRICK_HAND_REGULAR.bytes,
                    )),
                );
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .push("PatrickHand".to_owned());
                cc.egui_ctx.set_fonts(fonts);

                match editor::EditorApp::new(
                    job_dir.clone(),
                    session.clone(),
                    Arc::clone(&exit_state),
                ) {
                    Ok(app) => Ok(Box::new(app)),
                    Err(error) => {
                        eprintln!("failed to start editor: {error}");
                        exit_state.lock().unwrap().error = Some(error.to_string());
                        Err(error.to_string().into())
                    }
                }
            }
        }),
    );

    match run_result {
        Ok(_) => {
            let state = exit_state.lock().unwrap();
            match &state.action {
                Some(editor::ExitAction::Approve) => EXIT_OK,
                Some(editor::ExitAction::RequestFixes) => EXIT_OK,
                None => {
                    if state.error.is_some() {
                        EXIT_ERROR
                    } else {
                        EXIT_CLOSED
                    }
                }
            }
        }
        Err(error) => {
            eprintln!("editor exited with error: {error}");
            EXIT_ERROR
        }
    }
}
