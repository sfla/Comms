use std::error::Error;
use std::fmt::Formatter;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::num::ParseIntError;
use std::path::PathBuf;

use crate::ui::BUILD_TIMESTAMP;
use chrono::{DateTime, SubsecRound};
use directories::ProjectDirs;
use gtk::prelude::*;
use gtk::{Button, Label};
use ureq::http::header::ToStrError;

#[derive(Debug)]
enum CheckForUpdateError {
    Ureq(Box<ureq::Error>),
    Parse,
    NoWarpWrapper,
}

enum DownloadProgress {
    Partial(f64),
    Done(Vec<u8>),
}

impl From<ureq::Error> for CheckForUpdateError {
    fn from(e: ureq::Error) -> Self {
        Self::Ureq(Box::new(e))
    }
}

impl From<io::Error> for CheckForUpdateError {
    fn from(e: io::Error) -> Self {
        Self::Ureq(Box::new(e.into()))
    }
}

impl From<ParseIntError> for CheckForUpdateError {
    fn from(_: ParseIntError) -> Self {
        Self::Parse
    }
}

impl From<ToStrError> for CheckForUpdateError {
    fn from(_: ToStrError) -> Self {
        Self::Parse
    }
}

#[derive(Debug)]
struct UpdateIoError {
    path: PathBuf,
    wrapped: io::Error,
}

impl std::fmt::Display for UpdateIoError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "Self-update failed to operate on {:?} due to {}",
            self.path, self.wrapped
        ))
    }
}

impl Error for UpdateIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.wrapped)
    }
}

