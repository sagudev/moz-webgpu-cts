use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt::{self, Debug, Display, Formatter},
    num::NonZeroUsize,
    ops::{BitOr, BitOrAssign, Index, IndexMut},
    path::Path,
};

use camino::{Utf8Component, Utf8Path};

use clap::ValueEnum;
use enum_map::EnumMap;
use enumset::{EnumSet, EnumSetType};
use format::lazy_format;
use joinery::JoinableIterator;
use strum::IntoEnumIterator;

use crate::metadata::{BuildProfile, Platform};

/// A non-empty set of expected outcomes in a [`Test`] or [`Subtest`].
///
/// The default test expectation is a "good" outcome, where testing passes. The `Out` type
/// parameter should return this value in its implementation of `Default`.
///
/// [`Test`]: crate::metadata::Test
/// [`Subtest`]: crate::metadata::Subtest
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Expectation<Out>(EnumSet<Out>)
where
    Out: EnumSetType;

impl<Out> Default for Expectation<Out>
where
    Out: Default + EnumSetType,
{
    fn default() -> Self {
        Self::permanent(Out::default())
    }
}

impl<Out> Expectation<Out>
where
    Out: EnumSetType,
{
    /// Returns [`None`] if `outcomes` is empty.
    #[track_caller]
    pub fn new(outcomes: EnumSet<Out>) -> Option<Self> {
        (!outcomes.is_empty()).then_some(Self(outcomes))
    }

    pub fn permanent(outcome: Out) -> Self {
        Self(EnumSet::from_iter([outcome]))
    }

    #[track_caller]
    pub fn intermittent(outcomes: EnumSet<Out>) -> Option<Self> {
        if outcomes.len() <= 1 {
            None
        } else {
            Some(Self::new(outcomes).unwrap())
        }
    }

    fn inner(&self) -> &EnumSet<Out> {
        let Self(inner) = self;
        inner
    }

    pub fn len(&self) -> NonZeroUsize {
        self.inner()
            .len()
            .try_into()
            .expect("invariant violation: empty `Expectation`")
    }

    pub fn is_permanent(&self) -> bool {
        self.len().get() == 1
    }

    pub fn as_permanent(&self) -> Option<Out> {
        self.is_permanent().then(|| self.iter().next().unwrap())
    }

    pub fn iter(&self) -> impl Iterator<Item = Out> {
        self.inner().iter()
    }

    pub fn is_disjoint(&self, rep: EnumSet<Out>) -> bool {
        self.inner().is_disjoint(rep)
    }

    pub fn is_superset(&self, rep: &Expectation<Out>) -> bool
    where
        Out: std::fmt::Debug + Default + EnumSetType,
    {
        self.inner().is_superset(*rep.inner())
    }
}

impl<Out> Display for Expectation<Out>
where
    Out: Display + EnumSetType,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(perma) = self.as_permanent() {
            write!(f, "{perma}")
        } else {
            f.debug_list()
                .entries(
                    self.iter()
                        .map(|out| format::Debug(move |f| write!(f, "{out}"))),
                )
                .finish()
        }
    }
}

impl<Out> Debug for Expectation<Out>
where
    Out: Debug + EnumSetType,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<Out> BitOr for Expectation<Out>
where
    Out: EnumSetType,
{
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        let Self(lhs) = self;
        let Self(rhs) = rhs;

        Self(lhs | rhs)
    }
}

impl<Out> BitOrAssign for Expectation<Out>
where
    Out: EnumSetType,
{
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}

impl<Out> BitOr<EnumSet<Out>> for Expectation<Out>
where
    Out: EnumSetType,
{
    type Output = Self;

    fn bitor(self, rhs: EnumSet<Out>) -> Self::Output {
        let Self(lhs) = self;

        Self(lhs | rhs)
    }
}

impl<Out> BitOrAssign<EnumSet<Out>> for Expectation<Out>
where
    Out: EnumSetType,
{
    fn bitor_assign(&mut self, rhs: EnumSet<Out>) {
        *self = *self | rhs;
    }
}

impl<Out> BitOr<Out> for Expectation<Out>
where
    Out: EnumSetType,
{
    type Output = Self;

    fn bitor(self, rhs: Out) -> Self::Output {
        let Self(lhs) = self;

        Self(lhs | rhs)
    }
}

