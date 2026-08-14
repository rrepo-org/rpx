use std::io::IsTerminal;

use indicatif::{ProgressBar, ProgressStyle};

use crate::output::note;

#[derive(Debug)]
pub(crate) struct SystemDepsUi {
    interactive: bool,
    bar: ProgressBar,
}

impl SystemDepsUi {
    pub(crate) fn start() -> Self {
        let interactive = std::io::stderr().is_terminal();
        let bar = if interactive {
            let bar = ProgressBar::new_spinner();
            bar.set_style(
                ProgressStyle::with_template("{spinner} {msg}")
                    .expect("progress template should parse"),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(100));
            bar.set_message("installing system dependencies");
            bar
        } else {
            note("Installing system dependencies...");
            ProgressBar::hidden()
        };

        Self { interactive, bar }
    }

    pub(crate) fn finish(self) {
        if self.interactive {
            self.bar
                .finish_with_message("Installed system dependencies".to_string());
        } else {
            note("Installed system dependencies");
        }
    }

    pub(crate) fn fail(self) {
        if self.interactive {
            self.bar.finish_and_clear();
        }
    }
}