pub async fn check_for_updates(
    project_dirs: ProjectDirs,
    header_bar: adw::HeaderBar,
    update_button_revealer: gtk::Revealer,
    update_button: gtk::Button,
    update_button_label: gtk::Label,
    update_progress_css_provider: gtk::CssProvider,
) {
    let build_timestamp = match BUILD_TIMESTAMP.and_then(|s| DateTime::parse_from_rfc3339(s).ok()) {
        Some(timestamp) => timestamp,
        None => return,
    };

    let result = spawn_worker(move || {
        let result: Result<Option<(u64, PathBuf)>, CheckForUpdateError> = (|| {
            use crate::warp_wrapper_path::get_warp_wrapper_path;
            let comms_path = match get_warp_wrapper_path() {
                Some(p) => p,
                None => return Err(CheckForUpdateError::NoWarpWrapper)
            };

            let json = ureq::get("https://gitlab.com/api/v4/projects/comms-app%2Fcomms/pipelines/?status=success&per_page=1&ref=main")
                .call()?
                .body_mut()
                .read_json::<serde_json::Value>()?;
            let latest_pipeline_id = match json
                .get(0)
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_u64())
            {
                Some(v) => v,
                None => return Err(CheckForUpdateError::Parse),
            };

            let json = ureq::get(&format!(
                "https://gitlab.com/api/v4/projects/comms-app%2Fcomms/pipelines/{latest_pipeline_id}/jobs"
            ))
                .call()?
                .body_mut()
                .read_json::<serde_json::Value>()?;

            let json = match json.as_array()
                .and_then(|jobs| jobs.iter().find(|job| {
                    job.get("name").and_then(|n| n.as_str()) == Some("build_release_windows")
                })) {
                Some(j) => j,
                None => return Err(CheckForUpdateError::Parse),
            };

            let latest_job_timestamp = match json.get("started_at")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok()) {
                Some(d) => d,
                None => return Err(CheckForUpdateError::Parse),
            };

            // "started_at" has subseconds while "CI_JOB_STARTED_AT" does not.
            let latest_job_timestamp = latest_job_timestamp.trunc_subsecs(0);

            if latest_job_timestamp <= build_timestamp {
                return Ok(None);
            }

            let job_id = match json.get("id")
                .and_then(|v| v.as_u64())
            {
                Some(v) => v,
                None => return Err(CheckForUpdateError::Parse),
            };

            Ok(Some((job_id, comms_path)))
        })();
        result
    }).await;

    let show_update_failure =
        move |button: &Button, update_button_label: &Label, description: String| {
            button.remove_css_class("suggested-action");
            button.add_css_class("destructive-action");
            update_button_label.set_text("Failed to update");
            button.set_tooltip_text(Some(&description));
        };

    let (job_id, entry_comms_path) = match result {
        Ok(Some(tuple)) => tuple,
        Ok(None) => {
            println!("No update was found");
            return;
        }
        Err(CheckForUpdateError::Parse) => {
            eprintln!("Failed to parse GitLab JSON");
            return;
        }
        Err(CheckForUpdateError::Ureq(ureq_error)) => {
            eprintln!("Request error: {ureq_error:?}");
            return;
        }
        Err(CheckForUpdateError::NoWarpWrapper) => {
            let error_text = "Failed to find warp wrapper executable";
            eprintln!("{error_text}");
            header_bar.pack_end(&update_button_revealer);
            update_button_revealer.set_reveal_child(true);
            show_update_failure(&update_button, &update_button_label, error_text.to_string());
            return;
        }
    };
    header_bar.pack_end(&update_button_revealer);
    update_button_revealer.set_reveal_child(true);

    update_button.connect_clicked(move |button| {
        let update_progress_css_provider = update_progress_css_provider.clone();
        let update_progress_css = move |progress: u16| {
            update_progress_css_provider.load_from_data(&format!(".horizontal {{
                background: linear-gradient(to right, rgba(0, 0, 0, 0.15) {progress}%, transparent {progress}%);
            }}"));
        };

        button.set_sensitive(false);
        update_button_label.set_text("Updating...");
        button.set_tooltip_text(Some("Will restart once finished"));
        update_progress_css(0);

        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            let result: Result<(), CheckForUpdateError> = (|| {
                let mut response = ureq::get(&format!("https://gitlab.com/api/v4/projects/comms-app%2Fcomms/jobs/{job_id}/artifacts/comms.exe"))
                    .call()?;
                let content_length: usize = match response.headers().get("Content-Length") {
                    Some(h) => h.to_str()?.parse()?,
                    None => return Err(CheckForUpdateError::Parse),
                };
                let mut buf = Vec::with_capacity(content_length);
                let mut chunk = [0u8; 4096];
                let mut reader = response.body_mut().as_reader();
                loop {
                    match reader.read(&mut chunk)? {
                        0 => break,
                        amount => {
                            buf.extend_from_slice(&chunk[..amount]);
                            let percentage_progress = (buf.len() as f64) / content_length as f64;
                            tx.try_send(Ok(DownloadProgress::Partial(percentage_progress))).unwrap();
                        }
                    }
                }

                tx.try_send(Ok(DownloadProgress::Done(buf.clone()))).unwrap();
                Ok(())
            })();

            if let Err(e) = result {
                tx.try_send(Err(e)).unwrap();
            }
        });

        let show_mid_update_failure = {
            let button = button.clone();
            let update_button_label = update_button_label.clone();
            move |description: String| {
                show_update_failure(&button, &update_button_label, description);
            }
        };

        let glib_context = glib::MainContext::default();
        let update_button_label = update_button_label.clone();
        let project_dirs = project_dirs.clone();
        let entry_comms_path = entry_comms_path.clone();

        glib_context.spawn_local(async move {
            let buf = loop {
                match rx.recv().await {
                    Ok(Ok(DownloadProgress::Partial(percentage_progress))) => update_progress_css((percentage_progress * 100.0).round() as u16),
                    Ok(Ok(DownloadProgress::Done(buf))) => break buf,
                    Ok(Err(e)) => {
                        let text = match e {
                            CheckForUpdateError::Ureq(e) => format!("Request failed: {e:?}"),
                            CheckForUpdateError::Parse => "Failed to parse JSON/headers/etc".to_string(),
                            CheckForUpdateError::NoWarpWrapper => unimplemented!(),
                        };
                        show_mid_update_failure(text);
                        return;
                    }
                    _ => panic!("Loop should always exit before receiving 'channel closed' error"),
                }
            };
            update_button_label.set_text("Restarting");
            let cache_dir = project_dirs.cache_dir();
            let old_comms_cache_path = cache_dir.join("comms-old.exe");
            let new_comms_cache_path = cache_dir.join("comms-new.exe");

            let result = (move || -> Result<(), Box<dyn std::error::Error>> {
                let extracted_comms_exe_path = std::env::current_exe()?;
                let extracted_comms_dir_path = extracted_comms_exe_path
                    .parent()
                    .ok_or_else(|| Box::<dyn std::error::Error>::from(format!("{extracted_comms_exe_path:?} does not have a parent")))?
                    .to_owned();

                let extracted_comms_dir_backup_path = extracted_comms_dir_path
                    .parent()
                    .ok_or_else(|| Box::<dyn std::error::Error>::from(format!("{extracted_comms_dir_path:?} does not have a parent")))?
                    .join(format!("{}.old", extracted_comms_dir_path
                        .file_name()
                        .ok_or_else(|| Box::<dyn std::error::Error>::from(format!("{extracted_comms_dir_path:?} does not have a file name")))?
                        .to_string_lossy()));

                let ignore_not_found_error = |r: Result<_, io::Error>| {
                    match r {
                        Ok(_) => Ok(()),
                        Err(ref e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                        _ => r,
                    }
                };

                let add_err_context = |path: PathBuf, e: io::Error| UpdateIoError {
                    path,
                    wrapped: e
                };

                ignore_not_found_error(std::fs::remove_file(&old_comms_cache_path)).map_err(|e| add_err_context(old_comms_cache_path.clone(), e))?;
                ignore_not_found_error(std::fs::remove_file(&new_comms_cache_path)).map_err(|e| add_err_context(new_comms_cache_path.clone(), e))?;
                ignore_not_found_error(std::fs::remove_dir_all(&extracted_comms_dir_backup_path)).map_err(|e| add_err_context(extracted_comms_dir_backup_path.clone(), e))?;

                let comms_cache_parent_path = new_comms_cache_path.parent().ok_or_else(|| Box::<dyn std::error::Error>::from(format!("{new_comms_cache_path:?} does not have a parent")))?;
                std::fs::create_dir_all(comms_cache_parent_path).map_err(|e| add_err_context(comms_cache_parent_path.to_path_buf(), e))?;
                std::fs::create_dir_all(&extracted_comms_dir_backup_path).map_err(|e| add_err_context(extracted_comms_dir_backup_path.clone(), e))?;
                {
                    let mut new_comms = File::create(&new_comms_cache_path).map_err(|e| add_err_context(new_comms_cache_path.clone(), e))?;
                    new_comms.write_all(&buf)?;
                }

                for path in std::fs::read_dir(&extracted_comms_dir_path)? {
                    let path = path?;
                    std::fs::rename(path.path(), extracted_comms_dir_backup_path.join(path.file_name())).map_err(|e| add_err_context(path.path(), e))?;
                }

                std::fs::rename(&entry_comms_path, &old_comms_cache_path).map_err(|e| add_err_context(entry_comms_path.clone(), e))?;
                std::fs::rename(&new_comms_cache_path, &entry_comms_path).map_err(|e| add_err_context(new_comms_cache_path, e))?;

                std::process::Command::new(entry_comms_path).spawn()?;
                Ok(())
            })();

            if let Err(e) = result {
                show_mid_update_failure(format!("{e:?}"));
                #[cfg(not(debug_assertions))]
                sentry::capture_error(&*e);
            } else {
                std::process::exit(0);
            }
        });
    });
}

async fn spawn_worker<T: std::fmt::Debug + Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        tx.send(f()).unwrap();
    });
    rx.await.unwrap()
}