impl<Out> BitOrAssign<Out> for Expectation<Out>
where
    Out: EnumSetType,
{
    fn bitor_assign(&mut self, rhs: Out) {
        *self = *self | rhs;
    }
}

/// Similar to the ubiquitous `enum Either`, but with the implication that `Collapsed` values are
/// abbreviations of equivalent `Expanded` values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaybeCollapsed<C, E> {
    Collapsed(C),
    Expanded(E),
}

impl<C, E> Default for MaybeCollapsed<C, E>
where
    C: Default,
{
    fn default() -> Self {
        Self::Collapsed(Default::default())
    }
}

/// A completely flat representation of [`NormalizedExpectationPropertyValueData`] suitable for
/// byte representation in memory.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct FullyExpandedExpectationPropertyValue<Out>(
    EnumMap<Platform, EnumMap<BuildProfile, Expectation<Out>>>,
)
where
    Out: EnumSetType;

impl<Out> Default for FullyExpandedExpectationPropertyValue<Out>
where
    Out: Default + EnumSetType,
{
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<Out> Index<(Platform, BuildProfile)> for FullyExpandedExpectationPropertyValue<Out>
where
    Out: EnumSetType,
{
    type Output = Expectation<Out>;

    fn index(&self, (platform, build_profile): (Platform, BuildProfile)) -> &Self::Output {
        &self.0[platform][build_profile]
    }
}

impl<Out> IndexMut<(Platform, BuildProfile)> for FullyExpandedExpectationPropertyValue<Out>
where
    Out: EnumSetType,
{
    fn index_mut(
        &mut self,
        (platform, build_profile): (Platform, BuildProfile),
    ) -> &mut Self::Output {
        &mut self.0[platform][build_profile]
    }
}

impl<Out> FullyExpandedExpectationPropertyValue<Out>
where
    Out: EnumSetType,
{
    pub fn uniform(expectation: Expectation<Out>) -> Self {
        Self(EnumMap::from_fn(|_idx| {
            EnumMap::from_fn(|_idx| expectation)
        }))
    }

    pub fn get(&self, platform: Platform, build_profile: BuildProfile) -> Expectation<Out> {
        self.0[platform][build_profile]
    }

    pub(crate) fn iter(
        &self,
    ) -> impl Iterator<Item = ((Platform, BuildProfile), Expectation<Out>)> + '_ {
        self.0.iter().flat_map(|(platform, exps_by_bp)| {
            exps_by_bp.iter().map(move |(build_profile, expectations)| {
                ((platform, build_profile), *expectations)
            })
        })
    }

    pub(crate) fn iter_mut(
        &mut self,
    ) -> impl Iterator<Item = ((Platform, BuildProfile), &mut Expectation<Out>)> + '_ {
        self.0.iter_mut().flat_map(|(platform, exps_by_bp)| {
            exps_by_bp
                .iter_mut()
                .map(move |(build_profile, expectations)| ((platform, build_profile), expectations))
        })
    }
}

impl<Out> FullyExpandedExpectationPropertyValue<Out>
where
    Out: Default + EnumSetType,
{
    pub fn from_query<F>(f: F) -> Self
    where
        F: FnMut(Platform, BuildProfile) -> Expectation<Out>,
    {
        let mut f = f;
        let mut this = Self::default();
        for platform in Platform::iter() {
            let by_plat = &mut this.0[platform];
            for build_profile in BuildProfile::iter() {
                by_plat[build_profile] = f(platform, build_profile);
            }
        }
        this
    }
}

#[test]
fn fully_expanded_is_tiny() {
    use crate::metadata::{SubtestOutcome, TestOutcome};
    use std::mem::size_of;

    assert_eq!(
        size_of::<FullyExpandedExpectationPropertyValue<TestOutcome>>(),
        6
    );
    assert_eq!(
        size_of::<FullyExpandedExpectationPropertyValue<SubtestOutcome>>(),
        6
    );
}

/// A normalized representation of [`Expectation`]s in [`TestProps`], which collapses
/// backwards along the following branching factors:
///
/// * [`Platform`]
/// * [`BuildProfile`]
///
/// Yes, the type is _gnarly_. Sorry about that. This is some complex domain, okay? 😆😭
///
/// [`TestProps`]: crate::metadata::TestProps
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedExpectationPropertyValue<Out>(NormalizedExpectationPropertyValueData<Out>)
where
    Out: EnumSetType;

