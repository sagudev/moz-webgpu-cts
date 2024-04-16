mod metadata;
mod process_reports;
mod report;
mod shared;

use self::{
    metadata::{
        BuildProfile, File, FileProps, Platform, Subtest, SubtestOutcome, Test, TestOutcome,
        TestProps,
    },
    process_reports::{Entry, TestEntry},
    report::{
        ExecutionReport, RunInfo, SubtestExecutionResult, TestExecutionEntry, TestExecutionResult,
    },
    shared::{Expectation, FullyExpandedExpectationPropertyValue, TestPath},
};

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Debug, Display, Formatter},
    fs,
    hash::Hash,
    io::{self, BufReader, BufWriter},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        atomic::{self, AtomicBool},
        mpsc::channel,
        Arc,
    },
};

use camino::Utf8PathBuf;
use clap::{Parser, ValueEnum};
use enumset::EnumSetType;
use format::lazy_format;
use indexmap::{IndexMap, IndexSet};
use joinery::JoinableIterator;
use miette::{miette, Diagnostic, IntoDiagnostic, NamedSource, Report, SourceSpan, WrapErr};
use path_dsl::path;
use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use shared::Browser;
use wax::Glob;
use whippit::{
    metadata::SectionHeader,
    reexport::chumsky::{self, prelude::Rich},
};

#[derive(Debug, Parser)]
#[command(about, version)]
struct Cli {
    #[clap(long, alias = "gecko-checkout")]
    checkout: Option<PathBuf>,
    #[clap(value_enum, long, default_value_t = Default::default())]
    browser: Browser,
    #[clap(subcommand)]
    subcommand: Subcommand,
}

