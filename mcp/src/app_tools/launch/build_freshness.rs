use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use super::constants::BUILD_SCRIPT_FILE;
use super::constants::CARGO_CONFIG_DIR;
use super::constants::CARGO_CONFIG_FILE;
use super::constants::CARGO_CONFIG_TOML_FILE;
use super::constants::CARGO_LOCK_FILE;
use super::constants::DEP_INFO_EXTENSION;
use super::constants::RUST_TOOLCHAIN_FILE;
use super::constants::RUST_TOOLCHAIN_TOML_FILE;
use crate::app_tools::constants::CARGO_MANIFEST_FILE;
use crate::app_tools::targets::BevyTarget;
use crate::error::Error;
use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FreshnessCheckResult {
    Fresh,
    Stale(String),
    Unknown(String),
}

pub(super) fn check_target_freshness(target: &BevyTarget, profile: &str) -> FreshnessCheckResult {
    if !target.is_app() {
        return FreshnessCheckResult::Unknown(
            "lock-free freshness checks are only supported for app binaries".to_string(),
        );
    }

    try_check_target_freshness(target, profile)
        .unwrap_or_else(|error| FreshnessCheckResult::Unknown(format!("{error}")))
}

fn try_check_target_freshness(target: &BevyTarget, profile: &str) -> Result<FreshnessCheckResult> {
    let binary_path = target.get_binary_path(profile);
    if !binary_path.exists() {
        return Ok(FreshnessCheckResult::Stale(format!(
            "binary does not exist: {}",
            binary_path.display()
        )));
    }

    let binary_mtime = file_modified_time(&binary_path)?;
    let dep_info_path = dep_info_path(target, profile);
    if !dep_info_path.exists() {
        return Ok(FreshnessCheckResult::Unknown(format!(
            "dep-info file does not exist: {}",
            dep_info_path.display()
        )));
    }

    let dep_info_contents = fs::read_to_string(&dep_info_path).map_err(|error| {
        Error::FileOperation(format!(
            "Failed to read dep-info file {}: {error}",
            dep_info_path.display()
        ))
    })?;
    let dep_info_dir = dep_info_path
        .parent()
        .ok_or_else(|| Error::FileOrPathNotFound("Dep-info file has no parent directory".into()))?;

    if let Some(other_output) =
        foreign_workspace_output(&dep_info_contents, dep_info_dir, &target.workspace_root)
    {
        return Ok(FreshnessCheckResult::Stale(format!(
            "binary belongs to another workspace: dep-info for {} names {}, which is outside {}",
            binary_path.display(),
            other_output.display(),
            target.workspace_root.display()
        )));
    }

    let dependencies = parse_dep_info_dependencies(&dep_info_contents, dep_info_dir);

    if dependencies.is_empty() {
        return Ok(FreshnessCheckResult::Unknown(format!(
            "dep-info file had no dependencies: {}",
            dep_info_path.display()
        )));
    }

    for dependency in dependencies {
        let Some(staleness_reason) = compare_input_to_binary(&dependency, binary_mtime)? else {
            continue;
        };
        return Ok(FreshnessCheckResult::Stale(staleness_reason));
    }

    for input in extra_fingerprint_inputs(target) {
        let Some(staleness_reason) = compare_optional_input_to_binary(&input, binary_mtime)? else {
            continue;
        };
        return Ok(FreshnessCheckResult::Stale(staleness_reason));
    }

    Ok(FreshnessCheckResult::Fresh)
}

fn dep_info_path(target: &BevyTarget, profile: &str) -> PathBuf {
    target
        .get_binary_path(profile)
        .with_extension(DEP_INFO_EXTENSION)
}

