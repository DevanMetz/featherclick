//! FeatherClick - a tiny, cross-platform autoclicker.

// Release builds on Windows use the GUI subsystem so launching the exe from
// Explorer does not flash a console window. `--headless` reattaches to the
// parent console when there is one.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod cli;
mod config;
mod engine;
mod hotkeys;
mod icon;
mod platform;
mod ui;

use std::process::ExitCode;

use eframe::egui;

/// Version string shown in the UI and by `--version`.
pub fn version() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

fn main() -> ExitCode {
    // Before anything reads or writes a screen coordinate.
    platform::init_dpi();

    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            platform::attach_console();
            eprintln!("featherclick: {message}");
            return ExitCode::from(2);
        }
    };

    if args.help {
        platform::attach_console();
        println!("{}", cli::USAGE);
        return ExitCode::SUCCESS;
    }
    if args.version {
        platform::attach_console();
        println!("featherclick {}", version());
        return ExitCode::SUCCESS;
    }
    if let Some(run) = args.run {
        platform::attach_console();
        return ExitCode::from(run.execute());
    }

    gui()
}

fn gui() -> ExitCode {
    let settings = config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FeatherClick")
            .with_app_id("featherclick")
            .with_inner_size([420.0, 720.0])
            .with_min_inner_size([400.0, 420.0])
            .with_icon(icon::data()),
        ..Default::default()
    };

    let created = eframe::run_native(
        "FeatherClick",
        options,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, settings)))),
    );

    match created {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            platform::attach_console();
            eprintln!("featherclick: cannot open a window: {err}");
            ExitCode::FAILURE
        }
    }
}
