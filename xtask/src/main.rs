//! `cargo xtask install`: build harnessmux and put it on PATH. On macOS it is signed first, with the owner's Developer ID
//! when this Mac has it, so keychain and firewall grants survive a new build (lotar DEV-564). The new binary is staged
//! next to the installed one and renamed over it only once it is signed, verified and runs: any failure leaves the
//! installed binary as it was.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitCode};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = if cfg!(windows) { "harnessmux.exe" } else { "harnessmux" };
// macOS ties keychain and firewall grants to the identifier and the team: they never change.
const IDENTIFIER: &str = "net.pyxzl.harnessmux";
const DEFAULT_IDENTITY: &str = "Developer ID Application: Ravi Gairola (3HKD37M482)";
// codesign waits without end while the keychain asks at the Mac to use the key.
const CODESIGN_TIMEOUT: Duration = Duration::from_secs(120);

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args != ["install"] {
        eprintln!("usage: cargo xtask install");
        return ExitCode::from(2);
    }
    match install() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn install() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().ok_or("xtask has no parent directory")?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    run(Command::new(cargo).current_dir(root).args(["build", "--release", "--locked", "--package", "harnessmux"]))?;
    let target = env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let built = target.join("release").join(BIN);

    let dir = install_dir(env::var_os("HARNESSMUX_INSTALL_DIR"), env::var_os("CARGO_HOME"), env::var_os("HOME"))?;
    let signing = if cfg!(target_os = "macos") {
        let identities = output(Command::new("security").args(["find-identity", "-v", "-p", "codesigning"]))?;
        Some(signing(env::var("HARNESSMUX_SIGNING_IDENTITY").ok().filter(|s| !s.is_empty()), &identities)?)
    } else {
        None
    };
    if signing == Some(Signing::AdHoc) {
        eprintln!(
            "warning: no \"{DEFAULT_IDENTITY}\" in the keychain: signing ad-hoc. macOS asks for keychain and firewall grants \
             again after each new build; set HARNESSMUX_SIGNING_IDENTITY to sign with another identity."
        );
    }
    if let Some(Signing::Identity(identity)) = &signing {
        eprintln!("signing with \"{identity}\": the first time, macOS may ask at the Mac to use its key (choose Always Allow)");
    }
    let installed = stage_and_replace(&built, &dir, |staged| {
        if let Some(signing) = &signing {
            let identity = match signing {
                Signing::Identity(identity) => identity.as_str(),
                Signing::AdHoc => "-",
            };
            let sign = Command::new("codesign")
                .args(["--force", "--options", "runtime", "--timestamp=none", "--identifier", IDENTIFIER, "--sign", identity])
                .arg(staged)
                .spawn();
            wait(sign, CODESIGN_TIMEOUT).map_err(|e| format!("codesign: {e}"))?;
            run(Command::new("codesign").args(["--verify", "--strict"]).arg(staged))?;
        }
        output(Command::new(staged).arg("--version")).map(drop)
    })?;
    let how = match &signing {
        Some(Signing::Identity(identity)) => format!(", signed with \"{identity}\""),
        Some(Signing::AdHoc) => ", signed ad-hoc".into(),
        None => String::new(),
    };
    println!("installed {}{how}", installed.display());
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Signing {
    Identity(String),
    AdHoc,
}

/// HARNESSMUX_SIGNING_IDENTITY wins over the default. A set identity the keychain lacks stops the install; a missing
/// default falls back to ad-hoc, so other Macs and CI keep working.
fn signing(explicit: Option<String>, identities: &str) -> Result<Signing, String> {
    // `security find-identity` lists each one as `  1) <SHA-1> "<name>"`; codesign takes either.
    let known = |identity: &str| {
        identities
            .lines()
            .filter_map(|line| line.trim().split_once(") ")?.1.split_once(' '))
            .any(|(hash, name)| hash == identity || name == format!("\"{identity}\""))
    };
    match explicit {
        Some(identity) if known(&identity) => Ok(Signing::Identity(identity)),
        Some(identity) => Err(format!("HARNESSMUX_SIGNING_IDENTITY \"{identity}\" is not a valid code-signing identity in the keychain")),
        None if known(DEFAULT_IDENTITY) => Ok(Signing::Identity(DEFAULT_IDENTITY.into())),
        None => Ok(Signing::AdHoc),
    }
}