#[derive(Debug, Parser)]
enum Subcommand {
    /// Adjust test expectations in metadata, optionally using `wptreport.json` reports from CI
    /// runs covering Firefox's implementation of WebGPU.
    ///
    /// As Firefox's behavior changes, one generally expects CTS test outcomes to change. When you
    /// are testing your own changes in CI, you can use this subcommand to update expectations
    /// automatically with the following steps:
    ///
    /// 1. Run `moz-webgpu-cts process-reports --preset=new-fx …` against the first complete set of
    ///    reports you gather from CI with your new Firefox build. This will adjust for new
    ///    permanent outcomes, and may capture some (but not all) intermittent outcomes.
    ///
    /// 2. There may still exist intermittent issues that you do not discover in CI run(s) from the
    ///    previous step. As you discover them in further CI runs on the same build of Firefox,
    ///    adjust expected outcomes to match by running `moz-webgpu-cts process-reports
    ///    --preset=same-fx …` against the runs' new reports. Repeat as necessary.
    ///
    /// With both steps, you may delete the local copies of these reports after being processed
    /// with `process-reports`. You should not need to re-process them unless you have made an
    /// error in following these steps.
    #[clap(alias = "process-reports")]
    UpdateExpected {
        /// Direct paths to report files to be processed.
        report_paths: Vec<PathBuf>,
        /// Cross-platform `wax` globs to enumerate report files to be processed.
        ///
        /// N.B. for Windows users: backslashes are used strictly for escaped characters, and
        /// forward slashes (`/`) are the only valid path separator for these globs.
        #[clap(long = "glob", value_name = "REPORT_GLOB")]
        report_globs: Vec<String>,
        /// The heuristic for resolving differences between current metadata and processed reports.
        #[clap(long, default_value = "reset-contradictory")]
        preset: ReportProcessingPreset,
    },
    /// Parse test metadata, apply automated fixups, and re-emit it in normalized form.
    #[clap(name = "fixup", alias = "fmt")]
    Fixup,
    Triage {
        #[clap(value_enum, long, default_value_t = Default::default())]
        on_zero_item: OnZeroItem,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ReportProcessingPreset {
    /// alias: `new-fx`
    #[value(alias("new-fx"))]
    ResetContradictory,
    /// alias: `same-fx`
    #[value(alias("same-fx"))]
    Merge,
    ResetAll,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum OnZeroItem {
    Show,
    #[default]
    Hide,
}

fn main() -> ExitCode {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();
    run(Cli::parse())
}

fn run(cli: Cli) -> ExitCode {
    let Cli {
        browser,
        checkout,
        subcommand,
    } = cli;

    let checkout = match checkout.map(Ok).unwrap_or_else(search_for_moz_central_ckt) {
        Ok(ckt_path) => ckt_path,
        Err(AlreadyReportedToCommandline) => return ExitCode::FAILURE,
    };

    let read_metadata = || -> Result<_, AlreadyReportedToCommandline> {
        let webgpu_cts_meta_parent_dir = match browser {
            Browser::Firefox => {
                path!(&checkout | "testing" | "web-platform" | "mozilla" | "meta" | "webgpu")
            }
            Browser::Servo => path!(&checkout | "tests" | "wpt" | "webgpu" | "meta" | "webgpu"),
        };

        let mut found_err = false;
        let collected = read_files_at(&checkout, &webgpu_cts_meta_parent_dir, "**/*.ini")?
            .filter_map(|res| match res {
                Ok((p, _contents)) if p.ends_with("__dir__.ini") => None,
                Ok(ok) => Some(ok),
                Err(AlreadyReportedToCommandline) => {
                    found_err = true;
                    None
                }
            })
            .map(|(p, fc)| (Arc::new(p), Arc::new(fc)))
            .collect::<IndexMap<_, _>>();
        if found_err {
            Err(AlreadyReportedToCommandline)
        } else {
            Ok(collected)
        }
    };

    fn render_metadata_parse_errors<'a>(
        path: &Arc<PathBuf>,
        file_contents: &Arc<String>,
        errors: impl IntoIterator<Item = Rich<'a, char>>,
    ) {
        #[derive(Debug, Diagnostic, thiserror::Error)]
        #[error("{inner}")]
        struct ParseError {
            #[label]
            span: SourceSpan,
            #[source_code]
            source_code: NamedSource,
            inner: Rich<'static, char>,
        }
        let source_code = file_contents.clone();
        for error in errors {
            let span = error.span();
            let error = ParseError {
                source_code: NamedSource::new(path.to_str().unwrap(), source_code.clone()),
                inner: error.clone().into_owned(),
                span: SourceSpan::new(span.start.into(), (span.end - span.start).into()),
            };
            let error = Report::new(error);
            eprintln!("{error:?}");
        }
    }

    match subcommand {
        Subcommand::UpdateExpected {
            report_globs,
            report_paths,
            preset,
        } => {
            let report_globs = {
                let mut found_glob_parse_err = false;
                let globs = report_globs
                    .into_iter()
                    .filter_map(|glob| match Glob::diagnosed(&glob) {
                        Ok((glob, _diagnostics)) => Some(glob.into_owned().partition()),
                        Err(diagnostics) => {
                            found_glob_parse_err = true;
                            let error_reports = diagnostics
                                .into_iter()
                                .filter(|diag| {
                                    // N.B.: There should be at least one of these!
                                    diag.severity()
                                        .map_or(true, |sev| sev == miette::Severity::Error)
                                })
                                .map(Report::new_boxed);
                            for report in error_reports {
                                eprintln!("{report:?}");
                            }
                            None
                        }
                    })
                    .collect::<Vec<_>>();

                if found_glob_parse_err {
                    log::error!("failed to parse one or more WPT report globs; bailing");
                    return ExitCode::FAILURE;
                }

                globs
            };

            let report_paths_from_glob = {
                let mut found_glob_walk_err = false;
                let files = report_globs
                    .iter()
                    .flat_map(|(base_path, glob)| {
                        glob.walk(base_path)
                            .filter_map(|entry| match entry {
                                Ok(entry) => Some(entry.into_path()),
                                Err(e) => {
                                    found_glob_walk_err = true;
                                    let ctx_msg = if let Some(path) = e.path() {
                                        format!(
                                            "failed to enumerate files for glob `{}` at path {}",
                                            glob,
                                            path.display()
                                        )
                                    } else {
                                        format!("failed to enumerate files for glob `{glob}`")
                                    };
                                    let e = Report::msg(e).wrap_err(ctx_msg);
                                    eprintln!("{e:?}");
                                    None
                                }
                            })
                            .collect::<Vec<_>>() // OPT: Can we get rid of this somehow?
                    })
                    .collect::<Vec<_>>();

                if found_glob_walk_err {
                    log::error!(concat!(
                        "failed to enumerate files with WPT report globs, ",
                        "see above for more details"
                    ));
                    return ExitCode::FAILURE;
                }

                files
            };

            if report_paths_from_glob.is_empty() && !report_globs.is_empty() {
                if report_paths.is_empty() {
                    log::error!(concat!(
                        "reports were specified exclusively via glob search, ",
                        "but none were found; bailing"
                    ));
                    return ExitCode::FAILURE;
                } else {
                    log::warn!(concat!(
                        "report were specified via path and glob search, ",
                        "but none were found via glob; ",
                        "continuing with report paths"
                    ))
                }
            }

            let exec_report_paths = report_paths
                .into_iter()
                .chain(report_paths_from_glob)
                .collect::<Vec<_>>();

            log::trace!("working with the following WPT report files: {exec_report_paths:#?}");
            log::info!("working with {} WPT report files", exec_report_paths.len());

            let meta_files_by_path = {
                let raw_meta_files_by_path = match read_metadata() {
                    Ok(paths) => paths,
                    Err(AlreadyReportedToCommandline) => return ExitCode::FAILURE,
                };

                log::info!("parsing metadata…");
                let mut found_parse_err = false;

                let files = raw_meta_files_by_path
                    .into_iter()
                    .filter_map(|(path, file_contents)| {
                        match chumsky::Parser::parse(&File::parser(), &*file_contents).into_result()
                        {
                            Err(errors) => {
                                found_parse_err = true;
                                render_metadata_parse_errors(&path, &file_contents, errors);
                                None
                            }
                            Ok(file) => Some((path, file)),
                        }
                    })
                    .collect::<IndexMap<_, _>>();

                if found_parse_err {
                    log::error!(concat!(
                        "found one or more failures while parsing metadata, ",
                        "see above for more details"
                    ));
                    return ExitCode::FAILURE;
                }

                files
            };

            #[derive(Debug, Default)]
            struct EntryByCtsPath<'a> {
                metadata_path: Option<TestPath<'a>>,
                reported_path: Option<TestPath<'a>>,
                entry: TestEntry,
            }

            fn cts_path(test_path: &TestPath<'_>) -> Option<String> {
                test_path
                    .variant
                    .as_ref()
                    .filter(|v| v.starts_with("?q=webgpu:"))
                    .map(|v| v.strip_prefix("?q=").unwrap().to_owned())
                    .filter(|_q| test_path.path.ends_with("cts.https.html"))
            }

            let mut file_props_by_file = IndexMap::<Utf8PathBuf, FileProps>::default();
            let mut entries_by_cts_path = IndexMap::<String, EntryByCtsPath<'_>>::default();
            let mut other_entries_by_test = IndexMap::<TestPath<'_>, TestEntry>::default();
            let old_meta_file_paths = meta_files_by_path.keys().cloned().collect::<Vec<_>>();

            log::info!("loading metadata for comparison to reports…");
            for (path, file) in meta_files_by_path {
                let File { properties, tests } = file;

                let file_rel_path = path.strip_prefix(&checkout).unwrap();

                file_props_by_file.insert(
                    Utf8PathBuf::from(file_rel_path.to_str().unwrap()),
                    properties,
                );

                for (SectionHeader(name), test) in tests {
                    let Test {
                        properties,
                        subtests,
                    } = test;

                    let test_path = TestPath::from_metadata_test(file_rel_path, &name).unwrap();

                    let freak_out_do_nothing = |what: &dyn Display| {
                        log::error!("hoo boy, not sure what to do yet: {what}")
                    };

                    let mut reported_dupe_already = false;
                    let mut dupe_err = || {
                        if !reported_dupe_already {
                            freak_out_do_nothing(&format_args!(
                                concat!(
                                    "duplicate entry for {:?}",
                                    "discarding previous entries with ",
                                    "this and further dupes"
                                ),
                                test_path
                            ))
                        }
                        reported_dupe_already = true;
                    };

                    let TestEntry {
                        entry: test_entry,
                        subtests: subtest_entries,
                    } = if let Some(cts_path) = cts_path(&test_path) {
                        let entry = entries_by_cts_path.entry(cts_path).or_default();
                        if let Some(_old) =
                            entry.metadata_path.replace(test_path.clone().into_owned())
                        {
                            dupe_err();
                        }
                        &mut entry.entry
                    } else {
                        other_entries_by_test
                            .entry(test_path.clone().into_owned())
                            .or_default()
                    };

                    let test_path = &test_path;

                    if let Some(_old) = test_entry.meta_props.replace(properties) {
                        dupe_err();
                    }

                    for (SectionHeader(subtest_name), subtest) in subtests {
                        let Subtest { properties } = subtest;
                        let subtest_entry =
                            subtest_entries.entry(subtest_name.clone()).or_default();
                        if let Some(_old) = subtest_entry.meta_props.replace(properties) {
                            if !reported_dupe_already {
                                freak_out_do_nothing(&format_args!(
                                    concat!(
                                        "duplicate subtest in {:?} named {:?}, ",
                                        "discarding previous entries with ",
                                        "this and further dupes"
                                    ),
                                    test_path, subtest_name
                                ));
                            }
                        }
                    }
                }
            }

            log::info!("gathering reported test outcomes for reconciliation with metadata…");

            let using_reports = !exec_report_paths.is_empty();

            let (exec_reports_sender, exec_reports_receiver) = channel();
            exec_report_paths
                .into_par_iter()
                .for_each_with(exec_reports_sender, |sender, path| {
                    let res = fs::File::open(&path)
                        .map(BufReader::new)
                        .map_err(Report::msg)
                        .wrap_err("failed to open file")
                        .and_then(|reader| {
                            serde_json::from_reader::<_, ExecutionReport>(reader)
                                .into_diagnostic()
                                .wrap_err("failed to parse JSON")
                        })
                        .wrap_err_with(|| {
                            format!(
                                "failed to read WPT execution report from {}",
                                path.display()
                            )
                        })
                        .map(|parsed| (path, parsed))
                        .map_err(|e| {
                            log::error!("{e:?}");
                            AlreadyReportedToCommandline
                        });
                    let _ = sender.send(res);
                });

            for res in exec_reports_receiver {
                let (_path, exec_report) = match res {
                    Ok(ok) => ok,
                    Err(AlreadyReportedToCommandline) => return ExitCode::FAILURE,
                };

                let ExecutionReport {
                    run_info:
                        RunInfo {
                            platform,
                            build_profile,
                        },
                    entries,
                } = exec_report;

                for entry in entries {
                    let TestExecutionEntry { test_name, result } = entry;

                    let test_path = TestPath::from_execution_report(&test_name, browser).unwrap();
                    let TestEntry {
                        entry: test_entry,
                        subtests: subtest_entries,
                    } = if let Some(cts_path) = cts_path(&test_path) {
                        let entry = entries_by_cts_path.entry(cts_path).or_default();
                        if let Some(old) =
                            entry.reported_path.replace(test_path.clone().into_owned())
                        {
                            if old != test_path {
                                log::warn!(
                                    concat!(
                                        "found test execution entry containing the same ",
                                        "CTS test path as another, ",
                                        "discarding previous entries with ",
                                        "this and further dupes; entries:\n",
                                        "older: {:#?}\n",
                                        "newer: {:#?}\n",
                                    ),
                                    old,
                                    test_path
                                )
                            }
                        }
                        &mut entry.entry
                    } else {
                        other_entries_by_test
                            .entry(test_path.clone().into_owned())
                            .or_default()
                    };

                    let (reported_outcome, reported_subtests) = match result {
                        TestExecutionResult::Complete { outcome, subtests } => (outcome, subtests),
                        TestExecutionResult::JobMaybeTimedOut { status, subtests } => {
                            if !status.is_empty() {
                                log::warn!(
                                    concat!(
                                        "expected an empty `status` field for {:?}, ",
                                        "but found the {:?} status"
                                    ),
                                    test_path,
                                    status,
                                )
                            }
                            (TestOutcome::Timeout, subtests)
                        }
                    };

                    fn accumulate<Out>(
                        recorded: &mut BTreeMap<Platform, BTreeMap<BuildProfile, Expectation<Out>>>,
                        platform: Platform,
                        build_profile: BuildProfile,
                        reported_outcome: Out,
                    ) where
                        Out: Default + EnumSetType + Hash,
                    {
                        match recorded.entry(platform).or_default().entry(build_profile) {
                            std::collections::btree_map::Entry::Vacant(entry) => {
                                entry.insert(Expectation::permanent(reported_outcome));
                            }
                            std::collections::btree_map::Entry::Occupied(mut entry) => {
                                *entry.get_mut() |= reported_outcome
                            }
                        }
                    }
                    accumulate(
                        &mut test_entry.reported,
                        platform,
                        build_profile,
                        reported_outcome,
                    );

                    for reported_subtest in reported_subtests {
                        let SubtestExecutionResult {
                            subtest_name,
                            outcome,
                        } = reported_subtest;

                        accumulate(
                            &mut subtest_entries
                                .entry(subtest_name.clone())
                                .or_default()
                                .reported,
                            platform,
                            build_profile,
                            outcome,
                        );
                    }
                }
            }

            log::info!("metadata and reports gathered, now reconciling outcomes…");

            let mut found_reconciliation_err = false;
            let entries_by_cts_path = entries_by_cts_path.into_iter().map(|(_name, entry)| {
                let EntryByCtsPath {
                    metadata_path,
                    reported_path,
                    entry,
                } = entry;
                let output_path = if let Some((meta, rep)) = metadata_path
                    .as_ref()
                    .zip(reported_path.as_ref())
                    .filter(|(meta, rep)| meta != rep)
                {
                    log::info!(
                        concat!(
                            "metadata path for test is different from ",
                            "reported execution; relocating…\n",
                            "…metadata: {:#?}\n",
                            "…reported: {:#?}\n"
                        ),
                        meta,
                        rep
                    );
                    reported_path
                } else {
                    metadata_path.or(reported_path)
                };

                (
                    output_path.expect(concat!(
                        "internal error: CTS path entry created without at least one ",
                        "report or metadata path specified"
                    )),
                    entry,
                )
            });
            let recombined_tests_iter = entries_by_cts_path
                .chain(other_entries_by_test)
                .filter_map(|(test_path, test_entry)| {
                    fn reconcile<Out>(
                        entry: Entry<Out>,
                        preset: ReportProcessingPreset,
                    ) -> TestProps<Out>
                    where
                        Out: Debug + Default + EnumSetType,
                    {
                        let Entry {
                            meta_props,
                            reported,
                        } = entry;

                        let mut meta_props = meta_props.unwrap_or_default();
                        let reconciled = 'resolve: {
                            let reported = |platform, build_profile| {
                                reported
                                    .get(&platform)
                                    .and_then(|rep| rep.get(&build_profile))
                                    .copied()
                            };
                            let all_reported = || {
                                FullyExpandedExpectationPropertyValue::from_query(
                                    |platform, build_profile| {
                                        reported(platform, build_profile).unwrap_or_default()
                                    },
                                )
                            };
                            let resolve = match preset {
                                ReportProcessingPreset::ResetAll => {
                                    break 'resolve all_reported();
                                }
                                ReportProcessingPreset::ResetContradictory => {
                                    |meta: Expectation<_>, rep: Option<Expectation<_>>| {
                                        rep.filter(|rep| !meta.is_superset(rep)).unwrap_or(meta)
                                    }
                                }
                                ReportProcessingPreset::Merge => |meta, rep| match rep {
                                    Some(rep) => meta | rep,
                                    None => meta,
                                },
                            };

                            if let Some(meta_expectations) = meta_props.expectations {
                                FullyExpandedExpectationPropertyValue::from_query(
                                    |platform, build_profile| {
                                        resolve(
                                            meta_expectations.get(platform, build_profile),
                                            reported(platform, build_profile),
                                        )
                                    },
                                )
                            } else {
                                all_reported()
                            }
                        };
                        meta_props.expectations = Some(reconciled);
                        meta_props
                    }

                    let TestEntry {
                        entry: test_entry,
                        subtests: subtest_entries,
                    } = test_entry;

                    if test_entry.meta_props.is_none() {
                        log::info!("new test entry: {test_path:?}")
                    }

                    if test_entry.reported.is_empty() && using_reports {
                        let test_path = &test_path;
                        let msg = lazy_format!("no entries found in reports for {:?}", test_path);
                        match preset {
                            ReportProcessingPreset::Merge => log::warn!("{msg}"),
                            ReportProcessingPreset::ResetAll
                            | ReportProcessingPreset::ResetContradictory => {
                                log::warn!("removing metadata after {msg}");
                                return None;
                            }
                        }
                    }

                    let properties = reconcile(test_entry, preset);

                    let mut subtests = BTreeMap::new();
                    for (subtest_name, subtest) in subtest_entries {
                        let subtest_name = SectionHeader(subtest_name);
                        if subtests.contains_key(&subtest_name) {
                            found_reconciliation_err = true;
                            log::error!("internal error: duplicate test path {test_path:?}");
                        }

                        let mut properties = reconcile(subtest, preset);

                        for (_, expected) in properties.expectations.as_mut().unwrap().iter_mut() {
                            taint_subtest_timeouts_by_suspicion(expected);
                        }

                        subtests.insert(subtest_name, Subtest { properties });
                    }

                    if subtests.is_empty() && properties == Default::default() {
                        None
                    } else {
                        Some((test_path, (properties, subtests)))
                    }
                });

            log::info!(
                "outcome reconciliation complete, gathering tests back into new metadata files…"
            );

            let mut files = BTreeMap::<PathBuf, File>::new();
            for (test_path, (properties, subtests)) in recombined_tests_iter {
                let name = test_path.test_name().to_string();
                let rel_path = Utf8PathBuf::from(test_path.rel_metadata_path().to_string());
                let path = checkout.join(&rel_path);
                let file = files.entry(path).or_insert_with(|| File {
                    properties: file_props_by_file
                        .get(&rel_path)
                        .cloned()
                        .unwrap_or_else(|| {
                            log::warn!("creating new metadata file for `{rel_path}`");
                            Default::default()
                        }),
                    tests: Default::default(),
                });
                file.tests.insert(
                    SectionHeader(name),
                    Test {
                        properties,
                        subtests,
                    },
                );
            }

            for old_meta_file_path in old_meta_file_paths {
                files
                    .entry(Arc::into_inner(old_meta_file_path).unwrap())
                    .or_default();
            }

            files.retain(|path, file| {
                let is_empty = file.tests.is_empty();
                if is_empty {
                    log::info!("removing now-empty metadata file {}", path.display());
                    match fs::remove_file(path) {
                        Ok(()) => (),
                        Err(e) => match e.kind() {
                            io::ErrorKind::NotFound => (),
                            _ => log::error!(
                                "failed to remove now-empty metadata file {}",
                                path.display()
                            ),
                        },
                    }
                }
                !is_empty
            });

            log::info!("gathering of new metadata files completed, writing to file system…");

            for (path, file) in files {
                log::debug!("writing new metadata to {}", path.display());
                match write_to_file(&path, metadata::format_file(&file)) {
                    Ok(()) => (),
                    Err(AlreadyReportedToCommandline) => {
                        found_reconciliation_err = true;
                    }
                }
            }

            if found_reconciliation_err {
                log::error!(concat!(
                    "one or more errors found while reconciling, ",
                    "exiting with failure; see above for more details"
                ));
                return ExitCode::FAILURE;
            }

            ExitCode::SUCCESS
        }
        Subcommand::Fixup => {
            let raw_test_files_by_path = match read_metadata() {
                Ok(paths) => paths,
                Err(AlreadyReportedToCommandline) => return ExitCode::FAILURE,
            };
            log::info!("formatting metadata in-place…");
            let mut err_found = false;
            for (path, file_contents) in raw_test_files_by_path {
                match chumsky::Parser::parse(&File::parser(), &*file_contents).into_result() {
                    Err(errors) => {
                        err_found = true;
                        render_metadata_parse_errors(&path, &file_contents, errors);
                    }
                    Ok(mut file) => {
                        for test in file.tests.values_mut() {
                            for subtest in &mut test.subtests.values_mut() {
                                if let Some(expected) = subtest.properties.expectations.as_mut() {
                                    for (_, expected) in expected.iter_mut() {
                                        taint_subtest_timeouts_by_suspicion(expected);
                                    }
                                }
                            }
                        }

                        match write_to_file(&path, metadata::format_file(&file)) {
                            Ok(()) => (),
                            Err(AlreadyReportedToCommandline) => {
                                err_found = true;
                            }
                        };
                    }
                }
            }

            if err_found {
                log::error!(concat!(
                    "found one or more failures while formatting metadata, ",
                    "see above for more details"
                ));
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Subcommand::Triage { on_zero_item } => {
            #[derive(Debug)]
            struct TaggedTest {
                #[allow(unused)]
                orig_path: Arc<PathBuf>,
                inner: Test,
            }
            let tests_by_name = {
                let mut found_parse_err = false;
                let raw_test_files_by_path = match read_metadata() {
                    Ok(paths) => paths,
                    Err(AlreadyReportedToCommandline) => return ExitCode::FAILURE,
                };
                let extracted = raw_test_files_by_path
                    .iter()
                    .filter_map(|(path, file_contents)| {
                        match chumsky::Parser::parse(&metadata::File::parser(), file_contents)
                            .into_result()
                        {
                            Ok(File {
                                properties: _,
                                tests,
                            }) => Some(tests.into_iter().map({
                                let checkout = &checkout;
                                move |(name, inner)| {
                                    let SectionHeader(name) = &name;
                                    let test_path = TestPath::from_metadata_test(
                                        path.strip_prefix(checkout).unwrap(),
                                        name,
                                    )
                                    .unwrap();
                                    let url_path = test_path.runner_url_path().to_string();
                                    (
                                        url_path,
                                        TaggedTest {
                                            inner,
                                            orig_path: path.clone(),
                                        },
                                    )
                                }
                            })),
                            Err(errors) => {
                                found_parse_err = true;
                                render_metadata_parse_errors(path, file_contents, errors);
                                None
                            }
                        }
                    })
                    .flatten()
                    .collect::<BTreeMap<_, _>>();
                if found_parse_err {
                    log::error!(concat!(
                        "found one or more failures while parsing metadata, ",
                        "see above for more details"
                    ));
                    return ExitCode::FAILURE;
                }
                extracted
            };

            log::info!(concat!(
                "finished parsing of interesting properties ",
                "from metadata files, analyzing results…"
            ));

            #[derive(Clone, Default)]
            struct PermaAndIntermittent<T> {
                perma: T,
                intermittent: T,
            }

            impl<T> Debug for PermaAndIntermittent<T>
            where
                T: Debug,
            {
                fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                    let Self {
                        perma,
                        intermittent,
                    } = self;
                    f.debug_struct("") // the name is distracting, blank it out plz
                        .field("perma", perma)
                        .field("intermittent", intermittent)
                        .finish()
                }
            }

            impl<T> PermaAndIntermittent<T> {
                pub fn as_ref(&self) -> PermaAndIntermittent<&T> {
                    let Self {
                        perma,
                        intermittent,
                    } = self;
                    PermaAndIntermittent {
                        perma,
                        intermittent,
                    }
                }

                pub fn map<U>(self, f: impl Fn(T) -> U) -> PermaAndIntermittent<U> {
                    let Self {
                        perma,
                        intermittent,
                    } = self;
                    PermaAndIntermittent {
                        perma: f(perma),
                        intermittent: f(intermittent),
                    }
                }
            }

            type TestSet = PermaAndIntermittent<BTreeSet<Arc<String>>>;
            type SubtestByTestSet =
                PermaAndIntermittent<BTreeMap<Arc<String>, IndexSet<Arc<String>>>>;

            #[derive(Clone, Debug, Default)]
            struct PerPlatformAnalysis {
                tests_with_runner_errors: TestSet,
                tests_with_disabled_or_skip: TestSet,
                tests_with_crashes: TestSet,
                subtests_with_failures_by_test: SubtestByTestSet,
                subtests_with_timeouts_by_test: SubtestByTestSet,
            }

            #[derive(Clone, Debug, Default)]
            struct Analysis {
                windows: PerPlatformAnalysis,
                linux: PerPlatformAnalysis,
                mac_os: PerPlatformAnalysis,
            }

            impl Analysis {
                pub fn for_each_platform_mut<F>(&mut self, mut f: F)
                where
                    F: FnMut(&mut PerPlatformAnalysis),
                {
                    let Self {
                        windows,
                        linux,
                        mac_os,
                    } = self;
                    for analysis in [windows, linux, mac_os] {
                        f(analysis)
                    }
                }

                pub fn for_each_platform<F>(&self, mut f: F)
                where
                    F: FnMut(Platform, &PerPlatformAnalysis),
                {
                    let Self {
                        windows,
                        linux,
                        mac_os,
                    } = self;
                    for (platform, analysis) in [
                        (Platform::Windows, windows),
                        (Platform::Linux, linux),
                        (Platform::MacOs, mac_os),
                    ] {
                        f(platform, analysis)
                    }
                }

                pub fn for_platform_mut<F>(&mut self, platform: Platform, mut f: F)
                where
                    F: FnMut(&mut PerPlatformAnalysis),
                {
                    match platform {
                        Platform::Windows => f(&mut self.windows),
                        Platform::Linux => f(&mut self.linux),
                        Platform::MacOs => f(&mut self.mac_os),
                    }
                }
            }

            let mut analysis = Analysis::default();
            for (test_name, test) in tests_by_name {
                let TaggedTest {
                    orig_path: _,
                    inner: test,
                } = test;

                let Test {
                    properties,
                    subtests,
                } = test;

                let TestProps {
                    is_disabled,
                    expectations,
                } = properties;

                let test_name = Arc::new(test_name);

                if is_disabled {
                    analysis.for_each_platform_mut(|analysis| {
                        analysis
                            .tests_with_disabled_or_skip
                            .perma
                            .insert(test_name.clone());
                    })
                }

                fn insert_in_test_set<Out>(
                    poi: &mut TestSet,
                    test_name: &Arc<String>,
                    expectation: Expectation<Out>,
                    outcome: Out,
                ) where
                    Out: Debug + Default + EnumSetType,
                {
                    if expectation.is_superset(&Expectation::permanent(outcome)) {
                        if expectation.is_permanent() {
                            &mut poi.perma
                        } else {
                            &mut poi.intermittent
                        }
                        .insert(test_name.clone());
                    }
                }

                fn insert_in_subtest_by_test_set<Out>(
                    poi: &mut SubtestByTestSet,
                    test_name: &Arc<String>,
                    subtest_name: &Arc<String>,
                    expectation: Expectation<Out>,
                    outcome: Out,
                ) where
                    Out: Debug + Default + EnumSetType,
                {
                    if expectation.is_superset(&Expectation::permanent(outcome)) {
                        if expectation.is_permanent() {
                            &mut poi.perma
                        } else {
                            &mut poi.intermittent
                        }
                        .entry(test_name.clone())
                        .or_default()
                        .insert(subtest_name.clone());
                    }
                }

                if let Some(expectations) = expectations {
                    fn analyze_test_outcome<F>(
                        test_name: &Arc<String>,
                        expectation: Expectation<TestOutcome>,
                        mut receiver: F,
                    ) where
                        F: FnMut(&mut dyn FnMut(&mut PerPlatformAnalysis)),
                    {
                        for outcome in expectation.iter() {
                            match outcome {
                                TestOutcome::Ok => (),
                                // We skip this because this test _should_ contain subtests with
                                // `TIMEOUT` and `NOTRUN`, so we shouldn't actually miss anything.
                                TestOutcome::Timeout => (),
                                TestOutcome::Crash => receiver(&mut |analysis| {
                                    insert_in_test_set(
                                        &mut analysis.tests_with_crashes,
                                        test_name,
                                        expectation,
                                        outcome,
                                    )
                                }),
                                TestOutcome::Error => receiver(&mut |analysis| {
                                    insert_in_test_set(
                                        &mut analysis.tests_with_runner_errors,
                                        test_name,
                                        expectation,
                                        outcome,
                                    )
                                }),
                                TestOutcome::Skip => receiver(&mut |analysis| {
                                    insert_in_test_set(
                                        &mut analysis.tests_with_disabled_or_skip,
                                        test_name,
                                        expectation,
                                        outcome,
                                    )
                                }),
                            }
                        }
                    }

                    let apply_to_specific_platforms =
                        |analysis: &mut Analysis, platform, expectation| {
                            analyze_test_outcome(&test_name, expectation, |f| {
                                analysis.for_platform_mut(platform, f)
                            })
                        };

                    for ((platform, _build_profile), expectations) in expectations.iter() {
                        apply_to_specific_platforms(&mut analysis, platform, expectations)
                    }
                }

                for (subtest_name, subtest) in subtests {
                    let SectionHeader(subtest_name) = subtest_name;
                    let subtest_name = Arc::new(subtest_name);

                    let Subtest { properties } = subtest;
                    let TestProps {
                        is_disabled,
                        expectations,
                    } = properties;

                    if is_disabled {
                        analysis
                            .windows
                            .tests_with_disabled_or_skip
                            .perma
                            .insert(test_name.clone());
                    }

                    if let Some(expectations) = expectations {
                        fn analyze_subtest_outcome<Fo>(
                            test_name: &Arc<String>,
                            subtest_name: &Arc<String>,
                            expectation: Expectation<SubtestOutcome>,
                            mut receiver: Fo,
                        ) where
                            Fo: FnMut(&mut dyn FnMut(&mut PerPlatformAnalysis)),
                        {
                            for outcome in expectation.iter() {
                                match outcome {
                                    SubtestOutcome::Pass => (),
                                    SubtestOutcome::Timeout | SubtestOutcome::NotRun => {
                                        receiver(&mut |analysis| {
                                            insert_in_subtest_by_test_set(
                                                &mut analysis.subtests_with_timeouts_by_test,
                                                test_name,
                                                subtest_name,
                                                expectation,
                                                outcome,
                                            )
                                        })
                                    }
                                    SubtestOutcome::Crash => receiver(&mut |analysis| {
                                        insert_in_test_set(
                                            &mut analysis.tests_with_crashes,
                                            test_name,
                                            expectation,
                                            outcome,
                                        )
                                    }),
                                    SubtestOutcome::Fail => receiver(&mut |analysis| {
                                        insert_in_subtest_by_test_set(
                                            &mut analysis.subtests_with_failures_by_test,
                                            test_name,
                                            subtest_name,
                                            expectation,
                                            outcome,
                                        )
                                    }),
                                }
                            }
                        }

                        let apply_to_specific_platforms =
                            |analysis: &mut Analysis, platform, expectation| {
                                analyze_subtest_outcome(
                                    &test_name,
                                    &subtest_name,
                                    expectation,
                                    |f| analysis.for_platform_mut(platform, f),
                                )
                            };

                        for ((platform, _build_profile), expectations) in expectations.iter() {
                            apply_to_specific_platforms(&mut analysis, platform, expectations)
                        }
                    }
                }
            }
            log::info!("finished analysis, printing to `stdout`…");
            analysis.for_each_platform(|platform, analysis| {
                let show_zero_count_item = match on_zero_item {
                    OnZeroItem::Show => true,
                    OnZeroItem::Hide => false,
                };
                let PerPlatformAnalysis {
                    tests_with_runner_errors,
                    tests_with_disabled_or_skip,
                    tests_with_crashes,
                    subtests_with_failures_by_test,
                    subtests_with_timeouts_by_test,
                } = analysis;

                let PermaAndIntermittent {
                    perma: num_tests_with_perma_runner_errors,
                    intermittent: num_tests_with_intermittent_runner_errors,
                } = tests_with_runner_errors.as_ref().map(|tests| tests.len());

                let tests_with_perma_runner_errors = (show_zero_count_item
                    || num_tests_with_perma_runner_errors > 0)
                    .then_some(lazy_format!(
                        "{} test(s) with execution reporting permanent `ERROR`",
                        num_tests_with_perma_runner_errors,
                    ));

                let tests_with_intermittent_runner_errors = (show_zero_count_item
                    || num_tests_with_intermittent_runner_errors > 0)
                    .then_some(lazy_format!(
                        "{} test(s) with execution reporting intermittent `ERROR`",
                        num_tests_with_intermittent_runner_errors
                    ));

                let PermaAndIntermittent {
                    perma: num_tests_with_disabled,
                    intermittent: num_tests_with_intermittent_disabled,
                } = tests_with_disabled_or_skip
                    .as_ref()
                    .map(|tests| tests.len());
                let tests_with_disabled = (show_zero_count_item || num_tests_with_disabled > 0)
                    .then_some(lazy_format!(
                        "{num_tests_with_disabled} test(s) with some portion marked as `disabled`"
                    ));
                if num_tests_with_intermittent_disabled > 0 {
                    log::warn!(
                        concat!(
                            "found {} intermittent `SKIP` outcomes, which we don't understand ",
                            "yet; figure it out! The tests: {:#?}"
                        ),
                        num_tests_with_intermittent_disabled,
                        tests_with_disabled_or_skip,
                    )
                }

                let PermaAndIntermittent {
                    perma: num_tests_with_perma_crashes,
                    intermittent: num_tests_with_intermittent_crashes,
                } = tests_with_crashes.as_ref().map(|tests| tests.len());
                let tests_with_perma_crashes = (show_zero_count_item
                    || num_tests_with_perma_crashes > 0)
                    .then_some(lazy_format!(
                        "{} test(s) with some portion expecting permanent `CRASH`",
                        num_tests_with_perma_crashes
                    ));
                let tests_with_intermittent_crashes = (show_zero_count_item
                    || num_tests_with_intermittent_crashes > 0)
                    .then_some(lazy_format!(
                        "{} tests(s) with some portion expecting intermittent `CRASH`",
                        num_tests_with_intermittent_crashes
                    ));

                let PermaAndIntermittent {
                    perma: num_tests_with_perma_failures_somewhere,
                    intermittent: num_tests_with_intermittent_failures_somewhere,
                } = subtests_with_failures_by_test
                    .as_ref()
                    .map(|tests| tests.len());
                let PermaAndIntermittent {
                    perma: num_subtests_with_perma_failures_somewhere,
                    intermittent: num_subtests_with_intermittent_failures_somewhere,
                } = subtests_with_failures_by_test.as_ref().map(|tests| {
                    tests
                        .iter()
                        .flat_map(|(_name, subtests)| subtests.iter())
                        .count()
                });
                let tests_with_perma_failures = (show_zero_count_item
                    || num_tests_with_perma_failures_somewhere > 0
                    || num_subtests_with_perma_failures_somewhere > 0)
                    .then_some(lazy_format!(
                        "{} test(s) with some portion perma-`FAIL`ing, {} subtests total",
                        num_tests_with_perma_failures_somewhere,
                        num_subtests_with_perma_failures_somewhere,
                    ));
                let tests_with_intermittent_failures = (show_zero_count_item
                    || num_tests_with_intermittent_failures_somewhere > 0
                    || num_subtests_with_intermittent_failures_somewhere > 0)
                    .then_some(lazy_format!(|f| {
                        write!(
                            f,
                            concat!(
                                "{} test(s) with some portion intermittently `FAIL`ing, ",
                                "{} subtests total"
                            ),
                            num_tests_with_intermittent_failures_somewhere,
                            num_subtests_with_intermittent_failures_somewhere
                        )
                    }));

                let PermaAndIntermittent {
                    perma: num_tests_with_perma_timeouts_somewhere,
                    intermittent: num_tests_with_intermittent_timeouts_somewhere,
                } = subtests_with_timeouts_by_test
                    .as_ref()
                    .map(|tests| tests.len());
                let PermaAndIntermittent {
                    perma: num_subtests_with_perma_timeouts_somewhere,
                    intermittent: num_subtests_with_intermittent_timeouts_somewhere,
                } = subtests_with_timeouts_by_test.as_ref().map(|tests| {
                    tests
                        .iter()
                        .flat_map(|(_name, subtests)| subtests.iter())
                        .count()
                });
                let tests_with_perma_timeouts_somewhere = (show_zero_count_item
                    || num_tests_with_perma_timeouts_somewhere > 0)
                    .then_some(lazy_format!(|f| {
                        write!(
                            f,
                            concat!(
                                "{} test(s) with some portion returning permanent ",
                                "`TIMEOUT`/`NOTRUN`, {} subtests total"
                            ),
                            num_tests_with_perma_timeouts_somewhere,
                            num_subtests_with_perma_timeouts_somewhere
                        )
                    }));
                let tests_with_intermittent_timeouts_somewhere = (show_zero_count_item
                    || num_tests_with_intermittent_timeouts_somewhere > 0)
                    .then_some(lazy_format!(|f| {
                        write!(
                            f,
                            concat!(
                                "{} test(s) with some portion intermittently returning ",
                                "`TIMEOUT`/`NOTRUN`, {} subtest(s) total",
                            ),
                            num_tests_with_intermittent_timeouts_somewhere,
                            num_subtests_with_intermittent_timeouts_somewhere
                        )
                    }));

                fn priority_section<'a, const SIZE: usize>(
                    name: &'static str,
                    items: [Option<&'a dyn Display>; SIZE],
                ) -> Option<Box<dyn Display + 'a>> {
                    items.iter().any(Option::is_some).then(move || {
                        Box::new(lazy_format!(move |f| {
                            let items = items
                                .iter()
                                .filter_map(|opt| *opt)
                                .map(|item| lazy_format!("\n    {item}"))
                                .join_with("");
                            write!(f, "\n  {name} PRIORITY:{items}")
                        })) as Box<dyn Display>
                    })
                }
                fn item<T>(item: Option<&T>) -> Option<&dyn Display>
                where
                    T: Display,
                {
                    item.map(|disp| disp as &dyn Display)
                }
                let sections = [
                    priority_section(
                        "HIGH",
                        [
                            item(tests_with_perma_runner_errors.as_ref()),
                            item(tests_with_disabled.as_ref()),
                            item(tests_with_perma_crashes.as_ref()),
                        ],
                    ),
                    priority_section(
                        "MEDIUM",
                        [
                            item(tests_with_perma_failures.as_ref()),
                            item(tests_with_perma_timeouts_somewhere.as_ref()),
                            item(tests_with_intermittent_crashes.as_ref()),
                            item(tests_with_intermittent_runner_errors.as_ref()),
                        ],
                    ),
                    priority_section(
                        "LOW",
                        [
                            item(tests_with_intermittent_timeouts_somewhere.as_ref()),
                            item(tests_with_intermittent_failures.as_ref()),
                        ],
                    ),
                ];
                let sections = sections.iter().filter_map(Option::as_ref).join_with("");
                println!("{platform:?}:{sections}")
            });
            println!("Full analysis: {analysis:#?}");
            ExitCode::SUCCESS
        }
    }
}

