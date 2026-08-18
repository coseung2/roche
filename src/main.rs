use eframe::egui;
use roche_workstation::ui::desktop::DesktopApp;

fn main() -> eframe::Result {
    if std::env::args_os().any(|arg| arg == "--webgpt-browser-host") {
        if let Err(error) = roche_workstation::web_browser::run_browser_host() {
            eprintln!("WEBGPT_BROWSER_HOST_ERROR {error}");
            std::process::exit(2);
        }
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Roche AI Workstation")
            .with_decorations(false)
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([980.0, 640.0]),
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "roche-ai-workstation",
        options,
        Box::new(|cc| Ok(Box::new(DesktopApp::new(cc)))),
    )
}
