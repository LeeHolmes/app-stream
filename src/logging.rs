use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const CAPACITY: usize = 1000;

#[derive(Default)]
struct Log {
    revision: u64,
    entries: VecDeque<String>,
}

fn log() -> &'static Mutex<Log> {
    static LOG: OnceLock<Mutex<Log>> = OnceLock::new();
    LOG.get_or_init(Mutex::default)
}

pub fn write(text: String) {
    #[cfg(any(test, not(windows)))]
    std::eprintln!("{text}");
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System clock predates Unix epoch")
        .as_secs();
    let mut log = log().lock().expect("Log mutex poisoned");
    for line in text.lines() {
        if log.entries.len() == CAPACITY {
            log.entries.pop_front();
        }
        // Keep control characters out of the native edit control and bound each entry.
        let line: String = line
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .take(2000)
            .collect();
        log.entries.push_back(format!(
            "[{:02}:{:02}:{:02} UTC] {line}\r\n",
            seconds / 3600 % 24,
            seconds / 60 % 60,
            seconds % 60
        ));
    }
    log.revision = log.revision.wrapping_add(1);
}

#[cfg(windows)]
pub fn snapshot() -> (u64, String) {
    let log = log().lock().expect("Log mutex poisoned");
    (
        log.revision,
        log.entries.iter().map(String::as_str).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_is_bounded_and_sanitizes_native_control_text() {
        for _ in 0..CAPACITY + 1 {
            write("client\0 test".into());
        }
        let log = log().lock().unwrap();
        assert_eq!(log.entries.len(), CAPACITY);
        assert!(log.entries.iter().all(|line| !line.contains('\0')));
    }
}
