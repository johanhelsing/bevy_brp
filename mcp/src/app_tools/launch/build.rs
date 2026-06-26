use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use error_stack::Report;
use serde_json::Value;
use tracing::debug;
use tracing::info;

use super::constants::BUILD_OUTPUT_FRESH_FIELD;
use super::constants::BUILD_OUTPUT_NAME_FIELD;
use super::constants::BUILD_OUTPUT_TARGET_FIELD;
use super::constants::CARGO_RELEASE_FLAG;
use super::logging;
use crate::app_tools::constants::CARGO_BUILD_SUBCOMMAND;
use crate::app_tools::constants::CARGO_COMMAND_NAME;
use crate::app_tools::constants::CARGO_EXAMPLE_FLAG;
use crate::app_tools::constants::CARGO_MESSAGE_FORMAT_JSON_FLAG;
use crate::app_tools::constants::CARGO_RUN_SUBCOMMAND;
use crate::app_tools::constants::PROFILE_RELEASE;
use crate::app_tools::constants::USER_ARGUMENT_SEPARATOR;
use crate::app_tools::targets::TargetType;
use crate::brp_tools::BRP_EXTRAS_PORT_ENV_VAR;
use crate::brp_tools::Port;
use crate::error::Error;
use crate::error::Result;

pub(super) fn validate_manifest_directory(manifest_path: &Path) -> Result<&Path> {
    manifest_path.parent().ok_or_else(|| {
        Report::new(Error::FileOrPathNotFound(
            "Invalid manifest path".to_string(),
        ))
        .attach("No parent directory found")
        .attach(format!("Path: {}", manifest_path.display()))
    })
}

fn set_brp_env_vars(command: &mut Command, port: Option<Port>) {
    if let Some(port) = port {
        command.env(BRP_EXTRAS_PORT_ENV_VAR, port.to_string());
    }
}

fn set_user_env_vars(command: &mut Command, env: Option<&HashMap<String, String>>) {
    if let Some(env_vars) = env {
        for (key, value) in env_vars {
            command.env(key, value);
        }
    }
}

/// Name of the dynamic-loader search-path environment variable for the host OS.
#[cfg(target_os = "macos")]
const DYLIB_PATH_ENV_VAR: &str = "DYLD_LIBRARY_PATH";
#[cfg(target_os = "windows")]
const DYLIB_PATH_ENV_VAR: &str = "PATH";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const DYLIB_PATH_ENV_VAR: &str = "LD_LIBRARY_PATH";

/// Make a directly-executed app binary able to find the shared libraries it needs
/// when built with the `dynamic_linking` (a.k.a. `dylib`) feature.
///
/// `cargo run` sets the loader path up automatically, but a direct execution does
/// not, so we add the two directories cargo would:
/// - `target/<profile>/deps` (sibling of the binary) — holds `libbevy_dylib-<hash>.so`.
/// - the Rust toolchain's target lib dir (`rustc --print target-libdir`) — holds
///   `libstd-<hash>.so`. Without it the process fails to start with
///   `libstd-*.so: cannot open shared object file`.
fn set_dylib_library_path(command: &mut Command, binary_path: &Path) {
    let toolchain_libdir = rustc_target_libdir();
    if let Some(new_path) = dylib_search_path(
        binary_path,
        toolchain_libdir.as_deref(),
        std::env::var_os(DYLIB_PATH_ENV_VAR),
    ) {
        command.env(DYLIB_PATH_ENV_VAR, new_path);
    }
}