pub type NormalizedExpectationByBuildProfile<Out> =
    MaybeCollapsed<Expectation<Out>, BTreeMap<BuildProfile, Expectation<Out>>>;

/// Data from a [`NormalizedExpectationPropertyValue`].
pub type NormalizedExpectationPropertyValueData<Out> = MaybeCollapsed<
    NormalizedExpectationByBuildProfile<Out>,
    BTreeMap<Platform, NormalizedExpectationByBuildProfile<Out>>,
>;

impl<Out> Default for NormalizedExpectationPropertyValue<Out>
where
    Out: Default + EnumSetType,
{
    fn default() -> Self {
        Self(MaybeCollapsed::Collapsed(MaybeCollapsed::Collapsed(
            Default::default(),
        )))
    }
}

impl<Out> NormalizedExpectationPropertyValue<Out>
where
    Out: EnumSetType,
{
    pub fn inner(&self) -> &NormalizedExpectationPropertyValueData<Out> {
        let Self(inner) = self;
        inner
    }

    pub(crate) fn from_fully_expanded(
        outcomes: FullyExpandedExpectationPropertyValue<Out>,
    ) -> Self {
        fn same_value<T>(iter: impl IntoIterator<Item = T>) -> Option<T>
        where
            T: Eq,
        {
            let mut iter = iter.into_iter();
            let first = iter.next()?;
            for next in iter {
                if first != next {
                    return None;
                }
            }
            Some(first)
        }

        Self(
            if let Some(uniform) = same_value(outcomes.iter().map(|(_, outcomes)| outcomes)) {
                MaybeCollapsed::Collapsed(MaybeCollapsed::Collapsed(uniform))
            } else {
                let per_bp = |outcomes_by_bp: &EnumMap<_, _>| {
                    outcomes_by_bp
                        .iter()
                        .map(|(bp, outcomes)| (bp, *outcomes))
                        .collect()
                };
                if let Some(uniform_per_platform) =
                    same_value(outcomes.0.iter().map(|(_, outcomes_by_bp)| outcomes_by_bp))
                {
                    MaybeCollapsed::Collapsed(MaybeCollapsed::Expanded(per_bp(
                        uniform_per_platform,
                    )))
                } else {
                    MaybeCollapsed::Expanded(
                        outcomes
                            .0
                            .iter()
                            .map(|(platform, outcomes_by_bp)| {
                                if let Some(uniform_per_bp) =
                                    same_value(outcomes_by_bp.iter().map(|(_, outcomes)| *outcomes))
                                {
                                    (platform, MaybeCollapsed::Collapsed(uniform_per_bp))
                                } else {
                                    (platform, MaybeCollapsed::Expanded(per_bp(outcomes_by_bp)))
                                }
                            })
                            .collect(),
                    )
                }
            },
        )
    }
}

/// A single symbolic path to a test and its metadata.
///
/// This API is useful as a common representation of a path for [`crate::report::ExecutionReport`]s
/// and [`crate::metadata::File`]s.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TestPath<'a> {
    pub scope: TestScope,
    /// A relative offset into `scope`.
    pub path: Cow<'a, Utf8Path>,
    /// The variant of this particular test from this test's source code. If set, you should be
    /// able to correlate this with
    ///
    /// Generally, a test in WPT is _either_ a single test, or a set of test variants. That is, for
    /// a given `path`, there will be a single `variant: None`, or multiple tests with `variant:
    /// Some(…)`.
    pub variant: Option<Cow<'a, str>>,
}

const SCOPE_DIR_FX_PRIVATE_STR: &str = "testing/web-platform/mozilla";
const SCOPE_DIR_FX_PRIVATE_COMPONENTS: &[&str] = &["testing", "web-platform", "mozilla"];
const SCOPE_DIR_FX_PUBLIC_STR: &str = "testing/web-platform";
const SCOPE_DIR_FX_PUBLIC_COMPONENTS: &[&str] = &["testing", "web-platform"];
const SCOPE_DIR_SERVO_PUBLIC_STR: &str = "tests/wpt/webgpu";
const SCOPE_DIR_SERVO_PUBLIC_COMPONENTS: &[&str] = &["tests", "wpt", "webgpu"];

