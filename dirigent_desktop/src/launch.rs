//! Launch arguments forwarded unchanged by the installed launcher.

use std::{ffi::OsString, path::PathBuf};

pub(crate) fn project_directory(
    args: impl IntoIterator<Item = OsString>,
) -> Result<Option<PathBuf>, String> {
    let mut args = args.into_iter();
    let Some(argument) = args.next() else {
        return Ok(None);
    };
    if argument != "--open-project" {
        return Err(format!("Unknown argument: {}", argument.to_string_lossy()));
    }
    let path = args
        .next()
        .filter(|path| !path.is_empty())
        .ok_or("--open-project requires a directory path.")?;
    if args.next().is_some() {
        return Err("--open-project accepts one directory path.".into());
    }
    Ok(Some(PathBuf::from(path)))
}

#[cfg(windows)]
pub(crate) struct ExistingWindow {
    // Keep ownership until GPUI and all persistent state have been dropped.
    pub(crate) _guard: dirigent_launcher::instance::Instance,
    pub(crate) requests: async_channel::Receiver<dirigent_launcher::instance::Request>,
}

#[cfg(windows)]
pub(crate) fn single_instance(
    project_directory: &Result<Option<PathBuf>, String>,
) -> Result<Option<ExistingWindow>, String> {
    let config = crate::platform::config_dir()?;
    std::fs::create_dir_all(&config)
        .map_err(|error| format!("Cannot create {}: {error}", config.display()))?;
    // Junctions and differently spelled paths must not produce two owners of the same DB.
    let config = config
        .canonicalize()
        .map_err(|error| format!("Cannot resolve {}: {error}", config.display()))?;
    let endpoint = format!(
        "dirigent-{}",
        blake3::hash(config.as_os_str().as_encoded_bytes()).to_hex()
    );
    // A relative path belongs to the launching process, not the existing window's cwd.
    let project_directory = project_directory.clone().and_then(|path| {
        path.map(std::path::absolute)
            .transpose()
            .map_err(|error| format!("Cannot resolve project directory: {error}"))
    });
    let (tx, requests) = async_channel::unbounded();
    dirigent_launcher::instance::Instance::claim_or_forward(
        &endpoint,
        &project_directory,
        move |request| tx.send_blocking(request).is_ok(),
    )
    .map(|instance| {
        instance.map(|guard| ExistingWindow {
            _guard: guard,
            requests,
        })
    })
    .map_err(|error| format!("Could not contact the Dirigent window: {error}"))
}
