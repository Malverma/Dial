mod app;
mod audio;
mod backend;
mod cli;
mod config;
mod curve;
mod discovery;
mod edid;
mod hypr;
mod meter;
mod model;

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "io.github.dial";

fn main() -> glib::ExitCode {
    // Handle these before GApplication so it doesn't print its own help.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-h" | "--help" | "help" => {
                println!("{}", cli::USAGE);
                return glib::ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("dial {}", env!("CARGO_PKG_VERSION"));
                return glib::ExitCode::SUCCESS;
            }
            _ => {}
        }
    }

    let application = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let gui: Rc<RefCell<Option<app::AppRef>>> = Rc::default();
    application.connect_command_line(move |application, cmdline| {
        let args: Vec<String> =
            cmdline.arguments().iter().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();

        if args.is_empty() {
            let mut gui = gui.borrow_mut();
            match &*gui {
                Some(app) => app.borrow().present(),
                None => *gui = Some(app::App::new(application)),
            }
            return glib::ExitCode::SUCCESS;
        }

        let mut out = |s: &str| cmdline.print_literal(s);
        let mut err = |s: &str| cmdline.printerr_literal(s);
        let code = match &*gui.borrow() {
            // The window is open (in this or, via D-Bus, another process): it owns
            // every backend, including gamma and overlay.
            Some(app) => cli::run(&mut *app.borrow_mut(), &args, &mut out, &mut err),
            None => {
                let mut headless = cli::Headless::discover(&config::Config::load());
                cli::run(&mut headless, &args, &mut out, &mut err)
            }
        };
        glib::ExitCode::from(code as u8)
    });

    application.run()
}