impl<'a> TestPath<'a> {
    pub fn from_execution_report(
        test_url_path: &'a str,
        browser: Browser,
    ) -> Result<Self, ExecutionReportPathError<'a>> {
        let err = || ExecutionReportPathError { test_url_path };
        let Some((scope, path)) = (match browser {
            Browser::Firefox => test_url_path
                .strip_prefix("/_mozilla/")
                .map(|stripped| (TestScope::new(browser, TestVisibility::Private), stripped))
                .or_else(|| {
                    test_url_path
                        .strip_prefix('/')
                        .map(|stripped| (TestScope::public(), stripped))
                }),
            Browser::Servo => test_url_path
                .strip_prefix("/_webgpu/")
                .map(|stripped| (TestScope::servo(), stripped)),
        }) else {
            return Err(err());
        };

        if path.contains('\\') {
            return Err(err());
        }

        let (path, variant) = match path.split('/').next_back() {
            Some(path_and_maybe_variants) => match path_and_maybe_variants.find('?') {
                Some(query_params_start_idx) => (
                    &path[..path.len() - (path_and_maybe_variants.len() - query_params_start_idx)],
                    Some(&path_and_maybe_variants[query_params_start_idx..]),
                ),
                None => (path, None),
            },
            None => return Err(err()),
        };

        Ok(Self {
            scope,
            path: Utf8Path::new(path).into(),
            variant: variant.map(Into::into),
        })
    }

    pub fn from_metadata_test(
        rel_meta_file_path: &'a Path,
        test_name: &'a str,
    ) -> Result<Self, MetadataTestPathError<'a>> {
        let rel_meta_file_path =
            Utf8Path::new(rel_meta_file_path.to_str().ok_or(MetadataTestPathError {
                rel_meta_file_path,
                test_name,
            })?);
        let err = || MetadataTestPathError {
            rel_meta_file_path: rel_meta_file_path.as_std_path(),
            test_name,
        };
        let rel_meta_file_path = Utf8Path::new(
            rel_meta_file_path
                .as_str()
                .strip_suffix(".ini")
                .ok_or(err())?,
        );

        let (scope, path) = {
            if let Ok(path) = rel_meta_file_path.strip_prefix(SCOPE_DIR_FX_PRIVATE_STR) {
                (TestScope::firefox_private(), path)
            } else if let Ok(path) = rel_meta_file_path.strip_prefix(SCOPE_DIR_FX_PUBLIC_STR) {
                (TestScope::public(), path)
            } else if let Ok(path) = rel_meta_file_path.strip_prefix(SCOPE_DIR_SERVO_PUBLIC_STR) {
                (TestScope::servo(), path)
            } else {
                return Err(err());
            }
        };
        let Ok(path) = path.strip_prefix("meta/") else {
            return Err(err());
        };

        let (base_name, variant) = Self::split_test_base_name_from_variant(test_name);

        if path.components().next_back() != Some(Utf8Component::Normal(base_name)) {
            return Err(err());
        }

        Ok(Self {
            scope,
            path: Utf8Path::new(path).into(),
            variant: variant.map(Into::into),
        })
    }

    fn split_test_base_name_from_variant(url_ish_name: &'a str) -> (&'a str, Option<&'a str>) {
        match url_ish_name.find('?') {
            Some(query_params_start_idx) => (
                &url_ish_name[..url_ish_name.len() - (url_ish_name.len() - query_params_start_idx)],
                Some(&url_ish_name[query_params_start_idx..]),
            ),
            None => (url_ish_name, None),
        }
    }

    pub fn into_owned(self) -> TestPath<'static> {
        let Self {
            scope,
            path,
            variant,
        } = self;

        TestPath {
            scope: scope.clone(),
            path: path.clone().into_owned().into(),
            variant: variant.clone().map(|v| v.into_owned().into()),
        }
    }

    pub(crate) fn test_name(&self) -> impl Display + '_ {
        let Self {
            path,
            variant,
            scope: _,
        } = self;
        let base_name = path.file_name().unwrap();

        lazy_format!(move |f| {
            write!(f, "{base_name}")?;
            if let Some(variant) = variant {
                write!(f, "{variant}")?;
            }
            Ok(())
        })
    }

    pub(crate) fn runner_url_path(&self) -> impl Display + '_ {
        let Self {
            path,
            variant,
            scope,
        } = self;
        lazy_format!(move |f| {
            let scope_prefix = match scope.visibility {
                TestVisibility::Public => "",
                TestVisibility::Private => "_mozilla/",
            };
            write!(f, "{scope_prefix}{}", path.components().join_with('/'))?;
            if let Some(variant) = variant.as_ref() {
                write!(f, "{}", variant)?;
            }
            Ok(())
        })
    }

    pub(crate) fn rel_metadata_path(&self) -> impl Display + '_ {
        let Self {
            path,
            variant: _,
            scope,
        } = self;

        let scope_dir = match (scope.browser, scope.visibility) {
            (Browser::Firefox, TestVisibility::Public) => SCOPE_DIR_FX_PUBLIC_COMPONENTS,
            (Browser::Firefox, TestVisibility::Private) => SCOPE_DIR_FX_PRIVATE_COMPONENTS,
            (Browser::Servo, TestVisibility::Public) => SCOPE_DIR_SERVO_PUBLIC_COMPONENTS,
            (Browser::Servo, _) => todo!(),
        }
        .iter()
        .chain(&["meta"])
        .join_with(std::path::MAIN_SEPARATOR);

        lazy_format!(move |f| { write!(f, "{scope_dir}{}{path}.ini", std::path::MAIN_SEPARATOR) })
    }
}