/// Returns a "naturally" sorted list of files found by searching for `glob_pattern` in `base`.
/// `checkout` is stripped as a prefix from the absolute paths recorded into `log` entries
/// emitted by this function.
///
/// # Returns
///
/// An iterator over [`Result`]s containing either a checkout file's path and contents as a UTF-8
/// string, or the sentinel of an error encountered for the same file that is already reported to
/// the command line.
///
/// # Panics
///
/// This function will panick if `checkout` cannot be stripped as a prefix of `base`.
fn read_files_at(
    checkout: &Path,
    base: &Path,
    glob_pattern: &str,
) -> Result<
    impl Iterator<Item = Result<(PathBuf, String), AlreadyReportedToCommandline>>,
    AlreadyReportedToCommandline,
> {
    log::info!("reading {glob_pattern} files at {}", base.display());
    let mut found_read_err = false;
    let mut paths = Glob::new(glob_pattern)
        .unwrap()
        .walk(base)
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry.path().to_owned()),
            Err(e) => {
                let path_disp = e
                    .path()
                    .map(|p| format!(" in {}", p.strip_prefix(checkout).unwrap().display()));
                let path_disp: &dyn Display = match path_disp.as_ref() {
                    Some(disp) => disp,
                    None => &"",
                };
                log::error!(
                    "failed to enumerate {glob_pattern} files{}\n  caused by: {e}",
                    path_disp
                );
                found_read_err = true;
                None
            }
        })
        .collect::<Vec<_>>();

    paths.sort_by(|a, b| natord::compare(a.to_str().unwrap(), b.to_str().unwrap()));
    let paths = paths;

    log::debug!(
        "working with these files: {:#?}",
        paths
            .iter()
            .map(|f| f.strip_prefix(checkout).unwrap())
            .collect::<std::collections::BTreeSet<_>>()
    );

    if found_read_err {
        return Err(AlreadyReportedToCommandline);
    }

    Ok(paths.into_iter().map(|path| -> Result<_, _> {
        log::debug!("reading from {}…", path.display());
        fs::read_to_string(&path)
            .map_err(|e| {
                log::error!("failed to read {path:?}: {e}");
                AlreadyReportedToCommandline
            })
            .map(|file_contents| (path, file_contents))
    }))
}

