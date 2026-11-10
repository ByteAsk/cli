//! The per-repository check profile, `.byteask/checks.toml`, used by the merge check and the
//! local guardrails.
//!
//! ```toml
//! schema = 1
//!
//! # The golden-path build and tests. Each command runs with `bash -c` from the
//! # repository root, in order; the first non-zero exit fails the step. Commands get
//! # BYTEASK_RESULTS_DIR, an empty directory for reports.
//! [build]
//! run = ["cmake --preset ci", "cmake --build --preset ci"]
//! timeout_secs = 1800
//!
//! [test]
//! run = ["ctest --preset ci --output-junit \"$BYTEASK_RESULTS_DIR/junit.xml\""]
//! junit = "junit.xml"                     # in BYTEASK_RESULTS_DIR
//! manifest = ".byteask/required-tests.txt" # tests that must run and pass
//!
//! # Slow checks: before merge, only when the change touches a marked path.
//! [[slow]]
//! name = "asan"
//! run = ["cmake --preset asan", "cmake --build --preset asan", "ctest --preset asan"]
//!
//! [files]
//! marked = ["src/feed/**", "src/book/**"]  # hot paths
//! # non_code = [...]   replaces the default list below
//! # code = [...]       always code, even when non_code matches
//!
//! [merge]
//! reviewer_may_approve = ["no_code_change"]  # NOT RUN reasons a person may approve
//! perf = "off"                               # "optional" | "required" (perf card)
//! ```
//!
//! Who reads it from where: the merge check reads it from the protected base branch, and
//! the local guardrails from the commit the agent session started on, so a change to the
//! profile never applies to its own checks. Unknown keys are refused: a typo must not silently
//! switch a check off.

use std::path::Path;

use check_result::NotRunReason;
use globset::GlobBuilder;
use globset::GlobSet;
use globset::GlobSetBuilder;
use serde::Deserialize;

/// Where the profile lives, relative to the repository root.
pub const PATH: &str = ".byteask/checks.toml";
/// The only schema this build understands.
pub const SCHEMA: u32 = 1;
/// Longest a single step may be allowed to run (24 h).
pub const MAX_TIMEOUT_SECS: u64 = 86_400;
/// A step's time limit when the profile names none.
pub const DEFAULT_TIMEOUT_SECS: u64 = 3_600;

/// What counts as "no code or build change" when the profile does not say. Markdown,
/// reStructuredText, plain-text files in doc directories, the usual top-level text files,
/// images and licence files. Everything else, including unknown types, is code.
pub const DEFAULT_NON_CODE: &[&str] = &[
    "**/*.md",
    "**/*.markdown",
    "**/*.rst",
    "**/doc/**/*.txt",
    "**/docs/**/*.txt",
    "**/README",
    "**/README.*",
    "**/CHANGELOG",
    "**/CHANGELOG.*",
    "**/CHANGES",
    "**/CHANGES.*",
    "**/NEWS",
    "**/NEWS.*",
    "**/AUTHORS",
    "**/AUTHORS.*",
    "**/CONTRIBUTORS",
    "**/CONTRIBUTORS.*",
    "**/CONTRIBUTING",
    "**/CONTRIBUTING.*",
    "**/CODE_OF_CONDUCT.*",
    "**/LICENSE",
    "**/LICENSE.*",
    "**/LICENSE-*",
    "**/LICENCE",
    "**/LICENCE.*",
    "**/COPYING",
    "**/COPYING.*",
    "**/NOTICE",
    "**/NOTICE.*",
    "**/*.png",
    "**/*.jpg",
    "**/*.jpeg",
    "**/*.gif",
    "**/*.svg",
    "**/*.webp",
    "**/*.ico",
    "**/*.bmp",
];

/// Paths that are code even when a non-code pattern matches them, when the profile does not
/// name its own `code` list (build files with a text extension).
pub const DEFAULT_CODE: &[&str] = &["**/*.cmake", "**/conanfile.txt", "**/requirements*.txt"];

/// Always code, whatever the profile says: the checks' own settings and CMake's build files.
/// A change to the profile or the test manifest must never be skipped as "docs only".
pub const ALWAYS_CODE: &[&str] = &[".byteask/**", "**/CMakeLists.txt"];

