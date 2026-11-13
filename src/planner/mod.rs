//! Submission resource planning using supplied history observations.
pub mod locks;
pub mod predict;
use crate::domain::error::InvalidInput;
use crate::domain::job::Plan;
use crate::profile::Profiles;
use crate::recognize::Analysis;
use predict::{Defaults, Lookup, Stats, line_key, parse_hint, predict};
use std::path::Path;

pub fn plan(
    analysis: &Analysis,
    profiles: &Profiles,
    cwd: &Path,
    extra_locks: &[String],
    hint: Option<&str>,
    history: &impl Fn(Lookup) -> Stats,
) -> Result<Plan, InvalidInput> {
    let hint = hint
        .map(parse_hint)
        .transpose()
        .map_err(|e| InvalidInput(format!("--hint: {e}")))?;
    let prediction = predict(analysis, profiles, &Defaults::default(), hint, history);
    let locks = locks::for_line(analysis, cwd, profiles, extra_locks);
    let (parallel_env, jobserver) = parallelism(analysis, profiles);
    let mut names: Vec<String> = analysis
        .segments
        .iter()
        .filter_map(|s| s.profile.clone())
        .collect();
    names.sort();
    names.dedup();
    Ok(Plan {
        profiles: names,
        prediction,
        locks,
        parallel_env,
        jobserver,
        history_key: line_key(analysis),
        ..Plan::default()
    })
}

fn sets_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| {
        a.strip_prefix(flag)
            .is_some_and(|r| r.is_empty() || r.starts_with('=') || r.parse::<f64>().is_ok())
    })
}

fn parallelism(analysis: &Analysis, profiles: &Profiles) -> (Vec<String>, bool) {
    let (mut env, mut jobserver) = (Vec::new(), false);
    for seg in &analysis.segments {
        let Some(p) = profiles
            .find(&seg.argv)
            .filter(|p| Some(&p.name) == seg.profile.as_ref())
        else {
            continue;
        };
        if p.parallelism
            .flag
            .as_deref()
            .is_some_and(|f| sets_flag(&seg.argv[1..], f))
        {
            return (Vec::new(), false);
        }
        env.extend(p.parallelism.env.iter().cloned());
        jobserver |= p.parallelism.jobserver;
    }
    env.sort();
    env.dedup();
    (env, jobserver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recognize::Recognizer;

    #[test]
    fn parallelism_from_profiles() {
        let profiles = Profiles::builtin();
        let dir = tempfile::tempdir().unwrap();
        let r = Recognizer::new(&profiles, &[]);
        let par = |src: &str| parallelism(&r.shell(src, dir.path()), &profiles);
        assert_eq!(
            par("cargo build"),
            (vec!["CARGO_BUILD_JOBS".to_string()], true)
        );
        assert_eq!(
            par("make all && ninja"),
            (vec![], true),
            "make takes its jobs from the pool"
        );
        for explicit in ["ninja -j8", "make -j", "make -j 4", "jest --max-workers=4"] {
            assert_eq!(
                par(explicit),
                (vec![], false),
                "{explicit}: its own parallelism is kept"
            );
        }
        assert_eq!(par("ls"), (vec![], false));
    }
}