/// Search for a `mozilla-central` checkout either via Mercurial or Git, iterating from the CWD to
/// its parent directories.
///
/// This function reports to `log` automatically, so no meaningful [`Err`] value is returned.
fn search_for_moz_central_ckt() -> Result<PathBuf, AlreadyReportedToCommandline> {
    use lets_find_up::{find_up_with, FindUpKind, FindUpOptions};

    let find_up_opts = || FindUpOptions {
        cwd: Path::new("."),
        kind: FindUpKind::Dir,
    };
    let find_up = |repo_tech_name, root_dir_name| {
        log::debug!("searching for {repo_tech_name} checkout of `mozilla-central`…");
        let err = || {
            miette!(
                "failed to find a {} repository ({:?}) in {}",
                repo_tech_name,
                root_dir_name,
                "any of current working directory and its parent directories",
            )
        };
        find_up_with(root_dir_name, find_up_opts())
            .map_err(Report::msg)
            .wrap_err_with(err)
            .and_then(|loc_opt| loc_opt.ok_or_else(err))
            .map(|mut dir| {
                dir.pop();
                dir
            })
    };
    let gecko_source_root =
        find_up("Mercurial", ".hg").or_else(|e| match find_up("Git", ".git") {
            Ok(path) => {
                log::debug!("{e:?}");
                Ok(path)
            }
            Err(e2) => {
                log::warn!("{e:?}");
                log::warn!("{e2:?}");
                log::error!("failed to find a Gecko repository root");
                Err(AlreadyReportedToCommandline)
            }
        })?;

    log::info!(
        "detected Gecko repository root at {}",
        gecko_source_root.display()
    );

    Ok(gecko_source_root)
}

