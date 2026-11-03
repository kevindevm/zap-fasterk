//! Opens a local file whose opening must be seen to work, such as the log.
//!
//! `open::that_detached` reports success once a launcher starts, so on Linux
//! a `.log` file with no usable handler opened nothing and said nothing:
//! `xdg-open` outside GNOME and KDE runs the default application itself, and
//! runs a terminal program (Neovim is Omarchy's text editor) without a
//! terminal. Here each way of opening is waited for, and a failure moves on to
//! showing the file in its folder, so the reader always ends up somewhere, or
//! learns that nothing worked.

use std::path::Path;

/// Opens `path` in its default application or, failing that, shows it in its
/// folder. Blocks for up to a few seconds per attempt, so call it off the
/// interface thread. The error names the file so the reader can go there.
pub fn open_or_reveal(path: &Path) -> Result<(), String> {
    first_success(&attempts(path)).map_err(|error| {
        format!(
            "Could not open {}: {error}. Open it from its folder instead.",
            path.display()
        )
    })
}

type Attempt<'a> = Box<dyn Fn() -> Result<(), String> + 'a>;

/// Runs the attempts in order and stops at the first that works; if none
/// does, the last error explains why.
fn first_success(attempts: &[Attempt<'_>]) -> Result<(), String> {
    let mut last = String::from("no way to open files was found");
    for attempt in attempts {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(error) => {
                log::warn!("an open attempt failed: {error}");
                last = error;
            }
        }
    }
    Err(last)
}

/// macOS's `open` and Windows' `start` report a file nothing can open.
fn attempts(path: &Path) -> Vec<Attempt<'_>> {
    let mut attempts: Vec<Attempt<'_>> = vec![Box::new(move || {
        open::that(path).map_err(|error| error.to_string())
    })];
    if let Some(folder) = path.parent() {
        attempts.push(Box::new(move || {
            open::that(folder).map_err(|error| error.to_string())
        }));
    }
    attempts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn the_first_attempt_that_works_ends_the_chain() {
        let tried = Cell::new(0);
        let attempts: Vec<Attempt<'_>> = vec![
            Box::new(|| {
                tried.set(tried.get() + 1);
                Err("no handler".into())
            }),
            Box::new(|| {
                tried.set(tried.get() + 1);
                Ok(())
            }),
            Box::new(|| {
                tried.set(tried.get() + 1);
                Ok(())
            }),
        ];
        assert_eq!(first_success(&attempts), Ok(()));
        assert_eq!(tried.get(), 2, "the fallback ran and nothing after it");
    }

    #[test]
    fn when_nothing_works_the_last_reason_is_reported() {
        let attempts: Vec<Attempt<'_>> = vec![
            Box::new(|| Err("gio failed".into())),
            Box::new(|| Err("no file manager".into())),
        ];
        assert_eq!(first_success(&attempts), Err("no file manager".into()));
        assert!(first_success(&[]).is_err());
    }
}
