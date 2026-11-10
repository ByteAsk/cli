//! `byteask toolbox report`: the local value report.
//!
//! Reads only the local results store under `$BYTEASK_HOME`. It loads no
//! config, signs in to nothing and opens no network connection (a test runs
//! it with every socket call fatal), so it works the same offline and in a
//! company network.

use std::path::Path;

use byteask_policy::ResultsStoreLimits;
use results_store::ValueReport;

#[derive(Debug, Clone, Default, clap::Args)]
pub struct ReportArgs {
    /// Print the counts as JSON, the form to share. It holds counts only:
    /// no code, file names or branch names.
    #[arg(long)]
    pub json: bool,
    /// Only the last N days (default: everything kept on this machine).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..=3650))]
    pub days: Option<u64>,
}

/// The report for the store under `home`, as the command prints it.
pub fn render(home: &Path, args: &ReportArgs, now: u64) -> String {
    let (limits, _problems): (ResultsStoreLimits, _) = byteask_policy::results_store_limits(home);
    let loaded = results_store::store::load(&results_store::store::root(home));
    let since = args.days.map(|days| now.saturating_sub(days * 86_400));
    let report = ValueReport::build(&loaded, now, since);
    if args.json {
        let mut text = serde_json::to_string_pretty(&report.to_json()).unwrap_or_default();
        text.push('\n');
        text
    } else {
        report.render_text(&limits)
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
