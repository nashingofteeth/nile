//! `nile job ls|show|cancel|submit`: the durable queue on `nile serve`.
//!
//! Unlike `nile export` (synchronous, file-based), a job is submitted to the
//! running server, which snapshots the project and renders it in the
//! background. These commands never open the project directly: with no server
//! there is no queue to talk to, so the error says to start one.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use nle_core::jobs::{ExportPreset, JobExport, JobRecord};
use nle_protocol::Client;

use crate::cmd::export::QualityPreset;
use crate::{JobCmd, say};

type Rpc = Client<BufReader<UnixStream>, UnixStream>;

fn connect(project: &Path) -> Result<Rpc> {
    let socket = nle_core::state::session_socket_path(project);
    let stream = UnixStream::connect(&socket).with_context(|| {
        format!(
            "cannot reach a running server for {} (start `nile serve`)",
            project.display()
        )
    })?;
    let reader = stream.try_clone().context("clone the server socket")?;
    Ok(Client::new(BufReader::new(reader), stream))
}

pub(crate) fn cmd_job(project: &Path, action: &JobCmd) -> Result<()> {
    let mut client = connect(project)?;
    match action {
        JobCmd::Ls => {
            let list = client.jobs().map_err(anyhow::Error::msg)?;
            if list.jobs.is_empty() {
                say!("no jobs (revision {})", list.revision);
                return Ok(());
            }
            for job in list.jobs {
                say!("{}", job_line(&job));
            }
        }
        JobCmd::Show { id } => {
            let job = client.job(*id).map_err(anyhow::Error::msg)?;
            say!("{}", serde_json::to_string_pretty(&job)?);
        }
        JobCmd::Cancel { id } => {
            let job = client.cancel_job(*id).map_err(anyhow::Error::msg)?;
            say!("cancelling {}", job.summary());
        }
        JobCmd::Submit {
            output,
            sequence,
            preset,
            crf,
            encoder_preset,
        } => {
            let hello = client.hello().map_err(anyhow::Error::msg)?;
            let sequence = sequence.unwrap_or(hello.active);
            let output = absolute_output(output)?;
            let export = JobExport {
                preset: preset_of(*preset),
                crf: *crf,
                encoder_preset: encoder_preset.clone(),
            };
            let job = client
                .submit_job(hello.revision, output.clone(), sequence, export)
                .map_err(anyhow::Error::msg)?;
            say!(
                "queued job #{} for sequence #{} → {}",
                job.id,
                job.sequence,
                job.output.display()
            );
        }
    }
    Ok(())
}

/// The server's cwd differs from the client's, so a relative output would
/// name a different file than the user meant. Resolve it here.
fn absolute_output(output: &Path) -> Result<PathBuf> {
    let output = crate::cmd::import::expand_tilde(output);
    if output.is_absolute() {
        return Ok(output);
    }
    Ok(std::env::current_dir()?.join(output))
}

fn preset_of(preset: QualityPreset) -> ExportPreset {
    match preset {
        QualityPreset::Preview => ExportPreset::Preview,
        QualityPreset::Balanced => ExportPreset::Balanced,
        QualityPreset::Max => ExportPreset::Max,
    }
}

fn job_line(job: &JobRecord) -> String {
    let mut line = format!(
        "{:<4} {:<12} seq {} {:<6}  {}",
        format!("#{}", job.id),
        job.state.label(),
        job.sequence,
        (job.progress.fraction() * 100.0).round() as u32,
        job.output.display()
    );
    if let Some(report) = &job.report {
        line.push_str(&format!("  report {}", report.display()));
    }
    if let Some(error) = &job.error {
        line.push_str(&format!("  — {error}"));
    }
    line
}