/// The parsed profile.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub schema: u32,
    /// The golden-path build.
    pub build: Option<Step>,
    /// The golden-path tests.
    pub test: Option<TestStep>,
    /// Slow checks, run before merge when a marked path changes.
    #[serde(default)]
    pub slow: Vec<SlowCheck>,
    #[serde(default)]
    pub files: Files,
    #[serde(default)]
    pub merge: MergePolicy,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub run: Vec<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestStep {
    pub run: Vec<String>,
    pub timeout_secs: Option<u64>,
    /// The JUnit XML report the test commands write, relative to `BYTEASK_RESULTS_DIR`.
    pub junit: Option<String>,
    /// A file in the repository listing the tests that must run and pass, one per line
    /// (`suite.case` or `case`; `#` starts a comment). Read from the protected base branch.
    pub manifest: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SlowCheck {
    pub name: String,
    pub run: Vec<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Files {
    /// Replaces [`DEFAULT_NON_CODE`].
    pub non_code: Option<Vec<String>>,
    /// Replaces [`DEFAULT_CODE`]; [`ALWAYS_CODE`] applies on top.
    pub code: Option<Vec<String>>,
    /// Hot paths: a change touching one runs the slow checks and the perf card.
    #[serde(default)]
    pub marked: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergePolicy {
    /// NOT RUN reasons a human reviewer may approve at merge. `not_checked` can never
    /// be approved: it means CI never checked this version.
    #[serde(default = "default_approvable")]
    pub reviewer_may_approve: Vec<NotRunReason>,
    /// The perf card: whether a bench result is needed when a marked path changes.
    #[serde(default)]
    pub perf: PerfPolicy,
}

impl Default for MergePolicy {
    fn default() -> Self {
        Self {
            reviewer_may_approve: default_approvable(),
            perf: PerfPolicy::default(),
        }
    }
}

fn default_approvable() -> Vec<NotRunReason> {
    vec![NotRunReason::NoCodeChange]
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PerfPolicy {
    /// No perf card.
    #[default]
    Off,
    /// Show a perf card when a bench result exists; never block for a missing one.
    Optional,
    /// A change touching a marked path needs a bench result; its gate counts at merge.
    Required,
}

impl Profile {
    /// Parse and check a profile's text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let profile: Profile =
            toml::from_str(text).map_err(|err| format!("{PATH}: {}", err.message()))?;
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!(
                "{PATH}: schema {} is not supported (this ByteAsk reads schema {SCHEMA})",
                self.schema
            ));
        }
        let steps = self
            .build
            .iter()
            .map(|step| ("build", &step.run, step.timeout_secs))
            .chain(
                self.test
                    .iter()
                    .map(|step| ("test", &step.run, step.timeout_secs)),
            )
            .chain(
                self.slow
                    .iter()
                    .map(|check| (check.name.as_str(), &check.run, check.timeout_secs)),
            );
        for (name, run, timeout) in steps {
            if run.is_empty() || run.iter().any(|command| command.trim().is_empty()) {
                return Err(format!(
                    "{PATH}: [{name}] needs at least one non-empty command in `run`"
                ));
            }
            if let Some(secs) = timeout
                && !(1..=MAX_TIMEOUT_SECS).contains(&secs)
            {
                return Err(format!(
                    "{PATH}: [{name}] timeout_secs must be between 1 and {MAX_TIMEOUT_SECS}"
                ));
            }
        }
        let mut names = Vec::new();
        for check in &self.slow {
            let ok = !check.name.is_empty()
                && check.name.len() <= 40
                && check
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
            if !ok {
                return Err(format!(
                    "{PATH}: slow check name {:?} must be 1-40 of a-z, 0-9, '-' and '_'",
                    check.name
                ));
            }
            if RESERVED_NAMES.contains(&check.name.as_str()) || names.contains(&&check.name) {
                return Err(format!(
                    "{PATH}: slow check name {:?} is used twice or reserved",
                    check.name
                ));
            }
            names.push(&check.name);
        }
        if let Some(test) = &self.test {
            for (key, value) in [("junit", &test.junit), ("manifest", &test.manifest)] {
                if let Some(path) = value
                    && !is_plain_relative(path)
                {
                    return Err(format!(
                        "{PATH}: [test] {key} must be a relative path inside the repository: {path:?}"
                    ));
                }
            }
        }
        if self
            .merge
            .reviewer_may_approve
            .contains(&NotRunReason::NotChecked)
        {
            return Err(format!(
                "{PATH}: [merge] reviewer_may_approve cannot list not_checked: a version CI never \
                 checked cannot be approved"
            ));
        }
        FileClasses::new(&self.files)?;
        Ok(())
    }

    /// The profile's file classes (code, non-code, marked).
    pub fn file_classes(&self) -> FileClasses {
        // Validated in `parse`.
        FileClasses::new(&self.files).unwrap_or_else(|_| FileClasses::defaults())
    }
}

/// Check names the merge check uses itself.
pub const RESERVED_NAMES: &[&str] = &["build", "tests", "manifest", "perf", "profile"];

fn is_plain_relative(path: &str) -> bool {
    let path = path.trim();
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.split('/').any(|part| part == ".." || part.is_empty())
}

/// The profile's file classes: code, non-code and marked.
#[derive(Clone, Debug)]
pub struct FileClasses {
    non_code: GlobSet,
    code: GlobSet,
    marked: GlobSet,
}

fn globset<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .case_insensitive(true)
            .build()
            .map_err(|err| format!("{PATH}: bad file pattern {pattern:?}: {err}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|err| format!("{PATH}: bad file patterns: {err}"))
}

impl FileClasses {
    pub fn new(files: &Files) -> Result<Self, String> {
        let non_code = match &files.non_code {
            Some(list) => globset(list.iter().map(String::as_str))?,
            None => globset(DEFAULT_NON_CODE.iter().copied())?,
        };
        let code_list: Vec<&str> = match &files.code {
            Some(list) => list.iter().map(String::as_str).collect(),
            None => DEFAULT_CODE.to_vec(),
        };
        let code = globset(code_list.into_iter().chain(ALWAYS_CODE.iter().copied()))?;
        let marked = globset(files.marked.iter().map(String::as_str))?;
        Ok(Self {
            non_code,
            code,
            marked,
        })
    }

    /// The classes with no profile (the defaults; nothing marked).
    pub fn defaults() -> Self {
        Self::new(&Files::default()).unwrap_or_else(|err| panic!("default patterns: {err}"))
    }

    /// A path is code unless a non-code pattern matches it and no code pattern does.
    /// Unknown types are code.
    pub fn is_code(&self, path: &str) -> bool {
        let path = normalize(path);
        self.code.is_match(&path) || !self.non_code.is_match(&path)
    }

    /// Every changed path is non-code, and there is at least one. A change with no
    /// paths is not "docs only" (callers decide what nothing means).
    pub fn only_non_code<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> bool {
        let mut any = false;
        for path in paths {
            any = true;
            if self.is_code(path) {
                return false;
            }
        }
        any
    }

    /// Some changed path is a marked (hot-path) path.
    pub fn touches_marked<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> bool {
        paths
            .into_iter()
            .any(|path| self.marked.is_match(normalize(path).as_str()))
    }
}

fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    path.trim_start_matches("./").to_string()
}

/// A starting profile from what the repository already has (CMake presets, a
/// compile database next to a CMake build tree, or a top-level CMakeLists.txt). The text is
/// for a person to review and commit through code review; nothing reads it until it is on
/// the protected branch.
pub fn propose(root: &Path) -> Proposal {
    let junit = "--output-junit \"$BYTEASK_RESULTS_DIR/junit.xml\"";
    if let Some(presets) = read_presets(root) {
        let configure = pick_preset(&presets, "configurePresets");
        let build = pick_preset(&presets, "buildPresets").or_else(|| configure.clone());
        let test = pick_preset(&presets, "testPresets").or_else(|| configure.clone());
        if let (Some(configure), Some(build), Some(test)) = (configure, build, test) {
            return Proposal {
                source: format!("CMakePresets.json (presets {configure}, {build}, {test})"),
                text: render_proposal(
                    &[
                        format!("cmake --preset {configure}"),
                        format!("cmake --build --preset {build}"),
                    ],
                    &[format!("ctest --preset {test} {junit}")],
                ),
            };
        }
    }
    if let Some(dir) = find_compile_db_build_tree(root) {
        return Proposal {
            source: format!("compile_commands.json in the CMake build tree {dir}"),
            text: render_proposal(
                &[format!("cmake --build {dir}")],
                &[format!("ctest --test-dir {dir} {junit}")],
            ),
        };
    }
    if root.join("CMakeLists.txt").is_file() {
        let dir = "build/byteask-ci";
        return Proposal {
            source: "CMakeLists.txt".to_string(),
            text: render_proposal(
                &[
                    format!("cmake -S . -B {dir} -DCMAKE_BUILD_TYPE=RelWithDebInfo"),
                    format!("cmake --build {dir}"),
                ],
                &[format!("ctest --test-dir {dir} {junit}")],
            ),
        };
    }
    // Nothing to go on: empty command lists, so the unedited text does not parse and cannot
    // be committed by mistake as a profile whose "build" is a comment that always succeeds.
    Proposal {
        source: "nothing found (no CMake presets, compile database or CMakeLists.txt)".to_string(),
        text: render_proposal(&[], &[]),
    }
}

/// A proposed profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    /// What it was derived from, for the person reading it.
    pub source: String,
    pub text: String,
}

