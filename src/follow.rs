//! Reading a session's event log from another process: from the start or from now, following it as it grows.

use std::io::{BufRead as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::event::Event;

const POLL: Duration = Duration::from_millis(100);

pub struct Follower {
    path: PathBuf,
    offset: u64,
    partial: String,
}

impl Follower {
    /// From the first event.
    pub fn from_start(path: &Path) -> Self {
        Self { path: path.to_path_buf(), offset: 0, partial: String::new() }
    }

    /// From the next event written.
    pub fn from_end(path: &Path) -> Self {
        let offset = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        Self { path: path.to_path_buf(), offset, partial: String::new() }
    }

    /// The events written since the last call. Lines that are not events (a newer format's) are skipped.
    pub fn read_new(&mut self) -> Result<Vec<Event>> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::io(format!("reading {}", self.path.display()), error)),
        };
        file.seek(SeekFrom::Start(self.offset)).map_err(|error| Error::io(format!("reading {}", self.path.display()), error))?;
        let mut reader = std::io::BufReader::new(file);
        let mut events = Vec::new();
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).map_err(|error| Error::io(format!("reading {}", self.path.display()), error))?;
            if read == 0 {
                break;
            }
            self.offset += read as u64;
            if !line.ends_with('\n') {
                // The owner is still writing this line.
                self.partial.push_str(&line);
                break;
            }
            let full = std::mem::take(&mut self.partial) + &line;
            if let Ok(event) = serde_json::from_str::<Event>(full.trim_end()) {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Waits for the next batch of events.
    pub async fn next(&mut self) -> Result<Vec<Event>> {
        loop {
            let events = self.read_new()?;
            if !events.is_empty() {
                return Ok(events);
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn partial_lines_wait_for_their_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.jsonl");
        let line = r#"{"v":1,"ts":"t","session":"s","seq":1,"type":"notice","text":"hi"}"#;
        let mut file = std::fs::File::create(&path).unwrap();
        write!(file, "{}", &line[..20]).unwrap();
        let mut follower = Follower::from_start(&path);
        assert!(follower.read_new().unwrap().is_empty());
        writeln!(file, "{}", &line[20..]).unwrap();
        writeln!(file, "not json").unwrap();
        let events = follower.read_new().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 1);
        assert!(Follower::from_end(&path).read_new().unwrap().is_empty());
    }
}
