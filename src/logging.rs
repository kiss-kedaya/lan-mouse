//! Log setup shared by the entry points.
//!
//! Besides the console every run writes `<config dir>/lan-mouse.log`: a
//! window started by a double click has no console, and the reasons capture
//! or emulation do not start are what users need to see. The file is rotated
//! on start so it never grows beyond two runs.

use std::io::Write;

/// the log file next to the configuration, `None` when the directory cannot
/// be determined
pub fn log_file_path() -> Option<std::path::PathBuf> {
    let dir = crate::config::config_dir()?;
    Some(dir.join("lan-mouse.log"))
}

/// installs the logger: `LAN_MOUSE_LOG_LEVEL` (default `info`) for the
/// console, the same messages (at least `info`) in the log file
pub fn init() {
    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default().filter_or("LAN_MOUSE_LOG_LEVEL", "info"),
    );
    if let Some(path) = log_file_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // The frontend and its service both log, and each would start at the
        // beginning of the file: open in append mode so their lines are kept.
        // Rotating by renaming fails while the other process holds the file
        // open, which is fine: the log only grows.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 4 * 1024 * 1024) {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let file = std::sync::Mutex::new(file);
            // the file is the place to look when something does not work, so
            // it always holds the debug level messages
            builder
                .filter_level(log::LevelFilter::Debug)
                .target(env_logger::Target::Pipe(Box::new(FileWriter(file))));
        }
    }
    builder.init();
}

/// writes the formatted lines to the file
struct FileWriter(std::sync::Mutex<std::fs::File>);

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut file = self.0.lock().expect("log file");
        file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().expect("log file").flush()
    }
}