#[derive(Debug)]
pub struct ExecutionReportPathError<'a> {
    test_url_path: &'a str,
}

impl Display for ExecutionReportPathError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { test_url_path } = self;
        write!(
            f,
            concat!(
                "failed to derive test path from execution report's entry ",
                "for a test at URL path {:?}"
            ),
            test_url_path
        )
    }
}

#[derive(Debug)]
pub struct MetadataTestPathError<'a> {
    rel_meta_file_path: &'a Path,
    test_name: &'a str,
}

impl Display for MetadataTestPathError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            rel_meta_file_path,
            test_name,
        } = self;
        write!(
            f,
            "failed to derive test path from relative metadata path {:?} and test name {:?}",
            rel_meta_file_path, test_name
        )
    }
}

#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Default, ValueEnum)]
pub(crate) enum Browser {
    #[default]
    Firefox,
    Servo,
}

#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum TestVisibility {
    /// A public test available at some point in the history of [WPT upstream]. Note that while
    /// a test may be public, metadata associated with it is in a private location.
    ///
    /// [WPT upstream]: https://github.com/web-platform-tests/wpt
    Public,
    /// A private test specific to browser.
    Private,
}

/// Symbolically represents a file root from which tests and metadata are based.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TestScope {
    browser: Browser,
    visibility: TestVisibility,
}

impl TestScope {
    fn new(browser: Browser, visibility: TestVisibility) -> Self {
        Self {
            browser,
            visibility,
        }
    }

    fn servo() -> Self {
        Self::new(Browser::Servo, TestVisibility::Public)
    }

    fn firefox_private() -> Self {
        Self::new(Browser::Firefox, TestVisibility::Private)
    }

    fn public() -> Self {
        Self::new(Browser::Firefox, TestVisibility::Public)
    }
}

#[test]
fn parse_test_path() {
    assert_eq!(
        TestPath::from_metadata_test(
            Path::new("testing/web-platform/mozilla/meta/blarg/cts.https.html.ini"),
            "cts.https.html?stuff=things"
        )
        .unwrap(),
        TestPath {
            scope: TestScope::firefox_private(),
            path: Utf8Path::new("blarg/cts.https.html").into(),
            variant: Some("?stuff=things".into()),
        }
    );

    assert_eq!(
        TestPath::from_metadata_test(
            Path::new("testing/web-platform/meta/stuff/things/cts.https.html.ini"),
            "cts.https.html"
        )
        .unwrap(),
        TestPath {
            scope: TestScope::public(),
            path: Utf8Path::new("stuff/things/cts.https.html").into(),
            variant: None,
        }
    );

    assert_eq!(
        TestPath::from_metadata_test(
            Path::new("tests/wpt/webgpu/meta/webgpu/cts.https.html.ini"),
            "cts.https.html?stuff=things"
        )
        .unwrap(),
        TestPath {
            scope: TestScope::servo(),
            path: Utf8Path::new("webgpu/cts.https.html").into(),
            variant: Some("?stuff=things".into()),
        }
    );
}

