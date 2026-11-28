//! `nile job ls|show|cancel|rm|prune|submit|batch`: the durable queue on
//! `nile serve`.
//!
//! Unlike `nile export` (synchronous, file-based), a job is submitted to the
//! running server, which snapshots the project and renders it in the
//! background. These commands never open the project directly: with no server
//! there is no queue to talk to, so the error says to start one. `batch` is
//! only a client-side loop over `job.submit`: no queue or wire change.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

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
        JobCmd::Rm { id } => {
            let job = client.remove_job(*id).map_err(anyhow::Error::msg)?;
            say!("removed {}", job.summary());
        }
        JobCmd::Prune { keep, older_than } => {
            let pruned = client
                .prune_jobs(*keep, *older_than)
                .map_err(anyhow::Error::msg)?;
            if pruned.removed.is_empty() && pruned.failed.is_empty() {
                say!("nothing to prune");
            } else if !pruned.removed.is_empty() {
                let ids: Vec<String> = pruned.removed.iter().map(|id| format!("#{id}")).collect();
                say!("pruned {} job(s): {}", pruned.removed.len(), ids.join(" "));
            }
            if !pruned.failed.is_empty() {
                for failure in &pruned.failed {
                    say!("could not prune #{}: {}", failure.id, failure.error);
                }
                anyhow::bail!("{} job(s) could not be pruned", pruned.failed.len());
            }
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
            let output = absolute_path(output)?;
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
        JobCmd::Batch {
            output_dir,
            sequence,
            preset,
            crf,
            encoder_preset,
        } => {
            // Read the revision once: `job.submit` is not a model write, so
            // every submit in the batch uses the same CAS token. A concurrent
            // edit makes a later submit refuse instead of mixing revisions.
            let hello = client.hello().map_err(anyhow::Error::msg)?;
            let listed = client.sequences().map_err(anyhow::Error::msg)?;
            let available: Vec<u32> = listed.sequences.iter().map(|item| item.id).collect();
            let picked = select_sequences(&available, sequence)?;
            if picked.is_empty() {
                bail!("no sequences to render");
            }
            let dir = absolute_path(output_dir)?;
            // Refuse before `create_dir_all`: a bad selection or an existing
            // file must not leave an empty directory (or half a batch) behind.
            let plan = plan_outputs(&dir, &picked)?;
            let created = !dir.exists();
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("create the output directory {}", dir.display()))?;
            let export = JobExport {
                preset: preset_of(*preset),
                crf: *crf,
                encoder_preset: encoder_preset.clone(),
            };
            let mut queued: Vec<u64> = Vec::new();
            for (sequence, output) in plan {
                match client.submit_job(hello.revision, output, sequence, export.clone()) {
                    Ok(job) => {
                        say!(
                            "queued job #{} for sequence #{} → {}",
                            job.id,
                            job.sequence,
                            job.output.display()
                        );
                        queued.push(job.id);
                    }
                    Err(error) => {
                        // The server may refuse a path a live job owns even
                        // though our `exists()` plan passed. If nothing was
                        // queued, do not leave the directory this batch made.
                        if queued.is_empty() {
                            discard_empty_batch_dir(&dir, created);
                        }
                        let error = anyhow::Error::msg(error)
                            .context(format!("submit sequence #{sequence}"));
                        return Err(if queued.is_empty() {
                            error
                        } else {
                            error.context(format!(
                                "queued {} job(s) before the batch failed: {}",
                                queued.len(),
                                id_list(&queued)
                            ))
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// Resolve a batch's sequence ids. Explicit ids keep the command-line order,
/// so job ids follow the user's intent; no ids means the server's listing
/// order. Duplicates and unknown ids are refused here, before any submit or
/// output-directory side effect.
fn select_sequences(available: &[u32], chosen: &[u32]) -> Result<Vec<u32>> {
    if chosen.is_empty() {
        return Ok(available.to_vec());
    }
    let mut picked = Vec::with_capacity(chosen.len());
    for &id in chosen {
        if picked.contains(&id) {
            bail!("sequence #{id} was given more than once");
        }
        if !available.contains(&id) {
            bail!(
                "no sequence #{id} in this project (have: {})",
                id_list(available)
            );
        }
        picked.push(id);
    }
    Ok(picked)
}

/// One deterministic name per sequence. Ids are unique and stable; display
/// names are not (spaces, slashes, renames), so this never needs sanitizing.
fn batch_name(sequence: u32) -> String {
    format!("sequence-{sequence}.mp4")
}

/// Resolve every target before the first submit. Checking all of them up front
/// means an existing file refuses the whole batch rather than a half-queued
/// one.
fn plan_outputs(dir: &Path, sequences: &[u32]) -> Result<Vec<(u32, PathBuf)>> {
    let mut plan = Vec::with_capacity(sequences.len());
    for &sequence in sequences {
        let output = dir.join(batch_name(sequence));
        if output.exists() {
            bail!(
                "{} already exists; the batch refuses to overwrite a render",
                output.display()
            );
        }
        plan.push((sequence, output));
    }
    Ok(plan)
}

/// Drop the directory a failed batch created, but only while it is still
/// empty: `remove_dir` refuses a non-empty tree, which is the guard we want if
/// another writer dropped a file in meanwhile. The submit error is the one to
/// report, so a cleanup failure is ignored.
fn discard_empty_batch_dir(dir: &Path, created: bool) {
    if created {
        let _ = std::fs::remove_dir(dir);
    }
}

fn id_list<T: std::fmt::Display>(ids: &[T]) -> String {
    if ids.is_empty() {
        return "none".to_string();
    }
    ids.iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The server's cwd differs from the client's, so a relative path would name
/// a different file (or batch directory) than the user meant. Resolve it here.
fn absolute_path(output: &Path) -> Result<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_selection_defaults_to_the_listing_order() {
        assert_eq!(
            select_sequences(&[3, 1, 2], &[]).expect("default"),
            [3, 1, 2]
        );
    }

    #[test]
    fn batch_selection_keeps_the_requested_order() {
        assert_eq!(
            select_sequences(&[1, 2, 3], &[3, 1]).expect("explicit"),
            [3, 1]
        );
    }

    #[test]
    fn batch_selection_refuses_duplicates_and_unknown_ids() {
        let duplicate = select_sequences(&[1, 2], &[2, 2])
            .expect_err("duplicate")
            .to_string();
        assert!(
            duplicate.contains("sequence #2") && duplicate.contains("more than once"),
            "{duplicate}"
        );

        let unknown = select_sequences(&[1, 2], &[9])
            .expect_err("unknown")
            .to_string();
        assert!(unknown.contains("no sequence #9"), "{unknown}");
        assert!(unknown.contains("#1, #2"), "{unknown}");
    }

    #[test]
    fn batch_plan_names_outputs_in_selection_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = plan_outputs(dir.path(), &[4, 2]).expect("plan");
        assert_eq!(
            plan,
            vec![
                (4, dir.path().join("sequence-4.mp4")),
                (2, dir.path().join("sequence-2.mp4")),
            ]
        );
    }

    #[test]
    fn batch_plan_refuses_an_existing_output_without_writing() {
        let parent = tempfile::tempdir().expect("tempdir");
        let dir = parent.path().join("renders");
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::write(dir.join("sequence-2.mp4"), b"an old render").expect("write");

        let error = plan_outputs(&dir, &[1, 2])
            .expect_err("existing file")
            .to_string();
        assert!(error.contains("sequence-2.mp4"), "{error}");
        assert_eq!(
            std::fs::read(dir.join("sequence-2.mp4")).expect("read"),
            b"an old render",
            "the existing file is untouched"
        );
        assert!(
            !dir.join("sequence-1.mp4").exists(),
            "the plan writes nothing"
        );
    }

    #[test]
    fn failed_batch_cleanup_removes_only_the_empty_directory_it_created() {
        let parent = tempfile::tempdir().expect("tempdir");

        let created = parent.path().join("created");
        std::fs::create_dir(&created).expect("mkdir");
        discard_empty_batch_dir(&created, true);
        assert!(!created.exists(), "the batch's own empty directory goes");

        let foreign = parent.path().join("foreign");
        std::fs::create_dir(&foreign).expect("mkdir");
        discard_empty_batch_dir(&foreign, false);
        assert!(
            foreign.exists(),
            "a directory the batch did not create stays"
        );

        let occupied = parent.path().join("occupied");
        std::fs::create_dir(&occupied).expect("mkdir");
        std::fs::write(occupied.join("sequence-1.mp4"), b"a render").expect("write");
        discard_empty_batch_dir(&occupied, true);
        assert!(
            occupied.join("sequence-1.mp4").exists(),
            "a non-empty directory is left alone"
        );
    }
}