fn extra_fingerprint_inputs(target: &BevyTarget) -> Vec<PathBuf> {
    let mut inputs = vec![target.manifest.clone()];

    let workspace_manifest = target.workspace_root.join(CARGO_MANIFEST_FILE);
    if workspace_manifest != target.manifest {
        inputs.push(workspace_manifest);
    }

    inputs.push(target.workspace_root.join(CARGO_LOCK_FILE));
    inputs.extend(find_cargo_config_files(
        &target.manifest,
        &target.workspace_root,
    ));

    if let Some(package_dir) = target.manifest.parent() {
        inputs.push(package_dir.join(BUILD_SCRIPT_FILE));
    }

    inputs.push(target.workspace_root.join(RUST_TOOLCHAIN_TOML_FILE));
    inputs.push(target.workspace_root.join(RUST_TOOLCHAIN_FILE));

    inputs
}

fn find_cargo_config_files(manifest_path: &Path, workspace_root: &Path) -> Vec<PathBuf> {
    let mut configs = Vec::new();

    let Some(mut current_dir) = manifest_path.parent() else {
        return configs;
    };

    loop {
        configs.push(
            current_dir
                .join(CARGO_CONFIG_DIR)
                .join(CARGO_CONFIG_TOML_FILE),
        );
        configs.push(current_dir.join(CARGO_CONFIG_DIR).join(CARGO_CONFIG_FILE));

        if current_dir == workspace_root {
            break;
        }

        let Some(parent) = current_dir.parent() else {
            break;
        };
        current_dir = parent;
    }

    configs
}

fn compare_input_to_binary(input_path: &Path, binary_mtime: SystemTime) -> Result<Option<String>> {
    compare_path_to_binary(
        input_path,
        binary_mtime,
        MissingInputPolicy::TreatAsStale,
        "dependency listed in dep-info is missing",
        "dependency is newer than binary",
    )
}