#[test]
fn report_meta_match() {
    macro_rules! assert_test_matches_meta {
        ($test_run_path:expr, $rel_meta_path:expr, $test_section_header:expr) => {
            assert_test_matches_meta!(
                Browser::Firefox,
                $test_run_path,
                $rel_meta_path,
                $test_section_header
            )
        };
        ($browser:expr, $test_run_path:expr, $rel_meta_path:expr, $test_section_header:expr) => {
            assert_eq!(
                TestPath::from_execution_report($test_run_path, $browser).unwrap(),
                TestPath::from_metadata_test(Path::new($rel_meta_path), $test_section_header)
                    .unwrap()
            )
        };
    }

    assert_test_matches_meta!(
        "/_mozilla/blarg/cts.https.html?stuff=things",
        "testing/web-platform/mozilla/meta/blarg/cts.https.html.ini",
        "cts.https.html?stuff=things"
    );

    assert_test_matches_meta!(
        "/blarg/cts.https.html?stuff=things",
        "testing/web-platform/meta/blarg/cts.https.html.ini",
        "cts.https.html?stuff=things"
    );

    assert_test_matches_meta!(
        Browser::Servo,
        "/_webgpu/webgpu/cts.https.html?stuff=things",
        "tests/wpt/webgpu/meta/webgpu/cts.https.html.ini",
        "cts.https.html?stuff=things"
    );
}

#[test]
fn report_meta_reject() {
    macro_rules! assert_test_rejects_meta {
        ($test_run_path:expr, $rel_meta_path:expr, $test_section_header:expr) => {
            assert_ne!(
                TestPath::from_execution_report($test_run_path, Browser::Firefox).unwrap(),
                TestPath::from_metadata_test(Path::new($rel_meta_path), $test_section_header)
                    .unwrap()
            )
        };
    }

    assert_test_rejects_meta!(
        "/blarg/cts.https.html?stuff=things",
        // Wrong: the `mozilla` component shouldn't be after `web-platform`
        "testing/web-platform/mozilla/meta/blarg/cts.https.html.ini",
        "cts.https.html?stuff=things"
    );

    assert_test_rejects_meta!(
        "/_mozilla/blarg/cts.https.html?stuff=things",
        // Wrong: missing the `mozilla` component after `web-platform`
        "testing/web-platform/meta/blarg/cts.https.html.ini",
        "cts.https.html?stuff=things"
    );
}

#[test]
fn runner_url_path() {
    assert_eq!(
        TestPath::from_metadata_test(
            Path::new("testing/web-platform/meta/blarg/stuff.https.html.ini"),
            "stuff.https.html"
        )
        .unwrap()
        .runner_url_path()
        .to_string(),
        "blarg/stuff.https.html",
    );

    assert_eq!(
        TestPath::from_metadata_test(
            Path::new("testing/web-platform/meta/blarg/stuff.https.html.ini"),
            "stuff.https.html?win"
        )
        .unwrap()
        .runner_url_path()
        .to_string(),
        "blarg/stuff.https.html?win",
    );

    assert_eq!(
        TestPath::from_metadata_test(
            Path::new("testing/web-platform/mozilla/meta/blarg/stuff.https.html.ini"),
            "stuff.https.html"
        )
        .unwrap()
        .runner_url_path()
        .to_string(),
        "_mozilla/blarg/stuff.https.html",
    );

    assert_eq!(
        TestPath::from_metadata_test(
            Path::new("testing/web-platform/mozilla/meta/blarg/stuff.https.html.ini"),
            "stuff.https.html?win"
        )
        .unwrap()
        .runner_url_path()
        .to_string(),
        "_mozilla/blarg/stuff.https.html?win",
    );

    assert_eq!(
        TestPath::from_metadata_test(
            Path::new("tests/wpt/webgpu/meta/webgpu/cts.https.html.ini"),
            "cts.https.html?win"
        )
        .unwrap()
        .runner_url_path()
        .to_string(),
        "webgpu/cts.https.html?win",
    );
}