/// HARNESSMUX_INSTALL_DIR, else where `cargo install` puts binaries.
fn install_dir(explicit: Option<OsString>, cargo_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf, String> {
    let nonempty = |value: Option<OsString>| value.filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = nonempty(explicit) {
        return Ok(dir);
    }
    let cargo_home = nonempty(cargo_home).or_else(|| nonempty(home).map(|home| home.join(".cargo")));
    cargo_home.map(|home| home.join("bin")).ok_or_else(|| "set HARNESSMUX_INSTALL_DIR: neither CARGO_HOME nor HOME is set".into())
}

/// Copies `built` to a staged file in `dir`, runs `check` on it and renames it over the installed binary. A failed
/// check removes the staged file and leaves the installed binary alone. The rename keeps a running harnessmux on its old
/// file. Each install stages under its own name, so one running at the same time can't replace or remove the checked
/// file.
fn stage_and_replace(built: &Path, dir: &Path, check: impl FnOnce(&Path) -> Result<(), String>) -> Result<PathBuf, String> {
    static STAGES: AtomicU32 = AtomicU32::new(0);
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let staged = dir.join(format!(".{BIN}.{}-{}.new", process::id(), STAGES.fetch_add(1, Ordering::Relaxed)));
    let installed = dir.join(BIN);
    fs::copy(built, &staged).map_err(|e| format!("copy {} to {}: {e}", built.display(), staged.display()))?;
    let result = check(&staged).and_then(|()| fs::rename(&staged, &installed).map_err(|e| format!("rename to {}: {e}", installed.display())));
    if let Err(error) = result {
        // codesign killed mid-way leaves its work file next to the staged one.
        let mut cstemp = staged.clone().into_os_string();
        cstemp.push(".cstemp");
        for file in [staged, PathBuf::from(cstemp)] {
            let _ = fs::remove_file(file);
        }
        return Err(format!("{error}; {} is unchanged", installed.display()));
    }
    Ok(installed)
}

/// Runs a command with the terminal's stdout and stderr, so its own error shows.
fn run(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|e| format!("{}: {e}", describe(command)))?;
    if status.success() { Ok(()) } else { Err(format!("{} failed ({status})", describe(command))) }
}

/// Waits for a spawned command, killing it after `timeout`.
fn wait(child: std::io::Result<std::process::Child>, timeout: Duration) -> Result<(), String> {
    let mut child = child.map_err(|e| e.to_string())?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return if status.success() { Ok(()) } else { Err(format!("failed ({status})")) };
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "no answer after {} s: the keychain is probably asking at the Mac to use the signing key; allow it there (Always Allow) and run the install again",
                timeout.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn output(command: &mut Command) -> Result<String, String> {
    let output = command.output().map_err(|e| format!("{}: {e}", describe(command)))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!("{} failed ({}): {}", describe(command), output.status, String::from_utf8_lossy(&output.stderr).trim()))
    }
}

fn describe(command: &Command) -> String {
    let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy()).collect();
    format!("{} {}", command.get_program().to_string_lossy(), args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITIES: &str = r#"  1) A249626292A512C3C762C35999659014201AF8F3 "Apple Development: Ravi Gairola (3KAUY3X329)"
  2) 3AC6606E556C89E17F8A0969A06E8B0B1E2E84A3 "Developer ID Application: Ravi Gairola (3HKD37M482)"
     2 valid identities found