fn read_presets(root: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(root.join("CMakePresets.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// A visible preset, preferring the names CI usually uses.
fn pick_preset(presets: &serde_json::Value, kind: &str) -> Option<String> {
    let names: Vec<String> = presets[kind]
        .as_array()?
        .iter()
        .filter(|preset| !preset["hidden"].as_bool().unwrap_or(false))
        .filter_map(|preset| preset["name"].as_str().map(str::to_string))
        .filter(|name| is_shell_word(name))
        .collect();
    for wanted in ["ci", "release", "relwithdebinfo", "default"] {
        if let Some(name) = names.iter().find(|name| name.eq_ignore_ascii_case(wanted)) {
            return Some(name.clone());
        }
    }
    names.into_iter().next()
}

fn is_shell_word(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

fn find_compile_db_build_tree(root: &Path) -> Option<String> {
    let mut candidates = vec![root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut dirs: Vec<_> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.path())
            .collect();
        dirs.sort();
        candidates.extend(dirs);
    }
    candidates.into_iter().find_map(|dir| {
        let has =
            dir.join("compile_commands.json").is_file() && dir.join("CMakeCache.txt").is_file();
        if !has {
            return None;
        }
        let rel = dir.strip_prefix(root).ok()?.to_string_lossy().into_owned();
        let rel = if rel.is_empty() { ".".to_string() } else { rel };
        is_shell_word(&rel).then_some(rel)
    })
}

fn toml_list(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|item| toml::Value::String(item.clone()).to_string())
        .collect();
    format!("[{}]", quoted.join(", "))
}

fn render_proposal(build: &[String], test: &[String]) -> String {
    let mut text = String::from(
        "# ByteAsk check profile (proposed). Review it, then commit it to your protected branch:\n\
         # the merge check reads it from there, never from the branch being checked.\n\
         schema = 1\n\n",
    );
    text.push_str(
        "# The golden-path build: each command runs with bash -c from the repository root.\n",
    );
    text.push_str(&format!(
        "[build]\nrun = {}\ntimeout_secs = 1800\n\n",
        toml_list(build)
    ));
    text.push_str("# The golden-path tests. Reports go in $BYTEASK_RESULTS_DIR.\n");
    text.push_str(&format!(
        "[test]\nrun = {}\ntimeout_secs = 1800\njunit = \"junit.xml\"\n\
         # Tests that must run and pass (one name per line), so a branch cannot drop one:\n\
         # manifest = \".byteask/required-tests.txt\"\n\n",
        toml_list(test)
    ));
    text.push_str(
        "[files]\n\
         # Hot paths: a change here also runs the slow checks and the perf card.\n\
         marked = []\n\
         # A change touching only these skips the checks (NOT RUN, never PASS). The default\n\
         # is Markdown, reStructuredText, docs text, images and licence files:\n\
         # non_code = [\"**/*.md\", \"**/*.rst\", \"**/*.png\"]\n\n\
         [merge]\n\
         # NOT RUN reasons a reviewer may approve; everything else blocks the merge.\n\
         reviewer_may_approve = [\"no_code_change\"]\n\
         perf = \"off\"\n",
    );
    text
}

#[cfg(test)]
mod tests;
