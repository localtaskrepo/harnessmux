//! Where harnessmux keeps its files. `HARNESSMUX_HOME` puts all of them under one directory (tests, sandboxes).

use std::path::{Path, PathBuf};

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

pub fn home() -> Option<PathBuf> {
    env_dir("HOME").or_else(|| env_dir("USERPROFILE"))
}

fn base(xdg: &str, unix_default: &str, windows: &str, under_home: &str) -> PathBuf {
    if let Some(root) = env_dir("HARNESSMUX_HOME") {
        return root.join(under_home);
    }
    if cfg!(windows) {
        return env_dir(windows).unwrap_or_else(std::env::temp_dir).join("harnessmux");
    }
    env_dir(xdg).or_else(|| home().map(|home| home.join(unix_default))).unwrap_or_else(std::env::temp_dir).join("harnessmux")
}

/// Sessions and their event logs: `~/.local/state/harnessmux` (`%LOCALAPPDATA%\harnessmux` on Windows).
pub fn state_dir() -> PathBuf {
    base("XDG_STATE_HOME", ".local/state", "LOCALAPPDATA", "state")
}

/// Installed adapters: `~/.local/share/harnessmux` (`%LOCALAPPDATA%\harnessmux` on Windows).
pub fn data_dir() -> PathBuf {
    base("XDG_DATA_HOME", ".local/share", "LOCALAPPDATA", "data")
}

/// The user's agent table: `~/.config/harnessmux` (`%APPDATA%\harnessmux` on Windows).
pub fn config_dir() -> PathBuf {
    base("XDG_CONFIG_HOME", ".config", "APPDATA", "config")
}

/// `~/x` becomes the home directory plus `x`.
pub fn expand_home(raw: &str) -> PathBuf {
    match (raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")), home()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(raw),
    }
}

/// The full path of `program`: as given when it has a directory part, else the first match on PATH or in `extra`.
pub fn find_program(program: &str, extra: &[String]) -> Option<PathBuf> {
    let given = Path::new(program);
    if given.components().count() > 1 {
        return is_executable(given).then(|| given.to_path_buf());
    }
    let path_dirs = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect::<Vec<_>>()).unwrap_or_default();
    let names: Vec<String> =
        if cfg!(windows) { ["", ".exe", ".cmd", ".bat"].iter().map(|ext| format!("{program}{ext}")).collect() } else { vec![program.to_string()] };
    path_dirs
        .into_iter()
        .chain(extra.iter().map(|dir| expand_home(dir)))
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata().is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_with_a_directory_is_taken_as_given() {
        assert_eq!(find_program("/no/such/program", &[]), None);
        assert_eq!(expand_home("plain"), PathBuf::from("plain"));
    }
}
