use anyhow::{anyhow, bail, Context, Error, Result};
use std::{
    cmp::Ordering,
    fs::{self, read_dir, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{self, AtomicBool},
    thread,
};

use crate::{
    cargo_toml::{append_bins, bins_start_end_ind, BINS_BUFFER_CAPACITY},
    cmd::CmdRunner,
    exercise::{RunnableExercise, OUTPUT_CAPACITY},
    info_file::{ExerciseInfo, InfoFile},
    CURRENT_FORMAT_VERSION,
};

// Find a char that isn't allowed in the exercise's `name` or `dir`.
fn forbidden_char(input: &str) -> Option<char> {
    input.chars().find(|c| !c.is_alphanumeric() && *c != '_')
}

// Check that the Cargo.toml file is up-to-date.
fn check_cargo_toml(
    exercise_infos: &[ExerciseInfo],
    current_cargo_toml: &str,
    exercise_path_prefix: &[u8],
) -> Result<()> {
    let (bins_start_ind, bins_end_ind) = bins_start_end_ind(current_cargo_toml)?;

    let old_bins = &current_cargo_toml.as_bytes()[bins_start_ind..bins_end_ind];
    let mut new_bins = Vec::with_capacity(BINS_BUFFER_CAPACITY);
    append_bins(&mut new_bins, exercise_infos, exercise_path_prefix);

    if old_bins != new_bins {
        if cfg!(debug_assertions) {
            bail!("The file `dev/Cargo.toml` is outdated. Please run `cargo run -- dev update` to update it. Then run `cargo run -- dev check` again");
        }

        bail!("The file `Cargo.toml` is outdated. Please run `rustlings dev update` to update it. Then run `rustlings dev check` again");
    }

    Ok(())
}

// Check the info of all exercises and return their paths in a set.
fn check_info_file_exercises(info_file: &InfoFile) -> Result<hashbrown::HashSet<PathBuf>> {
    let mut names = hashbrown::HashSet::with_capacity(info_file.exercises.len());
    let mut paths = hashbrown::HashSet::with_capacity(info_file.exercises.len());

    let mut file_buf = String::with_capacity(1 << 14);
    for exercise_info in &info_file.exercises {
        let name = exercise_info.name.as_str();
        if name.is_empty() {
            bail!("Found an empty exercise name in `info.toml`");
        }
        if let Some(c) = forbidden_char(name) {
            bail!("Char `{c}` in the exercise name `{name}` is not allowed");
        }

        if let Some(dir) = &exercise_info.dir {
            if dir.is_empty() {
                bail!("The exercise `{name}` has an empty dir name in `info.toml`");
            }
            if let Some(c) = forbidden_char(dir) {
                bail!("Char `{c}` in the exercise dir `{dir}` is not allowed");
            }
        }

        if exercise_info.hint.trim().is_empty() {
            bail!("The exercise `{name}` has an empty hint. Please provide a hint or at least tell the user why a hint isn't needed for this exercise");
        }

        if !names.insert(name) {
            bail!("The exercise name `{name}` is duplicated. Exercise names must all be unique");
        }

        let path = exercise_info.path();

        OpenOptions::new()
            .read(true)
            .open(&path)
            .with_context(|| format!("Failed to open the file {path}"))?
            .read_to_string(&mut file_buf)
            .with_context(|| format!("Failed to read the file {path}"))?;

        if !file_buf.contains("fn main()") {
            bail!("The `main` function is missing in the file `{path}`.\nCreate at least an empty `main` function to avoid language server errors");
        }

        if !file_buf.contains("// TODO") {
            bail!("Didn't find any `// TODO` comment in the file `{path}`.\nYou need to have at least one such comment to guide the user.");
        }

        if !exercise_info.test && file_buf.contains("#[test]") {
            bail!("The file `{path}` contains tests annotated with `#[test]` but the exercise `{name}` has `test = false` in the `info.toml` file");
        }

        file_buf.clear();

        paths.insert(PathBuf::from(path));
    }

    Ok(paths)
}

// Check `dir` for unexpected files.
// Only Rust files in `allowed_rust_files` and `README.md` files are allowed.
// Only one level of directory nesting is allowed.
fn check_unexpected_files(
    dir: &str,
    allowed_rust_files: &hashbrown::HashSet<PathBuf>,
) -> Result<()> {
    let unexpected_file = |path: &Path| {
        anyhow!("Found the file `{}`. Only `README.md` and Rust files related to an exercise in `info.toml` are allowed in the `{dir}` directory", path.display())
    };

    for entry in read_dir(dir).with_context(|| format!("Failed to open the `{dir}` directory"))? {
        let entry = entry.with_context(|| format!("Failed to read the `{dir}` directory"))?;

        if entry.file_type().unwrap().is_file() {
            let path = entry.path();
            let file_name = path.file_name().unwrap();
            if file_name == "README.md" {
                continue;
            }

            if !allowed_rust_files.contains(&path) {
                return Err(unexpected_file(&path));
            }

            continue;
        }

        let dir_path = entry.path();
        for entry in read_dir(&dir_path)
            .with_context(|| format!("Failed to open the directory {}", dir_path.display()))?
        {
            let entry = entry
                .with_context(|| format!("Failed to read the directory {}", dir_path.display()))?;
            let path = entry.path();

            if !entry.file_type().unwrap().is_file() {
                bail!("Found `{}` but expected only files. Only one level of exercise nesting is allowed", path.display());
            }

            let file_name = path.file_name().unwrap();
            if file_name == "README.md" {
                continue;
            }

            if !allowed_rust_files.contains(&path) {
                return Err(unexpected_file(&path));
            }
        }
    }

    Ok(())
}

fn check_exercises_unsolved(info_file: &InfoFile, cmd_runner: &CmdRunner) -> Result<()> {
    let error_occurred = AtomicBool::new(false);

    println!(
        "Running all exercises to check that they aren't already solved. This may take a while…\n",
    );
    thread::scope(|s| {
        for exercise_info in &info_file.exercises {
            if exercise_info.skip_check_unsolved {
                continue;
            }

            s.spawn(|| {
                let error = |e| {
                    let mut stderr = io::stderr().lock();
                    stderr.write_all(e).unwrap();
                    stderr.write_all(b"\nProblem with the exercise ").unwrap();
                    stderr.write_all(exercise_info.name.as_bytes()).unwrap();
                    stderr.write_all(SEPARATOR).unwrap();
                    error_occurred.store(true, atomic::Ordering::Relaxed);
                };

                match exercise_info.run_exercise(None, cmd_runner) {
                    Ok(true) => error(b"Already solved!"),
                    Ok(false) => (),
                    Err(e) => error(e.to_string().as_bytes()),
                }
            });
        }
    });

    if error_occurred.load(atomic::Ordering::Relaxed) {
        bail!(CHECK_EXERCISES_UNSOLVED_ERR);
    }

    Ok(())
}

fn check_exercises(info_file: &InfoFile, cmd_runner: &CmdRunner) -> Result<()> {
    match info_file.format_version.cmp(&CURRENT_FORMAT_VERSION) {
        Ordering::Less => bail!("`format_version` < {CURRENT_FORMAT_VERSION} (supported version)\nPlease migrate to the latest format version"),
        Ordering::Greater => bail!("`format_version` > {CURRENT_FORMAT_VERSION} (supported version)\nTry updating the Rustlings program"),
        Ordering::Equal => (),
    }

    let info_file_paths = check_info_file_exercises(info_file)?;
    check_unexpected_files("exercises", &info_file_paths)?;

    check_exercises_unsolved(info_file, cmd_runner)
}

enum SolutionCheck {
    Success { sol_path: String },
    MissingRequired,
    MissingOptional,
    RunFailure { output: Vec<u8> },
    Err(Error),
}

fn check_solutions(
    require_solutions: bool,
    info_file: &InfoFile,
    cmd_runner: &CmdRunner,
) -> Result<()> {
    println!("Running all solutions. This may take a while…\n");
    thread::scope(|s| {
        let handles = info_file
            .exercises
            .iter()
            .map(|exercise_info| {
                s.spawn(|| {
                    let sol_path = exercise_info.sol_path();
                    if !Path::new(&sol_path).exists() {
                        if require_solutions {
                            return SolutionCheck::MissingRequired;
                        }

                        return SolutionCheck::MissingOptional;
                    }

                    let mut output = Vec::with_capacity(OUTPUT_CAPACITY);
                    match exercise_info.run_solution(Some(&mut output), cmd_runner) {
                        Ok(true) => SolutionCheck::Success { sol_path },
                        Ok(false) => SolutionCheck::RunFailure { output },
                        Err(e) => SolutionCheck::Err(e),
                    }
                })
            })
            .collect::<Vec<_>>();

        let mut sol_paths = hashbrown::HashSet::with_capacity(info_file.exercises.len());
        let mut fmt_cmd = Command::new("rustfmt");
        fmt_cmd
            .arg("--check")
            .arg("--edition")
            .arg("2021")
            .arg("--color")
            .arg("--always")
            .stdin(Stdio::null());

        for (exercise_name, handle) in info_file
            .exercises
            .iter()
            .map(|exercise_info| &exercise_info.name)
            .zip(handles)
        {
            match handle.join() {
                Ok(SolutionCheck::Success { sol_path }) => {
                    fmt_cmd.arg(&sol_path);
                    sol_paths.insert(PathBuf::from(sol_path));
                }
                Ok(SolutionCheck::MissingRequired) => {
                    bail!("The solution of the exercise {exercise_name} is missing");
                }
                Ok(SolutionCheck::MissingOptional) => (),
                Ok(SolutionCheck::RunFailure { output }) => {
                    io::stderr().lock().write_all(&output)?;
                    bail!("Running the solution of the exercise {exercise_name} failed with the error above");
                }
                Ok(SolutionCheck::Err(e)) => return Err(e),
                Err(_) => {
                    bail!("Panic while trying to run the solution of the exericse {exercise_name}");
                }
            }
        }

        let handle = s.spawn(move || check_unexpected_files("solutions", &sol_paths));

        if !fmt_cmd
            .status()
            .context("Failed to run `rustfmt` on all solution files")?
            .success()
        {
            bail!("Some solutions aren't formatted. Run `rustfmt` on them");
        }

        handle.join().unwrap()
    })
}

pub fn check(require_solutions: bool) -> Result<()> {
    let info_file = InfoFile::parse()?;

    if cfg!(debug_assertions) {
        // A hack to make `cargo run -- dev check` work when developing Rustlings.
        check_cargo_toml(
            &info_file.exercises,
            include_str!("../../dev-Cargo.toml"),
            b"../",
        )?;
    } else {
        let current_cargo_toml =
            fs::read_to_string("Cargo.toml").context("Failed to read the file `Cargo.toml`")?;
        check_cargo_toml(&info_file.exercises, &current_cargo_toml, b"")?;
    }

    let cmd_runner = CmdRunner::build()?;
    check_exercises(&info_file, &cmd_runner)?;
    check_solutions(require_solutions, &info_file, &cmd_runner)?;

    println!("\nEverything looks fine!");

    Ok(())
}

const SEPARATOR: &[u8] =
    b"\n========================================================================================\n";

const CHECK_EXERCISES_UNSOLVED_ERR: &str = "At least one exercise is already solved or failed to run. See the output above.
If this is an intro exercise that is intended to be already solved, add `skip_check_unsolved = true` to the exercise's metadata in the `info.toml` file.";