struct AlreadyReportedToCommandline;

fn write_to_file(path: &Path, contents: impl Display) -> Result<(), AlreadyReportedToCommandline> {
    let report_to_cmd_line = |e| {
        log::error!("{e}");
        AlreadyReportedToCommandline
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(Report::msg)
            .wrap_err_with(|| {
                format!(
                    "error while ensuring parent directories exist for `{}`",
                    path.display()
                )
            })
            .map_err(report_to_cmd_line)?;
    }
    let mut out = fs::File::create(path)
        .map(BufWriter::new)
        .map_err(Report::msg)
        .wrap_err_with(|| format!("error while creating new file at `{}`", path.display()))
        .map_err(report_to_cmd_line)?;
    use io::Write;
    write!(&mut out, "{contents}")
        .map_err(Report::msg)
        .wrap_err_with(|| format!("error while writing to `{}`", path.display()))
        .map_err(report_to_cmd_line)
}

/// Ensure that _both_ `TIMEOUT` and `NOTRUN` are in outcomes if at least one of them are present.
///
/// This transformation is desirable for reaching convergence quickly in tests where it may require
/// a high number of test runs to empirically observe all places where `TIMEOUT`s may occur. The
/// motivating example in Firefox's test runs are tests with a large matrix of subtests that are
/// deterministic if executed, but consistently exceed the timeout window offered by the test
/// runner.
fn taint_subtest_timeouts_by_suspicion(expected: &mut Expectation<SubtestOutcome>) {
    static PRINTED_WARNING: AtomicBool = AtomicBool::new(false);
    let already_printed_warning = PRINTED_WARNING.swap(true, atomic::Ordering::Relaxed);
    if !already_printed_warning {
        log::info!("encountered at least one case where taint-by-suspicion is being applied…")
    }
    if !expected.is_disjoint(SubtestOutcome::Timeout | SubtestOutcome::NotRun) {
        *expected |= SubtestOutcome::Timeout | SubtestOutcome::NotRun;
    }
}
