use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

use super::cli::{dedupe, CliInput, Request};
use super::tuning::{self, Tuning};
use super::Error;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct SearchRoots {
    pub home: Option<PathBuf>,
    pub exe_dir: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
}

impl SearchRoots {
    pub(super) fn from_process() -> Self {
        let home = std::env::var_os("HOME").and_then(|value| {
            if value.is_empty() {
                None
            } else {
                Some(PathBuf::from(value))
            }
        });
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let cwd = std::env::current_dir().ok();
        Self { home, exe_dir, cwd }
    }
}

#[derive(Debug, Clone)]
pub(super) struct LoadedConfig {
    pub path: PathBuf,
    pub tuning: Tuning,
}

pub(super) fn load(
    explicit: Option<&Path>,
    roots: &SearchRoots,
) -> Result<Option<LoadedConfig>, Error> {
    match explicit {
        Some(path) => load_required(&resolve_explicit(path, roots)).map(Some),
        None => match discover(roots) {
            Some(path) => load_required(&path).map(Some),
            None => Ok(None),
        },
    }
}

pub(super) fn resolve(input: CliInput, loaded: Option<LoadedConfig>) -> Result<Request, Error> {
    let mut tuning = match &loaded {
        Some(file) => file.tuning.clone(),
        None => Tuning::default(),
    };

    if !input.levels.is_empty() {
        let levels = dedupe(input.levels);
        if levels.is_empty() {
            return Err(Error::EmptyLevels);
        }
        tuning.levels = levels;
    }
    if let Some(timezone) = input.timezone {
        tuning.timezone = timezone;
    }

    Ok(Request {
        program_id: input.program_id,
        environment_id: input.environment_id,
        service: input.service,
        levels: tuning.levels.clone(),
        ims_context: input.ims_context,
        config: loaded.as_ref().map(|file| file.path.clone()),
        timezone: tuning.timezone,
        json: input.json,
        raw_sample: input.raw_sample,
        tuning,
    })
}

fn discover(roots: &SearchRoots) -> Option<PathBuf> {
    let mut candidates = Vec::with_capacity(4);
    if let Some(home) = &roots.home {
        candidates.push(home.join("aemlog.toml"));
        candidates.push(home.join(".config/aemlog/config.toml"));
    }
    if let Some(exe_dir) = &roots.exe_dir {
        candidates.push(exe_dir.join("aemlog.toml"));
    }
    if let Some(cwd) = &roots.cwd {
        candidates.push(cwd.join("aemlog.toml"));
    }
    candidates.into_iter().find(|path| is_regular_file(path))
}

fn resolve_explicit(path: &Path, roots: &SearchRoots) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = &roots.cwd {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

fn is_regular_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

fn load_required(path: &Path) -> Result<LoadedConfig, Error> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::ConfigNotFound(path.to_path_buf()));
        }
        Err(err) => return Err(unreadable(path, err.to_string())),
    };
    if !meta.is_file() {
        return Err(Error::ConfigNotRegular(path.to_path_buf()));
    }
    let text = fs::read_to_string(path).map_err(|err| unreadable(path, err.to_string()))?;
    let value: Value = toml::from_str(&text).map_err(|err| invalid(path, err.to_string()))?;
    let table = match value {
        Value::Table(table) => table,
        _ => return Err(invalid(path, "root must be a table")),
    };
    let tuning = tuning::from_table(&table).map_err(|message| invalid(path, message))?;
    Ok(LoadedConfig {
        path: path.to_path_buf(),
        tuning,
    })
}