fn compare_optional_input_to_binary(
    input_path: &Path,
    binary_mtime: SystemTime,
) -> Result<Option<String>> {
    compare_path_to_binary(
        input_path,
        binary_mtime,
        MissingInputPolicy::Ignore,
        "",
        "build input is newer than binary",
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MissingInputPolicy {
    Ignore,
    TreatAsStale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackslashState {
    ReadingToken,
    Escaped,
}

impl BackslashState {
    const fn is_escaped(self) -> bool { matches!(self, Self::Escaped) }
}

fn compare_path_to_binary(
    input_path: &Path,
    binary_mtime: SystemTime,
    missing_input_policy: MissingInputPolicy,
    missing_reason: &str,
    stale_reason: &str,
) -> Result<Option<String>> {
    if !input_path.exists() {
        return Ok((missing_input_policy == MissingInputPolicy::TreatAsStale)
            .then(|| format!("{missing_reason}: {}", input_path.display())));
    }

    let input_mtime = file_modified_time(input_path)?;
    Ok((input_mtime > binary_mtime).then(|| format!("{stale_reason}: {}", input_path.display())))
}

fn file_modified_time(path: &Path) -> Result<SystemTime> {
    fs::metadata(path)
        .map_err(|error| {
            Error::FileOperation(format!(
                "Failed to read metadata for {}: {error}",
                path.display()
            ))
        })?
        .modified()
        .map_err(|error| {
            Error::FileOperation(format!(
                "Failed to read modification time for {}: {error}",
                path.display()
            ))
            .into()
        })
}

/// The output path a dep-info file describes, when it lies outside `workspace_root`.
///
/// Cargo rewrites `target/<profile>/<name>.d` each time it uplifts the binary, and
/// the path on the left of the rule is the uplift destination as *that* build
/// computed it — `<its workspace root>/target/<profile>/<name>`. Workspaces that
/// share one `target/` (git worktrees whose `target` is a symlink to the main
/// checkout's, say) therefore take turns owning the one binary file, and the
/// dep-info names whoever owns it now.
///
/// Comparing mtimes cannot see this: the sibling's binary is newer than every
/// source file it lists, so the check reports fresh and the launch runs another
/// workspace's build from this workspace's directory. When the recorded output
/// path is outside the launching workspace, the binary is not this workspace's
/// and the caller must build before launching.
fn foreign_workspace_output(
    contents: &str,
    base_dir: &Path,
    workspace_root: &Path,
) -> Option<PathBuf> {
    let (rule_target, _) = split_dep_info_rule(contents)?;
    let declared_output = resolve_dep_info_path(rule_target.trim(), base_dir)?;
    (!declared_output.starts_with(workspace_root)).then_some(declared_output)
}

/// Splits a dep-info rule into its output path and its dependency list.
///
/// A make rule ends its target at a `:`, but a path may contain one (a Windows
/// drive letter, or simply a colon in a directory name), so the separator is the
/// first unescaped `:` followed by whitespace or the end of the text.
fn split_dep_info_rule(contents: &str) -> Option<(&str, &str)> {
    let mut escaped = BackslashState::ReadingToken;

    for (index, ch) in contents.char_indices() {
        if escaped.is_escaped() {
            escaped = BackslashState::ReadingToken;
            continue;
        }

        match ch {
            '\\' => escaped = BackslashState::Escaped,
            ':' => {
                let rest = &contents[index + ch.len_utf8()..];
                if rest.chars().next().is_none_or(char::is_whitespace) {
                    return Some((&contents[..index], rest));
                }
            },
            _ => {},
        }
    }

    None
}

/// Unescapes one dep-info path token and makes it absolute against `base_dir`.
fn resolve_dep_info_path(token: &str, base_dir: &Path) -> Option<PathBuf> {
    let mut unescaped = String::with_capacity(token.len());
    let mut escaped = BackslashState::ReadingToken;

    for ch in token.chars() {
        if escaped.is_escaped() {
            unescaped.push(ch);
            escaped = BackslashState::ReadingToken;
            continue;
        }

        match ch {
            '\\' => escaped = BackslashState::Escaped,
            _ => unescaped.push(ch),
        }
    }

    if unescaped.is_empty() {
        return None;
    }

    let path = PathBuf::from(unescaped);
    Some(if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    })
}

fn parse_dep_info_dependencies(contents: &str, base_dir: &Path) -> Vec<PathBuf> {
    let Some((_, dependency_text)) = split_dep_info_rule(contents) else {
        return Vec::new();
    };

    let mut dependencies = Vec::new();
    let mut current = String::new();
    let mut escaped = BackslashState::ReadingToken;

    for ch in dependency_text.chars() {
        if escaped.is_escaped() {
            match ch {
                '\n' | '\r' => {},
                _ => current.push(ch),
            }
            escaped = BackslashState::ReadingToken;
            continue;
        }

        match ch {
            '\\' => escaped = BackslashState::Escaped,
            c if c.is_whitespace() => {
                push_dependency(&mut dependencies, &mut current, base_dir);
            },
            _ => current.push(ch),
        }
    }

    push_dependency(&mut dependencies, &mut current, base_dir);
    dependencies
}

fn push_dependency(dependencies: &mut Vec<PathBuf>, current: &mut String, base_dir: &Path) {
    if current.is_empty() {
        return;
    }

    let raw_path = std::mem::take(current);
    let path = PathBuf::from(&raw_path);
    if path.is_absolute() {
        dependencies.push(path);
    } else {
        dependencies.push(base_dir.join(path));
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests should panic on unexpected values"
)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::FreshnessCheckResult;
    use super::check_target_freshness;
    use super::foreign_workspace_output;
    use super::parse_dep_info_dependencies;
    use super::split_dep_info_rule;
    use crate::app_tools::targets::BevyTarget;
    use crate::app_tools::targets::TargetType;

    const FILE_TIMESTAMP_ADVANCE_MS: u64 = 20;

    fn test_target(workspace_root: &Path, manifest_path: &Path, name: &str) -> BevyTarget {
        BevyTarget {
            name:           name.to_string(),
            target_type:    TargetType::App,
            package_name:   "pkg".to_string(),
            workspace_root: workspace_root.to_path_buf(),
            manifest:       manifest_path.to_path_buf(),
            relative:       PathBuf::new(),
            source:         PathBuf::new(),
        }
    }

    #[test]
    fn parses_dep_info_with_escaped_spaces_and_line_continuations() {
        let base_dir = Path::new("/tmp");
        let dependencies = parse_dep_info_dependencies(
            "target/debug/demo: /tmp/one.rs /tmp/two\\ with\\ spaces.rs \\\n             /tmp/three.rs",
            base_dir,
        );

        assert_eq!(
            dependencies,
            vec![
                PathBuf::from("/tmp/one.rs"),
                PathBuf::from("/tmp/two with spaces.rs"),
                PathBuf::from("/tmp/three.rs"),
            ]
        );
    }

    #[test]
    fn returns_fresh_when_binary_is_newer_than_inputs() {
        let temp_dir = tempdir().expect("temp dir");
        let workspace_root = temp_dir.path();
        let manifest_path = workspace_root.join("Cargo.toml");
        let src_path = workspace_root.join("src/main.rs");
        let binary_path = workspace_root.join("target/debug/demo");
        let dep_info_path = workspace_root.join("target/debug/demo.d");

        fs::create_dir_all(src_path.parent().expect("src parent")).expect("create src dir");
        fs::create_dir_all(binary_path.parent().expect("binary parent"))
            .expect("create target dir");
        fs::write(
            &manifest_path,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        fs::write(workspace_root.join("Cargo.lock"), "# lock\n").expect("write lock");
        fs::write(&src_path, "fn main() {}\n").expect("write source");

        thread::sleep(Duration::from_millis(FILE_TIMESTAMP_ADVANCE_MS));
        fs::write(&binary_path, "binary").expect("write binary");
        fs::write(
            &dep_info_path,
            format!("{}: {}\n", binary_path.display(), src_path.display()),
        )
        .expect("write dep info");

        let target = test_target(workspace_root, &manifest_path, "demo");
        assert_eq!(
            check_target_freshness(&target, "debug"),
            FreshnessCheckResult::Fresh
        );
    }

    #[test]
    fn returns_stale_when_dependency_is_newer_than_binary() {
        let temp_dir = tempdir().expect("temp dir");
        let workspace_root = temp_dir.path();
        let manifest_path = workspace_root.join("Cargo.toml");
        let src_path = workspace_root.join("src/main.rs");
        let binary_path = workspace_root.join("target/debug/demo");
        let dep_info_path = workspace_root.join("target/debug/demo.d");

        fs::create_dir_all(src_path.parent().expect("src parent")).expect("create src dir");
        fs::create_dir_all(binary_path.parent().expect("binary parent"))
            .expect("create target dir");
        fs::write(
            &manifest_path,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        fs::write(workspace_root.join("Cargo.lock"), "# lock\n").expect("write lock");
        fs::write(&binary_path, "binary").expect("write binary");

        thread::sleep(Duration::from_millis(FILE_TIMESTAMP_ADVANCE_MS));
        fs::write(&src_path, "fn main() {}\n").expect("write source");
        fs::write(
            &dep_info_path,
            format!("{}: {}\n", binary_path.display(), src_path.display()),
        )
        .expect("write dep info");

        let target = test_target(workspace_root, &manifest_path, "demo");
        assert!(matches!(
            check_target_freshness(&target, "debug"),
            FreshnessCheckResult::Stale(reason)
                if reason.contains("dependency is newer than binary")
        ));
    }

    /// Two checkouts of the same package sharing one `target/` (git worktrees whose
    /// `target` is a symlink to the main checkout's): the sibling built last, so the
    /// binary and its dep-info are the sibling's. Every mtime says fresh; only the
    /// recorded output path gives it away.
    #[test]
    fn returns_stale_when_binary_was_built_by_a_sibling_workspace() {
        let temp_dir = tempdir().expect("temp dir");
        let launching_root = temp_dir.path().join("worktree-b");
        let sibling_root = temp_dir.path().join("worktree-a");
        let manifest_path = launching_root.join("Cargo.toml");
        let src_path = launching_root.join("src/main.rs");
        let binary_path = launching_root.join("target/debug/demo");
        let dep_info_path = launching_root.join("target/debug/demo.d");

        fs::create_dir_all(src_path.parent().expect("src parent")).expect("create src dir");
        fs::create_dir_all(binary_path.parent().expect("binary parent"))
            .expect("create target dir");
        fs::write(
            &manifest_path,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        fs::write(launching_root.join("Cargo.lock"), "# lock\n").expect("write lock");
        fs::write(&src_path, "fn main() {}\n").expect("write source");

        // The sibling's sources exist too, so the mtime comparison finds nothing
        // missing and nothing newer than the binary.
        let sibling_src = sibling_root.join("src/main.rs");
        fs::create_dir_all(sibling_src.parent().expect("sibling src parent"))
            .expect("create sibling src dir");
        fs::write(&sibling_src, "fn main() {}\n").expect("write sibling source");

        // The sibling's build is newer than every source file, here and there.
        thread::sleep(Duration::from_millis(FILE_TIMESTAMP_ADVANCE_MS));
        fs::write(&binary_path, "sibling binary").expect("write binary");
        fs::write(
            &dep_info_path,
            format!(
                "{}: {}\n",
                sibling_root.join("target/debug/demo").display(),
                sibling_root.join("src/main.rs").display()
            ),
        )
        .expect("write dep info");

        let target = test_target(&launching_root, &manifest_path, "demo");
        let result = check_target_freshness(&target, "debug");
        assert!(
            matches!(
                &result,
                FreshnessCheckResult::Stale(reason)
                    if reason.contains("belongs to another workspace")
            ),
            "expected stale, got {result:?}"
        );
    }

    #[test]
    fn foreign_workspace_output_accepts_this_workspace() {
        let contents = "/work/b/target/debug/demo: /work/b/src/main.rs";
        assert_eq!(
            foreign_workspace_output(contents, Path::new("/work/b/target/debug"), Path::new("/work/b")),
            None
        );
    }

    #[test]
    fn foreign_workspace_output_is_not_fooled_by_a_shared_path_prefix() {
        // `/work/b` is not a parent of `/work/bee`, even though the string is a prefix.
        let contents = "/work/bee/target/debug/demo: /work/bee/src/main.rs";
        assert_eq!(
            foreign_workspace_output(contents, Path::new("/work/b/target/debug"), Path::new("/work/b")),
            Some(PathBuf::from("/work/bee/target/debug/demo"))
        );
    }

    #[test]
    fn foreign_workspace_output_is_none_without_a_rule_separator() {
        assert_eq!(
            foreign_workspace_output("garbage with no rule", Path::new("/work/b/target/debug"), Path::new("/work/b")),
            None
        );
    }

    #[test]
    fn split_dep_info_rule_keeps_a_windows_drive_letter_with_its_path() {
        let (rule_target, dependencies) =
            split_dep_info_rule(r"C:\work\target\debug\demo.exe: C:\work\src\main.rs")
                .expect("should split");
        assert_eq!(rule_target, r"C:\work\target\debug\demo.exe");
        assert_eq!(dependencies.trim(), r"C:\work\src\main.rs");
    }

    #[test]
    fn split_dep_info_rule_ignores_an_escaped_colon() {
        let (rule_target, _) =
            split_dep_info_rule(r"/work/odd\:name/demo: /work/odd\:name/src/main.rs")
                .expect("should split");
        assert_eq!(rule_target, r"/work/odd\:name/demo");
    }

    #[test]
    fn returns_unknown_when_dep_info_is_missing() {
        let temp_dir = tempdir().expect("temp dir");
        let workspace_root = temp_dir.path();
        let manifest_path = workspace_root.join("Cargo.toml");
        let binary_path = workspace_root.join("target/debug/demo");

        fs::create_dir_all(binary_path.parent().expect("binary parent"))
            .expect("create target dir");
        fs::write(
            &manifest_path,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        fs::write(&binary_path, "binary").expect("write binary");

        let target = test_target(workspace_root, &manifest_path, "demo");
        assert!(matches!(
            check_target_freshness(&target, "debug"),
            FreshnessCheckResult::Unknown(reason)
                if reason.contains("dep-info file does not exist")
        ));
    }
}