/// The active Rust toolchain's target lib dir, via `rustc --print target-libdir` —
/// where `libstd-<hash>.so` lives for `dynamic_linking` builds. Returns `None` if
/// `rustc` can't be run or prints nothing (the dylib path then omits it).
fn rustc_target_libdir() -> Option<PathBuf> {
    let output = Command::new("rustc")
        .args(["--print", "target-libdir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Compute the loader search-path value for a directly-executed app `binary_path`:
/// the binary's sibling `deps` dir, then the toolchain `target-libdir` (when known),
/// then any `existing` value.
///
/// Returns `None` if the path can't be derived (no parent directory) or the
/// resulting value can't be encoded for the environment.
fn dylib_search_path(
    binary_path: &Path,
    toolchain_libdir: Option<&Path>,
    existing: Option<OsString>,
) -> Option<OsString> {
    let mut paths = vec![binary_path.parent()?.join("deps")];
    if let Some(libdir) = toolchain_libdir {
        paths.push(libdir.to_path_buf());
    }
    if let Some(existing) = existing {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).ok()
}

pub(super) fn setup_launch_logging(
    name: &str,
    target_type: TargetType,
    profile: &str,
    binary_path: &Path,
    manifest_dir: &Path,
    port: Port,
    extra_log_info: Option<&str>,
) -> Result<(PathBuf, File)> {
    let (log_file_path, _) =
        logging::create_log_file(name, target_type, profile, binary_path, manifest_dir, port)
            .map_err(|e| Error::tool_call_failed(format!("Failed to create log file: {e}")))?;

    if let Some(extra_info) = extra_log_info {
        logging::append_to_log_file(&log_file_path, &format!("{extra_info}\n"))
            .map_err(|e| Error::tool_call_failed(format!("Failed to append to log file: {e}")))?;
    }

    let log_file_for_redirect =
        logging::open_log_file_for_redirect(&log_file_path).map_err(|e| {
            Error::tool_call_failed(format!("Failed to open log file for redirect: {e}"))
        })?;

    Ok((log_file_path, log_file_for_redirect))
}

pub(super) fn build_cargo_example_command(
    example_name: &str,
    profile: &str,
    port: Option<Port>,
    env: Option<&HashMap<String, String>>,
    command_line_arguments: Option<&[String]>,
) -> Command {
    let mut command = Command::new(CARGO_COMMAND_NAME);
    command
        .arg(CARGO_RUN_SUBCOMMAND)
        .arg(CARGO_EXAMPLE_FLAG)
        .arg(example_name);

    if profile == PROFILE_RELEASE {
        command.arg(CARGO_RELEASE_FLAG);
    }

    if let Some(user_arguments) = command_line_arguments {
        command.arg(USER_ARGUMENT_SEPARATOR).args(user_arguments);
    }

    set_brp_env_vars(&mut command, port);
    set_user_env_vars(&mut command, env);

    command
}

pub(super) fn build_app_command(
    binary_path: &Path,
    port: Option<Port>,
    env: Option<&HashMap<String, String>>,
    command_line_arguments: Option<&[String]>,
) -> Command {
    let mut command = Command::new(binary_path);
    if let Some(user_arguments) = command_line_arguments {
        command.args(user_arguments);
    }
    set_brp_env_vars(&mut command, port);
    set_dylib_library_path(&mut command, binary_path);
    set_user_env_vars(&mut command, env);
    command
}

#[derive(Debug, Clone, Copy)]
pub(super) enum BuildState {
    NotFound,
    Fresh,
    Rebuilt,
}

fn build_cargo_command(
    target_name: &str,
    target_type: TargetType,
    profile: &str,
    manifest_dir: &Path,
) -> Command {
    let mut command = Command::new(CARGO_COMMAND_NAME);
    command.current_dir(manifest_dir);
    command.arg(CARGO_BUILD_SUBCOMMAND);

    target_type.add_cargo_args(&mut command, target_name);

    if profile == PROFILE_RELEASE {
        command.arg(CARGO_RELEASE_FLAG);
    }

    command.arg(CARGO_MESSAGE_FORMAT_JSON_FLAG);

    command
}

fn execute_build_command(
    command: &mut Command,
    target_name: &str,
    target_type: TargetType,
    profile: &str,
    manifest_dir: &Path,
) -> Result<Output> {
    debug!("Running cargo build for {target_type} '{target_name}' with command: {command:?}");

    let output = command.output().map_err(|e| {
        Error::ProcessManagement(format!(
            "Failed to run cargo build for {target_type} '{target_name}' (profile: {profile}, dir: {}): {e}",
            manifest_dir.display()
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::ProcessManagement(format!(
            "Cargo build failed for {target_type} '{target_name}' (profile: {profile}, dir: {}): {stderr}",
            manifest_dir.display()
        ))
        .into());
    }

    Ok(output)
}

fn parse_build_output(stdout: &[u8], target_name: &str) -> BuildState {
    let stdout_str = String::from_utf8_lossy(stdout);

    for line in stdout_str.lines() {
        if let Ok(json) = serde_json::from_str::<Value>(line)
            && let Some(target) = json.get(BUILD_OUTPUT_TARGET_FIELD)
            && let Some(name) = target.get(BUILD_OUTPUT_NAME_FIELD)
            && name.as_str() == Some(target_name)
        {
            return json
                .get(BUILD_OUTPUT_FRESH_FIELD)
                .and_then(serde_json::Value::as_bool)
                .map_or(BuildState::Rebuilt, |is_fresh| {
                    if is_fresh {
                        BuildState::Fresh
                    } else {
                        BuildState::Rebuilt
                    }
                });
        }
    }

    BuildState::NotFound
}

fn log_build_result(build_state: BuildState, target_name: &str, target_type: TargetType) {
    match build_state {
        BuildState::NotFound => {
            debug!("Target '{target_name}' not found in build output, assuming it was built");
        },
        BuildState::Fresh => {
            debug!("{target_type} '{target_name}' was already up to date");
        },
        BuildState::Rebuilt => {
            info!("{target_type} '{target_name}' was built successfully");
        },
    }
}

pub(super) fn run_cargo_build(
    target_name: &str,
    target_type: TargetType,
    profile: &str,
    manifest_dir: &Path,
) -> Result<BuildState> {
    let mut command = build_cargo_command(target_name, target_type, profile, manifest_dir);
    let output = execute_build_command(
        &mut command,
        target_name,
        target_type,
        profile,
        manifest_dir,
    )?;
    let build_state = parse_build_output(&output.stdout, target_name);
    log_build_result(build_state, target_name, target_type);

    Ok(build_state)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn dylib_search_path_with_no_existing_value_is_just_deps_dir() {
        let binary = Path::new("/work/target/debug/my_app");
        let result = dylib_search_path(binary, None, None);
        assert_eq!(result, Some(OsString::from("/work/target/debug/deps")));
    }

    #[test]
    fn dylib_search_path_includes_toolchain_libdir_after_deps() {
        let binary = Path::new("/work/target/debug/my_app");
        let libdir = Path::new("/rust/lib");

        let result = dylib_search_path(binary, Some(libdir), None).expect("should produce a value");

        // deps first (freshly-built dylib wins), then the toolchain libdir (libstd).
        let entries: Vec<PathBuf> = std::env::split_paths(&result).collect();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/work/target/debug/deps"),
                PathBuf::from("/rust/lib"),
            ]
        );
    }

    #[test]
    fn dylib_search_path_prepends_deps_and_libdir_then_preserves_existing() {
        let binary = Path::new("/work/target/release/my_app");
        let libdir = Path::new("/rust/lib");
        let existing = std::env::join_paths(["/usr/lib", "/opt/lib"]).unwrap();

        let result =
            dylib_search_path(binary, Some(libdir), Some(existing)).expect("should produce a value");

        // The deps dir and toolchain libdir come first, with the pre-existing
        // entries retained after them.
        let entries: Vec<PathBuf> = std::env::split_paths(&result).collect();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/work/target/release/deps"),
                PathBuf::from("/rust/lib"),
                PathBuf::from("/usr/lib"),
                PathBuf::from("/opt/lib"),
            ]
        );
    }

    #[test]
    fn dylib_search_path_returns_none_when_binary_has_no_parent() {
        assert_eq!(dylib_search_path(Path::new(""), None, None), None);
    }
}