fn unreadable(path: &Path, message: impl Into<String>) -> Error {
    Error::ConfigUnreadable {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn invalid(path: &Path, message: impl Into<String>) -> Error {
    Error::ConfigInvalid {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::{env, fs, process};

    use super::*;
    use crate::app::cli::{Level, Service, Timezone};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Scratch {
        root: PathBuf,
        home: PathBuf,
        exe: PathBuf,
        cwd: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!("aemlog-cfg-{}-{n}", process::id()));
            let home = root.join("home");
            let exe = root.join("exe");
            let cwd = root.join("cwd");
            fs::create_dir_all(home.join(".config/aemlog")).expect("home config dir");
            fs::create_dir_all(&exe).expect("exe dir");
            fs::create_dir_all(&cwd).expect("cwd");
            Self {
                root,
                home,
                exe,
                cwd,
            }
        }

        fn roots(&self) -> SearchRoots {
            SearchRoots {
                home: Some(self.home.clone()),
                exe_dir: Some(self.exe.clone()),
                cwd: Some(self.cwd.clone()),
            }
        }

        fn write_home(&self, body: &str) {
            fs::write(self.home.join("aemlog.toml"), versioned(body)).expect("home file");
        }

        fn write_xdg(&self, body: &str) {
            fs::write(
                self.home.join(".config/aemlog/config.toml"),
                versioned(body),
            )
            .expect("xdg file");
        }

        fn write_exe(&self, body: &str) {
            fs::write(self.exe.join("aemlog.toml"), versioned(body)).expect("exe file");
        }

        fn write_cwd(&self, body: &str) {
            fs::write(self.cwd.join("aemlog.toml"), versioned(body)).expect("cwd file");
        }

        fn write_explicit(&self, name: &str, body: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, versioned(body)).expect("explicit file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn base_input() -> CliInput {
        CliInput {
            program_id: "p1".into(),
            environment_id: "e1".into(),
            service: Service::Author,
            levels: vec![],
            ims_context: None,
            config: None,
            timezone: None,
            json: true,
            raw_sample: false,
        }
    }

    fn finish(input: CliInput, roots: &SearchRoots) -> Request {
        let loaded = load(input.config.as_deref(), roots).expect("load");
        resolve(input, loaded).expect("resolve")
    }

    fn finish_err(input: CliInput, roots: &SearchRoots) -> Error {
        match load(input.config.as_deref(), roots) {
            Err(err) => err,
            Ok(loaded) => resolve(input, loaded).expect_err("expected resolve error"),
        }
    }
    fn versioned(body: &str) -> String {
        if body.contains("[[[") {
            body.to_owned()
        } else {
            format!("version = 1\n{body}")
        }
    }

    #[test]
    fn explicit_config_is_authoritative() {
        let scratch = Scratch::new();
        scratch.write_home("timezone = \"local\"\nlevels = [\"WARN\"]\n");
        let explicit = scratch.write_explicit("chosen.toml", "timezone = \"America/New_York\"\n");
        let mut input = base_input();
        input.config = Some(explicit.clone());
        let request = finish(input, &scratch.roots());
        assert_eq!(request.config.as_deref(), Some(explicit.as_path()));
        match request.timezone {
            Timezone::Iana(tz) => assert_eq!(tz.name(), "America/New_York"),
            other => panic!("expected IANA timezone, got {other:?}"),
        }
        assert_eq!(request.levels, vec![Level::Error]);
    }

    #[test]
    fn explicit_missing_file_fails_without_fallback() {
        let scratch = Scratch::new();
        scratch.write_home("timezone = \"local\"\n");
        let missing = scratch.root.join("missing.toml");
        let mut input = base_input();
        input.config = Some(missing.clone());
        match finish_err(input, &scratch.roots()) {
            Error::ConfigNotFound(path) => assert_eq!(path, missing),
            other => panic!("expected ConfigNotFound, got {other:?}"),
        }
    }

    #[test]
    fn explicit_invalid_file_fails_without_fallback() {
        let scratch = Scratch::new();
        scratch.write_home("timezone = \"local\"\n");
        let explicit = scratch.write_explicit("bad.toml", "[[[not toml");
        let mut input = base_input();
        input.config = Some(explicit.clone());
        match finish_err(input, &scratch.roots()) {
            Error::ConfigInvalid { path, message } => {
                assert_eq!(path, explicit);
                assert!(!message.is_empty());
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn explicit_directory_fails() {
        let scratch = Scratch::new();
        let mut input = base_input();
        input.config = Some(scratch.cwd.clone());
        match finish_err(input, &scratch.roots()) {
            Error::ConfigNotRegular(path) => assert_eq!(path, scratch.cwd),
            other => panic!("expected ConfigNotRegular, got {other:?}"),
        }
    }

    #[test]
    fn discovery_uses_first_existing_regular_file() {
        let scratch = Scratch::new();
        scratch.write_home("timezone = \"local\"\n");
        scratch.write_xdg("timezone = \"UTC\"\n");
        scratch.write_exe("timezone = \"America/New_York\"\n");
        scratch.write_cwd("timezone = \"Europe/Paris\"\n");
        let request = finish(base_input(), &scratch.roots());
        assert_eq!(request.timezone, Timezone::Local);
        assert_eq!(
            request.config.as_deref(),
            Some(scratch.home.join("aemlog.toml").as_path())
        );
    }

    #[test]
    fn discovery_falls_through_missing_files_only() {
        let scratch = Scratch::new();
        scratch.write_xdg("timezone = \"local\"\n");
        scratch.write_exe("timezone = \"UTC\"\n");
        scratch.write_cwd("timezone = \"Europe/Paris\"\n");
        let request = finish(base_input(), &scratch.roots());
        assert_eq!(request.timezone, Timezone::Local);
        assert_eq!(
            request.config.as_deref(),
            Some(scratch.home.join(".config/aemlog/config.toml").as_path())
        );
    }

    #[test]
    fn discovery_uses_executable_then_working_directory() {
        let scratch = Scratch::new();
        scratch.write_exe("timezone = \"America/New_York\"\n");
        scratch.write_cwd("timezone = \"Europe/Paris\"\n");
        let request = finish(base_input(), &scratch.roots());
        match request.timezone {
            Timezone::Iana(tz) => assert_eq!(tz.name(), "America/New_York"),
            other => panic!("expected IANA timezone, got {other:?}"),
        }

        fs::remove_file(scratch.exe.join("aemlog.toml")).expect("remove exe config");
        let request = finish(base_input(), &scratch.roots());
        match request.timezone {
            Timezone::Iana(tz) => assert_eq!(tz.name(), "Europe/Paris"),
            other => panic!("expected IANA timezone, got {other:?}"),
        }
    }

    #[test]
    fn missing_home_or_exe_skips_only_those_entries() {
        let scratch = Scratch::new();
        scratch.write_home("timezone = \"local\"\n");
        scratch.write_exe("timezone = \"UTC\"\n");
        scratch.write_cwd("timezone = \"Europe/Paris\"\n");
        let mut roots = scratch.roots();
        roots.home = None;
        let request = finish(base_input(), &roots);
        assert_eq!(request.timezone, Timezone::Utc);
        assert_eq!(
            request.config.as_deref(),
            Some(scratch.exe.join("aemlog.toml").as_path())
        );

        roots.exe_dir = None;
        let request = finish(base_input(), &roots);
        match request.timezone {
            Timezone::Iana(tz) => assert_eq!(tz.name(), "Europe/Paris"),
            other => panic!("expected IANA timezone, got {other:?}"),
        }
    }

    #[test]
    fn directory_candidates_are_skipped() {
        let scratch = Scratch::new();
        fs::create_dir(scratch.home.join("aemlog.toml")).expect("home dir candidate");
        scratch.write_cwd("timezone = \"local\"\n");
        let request = finish(base_input(), &scratch.roots());
        assert_eq!(request.timezone, Timezone::Local);
        assert_eq!(
            request.config.as_deref(),
            Some(scratch.cwd.join("aemlog.toml").as_path())
        );
    }

    #[test]
    fn broken_higher_priority_file_does_not_fall_through() {
        let scratch = Scratch::new();
        scratch.write_home("[[[broken");
        scratch.write_cwd("timezone = \"local\"\n");
        match finish_err(base_input(), &scratch.roots()) {
            Error::ConfigInvalid { path, .. } => {
                assert_eq!(path, scratch.home.join("aemlog.toml"));
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn multiple_files_never_merge_fields() {
        let scratch = Scratch::new();
        scratch.write_home("timezone = \"local\"\n");
        scratch.write_cwd("timezone = \"UTC\"\nlevels = [\"WARN\"]\n");
        let request = finish(base_input(), &scratch.roots());
        assert_eq!(request.timezone, Timezone::Local);
        assert_eq!(request.levels, vec![Level::Error]);
    }

    #[test]
    fn cli_overrides_file_overrides_defaults() {
        let scratch = Scratch::new();
        scratch.write_cwd("timezone = \"local\"\nlevels = [\"WARN\", \"ERROR\"]\n");
        let mut roots = scratch.roots();
        roots.home = None;
        roots.exe_dir = None;

        let from_file = finish(base_input(), &roots);
        assert_eq!(from_file.timezone, Timezone::Local);
        assert_eq!(from_file.levels, vec![Level::Warn, Level::Error]);
        assert_eq!(
            from_file.tuning.similarity,
            crate::app::tuning::DEFAULT_SIMILARITY
        );

        let mut input = base_input();
        input.timezone = Some(Timezone::Utc);
        input.levels = vec![Level::Info];
        let from_cli = finish(input, &roots);
        assert_eq!(from_cli.timezone, Timezone::Utc);
        assert_eq!(from_cli.levels, vec![Level::Info]);
        assert_eq!(from_cli.tuning.levels, vec![Level::Info]);

        let defaults = finish(
            base_input(),
            &SearchRoots {
                home: None,
                exe_dir: None,
                cwd: None,
            },
        );
        assert_eq!(defaults.timezone, Timezone::Utc);
        assert_eq!(defaults.levels, vec![Level::Error]);
        assert_eq!(defaults.config, None);
        assert_eq!(defaults.tuning, Tuning::default());
    }

    #[test]
    fn omitted_cli_fields_keep_file_values() {
        let scratch = Scratch::new();
        scratch.write_cwd("timezone = \"local\"\nlevels = [\"DEBUG\"]\n");
        let mut roots = scratch.roots();
        roots.home = None;
        roots.exe_dir = None;
        let mut input = base_input();
        input.timezone = Some(Timezone::Utc);
        let request = finish(input, &roots);
        assert_eq!(request.timezone, Timezone::Utc);
        assert_eq!(request.levels, vec![Level::Debug]);
    }

    #[test]
    fn file_tuning_overrides_defaults_and_cli_does_not_mask() {
        let scratch = Scratch::new();
        scratch.write_cwd(
            "\
timezone = \"local\"
levels = [\"WARN\"]
[templates]
similarity = 0.75
bucket_cap = 50
[groups]
max = 10
[event]
max_bytes = 1024
max_lines = 10
[sample]
max_bytes = 512
budget_bytes = 2048
[rates]
fast_half_life_secs = 5
baseline_half_life_secs = 20
new_age_secs = 15
increasing_min_age_secs = 30
increasing_ratio = 3.0
increasing_min_rate = 1.5
[redaction]
extra_patterns = [\"secret-[0-9]+\"]
",
        );
        let mut roots = scratch.roots();
        roots.home = None;
        roots.exe_dir = None;

        let from_file = finish(base_input(), &roots);
        assert_eq!(from_file.timezone, Timezone::Local);
        assert_eq!(from_file.levels, vec![Level::Warn]);
        assert_eq!(from_file.tuning.similarity, 0.75);
        assert_eq!(from_file.tuning.bucket_cap, 50);
        assert_eq!(from_file.tuning.max_groups, 10);
        assert_eq!(from_file.tuning.event_max_bytes, 1024);
        assert_eq!(from_file.tuning.event_max_lines, 10);
        assert_eq!(from_file.tuning.sample_max_bytes, 512);
        assert_eq!(from_file.tuning.sample_budget_bytes, 2048);
        assert_eq!(from_file.tuning.fast_half_life_secs, 5);
        assert_eq!(from_file.tuning.baseline_half_life_secs, 20);
        assert_eq!(from_file.tuning.new_age_secs, 15);
        assert_eq!(from_file.tuning.increasing_min_age_secs, 30);
        assert_eq!(from_file.tuning.increasing_ratio, 3.0);
        assert_eq!(from_file.tuning.increasing_min_rate, 1.5);
        assert_eq!(from_file.tuning.extra_patterns.len(), 1);

        let mut input = base_input();
        input.levels = vec![Level::Error];
        input.timezone = Some(Timezone::Utc);
        let from_cli = finish(input, &roots);
        assert_eq!(from_cli.levels, vec![Level::Error]);
        assert_eq!(from_cli.timezone, Timezone::Utc);
        assert_eq!(from_cli.tuning.similarity, 0.75);
        assert_eq!(from_cli.tuning.bucket_cap, 50);
        assert_eq!(from_cli.tuning.max_groups, 10);
    }

    #[test]
    fn invalid_file_timezone_fails_even_when_cli_overrides() {
        let scratch = Scratch::new();
        let explicit = scratch.write_explicit("bad-tz.toml", "timezone = \"Not/AZone\"\n");
        let mut input = base_input();
        input.config = Some(explicit.clone());
        input.timezone = Some(Timezone::Utc);
        match finish_err(input, &scratch.roots()) {
            Error::ConfigInvalid { path, message } => {
                assert_eq!(path, explicit);
                assert!(message.contains("Not/AZone"), "{message}");
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_selected_file_fails_without_fallback() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new();
        scratch.write_cwd("timezone = \"local\"\n");
        let explicit = scratch.write_explicit("secret.toml", "timezone = \"UTC\"\n");
        let mut perms = fs::metadata(&explicit).expect("meta").permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&explicit, perms.clone()).expect("chmod");
        if fs::read_to_string(&explicit).is_ok() {
            perms.set_mode(0o600);
            let _ = fs::set_permissions(&explicit, perms);
            return;
        }
        let mut input = base_input();
        input.config = Some(explicit.clone());
        let err = finish_err(input, &scratch.roots());
        perms.set_mode(0o600);
        let _ = fs::set_permissions(&explicit, perms);
        match err {
            Error::ConfigUnreadable { path, .. } => assert_eq!(path, explicit),
            other => panic!("expected ConfigUnreadable, got {other:?}"),
        }
    }
}
