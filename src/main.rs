mod app;
mod model;
mod store;
mod ui;
#[cfg(test)]
mod tests;

use app::App;
use crossterm::event::{self, Event};
use model::Paths;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn run_tui(app: &mut App) -> io::Result<()> {
    let mut terminal = ratatui::init();
    app.start_load(false);

    let result = (|| -> io::Result<()> {
        while !app.should_quit {
            app.on_tick();
            terminal.draw(|frame| ui::render(frame, app))?;

            let poll_timeout = if app.loading {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(200)
            };

            if event::poll(poll_timeout)? {
                if let Event::Key(key) = event::read()? {
                    app.handle_key(key);
                }
            }
        }
        Ok(())
    })();

    app.cancel_background_load();
    ratatui::restore();
    result
}

fn main() {
    let paths = Paths::from_env();
    let mut app = App::new(paths);

    if let Err(err) = run_tui(&mut app) {
        eprintln!("Error running TUI: {err}");
        std::process::exit(1);
    }

    let Some(action) = app.resume_action else {
        return;
    };

    if !action.session.project.is_empty() {
        let project_path = Path::new(&action.session.project);
        if project_path.is_dir() {
            let _ = std::env::set_current_dir(project_path);
        } else {
            eprintln!(
                "Warning: project directory no longer exists: {}",
                action.session.project
            );
        }
    }

    let args = action.command_args();
    let err = Command::new(args[0]).args(&args[1..]).exec();
    if err.kind() == io::ErrorKind::NotFound {
        eprintln!("Error: '{}' not found in PATH", args[0]);
        std::process::exit(127);
    }
    eprintln!("Error executing {}: {err}", args[0]);
    std::process::exit(1);
}