"#;

    #[test]
    fn the_developer_id_is_the_default() {
        assert_eq!(signing(None, IDENTITIES), Ok(Signing::Identity(DEFAULT_IDENTITY.into())));
    }

    #[test]
    fn a_set_identity_wins_by_name_or_hash() {
        let name = "Apple Development: Ravi Gairola (3KAUY3X329)";
        assert_eq!(signing(Some(name.into()), IDENTITIES), Ok(Signing::Identity(name.into())));
        let hash = "A249626292A512C3C762C35999659014201AF8F3";
        assert_eq!(signing(Some(hash.into()), IDENTITIES), Ok(Signing::Identity(hash.into())));
    }

    #[test]
    fn a_missing_default_signs_ad_hoc_and_a_missing_set_identity_stops() {
        let none = "     0 valid identities found\n";
        assert_eq!(signing(None, none), Ok(Signing::AdHoc));
        assert!(signing(Some("Developer ID Application: Someone (X)".into()), IDENTITIES).is_err());
        // Part of a name is not an identity.
        assert!(signing(Some("Ravi".into()), IDENTITIES).is_err());
    }

    #[test]
    fn install_dir_prefers_the_set_one_then_cargo_home_then_home() {
        let dir = |a: Option<&str>, b: Option<&str>, c: Option<&str>| install_dir(a.map(Into::into), b.map(Into::into), c.map(Into::into));
        assert_eq!(dir(Some("/opt/bin"), Some("/c"), Some("/h")), Ok(PathBuf::from("/opt/bin")));
        assert_eq!(dir(Some(""), Some("/c"), Some("/h")), Ok(PathBuf::from("/c/bin")));
        assert_eq!(dir(None, None, Some("/h")), Ok(PathBuf::from("/h/.cargo/bin")));
        assert!(dir(None, None, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_does_not_finish_is_killed_after_the_timeout() {
        let error = wait(Command::new("sleep").arg("30").spawn(), Duration::from_millis(300)).unwrap_err();
        assert!(error.contains("Always Allow"), "{error}");
        assert_eq!(wait(Command::new("false").spawn(), Duration::from_secs(5)), Err("failed (exit status: 1)".into()));
        assert_eq!(wait(Command::new("true").spawn(), Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn a_passing_check_replaces_the_installed_binary() {
        let temp = tempfile::tempdir().unwrap();
        let built = temp.path().join("built");
        fs::write(&built, "new").unwrap();
        let dir = temp.path().join("bin");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(BIN), "old").unwrap();

        let installed = stage_and_replace(&built, &dir, |_| Ok(())).unwrap();
        assert_eq!(fs::read_to_string(installed).unwrap(), "new");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "the staged file is left behind");
    }

    #[test]
    fn a_failing_check_leaves_the_installed_binary_as_it_was() {
        let temp = tempfile::tempdir().unwrap();
        let built = temp.path().join("built");
        fs::write(&built, "new").unwrap();
        let dir = temp.path().join("bin");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(BIN), "old").unwrap();

        let error = stage_and_replace(&built, &dir, |staged| {
            assert_eq!(fs::read_to_string(staged).unwrap(), "new");
            fs::write(format!("{}.cstemp", staged.display()), "").unwrap();
            Err("codesign failed".into())
        })
        .unwrap_err();
        assert!(error.starts_with("codesign failed"), "{error}");
        assert_eq!(fs::read_to_string(dir.join(BIN)).unwrap(), "old");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "the staged file is left behind");
    }

    #[test]
    fn concurrent_installs_each_install_only_the_bytes_they_checked() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("bin");
        let built = |name: &str| {
            let path = temp.path().join(name);
            fs::write(&path, name).unwrap();
            path
        };
        let (a, b, c) = (built("a"), built("b"), built("c"));

        // While A checks its staged binary, B installs and C fails in the same directory.
        let installed = stage_and_replace(&a, &dir, |staged| {
            assert_eq!(fs::read_to_string(dir.join(BIN)).unwrap_or_default(), "");
            stage_and_replace(&b, &dir, |_| Ok(())).unwrap();
            assert_eq!(fs::read_to_string(dir.join(BIN)).unwrap(), "b");
            stage_and_replace(&c, &dir, |_| Err("codesign failed".into())).unwrap_err();
            assert_eq!(fs::read_to_string(staged).unwrap(), "a");
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read_to_string(installed).unwrap(), "a");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "a staged file is left behind");
    }
}
